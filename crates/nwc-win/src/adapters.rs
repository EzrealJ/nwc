//! Windows 网卡枚举：GetAdaptersAddresses → IfaceInfo（对应 Node 版 os.networkInterfaces 的来源）。
//! FriendlyName 参与虚拟网卡正则过滤（如 "vEthernet (WSL)"、"本地连接* 1"）。

#[cfg(windows)]
pub fn list_ifaces() -> Vec<nwc_core::discovery::IfaceInfo> {
    use nwc_core::discovery::IfaceInfo;
    use std::net::Ipv4Addr;
    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::GetAdaptersAddresses;
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GET_ADAPTERS_ADDRESSES_FLAGS,
        IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::AF_INET;
    use windows::core::PWSTR;

    const FLAGS: u32 = GAA_FLAG_SKIP_ANYCAST.0 | GAA_FLAG_SKIP_MULTICAST.0 | GAA_FLAG_SKIP_DNS_SERVER.0;

    let mut out = Vec::new();
    unsafe {
        let mut size: u32 = 16 * 1024;
        for _ in 0..4 {
            let mut buf = vec![0u8; size as usize];
            let rc = GetAdaptersAddresses(
                AF_INET.0 as u32,
                GET_ADAPTERS_ADDRESSES_FLAGS(FLAGS),
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            );
            if rc == ERROR_BUFFER_OVERFLOW.0 {
                continue; // size 已被更新，重试
            }
            if rc != 0 {
                return out;
            }
            let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !cur.is_null() {
                let ad = &*cur;
                let name = PWSTR(ad.FriendlyName.as_ptr()).to_string().unwrap_or_default();
                let internal = ad.IfType == IF_TYPE_SOFTWARE_LOOPBACK;
                let mut unicast = ad.FirstUnicastAddress;
                while !unicast.is_null() {
                    let u = &*unicast;
                    let sa = u.Address.lpSockaddr as *const windows::Win32::Networking::WinSock::SOCKADDR_IN;
                    if !sa.is_null() {
                        let sin = &*sa;
                        // S_addr 是网络序 u32；本机 LE 读出后 from_be 还原成 192.168.x.x 的 u32 形式
                        let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                        let prefix = u.OnLinkPrefixLength as u32; // 掩码位数
                        if prefix <= 32 {
                            out.push(IfaceInfo {
                                name: name.clone(),
                                addr,
                                netmask: prefix_to_mask(prefix),
                                internal,
                            });
                        }
                    }
                    unicast = u.Next;
                }
                cur = ad.Next;
            }
            break;
        }
    }
    out
}

#[cfg(windows)]
fn prefix_to_mask(bits: u32) -> std::net::Ipv4Addr {
    if bits == 0 {
        return std::net::Ipv4Addr::new(0, 0, 0, 0);
    }
    // Ipv4Addr::from(u32) 按 0xAABBCCDD → AA.BB.CC.DD 解释
    std::net::Ipv4Addr::from(u32::MAX << (32 - bits))
}

#[cfg(not(windows))]
pub fn list_ifaces() -> Vec<nwc_core::discovery::IfaceInfo> {
    Vec::new()
}
