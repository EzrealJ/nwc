//! nwc-mac — NetWorkClipboard 原生版（macOS 壳）
//! NSStatusItem 菜单栏 + NSPasteboard（changeCount 进程内轮询，无子进程）。
//!
//! 构建：cargo build --release -p nwc-mac   （需在 macOS 上）

#[cfg(target_os = "macos")]
mod adapters;
#[cfg(target_os = "macos")]
mod autostart;
#[cfg(target_os = "macos")]
mod pasteboard;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod statusbar;

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::mpsc;
    use std::sync::Arc;

    use nwc_core::config::{load_config, usage};
    use nwc_core::sync::{Engine, EngineCmd, EngineEvent};

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = argv.iter().any(|a| a == "--cli" || a == "--no-gui");
    let mut cfg = match load_config(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if cfg.help {
        print!("{}", usage());
        return;
    }
    cfg.node_id = nwc_core::protocol::load_or_create_device_id(&cfg.state_dir);
    cfg.peer_hint = nwc_core::sync::load_peer_hint(&cfg);

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCmd>();
    let (evt_tx, evt_rx) = mpsc::channel::<EngineEvent>();

    // 剪贴板轮询线程（changeCount，进程内，无 osascript 子进程）
    let (clip_tx, clip_rx) = mpsc::channel::<nwc_core::sync::ClipEvent>();
    let poll_ms = cfg.poll_ms;
    let pb = pasteboard::MacPasteboard::start(clip_tx, poll_ms);
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
        pb as Arc<dyn nwc_core::sync::ClipboardWriter>;
    let ifaces: nwc_core::sync::IfaceProvider = Arc::new(adapters::list_ifaces);

    let cfg_path = cfg.state_dir.join("config.json");
    let inbox_dir = cfg.inbox_dir.clone();
    let state_dir = cfg.state_dir.clone();
    let mut engine = Engine::new(cfg, writer, ifaces, cmd_rx, evt_tx);
    engine.set_cmd_sender(cmd_tx.clone());
    std::thread::Builder::new()
        .name("nwc-engine".into())
        .spawn(move || engine.run())
        .ok();

    if cli {
        // 控制台模式
        while let Ok(ev) = evt_rx.recv() {
            match ev {
                EngineEvent::Log(m) => println!("{m}"),
                EngineEvent::Warn(m) => println!("[警告] {m}"),
                EngineEvent::Status(_) => {}
                EngineEvent::Notify(t, b) => println!("[通知] {t}: {b}"),
            }
        }
    } else {
        // 菜单栏模式单实例：state_dir/instance.lock 加 flock（进程退出自动释放）。
        // 双实例会互抢剪贴板回声抑制，必须拦下（对齐 Windows 端的命名互斥量）。
        // 句柄存全局：设置保存后 restart_process 需先释放再拉新实例。
        match acquire_single_instance(&state_dir) {
            Some(f) => *SINGLE_LOCK.lock().unwrap() = Some(f),
            None => {
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg("display alert \"NetWorkClipboard 已在运行（见菜单栏）\"")
                    .status();
                return;
            }
        }
        // 菜单栏模式（主线程跑 AppKit）：GUI 无控制台，日志落文件（按天滚动、保留 7 天）
        let logger = nwc_core::logger::FileLogger::new(&state_dir);
        logger.prune(7);
        let log_path = logger.latest_path();
        statusbar::run(cmd_tx, evt_rx, inbox_dir, cfg_path, logger, log_path);
    }
}

/// 单实例锁句柄（进程存活期间持有；重启时主动释放）
#[cfg(target_os = "macos")]
static SINGLE_LOCK: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// 释放单实例锁 → 拉起新实例 → 退出（设置保存后按新配置生效）
#[cfg(target_os = "macos")]
pub fn restart_process() {
    // 先释放 flock，再启动新进程（否则新实例会被单实例保护拦下）
    if let Some(f) = SINGLE_LOCK.lock().unwrap().take() {
        drop(f);
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
/// 菜单栏模式单实例锁：flock(state_dir/instance.lock)，返回 Some(句柄) = 本机首个实例。
/// 句柄须在 main 作用域持有；进程退出（含崩溃）时内核自动释放锁。
fn acquire_single_instance(state_dir: &std::path::Path) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    let _ = std::fs::create_dir_all(state_dir);
    let f = std::fs::File::create(state_dir.join("instance.lock")).ok()?;
    let fd = f.as_raw_fd();
    let r = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        Some(f)
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("nwc-mac 只能在 macOS 上运行（Windows 请用 nwc-win）");
    std::process::exit(1);
}
