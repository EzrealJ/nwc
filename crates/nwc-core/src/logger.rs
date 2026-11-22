//! 文件日志：~/.networkclipboard/logs/nwc-YYYY-MM-DD.log，按天滚动、保留 7 天。
//! 两端 GUI 模式共用（没有控制台，日志的去处；--cli 模式只出控制台）。

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

pub struct FileLogger {
    dir: PathBuf,
    state: Mutex<(String, Option<fs::File>)>, // (当天文件名, 打开的句柄)
}

impl FileLogger {
    pub fn new(state_dir: &std::path::Path) -> FileLogger {
        let dir = state_dir.join("logs");
        let _ = fs::create_dir_all(&dir);
        FileLogger { dir, state: Mutex::new((String::new(), None)) }
    }

    pub fn write(&self, line: &str) {
        let date = crate::timestamp::now_date();
        // 锁中毒也继续写（panic hook 会在普通写入 panic 后调用，不能二次 panic）
        let mut st: MutexGuard<'_, _> =
            self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if st.0 != date || st.1.is_none() {
            let path = self.dir.join(format!("nwc-{date}.log"));
            match fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => {
                    st.0 = date.clone();
                    st.1 = Some(f);
                }
                Err(_) => return,
            }
        }
        if let Some(f) = st.1.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    /// 保留最近 keep 天
    pub fn prune(&self, keep_days: usize) {
        let mut names: Vec<String> = match fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .filter(|s| s.starts_with("nwc-") && s.ends_with(".log"))
                .collect(),
            Err(_) => return,
        };
        names.sort();
        while names.len() > keep_days {
            let oldest = names.remove(0);
            let _ = fs::remove_file(self.dir.join(oldest));
        }
    }

    pub fn latest_path(&self) -> PathBuf {
        let date = crate::timestamp::now_date();
        self.dir.join(format!("nwc-{date}.log"))
    }
}

/// 把 panic 信息写入文件日志。GUI 子系统没有 stderr（Windows 尤甚），
/// panic = abort 的发布配置下不装钩子就是「静默闪退、永无现场」。
/// 必须在拿到 state_dir、建好日志目录后调用；重复调用以最后一次为准。
pub fn install_panic_hook(logger: Arc<FileLogger>) {
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("<非字符串 panic 载荷>");
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<未知位置>".into());
        logger.write(&format!("[PANIC] 线程「{name}」崩溃: {msg} @ {loc}"));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// panic hook 必须把崩溃信息写进当天日志（GUI 无 stderr 的最后防线）
    #[test]
    fn panic_hook_writes_to_log() {
        let dir = std::env::temp_dir().join(format!("nwc-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logger = Arc::new(FileLogger::new(&dir));
        install_panic_hook(Arc::clone(&logger));
        let _ = std::panic::catch_unwind(|| panic!("boom-marker-12345"));
        std::panic::take_hook(); // 还原默认钩子，避免影响其它测试输出
        let content =
            std::fs::read_to_string(dir.join("logs").join(format!("nwc-{}.log", crate::timestamp::now_date()))).unwrap();
        assert!(content.contains("[PANIC]"), "缺 PANIC 标记: {content}");
        assert!(content.contains("boom-marker-12345"), "缺 panic 消息: {content}");
        assert!(content.contains("panic_hook_writes_to_log"), "缺崩溃位置: {content}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
