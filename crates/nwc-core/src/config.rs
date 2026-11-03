//! 配置（字段与默认值对应 Node 版 src/config.js；config.json 键名兼容）。
//!
//! 差异：原生版配置文件固定在 `~/.networkclipboard/config.json`（Node 版在项目根目录），
//! 可用 `--config <路径>` 覆盖（测试/双实例用）。
//! 另修复 Node 版一个潜在 bug：`--scan-interval` 在 Node 里写入的是 scanInterval
//! （从未被读取，实际生效的是 scanIntervalMs）；原生版让它正确映射到扫描间隔。

use std::path::PathBuf;

use crate::protocol::MAX_FRAME;
use crate::util::fmt_bytes;

/// 单帧含 JSON 头与加密开销，文件上限必须留出余量（对应 Node 版 FRAME_CAP）
pub const FRAME_CAP: u64 = MAX_FRAME as u64 - 64 * 1024;

pub const DEFAULT_PORT: u16 = 8767;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub token: String,
    pub name: String,
    pub role: String, // auto | server | client
    pub allow_listen: Option<bool>,
    pub target: String,
    pub ifaces: Vec<String>,
    pub scan_cidr: Vec<String>,
    pub scan_timeout_ms: u64,
    pub scan_concurrency: usize,
    pub scan_interval_ms: u64,
    pub startup_scan_ms: u64,
    pub heartbeat_ms: u64,
    pub heartbeat_timeout_ms: u64,
    pub poll_ms: u64,
    pub batch_idle_ms: u64,
    pub max_text_bytes: u64,
    pub max_image_bytes: u64,
    pub max_file_bytes: u64,
    pub max_transfer_bytes: u64,
    pub max_transfer_files: u64,
    /// 文字剪贴板历史（默认开，20 条）
    pub history_enabled: bool,
    pub history_limit: u64,
    /// 收件箱自动清理：保留天数（收件箱是中转缓存而非归档；0 = 关闭自动清理）
    pub inbox_keep_days: u64,
    pub state_dir: PathBuf,
    pub inbox_dir: PathBuf,
    // 运行时状态
    pub peer_hint: String,
    pub node_id: String,
    pub help: bool,
}

impl Default for Config {
    fn default() -> Self {
        let home = home_dir();
        Config {
            port: DEFAULT_PORT,
            token: String::new(),
            name: default_name(),
            role: "auto".into(),
            allow_listen: None,
            target: String::new(),
            ifaces: Vec::new(),
            scan_cidr: Vec::new(),
            scan_timeout_ms: 300,
            scan_concurrency: 256,
            scan_interval_ms: 5000,
            startup_scan_ms: 3000,
            heartbeat_ms: 5000,
            heartbeat_timeout_ms: 15000,
            poll_ms: 400,
            batch_idle_ms: 30000,
            max_text_bytes: 2 * 1024 * 1024,
            max_image_bytes: 20 * 1024 * 1024,
            max_file_bytes: 100 * 1024 * 1024,
            max_transfer_bytes: 500 * 1024 * 1024,
            max_transfer_files: 5000,
            history_enabled: true,
            history_limit: 20,
            inbox_keep_days: 14,
            state_dir: home.join(".networkclipboard"),
            inbox_dir: home.join("NetWorkClipboard").join("inbox"),
            peer_hint: String::new(),
            node_id: String::new(),
            help: false,
        }
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

/// Windows 默认不监听（防火墙原则），除非显式 --allow-listen；对应 Node 版 canListenDefault。
pub fn can_listen_default(cfg: &Config) -> bool {
    match cfg.allow_listen {
        Some(v) => v,
        None => !cfg!(target_os = "windows"),
    }
}

const NUM_KEYS: &[&str] = &[
    "port",
    "scan-timeout-ms",
    "scan-concurrency",
    "scan-interval",
    "startup-scan-ms",
    "heartbeat-ms",
    "heartbeat-timeout-ms",
    "poll-ms",
    "batch-idle-ms",
    "max-text-bytes",
    "max-image-bytes",
    "max-file-bytes",
    "max-transfer-bytes",
    "max-transfer-files",
    "inbox-keep-days",
];

/// kebab → camel（对应 Node 版 parseArgs 的转换；scan-interval 特判修正，见模块注释）
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for c in key.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Default)]
struct Cli {
    values: Vec<(String, JsonV)>,
    help: bool,
    allow_listen: Option<bool>,
    config_path: Option<String>,
}

/// 极简 JSON 值（只取配置需要的类型；文件层直接用 serde_json::Value 合并）
type JsonV = serde_json::Value;

fn parse_args(argv: &[String]) -> Cli {
    let mut out = Cli::default();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if !a.starts_with("--") {
            i += 1;
            continue;
        }
        let key = &a[2..];
        if key == "help" {
            out.help = true;
            i += 1;
            continue;
        }
        if key == "allow-listen" {
            out.allow_listen = Some(true);
            i += 1;
            continue;
        }
        if key == "cli" || key == "no-gui" {
            // GUI 开关由 shell 处理，这里跳过
            i += 1;
            continue;
        }
        let val = argv.get(i + 1);
        let val = match val {
            Some(v) if !v.starts_with("--") => {
                i += 2;
                v.clone()
            }
            _ => {
                i += 1;
                continue;
            }
        };
        if key == "config" {
            out.config_path = Some(val);
            continue;
        }
        // 修复 Node 版 bug：--scan-interval 在 Node 里写进从未被读取的 scanInterval；
        // 这里直接映射到真正生效的 scanIntervalMs
        let ck = if key == "scan-interval" {
            "scanIntervalMs".to_string()
        } else {
            camel(key)
        };
        // iface/scan-cidr 支持逗号分隔与重复使用（累加）
        if ck == "iface" || ck == "ifaces" || ck == "scanCidr" {
            out.values.push((ck, serde_json::Value::String(val)));
            continue;
        }
        let v = if NUM_KEYS.contains(&key) {
            serde_json::Value::from(val.parse::<f64>().unwrap_or(0.0))
        } else {
            serde_json::Value::String(val)
        };
        out.values.retain(|(k, _)| k != &ck);
        out.values.push((ck, v));
    }
    out
}

fn to_list(v: Option<&JsonV>) -> Vec<String> {
    match v {
        None | Some(JsonV::Null) => Vec::new(),
        Some(JsonV::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        Some(JsonV::String(s)) if s.is_empty() => Vec::new(),
        Some(JsonV::String(s)) => s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
        _ => Vec::new(),
    }
}

fn u64_of(v: Option<&JsonV>) -> Option<u64> {
    match v {
        Some(JsonV::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Some(JsonV::String(s)) => s.parse::<u64>().ok(),
        _ => None,
    }
}

/// 默认 < config.json < CLI（对应 Node 版 loadConfig 的合并次序）
pub fn load_config(argv: &[String]) -> Result<Config, String> {
    let cli = parse_args(argv);
    let mut cfg = Config::default();

    let cfg_path = cli
        .config_path
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.state_dir.join("config.json"));
    if cfg_path.exists() {
        let text = std::fs::read_to_string(&cfg_path).map_err(|e| format!("config.json 读取失败: {e}"))?;
        let file: JsonV = serde_json::from_str(&text).map_err(|e| format!("config.json 解析失败: {e}"))?;
        if let JsonV::Object(map) = file {
            apply(&mut cfg, &serde_json::Value::Object(map));
        }
    }

    // CLI 覆盖
    let mut cli_obj = serde_json::Map::new();
    let mut ifaces: Vec<String> = Vec::new();
    let mut scan_cidr: Vec<String> = Vec::new();
    for (k, v) in &cli.values {
        if k == "iface" || k == "ifaces" {
            if let Some(s) = v.as_str() {
                ifaces.extend(s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
            }
            continue;
        }
        if k == "scanCidr" {
            if let Some(s) = v.as_str() {
                scan_cidr.extend(s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()));
            }
            continue;
        }
        cli_obj.insert(k.clone(), v.clone());
    }
    if !ifaces.is_empty() {
        cli_obj.insert("ifaces".into(), serde_json::Value::Array(ifaces.into_iter().map(serde_json::Value::String).collect()));
    }
    if !scan_cidr.is_empty() {
        cli_obj.insert(
            "scanCidr".into(),
            serde_json::Value::Array(scan_cidr.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    apply(&mut cfg, &serde_json::Value::Object(cli_obj));

    if cli.help {
        cfg.help = true;
        return Ok(cfg);
    }
    if let Some(v) = cli.allow_listen {
        cfg.allow_listen = Some(v);
    }

    // 校验与钳制（对应 Node 版 loadConfig 的后半段）
    if cfg.port == 0 {
        return Err("无效端口: 0".into());
    }
    if !matches!(cfg.role.as_str(), "auto" | "server" | "client") {
        return Err(format!("无效角色: {}（可选 auto/server/client）", cfg.role));
    }
    let mut clamp_with_warn = |cur: &mut u64, name: &str, dflt: u64| {
        let v = if *cur < 1 { dflt } else { (*cur).min(FRAME_CAP) };
        if v != *cur {
            eprintln!(
                "[警告] {name} 无效或超过单帧上限，已按 {} 生效（单帧上限 {}）",
                fmt_bytes(v),
                fmt_bytes(FRAME_CAP)
            );
        }
        *cur = v;
    };
    clamp_with_warn(&mut cfg.max_text_bytes, "maxTextBytes", 2 * 1024 * 1024);
    clamp_with_warn(&mut cfg.max_image_bytes, "maxImageBytes", 20 * 1024 * 1024);
    clamp_with_warn(&mut cfg.max_file_bytes, "maxFileBytes", 100 * 1024 * 1024);
    if cfg.max_transfer_bytes < 1 {
        cfg.max_transfer_bytes = 500 * 1024 * 1024;
    }
    if cfg.max_transfer_files < 1 {
        cfg.max_transfer_files = 5000;
    }
    cfg.batch_idle_ms = cfg.batch_idle_ms.clamp(1000, 600000);
    if cfg.name.is_empty() {
        cfg.name = default_name();
    }
    Ok(cfg)
}

/// 把 JSON 对象的键合入 cfg（camelCase 键名，与 Node 版 config.json 相同）
fn apply(cfg: &mut Config, v: &JsonV) {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return,
    };
    let get = |k: &str| obj.get(k);
    if let Some(p) = u64_of(get("port")) {
        // 越界值存 0，由 load_config 统一报"无效端口"
        cfg.port = if (1..=65535).contains(&p) { p as u16 } else { 0 };
    }
    if let Some(t) = get("token").and_then(JsonV::as_str) {
        cfg.token = t.to_string();
    }
    if let Some(n) = get("name").and_then(JsonV::as_str) {
        cfg.name = n.to_string();
    }
    if let Some(r) = get("role").and_then(JsonV::as_str) {
        cfg.role = r.to_string();
    }
    if let Some(al) = get("allowListen").and_then(JsonV::as_bool) {
        cfg.allow_listen = Some(al);
    }
    if let Some(t) = get("target").and_then(JsonV::as_str) {
        cfg.target = t.to_string();
    }
    if let Some(v) = get("ifaces").or_else(|| get("iface")) {
        let l = to_list(Some(v));
        if !l.is_empty() {
            cfg.ifaces = l;
        }
    }
    if let Some(v) = get("scanCidr") {
        let l = to_list(Some(v));
        if !l.is_empty() {
            cfg.scan_cidr = l;
        }
    }
    let nums: &[(&str, fn(&mut Config, u64), u64, u64)] = &[
        ("scanTimeoutMs", |c, v| c.scan_timeout_ms = v, 1, 60000),
        ("scanConcurrency", |c, v| c.scan_concurrency = v.max(1) as usize, 1, 4096),
        ("scanIntervalMs", |c, v| c.scan_interval_ms = v, 100, 600000),
        ("startupScanMs", |c, v| c.startup_scan_ms = v, 0, 600000),
        ("heartbeatMs", |c, v| c.heartbeat_ms = v, 100, 600000),
        ("heartbeatTimeoutMs", |c, v| c.heartbeat_timeout_ms = v, 100, 600000),
        ("pollMs", |c, v| c.poll_ms = v, 50, 600000),
        ("batchIdleMs", |c, v| c.batch_idle_ms = v, 1000, 600000),
    ];
    for (key, f, lo, hi) in nums {
        if let Some(v) = u64_of(get(key)) {
            f(cfg, v.clamp(*lo, *hi));
        }
    }
    let limits: &[(&str, fn(&mut Config, u64), u64, u64)] = &[
        ("maxTextBytes", |c, v| c.max_text_bytes = v, 1, FRAME_CAP),
        ("maxImageBytes", |c, v| c.max_image_bytes = v, 1, FRAME_CAP),
        ("maxFileBytes", |c, v| c.max_file_bytes = v, 1, FRAME_CAP),
        ("maxTransferBytes", |c, v| c.max_transfer_bytes = v, 1, u64::MAX),
        ("maxTransferFiles", |c, v| c.max_transfer_files = v, 1, u64::MAX),
    ];
    for (key, f, lo, hi) in limits {
        if let Some(v) = u64_of(get(key)) {
            f(cfg, v.max(*lo).min(*hi));
        }
    }
    if let Some(v) = get("historyEnabled").and_then(JsonV::as_bool) {
        cfg.history_enabled = v;
    }
    if let Some(v) = u64_of(get("historyLimit")) {
        cfg.history_limit = v.clamp(1, 100);
    }
    if let Some(v) = u64_of(get("inboxKeepDays")) {
        cfg.inbox_keep_days = v.min(3650);
    }
    if let Some(d) = get("inboxDir").and_then(JsonV::as_str) {
        if !d.is_empty() {
            cfg.inbox_dir = PathBuf::from(d);
        }
    }
    if let Some(d) = get("stateDir").and_then(JsonV::as_str) {
        if !d.is_empty() {
            cfg.state_dir = PathBuf::from(d);
        }
    }
}

pub fn usage() -> String {
    format!(
        "NetWorkClipboard 原生版 - 局域网剪贴板共享（macOS ↔ Windows）\n\
         \n\
         用法: nwc [选项]\n\
         \n\
         选项:\n\
           --token <口令>     双方一致的共享口令（AES-256-GCM 密钥来源，务必一致）\n\
           --role <角色>      auto | server | client（默认 auto：启动时自动抉择）\n\
           --port <端口>      监听/扫描端口（默认 {DEFAULT_PORT}，两端需一致）\n\
           --name <名称>      本机显示名（默认主机名）\n\
           --target <IP>      只连接指定 IP（调试可用 127.0.0.1 做本机双实例）\n\
           --iface <网卡名>   只扫描指定网卡（逗号分隔或重复使用；写了就绕过虚拟网卡过滤）\n\
           --scan-cidr <网段> 只扫描指定网段，如 192.168.31.0/24（逗号分隔；写了就忽略自动探测）\n\
           --allow-listen     允许在 Windows 上监听（默认禁止，避免防火墙弹窗）\n\
           --config <路径>    指定配置文件（默认 ~/.networkclipboard/config.json）\n\
           --cli              无界面控制台模式（仅日志，无托盘）\n\
           --scan-interval <毫秒>  断线重扫间隔（默认 5000）\n\
           --batch-idle-ms <毫秒>  接收批次的空闲兜底间隔（默认 30000）\n         --inbox-keep-days <天> 收件箱自动清理保留天数（默认 14，0 关闭）\n\
           --help             显示本帮助\n\
         \n\
         也可以把选项写进 ~/.networkclipboard/config.json（键名同长选项，驼峰形式）。\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn cli_overrides_and_kebab() {
        let cfg = load_config(&s(&["--token", "abc", "--scan-cidr", "192.168.1.0/24,10.0.0.0/24", "--port", "9000"]))
            .unwrap();
        assert_eq!(cfg.token, "abc");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.scan_cidr, vec!["192.168.1.0/24", "10.0.0.0/24"]);
    }

    #[test]
    fn scan_interval_maps_correctly() {
        // Node 版 bug：--scan-interval 写入从未被读取的 scanInterval；这里应正确生效
        let cfg = load_config(&s(&["--scan-interval", "2000"])).unwrap();
        assert_eq!(cfg.scan_interval_ms, 2000);
    }

    #[test]
    fn iface_repeatable() {
        let cfg = load_config(&s(&["--iface", "WLAN", "--iface", "以太网"])).unwrap();
        assert_eq!(cfg.ifaces, vec!["WLAN", "以太网"]);
    }

    #[test]
    fn busy_role_rejected() {
        assert!(load_config(&s(&["--role", "server"])).is_ok());
        assert!(load_config(&s(&["--role", "xyz"])).is_err());
    }

    #[test]
    fn max_file_bytes_clamped() {
        let cfg = load_config(&s(&["--max-file-bytes", "999999999"])).unwrap();
        assert_eq!(cfg.max_file_bytes, FRAME_CAP);
    }
}
