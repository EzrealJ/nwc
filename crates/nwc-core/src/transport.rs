//! 连接与握手（对应 Node 版 src/transport.js）。
//!
//! 内部实现差异（不影响线上字节）：帧读取用增量缓冲（Node 版逐 chunk 拼接在大传输下是
//! O(n²)）；心跳与读循环跑在读线程上，事件通过 PeerHandler 回调上抛。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::crypto::{self, IV_LEN, KEY_LEN, TAG_LEN};
use crate::protocol::{self, MsgT};

pub const HANDSHAKE_TIMEOUT_MS: u64 = 8000;
pub const CONNECT_TIMEOUT_MS: u64 = 5000;
const READ_TICK_MS: u64 = 200;

#[derive(Clone)]
pub struct PeerOpts {
    pub node_id: String,
    pub name: String,
    pub token: String,
    pub heartbeat_ms: u64,
    pub heartbeat_timeout_ms: u64,
    /// 本端声明的能力（写进 hello/hello_ack；空则与 Node 版字节一致）
    pub caps: Vec<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub struct PeerInfo {
    pub node_id: String,
    pub name: String,
    pub caps: Vec<String>,
}

impl PeerInfo {
    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }
}

pub trait PeerHandler: Send + 'static {
    fn on_msg(&mut self, v: Value);
    fn on_bin(&mut self, header: Value, payload: Vec<u8>);
    fn on_closed(&mut self, reason: String);
}

pub struct PeerConnection {
    writer: Mutex<TcpStream>,
    key: [u8; KEY_LEN],
    pub remote: PeerInfo,
    /// (heartbeat_ms, heartbeat_timeout_ms)
    hb: (u64, u64),
    closed: AtomicBool,
}

/// 带缓冲的读取端：握手阶段同步读，之后移交读线程
struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

enum ReadOutcome {
    Line(String),
    Frame(Vec<u8>),
    /// EOF / IO 错误 / 非法帧 / 解密失败 / 握手超时
    Fail(String),
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        Conn { stream, buf: Vec::with_capacity(16 * 1024) }
    }

    fn read_line(&mut self, deadline: Instant) -> ReadOutcome {
        self.read_until(deadline, true, None)
    }

    fn read_frame(&mut self, deadline: Instant, key: &[u8; KEY_LEN]) -> ReadOutcome {
        match self.read_until(deadline, false, Some(key)) {
            ReadOutcome::Fail(e) => ReadOutcome::Fail(e),
            ReadOutcome::Line(_) => unreachable!(),
            ReadOutcome::Frame(f) => ReadOutcome::Frame(f),
        }
    }

    /// want_line=true：读到首个 0x0A 返回一行；否则读满一个完整加密帧并解密。
    fn read_until(&mut self, deadline: Instant, want_line: bool, key: Option<&[u8; KEY_LEN]>) -> ReadOutcome {
        self.stream
            .set_read_timeout(Some(Duration::from_millis(READ_TICK_MS)))
            .ok();
        loop {
            if want_line {
                if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = self.buf.drain(..=i).collect();
                    return ReadOutcome::Line(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
                }
            } else if self.buf.len() >= 4 {
                let len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
                if len == 0 || len > protocol::MAX_FRAME {
                    return ReadOutcome::Fail(format!("非法帧长度: {len}"));
                }
                let total = 4 + IV_LEN + TAG_LEN + len;
                if self.buf.len() >= total {
                    let iv: Vec<u8> = self.buf[4..16].to_vec();
                    let tag: Vec<u8> = self.buf[16..32].to_vec();
                    let ct_start = 4 + IV_LEN + TAG_LEN;
                    let ct: Vec<u8> = self.buf[ct_start..total].to_vec();
                    self.buf.drain(..total);
                    let k = key.expect("key for frame");
                    return match crypto::decrypt_frame(k, &iv, &tag, &ct) {
                        Some(pt) => ReadOutcome::Frame(pt),
                        None => ReadOutcome::Fail("解密失败：口令不一致或数据被篡改".into()),
                    };
                }
            }
            if Instant::now() >= deadline {
                return ReadOutcome::Fail("握手超时".into());
            }
            let mut tmp = [0u8; 16384];
            match self.stream.read(&mut tmp) {
                Ok(0) => return ReadOutcome::Fail("连接已断开".into()),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) =>
                    if is_tick(&e) {
                        continue;
                    } else {
                        return ReadOutcome::Fail(format!("连接错误: {e}"));
                    },
            }
        }
    }
}

fn is_tick(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

fn parse_json_line(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}

fn caps_from(v: &Value) -> Vec<String> {
    v.get("caps")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// 握手完成产物：连接对象 + 读线程用的流 + 握手期间已缓冲的残留数据
pub struct Accepted {
    pub peer: Arc<PeerConnection>,
    pub reader: TcpStream,
    pub leftover: Vec<u8>,
}

impl PeerConnection {
    /// 客户端角色：TCP 连接 → hello/hello_ack → 加密 auth/auth_ok。
    /// 错误文案与 Node 版一致（busy/同一实例/版本不匹配/认证失败/握手超时）。
    pub fn connect(host: &str, port: u16, opts: &PeerOpts) -> Result<Accepted, String> {
        let stream = connect_socket(host, port, CONNECT_TIMEOUT_MS)?;
        let mut conn = Conn::new(stream);
        let deadline = Instant::now() + Duration::from_millis(HANDSHAKE_TIMEOUT_MS);

        let mut salt_c = [0u8; 16];
        getrandom::getrandom(&mut salt_c).map_err(|e| format!("getrandom: {e}"))?;
        let hello = proto_line(&protocol::make_hello(
            &opts.node_id,
            &opts.name,
            &crypto::b64(&salt_c),
            &opts.caps,
        ));
        if let Err(e) = conn.stream.write_all(&hello) {
            return Err(format!("写入失败: {e}"));
        }

        // 等一行：hello_ack 或 busy
        let ack = match conn.read_line(deadline) {
            ReadOutcome::Line(l) => l,
            ReadOutcome::Fail(e) => return Err(e),
            ReadOutcome::Frame(_) => unreachable!(),
        };
        let v = parse_json_line(&ack).ok_or_else(|| "对端不是本程序或版本不匹配".to_string())?;
        if v.get("t").and_then(Value::as_str) == Some("busy") {
            return Err("对端已有其他连接，稍后重试".into());
        }
        if !protocol::is_hello_msg(&v, "hello_ack") {
            return Err("对端不是本程序或版本不匹配".into());
        }
        let remote_node = v["nodeId"].as_str().unwrap_or("").to_string();
        if remote_node == opts.node_id {
            return Err("检测到同一实例".into());
        }
        let salt_s_b64 = v["salt"].as_str().unwrap_or("");
        let salt_s = crypto::unb64(salt_s_b64).ok_or_else(|| "对端不是本程序或版本不匹配".to_string())?;
        let key = crypto::derive_key(&opts.token, &salt_c, &salt_s);

        let remote = PeerInfo {
            node_id: remote_node,
            name: v["name"].as_str().unwrap_or("").to_string(),
            caps: caps_from(&v),
        };

        // 进入加密帧模式：发 auth，等 auth_ok
        let auth = crypto::encrypt_frame(&key, &protocol::json_msg_bytes(&MsgT("auth")));
        if let Err(e) = conn.stream.write_all(&auth) {
            return Err(format!("写入失败: {e}"));
        }
        match conn.read_frame(deadline, &key) {
            ReadOutcome::Frame(pt) => {
                if pt.first() == Some(&protocol::KIND_JSON) {
                    let m: Value = match serde_json::from_slice(&pt[1..]) {
                        Ok(m) => m,
                        Err(_) => return Err("认证失败：口令不匹配或连接中断".into()),
                    };
                    if m.get("t").and_then(Value::as_str) != Some("auth_ok") {
                        return Err("认证失败：口令不匹配".into());
                    }
                } else {
                    return Err("认证失败：口令不匹配".into());
                }
            }
            ReadOutcome::Fail(_) => return Err("认证失败：口令不匹配或连接中断".into()),
            ReadOutcome::Line(_) => unreachable!(),
        }

        let peer = PeerConnection::from_parts(conn.stream, key, remote, opts)?;
        let reader = peer.writer.lock().unwrap().try_clone().map_err(|e| e.to_string())?;
        Ok(Accepted { peer, reader, leftover: conn.buf })
    }

    /// 服务端角色：已 accept 的流上完成握手。busy 拒绝由调用方（服务循环）负责。
    pub fn accept(stream: TcpStream, opts: &PeerOpts) -> Result<Accepted, String> {
        let mut conn = Conn::new(stream);
        let deadline = Instant::now() + Duration::from_millis(HANDSHAKE_TIMEOUT_MS);

        let line = match conn.read_line(deadline) {
            ReadOutcome::Line(l) => l,
            ReadOutcome::Fail(_) => return Err("未收到握手请求".into()),
            ReadOutcome::Frame(_) => unreachable!(),
        };
        let v = parse_json_line(&line).ok_or_else(|| "对端不是本程序或版本不匹配".to_string())?;
        if !protocol::is_hello_msg(&v, "hello") {
            return Err("对端不是本程序或版本不匹配".into());
        }
        let remote_node = v["nodeId"].as_str().unwrap_or("").to_string();
        if remote_node == opts.node_id {
            return Err("检测到同一实例".into());
        }
        let salt_c = crypto::unb64(v["salt"].as_str().unwrap_or(""))
            .ok_or_else(|| "对端不是本程序或版本不匹配".to_string())?;

        let mut salt_s = [0u8; 16];
        getrandom::getrandom(&mut salt_s).map_err(|e| format!("getrandom: {e}"))?;
        let key = crypto::derive_key(&opts.token, &salt_c, &salt_s);
        let ack = proto_line(&protocol::make_hello_ack(
            &opts.node_id,
            &opts.name,
            &crypto::b64(&salt_s),
            &opts.caps,
        ));
        if let Err(e) = conn.stream.write_all(&ack) {
            return Err(format!("写入失败: {e}"));
        }

        // 等 auth，回 auth_ok
        match conn.read_frame(deadline, &key) {
            ReadOutcome::Frame(pt) => {
                let ok = pt.first() == Some(&protocol::KIND_JSON)
                    && serde_json::from_slice::<Value>(&pt[1..])
                        .map(|m| m.get("t").and_then(Value::as_str) == Some("auth"))
                        .unwrap_or(false);
                if !ok {
                    return Err("认证失败：口令不匹配".into());
                }
            }
            ReadOutcome::Fail(_) => return Err("认证失败：口令不匹配或连接中断".into()),
            ReadOutcome::Line(_) => unreachable!(),
        }
        let auth_ok = crypto::encrypt_frame(&key, &protocol::json_msg_bytes(&MsgT("auth_ok")));
        if let Err(e) = conn.stream.write_all(&auth_ok) {
            return Err(format!("写入失败: {e}"));
        }

        let remote = PeerInfo {
            node_id: remote_node,
            name: v["name"].as_str().unwrap_or("").to_string(),
            caps: caps_from(&v),
        };
        let peer = PeerConnection::from_parts(conn.stream, key, remote, opts)?;
        let reader = peer.writer.lock().unwrap().try_clone().map_err(|e| e.to_string())?;
        Ok(Accepted { peer, reader, leftover: conn.buf })
    }

    fn from_parts(
        stream: TcpStream,
        key: [u8; KEY_LEN],
        remote: PeerInfo,
        opts: &PeerOpts,
    ) -> Result<Arc<Self>, String> {
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        Ok(Arc::new(PeerConnection {
            writer: Mutex::new(writer),
            key,
            remote,
            hb: (opts.heartbeat_ms, opts.heartbeat_timeout_ms),
            closed: AtomicBool::new(false),
        }))
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 主动关闭（对应 Node 版 destroy）：标记 + 关闭套接字（读线程会随之退出并回调 on_closed）
    pub fn close(&self) {
        self.mark_closed();
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
    }

    /// 发送 JSON 消息（kind=1）。失败返回 false 并关闭连接。
    pub fn send_msg(&self, obj: &impl Serialize) -> bool {
        let plain = protocol::json_msg_bytes(obj);
        self.send_plain(&plain)
    }

    /// 发送二进制帧（kind=2）。失败返回 false 并关闭连接。
    pub fn send_bin(&self, header: &impl Serialize, payload: &[u8]) -> bool {
        let plain = protocol::bin_msg_bytes(header, payload);
        self.send_plain(&plain)
    }

    fn send_plain(&self, plain: &[u8]) -> bool {
        if self.is_closed() {
            return false;
        }
        let frame = crypto::encrypt_frame(&self.key, plain);
        let mut w = self.writer.lock().unwrap();
        if std::env::var_os("NWC_DEBUG").is_some() {
            eprintln!("[dbg] send_plain: plain={}B frame={}B", plain.len(), frame.len());
        }
        match w.write_all(&frame).and_then(|_| w.flush()) {
            Ok(_) => true,
            Err(_) => {
                self.mark_closed();
                let _ = w.shutdown(std::net::Shutdown::Both);
                false
            }
        }
    }

    fn mark_closed(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// 启动读线程：帧分发 + ping/pong + 心跳超时检测。on_closed 保证恰好回调一次。
    /// leftover 是握手期间已读入但未消费的缓冲（对应 Node 版 _enterFrames 的残留处理）。
    pub fn spawn_reader(self: &Arc<Self>, mut stream: TcpStream, leftover: Vec<u8>, handler: Box<dyn PeerHandler>) {
        let peer = Arc::clone(self);
        std::thread::Builder::new()
            .name("nwc-peer-read".into())
            .spawn(move || {
                peer.read_loop(&mut stream, leftover, handler);
            })
            .expect("spawn peer reader");
    }

    fn read_loop(self: Arc<Self>, stream: &mut TcpStream, leftover: Vec<u8>, mut handler: Box<dyn PeerHandler>) {
        let mut buf: Vec<u8> = if leftover.is_empty() {
            Vec::with_capacity(64 * 1024)
        } else {
            leftover
        };
        let hb_ms = self.hb.0;
        let hb_timeout = self.hb.1;
        let mut last_recv = Instant::now();
        let mut last_ping = Instant::now();
        let mut close_reason = String::from("连接已断开");
        stream.set_read_timeout(Some(Duration::from_millis(READ_TICK_MS))).ok();
        let mut tmp = [0u8; 16384];

        'outer: loop {
            // 解析缓冲中所有完整帧
            loop {
                if buf.len() < 4 {
                    break;
                }
                let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
                if len == 0 || len > protocol::MAX_FRAME {
                    close_reason = format!("非法帧长度: {len}");
                    break 'outer;
                }
                let total = 4 + IV_LEN + TAG_LEN + len;
                if buf.len() < total {
                    break;
                }
                let iv: Vec<u8> = buf[4..16].to_vec();
                let tag: Vec<u8> = buf[16..32].to_vec();
                let ct: Vec<u8> = buf[32..total].to_vec();
                buf.drain(..total);
                let pt = match crypto::decrypt_frame(&self.key, &iv, &tag, &ct) {
                    Some(p) => p,
                    None => {
                        close_reason = "解密失败：口令不一致或数据被篡改".into();
                        break 'outer;
                    }
                };
                let kind = pt[0];
                let body = pt[1..].to_vec();
                if kind == protocol::KIND_JSON {
                    let m: Value = match serde_json::from_slice(&body) {
                        Ok(m) => m,
                        Err(_) => continue, // Node 版：JSON 解析失败跳过该帧，不断连
                    };
                    last_recv = Instant::now();
                    let t = m.get("t").and_then(Value::as_str).unwrap_or("");
                    if t == "ping" {
                        self.send_msg(&MsgT("pong"));
                        continue;
                    }
                    if t == "pong" {
                        continue;
                    }
                    handler.on_msg(m);
                } else if kind == protocol::KIND_BIN {
                    last_recv = Instant::now();
                    match protocol::parse_bin(&body) {
                        Some((header, payload)) => handler.on_bin(header, payload.to_vec()),
                        None => {
                            close_reason = "非法二进制帧".into();
                            break 'outer;
                        }
                    }
                }
            }

            // 心跳检测（对应 Node 版 _startHeartbeat：超时断开；每 heartbeatMs 发 ping）
            if last_recv.elapsed().as_millis() as u64 > hb_timeout {
                close_reason = "心跳超时".into();
                break;
            }
            if last_ping.elapsed().as_millis() as u64 >= hb_ms {
                last_ping = Instant::now();
                if !self.send_msg(&MsgT("ping")) {
                    close_reason = "发送失败".into();
                    break;
                }
            }

            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    if std::env::var_os("NWC_DEBUG").is_some() {
                        eprintln!("[dbg] read_loop got {n}B (buf={}B)", buf.len() + n);
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) =>
                    if is_tick(&e) {
                        continue;
                    } else {
                        close_reason = format!("连接错误: {e}");
                        break;
                    },
            }
        }
        self.mark_closed();
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        handler.on_closed(close_reason);
    }
}

fn proto_line(json_bytes: &[u8]) -> Vec<u8> {
    let mut v = json_bytes.to_vec();
    v.push(b'\n');
    v
}

fn connect_socket(host: &str, port: u16, timeout_ms: u64) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("地址解析失败: {e}"))?
        .collect();
    let mut last = String::from("连接超时");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)) {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = if e.kind() == std::io::ErrorKind::TimedOut { "连接超时".into() } else { e.to_string() },
        }
    }
    Err(last)
}
