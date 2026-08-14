//! SQLite database endpoints per server.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models;
use crate::services::databases;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::Json;
use futures::StreamExt;
use serde::Deserialize;

// ---- DB execution off the async worker ----
//
// Pool-based `models` calls must not run on a Tokio worker thread;
// `blocking(...)` runs them on Tokio's blocking pool (see src/api/servers.rs
// for the full contract). This module owns no direct SQL, so `Db::call` is
// unused here.

async fn access_ok(
    state: &AppState,
    user: &AuthUser,
    server_id: i64,
    capability: Capability,
) -> ApiResult<crate::models::Server> {
    if !state.cfg.features.enable_databases {
        return Err(ApiError::not_found("databases are disabled"));
    }
    let s = blocking(state.db.clone(), move |db| {
        models::get_server(&db, server_id)
    })
    .await
    .map_err(|_| ApiError::not_found("server not found"))?;
    super::require_capability(state, user, s.id, capability).await?;
    if s.node != "local" {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "embedded SQLite databases are currently local-workload only",
        ));
    }
    Ok(s)
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseRead).await?;
    let names = databases::list(&s, &state.cfg.paths.datalab_dir)?;
    let out: Vec<serde_json::Value> = names
        .iter()
        .map(|n| {
            serde_json::json!({
                "name": n,
                "size": databases::size(&s, &state.cfg.paths.datalab_dir, n).unwrap_or(0),
            })
        })
        .collect();
    Ok(data(serde_json::json!(out)))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseWrite).await?;
    databases::validate_name(&req.name)?;
    if databases::list(&s, &state.cfg.paths.datalab_dir)?.contains(&req.name) {
        return Err(ApiError::bad_request("database already exists"));
    }
    databases::open_server_db(&s, &state.cfg.paths.datalab_dir, &req.name)?;
    Ok(ok(serde_json::json!({ "name": req.name })))
}

#[derive(Deserialize)]
pub struct ExecReq {
    pub sql: String,
}

pub async fn exec(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
    Json(req): Json<ExecReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseWrite).await?;
    databases::validate_name(&name)?;
    databases::exec(&s, &state.cfg.paths.datalab_dir, &name, &req.sql)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct QueryReq {
    pub sql: String,
}

pub async fn query(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
    Json(req): Json<QueryReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseRead).await?;
    databases::validate_name(&name)?;
    let rows = databases::query(&s, &state.cfg.paths.datalab_dir, &name, &req.sql)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(rows))
}

pub async fn drop(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseWrite).await?;
    databases::validate_name(&name)?;
    databases::drop(&s, &state.cfg.paths.datalab_dir, &name)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// A transient export snapshot left in the panel-owned Data Lab root (never
/// in the workload-visible `<uuid>` directory); deleting it on drop keeps a
/// client abort from leaking a snapshot on disk.
struct ExportCleanup(std::path::PathBuf);
impl Drop for ExportCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Download a consistent snapshot of one database (SQLite online backup), so
/// the bytes on the wire are a valid database even while the workload writes.
pub async fn export(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
) -> ApiResult<axum::response::Response> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseRead).await?;
    databases::validate_name(&name)?;
    let datalab = state.cfg.paths.datalab_dir.clone();
    // Snapshot into the panel-owned Data Lab root (same disk as the source,
    // invisible to the workload); the size is bounded by the Data Lab quota.
    let tmp = datalab.join(format!(
        ".export-{}-{}-{}.db",
        s.uuid,
        name,
        std::process::id()
    ));
    let cleanup = ExportCleanup(tmp.clone());
    let server = s.clone();
    let name_c = name.clone();
    let (file, size) = tokio::task::spawn_blocking(move || -> anyhow::Result<(std::fs::File, u64)> {
        databases::export_to(&server, &datalab, &name_c, &tmp)?;
        let size = std::fs::metadata(&tmp)?.len();
        let file = std::fs::File::open(&tmp)?;
        Ok((file, size))
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    // Keep the cleanup guard alive for the whole response by moving it into
    // the stream: it deletes the snapshot when the body completes or a client
    // aborts mid-stream.
    let stream = async_stream::stream! {
        let _guard = cleanup;
        let mut reader = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
        while let Some(chunk) = reader.next().await {
            yield chunk;
        }
    };
    Ok(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}.db\""),
        )
        .header(header::CONTENT_LENGTH, size.to_string())
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Replace a database with an uploaded `.db` file. The upload is
/// integrity-checked and staged before the live file is swapped, so a corrupt
/// upload is refused without touching the current database. Bounded by the
/// server's Data Lab byte quota.
pub async fn import(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
    req: Request,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseWrite).await?;
    databases::validate_name(&name)?;
    let cap = databases::datalab_byte_cap(&s);
    // Fail fast on a declared oversize upload before reading the body; the
    // streaming loop below is the real bound for chunked/undeclared bodies.
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > cap {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "database import exceeds the server Data Lab quota",
            ));
        }
    }
    // Buffer the upload on disk (panel-owned Data Lab root) instead of RAM,
    // capping at the same quota.
    let datalab = state.cfg.paths.datalab_dir.clone();
    let temp = tempfile::Builder::new()
        .prefix(&format!(".upload-{}-", s.uuid))
        .tempfile_in(&datalab)
        .map_err(ApiError::from)?;
    let mut stream = req.into_body().into_data_stream();
    let mut written: u64 = 0;
    {
        use std::io::Write;
        let mut temp = &temp;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::bad_request(e.to_string()))?;
            written = written.saturating_add(chunk.len() as u64);
            if written > cap {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "database import exceeds the server Data Lab quota",
                ));
            }
            temp.write_all(&chunk).map_err(ApiError::from)?;
        }
        temp.flush().map_err(ApiError::from)?;
    }
    let upload = temp.path().to_path_buf();
    let server = s.clone();
    let name_c = name.clone();
    blocking(state.db.clone(), move |_| {
        databases::import(&server, &datalab, &name_c, &upload)
    })
    .await?;
    Ok(ok(serde_json::json!({ "name": name })))
}

#[derive(Deserialize)]
pub struct SqliteQuery {
    pub sql: Option<String>,
}

pub async fn tables(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, name)): Path<(i64, String)>,
    Query(q): Query<SqliteQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &user, server_id, Capability::DatabaseRead).await?;
    databases::validate_name(&name)?;
    let sql = q.sql.unwrap_or_else(|| {
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name".into()
    });
    let rows = databases::query(&s, &state.cfg.paths.datalab_dir, &name, &sql)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(rows))
}

/// Reveal the stored database password to an authorized user. The password is
/// stripped from every serialized `Database` (server detail, lists), so this
/// is the only endpoint that returns it: owner, root admin, or a subuser with
/// `startup.secrets` on top of `database.read`.
pub async fn credentials(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, db_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, &user, server_id, Capability::DatabaseRead).await?;
    let can_reveal = if u.root_admin || u.id == s.user_id {
        true
    } else {
        let u2 = user.0.clone();
        let sid = s.id;
        blocking(state.db.clone(), move |db| {
            models::user_has_capability(&db, &u2, sid, Capability::StartupSecrets)
        })
        .await?
    };
    if !can_reveal {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "startup.secrets capability required to view database credentials",
        ));
    }
    let db = blocking(state.db.clone(), move |db| models::get_database(&db, db_id))
        .await
        .map_err(|_| ApiError::not_found("database not found"))?;
    if db.server_id != server_id {
        return Err(ApiError::not_found("database not found"));
    }
    let mut out = serde_json::to_value(&db)?;
    out["password"] = serde_json::json!(db.password);
    Ok(data(out))
}
