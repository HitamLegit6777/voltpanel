//! API key endpoints (bearer-token style access keys).
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::auth;
use crate::models;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let keys = models::list_api_keys(&state.db, u.id)?;
    let out: Vec<serde_json::Value> = keys
        .into_iter()
        .map(|k| json!({ "id": k.id, "name": k.name, "created_at": k.created_at, "last_used": k.last_used, "scopes": k.scopes }))
        .collect();
    Ok(data(json!(out)))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub scopes: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let raw = format!("vp_{}", auth::random_token(32));
    let scopes = req.scopes.unwrap_or_else(|| "full".into());
    // store only the hash; the raw token is shown once to the creator
    let id = models::create_api_key(&state.db, u.id, &auth::hash_token(&raw), &req.name, &scopes)?;
    Ok(Json(
        json!({ "id": id, "token": raw, "name": req.name, "scopes": scopes }),
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let owner: Option<i64> = conn
        .query_row("SELECT user_id FROM api_keys WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .ok();
    drop(conn);
    if owner != Some(u.id) && !u.root_admin {
        return Err(ApiError::forbidden("not your key"));
    }
    models::delete_api_key(&state.db, id)?;
    Ok(ok(json!({ "ok": true })))
}
