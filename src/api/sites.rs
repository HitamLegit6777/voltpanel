//! Site endpoints: domain publishing bound to a workspace.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::services::websites::{self, SiteInput};
use axum::extract::{Path, State};
use axum::Json;


pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    let sid = id;
    let sites = blocking(state.db.clone(), move |db| websites::list(&db, sid)).await?;
    Ok(data(serde_json::json!(sites)))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SiteInput>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let sid = id;
    let cfg = state.cfg.clone();
    let site = blocking(state.db.clone(), move |db| {
        websites::create(&db, &cfg, sid, &req)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(data(serde_json::json!(site)))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    let site = blocking(state.db.clone(), move |db| {
        websites::get(&db, id, site_id)
    })
    .await?
    .ok_or_else(|| ApiError::not_found("site not found"))?;
    Ok(data(serde_json::json!(site)))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
    Json(req): Json<SiteInput>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let cfg = state.cfg.clone();
    let site = blocking(state.db.clone(), move |db| {
        websites::update(&db, &cfg, id, site_id, &req)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("site not found"))?;
    Ok(data(serde_json::json!(site)))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let cfg = state.cfg.clone();
    if !blocking(state.db.clone(), move |db| {
        websites::delete(&db, &cfg, id, site_id)
    })
    .await?
    {
        return Err(ApiError::not_found("site not found"));
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let site = blocking(state.db.clone(), move |db| {
        websites::get(&db, id, site_id)
    })
    .await?
    .ok_or_else(|| ApiError::not_found("site not found"))?;
    let updated = blocking(state.db.clone(), move |db| {
        websites::set_enabled(&db, id, site_id, !site.enabled)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?
    .ok_or_else(|| ApiError::not_found("site not found"))?;
    Ok(data(serde_json::json!(updated)))
}
