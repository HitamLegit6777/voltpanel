//! Site endpoints: domain publishing bound to a workspace.
use super::{data, ok, require_capability, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::services::websites::{self, SiteInput};
use axum::extract::{Path, State};
use axum::Json;

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesRead)?;
    let sites = websites::list(&state.db, id)?;
    Ok(data(serde_json::json!(sites)))
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SiteInput>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesWrite)?;
    let site = websites::create(&state.db, id, &req)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(data(serde_json::json!(site)))
}

pub async fn get(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesRead)?;
    match websites::get(&state.db, id, site_id)? {
        Some(site) => Ok(data(serde_json::json!(site))),
        None => Err(ApiError::not_found("site not found")),
    }
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
    Json(req): Json<SiteInput>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesWrite)?;
    let site = websites::update(&state.db, id, site_id, &req)
        .map_err(|e| ApiError::bad_request(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("site not found"))?;
    Ok(data(serde_json::json!(site)))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesWrite)?;
    if !websites::delete(&state.db, id, site_id)? {
        return Err(ApiError::not_found("site not found"));
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, site_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, id, Capability::FilesWrite)?;
    let site = websites::get(&state.db, id, site_id)?
        .ok_or_else(|| ApiError::not_found("site not found"))?;
    let updated = websites::set_enabled(&state.db, id, site_id, !site.enabled)
        .map_err(|e| ApiError::bad_request(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("site not found"))?;
    Ok(data(serde_json::json!(updated)))
}
