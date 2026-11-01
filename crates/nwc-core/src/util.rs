//! 杂项工具（对应 Node 版 src/util.js 的可移植部分）。

use sha2::{Digest, Sha256};

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finalize())
}

/// 流式 SHA256（大文件用）
pub struct Hasher(Sha256);

impl Hasher {
    pub fn new() -> Self {
        Hasher(Sha256::new())
    }
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
    pub fn finish_hex(self) -> String {
        hex(&self.0.finalize())
    }
}

/// 文本哈希带命名空间前缀，防跨类型碰撞（对应 Node 版 hashText）
pub fn hash_text(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"text:");
    h.update(s.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const TAB: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(TAB[(b >> 4) as usize] as char);
        s.push(TAB[(b & 15) as usize] as char);
    }
    s
}

pub fn is_png(buf: &[u8]) -> bool {
    buf.len() > 8 && buf[0] == 0x89 && buf[1] == 0x50 && buf[2] == 0x4e && buf[3] == 0x47
}

pub fn fmt_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    }
}
