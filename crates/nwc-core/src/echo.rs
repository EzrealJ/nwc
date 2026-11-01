//! 回声抑制：窗口期内相同内容不再重复同步（对应 Node 版 src/echo.js）。

use std::collections::HashMap;
use std::time::Instant;

pub struct EchoGuard {
    ttl_ms: u128,
    map: HashMap<String, Instant>,
}

impl EchoGuard {
    pub fn new(ttl_ms: u64) -> Self {
        EchoGuard {
            ttl_ms: ttl_ms as u128,
            map: HashMap::new(),
        }
    }

    pub fn remember(&mut self, hash: &str) {
        self.map.insert(hash.to_string(), Instant::now());
        if self.map.len() > 2000 {
            self.prune();
        }
    }

    pub fn should_skip(&self, hash: &str) -> bool {
        match self.map.get(hash) {
            Some(t) => t.elapsed().as_millis() < self.ttl_ms,
            None => false,
        }
    }

    fn prune(&mut self) {
        self.map.retain(|_, t| t.elapsed().as_millis() <= self.ttl_ms);
    }
}
