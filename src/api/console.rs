//! Console endpoints: history, SSE live stream, clear, send command.
use super::{ok, ApiError, ApiResult, AppState, AuthUser};
use crate::models::{self, User};
use axum::extract::{Path, State};
use axum::Json;
use axum::response::sse::{Event, KeepAlive};
use axum::response::Sse;
use futures::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id).map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn history(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let server = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"console.read")?;
    let lines = if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        state.node_client.console(&node, &server.uuid, 0).await?.lines
    } else { state.hub.history(id) };
    Ok(Json(serde_json::json!({ "data": lines })))
}

pub async fn clear(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, id)?;
    state.hub.clear(id);
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// Live console stream over SSE. Emits the full buffer first, then live lines.
pub async fn stream(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>> {
    let server = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"console.read")?;
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
        let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(remote);
        return Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))));
    }
    let rx = state.hub.subscribe(id);
    let hist = state.hub.history(id);
    let local = async_stream::stream! {
        for line in hist { yield Ok(Event::default().event("console").data(line)); }
        let mut rx = rx;
        while let Some(line) = rx.recv().await { yield Ok(Event::default().event("console").data(line)); }
    };
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(local);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15))))
}

#[derive(Deserialize)]
pub struct CommandReq {
    pub command: String,
}

pub async fn send_command(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<CommandReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"console.write")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        state.node_client.command(&node, &s.uuid, &req.command).await?;
    } else { state.procs.send_input(s.id, &format!("{}\n", req.command))?; }
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// Download the console log file for a server.
pub async fn log_download(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<axum::response::Response> {
    access_ok(&state, &u, id)?;
    let path = state.cfg.paths.logs_dir.join(format!("server_{id}/console.log"));
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(axum::response::Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(axum::body::Body::from(content))
        .unwrap())
}

