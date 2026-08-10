//! Activity feed endpoints: per-user and per-server audit trails.
//!
//! `GET /api/activity` shows the caller's own actions (no capability needed —
//! an actor may always see their own history). `GET /api/servers/:id/activity`
//! shows one workspace's feed and is gated on `activity.read`.

use super::{data, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, Server, User};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread (see servers.rs
// for the full contract). `blocking` runs the pool-based `models` queries on
// Tokio's blocking pool. Never hold a pooled connection across an `.await` —
// split into separate blocking units instead.

#[derive(Debug, Deserialize)]
pub struct Q {
    pub limit: Option<i64>,
}

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

fn bounded_limit(q: &Q) -> i64 {
    q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

async fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<Server> {
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, server_id))
        .await
        .map_err(|_| ApiError::not_found("server not found"))?;
    let user = user.clone();
    let sid = s.id;
    if !blocking(state.db.clone(), move |db| models::user_has_server_access(&db, &user, sid)).await?
    {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

/// The caller's own activity feed, newest first. No capability required for
/// one's own actions.
pub async fn user_activity(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Query(q): Query<Q>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = bounded_limit(&q);
    let logs = blocking(state.db.clone(), move |db| models::list_user_activity(&db, u.id, limit)).await?;
    Ok(data(serde_json::to_value(logs)?))
}

/// One workspace's activity feed, newest first, gated on `activity.read`.
pub async fn server_activity(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<Q>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ActivityRead).await?;
    let limit = bounded_limit(&q);
    let logs = blocking(state.db.clone(), move |db| models::list_server_activity(&db, id, limit)).await?;
    Ok(data(serde_json::to_value(logs)?))
}