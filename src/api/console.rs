//! Console endpoints: history, SSE live stream, clear, send command.
use super::{ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::models::{self, User};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive};
use axum::response::Sse;
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn history(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let server = access_ok(&state, &u, id)?;
    super::require_capability(&state, &u, id, Capability::ConsoleRead)?;
    let lines = if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        state
            .node_client
            .console(&node, &server.uuid, 0)
            .await?
            .lines
    } else {
        state
            .hub
            .history(id, 0)
            .0
            .into_iter()
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
    };
    Ok(Json(serde_json::json!({ "data": lines })))
}

pub async fn clear(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let server = access_ok(&state, &u, id)?;
    super::require_capability(&state, &u, id, Capability::ConsoleWrite)?;
    if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        state.node_client.clear_console(&node, &server.uuid).await?;
    } else {
        state.hub.clear(id);
    }
    Ok(ok(serde_json::json!({"ok":true,"node":server.node})))
}

/// Live console stream over SSE. Replays buffered lines after the client's
/// `Last-Event-ID` (or `?since=`), then live lines. A truncated replay is
/// announced with a `truncated` event so the client can rebuild its view.
pub async fn stream(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Query(q): Query<StreamQuery>,
) -> ApiResult<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>> {
    let server = access_ok(&state, &u, id)?;
    super::require_capability(&state, &u, id, Capability::ConsoleRead)?;
    if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        let client = state.node_client.clone();
        let uuid = server.uuid.clone();
        let remote = async_stream::stream! {
            let mut cursor = 0u64;
            loop {
                match client.console(&node, &uuid, cursor).await {
                    Ok(snapshot) => {
                        cursor = snapshot.cursor;
                        for line in snapshot.lines { yield Ok(Event::default().event("console").data(line)); }
                    }
                    Err(e) => yield Ok(Event::default().event("error").data(e.to_string())),
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        };
        let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
            Box::pin(remote);
        return Ok(Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))));
    }
    // Last-Event-ID wins over ?since=; both are optional (fresh client starts at 0).
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            q.since
                .as_deref()
                .and_then(|s| s.trim().parse::<u64>().ok())
        });
    let after_seq = resume.unwrap_or(0);
    // Subscribe before snapshotting so lines arriving in between are not lost.
    let mut rx = state.hub.subscribe(id);
    let (hist, truncated) = state.hub.history(id, after_seq);
    // Live events whose seq the replay already covered are skipped (no dupes).
    let last_replayed = hist.last().map(|(s, _)| *s).unwrap_or(after_seq);
    let local = async_stream::stream! {
        if truncated && resume.is_some() {
            yield Ok(Event::default().event("truncated").data("1"));
        }
        for (seq, line) in hist {
            yield Ok(Event::default().event("console").id(seq.to_string()).data(line));
        }
        loop {
            match rx.recv().await {
                Ok((seq, text)) if seq > last_replayed => {
                    yield Ok(Event::default().event("console").id(seq.to_string()).data(text));
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(local);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
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

pub async fn send_command(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CommandReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_capability(&state, &u, id, Capability::ConsoleWrite)?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .command(&node, &s.uuid, &req.command)
            .await?;
    } else {
        state
            .procs
            .send_input(s.id, &format!("{}\n", req.command))?;
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// Download the console log file for a server.
pub async fn log_download(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<axum::response::Response> {
    let server = access_ok(&state, &u, id)?;
    super::require_capability(&state, &u, id, Capability::ConsoleRead)?;
    let content = if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        state
            .node_client
            .console(&node, &server.uuid, 0)
            .await?
            .lines
            .join("")
    } else {
        std::fs::read_to_string(
            state
                .cfg
                .paths
                .logs_dir
                .join(format!("server_{id}/console.log")),
        )
        .unwrap_or_default()
    };
    Ok(axum::response::Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(axum::body::Body::from(content))
        .unwrap())
}
