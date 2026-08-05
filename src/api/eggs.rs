//! Egg endpoints: CRUD + import/export.
use super::{data, ok, AdminUser, ApiResult, AppState, AuthUser};
use crate::models::{self, Egg, EggVariable};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

pub async fn list(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let eggs = models::list_eggs(&state.db)?;
    Ok(data(serde_json::to_value(eggs)?))
}

pub async fn get(
    State(state): State<AppState>,
    _u: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Egg>> {
    Ok(Json(models::get_egg(&state.db, id)?))
}

#[derive(Deserialize)]
pub struct CreateEggReq {
    pub name: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub docker_image: Option<String>,
    pub startup: String,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<EggVariable>>,
    pub default_config: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateEggReq>,
) -> ApiResult<Json<Egg>> {
    let uuid = uuid::Uuid::new_v4().to_string();
    let id = models::create_egg(
        &state.db,
        &uuid,
        &req.name,
        req.description.as_deref().unwrap_or(""),
        req.author.as_deref().unwrap_or(""),
        req.category.as_deref().unwrap_or("generic"),
        req.docker_image.as_deref().unwrap_or("alpine:latest"),
        &req.startup,
        req.default_config.as_deref(),
        None,
        req.install_script.as_deref(),
        req.variables.as_deref().unwrap_or(&[]),
        req.stop_command.as_deref().unwrap_or("stop"),
    )?;
    Ok(Json(models::get_egg(&state.db, id)?))
}

#[derive(Deserialize)]
pub struct UpdateEggReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub docker_image: Option<String>,
    pub startup: Option<String>,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<EggVariable>>,
    pub default_config: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateEggReq>,
) -> ApiResult<Json<Egg>> {
    let mut e = models::get_egg(&state.db, id)?;
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
    if let Some(v) = req.docker_image {
        e.docker_image = v;
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
    models::update_egg(&state.db, &e)?;
    Ok(Json(e))
}

pub async fn delete(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    models::delete_egg(&state.db, id)?;
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
) -> ApiResult<Json<Egg>> {
    let parsed = crate::services::egg::parse_egg_json(&req.json)?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let id = models::create_egg(
        &state.db,
        &uuid,
        &parsed.name,
        &parsed.description,
        &parsed.author,
        &parsed.category,
        &parsed.docker_image,
        &parsed.startup,
        parsed
            .config
            .as_ref()
            .map(|c| serde_json::to_string(c).unwrap_or_default())
            .as_deref(),
        None,
        parsed.install.as_ref().map(|i| i.script.clone()).as_deref(),
        &parsed.variables,
        &parsed.stop,
    )?;
    Ok(Json(models::get_egg(&state.db, id)?))
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
    let e = models::get_egg(&state.db, id)?;
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "name": e.name,
        "description": e.description,
        "author": e.author,
        "category": e.category,
        "docker_image": e.docker_image,
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
    let mut stmt = conn.prepare("SELECT DISTINCT category FROM egss ORDER BY category")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(data(serde_json::json!(out)))
}
