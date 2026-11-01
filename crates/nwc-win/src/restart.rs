//! 保存设置后的自动重启。
//!
//! 顺序很关键：先 CloseHandle 释放单实例互斥量（否则新进程会撞上"已在运行"提示框——
//! 这正是早期版本保存后报错的根因：cmd timeout 在无控制台环境会立即失败，
//! 新实例在旧进程退出前就启动，互斥量还没释放）。然后直接 spawn 新实例，再退出自己。

#![cfg(windows)]

use windows::Win32::Foundation::CloseHandle;

pub fn restart_process() -> ! {
    // 1) 释放单实例锁（互斥量引用计数归零即销毁，新实例的 CreateMutexW 不会撞车）
    if let Some(h) = crate::SINGLE_MUTEX_HANDLE.lock().unwrap().take() {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
    // 2) 拉起新实例（直接 spawn，无 cmd 中转、无窗口）
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    // 3) 退出旧进程（托盘/剪贴板窗口随进程销毁）
    std::process::exit(0);
}
