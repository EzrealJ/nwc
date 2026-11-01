//! 开机自启：LaunchAgent plist（~/Library/LaunchAgents/com.networkclipboard.nwc.plist）

#![cfg(target_os = "macos")]

use std::path::PathBuf;

fn plist_path() -> PathBuf {
    nwc_core::config::home_dir().join("Library/LaunchAgents/com.networkclipboard.nwc.plist")
}

pub fn autostart_enabled() -> bool {
    plist_path().exists()
}

pub fn set_autostart(enable: bool) -> bool {
    let p = plist_path();
    if enable {
        let Ok(exe) = std::env::current_exe() else { return false };
        let exe = exe.to_string_lossy().replace('\"', "");
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>com.networkclipboard.nwc</string>\n  <key>ProgramArguments</key>\n  <array><string>{exe}</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><false/>\n</dict>\n</plist>\n"
        );
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(&p, body).is_ok()
    } else {
        let _ = std::fs::remove_file(&p);
        true
    }
}
