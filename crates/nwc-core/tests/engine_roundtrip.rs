//! 双引擎端到端：两个 Engine 经真实 TCP（127.0.0.1）互连，验证「本地剪贴板事件 →
//! 传输加密 → 对端写入」的组合链路（组合可用性由单元/平台测试 + 本层共同保障）。

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nwc_core::config::Config;
use nwc_core::protocol;
use nwc_core::sync::{ClipEvent, ClipboardWriter, Engine, EngineCmd, EngineEvent, Status};

/// 记录型 writer：把对端写到的内容记下来供断言
#[derive(Default)]
struct RecordingWriter {
    texts: Mutex<Vec<String>>,
    images: Mutex<Vec<Vec<u8>>>,
    files: Mutex<Vec<Vec<PathBuf>>>,
}

impl ClipboardWriter for RecordingWriter {
    fn set_text(&self, s: &str) -> bool {
        self.texts.lock().unwrap().push(s.to_string());
        true
    }
    fn set_image(&self, png: &[u8]) -> bool {
        self.images.lock().unwrap().push(png.to_vec());
        true
    }
    fn set_files(&self, paths: &[PathBuf]) -> bool {
        self.files.lock().unwrap().push(paths.to_vec());
        true
    }
}

static PORT_SEQ: AtomicU64 = AtomicU64::new(0);

fn free_port() -> u16 {
    // 竞态窗口极小；冲突时连接建立断言会失败，重跑即可
    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

struct Node {
    cmd: mpsc::Sender<EngineCmd>,
    evt: mpsc::Receiver<EngineEvent>,
    writer: Arc<RecordingWriter>,
    done: mpsc::Receiver<()>,
}

fn spawn_node(role: &str, target: &str, port: u16) -> Node {
    let mut cfg = Config::default();
    cfg.node_id = format!("{}-{}", protocol::new_node_id(), PORT_SEQ.fetch_add(1, Ordering::SeqCst));
    cfg.port = port;
    cfg.token = "e2e".into();
    cfg.role = role.into();
    cfg.target = target.into();
    let dir = std::env::temp_dir().join(format!("nwc-e2e-{}-{}", std::process::id(), cfg.node_id));
    cfg.state_dir = dir.clone();
    cfg.inbox_dir = dir.join("inbox");
    cfg.scan_interval_ms = 100;
    cfg.startup_scan_ms = 100;
    cfg.heartbeat_ms = 1000;
    cfg.heartbeat_timeout_ms = 5000;
    cfg.allow_listen = Some(true);

    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let writer = Arc::new(RecordingWriter::default());
    let mut engine = Engine::new(cfg, Arc::clone(&writer) as Arc<dyn ClipboardWriter>, Box::new(Vec::new), cmd_rx, evt_tx);
    engine.set_cmd_sender(cmd_tx.clone());
    std::thread::spawn(move || {
        engine.run();
        let _ = done_tx.send(());
    });
    Node { cmd: cmd_tx, evt: evt_rx, writer, done: done_rx }
}

fn wait_connected(evt: &mpsc::Receiver<EngineEvent>, who: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        assert!(Instant::now() < deadline, "{who} 应在 8 秒内建立连接");
        match evt.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status(Status::Connected(_))) => return,
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{who} 引擎线程提前退出"),
        }
    }
}

fn wait_for<T>(mut probe: impl FnMut() -> Option<T>, timeout: Duration, what: &str) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = probe() {
            return v;
        }
        assert!(Instant::now() < deadline, "等待 {what} 超时");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn shutdown(n: Node) {
    let _ = n.cmd.send(EngineCmd::Shutdown);
    let _ = n.done.recv_timeout(Duration::from_secs(3));
}

/// 造一个 2×2 PNG（core 依赖 png，测试直接编码）
fn sample_png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 2u32, 2u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 128])
            .unwrap();
    }
    out
}

#[test]
fn engine_e2e_text_and_image_sync() {
    let port = free_port();
    let server = spawn_node("server", "", port);
    let client = spawn_node("client", "127.0.0.1", port);

    wait_connected(&client.evt, "client");
    wait_connected(&server.evt, "server");

    // ① client 复制文本 → server 收到并写入
    client.cmd.send(EngineCmd::Clip(ClipEvent::Text("端到端文本 ✓".into()))).unwrap();
    let got = wait_for(
        || server.writer.texts.lock().unwrap().first().cloned(),
        Duration::from_secs(5),
        "server 收到文本",
    );
    assert_eq!(got, "端到端文本 ✓");

    // ② server 复制图片 → client 收到并写入（字节级一致）
    let png = sample_png();
    server.cmd.send(EngineCmd::Clip(ClipEvent::Image(png.clone()))).unwrap();
    let got_img = wait_for(
        || client.writer.images.lock().unwrap().first().cloned(),
        Duration::from_secs(5),
        "client 收到图片",
    );
    assert_eq!(got_img, png, "图片应字节级一致");

    // ③ 反向再来一次文本（双向都通）
    server.cmd.send(EngineCmd::Clip(ClipEvent::Text("reverse".into()))).unwrap();
    let got2 = wait_for(
        || client.writer.texts.lock().unwrap().iter().any(|t| t == "reverse").then_some(()),
        Duration::from_secs(5),
        "client 收到反向文本",
    );
    assert_eq!(got2, ());

    shutdown(client);
    shutdown(server);
    // 清理测试 state 目录（history.json 等）
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("nwc-e2e-{}", std::process::id())));
}
