//! SQLite database endpoints per server.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::models::{self, User};
use crate::services::databases;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id).map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn list(State(state): State<AppState>, AuthUser(u): AuthUser, Path(server_id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    let names = databases::list(&s)?;
    let out: Vec<serde_json::Value> = names
        .iter()
        .map(|n| {
            serde_json::json!({
                "name": n,
                "size": databases::size(&s, n).unwrap_or(0),
            })
        })
        .collect();
    Ok(data(serde_json::json!(out)))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
}

pub async fn create(State(state): State<AppState>, AuthUser(u): AuthUser, Path(server_id): Path<i64>, Json(req): Json<CreateReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    databases::validate_name(&req.name)?;
    if databases::list(&s)?.contains(&req.name) {
        return Err(ApiError::bad_request("database already exists"));
    }
    databases::open_server_db(&s, &state.cfg.paths.servers_dir, &req.name)?;
    Ok(ok(serde_json::json!({ "name": req.name })))
}

#[derive(Deserialize)]
pub struct ExecReq {
    pub sql: String,
}

pub async fn exec(State(state): State<AppState>, AuthUser(u): AuthUser, Path((server_id, name)): Path<(i64, String)>, Json(req): Json<ExecReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    databases::validate_name(&name)?;
    databases::exec(&s, &name, &req.sql)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct QueryReq {
    pub sql: String,
}

pub async fn query(State(state): State<AppState>, AuthUser(u): AuthUser, Path((server_id, name)): Path<(i64, String)>, Json(req): Json<QueryReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    databases::validate_name(&name)?;
    let rows = databases::query(&s, &name, &req.sql)?;
    Ok(Json(rows))
}

pub async fn drop(State(state): State<AppState>, AuthUser(u): AuthUser, Path((server_id, name)): Path<(i64, String)>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    databases::validate_name(&name)?;
    databases::drop(&s, &name)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct SqliteQuery {
    pub sql: Option<String>,
}

pub async fn tables(State(state): State<AppState>, AuthUser(u): AuthUser, Path((server_id, name)): Path<(i64, String)>, Query(q): Query<SqliteQuery>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, server_id)?;
    databases::validate_name(&name)?;
    let sql = q.sql.unwrap_or_else(|| "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name".into());
    let rows = databases::query(&s, &name, &sql)?;
    Ok(Json(rows))
}
