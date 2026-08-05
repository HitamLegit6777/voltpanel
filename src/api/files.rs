//! File manager endpoints.
use super::{ok, ApiError, ApiResult, AppState, AuthUser};
use crate::models::{self, User};
use crate::services::files;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

const MAX_INLINE: usize = 2 * 1024 * 1024;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub path: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.read")?;
    let rel = q.path.unwrap_or_else(|| "/".into());
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let entries = state.node_client.files(&node, &s.uuid, &rel).await?;
        return Ok(Json(
            serde_json::json!({ "data": entries, "path": rel, "remote": true }),
        ));
    }
    let entries =
        files::list_dir(&state.cfg, &s, &rel).map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "data": entries, "path": rel, "remote": false }),
    ))
}

#[derive(Deserialize)]
pub struct ReadQuery {
    pub path: String,
}

pub async fn read(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.read")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let value = state.node_client.read_file(&node, &s.uuid, &q.path).await?;
        return Ok(Json(
            serde_json::json!({"path":q.path,"mime":mime_guess::from_path(&q.path).first_or_octet_stream().to_string(),"size":value.get("size").and_then(|v|v.as_u64()).unwrap_or(0),"content_b64":value.get("content_b64").cloned().unwrap_or_default(),"remote":true}),
        ));
    }
    let (bytes, mime) = files::read_file(&state.cfg, &s, &q.path, MAX_INLINE)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(
        serde_json::json!({ "path": q.path, "mime": mime, "size": bytes.len(), "content_b64": STANDARD.encode(&bytes), "remote": false }),
    ))
}

#[derive(Deserialize)]
pub struct WriteReq {
    pub path: String,
    pub content: Option<String>,
    pub content_b64: Option<String>,
    pub append: Option<bool>,
}

pub async fn write(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<WriteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    let data = match (&req.content, &req.content_b64) {
        (Some(c), _) => c.as_bytes().to_vec(),
        (None, Some(b)) => STANDARD
            .decode(b)
            .map_err(|_| ApiError::bad_request("invalid base64"))?,
        (None, None) => return Err(ApiError::bad_request("no content")),
    };
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let remote = crate::node_protocol::FileWriteRequest {
            path: req.path,
            content_b64: STANDARD.encode(data),
            append: req.append.unwrap_or(false),
        };
        state
            .node_client
            .write_file(&node, &s.uuid, &remote)
            .await?;
    } else if req.append.unwrap_or(false) {
        files::append_file(&state.cfg, &s, &req.path, &data)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } else {
        files::write_file(&state.cfg, &s, &req.path, &data)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({ "ok": true, "node": s.node })))
}

#[derive(Deserialize)]
pub struct UploadReq {
    pub path: String,
}
pub async fn upload_multipart(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    let mut saved = 0usize;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        let name = field.file_name().map(|n| n.to_string()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let path = format!("/{}", name.trim_start_matches('/'));
        if s.node != "local" {
            let node = crate::nodes::get_by_name(&state.db, &s.node)?;
            let req = crate::node_protocol::FileWriteRequest {
                path,
                content_b64: STANDARD.encode(&data),
                append: false,
            };
            state.node_client.write_file(&node, &s.uuid, &req).await?;
        } else {
            files::write_file(&state.cfg, &s, &path, &data)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        saved += 1;
    }
    Ok(ok(serde_json::json!({ "saved": saved })))
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub from: String,
    pub to: String,
}

pub async fn rename(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Rename {
                    from: req.from,
                    to: req.to,
                },
            )
            .await?;
    } else {
        files::rename(&state.cfg, &s, &req.from, &req.to)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct CopyReq {
    pub from: String,
    pub to: String,
}

pub async fn copy(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CopyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Copy {
                    from: req.from,
                    to: req.to,
                },
            )
            .await?;
    } else {
        files::copy(&state.cfg, &s, &req.from, &req.to)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct DeleteReq {
    pub path: String,
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<DeleteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Delete { path: req.path },
            )
            .await?;
    } else {
        files::delete(&state.cfg, &s, &req.path)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct ChmodReq {
    pub path: String,
    pub mode: String,
}

pub async fn chmod(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ChmodReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    let mode = u32::from_str_radix(req.mode.trim_start_matches("0o"), 8)
        .map_err(|_| ApiError::bad_request("invalid mode"))?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Chmod {
                    path: req.path,
                    mode,
                },
            )
            .await?;
    } else {
        files::chmod(&state.cfg, &s, &req.path, mode)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct MkdirReq {
    pub path: String,
}

pub async fn mkdir(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<MkdirReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Mkdir { path: req.path },
            )
            .await?;
    } else {
        files::create_dir(&state.cfg, &s, &req.path)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct TouchReq {
    pub path: String,
}

pub async fn touch(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<TouchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let n = crate::nodes::get_by_name(&state.db, &s.node)?;
        state
            .node_client
            .file_operation(
                &n,
                &s.uuid,
                &crate::node_protocol::FileOperation::Touch { path: req.path },
            )
            .await?;
    } else {
        files::create_file(&state.cfg, &s, &req.path)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true})))
}

#[derive(Deserialize)]
pub struct ArchiveReq {
    pub path: String,
    pub format: Option<String>, // zip | tar.gz
}

pub async fn create_archive(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ArchiveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "remote archive creation is not implemented yet",
        ));
    }
    let fmt = req.format.as_deref().unwrap_or("zip");
    let out_name = format!(
        "{}.{}",
        std::path::Path::new(&req.path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "archive".into()),
        if fmt == "tar.gz" { "tar.gz" } else { "zip" }
    );
    let out_rel = format!("/{}", out_name);
    let out_abs = files::resolve(&state.cfg, &s, &out_rel)?;
    let (size, _) = if fmt == "tar.gz" {
        let size = files::tar_gz_dir(&state.cfg, &s, &req.path, &out_abs)?;
        (size, 0u64)
    } else {
        let size = files::zip_dir(&state.cfg, &s, &req.path, &out_abs)?;
        (size, 0u64)
    };
    Ok(ok(serde_json::json!({ "path": out_rel, "size": size })))
}

#[derive(Deserialize)]
pub struct ExtractReq {
    pub archive: String,
    pub dest: String,
}

pub async fn extract(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ExtractReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "remote archive extraction is not implemented yet",
        ));
    }
    if req.archive.ends_with(".tar.gz") || req.archive.ends_with(".tgz") {
        files::extract_tar_gz_into(&state.cfg, &s, &req.archive, &req.dest)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } else {
        files::unzip_into(&state.cfg, &s, &req.archive, &req.dest)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// Download a file (or folder as zip).
pub async fn download(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Response> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.read")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let value = state.node_client.read_file(&node, &s.uuid, &q.path).await?;
        let bytes = STANDARD
            .decode(
                value
                    .get("content_b64")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let name = std::path::Path::new(&q.path)
            .file_name()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        return Ok(download_response(name, bytes));
    }
    let rel = &q.path;
    let abs =
        files::resolve(&state.cfg, &s, rel).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if abs.is_dir() {
        let out_name = format!(
            "{}.zip",
            std::path::Path::new(rel)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "folder".into())
        );
        let out_rel = format!("/.tmp_{}.zip", uuid::Uuid::new_v4().simple());
        let out_abs = files::resolve(&state.cfg, &s, &out_rel)?;
        files::zip_dir(&state.cfg, &s, rel, &out_abs)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        let bytes = std::fs::read(&out_abs).map_err(|e| ApiError::bad_request(e.to_string()))?;
        let _ = std::fs::remove_file(&out_abs);
        Ok(download_response(out_name, bytes))
    } else {
        let (name, bytes) = files::download_bytes(&state.cfg, &s, rel)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        Ok(download_response(name, bytes))
    }
}

fn download_response(name: String, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name.replace('"', "")),
        )
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

#[derive(Deserialize)]
pub struct B64UploadReq {
    pub path: String,
    pub content_b64: String,
}

pub async fn upload_b64(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<B64UploadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let req = crate::node_protocol::FileWriteRequest {
            path: req.path,
            content_b64: req.content_b64,
            append: false,
        };
        state.node_client.write_file(&node, &s.uuid, &req).await?;
    } else {
        files::base64_upload(&state.cfg, &s, &req.path, &req.content_b64)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(ok(serde_json::json!({"ok":true,"node":s.node})))
}

#[derive(Serialize)]
pub struct FileSummary {
    pub entries: usize,
    pub total_size: u64,
}

pub async fn summary(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<FileSummary>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.read")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let root_entries = state.node_client.files(&node, &s.uuid, "/").await?;
        let stats = state.node_client.stats(&node, &s.uuid).await?;
        return Ok(Json(FileSummary {
            entries: root_entries.len(),
            total_size: stats.disk_bytes,
        }));
    }
    let root = files::server_root(&state.cfg, &s);
    let mut total = 0u64;
    let mut entries = 0usize;
    for entry in walkdir::WalkDir::new(&root) {
        if let Ok(e) = entry {
            entries += 1;
            if e.file_type().is_file() {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    Ok(Json(FileSummary {
        entries,
        total_size: total,
    }))
}

pub async fn exists_check(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.read")?;
    if s.node != "local" {
        if q.path == "/" {
            return Ok(Json(serde_json::json!({"exists":true})));
        }
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let path = std::path::Path::new(&q.path);
        let name = path
            .file_name()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = path
            .parent()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".into());
        let exists = state
            .node_client
            .files(&node, &s.uuid, &parent)
            .await?
            .iter()
            .any(|v| v.name == name);
        return Ok(Json(serde_json::json!({"exists":exists})));
    }
    let ok = files::exists(&state.cfg, &s, &q.path);
    Ok(Json(serde_json::json!({ "exists": ok })))
}

pub async fn move_files(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<MoveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state, &u, id, "files.write")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        for from in &req.files {
            state
                .node_client
                .file_operation(
                    &node,
                    &s.uuid,
                    &crate::node_protocol::FileOperation::Move {
                        from: from.clone(),
                        destination: req.dest.clone(),
                    },
                )
                .await?;
        }
    } else {
        for from in &req.files {
            files::move_into(&state.cfg, &s, from, &req.dest)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
    }
    Ok(ok(serde_json::json!({"ok":true,"node":s.node})))
}

#[derive(Deserialize)]
pub struct MoveReq {
    pub files: Vec<String>,
    pub dest: String,
}
