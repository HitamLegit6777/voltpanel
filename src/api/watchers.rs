//! Console watcher endpoints: per-server log-pattern rules that fire an
//! action (notify | restart | stop | command) when a console line matches.
//!
//! Watchers live under the `console` capability family (read/write) because
//! they operate purely on console output — no new capability or feature flag.
//! CRUD mutations bump the watcher engine's per-server version so the hot-path
//! evaluator lazily recompiles its pattern set on the next console line.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, ConsoleWatcher, User};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

/// Actions a watcher may take when its pattern matches a console line.
const VALID_ACTIONS: &[&str] = &["notify", "restart", "stop", "command"];
/// Notify levels accepted in `action_payload` when action == "notify".
const NOTIFY_LEVELS: &[&str] = &["info", "warn", "error"];
/// Upper bound on compiled-pattern length to keep the regex engine bounded.
const MAX_PATTERN_LEN: usize = 512;
/// Upper bound on watcher name length.
const MAX_NAME_LEN: usize = 120;

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

/// Validate an action + payload pair, returning a normalized payload.
///
/// - `notify`: payload is the level; empty defaults to "info", otherwise it
///   must be one of `NOTIFY_LEVELS`.
/// - `command`: payload is the stdin text; it must be non-empty (a blank
///   command would silently no-op on match).
/// - `restart` / `stop`: payload is ignored and normalized to empty.
fn normalize_action(action: &str, payload: &str) -> ApiResult<String> {
    if !VALID_ACTIONS.contains(&action) {
        return Err(ApiError::bad_request(format!(
            "unsupported watcher action: {action}"
        )));
    }
    match action {
        "notify" => {
            let level = payload.trim();
            if level.is_empty() {
                Ok("info".to_string())
            } else if NOTIFY_LEVELS.contains(&level) {
                Ok(level.to_string())
            } else {
                Err(ApiError::bad_request(
                    "notify level must be one of: info, warn, error",
                ))
            }
        }
        "command" => {
            if payload.trim().is_empty() {
                return Err(ApiError::bad_request(
                    "command action requires a non-empty payload",
                ));
            }
            Ok(payload.to_string())
        }
        // restart | stop take no payload.
        _ => Ok(String::new()),
    }
}

/// Shared shape validation for name/pattern applied on create and update.
fn validate_shape(name: &str, pattern: &str, is_regex: bool) -> ApiResult<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("watcher name is required"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(ApiError::bad_request(format!(
            "watcher name must be <= {MAX_NAME_LEN} characters"
        )));
    }
    if pattern.is_empty() {
        return Err(ApiError::bad_request("watcher pattern is required"));
    }
    if pattern.len() > MAX_PATTERN_LEN {
        return Err(ApiError::bad_request(format!(
            "watcher pattern must be <= {MAX_PATTERN_LEN} characters"
        )));
    }
    // Reject an invalid regex up front so a match on the hot path can never
    // fail to compile (the evaluator would otherwise silently skip it).
    if is_regex {
        regex::Regex::new(pattern)
            .map_err(|e| ApiError::bad_request(format!("invalid regex: {e}")))?;
    }
    Ok(())
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ConsoleRead).await?;
    let watchers = blocking(state.db.clone(), move |db| {
        models::list_watchers(&db, server_id)
    })
    .await?;
    Ok(data(serde_json::to_value(watchers)?))
}

#[derive(Deserialize)]
pub struct CreateWatcherReq {
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub is_regex: bool,
    pub action: String,
    #[serde(default)]
    pub action_payload: String,
    #[serde(default)]
    pub cooldown_secs: Option<i64>,
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateWatcherReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ConsoleWrite).await?;
    validate_shape(&req.name, &req.pattern, req.is_regex)?;
    let payload = normalize_action(&req.action, &req.action_payload)?;
    let cooldown = req.cooldown_secs.unwrap_or(0);
    if cooldown < 0 {
        return Err(ApiError::bad_request("cooldown_secs must be >= 0"));
    }
    let name = req.name.trim().to_string();
    let pattern = req.pattern.clone();
    let is_regex = req.is_regex;
    let action = req.action.clone();
    let id = blocking(state.db.clone(), move |db| {
        models::create_watcher(
            &db, server_id, &name, &pattern, is_regex, &action, &payload, cooldown,
        )
    })
    .await?;
    // Recompile the server's watcher set on the next console line.
    state.watcher_engine.invalidate(server_id);
    let created = blocking(state.db.clone(), move |db| models::get_watcher(&db, id)).await?;
    Ok(data(serde_json::to_value(created)?))
}

#[derive(Deserialize)]
pub struct UpdateWatcherReq {
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub is_regex: bool,
    pub action: String,
    #[serde(default)]
    pub action_payload: String,
    pub enabled: bool,
    #[serde(default)]
    pub cooldown_secs: Option<i64>,
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, watcher_id)): Path<(i64, i64)>,
    Json(req): Json<UpdateWatcherReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ConsoleWrite).await?;
    validate_shape(&req.name, &req.pattern, req.is_regex)?;
    let payload = normalize_action(&req.action, &req.action_payload)?;
    let cooldown = req.cooldown_secs.unwrap_or(0);
    if cooldown < 0 {
        return Err(ApiError::bad_request("cooldown_secs must be >= 0"));
    }
    // Confirm the watcher exists and belongs to this server before writing.
    let existing = blocking(state.db.clone(), move |db| models::get_watcher(&db, watcher_id))
        .await
        .map_err(|_| ApiError::not_found("watcher not found"))?;
    if existing.server_id != server_id {
        return Err(ApiError::not_found("watcher not found"));
    }
    let updated = ConsoleWatcher {
        id: watcher_id,
        server_id,
        name: req.name.trim().to_string(),
        pattern: req.pattern.clone(),
        is_regex: req.is_regex,
        action: req.action.clone(),
        action_payload: payload,
        enabled: req.enabled,
        cooldown_secs: cooldown,
        last_fired_at: existing.last_fired_at,
        trigger_count: existing.trigger_count,
        created_at: existing.created_at,
    };
    let to_write = updated.clone();
    blocking(state.db.clone(), move |db| {
        models::update_watcher(&db, &to_write)
    })
    .await?;
    state.watcher_engine.invalidate(server_id);
    Ok(data(serde_json::to_value(updated)?))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, watcher_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ConsoleWrite).await?;
    let existing = blocking(state.db.clone(), move |db| models::get_watcher(&db, watcher_id))
        .await
        .map_err(|_| ApiError::not_found("watcher not found"))?;
    if existing.server_id != server_id {
        return Err(ApiError::not_found("watcher not found"));
    }
    blocking(state.db.clone(), move |db| {
        models::delete_watcher(&db, watcher_id)
    })
    .await?;
    state.watcher_engine.invalidate(server_id);
    Ok(ok(serde_json::json!({ "deleted": watcher_id })))
}
