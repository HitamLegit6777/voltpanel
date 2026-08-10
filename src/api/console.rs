//! Console endpoints: history, SSE live stream, clear, send command, crash state.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, User};
use crate::services::console::{self, ConsoleEntry, LineKind};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive};
use axum::response::Sse;
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

// ---- DB execution off the async worker ----
//
// Pool-based `models` calls must not run on a Tokio worker thread;
// `blocking(...)` runs them on Tokio's blocking pool (see src/api/servers.rs
// for the full contract). The console hub itself is in-memory; this module
// owns no direct SQL, so `Db::call` is unused here.

async fn access_ok(
    state: &AppState,
    user: &User,
    server_id: i64,
) -> ApiResult<crate::models::Server> {
    let s = blocking(state.db.clone(), move |db| {
        models::get_server(&db, server_id)
    })
    .await
    .map_err(|_| ApiError::not_found("server not found"))?;

    let user = user.clone();
    let sid = s.id;
    if !blocking(state.db.clone(), move |db| {
        models::user_has_server_access(&db, &user, sid)
    })
    .await?
    {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn history(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    let after_seq = q
        .since
        .as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if server.node != "local" {
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let snap = state
            .node_client
            .console(&node, &server.uuid, after_seq)
            .await?;
        return Ok(Json(serde_json::json!({
            "data": snap.lines,
            "cursor": snap.cursor,
            "truncated": false,
            "partial": false,
        })));
    }
    // Serialize the full replay state: lines after `since` with their stream
    // kind, the resume cursor (last completed id — it stays behind a pending
    // partial), and whether the ring evicted ids the caller still needed
    // (rebuild) or ends in a partial.
    let (lines, truncated, partial, last_ok) = state.hub.snapshot(id, after_seq);
    Ok(Json(serde_json::json!({
        "data": lines.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
        "cursor": last_ok,
        "truncated": truncated,
        "partial": partial,
    })))
}

pub async fn clear(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleWrite).await?;
    if server.node != "local" {
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        state.node_client.clear_console(&node, &server.uuid).await?;
    } else {
        state.hub.clear(id);
    }
    Ok(ok(serde_json::json!({"ok":true,"node":server.node})))
}

/// Resume cursor for a stream connection: `Last-Event-ID` wins over `?since=`;
/// both are optional (a fresh client resumes from 0). Shared by the local and
/// remote (node) stream paths so both honor the same resume contract.
fn resume_cursor(headers: &HeaderMap, q: &StreamQuery) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            q.since
                .as_deref()
                .and_then(|s| s.trim().parse::<u64>().ok())
        })
}

/// Classify a node-client error from the remote console poll. Auth failures
/// (401/403) are permanent — a session that can never authenticate will not
/// start succeeding — so the stream terminates. Everything else (transport and
/// server-side failures) is transient and retried with backoff.
fn node_console_error_is_permanent(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<crate::services::node::NodeClientError>()
            .and_then(|e| e.status),
        Some(401 | 403)
    )
}

/// Complete once graceful shutdown begins: the process-wide `running` flag
/// clears, polled every 100 ms. Long-lived SSE console streams must not hold
/// axum's drain open, so when this completes the stream simply ends — no
/// error is sent and the client's reconnect logic restores the view.
async fn wait_shutdown(running: &Arc<AtomicBool>) {
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
/// Live console stream over SSE. Replays buffered lines after the client's
/// `Last-Event-ID` (or `?since=`), then live lines. A truncated replay is
/// announced with a `truncated` event so the client can rebuild its view;
/// if the live channel ever lags, the gap is resynced from the buffer the
/// same way, so no line is permanently lost. A pending partial replays under
/// the last completed line's id (the event carries no id, so the client's
/// Last-Event-ID stays put) and its completion — carrying the partial's own
/// seq — is delivered, never skipped as a duplicate.
pub async fn stream(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    // Last-Event-ID wins over ?since=; both are optional (fresh client starts at 0).
    let resume = resume_cursor(&headers, &q);
    let after_seq = resume.unwrap_or(0);
    let running = state.running.clone();
    if server.node != "local" {
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let client = state.node_client.clone();
        let uuid = server.uuid.clone();
        let remote = async_stream::stream! {
            let mut cursor = after_seq;
            // Transient failures are retried with exponential backoff (500 ms
            // up to 30 s); a permanent auth failure ends the stream instead of
            // polling a dead session forever.
            let mut retry = std::time::Duration::from_millis(500);
            loop {
                let poll = tokio::select! {
                    poll = client.console(&node, &uuid, cursor) => poll,
                    // Graceful shutdown: end the stream so axum's drain
                    // completes; the client reconnects on the closed SSE.
                    _ = wait_shutdown(&running) => break,
                };
                match poll {
                    Ok(snapshot) => {
                        retry = std::time::Duration::from_millis(500);
                        if snapshot.cursor < cursor {
                            // The remote ring was reset (clear/restart on the
                            // node): never rewind — a backwards cursor would
                            // re-yield lines the client already rendered — and
                            // tell the client to rebuild its view instead.
                            yield Ok(Event::default().event("truncated").data("1"));
                        }
                        cursor = cursor.max(snapshot.cursor);
                        for line in snapshot.lines {
                            yield Ok(Event::default().event("console").data(line));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => {
                        yield Ok(Event::default().event("error").data(e.to_string()));
                        if node_console_error_is_permanent(&e) {
                            break;
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(retry) => {}
                            // The retry backoff can reach 30 s; do not let a
                            // node that is merely down hold the drain open.
                            _ = wait_shutdown(&running) => break,
                        }
                        retry = (retry * 2).min(std::time::Duration::from_secs(30));
                    }
                }
            }
        };
        let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
            Box::pin(remote);
        return Ok(Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))));
    }
    // Subscribe before snapshotting so lines arriving in between are not lost.
    // The per-server live-stream cap (each append is copied to every
    // subscriber) is enforced here: 429 once the limit is reached.
    let mut rx = state.hub.subscribe(id).map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many live console streams open for this server",
        )
    })?;
    let hub = state.hub.clone();
    let (hist, truncated, partial_last, _) = hub.snapshot(id, after_seq);
    let mut client = ConsoleClient::new(after_seq);
    let replay = client.replay(&hist, partial_last);
    let local = async_stream::stream! {
        if truncated && resume.is_some() {
            yield Ok(Event::default().event("truncated").data("1"));
        }
        for (id, line, kind) in replay {
            let mut ev = Event::default().event(kind.event_name()).data(line);
            if let Some(id) = id {
                ev = ev.id(id.to_string());
            }
            yield Ok(ev);
        }
        loop {
            let recv = tokio::select! {
                recv = rx.recv() => recv,
                // Graceful shutdown: end the stream so axum's drain completes;
                // the client reconnects on the closed SSE.
                _ = wait_shutdown(&running) => break,
            };
            match recv {
                Ok((0, raw, _, _, _)) if raw.is_empty() => {
                    // Rebuild sentinel (see ConsoleHub::clear): the console
                    // was cleared, so the client must drop its view and the
                    // in-flight partial it was rendering is gone too.
                    client.reset();
                    yield Ok(Event::default().event("truncated").data("1"));
                }
                Ok((seq, raw, full, ends_partial, kind)) => {
                    if let Some((id, data, kind)) =
                        client.on_chunk(seq, &raw, &full, ends_partial, kind)
                    {
                        let mut ev = Event::default().event(kind.event_name()).data(data);
                        if let Some(id) = id {
                            ev = ev.id(id.to_string());
                        }
                        yield Ok(ev);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Broadcast dropped events we never received; resync from
                    // the ring so the gap is replayed instead of lost forever.
                    let (hist, truncated, partial_last, _) = hub.snapshot(id, client.cursor);
                    if truncated {
                        yield Ok(Event::default().event("truncated").data("1"));
                    }
                    for (id, line, kind) in client.replay(&hist, partial_last) {
                        let mut ev = Event::default().event(kind.event_name()).data(line);
                        if let Some(id) = id {
                            ev = ev.id(id.to_string());
                        }
                        yield Ok(ev);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(local);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}

/// Delivery state for one console SSE client: the id of the last completed
/// line (the resume cursor) plus the in-flight partial line being rendered.
///
/// Serialization contract:
/// - Completed lines carry their own seq as the SSE id and their text with a
///   trailing '\n', byte-identical to the live chunks they were delivered from.
/// - A pending partial is delivered without an id: the client's Last-Event-ID
///   stays at the last completed line, so a reconnect replays the partial
///   under the last completed id and its completion (own seq) still arrives.
/// - Live chunks whose effect is already inside a replayed line are detected
///   via the ring's merged line text and suppressed, so no text is duplicated.
struct ConsoleClient {
    /// Id of the last COMPLETE line delivered this connection.
    cursor: u64,
    /// Own seq of the partial line whose text was (partly) rendered.
    pending: Option<u64>,
    /// Merged ring text of that partial line as last rendered.
    prefix: String,
}

impl ConsoleClient {
    fn new(after_seq: u64) -> Self {
        Self {
            cursor: after_seq,
            pending: None,
            prefix: String::new(),
        }
    }

    /// Drop the in-flight partial state after a clear. The cursor is kept:
    /// seqs stay monotonic across clears, so the next real chunk (seq >
    /// cursor) is delivered normally and a pre-clear partial can never be
    /// mistaken for a post-clear line.
    fn reset(&mut self) {
        self.pending = None;
        self.prefix.clear();
    }

    /// Serialize ring lines into events and advance the cursor. The trailing
    /// partial replays without an id; a line the client is already rendering
    /// (mid-lag resync) is reduced to the missing delta so nothing repeats.
    /// Each event carries its line's stream kind so the SSE event name can
    /// distinguish install output from runtime output.
    fn replay(
        &mut self,
        hist: &[(u64, ConsoleEntry)],
        partial_last: bool,
    ) -> Vec<(Option<u64>, String, LineKind)> {
        let mut events = Vec::with_capacity(hist.len());
        for (i, (seq, entry)) in hist.iter().enumerate() {
            if partial_last && i + 1 == hist.len() {
                if self.pending == Some(*seq) {
                    let delta = entry.text.strip_prefix(&self.prefix).unwrap_or(&entry.text);
                    if !delta.is_empty() {
                        events.push((None, delta.to_string(), entry.kind));
                    }
                } else {
                    events.push((None, entry.text.clone(), entry.kind));
                }
                self.pending = Some(*seq);
                self.prefix = entry.text.clone();
            } else {
                let data = if self.pending == Some(*seq) {
                    // The client lagged while this line completed; deliver only
                    // the tail that was not rendered before the gap.
                    let delta = entry.text.strip_prefix(&self.prefix).unwrap_or(&entry.text);
                    format!("{delta}\n")
                } else {
                    format!("{}\n", entry.text)
                };
                self.cursor = *seq;
                self.pending = None;
                self.prefix.clear();
                events.push((Some(*seq), data, entry.kind));
            }
        }
        events
    }

    /// Apply one live broadcast chunk; `None` skips a duplicate. A partial
    /// chunk is delivered without advancing the cursor (the line is still in
    /// flight); a completion chunk advances it to its own seq.
    fn on_chunk(
        &mut self,
        seq: u64,
        raw: &str,
        full: &str,
        ends_partial: bool,
        kind: LineKind,
    ) -> Option<(Option<u64>, String, LineKind)> {
        if ends_partial {
            if self.pending != Some(seq) {
                self.pending = Some(seq);
            } else if full == self.prefix {
                // Chunk already merged into the line this client rendered.
                return None;
            }
            self.prefix = full.to_string();
            Some((None, raw.to_string(), kind))
        } else if seq > self.cursor {
            self.cursor = seq;
            self.pending = None;
            self.prefix.clear();
            Some((Some(seq), raw.to_string(), kind))
        } else {
            None
        }
    }
}

#[derive(Deserialize)]
pub struct StreamQuery {
    /// Optional resume point (?since=123); the Last-Event-ID header takes priority.
    since: Option<String>,
}

#[derive(Deserialize)]
pub struct CommandReq {
    pub command: String,
}

/// Console commands are short interactive inputs; anything longer is rejected
/// (and would otherwise flood a child that never drains its stdin).
const COMMAND_MAX_BYTES: usize = 4096;

pub async fn send_command(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CommandReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleWrite).await?;
    // Bound the command before anything touches the child: a >4 KiB "command"
    // is a mistake, not a legitimate interactive input, and capping it also
    // bounds the child-stdin write.
    if req.command.len() > COMMAND_MAX_BYTES {
        return Err(ApiError::bad_request(format!(
            "command too long (max {COMMAND_MAX_BYTES} bytes)"
        )));
    }
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        state
            .node_client
            .command(&node, &s.uuid, &req.command)
            .await?;
    } else {
        // The write goes through a dedicated per-server stdin writer (one
        // thread, bounded queue, per-command oneshot ack): a child that never
        // drains its pipe can only wedge that one writer thread — it can no
        // longer pin a blocking-pool thread per request or hold the stdin
        // mutex hostage, and every later command fails fast (timeout or busy)
        // instead of queuing doomed writes.
        let cmd = format!("{}\n", req.command);
        match state.hub.write_stdin(s.id, state.procs.clone(), cmd).await {
            Ok(()) => {}
            Err(console::StdinError::Busy) => {
                return Err(ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "console input backlog full; the server process may not be draining stdin",
                ));
            }
            Err(console::StdinError::TimedOut) => {
                return Err(ApiError::new(
                    axum::http::StatusCode::REQUEST_TIMEOUT,
                    "console command write timed out",
                ));
            }
            Err(console::StdinError::WriteFailed(e)) => {
                return Err(ApiError::bad_request(format!(
                    "console command failed: {e}"
                )));
            }
        }
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// A console log larger than this is served tail-first (the last
/// `MAX_DOWNLOAD_BYTES`), so the response never buffers more than the ceiling.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// Wrap a body as a plain-text download response.
fn text_response(body: axum::body::Body) -> axum::response::Response {
    axum::response::Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(body)
        .unwrap()
}

/// Download the console log file for a server.
pub async fn log_download(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<axum::response::Response> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    if server.node != "local" {
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let content = state
            .node_client
            .console(&node, &server.uuid, 0)
            .await?
            .lines
            .join("\n");
        return Ok(text_response(axum::body::Body::from(content)));
    }
    // Stream the log straight from disk: `read_to_string` would pull the whole
    // file into RAM, and a log that outgrew its rotation ceiling could OOM the
    // panel. A log past the ceiling is served tail-first, so the response
    // stays bounded at MAX_DOWNLOAD_BYTES.
    use tokio::io::AsyncSeekExt;
    let path = state
        .cfg
        .paths
        .logs_dir
        .join(format!("server_{id}/console.log"));
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(text_response(axum::body::Body::from(String::new())));
        }
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await.map_err(ApiError::from)?.len();
    if len > MAX_DOWNLOAD_BYTES {
        file.seek(std::io::SeekFrom::Start(len - MAX_DOWNLOAD_BYTES))
            .await
            .map_err(ApiError::from)?;
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024);
    Ok(text_response(axum::body::Body::from_stream(stream)))
}

// ---------------- Crash state & policy (G8) ----------------

/// Current crash state and policy for a server, surfaced by `crash_info`:
/// the last exit classification, the live burst budget, and the operator
/// policy knobs that control restarts.
pub async fn crash_info(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    Ok(data(serde_json::json!({
        "status": s.status,
        "auto_restart": s.auto_restart,
        "detect_clean_exit_as_crash": s.crash_detect_clean_exit,
        "restart_budget": s.crash_restart_budget,
        "restarts_in_burst": s.crash_restarts,
        "burst_since": s.crash_window_start,
        "reason": s.crash_reason,
    })))
}

#[derive(Deserialize)]
pub struct CrashPolicyReq {
    /// Mirrors the `detect-clean-exit-as-crash` toggle: treat an unrequested clean
    /// exit as a crash (restart + crashed state) instead of a normal stop.
    pub detect_clean_exit_as_crash: Option<bool>,
    /// Max auto-restarts per crash burst (0 disables crash restarts).
    pub restart_budget: Option<i64>,
    /// Clear the current burst (consumed slots, window, recorded reason).
    pub reset_burst: Option<bool>,
}

/// Update a server's crash policy. Mutating the policy is a control-plane
/// action on the workload, so it rides `ControlRestart` (not the read-only
/// `ConsoleRead` used by the state endpoint).
pub async fn crash_policy(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CrashPolicyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let mut s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ControlRestart).await?;
    if let Some(v) = req.detect_clean_exit_as_crash {
        s.crash_detect_clean_exit = v;
    }
    if let Some(b) = req.restart_budget {
        if !(0..=20).contains(&b) {
            return Err(ApiError::bad_request(
                "restart budget must be between 0 and 20",
            ));
        }
        s.crash_restart_budget = b;
    }
    let sid = s.id;
    blocking(state.db.clone(), move |db| models::update_server(&db, &s)).await?;
    if req.reset_burst == Some(true) {
        blocking(state.db.clone(), move |db| {
            models::reset_crash_window(&db, sid)
        })
        .await?;
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            text: text.to_string(),
            kind: LineKind::Runtime,
        }
    }
    fn inst(text: &str) -> ConsoleEntry {
        ConsoleEntry {
            text: text.to_string(),
            kind: LineKind::Install,
        }
    }

    #[test]
    fn replay_serializes_complete_lines_and_idless_partial() {
        let hist = vec![(1, rt("a")), (2, rt("hello"))];
        let mut c = ConsoleClient::new(0);
        let ev = c.replay(&hist, true);
        assert_eq!(
            ev,
            vec![
                (Some(1), "a\n".to_string(), LineKind::Runtime),
                (None, "hello".to_string(), LineKind::Runtime)
            ]
        );
        assert_eq!(c.cursor, 1, "cursor stays at the last completed id");
        assert_eq!(c.pending, Some(2));
        assert_eq!(c.prefix, "hello");
    }

    #[test]
    fn replay_ring_without_partial_uses_own_seqs() {
        let hist = vec![(1, rt("a")), (2, rt("b"))];
        let mut c = ConsoleClient::new(0);
        let ev = c.replay(&hist, false);
        assert_eq!(
            ev,
            vec![
                (Some(1), "a\n".to_string(), LineKind::Runtime),
                (Some(2), "b\n".to_string(), LineKind::Runtime)
            ]
        );
        assert_eq!(c.cursor, 2);
        assert_eq!(c.pending, None);
    }

    #[test]
    fn live_chunks_deliver_partial_then_completion_without_loss_or_dupe() {
        let mut c = ConsoleClient::new(0);
        assert_eq!(
            c.on_chunk(1, "a\n", "a", false, LineKind::Runtime),
            Some((Some(1), "a\n".to_string(), LineKind::Runtime))
        );
        assert_eq!(
            c.on_chunk(2, "hel", "hel", true, LineKind::Runtime),
            Some((None, "hel".to_string(), LineKind::Runtime))
        );
        // continuation of the same in-flight line
        assert_eq!(
            c.on_chunk(2, "lo", "hello", true, LineKind::Runtime),
            Some((None, "lo".to_string(), LineKind::Runtime))
        );
        // completion carries the partial's own seq and advances the cursor
        assert_eq!(
            c.on_chunk(2, "lo\n", "hello", false, LineKind::Runtime),
            Some((Some(2), "lo\n".to_string(), LineKind::Runtime))
        );
        assert_eq!(c.cursor, 2);
        assert_eq!(c.pending, None);
    }

    #[test]
    fn replayed_partial_suppresses_late_creation_chunk_but_keeps_continuations() {
        // The partial was created between subscribe and snapshot on another
        // thread: the ring replay renders it AND its creation chunk is still
        // in the broadcast buffer. It must not be rendered twice.
        let hist = vec![(2, rt("hel"))];
        let mut c = ConsoleClient::new(1);
        let ev = c.replay(&hist, true);
        assert_eq!(ev, vec![(None, "hel".to_string(), LineKind::Runtime)]);
        assert_eq!(c.cursor, 1);
        assert_eq!(
            c.on_chunk(2, "hel", "hel", true, LineKind::Runtime),
            None,
            "already rendered"
        );
        // a genuine continuation after the replay is new output
        assert_eq!(
            c.on_chunk(2, "lo", "hello", true, LineKind::Runtime),
            Some((None, "lo".to_string(), LineKind::Runtime))
        );
        assert_eq!(
            c.on_chunk(2, "\n", "hello", false, LineKind::Runtime),
            Some((Some(2), "\n".to_string(), LineKind::Runtime))
        );
    }

    #[test]
    fn lag_resync_merges_completed_pending_line_as_delta() {
        let mut c = ConsoleClient::new(1);
        assert_eq!(
            c.on_chunk(2, "hel", "hel", true, LineKind::Runtime),
            Some((None, "hel".to_string(), LineKind::Runtime))
        );
        // The client lags; the line completes on the ring in the meantime.
        let hist = vec![(2, rt("hello"))];
        let ev = c.replay(&hist, false);
        assert_eq!(
            ev,
            vec![(Some(2), "lo\n".to_string(), LineKind::Runtime)],
            "only the missing tail"
        );
        assert_eq!(c.cursor, 2);
        assert_eq!(c.pending, None);
    }

    #[test]
    fn lag_resync_replays_new_lines_after_gap() {
        let hist = vec![(2, rt("b")), (3, rt("c"))];
        let mut c = ConsoleClient::new(1);
        let ev = c.replay(&hist, false);
        assert_eq!(
            ev,
            vec![
                (Some(2), "b\n".to_string(), LineKind::Runtime),
                (Some(3), "c\n".to_string(), LineKind::Runtime)
            ]
        );
        assert_eq!(c.cursor, 3);
    }

    #[test]
    fn ids_are_strictly_increasing_and_never_skipped() {
        let mut c = ConsoleClient::new(0);
        let mut ids = Vec::new();
        for (seq, raw, full, partial) in [
            (1u64, "a\n", "a", false),
            (2, "hel", "hel", true),
            (2, "lo", "hello", true),
            (2, "lo\n", "hello", false),
            (3, "b\n", "b", false),
            (4, "c", "c", true),
            (4, "c\n", "c", false),
        ] {
            if let Some((id, _, _)) = c.on_chunk(seq, raw, full, partial, LineKind::Runtime) {
                ids.extend(id);
            }
        }
        assert_eq!(
            ids,
            vec![1, 2, 3, 4],
            "one id per complete line, none skipped"
        );
    }

    #[test]
    fn resume_cursor_after_clear_still_advances() {
        // clear() keeps next_seq; a pre-clear cursor must not filter out the
        // post-clear line, and replay serializes it under its new seq.
        let hist = vec![(4, rt("d"))];
        let mut c = ConsoleClient::new(3);
        let ev = c.replay(&hist, false);
        assert_eq!(ev, vec![(Some(4), "d\n".to_string(), LineKind::Runtime)]);
        assert_eq!(c.cursor, 4);
    }

    #[test]
    fn install_lines_keep_their_kind_through_replay_and_chunks() {
        // Install output rides its own event kind end to end: replay and live
        // chunks both carry LineKind::Install.
        let hist = vec![(1, inst("fetching deps...")), (2, rt("started"))];
        let mut c = ConsoleClient::new(0);
        let ev = c.replay(&hist, false);
        assert_eq!(
            ev,
            vec![
                (Some(1), "fetching deps...\n".to_string(), LineKind::Install),
                (Some(2), "started\n".to_string(), LineKind::Runtime)
            ]
        );
        assert_eq!(
            c.on_chunk(3, "build\n", "build", false, LineKind::Install),
            Some((Some(3), "build\n".to_string(), LineKind::Install))
        );
        assert_eq!(LineKind::Install.event_name(), "install");
        assert_eq!(LineKind::Runtime.event_name(), "console");
    }
    #[test]
    fn resume_cursor_last_event_id_wins_over_since() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        let q = StreamQuery {
            since: Some("7".to_string()),
        };
        assert_eq!(resume_cursor(&headers, &q), Some(42));
    }

    #[test]
    fn resume_cursor_falls_back_to_since_on_missing_or_invalid_id() {
        let headers = HeaderMap::new();
        let q = StreamQuery {
            since: Some("7".to_string()),
        };
        assert_eq!(
            resume_cursor(&headers, &q),
            Some(7),
            "missing id falls back to since"
        );

        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(
            resume_cursor(&headers, &q),
            Some(7),
            "non-numeric id falls back to since"
        );
    }

    #[test]
    fn resume_cursor_none_for_fresh_client() {
        let headers = HeaderMap::new();
        let q = StreamQuery { since: None };
        assert_eq!(resume_cursor(&headers, &q), None);
        let q2 = StreamQuery {
            since: Some("   ".to_string()),
        };
        assert_eq!(
            resume_cursor(&headers, &q2),
            None,
            "whitespace parses as absent"
        );
    }
}
