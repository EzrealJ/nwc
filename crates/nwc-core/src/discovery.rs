//! 发现与扫描（对应 Node 版 src/discovery.js）。
//!
//! 网卡枚举是平台相关的（Windows GetAdaptersAddresses / macOS getifaddrs），由壳层注入；
//! 这里只做纯逻辑：虚拟网卡过滤、CIDR 数学、扫描计划、TCP 探测池。

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

pub const MAX_AUTO_HOSTS: usize = 4096;
const MAX_CIDR_HOSTS: u64 = 2048;
const AUTO_COLLAPSE_THRESHOLD: u64 = 1024;

/// 平台无关的网卡信息（壳层填充）：name 用 FriendlyName（Win）/ BSD 名（mac）
#[derive(Clone, Debug)]
pub struct IfaceInfo {
    pub name: String,
    pub addr: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub internal: bool,
}

// ---- 虚拟网卡过滤（正则清单逐条对应 Node 版 discovery.js） ----

struct VPatterns(Vec<regex::Regex>);

impl VPatterns {
    fn match_name(&self, name: &str) -> bool {
        self.0.iter().any(|re| re.is_match(name))
    }
}

static VIRTUAL_PATTERNS: LazyLock<VPatterns> = LazyLock::new(|| {
    let pats = [
        // Hyper-V / WSL / 容器
        r"(?i)vethernet", r"(?i)hyper-?v", r"(?i)\bwsl\b", r"(?i)docker", r"(?i)container", r"(?i)npcap",
        // 虚拟机软件
        r"(?i)vmware", r"(?i)vmnet", r"(?i)virtualbox", r"(?i)\bvbox", r"(?i)parallels", r"(?i)qemu",
        // VPN / 虚拟组网
        r"(?i)tailscale", r"(?i)zero\s?tier", r"(?i)wireguard", r"(?i)wintun", r"(?i)openvpn",
        r"(?i)\bvpn\b", r"(?i)\btap([\s_-]?\d+)?\b", r"(?i)\btun([\s_-]?\d+)?\b", r"(?i)tunnel",
        // 蓝牙 / 伪接口 / 过渡隧道
        r"(?i)bluetooth", "蓝牙", r"(?i)loopback", "回环", r"(?i)pseudo", "伪", r"(?i)teredo", r"(?i)isatap", r"(?i)6to4",
        // Windows 直连与热点虚拟适配器
        r"(?i)wi-?fi\s*direct", r"(?i)hosted\s*network", r"\*",
        // macOS 虚拟接口（en0/en1 等真实网卡不受影响）
        r"(?i)^(lo|gif|stf|utun|awdl|llw|anpi|ap|bridge|veth|p2p|vmenet|vnic|vmnet|feth|ham|ipsec)\d*$",
    ];
    VPatterns(pats.iter().map(|p| regex::Regex::new(p).expect("iface regex")).collect())
});

pub fn is_virtual_iface(name: &str) -> bool {
    VIRTUAL_PATTERNS.match_name(name)
}

// ---- CIDR 数学 ----

pub fn ip_to_int(ip: &Ipv4Addr) -> u32 {
    u32::from(*ip)
}

pub fn int_to_ip(n: u32) -> Ipv4Addr {
    Ipv4Addr::from(n)
}

pub fn size_to_bits(size: u64) -> u32 {
    let s = size.max(1);
    32u32.saturating_sub(s.ilog2()).max(1).min(32)
}

#[derive(Clone, Debug)]
pub struct Cidr {
    pub address: String,
    pub bits: u32,
    pub network: u32,
    pub size: u64,
}

pub fn parse_cidr(text: &str) -> Option<Cidr> {
    let text = text.trim();
    let (addr_part, bits_part) = match text.rfind('/') {
        Some(i) => (&text[..i], Some(&text[i + 1..])),
        None => (text, None),
    };
    let addr: Ipv4Addr = addr_part.parse().ok()?;
    // 对应 Node 的 ^\d{1,3}(\.\d{1,3}){3}$ 限制：恰好 4 段、各段纯数字（Ipv4Addr 另行校验范围）
    let parts: Vec<&str> = addr_part.split('.').collect();
    if parts.len() != 4 || !parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let bits = match bits_part {
        None => 32,
        Some(b) => {
            if !b.chars().all(|c| c.is_ascii_digit()) || b.is_empty() || b.len() > 2 {
                return None;
            }
            b.parse::<u32>().ok()?
        }
    };
    if bits > 32 {
        return None;
    }
    let size: u64 = if bits == 0 { 1u64 << 32 } else { 1u64 << (32 - bits) };
    let mask: u32 = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    Some(Cidr {
        address: addr_part.to_string(),
        bits,
        network: ip_to_int(&addr) & mask,
        size,
    })
}

fn hosts_for_cidr(network: u32, size: u64, self_ips: &HashSet<Ipv4Addr>) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let (start, end): (u64, u64) = if size <= 1 {
        (network as u64, network as u64)
    } else if size == 2 {
        (network as u64, network as u64 + 1)
    } else {
        (network as u64 + 1, network as u64 + size - 2)
    };
    for i in start..=end {
        let ip = int_to_ip(i as u32);
        if !self_ips.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

// ---- 扫描计划（对应 Node 版 planScan） ----

#[derive(Clone, Debug)]
pub struct SubnetPlan {
    pub iface: String,
    pub address: String,
    pub cidr: String,
    pub count: usize,
    pub note: String,
}

#[derive(Clone, Debug)]
pub struct Skipped {
    pub iface: String,
    pub address: String,
    pub reason: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanPlan {
    pub hosts: Vec<Ipv4Addr>,
    pub subnets: Vec<SubnetPlan>,
    pub skipped: Vec<Skipped>,
    pub explicit_target: bool,
    pub truncated: bool,
}

pub struct ScanInput<'a> {
    pub target: &'a str,
    pub peer_hint: &'a str,
    pub ifaces: &'a [String],
    pub scan_cidr: &'a [String],
    pub iface_list: &'a [IfaceInfo],
}

fn self_ip_set(iface_list: &[IfaceInfo]) -> HashSet<Ipv4Addr> {
    iface_list.iter().map(|i| i.addr).collect()
}

pub fn plan_scan(input: &ScanInput) -> ScanPlan {
    let self_ips = self_ip_set(input.iface_list);
    let mut seen = HashSet::new();
    let mut hosts: Vec<Ipv4Addr> = Vec::new();
    let mut subnets = Vec::new();
    let mut skipped = Vec::new();
    let mut push = |ip: Ipv4Addr, hosts: &mut Vec<Ipv4Addr>, seen: &mut HashSet<Ipv4Addr>| {
        if seen.insert(ip) {
            hosts.push(ip);
        }
    };

    if !input.target.is_empty() {
        return ScanPlan {
            hosts: input.target.parse().into_iter().collect(),
            explicit_target: true,
            ..Default::default()
        };
    }
    if !input.peer_hint.is_empty() {
        if let Ok(hint) = input.peer_hint.parse::<Ipv4Addr>() {
            if !self_ips.contains(&hint) {
                push(hint, &mut hosts, &mut seen);
            }
        }
    }

    if !input.scan_cidr.is_empty() {
        for spec in input.scan_cidr {
            let Some(cidr) = parse_cidr(spec) else {
                skipped.push(Skipped {
                    iface: "--scan-cidr".into(),
                    address: spec.clone(),
                    reason: "格式无效（示例 192.168.31.0/24）".into(),
                });
                continue;
            };
            let (mut network, mut size) = (cidr.network, cidr.size);
            let mut note = String::new();
            if size > MAX_CIDR_HOSTS {
                network &= 0xFFFFFF00;
                size = 256;
                note = format!("网段过大（/{}），已收敛为 /24", cidr.bits);
            }
            let list = hosts_for_cidr(network, size, &self_ips);
            for ip in &list {
                push(*ip, &mut hosts, &mut seen);
            }
            subnets.push(SubnetPlan {
                iface: "--scan-cidr".into(),
                address: cidr.address.clone(),
                cidr: format!("{}/{}", int_to_ip(network), size_to_bits(size)),
                count: list.len(),
                note,
            });
        }
        return finalize(ScanPlan { hosts, subnets, skipped, explicit_target: false, truncated: false });
    }

    let want_ifaces = input.ifaces;
    let mut matched = HashSet::new();
    for info in input.iface_list {
        if info.internal {
            continue;
        }
        let addr = ip_to_int(&info.addr);
        let mask = ip_to_int(&info.netmask);
        let size = ((!mask).wrapping_add(1)) as u64;
        let bits = size_to_bits(size);

        if !want_ifaces.is_empty() {
            let hit = want_ifaces.iter().find(|w| info.name.to_lowercase().contains(&w.to_lowercase()));
            let Some(hit) = hit else {
                skipped.push(Skipped {
                    iface: info.name.clone(),
                    address: info.addr.to_string(),
                    reason: "未在 --iface 列表中".into(),
                });
                continue;
            };
            matched.insert(hit.to_lowercase());
        } else {
            if is_virtual_iface(&info.name) {
                skipped.push(Skipped {
                    iface: info.name.clone(),
                    address: info.addr.to_string(),
                    reason: "虚拟网卡（已排除）".into(),
                });
                continue;
            }
            if (addr & 0xFFFF0000) == 0xA9FE0000 {
                skipped.push(Skipped {
                    iface: info.name.clone(),
                    address: info.addr.to_string(),
                    reason: "链路本地地址（169.254.x.x）".into(),
                });
                continue;
            }
            // 198.18.0.0/15：基准测试网段，也被 Clash/sing-box 等 fake-IP TUN 用作虚拟地址池；
            // 实测这类网卡探测任何端口都会"命中"，导致误判对端（Node 版同样存在此问题）
            if (addr & 0xFFFE0000) == 0xC6120000 {
                skipped.push(Skipped {
                    iface: info.name.clone(),
                    address: info.addr.to_string(),
                    reason: "基准测试/fake-IP 网段（198.18.x.x）".into(),
                });
                continue;
            }
            if size <= 2 {
                skipped.push(Skipped {
                    iface: info.name.clone(),
                    address: info.addr.to_string(),
                    reason: format!("掩码 /{bits}，无可用内网主机"),
                });
                continue;
            }
        }

        let (mut network, mut use_size) = (addr & mask, size);
        let mut note = String::new();
        if size > AUTO_COLLAPSE_THRESHOLD {
            network &= 0xFFFFFF00;
            use_size = 256;
            note = format!("网段过大（/{bits}），已收敛为 /24");
        }
        let list = hosts_for_cidr(network, use_size, &self_ips);
        for ip in &list {
            push(*ip, &mut hosts, &mut seen);
        }
        subnets.push(SubnetPlan {
            iface: info.name.clone(),
            address: info.addr.to_string(),
            cidr: format!("{}/{}", int_to_ip(network), size_to_bits(use_size)),
            count: list.len(),
            note,
        });
    }
    for w in want_ifaces {
        if !matched.contains(&w.to_lowercase()) {
            skipped.push(Skipped {
                iface: "--iface".into(),
                address: w.clone(),
                reason: "未找到匹配的网卡名".into(),
            });
        }
    }
    finalize(ScanPlan { hosts, subnets, skipped, explicit_target: false, truncated: false })
}

fn finalize(mut plan: ScanPlan) -> ScanPlan {
    if plan.hosts.len() > MAX_AUTO_HOSTS {
        plan.truncated = true;
        plan.hosts.truncate(MAX_AUTO_HOSTS);
    }
    plan
}

/// 日志友好的扫描计划描述（对应 Node 版 formatScanPlan）
pub fn format_scan_plan(plan: &ScanPlan, port: u16, peer_hint: &str, via_cidr: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if plan.explicit_target {
        lines.push(format!("扫描计划：仅探测 --target（端口 {port}）"));
        return lines;
    }
    let via = if via_cidr { "（--scan-cidr 指定）" } else { "" };
    lines.push(format!(
        "扫描计划{via}：{} 个网段，共 {} 个目标（端口 {port}）",
        plan.subnets.len(),
        plan.hosts.len()
    ));
    if !peer_hint.is_empty() && plan.hosts.iter().any(|h| h.to_string() == peer_hint) {
        lines.push(format!("  · 上次成功对端 {peer_hint}，优先尝试"));
    }
    for s in &plan.subnets {
        let who = if s.iface == "--scan-cidr" {
            String::new()
        } else {
            format!("\"{}\" {}  ", s.iface, s.address)
        };
        let note = if s.note.is_empty() { String::new() } else { format!("（{}）", s.note) };
        lines.push(format!("  · {who}{} → {} 台主机{note}", s.cidr, s.count));
    }
    for s in &plan.skipped {
        lines.push(format!("  · 跳过 {} {}：{}", s.iface, s.address, s.reason));
    }
    if plan.truncated {
        lines.push(format!("  · 目标数量超过上限，已截断为 {} 个", plan.hosts.len()));
    }
    if plan.subnets.is_empty() && plan.hosts.is_empty() {
        lines.push("  · 没有可扫描的网段：可用 --iface 指定网卡，或 --scan-cidr 直接指定网段".into());
    }
    lines
}

// ---- TCP 探测（对应 Node 版 probe / scanHosts：连接成功即命中） ----

pub fn probe(host: Ipv4Addr, port: u16, timeout_ms: u64, cancel: &AtomicBool) -> bool {
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    let addr = SocketAddrV4::new(host, port);
    TcpStream::connect_timeout(&addr.into(), Duration::from_millis(timeout_ms)).is_ok()
}

/// 并发探测池（worker 数 = min(concurrency, hosts.len())，对应 Node 版 scanHosts）
pub fn scan_hosts(
    hosts: &[Ipv4Addr],
    port: u16,
    timeout_ms: u64,
    concurrency: usize,
    cancel: &AtomicBool,
) -> Vec<Ipv4Addr> {
    if hosts.is_empty() {
        return Vec::new();
    }
    let n = concurrency.min(hosts.len()).max(1);
    let next = AtomicUsize::new(0);
    let hits: Mutex<Vec<Ipv4Addr>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= hosts.len() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                if probe(hosts[i], port, timeout_ms, cancel) {
                    hits.lock().unwrap().push(hosts[i]);
                }
            });
        }
    });
    hits.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, addr: &str, mask: &str) -> IfaceInfo {
        IfaceInfo {
            name: name.into(),
            addr: addr.parse().unwrap(),
            netmask: mask.parse().unwrap(),
            internal: false,
        }
    }

    #[test]
    fn virtual_filters() {
        assert!(is_virtual_iface("vEthernet (WSL)"));
        assert!(is_virtual_iface("本地连接* 1"));
        assert!(is_virtual_iface("蓝牙网络连接"));
        assert!(is_virtual_iface("Tailscale"));
        assert!(is_virtual_iface("utun3"));
        assert!(!is_virtual_iface("WLAN"));
        assert!(!is_virtual_iface("以太网"));
        assert!(!is_virtual_iface("en0"));
    }

    #[test]
    fn cidr_math() {
        let c = parse_cidr("192.168.31.0/24").unwrap();
        assert_eq!(c.network, ip_to_int(&"192.168.31.0".parse().unwrap()));
        assert_eq!(c.size, 256);
        assert_eq!(size_to_bits(256), 24);
        assert!(parse_cidr("192.168.31.0/33").is_none());
        assert!(parse_cidr("abc").is_none());
        assert_eq!(parse_cidr("10.0.0.5").unwrap().size, 1);
    }

    #[test]
    fn plan_excludes_self_and_virtual() {
        let list = [
            iface("WLAN", "192.168.31.100", "255.255.255.0"),
            iface("vEthernet (WSL)", "172.20.0.1", "255.255.255.0"),
            IfaceInfo { name: "环回".into(), addr: "127.0.0.1".parse().unwrap(), netmask: "255.0.0.0".parse().unwrap(), internal: true },
        ];
        let plan = plan_scan(&ScanInput {
            target: "",
            peer_hint: "",
            ifaces: &[],
            scan_cidr: &[],
            iface_list: &list,
        });
        // 只扫 WLAN 网段；/24 共 254 可用主机，排除自己后 253
        assert_eq!(plan.subnets.len(), 1);
        assert_eq!(plan.subnets[0].cidr, "192.168.31.0/24");
        assert_eq!(plan.subnets[0].count, 253);
        assert!(!plan.hosts.iter().any(|h| h.to_string() == "192.168.31.100"));
        assert!(plan.skipped.iter().any(|s| s.iface == "vEthernet (WSL)"));
    }

    #[test]
    fn scan_cidr_overrides_probe() {
        let plan = plan_scan(&ScanInput {
            target: "",
            peer_hint: "",
            ifaces: &[],
            scan_cidr: &["10.1.2.0/30".into()],
            iface_list: &[],
        });
        assert_eq!(plan.hosts.len(), 2); // /30 去掉网络地址与广播地址
    }

    #[test]
    fn fake_ip_excluded() {
        let list = [iface("Mihomo", "198.18.0.2", "255.255.255.0")];
        let plan = plan_scan(&ScanInput {
            target: "",
            peer_hint: "",
            ifaces: &[],
            scan_cidr: &[],
            iface_list: &list,
        });
        assert!(plan.hosts.is_empty());
        assert!(plan.skipped.iter().any(|s| s.reason.contains("fake-IP")));
    }

    #[test]
    fn target_wins() {
        let plan = plan_scan(&ScanInput {
            target: "1.2.3.4",
            peer_hint: "9.9.9.9",
            ifaces: &[],
            scan_cidr: &["10.0.0.0/8".into()],
            iface_list: &[],
        });
        assert_eq!(plan.hosts.len(), 1);
        assert!(plan.explicit_target);
    }
}
