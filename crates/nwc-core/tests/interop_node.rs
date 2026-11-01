//! 与 Node 版（src/index.js）的真实互通回归。
//! 拉起 Node 进程作为对端，验证握手/认证/busy/心跳在字节层面互通。
//!
//! 注意：本测试不触发剪贴板写入（不发 text/image/file），不会动用户剪贴板；
//! Node 端启动时会照常 spawn PowerShell 剪贴板代理（只读监听，随进程退出）。

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nwc_core::protocol;
use nwc_core::transport::{PeerConnection, PeerHandler, PeerOpts};
use serde_json::Value;

fn repo_root() -> std::path::PathBuf {
    // 本仓库根上跳 2 级；若嵌回原 monorepo（native/crates/nwc-core），Node 版在 3 级之上
    let two = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let three = two.join("..");
    let p = if three.join("src/index.js").exists() { three } else { two };
    let c = p.canonicalize().unwrap_or(p);
    // canonicalize 在 Windows 产生 \\?\ 前缀，node 的模块加载不认，去掉
    let s = c.to_string_lossy();
    std::path::PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(s.as_ref()))
}

/// Node 版（src/index.js）不随本仓库分发；缺失时互通回归整体跳过
fn node_version_available() -> bool {
    repo_root().join("src/index.js").exists()
}

fn skip_if_no_node_version() -> bool {
    if !node_version_available() {
        eprintln!("skip: 未找到 Node 版（src/index.js），互通回归跳过");
        return true;
    }
    false
}

fn spawn_node(args: &[String]) -> (Child, Arc<Mutex<Vec<String>>>) {
    let mut cmd = Command::new("node");
    cmd.arg(repo_root().join("src/index.js"));
    for a in args {
        cmd.arg(a);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn node");
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().flatten() {
            sink.lock().unwrap().push(format!("[out] {line}"));
        }
    });
    let sink2 = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().flatten() {
            sink2.lock().unwrap().push(format!("[err] {line}"));
        }
    });
    (child, lines)
}

fn wait_log(lines: &Arc<Mutex<Vec<String>>>, pat: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if lines.lock().unwrap().iter().any(|l| l.contains(pat)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("--- node stdout ---\n{}", lines.lock().unwrap().join("\n"));
    false
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("bind :0").local_addr().expect("addr").port()
}

/// 带 deadline 的 accept（避免对端异常时测试永久挂起）。
/// Node 客户端的 TCP 探测（probe）也会先连一次再断开，探测连接握手必然失败，
/// 需要跳过后等真正的 hello 连接。
fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> Option<(TcpStream, std::net::SocketAddr)> {
    use std::io::Read;
    listener.set_nonblocking(true).ok();
    let deadline = Instant::now() + timeout;
    let mut probe_count = 0;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((s, a)) => {
                // 尝试读首个字节判断是否是探测连接：探测连接不会有数据（对端立即销毁）
                s.set_read_timeout(Some(Duration::from_millis(1500))).ok();
                let mut peek = [0u8; 1];
                let verdict = match s.peek(&mut peek) {
                    Ok(1) => true,  // 有数据 → 真 hello
                    Ok(_) => false,
                    Err(_) => false, // 对端已断（probe 探完即毁）
                };
                if verdict {
                    listener.set_nonblocking(false).ok();
                    s.set_read_timeout(None).ok();
                    eprintln!("[test] 跳过了 {probe_count} 个探测连接");
                    return Some((s, a));
                }
                probe_count += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    None
}

fn opts(token: &str) -> PeerOpts {
    PeerOpts {
        node_id: protocol::new_node_id(),
        name: "rust-test".into(),
        token: token.into(),
        heartbeat_ms: 5000,
        heartbeat_timeout_ms: 15000,
        caps: vec![],
    }
}

#[derive(Clone, Debug)]
enum Ev {
    Msg(Value),
    Bin(Value, usize),
    Closed(String),
}

#[derive(Default)]
struct Collector(Arc<Mutex<Vec<Ev>>>);

impl Collector {
    fn shared() -> (Arc<Collector>, Spy) {
        let c = Arc::new(Collector::default());
        (Arc::clone(&c), Spy { c })
    }
    fn wait<F: Fn(&Ev) -> bool>(&self, pred: F, timeout: Duration) -> Option<Ev> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(e) = self.0.lock().unwrap().iter().find(|e| pred(e)).cloned() {
                return Some(e);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("[test] wait 超时，已收到的事件: {:?}", self.0.lock().unwrap());
        None
    }
    fn any_closed(&self) -> bool {
        self.0.lock().unwrap().iter().any(|e| matches!(e, Ev::Closed(_)))
    }
}

struct Spy {
    c: Arc<Collector>,
}

impl PeerHandler for Spy {
    fn on_msg(&mut self, v: Value) {
        self.c.0.lock().unwrap().push(Ev::Msg(v));
    }
    fn on_bin(&mut self, header: Value, payload: Vec<u8>) {
        self.c.0.lock().unwrap().push(Ev::Bin(header, payload.len()));
    }
    fn on_closed(&mut self, reason: String) {
        self.c.0.lock().unwrap().push(Ev::Closed(reason));
    }
}

/// Node 作为服务端，Rust 作为客户端：握手 + 认证 + 我们发 ping / Node 回 pong + busy 拒绝
#[test]
fn rust_client_to_node_server() {
    if skip_if_no_node_version() {
        return;
    }
    let port = free_port();
    let (mut node, log) = spawn_node(&[
        "--role".into(),
        "server".into(),
        "--port".into(),
        port.to_string(),
        "--token".into(),
        "t1".into(),
    ]);
    assert!(wait_log(&log, "正在监听", Duration::from_secs(15)), "node server 未就绪");

    // 1. 正常握手 + 认证
    let acc = PeerConnection::connect("127.0.0.1", port, &opts("t1")).expect("握手应成功");
    assert!(!acc.peer.remote.node_id.is_empty(), "应拿到对端 nodeId");
    assert!(acc.peer.remote.caps.is_empty(), "Node 端不应有 caps 扩展");

    let (collector, spy) = Collector::shared();
    acc.peer.spawn_reader(acc.reader, acc.leftover, Box::new(spy));

    // 2. 心跳：发 ping（pong 在传输层被吃掉不上抛，与 Node 版一致），
    //    观察点是连接保持（若帧有问题 Node 会立刻断开并触发 Closed）
    assert!(acc.peer.send_msg(&protocol::MsgT("ping")));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!collector.any_closed(), "ping 后连接应保持");
    assert!(acc.peer.send_msg(&protocol::MsgT("ping")));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!collector.any_closed(), "第二次 ping 后连接应保持");

    // 3. busy：第一条连接活跃时第二条应被拒
    match PeerConnection::connect("127.0.0.1", port, &opts("t1")) {
        Err(e) => assert!(e.contains("对端已有其他连接"), "应报 busy，实际: {e}"),
        Ok(_) => panic!("第二条连接应被 busy 拒绝"),
    }

    // 4. Node 收到我们的 ping/pong 流量后连接仍保持
    std::thread::sleep(Duration::from_millis(300));
    assert!(!collector.any_closed());

    drop(acc.peer);
    let _ = node.kill();
    let _ = node.wait();
}

/// 错误口令：GCM 解密失败 → 连接断开 → 认证失败
#[test]
fn wrong_token_rejected() {
    if skip_if_no_node_version() {
        return;
    }
    let port = free_port();
    let (mut node, log) = spawn_node(&[
        "--role".into(),
        "server".into(),
        "--port".into(),
        port.to_string(),
        "--token".into(),
        "right".into(),
    ]);
    assert!(wait_log(&log, "正在监听", Duration::from_secs(15)));

    match PeerConnection::connect("127.0.0.1", port, &opts("wrong")) {
        Err(e) => assert!(e.contains("认证失败"), "应报认证失败，实际: {e}"),
        Ok(_) => panic!("口令不匹配应被拒绝"),
    }
    let _ = node.kill();
    let _ = node.wait();
}

/// Rust 作为服务端，Node 作为客户端：accept 握手 + Node 主动 ping / 我们自动回 pong
#[test]
fn node_client_to_rust_server() {
    if skip_if_no_node_version() {
        return;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let port = listener.local_addr().unwrap().port();

    let (mut node, log) = spawn_node(&[
        "--role".into(),
        "client".into(),
        "--target".into(),
        "127.0.0.1".into(),
        "--port".into(),
        port.to_string(),
        "--token".into(),
        "t1".into(),
    ]);
    // 给 node 一点启动时间（spawn PowerShell 剪贴板代理约 1 秒）
    std::thread::sleep(Duration::from_millis(1500));

    let (stream, _addr): (TcpStream, _) =
        match accept_with_timeout(&listener, Duration::from_secs(20)) {
            Some(r) => r,
            None => {
                eprintln!("--- node client stdout ---\n{}", log.lock().unwrap().join("\n"));
                panic!("accept node 超时：20 秒内没有收到真实连接");
            }
        };
    eprintln!("[test] accepted connection from {_addr}");
    stream.set_nodelay(true).ok();
    let acc = PeerConnection::accept(stream, &opts("t1")).expect("服务端握手应成功");
    assert!(!acc.peer.remote.node_id.is_empty());

    let (collector, spy) = Collector::shared();
    acc.peer.spawn_reader(acc.reader, acc.leftover, Box::new(spy));

    // Node 客户端完成握手后会打日志"已连接"；ping/pong 在传输层被吃掉不上抛，
    // 连接保持 8 秒即证明我们自动回 pong 正常（否则 Node 15 秒心跳超时会断开并记录）
    assert!(wait_log(&log, "已连接", Duration::from_secs(10)), "node 客户端应完成握手并记录已连接");
    assert!(!wait_log(&log, "连接已断开", Duration::from_secs(2)), "不应出现断开日志");
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!collector.any_closed(), "连接应保持（pong 自动回复正常）");

    drop(acc.peer);
    let _ = node.kill();
    let _ = node.wait();
}
