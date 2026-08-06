//! API key endpoints (bearer-token style access keys).
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let keys = crate::services::keys::list(&state.db, u.id)?;
    Ok(data(json!(keys)))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub server_ids: Option<Vec<i64>>,
    #[serde(default)]
    pub ttl_days: Option<i64>,
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    for c in req.capabilities.iter().flatten() {
        if c != "*" && Capability::from_str(c).is_err() {
            return Err(ApiError::bad_request(format!("unknown capability: {c}")));
        }
    }
    // the raw token is shown exactly once; the service stores only its hash
    let (id, raw) = crate::services::keys::create(
        &state.db,
        u.id,
        &req.name,
        req.capabilities.as_deref().unwrap_or(&[]),
        req.server_ids.as_deref().unwrap_or(&[]),
        req.ttl_days,
    )?;
    Ok(Json(json!({ "id": id, "token": raw, "name": req.name })))
}

/// Confirm the caller owns the key (or is admin) before revoke/delete.
fn check_owner(state: &AppState, user_id: i64, root_admin: bool, id: i64) -> ApiResult<()> {
    let conn = state.db.lock();
    let owner: Option<i64> = conn
        .query_row("SELECT user_id FROM api_keys WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .ok();
    drop(conn);
    if owner != Some(user_id) && !root_admin {
        return Err(ApiError::forbidden("not your key"));
    }
    Ok(())
}

pub async fn revoke(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    check_owner(&state, u.id, u.root_admin, id)?;
    crate::services::keys::revoke(&state.db, id)?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    check_owner(&state, u.id, u.root_admin, id)?;
    crate::services::keys::delete(&state.db, id)?;
    Ok(ok(json!({ "ok": true })))
}
