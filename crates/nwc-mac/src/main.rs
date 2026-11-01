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
    cfg.node_id = nwc_core::protocol::new_node_id();
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
    let ifaces: nwc_core::sync::IfaceProvider = Box::new(adapters::list_ifaces);

    let cfg_path = cfg.state_dir.join("config.json");
    let inbox_dir = cfg.inbox_dir.clone();
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
        // 菜单栏模式（主线程跑 AppKit）
        statusbar::run(cmd_tx, evt_rx, inbox_dir, cfg_path);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("nwc-mac 只能在 macOS 上运行（Windows 请用 nwc-win）");
    std::process::exit(1);
}
