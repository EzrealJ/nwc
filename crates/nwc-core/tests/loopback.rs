//! 纯 Rust 双端回环：Rust 服务端 + Rust 客户端，验证握手/帧收发/心跳不依赖 Node。

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nwc_core::protocol;
use nwc_core::transport::{PeerConnection, PeerHandler, PeerOpts};
use serde_json::Value;

fn opts(token: &str, name: &str) -> PeerOpts {
    PeerOpts {
        node_id: protocol::new_node_id(),
        name: name.into(),
        token: token.into(),
        heartbeat_ms: 5000,
        heartbeat_timeout_ms: 15000,
        caps: vec![],
    }
}

#[derive(Debug, Clone)]
enum Ev {
    Msg(Value),
    Bin(Value, usize),
    Closed(String),
}

#[derive(Default)]
struct Coll(Mutex<Vec<Ev>>);

impl Coll {
    fn wait_t<F: Fn(&Ev) -> bool>(&self, pred: F, timeout: Duration) -> Option<Ev> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(e) = self.0.lock().unwrap().iter().find(|e| pred(e)).cloned() {
                return Some(e);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("events: {:?}", self.0.lock().unwrap());
        None
    }
}

struct H(Arc<Coll>);
impl PeerHandler for H {
    fn on_msg(&mut self, v: Value) {
        self.0 .0.lock().unwrap().push(Ev::Msg(v));
    }
    fn on_bin(&mut self, header: Value, payload: Vec<u8>) {
        self.0 .0.lock().unwrap().push(Ev::Bin(header, payload.len()));
    }
    fn on_closed(&mut self, reason: String) {
        self.0 .0.lock().unwrap().push(Ev::Closed(reason));
    }
}

#[test]
fn rust_to_rust_full_flow() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let server_coll = Arc::new(Coll::default());
    let sc = Arc::clone(&server_coll);
    // 服务端连接暴露出来，供测试反向发送
    let server_peer: Arc<Mutex<Option<Arc<PeerConnection>>>> = Arc::new(Mutex::new(None));
    let sp = Arc::clone(&server_peer);
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let acc = PeerConnection::accept(stream, &opts("t1", "rust-server")).unwrap();
        acc.peer.spawn_reader(acc.reader, acc.leftover, Box::new(H(sc)));
        *sp.lock().unwrap() = Some(acc.peer);
    });

    let acc = PeerConnection::connect("127.0.0.1", port, &opts("t1", "rust-client"))
        .expect("rust↔rust 握手");
    assert_eq!(acc.peer.remote.name, "rust-server");
    assert_eq!(acc.peer.remote.node_id.len(), 16);

    let client_coll = Arc::new(Coll::default());
    let cc = Arc::clone(&client_coll);
    acc.peer.spawn_reader(acc.reader, acc.leftover, Box::new(H(cc)));

    // 客户端 → 服务端：文本消息（ping/pong 在传输层被吃掉不上抛，与 Node 版一致，故用 text 验证）
    use nwc_core::protocol::MsgText;
    assert!(acc.peer.send_msg(&MsgText { t: "text", data: "你好 nwc" }));
    let got = server_coll.wait_t(
        |e| matches!(e, Ev::Msg(m) if m["t"] == "text" && m["data"] == "你好 nwc"),
        Duration::from_secs(3),
    );
    assert!(got.is_some(), "服务端应收到文本消息");

    // 服务端 → 客户端
    let deadline = Instant::now() + Duration::from_secs(3);
    let srv = loop {
        if let Some(p) = server_peer.lock().unwrap().clone() {
            break p;
        }
        assert!(Instant::now() < deadline, "服务端连接未就绪");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(srv.send_msg(&MsgText { t: "text", data: "回声" }));
    let got2 = client_coll.wait_t(
        |e| matches!(e, Ev::Msg(m) if m["t"] == "text" && m["data"] == "回声"),
        Duration::from_secs(3),
    );
    assert!(got2.is_some(), "客户端应收到服务端文本");

    // 大帧：256KB 二进制帧（含 kind 头 + JSON header + 0x0A 分界）
    let big: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    assert!(acc.peer.send_bin(&serde_json::json!({"t":"image","mime":"image/png","size":big.len()}), &big));
    let got_bin = server_coll.wait_t(|e| matches!(e, Ev::Bin(h, n) if h["t"]=="image" && *n == 256*1024), Duration::from_secs(5));
    assert!(got_bin.is_some(), "服务端应收到 256KB 二进制帧");

    // 分帧到达：连发 3 个不同尺寸的帧，全部按序到达
    for size in [10usize, 1000, 70000] {
        let payload: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
        assert!(acc.peer.send_bin(&serde_json::json!({"t":"image","seq":size,"size":size}), &payload));
    }
    for size in [10usize, 1000, 70000] {
        let got = server_coll.wait_t(|e| matches!(e, Ev::Bin(h, n) if h["seq"]==size && *n==size), Duration::from_secs(5));
        assert!(got.is_some(), "帧 seq={size} 未按序到达");
    }

    std::thread::sleep(Duration::from_millis(200));
    assert!(!client_coll.0.lock().unwrap().iter().any(|e| matches!(e, Ev::Closed(_))));
    assert!(!server_coll.0.lock().unwrap().iter().any(|e| matches!(e, Ev::Closed(_))));
}

#[test]
fn rust_to_rust_caps_negotiated() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server_coll = Arc::new(Coll::default());
    let sc = Arc::clone(&server_coll);
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut o = opts("t1", "srv");
        o.caps = vec![protocol::CAP_FILE_CHUNK];
        let acc = PeerConnection::accept(stream, &o).unwrap();
        acc.peer.spawn_reader(acc.reader, acc.leftover, Box::new(H(sc)));
    });
    let mut co = opts("t1", "cli");
    co.caps = vec![protocol::CAP_FILE_CHUNK];
    let acc = PeerConnection::connect("127.0.0.1", port, &co).unwrap();
    assert!(acc.peer.remote.has_cap(protocol::CAP_FILE_CHUNK), "应协商出 fileChunk 能力");
}

/// 回归：连接活跃期间收到 Shutdown，engine.run() 必须能返回
/// （旧 bug：Shutdown 是一次性命令，被 session() 消费后外层循环再也看不到，
///  引擎线程卡死 → 壳层等事件通道关闭也跟着挂 → 点托盘“退出”进程残留）
#[test]
fn engine_shutdown_during_session_exits() {
    use std::sync::mpsc;
    use nwc_core::config::Config;
    use nwc_core::sync::{ClipEvent, ClipboardWriter, Engine, EngineCmd, EngineEvent, Status};

    struct NoopWriter;
    impl ClipboardWriter for NoopWriter {
        fn set_text(&self, _s: &str) -> bool { true }
        fn set_image(&self, _p: &[u8]) -> bool { true }
        fn set_files(&self, _p: &[std::path::PathBuf]) -> bool { true }
    }

    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);

    let tmp = std::env::temp_dir().join(format!("nwc-shutdown-{}", std::process::id()));
    let mk_cfg = |role: &str, target: &str| {
        let mut c = Config::default();
        c.node_id = protocol::new_node_id(); // 两端必须不同，否则握手判“同一实例”
        c.port = port;
        c.token = "t".into();
        c.role = role.into();
        c.target = target.into();
        c.state_dir = tmp.join(role);
        c.inbox_dir = tmp.join(role).join("inbox");
        c.scan_interval_ms = 100;
        c.startup_scan_ms = 100;
        c.heartbeat_ms = 1000;
        c.heartbeat_timeout_ms = 5000;
        c.allow_listen = Some(true);
        c
    };

    let spawn_engine = |cfg: Config| -> (mpsc::Sender<EngineCmd>, mpsc::Receiver<EngineEvent>, mpsc::Receiver<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut engine = Engine::new(
            cfg,
            Arc::new(NoopWriter),
            Arc::new(Vec::new),
            cmd_rx,
            evt_tx,
        );
        engine.set_cmd_sender(cmd_tx.clone());
        let fwd = cmd_tx.clone();
        std::thread::spawn(move || {
            // 无剪贴板源：直接喂一个无害事件让管线跑起来即可（省去平台层）
            let _ = fwd.send(EngineCmd::Clip(ClipEvent::Text("boot".into())));
            engine.run();
            let _ = done_tx.send(());
        });
        (cmd_tx, evt_rx, done_rx)
    };

    let (s_tx, s_evt, s_done) = spawn_engine(mk_cfg("server", ""));
    let (c_tx, c_evt, c_done) = spawn_engine(mk_cfg("client", "127.0.0.1"));

    // 等客户端进入已连接（最多 8 秒）
    let deadline = Instant::now() + Duration::from_secs(8);
    let connected = loop {
        if Instant::now() > deadline { break false; }
        match c_evt.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status(Status::Connected(_))) => break true,
            Ok(_) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break false,
        }
    };
    assert!(connected, "回环连接应建立（否则后续断言无意义）");

    // 连接活跃时发 Shutdown：run() 必须在 3 秒内返回
    c_tx.send(EngineCmd::Shutdown).unwrap();
    let exited = c_done.recv_timeout(Duration::from_secs(3)).is_ok();
    assert!(exited, "连接期间收到 Shutdown，engine.run() 应能返回（旧实现卡死在 session→外层循环）");

    // 收尾：让服务端也退出
    let _ = s_tx.send(EngineCmd::Shutdown);
    let _ = s_done.recv_timeout(Duration::from_secs(3));
    let _ = std::fs::remove_dir_all(&tmp);
}
