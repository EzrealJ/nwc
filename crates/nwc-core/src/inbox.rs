//! 收件箱维护。收件箱是「文件中转缓存」而非归档存储：
//! - 自动：按保留天数清理（mtime 早于 cutoff 的文件与空目录），默认见 config.inbox_keep_days
//! - 手动：全量清空（菜单触发，见 EngineCmd::ClearInbox）
//! 另清理分块传输的陈旧临时文件（state/parts/，崩溃残留）。
//!
//! 安全约定：只用 symlink_metadata 判断（不跟随符号链接），只删常规文件与空目录。

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// 单个文件的元信息（供按 mtime 过滤）
struct Ent {
    path: std::path::PathBuf,
    mtime: SystemTime,
    size: u64,
}

/// 递归收集目录下的常规文件（symlink_metadata 判断，不跟随符号链接）
fn walk_files(dir: &Path, out: &mut Vec<Ent>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        // symlink_metadata：不跟随链接，避免误删链接目标；符号链接本身落入"其它类型"跳过
        let Ok(md) = fs::symlink_metadata(ent.path()) else { continue };
        if md.is_dir() {
            walk_files(&ent.path(), out);
        } else if md.is_file() {
            let mtime = md.modified().unwrap_or(SystemTime::now());
            out.push(Ent { path: ent.path(), mtime, size: md.len() });
        }
        // 其它类型（符号链接/套接字/管道等）不碰
    }
}

/// 删除 dir 下的空子目录（自底向上；保留 dir 本身）
fn prune_empty_dirs(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let subs: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    for sub in subs {
        if sub.is_dir() {
            prune_empty_dirs(&sub);
            let _ = fs::remove_dir(&sub); // 非空时失败即保留
        }
    }
}

/// 删除 mtime 早于 keep_days 天前的文件；返回 (删除文件数, 释放字节数)。
/// keep_days = 0 表示关闭自动清理。
pub fn prune_inbox(dir: &Path, keep_days: u64, now: SystemTime) -> (u32, u64) {
    if keep_days == 0 {
        return (0, 0);
    }
    let cutoff = now.checked_sub(Duration::from_secs(keep_days.saturating_mul(86400))).unwrap_or(now);
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    let mut n = 0u32;
    let mut bytes = 0u64;
    for f in files {
        if f.mtime < cutoff {
            if fs::remove_file(&f.path).is_ok() {
                n += 1;
                bytes += f.size;
            }
        }
    }
    if n > 0 {
        prune_empty_dirs(dir);
    }
    (n, bytes)
}

/// 全量清空收件箱（保留 inbox 目录本身）；返回 (删除文件数, 释放字节数)
pub fn clear_inbox(dir: &Path) -> (u32, u64) {
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    let mut n = 0u32;
    let mut bytes = 0u64;
    for f in files {
        if fs::remove_file(&f.path).is_ok() {
            n += 1;
            bytes += f.size;
        }
    }
    prune_empty_dirs(dir);
    let _ = fs::create_dir_all(dir);
    (n, bytes)
}

/// 清理分块传输残留的临时文件（state/parts/ 下超过 24 小时的 *-part）
pub fn prune_parts(state_dir: &Path, now: SystemTime) -> u32 {
    let parts = state_dir.join("parts");
    let cutoff = now.checked_sub(Duration::from_secs(24 * 3600)).unwrap_or(now);
    let mut files = Vec::new();
    walk_files(&parts, &mut files);
    let mut n = 0u32;
    for f in files {
        if f.mtime < cutoff && fs::remove_file(&f.path).is_ok() {
            n += 1;
        }
    }
    prune_empty_dirs(&parts);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_inbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nwc-inbox-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("sub/b.txt"), b"bb").unwrap();
        fs::write(dir.join("sub/deep/c.txt"), b"ccc").unwrap();
        dir
    }

    #[test]
    fn prune_respects_keep_days_with_fake_now() {
        let dir = mk_inbox("prune");
        // now 向未来推进 2 天 + keep 1 天 → cutoff 在"未来 1 天"，现有文件全部过期
        let future = SystemTime::now() + Duration::from_secs(2 * 86400);
        let (n, bytes) = prune_inbox(&dir, 1, future);
        assert_eq!((n, bytes), (3, 1 + 2 + 3));
        // 空目录应一并移除（只剩 inbox 根）
        assert!(dir.join("sub").exists() == false, "空子目录应被移除");
        assert!(dir.is_dir(), "inbox 根目录保留");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_fresh_files() {
        let dir = mk_inbox("fresh");
        let (n, _) = prune_inbox(&dir, 14, SystemTime::now());
        assert_eq!(n, 0, "刚写入的文件不应被清理");
        assert!(dir.join("sub/deep/c.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_disabled_when_zero() {
        let dir = mk_inbox("zero");
        let future = SystemTime::now() + Duration::from_secs(365 * 86400);
        let (n, _) = prune_inbox(&dir, 0, future);
        assert_eq!(n, 0, "keep_days=0 关闭自动清理");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_removes_everything() {
        let dir = mk_inbox("clear");
        let (n, bytes) = clear_inbox(&dir);
        assert_eq!((n, bytes), (3, 6));
        assert!(dir.exists() && fs::read_dir(&dir).unwrap().next().is_none(), "inbox 清空且目录保留");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_parts_removes_stale_only() {
        let state = std::env::temp_dir().join(format!("nwc-parts-test-{}", std::process::id()));
        let parts = state.join("parts");
        fs::create_dir_all(&parts).unwrap();
        fs::write(parts.join("x-part"), b"x").unwrap();
        // 未过期：不删
        assert_eq!(prune_parts(&state, SystemTime::now()), 0);
        assert!(parts.join("x-part").exists());
        // 推到 25 小时后：删
        let future = SystemTime::now() + Duration::from_secs(25 * 3600);
        assert_eq!(prune_parts(&state, future), 1);
        assert!(!parts.join("x-part").exists());
        let _ = fs::remove_dir_all(&state);
    }
}
