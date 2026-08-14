//! Console service: per-server ring buffer, SSE broadcast, log files.
use crate::config::Config;
use crate::services::proc::ProcManager;
use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, mpsc};
use tokio::sync::broadcast;

/// What produced a console line: runtime output from the server process, or
/// install-script output. The UI styles install lines distinctly and the SSE
/// stream names its events after the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Runtime,
    Install,
}

impl LineKind {
    /// SSE event name used to deliver lines of this kind. Runtime output keeps
    /// the historical "console" event name so existing clients keep working;
    /// install lines ride their own "install" event.
    pub fn event_name(self) -> &'static str {
        match self {
            LineKind::Runtime => "console",
            LineKind::Install => "install",
        }
    }
}

/// One complete console line: its text plus the stream it came from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub text: String,
    pub kind: LineKind,
}

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
    lines: Vec<(u64, ConsoleEntry)>,
    /// True when `lines` ends in a partial line (last chunk had no trailing '\n').
    partial: bool,
    /// Kind of the in-flight partial line. A continuation chunk is only merged
    /// into it when the kinds match, so install text can never corrupt a
    /// pending runtime line (or vice versa); a kind switch seals the partial
    /// as complete and starts a fresh line.
    partial_kind: Option<LineKind>,
    /// True once the in-flight partial line hit `PARTIAL_CAP`. Continuation
    /// bytes are then dropped (not appended), so a writer that never emits
    /// '\n' cannot grow the ring entry — or the per-chunk `full` clone — past
    /// the cap.
    partial_truncated: bool,
}

impl Default for LineBuf {
    fn default() -> Self {
        Self {
            next_seq: 1,
            last_ok: 0,
            lines: Vec::new(),
            partial: false,
            partial_kind: None,
            partial_truncated: false,
        }
    }
}

/// A live console broadcast payload: `(seq, raw, full, ends_partial, kind)`.
///
/// `seq` is the own seq of the last ring line after the append, `raw` the raw
/// chunk text, `full` the merged text of that last line (the trailing
/// partial's current text for unterminated chunks, the completed line's text
/// otherwise), `ends_partial` whether the chunk leaves a line unterminated,
/// and `kind` the stream that produced the chunk. Consumers use `full` to tell
/// a continuation chunk that was already merged into a replayed line apart
/// from genuinely new content.
pub type ConsoleEvent = (u64, String, String, bool, LineKind);

#[derive(Clone)]
pub struct ConsoleHub {
    pub config: Config,
    /// server_id -> per-server line buffer
    buffers: DashMap<i64, LineBuf>,
    /// server_id -> bounded broadcast channel of live chunks.
    subs: DashMap<i64, broadcast::Sender<ConsoleEvent>>,
    log_enabled: bool,
    /// server_id -> bounded queue of chunks for the dedicated log-writer
    /// thread. Blocking file I/O (open, rotate, write) happens only on that
    /// thread, so an append from a Tokio task never stalls a worker or the
    /// shared log mutex; the queue is bounded and drops chunks (warn once per
    /// server) when the disk stalls.
    log_tx: mpsc::SyncSender<LogCmd>,
    /// servers that already logged a console-log write failure (warn once).
    log_warned: DashSet<i64>,
    /// server_id -> dedicated stdin writer for console commands. One thread
    /// per server serializes child-stdin writes, so a wedged child (a process
    /// that never drains its pipe) can only stall that one writer thread and
    /// the bounded queue makes new commands fail fast instead of pinning a
    /// blocking-pool thread and the stdin mutex forever.
    stdin_writers: DashMap<i64, StdinWriter>,
    /// Console watcher engine, wired in after construction (the engine holds a
    /// `Weak` back-edge to this hub, so the cell is shared across clones and
    /// set exactly once). `None` until `set_engine`; `append` skips evaluation
    /// while unset.
    engine: Arc<OnceLock<Arc<super::watcher::WatcherEngine>>>,
}

pub const BUFFER_CAP: usize = 500;

/// Per-server log file rotation threshold: the log rotates once this chunk
/// would push it past this many bytes, keeping `console.log.1` and
/// `console.log.2` as backups.
pub const LOG_ROTATE_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

/// Cap for an unterminated partial line. A writer that streams forever
/// without a '\n' is truncated at this size with a visible marker; the flag
/// on the line then drops all further continuation bytes.
pub const PARTIAL_CAP: usize = 64 * 1024; // 64 KiB
const PARTIAL_TRUNC_MARKER: &str = "…[truncated]";

/// Append `add` to the in-flight partial line `entry`, capping the total at
/// `PARTIAL_CAP` bytes. Once capped, the line is marked truncated and any
/// later continuation bytes are dropped: a writer that never emits '\n'
/// cannot grow the ring entry or the per-chunk `full` clone without bound.
fn merge_partial(entry: &mut ConsoleEntry, truncated: bool, add: &str) -> bool {
    if truncated {
        return true;
    }
    let room = PARTIAL_CAP.saturating_sub(entry.text.len());
    if add.len() > room {
        let cut = add.floor_char_boundary(room);
        entry.text.push_str(&add[..cut]);
        entry.text.push_str(PARTIAL_TRUNC_MARKER);
        true
    } else {
        entry.text.push_str(add);
        truncated
    }
}

impl ConsoleHub {
    pub fn new(config: Config) -> Self {
        let log_enabled = std::fs::create_dir_all(&config.paths.logs_dir).is_ok();
        // Dedicated log-writer thread: all blocking console.log I/O (lazy
        // open, rotation, write) happens here, never on a Tokio worker or on
        // a caller of append(). The thread exits when the last sender clone
        // is dropped (hub teardown); `Flush` commands give synchronous
        let (log_tx, log_rx) = mpsc::sync_channel(LOG_QUEUE_CAP);
        let base_dir = config.paths.logs_dir.clone();
        std::thread::spawn(move || log_writer_loop(base_dir, log_rx));
        Self {
            config,
            buffers: DashMap::new(),
            subs: DashMap::new(),
            log_enabled,
            log_tx,
            log_warned: DashSet::new(),
            stdin_writers: DashMap::new(),
            engine: Arc::new(OnceLock::new()),
        }
    }

    /// Wire the console watcher engine in after both it and the hub exist (the
    /// engine holds a `Weak` back-edge to this hub). Idempotent: a second call
    /// is ignored, matching the set-once cell.
    pub fn set_engine(&self, engine: Arc<super::watcher::WatcherEngine>) {
        let _ = self.engine.set(engine);
    }

    /// Append raw output from a server process or install script. Splits into
    /// lines, assigns each a monotonic seq, keeps the bounded ring, and
    /// broadcasts the live chunk. Sync on purpose: it is called from blocking
    /// reaper and install threads as well as Tokio tasks.
    pub fn append(&self, server_id: i64, text: &str, kind: LineKind) {
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
        let mut start = 0;
        if buf.partial {
            let pending_seq = buf.lines.last().map(|(s, _)| *s);
            if buf.partial_kind == Some(kind) {
                // The first part completes the pending partial, keeping its
                // original seq. Read the truncation flag before borrowing the
                // ring so the two mutable borrows never overlap.
                let was_truncated = buf.partial_truncated;
                let now_truncated = if let Some((_, e)) = buf.lines.last_mut() {
                    merge_partial(e, was_truncated, parts[0])
                } else {
                    was_truncated
                };
                if now_truncated {
                    buf.partial_truncated = true;
                }
                if complete_count >= 1 {
                    if let Some(s) = pending_seq {
                        buf.last_ok = s;
                    }
                }
                buf.partial = false;
                start = 1;
            } else {
                // A different stream took over mid-line: seal the pending
                // partial as complete (it is a real line of its own kind) and
                // treat this chunk as a fresh line, so install text can never
                // merge into a runtime partial or vice versa.
                if let Some(s) = pending_seq {
                    buf.last_ok = s;
                }
                buf.partial = false;
                start = 0;
            }
        }
        // Console watchers evaluate only completed Runtime lines: install-script
        // output must never trigger restart/stop/notify actions. Collect the
        // just-completed line texts (borrowed from `parts`, valid past `drop`)
        // only when a watcher engine is wired and this chunk is runtime output.
        let watch = matches!(kind, LineKind::Runtime) && self.engine.get().is_some();
        let mut completed: Vec<&str> = Vec::new();
        for part in parts.iter().take(complete_count).skip(start) {
            if part.is_empty() {
                continue;
            }
            let seq = buf.next_seq;
            buf.next_seq += 1;
            buf.lines.push((seq, ConsoleEntry { text: part.to_string(), kind }));
            buf.last_ok = seq;
            if watch {
                completed.push(part);
            }
        }
        if !text.ends_with('\n') {
            // The trailing part is still partial; stash it as the pending line.
            // When it merely continued an existing partial it was already merged.
            let tail = parts[complete_count];
            let already_merged = start == 1 && complete_count == 0;
            if !already_merged && !tail.is_empty() {
                let seq = buf.next_seq;
                buf.next_seq += 1;
                let mut entry = ConsoleEntry { text: String::new(), kind };
                let truncated = merge_partial(&mut entry, false, tail);
                buf.partial_truncated = truncated;
                buf.lines.push((seq, entry));
            }
            buf.partial = true;
            buf.partial_kind = Some(kind);
        }
        if buf.lines.len() > BUFFER_CAP {
            let overflow = buf.lines.len() - BUFFER_CAP;
            buf.lines.drain(0..overflow);
        }
        let live_seq = buf.lines.last().map(|(s, _)| *s).unwrap_or(buf.last_ok);
        let live_full = buf
            .lines
            .last()
            .map(|(_, e)| e.text.clone())
            .unwrap_or_default();
        let partial_end = buf.partial;
        drop(buf);
        // Evaluate console watchers off the ring lock (the buffer is dropped).
        // Cheap and allocation-free when nothing matches; dispatch is spawned
        // onto the Tokio handle inside the engine, so this never blocks append.
        if watch && !completed.is_empty() {
            if let Some(engine) = self.engine.get() {
                engine.evaluate(server_id, &completed);
            }
        }
        // Persist to the log file on the dedicated writer thread (blocking
        // I/O never runs on a caller of append). The queue is bounded; when
        // the disk stalls chunks are dropped (warn once per server) rather
        // than buffering a chatty server's whole output in RAM.
        if self.log_enabled {
            match self.log_tx.try_send(LogCmd::Write {
                server_id,
                text: text.to_string(),
            }) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => self.warn_log_dropped(server_id),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    // Writer thread already gone (hub teardown): drop quietly.
                }
            }
        }
        if let Some(tx) = self.subs.get(&server_id) {
            // Cap the live `raw` chunk at PARTIAL_CAP with the same marker the
            // ring uses: the merged `full` text is already bounded by the ring
            // for partial lines, but `raw` was passed through untouched, so a
            // single multi-MiB drain (proc forward merges up to 1024x4KiB)
            // used to be cloned per event per subscriber (256 slots x
            // subscribers). The ends_partial flag still reflects whether the
            // chunk leaves a line unterminated, which keeps the client's
            // partial-dedup intact.
            let raw = if text.len() > PARTIAL_CAP {
                let mut capped = ConsoleEntry {
                    text: String::new(),
                    kind,
                };
                merge_partial(&mut capped, false, text);
                capped.text
            } else {
                text.to_string()
            };
            let _ = tx.send((live_seq, raw, live_full, partial_end, kind));
        }
    }
    /// Surface the first dropped (queue-full) console-log chunk per server as
    /// a warn; later drops stay silent so a stalled disk cannot spam the log.
    fn warn_log_dropped(&self, server_id: i64) {
        if self.log_warned.insert(server_id) {
            tracing::warn!(
                "console log backlog full for server {server_id}: dropping output until the disk catches up"
            );
        }
    }

    /// Wait until every queued log command has been processed by the writer
    /// thread. Only tests need this barrier (they assert on file state right
    /// after an append/clear/drop); production never blocks on log I/O.
    pub fn flush_log(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.log_tx.send(LogCmd::Flush { ack: ack_tx }).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// Send one console command to a server's stdin through the dedicated
    /// per-server writer. A child that stops draining its pipe can only wedge
    /// that one writer thread; every later command either times out against
    /// its own bounded slot (never pinning a blocking-pool thread or the
    /// stdin mutex) or is rejected immediately when the queue is full.
    pub async fn write_stdin(
        &self,
        server_id: i64,
        procs: Arc<ProcManager>,
        cmd: String,
    ) -> Result<(), StdinError> {
        let tx = {
            let w = self
                .stdin_writers
                .entry(server_id)
                .or_insert_with(|| StdinWriter::spawn(procs, server_id));
            w.tx.clone()
        };
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        match tx.try_send(StdinCmd {
            line: cmd,
            ack: ack_tx,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => return Err(StdinError::Busy),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                // Writer thread exited (server torn down); drop the stale
                // entry so the next call spawns a fresh writer.
                self.stdin_writers.remove(&server_id);
                return Err(StdinError::Busy);
            }
        }
        match tokio::time::timeout(COMMAND_WRITE_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(StdinError::WriteFailed(e)),
            Ok(Err(_)) => Err(StdinError::WriteFailed(
                "console command writer went away".to_string(),
            )),
            Err(_) => Err(StdinError::TimedOut),
        }
    }

    /// Snapshot for a resuming client: lines with seq > `after_seq`, whether the
    /// ring evicted ids the client still needs (client should rebuild), whether
    /// the last line is an unterminated partial, and the seq of the last
    /// completed line (the resume cursor that stays behind that partial).
    pub fn snapshot(
        &self,
        server_id: i64,
        after_seq: u64,
    ) -> (Vec<(u64, ConsoleEntry)>, bool, bool, u64) {
        let Some(buf) = self.buffers.get(&server_id) else {
            return (Vec::new(), false, false, 0);
        };
        let truncated = match buf.lines.first() {
            // saturating_add: `after_seq` is client-controlled (?since=u64::MAX
            // would wrap `after_seq + 1` to 0, panicking in debug and wrongly
            // reporting truncation in release).
            Some((first, _)) => *first > after_seq.saturating_add(1),
            None => false,
        };
        let lines = if truncated {
            buf.lines.clone()
        } else {
            let start = buf.lines.partition_point(|(s, _)| *s <= after_seq);
            buf.lines[start..].to_vec()
        };
        (lines, truncated, buf.partial, buf.last_ok)
    }

    /// Lines with seq > `after_seq` plus whether ids between `after_seq` and the
    /// buffer start were evicted. On truncation the whole buffer is returned so
    /// the client can rebuild; otherwise only the strictly-newer lines.
    pub fn history(&self, server_id: i64, after_seq: u64) -> (Vec<(u64, ConsoleEntry)>, bool) {
        let (lines, truncated, _, _) = self.snapshot(server_id, after_seq);
        (lines, truncated)
    }

    pub fn clear(&self, server_id: i64) {
        // Keep next_seq: ids stay monotonic across clears so a client that
        // reconnects with a pre-clear cursor sees the new lines (via a truncated
        // rebuild) instead of silently filtering every new line out.
        if let Some(mut buf) = self.buffers.get_mut(&server_id) {
            buf.lines.clear();
            buf.partial = false;
            buf.partial_kind = None;
            buf.partial_truncated = false;
            buf.last_ok = 0;
        }
        // Remove the persisted log on the writer thread, in queue order: a
        // chunk queued before the clear lands first, then the file is dropped,
        // so the next append starts from a fresh file.
        let _ = self.log_tx.send(LogCmd::Clear { server_id });
        // Tell live subscribers to rebuild their view. The sentinel is
        // unambiguous: real events always carry a non-empty raw chunk and a
        // seq >= 1 (next_seq only ever increments). Seq 0 + empty raw means
        // "console cleared".
        if let Some(tx) = self.subs.get(&server_id) {
            let _ = tx.send((0, String::new(), String::new(), false, LineKind::Runtime));
        }
    }

    /// Subscribe to a server's live console stream, up to
    /// `MAX_SUBSCRIBERS_PER_SERVER` concurrent receivers. The broadcast copies
    /// every chunk to every receiver, so an unbounded subscriber count would
    /// turn each append into O(subscribers) work and memory; the API maps the
    /// rejection to 429.
    pub fn subscribe(
        &self,
        server_id: i64,
    ) -> Result<broadcast::Receiver<ConsoleEvent>, TooManySubscribers> {
        let sender = self
            .subs
            .entry(server_id)
            .or_insert_with(|| broadcast::channel(256).0);
        if sender.receiver_count() >= MAX_SUBSCRIBERS_PER_SERVER {
            return Err(TooManySubscribers);
        }
        Ok(sender.subscribe())
    }

    pub fn clear_subs(&self, server_id: i64) {
        self.subs.remove(&server_id);
    }
}

/// Live SSE subscribers allowed per server before further stream requests are
/// refused. The broadcast copies every chunk to every receiver, so capping
/// subscribers bounds the per-append O(subscribers) copy cost.
pub const MAX_SUBSCRIBERS_PER_SERVER: usize = 8;

/// Time cap on one blocking child-stdin write; a wedged child must not hang
/// the request forever.
pub const COMMAND_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Failure modes of [`ConsoleHub::write_stdin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinError {
    /// The per-server writer queue is full: earlier commands are backed up on
    /// a child that is not draining stdin. Fail fast rather than queue more
    /// doomed writes.
    Busy,
    /// The child did not drain the write within [`COMMAND_WRITE_TIMEOUT`].
    TimedOut,
    /// The write itself failed (child exited, stdin not available, ...).
    WriteFailed(String),
}

/// The subscriber cap was reached for a server's live console stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManySubscribers;

/// One queued console command for a server's stdin writer thread.
struct StdinCmd {
    line: String,
    /// Delivers the write result back to the waiting request; the sender
    /// side drops it (and the request times out) when the write is abandoned.
    ack: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// Per-server stdin writer: one dedicated std thread serializes child-stdin
/// writes through a bounded queue. A child that stops draining its pipe can
/// only wedge this one thread (never a blocking-pool thread, never the stdin
/// mutex held by anyone else); the queue bounds how many commands back up.
#[derive(Clone)]
struct StdinWriter {
    tx: mpsc::SyncSender<StdinCmd>,
}

const STDIN_QUEUE_CAP: usize = 16;

impl StdinWriter {
    fn spawn(procs: Arc<ProcManager>, server_id: i64) -> Self {
        let (tx, rx): (mpsc::SyncSender<StdinCmd>, mpsc::Receiver<StdinCmd>) =
            mpsc::sync_channel(STDIN_QUEUE_CAP);
        std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                let res = procs
                    .send_input(server_id, &cmd.line)
                    .map_err(|e| e.to_string());
                let _ = cmd.ack.send(res);
            }
        });
        Self { tx }
    }
}

/// One command for the dedicated log-writer thread.
enum LogCmd {
    Write {
        server_id: i64,
        text: String,
    },
    Clear {
        server_id: i64,
    },
    Drop {
        server_id: i64,
    },
    /// Barrier: the sender blocks until the writer reaches this command.
    Flush {
        ack: mpsc::Sender<()>,
    },
}

/// Bound on queued-but-unwritten log chunks per hub. When the disk stalls the
/// writer cannot keep up and append() drops new chunks instead of buffering a
/// chatty server's whole output in RAM (same policy as the console forwarder).
const LOG_QUEUE_CAP: usize = 1024;

/// Dedicated log-writer thread body. All blocking console.log I/O (lazy open,
/// rotation, sanitize, write) happens here, in FIFO order, so appends from
/// Tokio tasks never block on disk and per-server file ordering is preserved.
fn log_writer_loop(base_dir: std::path::PathBuf, rx: mpsc::Receiver<LogCmd>) {
    let mut files: HashMap<i64, std::fs::File> = HashMap::new();
    let mut warned: HashSet<i64> = HashSet::new();
    for cmd in rx {
        match cmd {
            LogCmd::Write { server_id, text } => {
                write_log_chunk(&base_dir, server_id, &text, &mut files, &mut warned);
            }
            LogCmd::Clear { server_id } => {
                files.remove(&server_id);
                let _ = std::fs::remove_file(
                    base_dir.join(format!("server_{server_id}/console.log")),
                );
            }
            LogCmd::Drop { server_id } => {
                files.remove(&server_id);
                let dir = base_dir.join(format!("server_{server_id}"));
                let _ = std::fs::remove_file(dir.join("console.log"));
                let _ = std::fs::remove_file(dir.join("console.log.1"));
                let _ = std::fs::remove_file(dir.join("console.log.2"));
                let _ = std::fs::remove_dir(&dir);
            }
            LogCmd::Flush { ack } => {
                let _ = ack.send(());
            }
        }
    }
}

/// Surface the first console-log failure per server as a warn; later failures
/// stay silent so a wedged disk cannot spam the log forever.
fn warn_once(warned: &mut HashSet<i64>, server_id: i64, what: &str) {
    if warned.insert(server_id) {
        tracing::warn!("console log write failed for server {server_id}: {what}");
    }
}

/// Write one sanitized chunk to a server's console.log, keeping one open
/// handle per server. The log rotates once this chunk would push it past
/// `LOG_ROTATE_BYTES`: console.log -> console.log.1 -> console.log.2, then a
/// fresh console.log is opened.
fn write_log_chunk(
    base_dir: &std::path::Path,
    server_id: i64,
    text: &str,
    files: &mut HashMap<i64, std::fs::File>,
    warned: &mut HashSet<i64>,
) {
    use std::io::Write;
    // Terminal escape injection: neutralize C0 controls and ESC sequences in
    // the PERSISTED copy (the live SSE stream keeps the raw text). The tailer
    // of console.log therefore cannot be driven by server output.
    let text = sanitize_log_text(text);
    if text.is_empty() {
        return;
    }
    let dir = base_dir.join(format!("server_{server_id}"));
    if std::fs::create_dir_all(&dir).is_err() {
        warn_once(warned, server_id, "could not create log directory");
        return;
    }
    let rotated = {
        let f = match files.get_mut(&server_id) {
            Some(f) => f,
            None => {
                // Lazy-open once per server: reuse the handle instead of
                // reopening the file on every chunk.
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("console.log"))
                {
                    Ok(f) => {
                        files.insert(server_id, f);
                        files.get_mut(&server_id).expect("just inserted")
                    }
                    Err(e) => {
                        warn_once(warned, server_id, &format!("open console.log: {e}"));
                        return;
                    }
                }
            }
        };
        // Rotate when this chunk would push the log past the cap.
        f.metadata()
            .map(|m| m.len() + text.len() as u64 > LOG_ROTATE_BYTES)
            .unwrap_or(false)
    };
    if rotated {
        if let Some(f) = files.remove(&server_id) {
            drop(f); // close before the file it points at is renamed
            let _ = std::fs::remove_file(dir.join("console.log.2"));
            let _ = std::fs::rename(dir.join("console.log.1"), dir.join("console.log.2"));
            let _ = std::fs::rename(dir.join("console.log"), dir.join("console.log.1"));
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("console.log"))
        {
            Ok(f) => {
                files.insert(server_id, f);
            }
            Err(e) => {
                warn_once(warned, server_id, &format!("reopen console.log after rotation: {e}"));
                return;
            }
        }
    }
    if let Some(f) = files.get_mut(&server_id) {
        if let Err(e) = f.write_all(text.as_bytes()) {
            warn_once(warned, server_id, &format!("write console.log: {e}"));
        }
    }
}

/// Neutralize terminal control sequences in a chunk before it is persisted:
/// C0 controls (except `\t` `\n` `\r`) and DEL are dropped, and any
/// ESC-initiated sequence (CSI/OSC/DCS/PM/APC/charset designator) is removed
/// wholesale. The dangerous byte is always the ESC itself, so even a sequence
/// split across chunk boundaries leaves only inert literal text behind.
fn sanitize_log_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                i += 1;
                match bytes.get(i) {
                    Some(0x5b) => {
                        // CSI: params (0x30-0x3F), intermediates (0x20-0x2F),
                        // final byte (0x40-0x7E).
                        i += 1;
                        while i < bytes.len()
                            && (0x30..=0x3f).contains(&bytes[i])
                        {
                            i += 1;
                        }
                        while i < bytes.len()
                            && (0x20..=0x2f).contains(&bytes[i])
                        {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                    }
                    Some(0x5d | 0x50 | 0x5e | 0x5f) => {
                        // OSC/DCS/PM/APC: consume until BEL or ST (ESC \).
                        i += 1;
                        while i < bytes.len() && bytes[i] != 0x07 {
                            if bytes[i] == 0x1b {
                                if bytes.get(i + 1) == Some(&0x5c) {
                                    i += 2;
                                } else {
                                    i += 1;
                                }
                                break;
                            }
                            i += 1;
                        }
                    }
                    Some(0x28..=0x2b) => {
                        // Charset designator: ESC ( A / ESC ) B etc.
                        i += 2;
                    }
                    Some(_) => {
                        // Single-character escape (ESC 7, ESC =, ESC c, ...).
                        i += 1;
                    }
                    None => {}
                }
            }
            b if b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r') => i += 1,
            0x7f => i += 1,
            _ => {
                let ch = text[i..].chars().next().expect("valid utf-8");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// Trim history, live subscriptions, stdin writer, and persisted logs for a
/// deleted server. Log files are removed by the writer thread (in queue
/// order), so a chunk queued before the drop cannot recreate a file that a
/// concurrent synchronous removal had already unlinked.
pub fn drop_server(hub: &ConsoleHub, server_id: i64) {
    hub.buffers.remove(&server_id);
    hub.subs.remove(&server_id);
    hub.stdin_writers.remove(&server_id);
    let _ = hub.log_tx.send(LogCmd::Drop { server_id });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hub() -> ConsoleHub {
        let mut cfg = Config::default();
        // A unique logs dir per hub: tests run concurrently in one process and
        // share server ids, so a shared dir lets one test's append recreate a
        // log file another test just deleted.
        static DIR_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = DIR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        cfg.paths.logs_dir = std::env::temp_dir().join(format!(
            "voltpanel-console-test-{}-{id}",
            std::process::id()
        ));
        ConsoleHub::new(cfg)
    }
    /// Receive the next broadcast with a hard timeout so a missing event
    /// fails the test instead of hanging the whole suite.
    async fn recv_timeout(
        rx: &mut broadcast::Receiver<ConsoleEvent>,
    ) -> Result<ConsoleEvent, broadcast::error::RecvError> {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for a console broadcast")
    }

    #[tokio::test]
    async fn ids_are_monotonic_across_chunks() {
        let hub = test_hub();
        hub.append(1, "alpha\nbeta\n", LineKind::Runtime);
        hub.append(1, "gamma\n", LineKind::Runtime);
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        let seqs: Vec<u64> = lines.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let text: Vec<&str> = lines.iter().map(|(_, e)| e.text.as_str()).collect();
        assert_eq!(text, vec!["alpha", "beta", "gamma"]);
        assert!(lines.iter().all(|(_, e)| e.kind == LineKind::Runtime));
    }

    #[tokio::test]
    async fn history_after_known_id_returns_exactly_newer_lines() {
        let hub = test_hub();
        hub.append(1, "a\nb\nc\nd\ne\n", LineKind::Runtime);
        let (lines, truncated) = hub.history(1, 2);
        assert!(!truncated);
        let expect: Vec<(u64, &str)> = vec![(3, "c"), (4, "d"), (5, "e")];
        assert_eq!(lines.len(), expect.len());
        for ((s, e), (es, el)) in lines.iter().zip(&expect) {
            assert_eq!(*s, *es);
            assert_eq!(e.text, *el);
        }
    }

    #[tokio::test]
    async fn eviction_past_capacity_reports_truncated() {
        let hub = test_hub();
        for i in 0..(BUFFER_CAP + 50) {
            hub.append(1, &format!("line{i}\n"), LineKind::Runtime);
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
        hub.append(1, "a\nb\n", LineKind::Runtime);
        let (lines, truncated) = hub.history(1, 2);
        assert!(lines.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn partial_lines_keep_their_seq_until_completed() {
        let hub = test_hub();
        hub.append(1, "hel", LineKind::Runtime); // partial line, seq 1
        hub.append(1, "lo\n", LineKind::Runtime); // completes seq 1 in place
        hub.append(1, "bye", LineKind::Runtime); // new partial, seq 2
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        let text: Vec<(u64, String)> = lines
            .into_iter()
            .map(|(s, e)| (s, e.text))
            .collect();
        assert_eq!(
            text,
            vec![(1, "hello".to_string()), (2, "bye".to_string())]
        );
    }

    #[tokio::test]
    async fn clear_keeps_sequence_monotonic() {
        let hub = test_hub();
        hub.append(1, "a\nb\nc\n", LineKind::Runtime); // seqs 1..3
        hub.clear(1);
        hub.append(1, "d\n", LineKind::Runtime);
        hub.append(1, "e\n", LineKind::Runtime);
        let (lines, truncated) = hub.history(1, 0);
        assert!(truncated, "a post-clear ring cannot resume from 0");
        let seqs: Vec<u64> = lines.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![4, 5], "seqs keep counting across clear");
    }

    #[tokio::test]
    async fn stale_cursor_after_clear_still_sees_new_lines() {
        let hub = test_hub();
        hub.append(1, "a\nb\nc\n", LineKind::Runtime); // seqs 1..3
        hub.clear(1);
        hub.append(1, "d\n", LineKind::Runtime); // seq 4 — must not be silently filtered out
        let (lines, truncated) = hub.history(1, 3);
        assert!(!truncated);
        let text: Vec<(u64, String)> = lines
            .into_iter()
            .map(|(s, e)| (s, e.text))
            .collect();
        assert_eq!(text, vec![(4, "d".to_string())]);
    }

    #[tokio::test]
    async fn snapshot_reports_partial_and_last_completed() {
        let hub = test_hub();
        hub.append(1, "a\n", LineKind::Runtime);
        hub.append(1, "hel", LineKind::Runtime); // partial seq 2, last completed 1
        let (_, _, partial, last_ok) = hub.snapshot(1, 0);
        assert!(partial);
        assert_eq!(last_ok, 1);
        hub.append(1, "lo\n", LineKind::Runtime); // completes seq 2
        let (lines, _, partial, last_ok) = hub.snapshot(1, 0);
        assert!(!partial);
        assert_eq!(last_ok, 2);
        let text: Vec<(u64, String)> = lines
            .into_iter()
            .map(|(s, e)| (s, e.text))
            .collect();
        assert_eq!(text, vec![(1, "a".to_string()), (2, "hello".to_string())]);
    }

    #[tokio::test]
    async fn history_resync_covers_lagged_gap() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        // Overflow the 256-slot broadcast channel so the receiver lags.
        for i in 0..300 {
            hub.append(1, &format!("l{i}\n"), LineKind::Runtime);
        }
        assert!(matches!(
            recv_timeout(&mut rx).await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_))
        ));
        // The ring still holds everything (BUFFER_CAP > 300), so resyncing from
        // the cursor replays the whole gap without loss.
        let (lines, _) = hub.history(1, 0);
        assert_eq!(lines.len(), 300);
        assert_eq!(lines.first().map(|(s, _)| *s), Some(1));
        assert_eq!(lines.last().map(|(s, _)| *s), Some(300));
    }

    #[tokio::test]
    async fn drop_server_deletes_persisted_log() {
        let hub = test_hub();
        hub.append(1, "a\n", LineKind::Runtime);
        hub.flush_log(); // writer thread is async; the test asserts on disk
        let path = hub.config.paths.logs_dir.join("server_1/console.log");
        assert!(path.exists());
        drop_server(&hub, 1);
        hub.flush_log();
        assert!(!path.exists());
        let (lines, _) = hub.history(1, 0);
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn broadcast_chunks_carry_monotonic_seq_and_partial_flag() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        hub.append(1, "a\n", LineKind::Runtime);
        hub.append(1, "hel", LineKind::Runtime); // in-flight line, seq 2
        hub.append(1, "lo", LineKind::Runtime); // continuation of the same in-flight line
        hub.append(1, "\n", LineKind::Runtime); // completes seq 2
        hub.append(1, "b\n", LineKind::Runtime);
        let (seq1, raw1, full1, p1, k1) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq1, raw1.as_str(), full1.as_str(), p1, k1),
            (1, "a\n", "a", false, LineKind::Runtime)
        );
        let (seq2, raw2, full2, p2, k2) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq2, raw2.as_str(), full2.as_str(), p2, k2),
            (2, "hel", "hel", true, LineKind::Runtime)
        );
        let (seq3, raw3, full3, p3, k3) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq3, raw3.as_str(), full3.as_str(), p3, k3),
            (2, "lo", "hello", true, LineKind::Runtime)
        );
        let (seq4, raw4, full4, p4, k4) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq4, raw4.as_str(), full4.as_str(), p4, k4),
            (2, "\n", "hello", false, LineKind::Runtime)
        );
        let (seq5, raw5, full5, p5, k5) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq5, raw5.as_str(), full5.as_str(), p5, k5),
            (3, "b\n", "b", false, LineKind::Runtime)
        );
        // No id skipped: 1, 2, 2, 2, 3 — the in-flight line keeps its own seq.
        assert_eq!(hub.history(1, 0).0.len(), 3);
    }

    #[tokio::test]
    async fn broadcast_chunk_with_embedded_complete_line_reports_new_partial_seq() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        // One chunk "a\nb": completes seq 1 and opens the in-flight seq 2. A
        // single append emits exactly one broadcast event; the completion of
        // seq 2 is a second chunk ("c\n") and its own event.
        hub.append(1, "a\nb", LineKind::Runtime);
        let (seq, raw, full, partial, kind) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq, raw.as_str(), full.as_str(), partial, kind),
            (2, "a\nb", "b", true, LineKind::Runtime)
        );
        hub.append(1, "c\n", LineKind::Runtime); // completes the in-flight seq 2
        let (seq2, raw2, full2, partial2, kind2) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq2, raw2.as_str(), full2.as_str(), partial2, kind2),
            (2, "c\n", "bc", false, LineKind::Runtime)
        );
    }

    #[tokio::test]
    async fn clear_resets_cursor_but_keeps_seq_monotonic_and_prunes_log() {
        let hub = test_hub();
        hub.append(1, "a\nb\nc\n", LineKind::Runtime); // seqs 1..3
        hub.flush_log();
        let path = hub.config.paths.logs_dir.join("server_1/console.log");
        assert!(path.exists());
        hub.clear(1);
        hub.flush_log();
        assert!(
            !path.exists(),
            "clear must delete the persisted console log"
        );
        hub.append(1, "d\n", LineKind::Runtime); // seq 4
        hub.flush_log();
        let path2 = hub.config.paths.logs_dir.join("server_1/console.log");
        assert!(path2.exists(), "append after clear must recreate the log");
        let (lines, truncated) = hub.history(1, 3);
        assert!(
            !truncated,
            "a pre-clear cursor must still see post-clear lines"
        );
        let text: Vec<(u64, String)> = lines
            .into_iter()
            .map(|(s, e)| (s, e.text))
            .collect();
        assert_eq!(text, vec![(4, "d".to_string())]);
        let (all, _) = hub.history(1, 0);
        assert_eq!(all.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![4]);
    }

    #[tokio::test]
    async fn clear_wipes_ring_and_partial_state() {
        let hub = test_hub();
        hub.append(1, "hel", LineKind::Runtime); // in-flight seq 1
        hub.clear(1);
        let (lines, truncated, partial, last_ok) = hub.snapshot(1, 0);
        assert!(lines.is_empty());
        assert!(!partial, "clear must drop the pending partial");
        assert_eq!(last_ok, 0);
        assert!(!truncated);
        // The in-flight seq must not resurface after the clear.
        hub.append(1, "x\n", LineKind::Runtime);
        let (lines, _, _, _) = hub.snapshot(1, 0);
        let text: Vec<(u64, String)> = lines
            .into_iter()
            .map(|(s, e)| (s, e.text))
            .collect();
        assert_eq!(text, vec![(2, "x".to_string())]);
    }

    // ---------------- Install output tagging (G11) ----------------

    #[tokio::test]
    async fn install_lines_are_tagged_and_replayable() {
        let hub = test_hub();
        hub.append(1, "runtime line\n", LineKind::Runtime);
        hub.append(1, "fetching deps...\n", LineKind::Install);
        hub.append(1, "runtime again\n", LineKind::Runtime);
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].1.kind, LineKind::Runtime);
        assert_eq!(lines[0].1.text, "runtime line");
        assert_eq!(lines[1].1.kind, LineKind::Install);
        assert_eq!(lines[1].1.text, "fetching deps...");
        assert_eq!(lines[2].1.kind, LineKind::Runtime);
        assert_eq!(lines[2].1.text, "runtime again");
    }

    #[tokio::test]
    async fn broadcast_carries_install_kind() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        hub.append(1, "installing...\n", LineKind::Install);
        let (seq, raw, full, partial, kind) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq, raw.as_str(), full.as_str(), partial, kind),
            (1, "installing...\n", "installing...", false, LineKind::Install)
        );
        assert_eq!(kind.event_name(), "install");
        assert_eq!(LineKind::Runtime.event_name(), "console");
    }

    #[tokio::test]
    async fn kind_switch_seals_pending_partial() {
        let hub = test_hub();
        // A runtime partial is left open; an install chunk must NOT merge into
        // it. The partial seals as its own line and install text starts fresh.
        hub.append(1, "run", LineKind::Runtime); // partial seq 1
        hub.append(1, "inst", LineKind::Install); // seals seq 1, opens seq 2 partial
        hub.append(1, "all\n", LineKind::Install); // completes seq 2
        let (lines, truncated, partial, _) = hub.snapshot(1, 0);
        assert!(!truncated);
        assert!(!partial);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].0, lines[0].1.text.as_str(), lines[0].1.kind), (1, "run", LineKind::Runtime));
        assert_eq!((lines[1].0, lines[1].1.text.as_str(), lines[1].1.kind), (2, "install", LineKind::Install));
    }

    #[tokio::test]
    async fn partial_line_is_capped_and_stops_growing() {
        let hub = test_hub();
        // A writer that never emits '\n' must not grow the ring unboundedly:
        // the line is cut at PARTIAL_CAP with a visible marker and later
        // continuation bytes are dropped.
        hub.append(1, &"x".repeat(PARTIAL_CAP + 100), LineKind::Runtime);
        hub.append(1, "more", LineKind::Runtime); // dropped: already truncated
        hub.append(1, "end\n", LineKind::Runtime); // completes the capped partial
        let (lines, truncated) = hub.history(1, 0);
        assert!(!truncated);
        assert_eq!(lines.len(), 1);
        let text = &lines[0].1.text;
        assert_eq!(text.len(), PARTIAL_CAP + PARTIAL_TRUNC_MARKER.len());
        assert!(text.ends_with(PARTIAL_TRUNC_MARKER));
        // The broadcast `full` payload stays bounded too.
        let mut rx = hub.subscribe(1).unwrap();
        hub.append(1, &"y".repeat(PARTIAL_CAP * 2), LineKind::Runtime);
        let (_, _, full, _, _) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(full.len(), PARTIAL_CAP + PARTIAL_TRUNC_MARKER.len());
    }

    #[tokio::test]
    async fn log_rotates_at_size_keeping_two_backups() {
        let hub = test_hub();
        // One oversized chunk pushes past the cap and rotates (the fresh log
        // then holds the whole chunk); the next chunk rotates again, leaving
        // console.log.1 and console.log.2 behind.
        let big = "x".repeat(LOG_ROTATE_BYTES as usize + 1);
        hub.append(1, &big, LineKind::Runtime);
        hub.append(1, "tail\n", LineKind::Runtime);
        hub.flush_log();
        let dir = hub.config.paths.logs_dir.join("server_1");
        assert_eq!(
            std::fs::read_to_string(dir.join("console.log")).unwrap(),
            "tail\n"
        );
        assert!(dir.join("console.log.1").exists());
        assert!(dir.join("console.log.2").exists());
    }
    #[test]
    fn sanitize_log_text_strips_control_sequences() {
        // C0 controls and ESC sequences must not reach the persisted log; a
        // log tailer's terminal cannot be driven by server output.
        let clean = sanitize_log_text("ok\x07bell\x1b[31mred\x1b[0m\x1b]0;title\x07t\x00x\x7f\n");
        assert_eq!(clean, "okbellredtx\n");
        // \t \n \r are content, not injection.
        assert_eq!(sanitize_log_text("a\tb\r\nc"), "a\tb\r\nc");
        // A lone trailing ESC (split sequence across chunks) leaves only the
        // inert tail behind.
        assert_eq!(sanitize_log_text("line\x1b[31"), "line");
        assert_eq!(sanitize_log_text("col"), "col");
    }

    #[test]
    fn snapshot_after_seq_max_does_not_overflow() {
        // ?since=u64::MAX used to wrap `after_seq + 1` to 0 (debug panic /
        // spurious truncation); the saturating add must keep it a no-op.
        let hub = test_hub();
        hub.append(1, "a\nb\n", LineKind::Runtime);
        let (lines, truncated, partial, _) = hub.snapshot(1, u64::MAX);
        assert!(lines.is_empty());
        assert!(!truncated);
        assert!(!partial);
    }

    #[tokio::test]
    async fn broadcast_raw_is_capped_with_marker() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        // A single multi-MiB complete line must not be cloned raw into the
        // broadcast; it is cut at PARTIAL_CAP with the ring's marker.
        hub.append(1, &"x".repeat(PARTIAL_CAP * 2), LineKind::Runtime);
        let (_, raw, _, _, _) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(raw.len(), PARTIAL_CAP + PARTIAL_TRUNC_MARKER.len());
        assert!(raw.ends_with(PARTIAL_TRUNC_MARKER));
    }

    #[tokio::test]
    async fn subscribe_caps_live_streams_per_server() {
        let hub = test_hub();
        let mut keep = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS_PER_SERVER {
            keep.push(hub.subscribe(1).unwrap());
        }
        assert!(matches!(hub.subscribe(1), Err(TooManySubscribers)));
        drop(keep);
        // Dropped receivers free a slot again.
        assert!(hub.subscribe(1).is_ok());
    }

    #[tokio::test]
    async fn clear_broadcasts_rebuild_marker() {
        let hub = test_hub();
        let mut rx = hub.subscribe(1).unwrap();
        hub.append(1, "a\n", LineKind::Runtime);
        // The append's own event arrives first...
        let (seq, raw, _, _, _) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!((seq, raw.as_str()), (1, "a\n"));
        hub.clear(1);
        // ...then the rebuild sentinel.
        let (seq, raw, _, _, _) = recv_timeout(&mut rx).await.unwrap();
        assert_eq!(
            (seq, raw.as_str()),
            (0, ""),
            "clear must signal live subscribers with the rebuild sentinel"
        );
        // Real chunks can never collide with the sentinel.
        hub.append(1, "b\n", LineKind::Runtime);
        let (seq, raw, _, _, _) = recv_timeout(&mut rx).await.unwrap();
        assert_ne!(seq, 0);
        assert!(!raw.is_empty());
    }
}