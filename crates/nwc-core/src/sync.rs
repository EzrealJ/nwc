//! 同步引擎（对应 Node 版 src/index.js 的主循环与全部同步管线）。
//!
//! 单线程事件循环：剪贴板事件、对端事件、控制命令都经同一个 channel 串行处理
//! （等价于 Node 版的 sendChain / binChain 串行化语义）。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::{can_listen_default, Config};
use crate::crypto;
use crate::discovery::{self, IfaceInfo, ScanInput};
use crate::echo::EchoGuard;
use crate::protocol::{self, CAP_FILE_CHUNK};
use crate::transport::{PeerConnection, PeerHandler, PeerOpts};
use crate::util::{fmt_bytes, hash_text, is_png, sha256_hex, Hasher};

/// 分块传输的块大小（Rust↔Rust 协商成功时使用，内存 O(块) 而非 O(整个文件)）
pub const CHUNK_SIZE: usize = 256 * 1024;
/// 传输缓冲复用的初始容量
const IO_BUF: usize = 256 * 1024;

// ---- 与壳层的接口 ----

#[derive(Debug)]
pub enum ClipEvent {
    Text(String),
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
}

pub enum EngineCmd {
    Clip(ClipEvent),
    PeerMsg(Value),
    PeerBin(Value, Vec<u8>),
    PeerClosed(String),
    Pause,
    Resume,
    /// 从历史记录重新粘贴：写本地剪贴板并同步到对端
    PasteText(String),
    /// 删除单条历史（按文本匹配；列表内已去重，文本即唯一键）
    RemoveHistory(String),
    /// 清空历史
    ClearHistory,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Starting,
    Scanning,
    Connecting(String),
    Connected(String), // "name (ip)"
    Listening,
    Paused,
    Stopped,
}

pub enum EngineEvent {
    Log(String),
    Warn(String),
    Status(Status),
    /// 气泡通知 (标题, 正文)
    Notify(String, String),
}

pub trait ClipboardWriter: Send + Sync {
    fn set_text(&self, s: &str) -> bool;
    fn set_image(&self, png: &[u8]) -> bool;
    fn set_files(&self, paths: &[PathBuf]) -> bool;
}

pub type IfaceProvider = Box<dyn Fn() -> Vec<IfaceInfo> + Send>;

pub struct Engine {
    cfg: Config,
    echo: EchoGuard,
    writer: Arc<dyn ClipboardWriter>,
    ifaces: IfaceProvider,
    rx: Receiver<EngineCmd>,
    tx: Sender<EngineEvent>,
    /// 引擎命令通道的发送端克隆（供对端读线程桥接用）
    tx_cmd: Sender<EngineCmd>,
    conn: Option<Arc<PeerConnection>>,
    conn_closed: bool,
    batches: HashMap<String, Batch>,
    paused: bool,
    stop: Arc<AtomicBool>,
    last_peer_label: String,
    history: Vec<crate::history::HistoryEntry>,
    /// 接收分块传输的进行中文件（batchId, rel) -> 状态
    incoming: HashMap<(String, String), IncomingFile>,
}

struct IncomingFile {
    ri: usize,
    size: u64,
    sha256: String,
    written: u64,
    hasher: Hasher,
    file: std::fs::File,
    tmp: PathBuf,
}

#[derive(Default)]
struct BatchRoot {
    dest: PathBuf,
    files: u64,
    bytes: u64,
    paths: HashSet<PathBuf>,
    logged_files: u64,
    logged_bytes: u64,
}

struct Batch {
    count: u64,
    got: u64,
    files: u64,
    bytes: u64,
    aborted: bool,
    empty_logged: bool,
    items: Vec<PathBuf>,
    roots: HashMap<String, BatchRoot>,
    hashes: Vec<String>,
    idle_at: Instant,
    touched: Instant,
}

impl Batch {
    fn new(count: u64) -> Self {
        Batch {
            count,
            got: 0,
            files: 0,
            bytes: 0,
            aborted: false,
            empty_logged: false,
            items: Vec::new(),
            roots: HashMap::new(),
            hashes: Vec::new(),
            idle_at: Instant::now(),
            touched: Instant::now(),
        }
    }
}

/// 对端事件 → 引擎 channel 的桥（跑在读线程上）
struct PeerBridge {
    tx: Sender<EngineCmd>,
    stop: Arc<AtomicBool>,
}

/// 占位 sender（new 里 tx_cmd 初始化用，set_cmd_sender 会被壳层立即覆盖）
fn rx_dummy() -> Sender<EngineCmd> {
    let (tx, _rx) = std::sync::mpsc::channel();
    tx
}

impl PeerHandler for PeerBridge {
    fn on_msg(&mut self, v: Value) {
        let _ = self.tx.send(EngineCmd::PeerMsg(v));
    }
    fn on_bin(&mut self, header: Value, payload: Vec<u8>) {
        let _ = self.tx.send(EngineCmd::PeerBin(header, payload));
    }
    fn on_closed(&mut self, reason: String) {
        let _ = self.tx.send(EngineCmd::PeerClosed(reason));
    }
}

const JUNK_FILES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

// ---- 路径消毒（对应 Node 版 safeName / resolveRel / uniqueDestPath / isInsideInbox） ----

fn safe_name(name: &str) -> String {
    let mut n: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // 去掉尾部空格和点
    while n.ends_with(' ') || n.ends_with('.') {
        n.pop();
    }
    n = n.trim().to_string();
    if n.is_empty() {
        n = "file".into();
    }
    if cfg!(windows) {
        // 对应 Node 版 /^(con|prn|aux|nul|com\d|lpt\d)(\.|$)/i：com/lpt 只跟单个数字
        let lower = n.to_lowercase();
        let stem = lower.split('.').next().unwrap_or("");
        let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
            || ((stem.starts_with("com") || stem.starts_with("lpt"))
                && stem.len() == 4
                && stem.as_bytes()[3].is_ascii_digit());
        if reserved {
            n = format!("_{n}");
        }
    }
    // 截断到 180 字符后再清一次结尾的点/空格
    if n.chars().count() > 180 {
        n = n.chars().take(180).collect();
        while n.ends_with(' ') || n.ends_with('.') {
            n.pop();
        }
    }
    if n.is_empty() {
        n = "file".into();
    }
    n
}

fn resolve_rel(rel: &str) -> Vec<String> {
    let mut segs = Vec::new();
    for raw in rel.split('/') {
        if raw.is_empty() || raw == "." || raw == ".." {
            continue;
        }
        segs.push(safe_name(raw));
    }
    segs.truncate(64);
    segs
}

fn unique_dest_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let stem = Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = Path::new(name).extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    for i in 1..u32::MAX {
        let p = dir.join(format!("{stem} ({i}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem} (x){ext}"))
}

fn is_inside_inbox(p: &Path, inbox: &Path) -> bool {
    let root = inbox.canonicalize().unwrap_or_else(|_| inbox.to_path_buf());
    let q = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    match q.strip_prefix(&root) {
        Ok(_) => true,
        Err(_) => false,
    }
}

fn peer_opts(cfg: &Config) -> PeerOpts {
    PeerOpts {
        node_id: cfg.node_id.clone(),
        name: cfg.name.clone(),
        token: cfg.token.clone(),
        heartbeat_ms: cfg.heartbeat_ms,
        heartbeat_timeout_ms: cfg.heartbeat_timeout_ms,
        caps: vec![CAP_FILE_CHUNK],
    }
}

impl Engine {
    pub fn new(
        cfg: Config,
        writer: Arc<dyn ClipboardWriter>,
        ifaces: IfaceProvider,
        rx: Receiver<EngineCmd>,
        tx: Sender<EngineEvent>,
    ) -> Self {
        let tx_cmd = rx_dummy();
        let history = crate::history::load(&cfg.state_dir);
        Engine {
            cfg,
            echo: EchoGuard::new(10000),
            writer,
            ifaces,
            rx,
            tx,
            tx_cmd,
            conn: None,
            conn_closed: false,
            batches: HashMap::new(),
            paused: false,
            stop: Arc::new(AtomicBool::new(false)),
            last_peer_label: String::new(),
            incoming: HashMap::new(),
            history,
        }
    }

    /// 壳层在 run 之前注入命令通道的发送端（供对端读线程桥接）
    pub fn set_cmd_sender(&mut self, tx: Sender<EngineCmd>) {
        self.tx_cmd = tx;
    }

    fn log(&self, msg: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Log(msg.into()));
    }
    fn warn(&self, msg: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Warn(msg.into()));
    }
    fn status(&self, s: Status) {
        let _ = self.tx.send(EngineEvent::Status(s));
    }
    fn notify(&self, title: &str, body: impl Into<String>) {
        let _ = self.tx.send(EngineEvent::Notify(title.into(), body.into()));
    }

    /// 响应式等待：在等待窗口内持续处理 channel 事件；返回 false 表示收到 Shutdown。
    /// wake 提前返回时返回 Some(())。
    fn pump(&mut self, wait: Duration) -> PumpResult {
        let deadline = Instant::now() + wait;
        loop {
            let remain = deadline.saturating_duration_since(Instant::now());
            if remain.is_zero() {
                return PumpResult::Timeout;
            }
            match self.rx.recv_timeout(remain.min(Duration::from_millis(200))) {
                Ok(cmd) => {
                    if !self.handle_cmd(cmd) {
                        return PumpResult::Shutdown;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.tick_batches();
                    if Instant::now() >= deadline {
                        return PumpResult::Timeout;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return PumpResult::Shutdown,
            }
        }
    }

    /// 引擎主入口：supervise（对应 Node 版 supervise/main）
    pub fn run(&mut self) {
        self.log("NetWorkClipboard 启动（原生版）");
        self.log(format!(
            "设备: {}   角色: {}   端口: {}   平台: {}",
            self.cfg.name,
            self.cfg.role,
            self.cfg.port,
            std::env::consts::OS
        ));
        if self.cfg.token.is_empty() {
            self.warn("未设置口令（--token）：链路仍加密，但任何知道端口的人都能连上");
        } else {
            self.log("链路加密: AES-256-GCM（scrypt 口令派生密钥）");
        }
        // 启动时打印扫描计划
        let plan = discovery::plan_scan(&ScanInput {
            target: &self.cfg.target,
            peer_hint: &self.cfg.peer_hint,
            ifaces: &self.cfg.ifaces,
            scan_cidr: &self.cfg.scan_cidr,
            iface_list: &(self.ifaces)(),
        });
        for line in discovery::format_scan_plan(&plan, self.cfg.port, &self.cfg.peer_hint, !self.cfg.scan_cidr.is_empty()) {
            self.log(line);
        }

        let can_listen = can_listen_default(&self.cfg);
        let mut mode = if self.cfg.role == "auto" {
            self.status(Status::Scanning);
            self.log(format!("角色抉择：先扫描内网（最多 {} 秒）…", self.cfg.startup_scan_ms / 1000));
            match self.scan_for_peer(self.cfg.startup_scan_ms) {
                Some(ip) => {
                    self.cfg.peer_hint = ip.to_string();
                    self.log(format!("发现对端 {ip}，本机作为客户端"));
                    "client".to_string()
                }
                None if can_listen => {
                    self.log("未发现对端，本机作为服务端监听（后台持续扫描）");
                    "server".to_string()
                }
                None => {
                    self.log("未发现对端；本平台默认不监听，本机作为客户端持续扫描");
                    "client".to_string()
                }
            }
        } else {
            self.cfg.role.clone()
        };

        let escalate = self.cfg.role != "client" && can_listen;
        loop {
            // Shutdown 是一次性命令（可能已被 session/scan_for_peer 消费），
            // 全局 stop 标志是各循环统一的退出依据（否则引擎线程残留、壳层跟着挂）
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            if mode == "server" {
                if !self.server_phase() {
                    return; // Shutdown
                }
                self.log("已退出监听，切换为客户端模式重新扫描");
                mode = "client".into();
                continue;
            }
            match self.client_loop(escalate) {
                ClientOutcome::Shutdown => return,
                ClientOutcome::Escalate => {
                    self.log("连续多轮未发现任何对端，回到服务端监听模式");
                    mode = "server".into();
                }
            }
        }
    }

    /// 扫描直到命中或超预算（对应 Node 版 scanForPeer）
    fn scan_for_peer(&mut self, budget_ms: u64) -> Option<Ipv4Addr> {
        let deadline = Instant::now() + Duration::from_millis(budget_ms.max(100));
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return None;
            }
            if let Some(ip) = self.find_peers() {
                return Some(ip);
            }
            if Instant::now() >= deadline {
                return None;
            }
            if matches!(self.pump(Duration::from_millis(300)), PumpResult::Shutdown) {
                return None;
            }
        }
    }

    fn find_peers(&mut self) -> Option<Ipv4Addr> {
        let plan = discovery::plan_scan(&ScanInput {
            target: &self.cfg.target,
            peer_hint: &self.cfg.peer_hint,
            ifaces: &self.cfg.ifaces,
            scan_cidr: &self.cfg.scan_cidr,
            iface_list: &(self.ifaces)(),
        });
        if plan.hosts.is_empty() {
            return None;
        }
        let hits = discovery::scan_hosts(&plan.hosts, self.cfg.port, self.cfg.scan_timeout_ms, self.cfg.scan_concurrency, &self.stop);
        hits.first().copied()
    }

    /// 客户端循环（对应 Node 版 clientLoop）
    fn client_loop(&mut self, escalate: bool) -> ClientOutcome {
        let mut bad_until: HashMap<Ipv4Addr, Instant> = HashMap::new();
        let mut misses: u32 = 0;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return ClientOutcome::Shutdown;
            }
            if matches!(self.pump(Duration::from_millis(200)), PumpResult::Shutdown) {
                return ClientOutcome::Shutdown;
            }
            if self.conn.is_some() {
                // 有连接：等待其结束（PeerClosed 事件会清掉 conn）
                if matches!(self.pump(Duration::from_millis(500)), PumpResult::Shutdown) {
                    return ClientOutcome::Shutdown;
                }
                continue;
            }
            let hits = self.find_peers();
            let now = Instant::now();
            let ip = hits.into_iter().find(|h| bad_until.get(h).map(|t| *t > now).unwrap_or(false) == false);
            let Some(ip) = ip else {
                misses += 1;
                if escalate && misses >= 5 {
                    return ClientOutcome::Escalate;
                }
                if matches!(self.pump(Duration::from_millis(self.cfg.scan_interval_ms)), PumpResult::Shutdown) {
                    return ClientOutcome::Shutdown;
                }
                continue;
            };
            misses = 0;
            self.status(Status::Connecting(ip.to_string()));
            self.log(format!("尝试连接 {ip}:{} …", self.cfg.port));
            match PeerConnection::connect(&ip.to_string(), self.cfg.port, &peer_opts(&self.cfg)) {
                Ok(acc) => {
                    self.cfg.peer_hint = ip.to_string();
                    save_peer_hint(&self.cfg, ip);
                    let peer = acc.peer;
                    self.log(format!("已连接 {ip}（{}）", peer.remote.name.as_str()));
                    peer.spawn_reader(
                        acc.reader,
                        acc.leftover,
                        Box::new(PeerBridge {
                            tx: self.tx_up(),
                            stop: Arc::clone(&self.stop),
                        }),
                    );
                    self.session(peer);
                    self.log("连接结束，重新扫描…");
                }
                Err(e) => {
                    if e.contains("认证失败") {
                        self.notify("连接失败", format!("对端 {ip} 认证失败：{e}"));
                    }
                    self.warn(format!("连接 {ip} 失败: {e}"));
                    bad_until.insert(ip, Instant::now() + Duration::from_secs(15));
                }
            }
            if matches!(self.pump(Duration::from_millis(800)), PumpResult::Shutdown) {
                return ClientOutcome::Shutdown;
            }
        }
    }

    fn tx_up(&self) -> Sender<EngineCmd> {
        self.tx_cmd.clone()
    }

    /// 服务端阶段（对应 Node 版 serverPhase）：监听、busy 拒绝、后台扫描降级。
    /// 返回 false = Shutdown（降级路径返回 true 并由调用方切换客户端模式）。
    fn server_phase(&mut self) -> bool {
        let listener = match TcpListener::bind(("0.0.0.0", self.cfg.port)) {
            Ok(l) => l,
            Err(e) => {
                let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
                    "（端口被占用，请换端口或关闭重复实例）"
                } else {
                    ""
                };
                self.warn(format!("监听 0.0.0.0:{} 失败: {e}{hint}", self.cfg.port));
                self.status(Status::Stopped);
                // 对应 Node：监听失败 → supervise 抛错退出；这里进入长等待以保持进程
                self.pump(Duration::from_secs(3600));
                return false;
            }
        };
        listener
            .set_nonblocking(true)
            .ok();
        self.log(format!("正在监听 0.0.0.0:{}（等待对端连接）", self.cfg.port));
        self.status(Status::Listening);

        let mut demoted = false;
        let mut demote_fail_until = Instant::now();
        let mut next_scan = Instant::now() + Duration::from_millis(self.cfg.scan_interval_ms);
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return false;
            }
            if matches!(self.pump(Duration::from_millis(80)), PumpResult::Shutdown) {
                return false;
            }
            // 接受新连接
            match listener.accept() {
                Ok((sock, addr)) => {
                    let peer_ip = addr.ip().to_string();
                    if self.conn.is_some() || demoted {
                        let mut sock = sock;
                        let _ = sock.write_all(busy_line().as_bytes());
                        let _ = sock.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                    sock.set_nonblocking(false).ok();
                    sock.set_nodelay(true).ok();
                    match PeerConnection::accept(sock, &peer_opts(&self.cfg)) {
                        Ok(acc) => {
                            let peer = acc.peer;
                            self.log(format!("已接受连接：{peer_ip}（{}）", peer.remote.name));
                            peer.spawn_reader(
                                acc.reader,
                                acc.leftover,
                                Box::new(PeerBridge {
                                    tx: self.tx_up(),
                                    stop: Arc::clone(&self.stop),
                                }),
                            );
                            self.session(peer);
                            if !demoted {
                                self.log(format!("继续监听 0.0.0.0:{} …", self.cfg.port));
                            }
                        }
                        Err(e) => {
                            if !e.contains("同一实例")
                                && !e.contains("版本不匹配")
                                && !e.contains("未收到握手请求")
                                && !e.contains("握手期间断开")
                                && !e.contains("认证失败")
                            {
                                self.warn(format!("来自 {peer_ip} 的连接握手失败: {e}"));
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
            // 后台扫描：发现别的监听端则降级为客户端连过去
            if !demoted && self.conn.is_none() && Instant::now() >= next_scan {
                next_scan = Instant::now() + Duration::from_millis(self.cfg.scan_interval_ms);
                if Instant::now() >= demote_fail_until {
                    if let Some(ip) = self.find_peers() {
                        self.log(format!("发现其他监听端 {ip}，降级为客户端连接"));
                        match PeerConnection::connect(&ip.to_string(), self.cfg.port, &peer_opts(&self.cfg)) {
                            Ok(acc) => {
                                demoted = true;
                                drop(listener);
                                self.cfg.peer_hint = ip.to_string();
                                save_peer_hint(&self.cfg, ip);
                                let peer = acc.peer;
                                peer.spawn_reader(
                                    acc.reader,
                                    acc.leftover,
                                    Box::new(PeerBridge {
                                        tx: self.tx_up(),
                                        stop: Arc::clone(&self.stop),
                                    }),
                                );
                                self.session(peer);
                                return true; // 降级会话结束 → supervise 切客户端
                            }
                            Err(_) => {
                                demote_fail_until = Instant::now() + Duration::from_secs(30);
                            }
                        }
                    }
                }
            }
        }
    }

    /// 有活跃连接期间的事件泵（对应 Node 版 session）
    fn session(&mut self, peer: Arc<PeerConnection>) {
        self.last_peer_label = format!("{} ({})", peer.remote.name, self.cfg.peer_hint);
        if !self.paused {
            self.status(Status::Connected(self.last_peer_label.clone()));
        }
        self.conn = Some(peer);
        self.conn_closed = false;
        while !self.conn_closed {
            if matches!(self.pump(Duration::from_millis(200)), PumpResult::Shutdown) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(p) = self.conn.take() {
                    p.close();
                }
                self.conn = None;
                return;
            }
        }
        self.conn = None;
        self.flush_all_batches();
    }

    fn handle_cmd(&mut self, cmd: EngineCmd) -> bool {
        match cmd {
            EngineCmd::Shutdown => {
                self.stop.store(true, Ordering::SeqCst);
                self.status(Status::Stopped);
                return false;
            }
            EngineCmd::Pause => {
                self.paused = true;
                self.status(Status::Paused);
            }
            EngineCmd::Resume => {
                self.paused = false;
                self.status(if self.conn.is_some() { Status::Connected(String::new()) } else { Status::Scanning });
            }
            EngineCmd::PasteText(text) => self.paste_text(text),
            EngineCmd::RemoveHistory(text) => {
                if crate::history::remove(&mut self.history, &text) {
                    crate::history::save(&self.cfg.state_dir, &self.history);
                }
            }
            EngineCmd::ClearHistory => {
                self.history.clear();
                crate::history::save(&self.cfg.state_dir, &self.history);
            }
            EngineCmd::Clip(ev) => {
                if !self.paused {
                    self.on_clip(ev);
                }
            }
            EngineCmd::PeerMsg(m) => self.on_peer_msg(m),
            EngineCmd::PeerBin(h, p) => self.on_peer_bin(h, p),
            EngineCmd::PeerClosed(reason) => {
                self.log(format!("连接已断开（{reason}）"));
                self.conn_closed = true;
            }
        }
        true
    }

    // ---- 本地剪贴板变化 → 上行（对应 onLocalChange/sendText/sendImage/sendFiles） ----

    fn on_clip(&mut self, ev: ClipEvent) {
        // 历史与连接无关：断线时的本机复制也留痕（远端写入已被平台层序列号/令牌吸收，
        // 到达这里的都是真实复制；回声命中不记录由 record_text 的去重置顶兜住）
        if let ClipEvent::Text(s) = &ev {
            self.record_text(s, false);
        }
        let Some(conn) = self.conn.clone() else { return };
        if conn.is_closed() {
            return;
        }
        match ev {
            ClipEvent::Text(s) => self.send_text(&conn, &s),
            ClipEvent::Image(png) => self.send_image(&conn, &png),
            ClipEvent::Files(mut paths) => {
                // 去重保序 + 排除收件箱内路径（收件箱只收不发）
                let mut seen = HashSet::new();
                paths.retain(|p| {
                    if !seen.insert(p.clone()) {
                        return false;
                    }
                    if is_inside_inbox(p, &self.cfg.inbox_dir) {
                        self.log(format!("跳过收件箱内的路径（收件箱只接收，不外发）: {}", p.display()));
                        return false;
                    }
                    true
                });
                if !paths.is_empty() {
                    self.send_files(&conn, &paths);
                }
            }
        }
    }

    fn send_text(&mut self, conn: &Arc<PeerConnection>, data: &str) {
        if data.len() as u64 > self.cfg.max_text_bytes {
            self.warn(format!("文本超过上限（{}），未同步", fmt_bytes(self.cfg.max_text_bytes)));
            return;
        }
        let h = hash_text(data);
        if self.echo.should_skip(&h) {
            return;
        }
        self.echo.remember(&h);
        if conn.send_msg(&protocol::MsgText { t: "text", data }) {
            self.log(format!("↑ 已发送文本（{} 字符）", data.chars().count()));
        }
    }

    fn send_image(&mut self, conn: &Arc<PeerConnection>, png: &[u8]) {
        if !is_png(png) {
            return;
        }
        if png.len() as u64 > self.cfg.max_image_bytes {
            self.warn(format!("图片超过上限（{}），未同步", fmt_bytes(self.cfg.max_image_bytes)));
            return;
        }
        let h = sha256_hex(png);
        if self.echo.should_skip(&h) {
            return;
        }
        self.echo.remember(&h);
        if conn.send_bin(
            &protocol::HdrImage { t: "image", mime: "image/png", size: png.len() },
            png,
        ) {
            self.log(format!("↑ 已发送图片（{}）", fmt_bytes(png.len() as u64)));
        }
    }

    // ---- 文件/文件夹发送（对应 buildManifest + sendFiles，流式化改造） ----

    fn send_files(&mut self, conn: &Arc<PeerConnection>, paths: &[PathBuf]) {
        let chunked = conn.remote.has_cap(CAP_FILE_CHUNK);
        let m = match self.build_manifest(paths) {
            Some(m) => m,
            None => return,
        };
        if m.notes.junk > 0 {
            self.log(format!("已跳过系统文件（.DS_Store / Thumbs.db / desktop.ini）：{} 个", m.notes.junk));
        }
        if m.notes.link > 0 {
            self.log(format!("已跳过符号链接：{} 个", m.notes.link));
        }
        if m.notes.other > 0 {
            self.log(format!("已跳过特殊文件：{} 个", m.notes.other));
        }
        for r in &m.notes.oversize {
            self.warn(format!("文件超过上限（{}），已跳过: {}", fmt_bytes(self.cfg.max_file_bytes), r));
        }
        if m.frames.is_empty() {
            self.warn("没有可发送的文件");
            return;
        }
        if m.files > self.cfg.max_transfer_files || m.bytes > self.cfg.max_transfer_bytes {
            self.warn(format!(
                "本次传输共 {} 个文件 / {}，超过上限（{} 个 / {}），已整体放弃",
                m.files,
                fmt_bytes(m.bytes),
                self.cfg.max_transfer_files,
                fmt_bytes(self.cfg.max_transfer_bytes)
            ));
            return;
        }

        let batch_id = protocol::new_node_id();
        let has_tree = m.frames.iter().any(|f| f.dir || f.rel.contains('/'));
        let mut dir_frames: Vec<ManifestEntry> = Vec::new();
        let mut file_frames: Vec<ManifestEntry> = Vec::new();
        for f in m.frames {
            if f.dir {
                dir_frames.push(f);
            } else {
                file_frames.push(f);
            }
        }
        let mut totals: HashMap<usize, u64> = HashMap::new();
        for f in &file_frames {
            *totals.entry(f.ri).or_insert(0) += 1;
        }

        let total = (file_frames.len() + dir_frames.len()) as u64;
        let mut stats: HashMap<usize, (String, u64, u64, bool)> = HashMap::new(); // ri -> (item, files, bytes, isDir)
        let mut deferred: Vec<(ManifestEntry, u64)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut echo_hits = 0u64;
        let mut sent_files = 0u64;
        let mut sent_dirs = 0u64;
        let mut frame_no = 0u64;

        // 复用缓冲（非分块路径）
        let mut buf: Vec<u8> = Vec::with_capacity(IO_BUF);

        for f in &file_frames {
            if conn.is_closed() {
                return;
            }
            frame_no += 1;
            let s = stat_of(&mut stats, f);
            let hash = match stream_hash(&f.abs) {
                Some(h) => h,
                None => {
                    self.warn(format!("读取失败 {}: {}", f.rel, "无法打开"));
                    continue;
                }
            };
            if !seen.contains(&hash) {
                if self.echo.should_skip(&hash) {
                    echo_hits += 1;
                    deferred.push((f.clone(), frame_no));
                    continue;
                }
                self.echo.remember(&hash);
                seen.insert(hash.clone());
            }
            if !self.send_one_file(conn, f, &hash, frame_no, total, batch_id.as_str(), chunked, &mut buf) {
                return;
            }
            let e = stats.get_mut(&f.ri).unwrap();
            e.1 += 1;
            e.2 += f.size;
            sent_files += 1;
            if !has_tree {
                self.log(format!("↑ 已发送文件 {}（{}，{}/{}）", f.rel, fmt_bytes(f.size), frame_no, total));
            }
        }

        // 补发：目录树里部分文件命中回声、同批又有新内容时必须补发，否则对端目录残缺
        if !deferred.is_empty() && sent_files > 0 {
            self.log(format!("↑ 本批次含新内容，补发 {} 个回声命中的文件以保证目录完整", deferred.len()));
            for (d, idx) in deferred {
                if conn.is_closed() {
                    return;
                }
                let Some(hash) = stream_hash(&d.abs) else { continue };
                self.echo.remember(&hash);
                if !self.send_one_file(conn, &d, &hash, idx, total, batch_id.as_str(), chunked, &mut buf) {
                    return;
                }
                let e = stats.get_mut(&d.ri).unwrap();
                e.1 += 1;
                e.2 += d.size;
                sent_files += 1;
                if !has_tree {
                    self.log(format!("↑ 已发送文件 {}（{}，{}/{}）", d.rel, fmt_bytes(d.size), idx, total));
                }
            }
        }

        // 目录帧：只在所属顶层项确有文件发出（或该项本无文件）时才发，防空目录骨架来回打转
        for f in &dir_frames {
            let e = stats.get(&f.ri);
            let has_files_for_ri = totals.get(&f.ri).copied().unwrap_or(0) > 0;
            let sent_for_ri = e.map(|x| x.1).unwrap_or(0);
            if has_files_for_ri && sent_for_ri == 0 {
                continue;
            }
            if conn.is_closed() {
                return;
            }
            frame_no += 1;
            if conn.send_bin(
                &protocol::HdrDir {
                    t: "dir",
                    rel: &f.rel,
                    index: frame_no,
                    count: total,
                    ri: f.ri,
                    batch_id: &batch_id,
                },
                b"",
            ) {
                sent_dirs += 1;
            }
        }

        // 批次结束前刷新回声窗口（长传输会让最早的哈希过期）
        for h in &seen {
            self.echo.remember(h);
        }

        if sent_files == 0 && sent_dirs == 0 {
            if echo_hits as usize == file_frames.len() {
                self.log("本批次的文件都在最近同步过（回声抑制），已跳过本次传输");
            } else {
                self.log("本次没有可发送的内容，已跳过");
            }
            return;
        }
        if has_tree {
            for (ri, (item, files, bytes, is_dir)) in &stats {
                if *files > 0 {
                    if *is_dir {
                        self.log(format!("↑ 已发送文件夹 {item}（{files} 个文件，共 {}）", fmt_bytes(*bytes)));
                    } else {
                        self.log(format!("↑ 已发送文件 {item}（{}）", fmt_bytes(*bytes)));
                    }
                } else if *is_dir && totals.get(ri).copied().unwrap_or(0) == 0 {
                    self.log(format!("↑ 已发送空文件夹 {item}"));
                }
            }
        }
        let _ = conn.send_bin(
            &protocol::HdrBatchEnd {
                t: "batchEnd",
                files: sent_files,
                dirs: sent_dirs,
                total,
                batch_id: &batch_id,
            },
            b"",
        );
    }

    /// 单文件发送：对端支持 fileChunk 时 256KB 流式分块；否则单帧（协议兼容 Node）
    fn send_one_file(
        &self,
        conn: &Arc<PeerConnection>,
        f: &ManifestEntry,
        hash: &str,
        index: u64,
        total: u64,
        batch_id: &str,
        chunked: bool,
        buf: &mut Vec<u8>,
    ) -> bool {
        if !chunked {
            // 兼容路径：整文件一帧（Node 协议），缓冲复用避免重复分配
            buf.clear();
            if std::fs::File::open(&f.abs).and_then(|mut fh| fh.read_to_end(buf)).is_err() {
                return true; // 读取失败按 Node 语义跳过（上层已计入）；这里保守返回 true 继续后续文件
            }
            return conn.send_bin(
                &protocol::HdrFile {
                    t: "file",
                    rel: &f.rel,
                    size: buf.len() as u64,
                    sha256: hash,
                    index,
                    count: total,
                    ri: f.ri,
                    batch_id,
                },
                buf,
            );
        }
        // 流式分块：读 256KB → 一帧；顺序读到不足一块即为最后一块
        let Ok(mut fh) = std::fs::File::open(&f.abs) else {
            return true;
        };
        let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
        let mut seq: u64 = 0;
        loop {
            chunk.clear();
            let mut take = CHUNK_SIZE;
            while take > 0 {
                let mut tmp = [0u8; 65536];
                let want = take.min(tmp.len());
                match fh.read(&mut tmp[..want]) {
                    Ok(0) => break,
                    Ok(n) => {
                        chunk.extend_from_slice(&tmp[..n]);
                        take -= n;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return true,
                }
            }
            let last = chunk.len() < CHUNK_SIZE;
            if !conn.send_bin(
                &protocol::HdrFileChunk {
                    t: "fileChunk",
                    rel: &f.rel,
                    size: f.size,
                    sha256: hash,
                    seq,
                    last: u8::from(last),
                    ri: f.ri,
                    batch_id,
                },
                &chunk,
            ) {
                return false;
            }
            seq += 1;
            if last {
                break;
            }
        }
        // 空文件：发一个 last=1 的空块
        if seq == 0 {
            return conn.send_bin(
                &protocol::HdrFileChunk {
                    t: "fileChunk",
                    rel: &f.rel,
                    size: f.size,
                    sha256: hash,
                    seq: 0,
                    last: 1,
                    ri: f.ri,
                    batch_id,
                },
                b"",
            );
        }
        true
    }

    fn build_manifest(&mut self, paths: &[PathBuf]) -> Option<Manifest> {
        let mut m = Manifest::default();
        fn walk(engine: &mut Engine, abs_dir: &Path, rel_dir: &str, ri: usize, m: &mut Manifest) -> bool {
            let Ok(entries) = std::fs::read_dir(abs_dir) else {
                engine.warn(format!("读取目录失败 {}: {}", abs_dir.display(), "无法读取"));
                return false;
            };
            let mut kept = 0u32;
            for ent in entries.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                let abs = ent.path();
                let rel = format!("{rel_dir}/{name}");
                let Ok(ft) = ent.file_type() else { continue };
                if ft.is_symlink() {
                    m.notes.link += 1;
                    continue;
                }
                if ft.is_dir() {
                    if walk(engine, &abs, &rel, ri, m) {
                        kept += 1;
                    }
                    continue;
                }
                if !ft.is_file() {
                    m.notes.other += 1;
                    continue;
                }
                if JUNK_FILES.iter().any(|j| j.eq_ignore_ascii_case(&name)) {
                    m.notes.junk += 1;
                    continue;
                }
                let Ok(st) = ent.metadata() else {
                    m.notes.other += 1;
                    continue;
                };
                if st.len() > engine.cfg.max_file_bytes {
                    m.notes.oversize.push(rel.clone());
                    continue;
                }
                m.frames.push(ManifestEntry {
                    item: rel_dir.split('/').next().unwrap_or("").to_string(),
                    ri,
                    abs,
                    rel,
                    size: st.len(),
                    dir: false,
                });
                m.files += 1;
                m.bytes += st.len();
                kept += 1;
            }
            if kept == 0 {
                m.frames.push(ManifestEntry {
                    item: rel_dir.split('/').next().unwrap_or("").to_string(),
                    ri,
                    abs: abs_dir.to_path_buf(),
                    rel: rel_dir.to_string(),
                    size: 0,
                    dir: true,
                });
            }
            true
        }

        for (ri, p) in paths.iter().enumerate() {
            let Ok(st) = std::fs::symlink_metadata(p) else {
                self.warn(format!("跳过（不存在）: {}", p.display()));
                continue;
            };
            if st.is_dir() {
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".into());
                walk(self, p, &name, ri, &mut m);
            } else if st.is_file() {
                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                if JUNK_FILES.iter().any(|j| j.eq_ignore_ascii_case(&name)) {
                    m.notes.junk += 1;
                    continue;
                }
                if st.len() > self.cfg.max_file_bytes {
                    m.notes.oversize.push(name);
                    continue;
                }
                m.frames.push(ManifestEntry {
                    item: name.clone(),
                    ri,
                    abs: p.clone(),
                    rel: name,
                    size: st.len(),
                    dir: false,
                });
                m.files += 1;
                m.bytes += st.len();
            } else {
                m.notes.other += 1;
            }
        }
        Some(m)
    }

    // ---- 接收（对应 wireConnection/handleBinary） ----

    fn on_peer_msg(&mut self, m: Value) {
        if m.get("t").and_then(Value::as_str) == Some("text") {
            let Some(data) = m.get("data").and_then(Value::as_str) else { return };
            self.echo.remember(&hash_text(data));
            self.record_text(data, true);
            if self.writer.set_text(data) {
                self.log(format!("↓ 已接收文本（{} 字符）", data.chars().count()));
            } else {
                self.warn("写入本地剪贴板失败");
            }
        }
    }

    /// 历史记录（受 config 开关/上限控制；重复内容置顶而非重复插入）
    fn record_text(&mut self, text: &str, peer: bool) {
        if !self.cfg.history_enabled {
            return;
        }
        if crate::history::push(&mut self.history, text, peer, self.cfg.history_limit as usize) {
            crate::history::save(&self.cfg.state_dir, &self.history);
        }
    }

    /// 从历史记录重新粘贴：写本地剪贴板 + 同步对端（等同一次接收动作，双向回声均已抑制）
    fn paste_text(&mut self, text: String) {
        self.echo.remember(&hash_text(&text));
        if let Some(conn) = self.conn.clone() {
            if !conn.is_closed() && conn.send_msg(&protocol::MsgText { t: "text", data: &text }) {
                self.log(format!("↑ 已从历史记录同步到对端（{} 字符）", text.chars().count()));
            }
        }
        if self.writer.set_text(&text) {
            self.record_text(&text, false); // 置顶
            self.log(format!("✓ 已从历史记录粘贴（{} 字符）", text.chars().count()));
        } else {
            self.warn("写入剪贴板失败");
        }
    }

    fn on_peer_bin(&mut self, header: Value, payload: Vec<u8>) {
        let t = header.get("t").and_then(Value::as_str).unwrap_or("");
        match t {
            "image" => {
                if payload.len() as u64 > self.cfg.max_image_bytes {
                    self.warn(format!("收到的图片超过上限（{}），已忽略", fmt_bytes(self.cfg.max_image_bytes)));
                    return;
                }
                if !is_png(&payload) {
                    self.warn("收到的图片不是 PNG，已忽略");
                    return;
                }
                self.echo.remember(&sha256_hex(&payload));
                if self.writer.set_image(&payload) {
                    self.log(format!("↓ 已接收图片（{}）", fmt_bytes(payload.len() as u64)));
                } else {
                    self.warn("写入本地剪贴板失败（图片）");
                }
            }
            "file" => self.recv_single_file(&header, &payload),
            "fileChunk" => self.recv_chunk(&header, &payload),
            "dir" => self.recv_dir(&header),
            "batchEnd" => {
                let id = header.get("batchId").and_then(Value::as_str).unwrap_or("b").to_string();
                if let Some(b) = self.batches.remove(&id) {
                    self.finalize_batch(b);
                }
            }
            _ => {}
        }
    }

    fn batch_of(&mut self, header: &Value) -> &mut Batch {
        let id = header.get("batchId").and_then(Value::as_str).unwrap_or("b").to_string();
        let count = header.get("count").and_then(Value::as_u64).filter(|c| *c > 0).unwrap_or(1);
        self.batches.entry(id.clone()).or_insert_with(|| Batch::new(count))
    }

    fn arm_idle(b: &mut Batch, idle_ms: u64) {
        b.touched = Instant::now();
        b.idle_at = Instant::now() + Duration::from_millis(idle_ms);
    }

    fn tick_batches(&mut self) {
        let idle_ms = self.cfg.batch_idle_ms;
        let now = Instant::now();
        let due: Vec<String> = self
            .batches
            .iter()
            .filter(|(_, b)| now >= b.idle_at)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            if let Some(mut b) = self.batches.remove(&k) {
                // 空闲兜底 flush；同一批次 id 的后续帧会继续并入同一根目录，不会分裂
                b.idle_at = Instant::now() + Duration::from_millis(idle_ms);
                let incomplete = b.count > 0 && b.got < b.count;
                self.flush_batch(&mut b, incomplete);
                b.touched = Instant::now();
                self.batches.insert(k, b);
            }
        }
        // 清理超时批次（5 分钟无活动或总量超 8）
        let now = Instant::now();
        self.batches.retain(|_, b| now.duration_since(b.touched) < Duration::from_secs(5 * 60));
        if self.batches.len() > 8 {
            let mut keys: Vec<String> = self.batches.keys().cloned().collect();
            keys.sort_by(|a, b| {
                self.batches.get(b).map(|x| x.touched).cmp(&self.batches.get(a).map(|x| x.touched))
            });
            while self.batches.len() > 8 {
                let k = keys.pop().unwrap();
                self.batches.remove(&k);
            }
        }
    }

    fn recv_single_file(&mut self, header: &Value, payload: &[u8]) {
        let rel = header
            .get("rel")
            .or_else(|| header.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let b_idle = self.cfg.batch_idle_ms;
        {
            let b = self.batch_of(header);
            Engine::arm_idle(b, b_idle);
            b.got += 1;
        }
        let segs = resolve_rel(&rel);
        if segs.is_empty() {
            self.warn(format!("收到的文件路径无效，已忽略: {rel}"));
            return;
        }
        if payload.len() as u64 > self.cfg.max_file_bytes {
            self.warn(format!("收到的文件超过上限（{}），已忽略: {}", fmt_bytes(self.cfg.max_file_bytes), rel));
            return;
        }
        let id = header.get("batchId").and_then(Value::as_str).unwrap_or("b").to_string();
        let h = sha256_hex(payload);
        self.echo.remember(&h);
        let over_limit = {
            let b = self.batches.get_mut(&id).unwrap();
            b.files += 1;
            b.bytes += payload.len() as u64;
            b.hashes.push(h.clone());
            if b.files > self.cfg.max_transfer_files || b.bytes > self.cfg.max_transfer_bytes {
                b.aborted = true;
                true
            } else {
                false
            }
        };
        if over_limit {
            self.warn(format!(
                "本批次接收量超过上限（{} 个文件 / {}），后续内容不再写入",
                self.cfg.max_transfer_files,
                fmt_bytes(self.cfg.max_transfer_bytes)
            ));
            return;
        }
        let dest = self.dest_for(&id, header, &segs, false);
        let Some(dest) = dest else {
            self.warn(format!("路径越界或过长，已忽略: {rel}"));
            return;
        };
        if write_file_atomically(&dest, payload).is_err() {
            self.warn(format!("写入接收文件失败 {}: {rel}", "IO 错误"));
            return;
        }
        let b = self.batches.get_mut(&id).unwrap();
        if segs.len() > 1 {
            if let Some(root) = b.roots.get_mut(&root_key_of(header, &segs)) {
                if root.paths.insert(dest.clone()) {
                    root.files += 1;
                    root.bytes += payload.len() as u64;
                }
            }
        } else {
            b.items.push(dest.clone());
            self.log(format!("↓ 已接收文件 {}（{}）", file_name_of(&dest), fmt_bytes(payload.len() as u64)));
        }
        let done = {
            let b = self.batches.get(&id).unwrap();
            b.got >= b.count
        };
        if done {
            if let Some(b) = self.batches.remove(&id) {
                self.finalize_batch(b);
            }
        }
    }

    fn recv_dir(&mut self, header: &Value) {
        let rel = header.get("rel").and_then(Value::as_str).unwrap_or("").to_string();
        let b_idle = self.cfg.batch_idle_ms;
        {
            let b = self.batch_of(header);
            Engine::arm_idle(b, b_idle);
            b.got += 1;
        }
        let segs = resolve_rel(&rel);
        if segs.is_empty() {
            self.warn(format!("收到的目录路径无效，已忽略: {rel}"));
            return;
        }
        let id = header.get("batchId").and_then(Value::as_str).unwrap_or("b").to_string();
        if self.batches.get(&id).map(|b| b.aborted).unwrap_or(false) {
            return;
        }
        let Some(dest) = self.dest_for(&id, header, &segs, true) else {
            self.warn(format!("路径越界或过长，已忽略: {rel}"));
            return;
        };
        if std::fs::create_dir_all(&dest).is_err() {
            self.warn(format!("创建接收目录失败 {}: {rel}", "IO 错误"));
        }
        let done = {
            let b = self.batches.get(&id).unwrap();
            b.got >= b.count
        };
        if done {
            if let Some(b) = self.batches.remove(&id) {
                self.finalize_batch(b);
            }
        }
    }

    /// 解析接收目标路径（ensureRoot + uniqueDestPath 语义；seg_only=true 表示顶层单项直接唯一化）
    fn dest_for(&mut self, batch_id: &str, header: &Value, segs: &[String], is_dir: bool) -> Option<PathBuf> {
        if segs.len() == 1 && !is_dir {
            std::fs::create_dir_all(&self.cfg.inbox_dir).ok()?;
            return Some(unique_dest_path(&self.cfg.inbox_dir, &segs[0]));
        }
        let key = root_key_of(header, segs);
        let name = segs[0].clone();
        let b = self.batches.get_mut(batch_id)?;
        if !b.roots.contains_key(&key) {
            std::fs::create_dir_all(&self.cfg.inbox_dir).ok()?;
            let dest = unique_dest_path(&self.cfg.inbox_dir, &name);
            b.roots.insert(
                key.clone(),
                BatchRoot {
                    dest: dest.clone(),
                    files: 0,
                    bytes: 0,
                    paths: HashSet::new(),
                    logged_files: 0,
                    logged_bytes: 0,
                },
            );
            b.items.push(dest);
        }
        let root = b.roots.get(&key)?;
        let dest = root.dest.join(segs[1..].iter().collect::<PathBuf>());
        if cfg!(windows) && dest.to_string_lossy().len() > 240 {
            return None;
        }
        // 目录已存在时（重名顶层项已在 dest_for 处理），内部子路径直接返回
        Some(if is_dir { dest } else { dest })
    }

    fn recv_chunk(&mut self, header: &Value, payload: &[u8]) {
        let rel = header.get("rel").and_then(Value::as_str).unwrap_or("").to_string();
        let batch_id = header.get("batchId").and_then(Value::as_str).unwrap_or("b").to_string();
        let ri = header.get("ri").and_then(Value::as_u64).unwrap_or(0) as usize;
        let seq = header.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let last = header.get("last").and_then(Value::as_u64).unwrap_or(0) == 1;
        let size = header.get("size").and_then(Value::as_u64).unwrap_or(0);
        let sha = header.get("sha256").and_then(Value::as_str).unwrap_or("").to_string();
        {
            let idle = self.cfg.batch_idle_ms;
            let b = self.batch_of(header);
            Engine::arm_idle(b, idle);
        }
        let segs = resolve_rel(&rel);
        if segs.is_empty() {
            self.warn(format!("收到的文件路径无效，已忽略: {rel}"));
            return;
        }
        if payload.len() as u64 > self.cfg.max_file_bytes {
            self.warn("收到的分块超过单文件上限，已忽略");
            return;
        }
        let key = (batch_id.clone(), rel.clone());
        if !self.incoming.contains_key(&key) {
            // 新文件：在 parts 目录建临时写入
            let parts_dir = self.cfg.state_dir.join("parts");
            let Some(tmp) = unique_path_in(&parts_dir, &format!("{}-part", sanitize_part(&rel))) else {
                return;
            };
            match std::fs::File::create(&tmp) {
                Ok(f) => {
                    self.incoming.insert(
                        key.clone(),
                        IncomingFile {
                            ri,
                            size,
                            sha256: sha.clone(),
                            written: 0,
                            hasher: Hasher::new(),
                            file: f,
                            tmp,
                        },
                    );
                }
                Err(_) => return,
            }
        }
        let Some(inc) = self.incoming.get_mut(&key) else { return };
        if inc.file.write_all(payload).is_err() {
            return;
        }
        inc.hasher.update(payload);
        inc.written += payload.len() as u64;
        if !last {
            return;
        }
        // 最后一块：校验哈希并落位
        let inc = self.incoming.remove(&key).unwrap();
        drop(inc.file);
        let got_hash = inc.hasher.finish_hex();
        if got_hash != inc.sha256 {
            let _ = std::fs::remove_file(&inc.tmp);
            self.warn(format!("分块文件哈希不匹配，已丢弃: {rel}"));
            return;
        }
        let h = got_hash;
        self.echo.remember(&h);
        let over_limit = {
            let Some(b) = self.batches.get_mut(&batch_id) else { return };
            b.files += 1;
            b.bytes += inc.written;
            b.hashes.push(h);
            if b.files > self.cfg.max_transfer_files || b.bytes > self.cfg.max_transfer_bytes {
                b.aborted = true;
                true
            } else {
                false
            }
        };
        if over_limit {
            let _ = std::fs::remove_file(&inc.tmp);
            self.warn("本批次接收量超过上限，后续内容不再写入");
            return;
        }
        let hdr_json = serde_json::json!({"ri": ri});
        let Some(dest) = self.dest_for(&batch_id, &hdr_json, &segs, false) else {
            let _ = std::fs::remove_file(&inc.tmp);
            self.warn(format!("路径越界或过长，已忽略: {rel}"));
            return;
        };
        if move_file(&inc.tmp, &dest).is_err() {
            let _ = std::fs::remove_file(&inc.tmp);
            self.warn(format!("落盘失败: {rel}"));
            return;
        }
        let b = self.batches.get_mut(&batch_id).unwrap();
        if segs.len() > 1 {
            if let Some(root) = b.roots.get_mut(&root_key_of(&hdr_json, &segs)) {
                if root.paths.insert(dest.clone()) {
                    root.files += 1;
                    root.bytes += inc.written;
                }
            }
        } else {
            b.items.push(dest.clone());
            self.log(format!("↓ 已接收文件 {}（{}）", file_name_of(&dest), fmt_bytes(inc.written)));
        }
    }

    fn flush_batch(&mut self, b: &mut Batch, incomplete: bool) {
        for root in b.roots.values() {
            if root.files == root.logged_files && root.bytes == root.logged_bytes {
                continue;
            }
            self.log(format!(
                "↓ 文件夹 {} 已就绪（{} 个文件，共 {}）",
                file_name_of(&root.dest),
                root.files,
                fmt_bytes(root.bytes)
            ));
        }
        for r in b.roots.values_mut() {
            r.logged_files = r.files;
            r.logged_bytes = r.bytes;
        }
        if b.files == 0 {
            if !b.empty_logged {
                b.empty_logged = true;
                let names: Vec<String> = b.items.iter().map(|p| file_name_of(p)).collect();
                self.log(format!(
                    "↓ 已创建空文件夹（{} 项）：{}（空目录不写入剪贴板）",
                    b.items.len(),
                    names.join("、")
                ));
            }
            return;
        }
        if b.aborted {
            let names: Vec<String> = b.items.iter().map(|p| file_name_of(p)).collect();
            self.warn(format!("本批次超过接收上限，未写入剪贴板（已落盘的文件保留）：{}", names.join("、")));
            return;
        }
        if b.items.is_empty() {
            self.warn("本批次没有成功落盘的文件，未写入剪贴板");
            return;
        }
        if incomplete && b.count > 0 {
            self.warn(format!("批次数据不完整（{}/{} 帧），先按已收到的内容处理", b.got, b.count));
        }
        for h in &b.hashes {
            self.echo.remember(h);
        }
        let items = b.items.clone();
        if self.writer.set_files(&items) {
            let names: Vec<String> = items.iter().map(|p| file_name_of(p)).collect();
            self.log(format!("↓ 已存入收件箱并设为剪贴板文件列表（共 {} 项）：{}", items.len(), names.join("、")));
            let brief: String = names.join("、").chars().take(100).collect();
            self.notify("收到文件", format!("共 {} 项已存入收件箱：{}", items.len(), brief));
        } else {
            let names: Vec<String> = items.iter().map(|p| file_name_of(p)).collect();
            self.warn(format!("已存入收件箱，但写入剪贴板文件列表失败：{}", names.join("、")));
        }
    }

    fn finalize_batch(&mut self, mut b: Batch) {
        let incomplete = b.count > 0 && b.got < b.count;
        b.idle_at = Instant::now() + Duration::from_secs(3600);
        self.flush_batch(&mut b, incomplete);
    }

    fn flush_all_batches(&mut self) {
        let batches = std::mem::take(&mut self.batches);
        for (_, mut b) in batches {
            self.finalize_batch(b);
        }
    }
}

#[derive(PartialEq)]
enum ClientOutcome {
    Shutdown,
    Escalate,
}

enum PumpResult {
    Timeout,
    Shutdown,
}

fn stat_of<'a>(stats: &'a mut HashMap<usize, (String, u64, u64, bool)>, f: &ManifestEntry) -> &'a mut (String, u64, u64, bool) {
    stats.entry(f.ri).or_insert_with(|| (f.item.clone(), 0, 0, f.dir || f.rel.contains('/')))
}

#[derive(Clone, Debug)]
struct ManifestEntry {
    item: String,
    ri: usize,
    abs: PathBuf,
    rel: String,
    size: u64,
    dir: bool,
}

#[derive(Default)]
struct Manifest {
    frames: Vec<ManifestEntry>,
    files: u64,
    bytes: u64,
    notes: ManifestNotes,
}

#[derive(Default)]
struct ManifestNotes {
    junk: u32,
    link: u32,
    other: u32,
    oversize: Vec<String>,
}

fn stream_hash(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Hasher::new();
    let mut buf = vec![0u8; IO_BUF];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(h.finish_hex())
}

fn write_file_atomically(dest: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, data)
}

fn move_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // 跨卷回退：复制后删除
    let data = std::fs::read(src)?;
    std::fs::write(dest, &data)?;
    std::fs::remove_file(src)
}

fn root_key_of(header: &Value, segs: &[String]) -> String {
    match header.get("ri").and_then(Value::as_u64) {
        Some(ri) => format!("i{ri}"),
        None => format!("s{}", segs.first().cloned().unwrap_or_default()),
    }
}

fn file_name_of(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn sanitize_part(rel: &str) -> String {
    rel.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .take(80)
        .collect()
}

fn unique_path_in(dir: &Path, prefix: &str) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let base = format!("{prefix}-{}", protocol::new_node_id());
    let p = dir.join(base);
    if !p.exists() {
        Some(p)
    } else {
        None
    }
}

fn busy_line() -> String {
    serde_json::json!({"t":"busy","magic":protocol::MAGIC}).to_string() + "\n"
}

pub fn load_peer_hint(cfg: &Config) -> String {
    let p = cfg.state_dir.join("state.json");
    let Ok(text) = std::fs::read_to_string(p) else { return String::new() };
    let v: Value = serde_json::from_str(&text).ok().unwrap_or(Value::Null);
    v.get("lastPeer").and_then(Value::as_str).unwrap_or("").to_string()
}

fn save_peer_hint(cfg: &Config, ip: Ipv4Addr) {
    std::fs::create_dir_all(&cfg.state_dir).ok();
    let p = cfg.state_dir.join("state.json");
    let body = serde_json::json!({"lastPeer": ip.to_string(), "updatedAt": chrono_less_now()});
    std::fs::write(p, body.to_string()).ok();
}

fn chrono_less_now() -> String {
    // ISO 时间戳仅用于 state.json 的可读性，这里用 unix 秒即可（避免引入 chrono）
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("normal.txt"), "normal.txt");
        assert_eq!(safe_name("trail... "), "trail");
        assert_eq!(safe_name(""), "file");
        if cfg!(windows) {
            assert_eq!(safe_name("CON"), "_CON");
            assert_eq!(safe_name("com1.txt"), "_com1.txt");
            assert_eq!(safe_name("com.txt"), "com.txt"); // com 后无数字不保留名
        }
        let long = "a".repeat(300);
        assert!(safe_name(&long).chars().count() <= 180);
    }

    #[test]
    fn resolve_rels() {
        assert_eq!(resolve_rel("a/b/c"), vec!["a", "b", "c"]);
        assert_eq!(resolve_rel("../x"), vec!["x"]);
        assert!(resolve_rel("/./").is_empty());
        let many: Vec<String> = (0..100).map(|i| format!("d{i}")).collect();
        assert_eq!(resolve_rel(&many.join("/")).len(), 64);
    }

    #[test]
    fn inside_inbox() {
        let inbox = Path::new("C:/Users/x/NetWorkClipboard/inbox");
        assert!(is_inside_inbox(Path::new("C:/Users/x/NetWorkClipboard/inbox/a.txt"), inbox));
        assert!(!is_inside_inbox(Path::new("C:/Users/x/NetWorkClipboard/out/a.txt"), inbox));
    }
}
