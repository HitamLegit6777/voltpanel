//! API key endpoints (bearer-token style access keys).
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let uid = u.id;
    let keys = blocking(state.db.clone(), move |db| {
        crate::services::keys::list(&db, uid)
    })
    .await?;
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
    let caps_vec = req.capabilities.clone().unwrap_or_default();
    let caps: &[String] = &caps_vec;
    if caps.is_empty() {
        return Err(ApiError::bad_request("at least one capability is required"));
    }
    for c in caps {
        if c != "*" && Capability::from_str(c).is_err() {
            return Err(ApiError::bad_request(format!("unknown capability: {c}")));
        }
    }
    if let Some(days) = req.ttl_days {
        if days <= 0 {
            return Err(ApiError::bad_request("ttl_days must be positive"));
        }
        if days > crate::services::keys::MAX_TTL_DAYS {
            return Err(ApiError::bad_request(format!(
                "ttl_days exceeds the {}-day maximum",
                crate::services::keys::MAX_TTL_DAYS
            )));
        }
    }
    let uid = u.id;
    let name = req.name.clone();
    let name2 = name.clone();
    let server_ids = req.server_ids.clone().unwrap_or_default();
    let ttl_days = req.ttl_days;
    let (id, raw) = blocking(state.db.clone(), move |db| {
        crate::services::keys::create(
            &db,
            uid,
            &name,
            caps_vec.as_slice(),
            &server_ids,
            ttl_days,
        )
    })
    .await?;
    Ok(Json(json!({ "id": id, "token": raw, "name": name2 })))
}

async fn check_owner(state: &AppState, user_id: i64, root_admin: bool, id: i64) -> ApiResult<()> {
    let owner: Option<i64> = state
        .db
        .call(move |conn| {
            Ok::<_, anyhow::Error>(
                conn.query_row("SELECT user_id FROM api_keys WHERE id=?1", [id], |r| {
                    r.get::<_, i64>(0)
                })
                .ok(),
            )
        })
        .await?;
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
    check_owner(&state, u.id, u.root_admin, id).await?;
    let kid = id;
    blocking(state.db.clone(), move |db| {
        crate::services::keys::revoke(&db, kid)
    })
    .await?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    check_owner(&state, u.id, u.root_admin, id).await?;
    let kid = id;
    blocking(state.db.clone(), move |db| {
        crate::services::keys::delete(&db, kid)
    })
    .await?;
    Ok(ok(json!({ "ok": true })))
}
