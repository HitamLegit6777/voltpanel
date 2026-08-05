//! Console service: per-server ring buffer, SSE broadcast, log files.
use crate::config::Config;
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
pub struct ConsoleLine {
    pub server_id: i64,
    pub line: String,
    pub at: String,
}

#[derive(Clone)]
pub struct ConsoleHub {
    pub config: Config,
    /// server_id -> ring buffer (fixed cap)
    buffers: DashMap<i64, Vec<String>>,
    /// server_id -> bounded broadcast channel
    subs: DashMap<i64, broadcast::Sender<String>>,
    log_enabled: bool,
}

pub const BUFFER_CAP: usize = 500;

impl ConsoleHub {
    pub fn new(config: Config) -> Self {
        let log_enabled = std::fs::create_dir_all(&config.paths.logs_dir).is_ok();
        Self {
            config,
            buffers: DashMap::new(),
            subs: DashMap::new(),
            log_enabled,
        }
    }

    /// Append raw output from a server process.
    pub async fn append(&self, server_id: i64, text: &str) {
        if text.is_empty() {
            return;
        }
        // split into lines, keep partial tail buffered
        let mut buf = self.buffers.entry(server_id).or_default();
        let overflow = buf.len().saturating_sub(BUFFER_CAP - 1);
        if overflow > 0 {
            buf.drain(0..overflow);
        }
        let mut last = buf.last().cloned().unwrap_or_default();
        let parts: Vec<&str> = text.split('\n').collect();
        for (i, part) in parts.iter().enumerate() {
            let last_line = i == parts.len() - 1;
            if last_line && part.is_empty() {
                continue;
            }
            last.push_str(part);
            if last_line && !text.ends_with('\n') {
                // partial line, stash for next append
                if buf.is_empty() {
                    buf.push(std::mem::take(&mut last));
                } else if let Some(l) = buf.last_mut() {
                    *l = std::mem::take(&mut last);
                }
                continue;
            }
            if !last.is_empty() {
                buf.push(std::mem::take(&mut last));
            }
        }
        drop(buf);
        // persist to log file
        if self.log_enabled {
            self.write_log(server_id, text);
        }
        if let Some(tx) = self.subs.get(&server_id) {
            let _ = tx.send(text.to_string());
        }
    }

    fn write_log(&self, server_id: i64, text: &str) {
        use std::io::Write;
        let dir = self
            .config
            .paths
            .logs_dir
            .join(format!("server_{server_id}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("console.log");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(text.as_bytes());
        }
    }

    /// Historical lines for a server.
    pub fn history(&self, server_id: i64) -> Vec<String> {
        self.buffers
            .get(&server_id)
            .map(|b| b.clone())
            .unwrap_or_default()
    }

    pub fn clear(&self, server_id: i64) {
        self.buffers.remove(&server_id);
        let path = self
            .config
            .paths
            .logs_dir
            .join(format!("server_{server_id}/console.log"));
        let _ = std::fs::remove_file(path);
    }

    pub fn subscribe(&self, server_id: i64) -> broadcast::Receiver<String> {
        self.subs
            .entry(server_id)
            .or_insert_with(|| broadcast::channel(256).0)
            .subscribe()
    }

    pub fn clear_subs(&self, server_id: i64) {
        self.subs.remove(&server_id);
    }
}

/// Trim history for a deleted server.
pub fn drop_server(hub: &ConsoleHub, server_id: i64) {
    hub.buffers.remove(&server_id);
    hub.subs.remove(&server_id);
}
