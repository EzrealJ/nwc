//! macOS 网卡枚举：getifaddrs（BSD 名参与虚拟网卡过滤：utun/bridge/awdl…）

#![cfg(target_os = "macos")]

use std::net::Ipv4Addr;

use nwc_core::discovery::IfaceInfo;

#[repr(C)]
struct Ifaddrs {
    ifa_next: *mut Ifaddrs,
    ifa_name: *mut std::ffi::c_char,
    ifa_flags: u32,
    ifa_addr: *mut SockaddrInet,
    ifa_netmask: *mut SockaddrInet,
    _ifa_ifu: *mut SockaddrInet,
    _ifa_data: *mut std::ffi::c_void,
}

#[repr(C)]
struct SockaddrInet {
    sa_family: u16,
    _sa_data: [u8; 14],
}

#[repr(C)]
struct SockaddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: InAddr,
    sin_zero: [u8; 8],
}

#[repr(C)]
struct InAddr {
    s_addr: u32, // 网络序
}

unsafe extern "C" {
    fn getifaddrs(ifap: *mut *mut Ifaddrs) -> i32;
    fn freeifaddrs(ifa: *mut Ifaddrs);
}

fn loopback_mask() -> Ipv4Addr {
    "255.0.0.0".parse().unwrap()
}

pub fn list_ifaces() -> Vec<IfaceInfo> {
    let mut out = Vec::new();
    unsafe {
        let mut head: *mut Ifaddrs = std::ptr::null_mut();
        if getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_name.is_null() && !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family == 2 /* AF_INET */ {
                let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                let sin = &*(ifa.ifa_addr as *const SockaddrIn);
                let bytes = sin.sin_addr.s_addr.to_be_bytes(); // 网络序 → 逐字节
                let addr = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
                let internal = addr.is_loopback();
                let netmask = if ifa.ifa_netmask.is_null() {
                    "255.255.255.0".parse().unwrap()
                } else {
                    let sn = &*(ifa.ifa_netmask as *const SockaddrIn);
                    let mb = sn.sin_addr.s_addr.to_be_bytes();
                    Ipv4Addr::new(mb[0], mb[1], mb[2], mb[3])
                };
                let _ = loopback_mask();
                out.push(IfaceInfo { name, addr, netmask, internal });
            }
            cur = ifa.ifa_next;
        }
        freeifaddrs(head);
    }
    out
}
