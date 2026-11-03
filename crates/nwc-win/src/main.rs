//! nwc — NetWorkClipboard 原生版（Windows 壳）
//!
//! 用法：nwc.exe [选项]
//!   默认托盘模式（后台运行，日志写文件）；--cli 为控制台模式（日志直出，调试/双实例用）
//! 完整选项见 --help。
//!
//! PE 子系统说明：默认编译为 GUI 子系统（双击不弹黑终端）；debug 构建保留控制台便于开发。
//! --cli 模式会 AttachConsole 挂回父终端并重开 stdout/stderr，cmd/PowerShell 直跑也能看日志。

// release 双击不弹终端；debug 保留控制台（cargo run 可见日志）
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod adapters;
#[cfg(windows)]
mod autostart;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod dialog;
#[cfg(windows)]
mod history_win;
#[cfg(windows)]
mod icon;
#[cfg(windows)]
mod restart;
#[cfg(windows)]
mod tray;

use std::sync::mpsc;
use std::sync::Arc;

use nwc_core::config::{load_config, usage};
use nwc_core::sync::{Engine, EngineCmd, EngineEvent, Status};

#[cfg(windows)]
fn main() {
    std::process::exit(real_main());
}

#[cfg(windows)]
fn real_main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli_mode = argv.iter().any(|a| a == "--cli" || a == "--no-gui");
    if cli_mode {
        attach_parent_console();
    }
    let cfg = match load_config(&argv) {
        Ok(c) => c,
        Err(e) => {
            show_console_error(&e);
            return 1;
        }
    };
    if cfg.help {
        print!("{}", usage());
        return 0;
    }
    if cli_mode {
        return run_console(cfg);
    }
    // GUI（托盘）模式需要单实例：同名互斥量已存在则提示退出
    if acquire_single_instance() {
        message_box("NetWorkClipboard 已在运行（见系统托盘）", "NetWorkClipboard");
        return 0;
    }
    run_gui(cfg)
}

#[cfg(windows)]
fn show_console_error(msg: &str) {
    // GUI 子系统下 eprintln 不可见；直接弹窗 + stderr 双保险
    eprintln!("{msg}");
    message_box(msg, "NetWorkClipboard");
}

/// --cli 模式：GUI 子系统进程默认没有控制台。
/// 1) 有父终端（cmd/PowerShell 里启动）→ AttachConsole 挂回去；
///    Rust 标准库的 stdout 走 GetStdHandle，挂接后 println 直接可见
/// 2) 双击启动 --cli（无父终端）→ AllocConsole 自开一个，日志至少有去处
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{
        AllocConsole, AttachConsole, GetConsoleWindow, ATTACH_PARENT_PROCESS,
    };
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        if GetConsoleWindow().0 == 0 {
            let _ = AllocConsole();
        }
    }
}

#[cfg(windows)]
fn message_box(text: &str, title: &str) {
    use windows::core::PCWSTR;
    let mut t: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut ti: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
            None,
            PCWSTR(t.as_mut_ptr()),
            PCWSTR(ti.as_mut_ptr()),
            windows::Win32::UI::WindowsAndMessaging::MB_OK | windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING,
        );
    }
}

/// 单实例互斥量句柄（保存设置重启前要显式释放，见 restart.rs）
#[cfg(windows)]
pub static SINGLE_MUTEX_HANDLE: std::sync::Mutex<Option<windows::Win32::Foundation::HANDLE>> =
    std::sync::Mutex::new(None);

/// 返回 true = 已有实例在运行
#[cfg(windows)]
fn acquire_single_instance() -> bool {
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    let name: Vec<u16> = "NetWorkClipboard_nwc_single_instance\0".encode_utf16().collect();
    unsafe {
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(h) => {
                if windows::Win32::Foundation::GetLastError().0 == 183 {
                    // ERROR_ALREADY_EXISTS：别的进程持有
                    true
                } else {
                    // 我们是第一个：保存句柄以便重启前释放
                    *SINGLE_MUTEX_HANDLE.lock().unwrap() = Some(h);
                    false
                }
            }
            Err(_) => false,
        }
    }
}

use windows::core::PCWSTR;

/// 公共装配：剪贴板 + 引擎线程；返回 (cmd_tx, evt_rx, clipboard)
#[cfg(windows)]
fn assemble(mut cfg: nwc_core::config::Config) -> (mpsc::Sender<EngineCmd>, mpsc::Receiver<EngineEvent>, Arc<clipboard::WinClipboard>) {
    cfg.node_id = nwc_core::protocol::load_or_create_device_id(&cfg.state_dir);
    cfg.peer_hint = nwc_core::sync::load_peer_hint(&cfg);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<EngineEvent>();

    let (clip_tx, clip_rx) = mpsc::channel::<nwc_core::sync::ClipEvent>();
    // 剪贴板诊断（内容识别失败的原因/实际格式列表）→ 引擎事件通道（写成 Warn 日志）
    let (diag_tx, diag_rx) = mpsc::channel::<String>();
    let clip = clipboard::WinClipboard::start(clip_tx, Some(diag_tx));
    {
        let evt_tx2 = evt_tx.clone();
        std::thread::Builder::new()
            .name("nwc-clip-diag".into())
            .spawn(move || {
                while let Ok(m) = diag_rx.recv() {
                    if evt_tx2.send(EngineEvent::Warn(m)).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }
    {
        let fwd = cmd_tx.clone();
        std::thread::Builder::new()
            .name("nwc-clip-bridge".into())
            .spawn(move || {
                while let Ok(ev) = clip_rx.recv() {
                    if fwd.send(EngineCmd::Clip(ev)).is_err() {
                        break;
                    }
                }
            })
            .ok();
    }

    let writer: Arc<dyn nwc_core::sync::ClipboardWriter> =
        Arc::clone(&clip) as Arc<dyn nwc_core::sync::ClipboardWriter>;
    let ifaces: nwc_core::sync::IfaceProvider = Box::new(adapters::list_ifaces);

    let mut engine = Engine::new(cfg, writer, ifaces, cmd_rx, evt_tx);
    engine.set_cmd_sender(cmd_tx.clone());
    std::thread::Builder::new()
        .name("nwc-engine".into())
        .spawn(move || engine.run())
        .ok();

    (cmd_tx, evt_rx, clip)
}

/// 控制台模式：日志直出
#[cfg(windows)]
fn run_console(cfg: nwc_core::config::Config) -> i32 {
    let (_cmd_tx, evt_rx, clip) = assemble(cfg);
    while let Ok(ev) = evt_rx.recv() {
        let ts = nwc_core::timestamp::now_hms();
        match ev {
            EngineEvent::Log(m) => println!("[{ts}] {m}"),
            EngineEvent::Warn(m) => println!("[{ts}] [警告] {m}"),
            EngineEvent::Status(s) => println!("[{ts}] [状态] {}", status_label(&s)),
            EngineEvent::Notify(t, b) => println!("[{ts}] [通知] {t}: {b}"),
        }
    }
    clip.request_stop();
    0
}

#[cfg(windows)]
fn status_label(s: &Status) -> String {
    match s {
        Status::Starting => "启动中".into(),
        Status::Scanning => "扫描中".into(),
        Status::Connecting(ip) => format!("连接中 {ip}"),
        Status::Connected(name) => format!("已连接 {name}"),
        Status::Listening => "监听中".into(),
        Status::Paused => "已暂停".into(),
        Status::Stopped => "已停止".into(),
    }
}

/// 托盘模式：文件日志 + 托盘 + 状态/气泡转发
#[cfg(windows)]
fn run_gui(cfg: nwc_core::config::Config) -> i32 {
    let log_file = nwc_core::logger::FileLogger::new(&cfg.state_dir);
    log_file.prune(7);

    let cfg_path = cfg.state_dir.join("config.json");
    let state_dir = cfg.state_dir.clone();
    let inbox_dir = cfg.inbox_dir.clone();
    let log_path = log_file.latest_path();

    let (_cmd_tx, evt_rx, clip) = assemble(cfg);
    let tray = tray::start_tray(
        _cmd_tx.clone(),
        inbox_dir,
        log_path,
        cfg_path,
        state_dir,
    );

    let exit_code = {
        let mut paused = false;
        loop {
            match evt_rx.recv() {
                Ok(ev) => {
                    let ts = nwc_core::timestamp::now_hms();
                    match ev {
                        EngineEvent::Log(m) => log_file.write(&format!("[{ts}] {m}")),
                        EngineEvent::Warn(m) => log_file.write(&format!("[{ts}] [警告] {m}")),
                        EngineEvent::Status(s) => {
                            if s == Status::Paused {
                                paused = true;
                            } else if !matches!(s, Status::Stopped) {
                                paused = false;
                            }
                            *tray.shared.icon_kind.lock().unwrap() = icon::IconKind::from_status(&s);
                            *tray.shared.tooltip.lock().unwrap() = status_label(&s);
                            *tray.shared.paused.lock().unwrap() = paused;
                            post(tray.hwnd, tray::WM_UPDATE_STATUS);
                            log_file.write(&format!("[{ts}] [状态] {}", status_label(&s)));
                        }
                        EngineEvent::Notify(t, b) => {
                            *tray.shared.balloon.lock().unwrap() = Some((t.clone(), b.clone()));
                            post(tray.hwnd, tray::WM_NOTIFY);
                            log_file.write(&format!("[{ts}] [通知] {t}: {b}"));
                        }
                    }
                }
                Err(_) => break 0, // 引擎线程结束
            }
        }
    };
    clip.request_stop();
    exit_code
}

#[cfg(windows)]
fn post(hwnd: isize, msg: u32) {
    use windows::Win32::Foundation::{HWND, WPARAM};
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            HWND(hwnd),
            msg,
            WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("nwc-win 只能在 Windows 上运行（macOS 请用 nwc-mac）");
    std::process::exit(1);
}
