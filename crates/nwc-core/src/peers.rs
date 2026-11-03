//! 已知对端登记与配对偏好（peers.json / state.json）。
//!
//! - 登记：每次成功握手 upsert {node_id, name, last_ip, last_seen}，供菜单展示与配对选择
//! - 配对偏好：preferredPeer（node_id）+ peerStrict 存 state.json；
//!   strict=false 时偏好仅影响连接顺序（优先试它的 IP）；
//!   strict=true 时握手校验 node_id，非配对设备直接拒绝（剪贴板只流向指定设备）

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 已知对端上限（超过淘汰最旧）
pub const MAX_PEERS: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KnownPeer {
    pub node_id: String,
    pub name: String,
    pub last_ip: String,
    /// 最后握手成功的 unix 秒
    pub last_seen: u64,
}

fn peers_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("peers.json")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load(state_dir: &Path) -> Vec<KnownPeer> {
    let Ok(text) = std::fs::read_to_string(peers_path(state_dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// 握手成功时登记/置顶；返回 upsert 后的条目
pub fn upsert(state_dir: &Path, node_id: &str, name: &str, ip: &str) {
    let mut list = load(state_dir);
    if let Some(pos) = list.iter().position(|p| p.node_id == node_id) {
        let mut e = list.remove(pos);
        e.name = name.to_string();
        e.last_ip = ip.to_string();
        e.last_seen = now_secs();
        list.insert(0, e);
    } else {
        list.insert(
            0,
            KnownPeer {
                node_id: node_id.to_string(),
                name: name.to_string(),
                last_ip: ip.to_string(),
                last_seen: now_secs(),
            },
        );
    }
    list.truncate(MAX_PEERS);
    let _ = std::fs::create_dir_all(state_dir);
    if let Ok(json) = serde_json::to_string(&list) {
        let _ = std::fs::write(peers_path(state_dir), json);
    }
}

/// 读取配对偏好 (preferred node_id, strict)。state.json 不存在或字段缺失 → 空偏好
pub fn load_pref(state_dir: &Path) -> (String, bool) {
    let Ok(text) = std::fs::read_to_string(state_dir.join("state.json")) else {
        return (String::new(), false);
    };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (
        v.get("preferredPeer").and_then(Value::as_str).unwrap_or("").to_string(),
        v.get("peerStrict").and_then(Value::as_bool).unwrap_or(false),
    )
}

/// 写入配对偏好（保留 state.json 其它字段，如 lastPeer）
pub fn save_pref(state_dir: &Path, preferred: &str, strict: bool) {
    let _ = std::fs::create_dir_all(state_dir);
    let p = state_dir.join("state.json");
    let mut v: Value = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        if preferred.is_empty() {
            obj.remove("preferredPeer");
        } else {
            obj.insert("preferredPeer".into(), Value::String(preferred.to_string()));
        }
        obj.insert("peerStrict".into(), Value::Bool(strict));
    }
    let _ = std::fs::write(p, v.to_string());
}

/// node_id 对用户友好的短显示（列表里配合名字区分重名设备）
pub fn short_id(node_id: &str) -> String {
    node_id.chars().take(4).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nwc-peers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn upsert_dedup_and_reorder() {
        let d = tmp("upsert");
        upsert(&d, "aa", "mac1", "1.1.1.1");
        upsert(&d, "bb", "win1", "2.2.2.2");
        upsert(&d, "aa", "mac1-renamed", "1.1.1.9");
        let list = load(&d);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].node_id, "aa", "最近握手应置顶");
        assert_eq!(list[0].name, "mac1-renamed");
        assert_eq!(list[0].last_ip, "1.1.1.9");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn upsert_caps_at_limit() {
        let d = tmp("cap");
        for i in 0..(MAX_PEERS + 3) {
            upsert(&d, &format!("n{i:02}"), "x", "1.1.1.1");
        }
        assert_eq!(load(&d).len(), MAX_PEERS);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pref_roundtrip_keeps_other_fields() {
        let d = tmp("pref");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("state.json"), r#"{"lastPeer":"3.3.3.3"}"#).unwrap();
        save_pref(&d, "abcd", true);
        assert_eq!(load_pref(&d), ("abcd".to_string(), true));
        let raw = std::fs::read_to_string(d.join("state.json")).unwrap();
        assert!(raw.contains("3.3.3.3"), "应保留其它字段");
        // 清除配对
        save_pref(&d, "", true);
        assert_eq!(load_pref(&d).0, "");
        let _ = std::fs::remove_dir_all(&d);
    }
}
