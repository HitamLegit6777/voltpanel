//! File manager endpoints.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, User};
use crate::services::files;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

// ---- DB execution off the async worker ----
//
// Pool-based `models`/`nodes` calls must not run on a Tokio worker thread;
// `blocking(...)` runs them on Tokio's blocking pool (see src/api/servers.rs
// for the full contract). This module owns no direct SQL, so `Db::call` is
// unused here.

/// Inline read ceiling (MiB-based, mirroring the remote daemon's max_upload
/// cap) so a file viewable on a node inline is viewable locally too.
fn inline_cap(cfg: &crate::config::Config) -> usize {
    (cfg.web.max_body_mb as usize).saturating_mul(1024 * 1024)
}

/// Reject upload payloads above the configured body cap (web.max_body_mb),
/// mirroring the remote daemon's max_upload limit so local and remote paths
/// enforce the same bound. The HTTP layer's DefaultBodyLimit bounds the raw
/// body; this makes the decoded-payload bound explicit and uniform.
fn check_upload_size(cfg: &crate::config::Config, len: usize) -> ApiResult<()> {
    let cap = inline_cap(cfg);
    if len > cap {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("upload exceeds {} MiB limit", cfg.web.max_body_mb),
        ));
    }
    Ok(())
}

async fn access_ok(
    state: &AppState,
    user: &User,
    server_id: i64,
) -> ApiResult<crate::models::Server> {
    let s = blocking(state.db.clone(), move |db| {
        models::get_server(&db, server_id)
    })
    .await
    .map_err(|_| ApiError::not_found("server not found"))?;

    let user = user.clone();
    let sid = s.id;
    if !blocking(state.db.clone(), move |db| {
        models::user_has_server_access(&db, &user, sid)
    })
    .await?
    {
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
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    let rel = q.path.unwrap_or_else(|| "/".into());
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let value = state.node_client.read_file(&node, &s.uuid, &q.path).await?;
        return Ok(Json(
            serde_json::json!({"path":q.path,"mime":mime_guess::from_path(&q.path).first_or_octet_stream().to_string(),"size":value.get("size").and_then(|v|v.as_u64()).unwrap_or(0),"content_b64":value.get("content_b64").cloned().unwrap_or_default(),"remote":true}),
        ));
    }
    let (bytes, mime) = files::read_file(&state.cfg, &s, &q.path, inline_cap(&state.cfg))
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<WriteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let data = match (&req.content, &req.content_b64) {
        (Some(c), _) => c.as_bytes().to_vec(),
        (None, Some(b)) => STANDARD
            .decode(b)
            .map_err(|_| ApiError::bad_request("invalid base64"))?,
        (None, None) => return Err(ApiError::bad_request("no content")),
    };
    // Enforce the configured body cap here too: the HTTP layer's
    // DefaultBodyLimit already bounds the raw body, but this keeps the
    // decoded payload limit identical for local and remote paths.
    check_upload_size(&state.cfg, data.len())?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let mut saved = 0usize;
    // The optional "path" text field selects the target directory (e.g.
    // "/sub/dir"); files then land inside it. Falls back to the sandbox root
    // when the field is absent so older clients keep working. The field may
    // appear anywhere in the part stream, so it is recognized in the loop.
    let mut target_dir = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        if field.file_name().is_none() {
            if field.name() == Some("path") {
                target_dir = field
                    .text()
                    .await
                    .map_err(|e| ApiError::bad_request(e.to_string()))?;
            } else {
                // A control field we do not know about is a file field that
                // carries no filename: reject it rather than guessing.
                return Err(ApiError::bad_request("invalid filename"));
            }
            continue;
        }
        let name = field.file_name().map(|n| n.to_string()).unwrap_or_default();
        validate_upload_name(&name)?;
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        check_upload_size(&state.cfg, data.len())?;
        let path = join_upload_path(&target_dir, &name);
        if s.node != "local" {
            let node_name = s.node.clone();
            let node = blocking(state.db.clone(), move |db| {
                crate::nodes::get_by_name(&db, &node_name)
            })
            .await?;
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

/// Prefix a target directory onto an uploaded filename, keeping a single
/// leading slash and refusing absolute names that would bypass the dir.
fn join_upload_path(dir: &str, name: &str) -> String {
    let base = dir.trim_matches('/');
    let file = name.trim_start_matches('/');
    if base.is_empty() {
        format!("/{}", file)
    } else {
        format!("/{}/{}", base, file)
    }
}

/// Reject upload filenames that would smuggle a path: separators, `..`,
/// NUL/control characters. Browsers send bare basenames; anything else is a
/// malformed request and must fail with 400 rather than silently rewriting
/// (or worse, resolving) the path.
fn validate_upload_name(name: &str) -> ApiResult<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request("invalid filename"));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub from: String,
    pub to: String,
}

pub async fn rename(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<RenameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<CopyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<DeleteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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

/// Parse an octal mode string and mask to rwx bits only: never grant
/// setuid/setgid/sticky on panel-managed files (privilege-escalation
/// surface). A caller asking for `0o4755` gets `0o755`, never more.
fn parse_mode_mask(raw: &str) -> ApiResult<u32> {
    let mode = u32::from_str_radix(raw.trim_start_matches("0o"), 8)
        .map_err(|_| ApiError::bad_request("invalid mode"))?;
    Ok(mode & 0o777)
}

pub async fn chmod(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ChmodReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    let mode = parse_mode_mask(&req.mode)?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<MkdirReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<TouchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ArchiveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<ExtractReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Response> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let value = state.node_client.read_file(&node, &s.uuid, &q.path).await?;
        let b64 = value
            .get("content_b64")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Bound the decoded size up front (4 base64 chars encode 3 bytes) and
        // again after decoding: a multi-GB remote file must never be
        // materialized in panel memory. Mirrors the local cap (`inline_cap`)
        // so remote downloads enforce the same ceiling as local ones.
        check_upload_size(&state.cfg, b64.len().saturating_mul(3) / 4)?;
        let bytes = STANDARD
            .decode(b64)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        check_upload_size(&state.cfg, bytes.len())?;
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
        // Write the archive to a unique path in the system temp dir, never
        // inside the sandbox: an archive written into the very folder being
        // zipped (e.g. the root "/") would include itself and grow
        // unboundedly. The temp file is removed on every path — including
        // when zipping fails — so nothing leaks.
        let out_abs = std::env::temp_dir().join(format!(
            ".voltpanel-dl-{}.zip",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> ApiResult<Response> {
            files::zip_dir(&state.cfg, &s, rel, &out_abs)
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            let file =
                std::fs::File::open(&out_abs).map_err(|e| ApiError::bad_request(e.to_string()))?;
            let size = file
                .metadata()
                .map_err(|e| ApiError::bad_request(e.to_string()))?
                .len();
            Ok(file_response(out_name, file, size))
        })();
        let _ = std::fs::remove_file(&out_abs);
        Ok(result?)
    } else {
        let (name, file, size) = files::download_file(&state.cfg, &s, rel)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        Ok(file_response(name, file, size))
    }
}

/// Stream a file as a download response: Content-Length from the file size
/// and the body streamed from disk, so multi-GB files never materialize in
/// RAM. The temp/archive is safe to unlink right after this returns — the
/// open fd keeps the data alive on Unix.
fn file_response(name: String, file: std::fs::File, size: u64) -> Response {
    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", sanitize_attachment(&name)),
        )
        .header(header::CONTENT_LENGTH, size.to_string())
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}
/// Strip characters that must not appear in a Content-Disposition header
/// value. CR/LF would let a filename inject header lines (and, when fed
/// through `Response::builder().header`, make it panic); control chars and
/// quotes are equally unsafe.
fn sanitize_attachment(name: &str) -> String {
    name.chars()
        .filter(|&c| !c.is_control() && c != '"' && c != '\\')
        .collect()
}

fn download_response(name: String, bytes: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", sanitize_attachment(&name)),
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<B64UploadReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    // Upper bound on the decoded size (4 base64 chars encode 3 bytes); the
    // payload is decoded and validated below, but a size above the cap is
    // rejected up front for both local and remote paths.
    check_upload_size(&state.cfg, req.content_b64.len().saturating_mul(3) / 4)?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<FileSummary>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    for e in walkdir::WalkDir::new(&root).into_iter().flatten() {
        entries += 1;
        if e.file_type().is_file() {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(Json(FileSummary {
        entries,
        total_size: total,
    }))
}

pub async fn exists_check(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    if s.node != "local" {
        if q.path == "/" {
            return Ok(Json(serde_json::json!({"exists":true})));
        }
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<MoveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    if s.node != "local" {
        let node_name = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
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

#[derive(Deserialize)]
pub struct PullReq {
    pub url: String,
    /// Destination directory inside the workspace (e.g. "/" or "/sub").
    pub path: String,
    /// Optional filename override; defaults to the URL's basename.
    pub filename: Option<String>,
}

/// Start a background download of `url` into the server's workspace. The SSRF
/// guard runs synchronously here so a bad scheme, a blocked literal host, or
/// a blocked resolution fails fast with a 400; the background task re-validates
/// with a fresh resolution at connect time and on every redirect, then writes
/// through the same workspace containment the rest of the file API uses.
pub async fn pull(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<PullReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let _ = files::prepare_pull(&req.url).await?;
    let name = req
        .filename
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| files::url_basename(&req.url));
    validate_upload_name(&name)?;
    let rel = join_upload_path(&req.path, &name);
    let cap = inline_cap(&state.cfg) as u64;
    let handle = files::start_pull(server.id, &req.url, &rel, &server.node);
    if server.node != "local" {
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let node_client = state.node_client.clone();
        let node = node.clone();
        let uuid = server.uuid.clone();
        let url = req.url.clone();
        let rel = rel.clone();
        let handle_clone = handle.clone();
        tokio::spawn(async move {
            let result = files::remote_pull(
                &node_client,
                &node,
                &uuid,
                &rel,
                &url,
                cap,
                &handle_clone.state,
            )
            .await;
            files::finish_pull(&handle_clone, result);
        });
    } else {
        let cfg = state.cfg.clone();
        let server = server.clone();
        let url = req.url.clone();
        let rel = rel.clone();
        let handle_clone = handle.clone();
        tokio::spawn(async move {
            let result =
                files::local_pull(&cfg, &server, &rel, &url, cap, &handle_clone.state).await;
            files::finish_pull(&handle_clone, result);
        });
    }
    Ok(data(serde_json::json!({
        "id": handle.id,
        "url": req.url,
        "dest": rel,
        "node": server.node,
    })))
}

/// Query a background pull's status and progress.
pub async fn pull_status(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, transfer_id)): Path<(i64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesRead).await?;
    let handle =
        files::get_pull(&transfer_id).ok_or_else(|| ApiError::not_found("transfer not found"))?;
    if handle.server_id != server.id {
        return Err(ApiError::not_found("transfer not found"));
    }
    Ok(data(serde_json::to_value(files::pull_status(&handle))?))
}

/// Cancel a running background pull; a finished transfer cannot be cancelled.
pub async fn pull_cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, transfer_id)): Path<(i64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let server = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::FilesWrite).await?;
    let handle =
        files::get_pull(&transfer_id).ok_or_else(|| ApiError::not_found("transfer not found"))?;
    if handle.server_id != server.id {
        return Err(ApiError::not_found("transfer not found"));
    }
    Ok(data(
        serde_json::json!({ "cancelled": files::cancel_pull(&handle) }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_attachment_strips_header_unsafe_chars() {
        assert_eq!(sanitize_attachment("a\"b"), "ab");
        assert_eq!(sanitize_attachment("a\r\nX-Injected: 1"), "aX-Injected: 1");
        assert_eq!(sanitize_attachment("a\u{0000}b\u{001f}"), "ab");
        assert_eq!(sanitize_attachment("plain.txt"), "plain.txt");
    }

    #[test]
    fn validate_upload_name_rejects_path_smuggling() {
        assert!(validate_upload_name("ok.txt").is_ok());
        assert!(validate_upload_name("a b.txt").is_ok());
        // Empty, dot, traversal, separators, NUL/CRLF controls: all 400.
        for bad in [
            "",
            "..",
            ".",
            "../x",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "a\u{0000}b",
            "a\r\nb",
        ] {
            assert!(validate_upload_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn parse_mode_mask_never_grants_special_bits() {
        assert_eq!(parse_mode_mask("644").unwrap(), 0o644);
        assert_eq!(parse_mode_mask("0o755").unwrap(), 0o755);
        assert_eq!(parse_mode_mask("0o4755").unwrap(), 0o755);
        assert_eq!(parse_mode_mask("7777").unwrap(), 0o777);
        assert_eq!(parse_mode_mask("0o1777").unwrap(), 0o777);
        assert!(parse_mode_mask("not-a-mode").is_err());
        assert!(parse_mode_mask("").is_err());
    }

    #[test]
    fn check_upload_size_enforces_configured_cap() {
        let cfg = crate::config::Config::default(); // max_body_mb = 64
        let cap = 64usize * 1024 * 1024;
        assert!(check_upload_size(&cfg, cap).is_ok());
        assert!(check_upload_size(&cfg, cap - 1).is_ok());
        assert!(check_upload_size(&cfg, cap + 1).is_err());
    }

    #[test]
    fn join_upload_path_keeps_single_leading_slash() {
        assert_eq!(join_upload_path("", "file.txt"), "/file.txt");
        assert_eq!(join_upload_path("/sub", "file.txt"), "/sub/file.txt");
        assert_eq!(join_upload_path("/sub/", "file.txt"), "/sub/file.txt");
    }
}
