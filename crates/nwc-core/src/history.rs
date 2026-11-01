//! 剪贴板文字历史：环形列表 + JSONL 持久化（~/.networkclipboard/history.json）。
//! 只记录文字（图片/文件不做历史，内存考量）；条目去重置顶；上限与开关见 config。

use std::path::Path;

use serde::{Deserialize, Serialize};

/// 单条记录的体积上限：超出则不记录（128KB × 20 条 = 最坏 2.5MB，典型场景 KB 级）
pub const MAX_ENTRY_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub text: String,
    /// 记录时刻（unix 毫秒）
    pub at: u64,
    /// true = 来自对端接收；false = 本机复制
    pub peer: bool,
}

fn history_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("history.json")
}

pub fn load(state_dir: &Path) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(history_path(state_dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(state_dir: &Path, entries: &[HistoryEntry]) {
    let p = history_path(state_dir);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(json) = serde_json::to_string(entries) {
        let _ = std::fs::write(p, json);
    }
}

/// 去重置顶插入（相同内容移到最前并刷新时间），返回是否插入/移动
pub fn push(entries: &mut Vec<HistoryEntry>, text: &str, peer: bool, limit: usize) -> bool {
    if text.is_empty() || text.len() > MAX_ENTRY_BYTES {
        return false;
    }
    let at = now_ms();
    if let Some(pos) = entries.iter().position(|e| e.text == text) {
        let mut e = entries.remove(pos);
        e.at = at;
        e.peer = peer;
        entries.insert(0, e);
        return true;
    }
    entries.insert(
        0,
        HistoryEntry {
            text: text.to_string(),
            at,
            peer,
        },
    );
    if entries.len() > limit {
        entries.truncate(limit);
    }
    true
}

/// 删除第一个内容匹配的条目（列表内文本已去重，文本即唯一键）
pub fn remove(entries: &mut Vec<HistoryEntry>, text: &str) -> bool {
    if let Some(pos) = entries.iter().position(|e| e.text == text) {
        entries.remove(pos);
        true
    } else {
        false
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
