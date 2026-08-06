//! Workload blueprint endpoints: CRUD + import/export.
use super::{data, ok, require_capability, AdminUser, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::models::{self, Blueprint, BlueprintInput};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

pub async fn list(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let blueprints = models::list_blueprints(&state.db)?;
    Ok(data(serde_json::to_value(blueprints)?))
}

pub async fn get(
    State(state): State<AppState>,
    _u: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Blueprint>> {
    Ok(Json(models::get_blueprint(&state.db, id)?))
}

#[derive(Deserialize)]
pub struct CreateBlueprintReq {
    pub name: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub runtime_hint: Option<String>,
    pub startup: String,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<BlueprintInput>>,
    pub default_config: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateBlueprintReq>,
) -> ApiResult<Json<Blueprint>> {
    let uuid = uuid::Uuid::new_v4().to_string();
    let id = models::create_blueprint(
        &state.db,
        &uuid,
        &req.name,
        req.description.as_deref().unwrap_or(""),
        req.author.as_deref().unwrap_or(""),
        req.category.as_deref().unwrap_or("generic"),
        req.runtime_hint.as_deref().unwrap_or("native"),
        &req.startup,
        req.default_config.as_deref(),
        req.install_script.as_deref(),
        req.variables.as_deref().unwrap_or(&[]),
        req.stop_command.as_deref().unwrap_or("stop"),
    )?;
    Ok(Json(models::get_blueprint(&state.db, id)?))
}

#[derive(Deserialize)]
pub struct UpdateBlueprintReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub runtime_hint: Option<String>,
    pub startup: Option<String>,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<BlueprintInput>>,
    pub default_config: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateBlueprintReq>,
) -> ApiResult<Json<Blueprint>> {
    // Preserve history: capture the pre-mutation state as the next revision
    // before any field below changes, so every update is undoable.
    crate::services::blueprint::snapshot(&state.db, id, &a.0.username, "")?;
    let mut e = models::get_blueprint(&state.db, id)?;
    if let Some(v) = req.name {
        e.name = v;
    }
    if let Some(v) = req.description {
        e.description = v;
    }
    if let Some(v) = req.author {
        e.author = v;
    }
    if let Some(v) = req.category {
        e.category = v;
    }
    if let Some(v) = req.runtime_hint {
        e.runtime_hint = v;
    }
    if let Some(v) = req.startup {
        e.startup = v;
    }
    if let Some(v) = req.install_script {
        e.install_script = Some(v);
    }
    if let Some(v) = req.stop_command {
        e.stop_command = v;
    }
    if let Some(v) = req.variables {
        e.variables = v;
    }
    if let Some(v) = req.default_config {
        e.default_config = Some(v);
    }
    models::update_blueprint(&state.db, &e)?;
    Ok(Json(e))
}

pub async fn delete(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    models::delete_blueprint(&state.db, id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ImportReq {
    pub json: String,
}

pub async fn import(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<ImportReq>,
) -> ApiResult<Json<Blueprint>> {
    let parsed = crate::services::blueprint::parse_blueprint_json(&req.json)?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let id = models::create_blueprint(
        &state.db,
        &uuid,
        &parsed.name,
        &parsed.description,
        &parsed.author,
        &parsed.category,
        &parsed.runtime_hint,
        &parsed.startup,
        parsed
            .config
            .as_ref()
            .map(|c| serde_json::to_string(c).unwrap_or_default())
            .as_deref(),
        parsed.install.as_ref().map(|i| i.script.clone()).as_deref(),
        &parsed.variables,
        &parsed.stop,
    )?;
    Ok(Json(models::get_blueprint(&state.db, id)?))
}

#[derive(Serialize)]
pub struct ExportResp {
    pub json: String,
}

pub async fn export(
    State(state): State<AppState>,
    _u: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<ExportResp>> {
    let e = models::get_blueprint(&state.db, id)?;
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "name": e.name,
        "description": e.description,
        "author": e.author,
        "category": e.category,
        "runtime_hint": e.runtime_hint,
        "startup": e.startup,
        "install": e.install_script.map(|s| serde_json::json!({ "script": s })),
        "variables": e.variables,
        "stop": e.stop_command,
    }))?;
    Ok(Json(ExportResp { json: out }))
}

pub async fn categories(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let mut stmt = conn.prepare("SELECT DISTINCT category FROM blueprints ORDER BY category")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(data(serde_json::json!(out)))
}

// ---------------- Versioning & drift ----------------

pub async fn revisions(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = crate::services::blueprint::list_revisions(&state.db, id)?;
    Ok(data(serde_json::to_value(rows)?))
}

pub async fn revision_detail(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((id, version)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = crate::services::blueprint::revision_snapshot(&state.db, id, version)?;
    Ok(data(snap))
}

#[derive(Deserialize)]
pub struct RollbackReq {
    pub version: i64,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn rollback(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<RollbackReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let version = match req.note {
        Some(note) => crate::services::blueprint::rollback_with_note(
            &state.db,
            id,
            req.version,
            &a.0.username,
            &note,
        )?,
        None => crate::services::blueprint::rollback(&state.db, id, req.version, &a.0.username)?,
    };
    Ok(data(serde_json::json!({ "version": version })))
}

pub async fn drift(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = crate::services::blueprint::drift_for_blueprint(&state.db, id)?;
    Ok(data(serde_json::to_value(rows)?))
}

#[derive(Deserialize)]
pub struct PinReq {
    pub version: i64,
}

pub async fn pin(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<PinReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_capability(&state, &u, server_id, Capability::StartupUpdate)?;
    crate::services::blueprint::pin_server(&state.db, server_id, req.version)?;
    Ok(data(serde_json::json!({ "ok": true })))
}
