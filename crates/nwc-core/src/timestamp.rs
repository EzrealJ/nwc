//! 本地时间戳（日志文件名/行前缀用；避免引入 chrono 之类的重依赖）。
//!
//! - unix：libc::localtime_r
//! - Windows：Win32 GetLocalTime（与壳层原本的实现一致，windows crate 已锁定 0.57）

#[cfg(unix)]
fn local_now() -> (i32, u32, u32, u32, u32, u32) {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return (0, 0, 0, 0, 0, 0);
        }
        (
            tm.tm_year + 1900,
            tm.tm_mon as u32 + 1,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
            tm.tm_sec as u32,
        )
    }
}

#[cfg(windows)]
fn local_now() -> (i32, u32, u32, u32, u32, u32) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
        (
        st.wYear as i32,
        st.wMonth as u32,
        st.wDay as u32,
        st.wHour as u32,
        st.wMinute as u32,
        st.wSecond as u32,
    )
}

pub fn now_hms() -> String {
    let (_, _, _, h, m, s) = local_now();
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn now_date() -> String {
    let (y, mo, d, _, _, _) = local_now();
    format!("{y:04}-{mo:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_look_sane() {
        assert_eq!(now_hms().len(), 8);
        assert_eq!(now_date().len(), 10);
        assert!(now_date().starts_with("20"));
    }
}
