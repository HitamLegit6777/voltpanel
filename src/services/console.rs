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

/// Per-server console line store: monotonic sequence ids over a bounded ring.
#[derive(Debug, Clone)]
struct LineBuf {
    /// Seq to assign to the next newly created line (1-based; 0 = none yet).
    next_seq: u64,
    /// Seq of the last COMPLETE line. SSE events carry this id so a reconnecting
    /// client resumes after it; an in-progress partial replays under its own id.
    last_ok: u64,
    /// Ring of (seq, line), ascending seq, at most BUFFER_CAP entries.
    lines: Vec<(u64, String)>,
    /// True when `lines` ends in a partial line (last chunk had no trailing '\n').
    partial: bool,
}

impl Default for LineBuf {
    fn default() -> Self {
        Self {
            next_seq: 1,
            last_ok: 0,
            lines: Vec::new(),
            partial: false,
        }
    }
}

#[derive(Clone)]
pub struct ConsoleHub {
    pub config: Config,
    /// server_id -> per-server line buffer
    buffers: DashMap<i64, LineBuf>,
    /// server_id -> bounded broadcast channel of (last completed seq, raw chunk)
    subs: DashMap<i64, broadcast::Sender<(u64, String)>>,
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

    /// Append raw output from a server process. Splits into lines, assigns each a
    /// monotonic seq, and keeps the bounded ring. Cheap: no awaits; the buffer
    /// guard is dropped before the broadcast send.
    pub async fn append(&self, server_id: i64, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut buf = self.buffers.entry(server_id).or_default();
        let overflow = buf.lines.len().saturating_sub(BUFFER_CAP - 1);
        if overflow > 0 {
            buf.lines.drain(0..overflow);
        }
        let parts: Vec<&str> = text.split('\n').collect();
        let complete_count = parts.len() - 1; // '\n'-terminated lines in this chunk
                                              // The first part completes the pending partial, keeping its original seq.
        let mut start = 0;
        if buf.partial {
            let pending_seq = buf.lines.last().map(|(s, _)| *s);
            if let Some((_, l)) = buf.lines.last_mut() {
                l.push_str(parts[0]);
            }
            if complete_count >= 1 {
                if let Some(s) = pending_seq {
                    buf.last_ok = s;
                }
            }
            buf.partial = false;
            start = 1;
        }
        for part in parts.iter().take(complete_count).skip(start) {
            if part.is_empty() {
                continue;
            }
            let seq = buf.next_seq;
            buf.next_seq += 1;
            buf.lines.push((seq, part.to_string()));
            buf.last_ok = seq;
        }
        if !text.ends_with('\n') {
            // The trailing part is still partial; stash it as the pending line.
            // When it merely continued an existing partial it was already merged.
            let tail = parts[complete_count];
            let already_merged = start == 1 && complete_count == 0;
            if !already_merged && !tail.is_empty() {
                let seq = buf.next_seq;
                buf.next_seq += 1;
                buf.lines.push((seq, tail.to_string()));
            }
            buf.partial = true;
        }
        if buf.lines.len() > BUFFER_CAP {
            let overflow = buf.lines.len() - BUFFER_CAP;
            buf.lines.drain(0..overflow);
        }
        let live_id = buf.last_ok;
        drop(buf);
        // persist to log file
        if self.log_enabled {
            self.write_log(server_id, text);
        }
        if let Some(tx) = self.subs.get(&server_id) {
            let _ = tx.send((live_id, text.to_string()));
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

    /// Lines with seq > `after_seq` plus whether ids between `after_seq` and the
    /// buffer start were evicted. On truncation the whole buffer is returned so
    /// the client can rebuild; otherwise only the strictly-newer lines.
    pub fn history(&self, server_id: i64, after_seq: u64) -> (Vec<(u64, String)>, bool) {
        let Some(buf) = self.buffers.get(&server_id) else {
            return (Vec::new(), false);
        };
        let truncated = match buf.lines.first() {
            Some((first, _)) => *first > after_seq + 1,
            None => false,
        };
        let lines = if truncated {
            buf.lines.clone()
        } else {
            let start = buf.lines.partition_point(|(s, _)| *s <= after_seq);
            buf.lines[start..].to_vec()
        };
        (lines, truncated)
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

    pub fn subscribe(&self, server_id: i64) -> broadcast::Receiver<(u64, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hub() -> ConsoleHub {
        let mut cfg = Config::default();
        cfg.paths.logs_dir =
            std::env::temp_dir().join(format!("voltpanel-console-test-{}", std::process::id()));
        ConsoleHub::new(cfg)
    }

    #[tokio::test]
    async fn ids_are_monotonic_across_chunks() {
        let hub = test_hub();
        hub.append(1, "alpha\nbeta\n").await;
        hub.append(1, "gamma\n").await;
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        let seqs: Vec<u64> = lines.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let text: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(text, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn history_after_known_id_returns_exactly_newer_lines() {
        let hub = test_hub();
        hub.append(1, "a\nb\nc\nd\ne\n").await;
        let (lines, truncated) = hub.history(1, 2);
        assert!(!truncated);
        let expect: Vec<(u64, &str)> = vec![(3, "c"), (4, "d"), (5, "e")];
        assert_eq!(lines.len(), expect.len());
        for ((s, l), (es, el)) in lines.iter().zip(&expect) {
            assert_eq!(*s, *es);
            assert_eq!(l, el);
        }
    }

    #[tokio::test]
    async fn eviction_past_capacity_reports_truncated() {
        let hub = test_hub();
        for i in 0..(BUFFER_CAP + 50) {
            hub.append(1, &format!("line{i}\n")).await;
        }
        let (lines, truncated) = hub.history(1, 1);
        assert!(truncated);
        assert_eq!(lines.len(), BUFFER_CAP);
        assert_eq!(lines.first().map(|(s, _)| *s), Some(51));
        assert_eq!(lines.last().map(|(s, _)| *s), Some(550));
    }

    #[tokio::test]
    async fn id_newer_than_everything_returns_empty() {
        let hub = test_hub();
        hub.append(1, "a\nb\n").await;
        let (lines, truncated) = hub.history(1, 2);
        assert!(lines.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn partial_lines_keep_their_seq_until_completed() {
        let hub = test_hub();
        hub.append(1, "hel").await; // partial line, seq 1
        hub.append(1, "lo\n").await; // completes seq 1 in place
        hub.append(1, "bye").await; // new partial, seq 2
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        assert_eq!(
            lines,
            vec![(1, "hello".to_string()), (2, "bye".to_string())]
        );
    }
}
