//! 帧编解码与握手消息（对应 Node 版 src/protocol.js）。

use serde::Serialize;
use serde_json::Value;

pub const MAGIC: &str = "NWC1";
pub const VERSION: u32 = 1;
pub const KIND_JSON: u8 = 1;
pub const KIND_BIN: u8 = 2;
/// 单帧密文长度上限（对应 Node 版 MAX_FRAME）
pub const MAX_FRAME: usize = 128 * 1024 * 1024;

/// 8 随机字节的 hex（16 字符）。Node 版同时用它当节点 ID 和文件批次 batchId。
pub fn new_node_id() -> String {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).expect("getrandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 设备身份：首次生成后持久化于 state_dir/device_id，重启不变。
/// （对端列表/配对依赖稳定 ID；同机双实例同 ID，「检测到同一实例」判断照常成立；
///   换机器/重装 = 新设备，语义正确。随机 ID 无隐私泄漏——握手 hello 是明文的，
///   不能用 MAC/主机名这类可追踪值）
pub fn load_or_create_device_id(state_dir: &std::path::Path) -> String {
    let p = state_dir.join("device_id");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let t = s.trim().to_string();
        if t.len() == 16 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return t;
        }
    }
    let id = new_node_id();
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(&p, &id);
    id
}

/// 对端能力（hello/hello_ack 里可选的 caps 数组；Node 版不发，视为无任何扩展）。
/// Rust↔Rust 协商出 fileChunk 后大文件分块流式传输，避免整帧 100MB 进内存。
pub const CAP_FILE_CHUNK: &str = "fileChunk";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HelloMsg<'a> {
    t: &'a str,
    magic: &'a str,
    ver: u32,
    node_id: &'a str,
    name: &'a str,
    salt: &'a str,
    /// 能力扩展。Node 版不带该字段也忽略未知字段；为空时不序列化，保持字节级一致。
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    caps: &'a [&'a str],
}

pub fn make_hello(node_id: &str, name: &str, salt_b64: &str, caps: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&HelloMsg {
        t: "hello",
        magic: MAGIC,
        ver: VERSION,
        node_id,
        name,
        salt: salt_b64,
        caps,
    })
    .expect("serialize hello")
}

pub fn make_hello_ack(node_id: &str, name: &str, salt_b64: &str, caps: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&HelloMsg {
        t: "hello_ack",
        magic: MAGIC,
        ver: VERSION,
        node_id,
        name,
        salt: salt_b64,
        caps,
    })
    .expect("serialize hello_ack")
}

/// 对应 Node 版 isHelloMsg：t/magic/ver 严格匹配，nodeId 与 salt 必须是 string（name 不校验）。
pub fn is_hello_msg(m: &Value, ty: &str) -> bool {
    m.get("t").and_then(Value::as_str) == Some(ty)
        && m.get("magic").and_then(Value::as_str) == Some(MAGIC)
        && m.get("ver").and_then(Value::as_i64) == Some(VERSION as i64)
        && m.get("nodeId").map(Value::is_string) == Some(true)
        && m.get("salt").map(Value::is_string) == Some(true)
}

/// kind=1 JSON 帧：`[1][JSON 字节]`
pub fn json_msg_bytes(obj: &impl Serialize) -> Vec<u8> {
    let payload = serde_json::to_vec(obj).expect("serialize json msg");
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(KIND_JSON);
    out.extend_from_slice(&payload);
    out
}

/// kind=2 二进制帧：`[2][header JSON][0x0A][payload]`
/// 注意 header 序列化不得产生字面换行（紧凑序列化天然满足）。
pub fn bin_msg_bytes(header: &impl Serialize, payload: &[u8]) -> Vec<u8> {
    let h = serde_json::to_vec(header).expect("serialize bin header");
    let mut out = Vec::with_capacity(h.len() + payload.len() + 2);
    out.push(KIND_BIN);
    out.extend_from_slice(&h);
    out.push(0x0A);
    out.extend_from_slice(payload);
    out
}

/// 解析二进制帧 body：以首个 0x0A 分界（对应 Node 版 parseBin）。
pub fn parse_bin(body: &[u8]) -> Option<(Value, &[u8])> {
    let i = body.iter().position(|&b| b == 0x0A)?;
    let header: Value = serde_json::from_slice(&body[..i]).ok()?;
    if header.get("t").and_then(Value::as_str).is_none() {
        return None;
    }
    Some((header, &body[i + 1..]))
}

// ---- 应用层消息（字段名必须与 Node 版 src/index.js 完全一致） ----

#[derive(Serialize)]
pub struct MsgText<'a> {
    pub t: &'a str,
    pub data: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrImage {
    pub t: &'static str,
    pub mime: &'static str,
    pub size: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrFile<'a> {
    pub t: &'static str,
    pub rel: &'a str,
    pub size: u64,
    pub sha256: &'a str,
    pub index: u64,
    pub count: u64,
    pub ri: usize,
    pub batch_id: &'a str,
}

/// Rust↔Rust 专用：文件分块传输（对端声明了 CAP_FILE_CHUNK 才可用）。
/// seq 从 0 开始；last=1 表示这是最后一块。size/sha256 是整个文件的。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrFileChunk<'a> {
    pub t: &'static str,
    pub rel: &'a str,
    pub size: u64,
    pub sha256: &'a str,
    pub seq: u64,
    pub last: u8,
    pub ri: usize,
    pub batch_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrDir<'a> {
    pub t: &'static str,
    pub rel: &'a str,
    pub index: u64,
    pub count: u64,
    pub ri: usize,
    pub batch_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HdrBatchEnd<'a> {
    pub t: &'static str,
    pub files: u64,
    pub dirs: u64,
    pub total: u64,
    pub batch_id: &'a str,
}

/// 只有 t 字段的简单消息（ping/pong/auth/auth_ok）。序列化为 {"t": ...}。
pub struct MsgT(pub &'static str);

impl Serialize for MsgT {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("t", self.0)?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_fields_match_node() {
        // Node: { t:'hello', magic:'NWC1', ver:1, nodeId, name, salt }
        let b = make_hello("0123456789abcdef", "PC", "AAAAAAAAAAAAAAAAAAAAAA==", &[]);
        let s = String::from_utf8(b).unwrap();
        assert_eq!(
            s,
            r#"{"t":"hello","magic":"NWC1","ver":1,"nodeId":"0123456789abcdef","name":"PC","salt":"AAAAAAAAAAAAAAAAAAAAAA=="}"#
        );
        // 带 caps 时字段附加在末尾（Node 端会忽略未知字段）
        let b2 = make_hello("id", "PC", "QQ==", &[CAP_FILE_CHUNK]);
        let s2 = String::from_utf8(b2).unwrap();
        assert!(s2.ends_with(&format!(r#","caps":["{CAP_FILE_CHUNK}"]}}"#)));
    }

    #[test]
    fn bin_msg_splits_at_first_newline() {
        let h = HdrImage { t: "image", mime: "image/png", size: 3 };
        let b = bin_msg_bytes(&h, b"abc");
        assert_eq!(b[0], KIND_BIN);
        let (v, payload) = parse_bin(&b[1..]).unwrap();
        assert_eq!(v["t"], "image");
        assert_eq!(payload, b"abc");
    }

    #[test]
    fn json_msg_layout() {
        let b = json_msg_bytes(&MsgT("ping"));
        assert_eq!(b, b"\x01{\"t\":\"ping\"}");
    }
}
