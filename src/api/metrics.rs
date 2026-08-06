//! Telemetry endpoints: time-series queries for a workspace.
use super::{data, require_capability, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Q {
    pub window: Option<String>,
    pub points: Option<usize>,
}

fn window_secs(window: &str) -> Option<i64> {
    match window {
        "1h" => Some(3600),
        "6h" => Some(6 * 3600),
        "24h" => Some(24 * 3600),
        "7d" => Some(7 * 86400),
        _ => None,
    }
}

pub async fn series(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<Q>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::ConsoleRead)?;
    let window = q.window.as_deref().unwrap_or("1h");
    let secs = window_secs(window).ok_or_else(|| ApiError::bad_request("unknown window"))?;
    let points = q.points.unwrap_or(120).clamp(10, 1000);
    let now = chrono::Utc::now().timestamp();
    let rows = crate::services::metrics::range(&state.db, id, now - secs, now, points)?;
    let sum = crate::services::metrics::summary(&state.db, id, now - secs)?;
    Ok(data(serde_json::json!({
        "window": window,
        "points": points,
        "summary": sum,
        "series": rows,
    })))
}

pub async fn summary(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::ConsoleRead)?;
    let now = chrono::Utc::now().timestamp();
    let sum = crate::services::metrics::summary(&state.db, id, now - 24 * 3600)?;
    Ok(data(serde_json::json!(sum)))
}
