//! 剪贴板历史：环形列表 + JSON 持久化（~/.networkclipboard/history.json）。
//! 三种条目：文本（正文即摘要）、图片（PNG 落盘 state/history/，条目存路径+尺寸摘要）、
//! 文件（条目存路径+大小摘要）。条目去重置顶（键：data 优先，无 data 用 text）；
//! 上限与开关见 config。旧版 history.json 无 kind 字段 → 视为文本。

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::util::{fmt_bytes, is_png, sha256_hex};

/// 文本条目的体积上限：超出则不记录（128KB × 20 条 = 最坏 2.5MB，典型场景 KB 级）
pub const MAX_ENTRY_BYTES: usize = 128 * 1024;
/// 图片进历史的体积上限（历史 PNG 落盘会占磁盘，超限只留在剪贴板不进历史）
pub const MAX_IMAGE_ENTRY_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    Text,
    Image,
    File,
}

impl EntryKind {
    pub fn is_text(v: &EntryKind) -> bool {
        matches!(v, EntryKind::Text)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub text: String,
    /// 记录时刻（unix 毫秒）
    pub at: u64,
    /// true = 来自对端接收；false = 本机复制
    pub peer: bool,
    /// 条目类型（旧文件缺省 = 文本）
    #[serde(default, skip_serializing_if = "EntryKind::is_text")]
    pub kind: EntryKind,
    /// image: PNG 落盘路径；file: 文件路径；text: 无
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

impl HistoryEntry {
    /// 去重/删除用的唯一键：有 data 用 data（路径），否则用文本
    pub fn key(&self) -> &str {
        self.data.as_deref().unwrap_or(&self.text)
    }
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

/// 去重置顶插入（键相同移到最前并整体替换，刷新时间），返回是否插入/移动
pub fn push_entry(entries: &mut Vec<HistoryEntry>, entry: HistoryEntry, limit: usize) -> bool {
    if entry.text.is_empty() {
        return false;
    }
    // 只有纯文本条目受 MAX_ENTRY_BYTES 约束（图片/文件的 text 只是摘要）
    if entry.kind == EntryKind::Text && entry.text.len() > MAX_ENTRY_BYTES {
        return false;
    }
    if limit == 0 {
        return false;
    }
    let key = entry.key().to_string();
    if let Some(pos) = entries.iter().position(|e| e.key() == key) {
        entries.remove(pos);
    }
    entries.insert(0, entry);
    if entries.len() > limit {
        entries.truncate(limit);
    }
    true
}

/// 文本条目便捷入口（保持旧签名语义）
pub fn push(entries: &mut Vec<HistoryEntry>, text: &str, peer: bool, limit: usize) -> bool {
    push_entry(
        entries,
        HistoryEntry {
            text: text.to_string(),
            at: now_ms(),
            peer,
            kind: EntryKind::Text,
            data: None,
        },
        limit,
    )
}

/// 删除第一个键匹配的条目（列表内已去重，键即唯一标识）
pub fn remove(entries: &mut Vec<HistoryEntry>, key: &str) -> bool {
    if let Some(pos) = entries.iter().position(|e| e.key() == key) {
        entries.remove(pos);
        true
    } else {
        false
    }
}

/// PNG 头解析宽高（签名 8B + IHDR 长度/类型 8B + 宽高各 4B 大端）
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if png.len() < 24 || png[..8] != SIG {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// 图片条目的显示摘要："图片 1920×1080 · 45.2 KB"
pub fn image_summary(png: &[u8]) -> String {
    let size = fmt_bytes(png.len() as u64);
    match png_dimensions(png) {
        Some((w, h)) => format!("图片 {w}×{h} · {size}"),
        None => format!("图片 · {size}"),
    }
}

/// 图片是否允许进历史（真 PNG + 体积上限内）
pub fn image_entry_allowed(png: &[u8]) -> bool {
    is_png(png) && png.len() <= MAX_IMAGE_ENTRY_BYTES
}

/// PNG 落盘到历史目录：{sha256 前 16 hex}.png，同内容天然去重；返回落盘路径字符串
pub fn store_image(dir: &Path, png: &[u8]) -> Option<String> {
    let p = dir.join(format!("{}.png", &sha256_hex(png)[..16]));
    if !p.exists() {
        std::fs::create_dir_all(dir).ok()?;
        std::fs::write(&p, png).ok()?;
    }
    Some(p.to_string_lossy().into_owned())
}

/// 清理历史目录中不再被任何条目引用的 PNG（条目被删除/截断后的孤儿文件）
pub fn prune_history_files(dir: &Path, entries: &[HistoryEntry]) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let referenced: HashSet<String> = entries.iter().filter_map(|e| e.data.clone()).collect();
    let mut n = 0;
    for ent in rd.flatten() {
        let p = ent.path();
        if p.extension().map(|e| e == "png").unwrap_or(false) {
            let s = p.to_string_lossy().into_owned();
            if !referenced.contains(&s) {
                if std::fs::remove_file(&p).is_ok() {
                    n += 1;
                }
            }
        }
    }
    n
}

/// 图片入库：落盘 + 摘要条目 + 孤儿清理；返回是否产生变更（引擎据此 save）
pub fn push_image(entries: &mut Vec<HistoryEntry>, dir: &Path, png: &[u8], peer: bool, limit: usize) -> bool {
    if !image_entry_allowed(png) {
        return false;
    }
    let Some(data) = store_image(dir, png) else {
        return false;
    };
    let changed = push_entry(
        entries,
        HistoryEntry {
            text: image_summary(png),
            at: now_ms(),
            peer,
            kind: EntryKind::Image,
            data: Some(data),
        },
        limit,
    );
    if changed {
        prune_history_files(dir, entries);
    }
    changed
}

/// 文件/文件夹入库：路径 + 大小摘要；返回是否产生变更
pub fn push_file(entries: &mut Vec<HistoryEntry>, path: &Path, peer: bool, limit: usize) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else {
        return false;
    };
    let size = fmt_bytes(md.len());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let label = if md.is_dir() { "文件夹" } else { "文件" };
    push_entry(
        entries,
        HistoryEntry {
            text: format!("{label} {name} · {size}"),
            at: now_ms(),
            peer,
            kind: EntryKind::File,
            data: Some(path.to_string_lossy().into_owned()),
        },
        limit,
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_entry(text: &str, peer: bool) -> HistoryEntry {
        HistoryEntry {
            text: text.to_string(),
            at: 1,
            peer,
            kind: EntryKind::Text,
            data: None,
        }
    }

    /// 旧版 history.json（无 kind 字段）加载后应视为文本
    #[test]
    fn legacy_json_without_kind_loads_as_text() {
        let legacy = r#"[{"text":"hello","at":123,"peer":false}]"#;
        let entries: Vec<HistoryEntry> = serde_json::from_str(legacy).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, EntryKind::Text);
        assert_eq!(entries[0].data, None);
        // 新序列化对纯文本条目不写 kind/data，保持旧格式形态（可被旧版读回）
        let out = serde_json::to_string(&entries).unwrap();
        assert!(!out.contains("kind"), "纯文本条目不应序列化 kind 字段: {out}");
        let round: Vec<HistoryEntry> = serde_json::from_str(&out).unwrap();
        assert_eq!(round[0].text, "hello");
    }

    /// 去重键：文本用 text；图片/文件用 data 路径——同摘要不同路径不互吞
    #[test]
    fn push_entry_dedupes_by_key() {
        let mut v: Vec<HistoryEntry> = Vec::new();
        assert!(push(&mut v, "a", false, 10));
        assert!(push(&mut v, "b", false, 10));
        // 重复文本：置顶而非新增
        assert!(push(&mut v, "a", true, 10));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].text, "a");
        assert!(v[0].peer, "重复插入应刷新 peer 标记");

        // 两张不同图片，摘要相同（同尺寸同大小）但 data 不同 → 两条共存
        let summary = "图片 10×10 · 1 KB";
        let e1 = HistoryEntry {
            text: summary.into(),
            at: 2,
            peer: false,
            kind: EntryKind::Image,
            data: Some("/h/aaaa.png".into()),
        };
        let e2 = HistoryEntry {
            text: summary.into(),
            at: 3,
            peer: true,
            kind: EntryKind::Image,
            data: Some("/h/bbbb.png".into()),
        };
        assert!(push_entry(&mut v, e1, 10));
        assert!(push_entry(&mut v, e2, 10));
        assert_eq!(v.len(), 4, "同摘要不同路径的图片不应互吞");
        // 同路径图片再来：按键置顶，不新增
        let e1_again = HistoryEntry {
            text: summary.into(),
            at: 4,
            peer: false,
            kind: EntryKind::Image,
            data: Some("/h/aaaa.png".into()),
        };
        assert!(push_entry(&mut v, e1_again, 10));
        assert_eq!(v.len(), 4);
        assert_eq!(v[0].data.as_deref(), Some("/h/aaaa.png"));
    }

    /// 上限截断与超限拒收
    #[test]
    fn push_entry_limit_and_oversize() {
        let mut v: Vec<HistoryEntry> = Vec::new();
        for i in 0..5 {
            push(&mut v, &format!("t{i}"), false, 3);
        }
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].text, "t4", "最新在前");

        let big = "x".repeat(MAX_ENTRY_BYTES + 1);
        assert!(!push(&mut v, &big, false, 3), "超限文本应拒收");

        // limit=0 关闭
        assert!(!push(&mut v, "y", false, 0));

        // 图片条目的 text 是短摘要，不受文本 128KB 限制
        let img = HistoryEntry {
            text: "图片 · 4.9 MB".into(),
            at: 1,
            peer: false,
            kind: EntryKind::Image,
            data: Some("/h/big.png".into()),
        };
        assert!(push_entry(&mut v, img, 3));
    }

    /// remove 按键匹配：图片条目用路径删，文本条目用文本删
    #[test]
    fn remove_matches_by_key() {
        let mut v: Vec<HistoryEntry> = Vec::new();
        push(&mut v, "hello", false, 10);
        push_entry(
            &mut v,
            HistoryEntry {
                text: "图片 1×1 · 1 KB".into(),
                at: 2,
                peer: false,
                kind: EntryKind::Image,
                data: Some("/h/k.png".into()),
            },
            10,
        );
        assert_eq!(v.len(), 2);
        // 用路径删图片
        assert!(remove(&mut v, "/h/k.png"));
        assert_eq!(v.len(), 1);
        // 用文本删文本
        assert!(remove(&mut v, "hello"));
        assert!(v.is_empty());
        // 同名摘要但无 data 的文本条目：文本仍可删
        push(&mut v, "图片 1×1 · 1 KB", false, 10);
        assert!(remove(&mut v, "图片 1×1 · 1 KB"));
    }

    /// PNG 头解析：合法 1×1 PNG / 截断 / 非 PNG
    #[test]
    fn png_dimensions_parses_header() {
        // 1×1 红色像素 PNG（完整合法文件）
        let png: Vec<u8> = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
        .to_vec();
        assert_eq!(png_dimensions(&png), Some((1, 1)));
        assert_eq!(image_summary(&png), format!("图片 1×1 · {}", fmt_bytes(png.len() as u64)));
        assert!(image_entry_allowed(&png));

        assert_eq!(png_dimensions(&png[..20]), None, "截断应返回 None");
        assert_eq!(png_dimensions(b"not a png at all......"), None, "非 PNG 应返回 None");
        assert!(!image_entry_allowed(b"not a png"), "非 PNG 不进历史");
        assert!(!image_entry_allowed(&vec![0u8; MAX_IMAGE_ENTRY_BYTES + 1]), "超限不进历史");
    }

    /// store_image：内容寻址落盘，同内容幂等；prune 清孤儿
    #[test]
    fn store_and_prune_history_files() {
        let dir = std::env::temp_dir().join(format!("nwc-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let png = vec![1, 2, 3, 4];
        let p1 = store_image(&dir, &png).unwrap();
        assert!(Path::new(&p1).exists());
        // 同内容再存：同路径（内容寻址幂等）
        let p2 = store_image(&dir, &png).unwrap();
        assert_eq!(p1, p2);

        let entries = vec![HistoryEntry {
            text: "图片 · 4 B".into(),
            at: 1,
            peer: false,
            kind: EntryKind::Image,
            data: Some(p1.clone()),
        }];
        // 引用中的文件不删
        assert_eq!(prune_history_files(&dir, &entries), 0);
        // 存第二张但不在引用列表 → 是孤儿，被清
        let orphan = store_image(&dir, &[9, 9, 9]).unwrap();
        assert_ne!(orphan, p1);
        assert_eq!(prune_history_files(&dir, &entries), 1);
        assert!(!Path::new(&orphan).exists());
        assert!(Path::new(&p1).exists());
        // 全部条目删除后 → 引用文件也成孤儿
        assert_eq!(prune_history_files(&dir, &[]), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// push_image：合法 PNG 入库（条目+落盘+摘要），非法/超限拒收；push_file：文件/文件夹摘要
    #[test]
    fn push_image_and_file_entries() {
        let mut v: Vec<HistoryEntry> = Vec::new();
        let dir = std::env::temp_dir().join(format!("nwc-hist2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let png: Vec<u8> = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
        .to_vec();
        assert!(push_image(&mut v, &dir, &png, true, 10));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, EntryKind::Image);
        assert!(v[0].peer, "peer 标记透传");
        assert!(v[0].text.contains("1×1"), "摘要应含尺寸: {}", v[0].text);
        assert!(Path::new(v[0].data.as_deref().unwrap()).exists(), "PNG 应已落盘");
        // 非 PNG 拒收
        assert!(!push_image(&mut v, &dir, b"junk", false, 10));
        assert_eq!(v.len(), 1);

        // 文件条目
        let f = dir.join("样本.txt");
        std::fs::write(&f, b"0123456789").unwrap();
        assert!(push_file(&mut v, &f, false, 10));
        let last = &v[0];
        assert_eq!(last.kind, EntryKind::File);
        assert!(last.text.starts_with("文件 样本.txt · "), "摘要: {}", last.text);
        assert_eq!(last.data.as_deref(), Some(f.to_string_lossy().as_ref()));
        // 文件夹条目
        let d = dir.join("子目录");
        std::fs::create_dir_all(&d).unwrap();
        assert!(push_file(&mut v, &d, false, 10));
        assert!(v[0].text.starts_with("文件夹 子目录"), "摘要: {}", v[0].text);
        // 不存在的路径拒收
        assert!(!push_file(&mut v, &dir.join("不存在"), false, 10));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
