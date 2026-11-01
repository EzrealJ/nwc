//! 文件日志：~/.networkclipboard/logs/nwc-YYYY-MM-DD.log，按天滚动、保留 7 天
//! （GUI 模式没有控制台，日志的去处；--cli 模式只出控制台）

#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

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
        let mut st = self.state.lock().unwrap();
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
            let _ = writeln!(f, "{}", line);
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
