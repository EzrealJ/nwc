//! 开机自启：HKCU\Software\Microsoft\Windows\CurrentVersion\Run（无需管理员/UAC）

#![cfg(windows)]

use winreg::enums::{KEY_READ, KEY_WRITE, HKEY_CURRENT_USER};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "NetWorkClipboard";

pub fn autostart_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    match hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) {
        Ok(k) => k.get_value::<String, _>(VALUE_NAME).is_ok(),
        Err(_) => false,
    }
}

pub fn set_autostart(enable: bool) -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE) else {
        return false;
    };
    if enable {
        let exe = std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned());
        let Some(exe) = exe else { return false };
        key.set_value(VALUE_NAME, &format!("\"{exe}\""))
            .is_ok()
    } else {
        let _ = key.delete_value(VALUE_NAME);
        true
    }
}
