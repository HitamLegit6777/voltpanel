//! Backup endpoints.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::models::{self, User};
use crate::services::backups;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, server_id)?;
    super::require_capability(&state, &u, server_id, Capability::BackupsRead)?;
    let backups = models::list_backups(&state.db, server_id)?;
    Ok(data(serde_json::to_value(backups)?))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let server = access_ok(&state, &u, server_id)?;
    super::require_capability(&state, &u, server_id, Capability::BackupsWrite)?;
    let name = req
        .name
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        let was_running = server.status == "running";
        if was_running {
            state
                .node_client
                .power(&node, &server.uuid, crate::node_protocol::PowerAction::Stop)
                .await?;
            for _ in 0..50 {
                if state
                    .node_client
                    .stats(&node, &server.uuid)
                    .await
                    .map(|s| s.pid.is_none())
                    .unwrap_or(true)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        let snapshot_result = state.node_client.snapshot(&node, &server.uuid).await;
        if was_running {
            let _ = state
                .node_client
                .power(
                    &node,
                    &server.uuid,
                    crate::node_protocol::PowerAction::Start,
                )
                .await;
        }
        let snapshot = snapshot_result?;
        let uuid = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(&state.cfg.paths.backups_dir)?;
        let path = state.cfg.paths.backups_dir.join(format!("{uuid}.tar.gz"));
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            snapshot.archive_b64,
        )
        .map_err(|e| ApiError::bad_request(format!("remote snapshot decode: {e}")))?;
        std::fs::write(&path, &bytes)?;
        let id = models::create_backup(
            &state.db,
            &uuid,
            server.id,
            &name,
            &path.to_string_lossy(),
            bytes.len() as i64,
            &snapshot.checksum,
            "tar.gz",
        )?;
        return Ok(Json(
            serde_json::json!({"id":id,"size":bytes.len(),"checksum":snapshot.checksum,"remote":true}),
        ));
    }
    let (id, size, checksum) = backups::create(&state.db, &state.cfg, server_id, &name).await?;
    Ok(Json(
        serde_json::json!({"id":id,"size":size,"checksum":checksum,"remote":false}),
    ))
}

pub async fn download(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Response> {
    let b = models::get_backup(&state.db, backup_id)?;
    access_ok(&state, &u, b.server_id)?;
    super::require_capability(&state, &u, b.server_id, Capability::BackupsRead)?;
    let (name, bytes) = backups::download(&state.db, backup_id)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name.replace('"', "")),
        )
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(axum::body::Body::from(bytes))
        .unwrap())
}

pub async fn restore(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let b = models::get_backup(&state.db, backup_id)?;
    let server = access_ok(&state, &u, b.server_id)?;
    super::require_capability(&state, &u, b.server_id, Capability::BackupsWrite)?;
    if server.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &server.node)?;
        let bytes = std::fs::read(&b.path)?;
        let req = crate::node_protocol::RestoreSnapshotRequest {
            archive_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes),
            checksum: b.checksum,
        };
        let _ = state
            .node_client
            .power(&node, &server.uuid, crate::node_protocol::PowerAction::Stop)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        state
            .node_client
            .restore_snapshot(&node, &server.uuid, &req)
            .await?;
    } else {
        state.procs.stop(b.server_id)?;
        backups::restore(&state.db, &state.cfg, backup_id).await?;
    }
    Ok(ok(serde_json::json!({"ok":true,"node":server.node})))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let b = models::get_backup(&state.db, backup_id)?;
    access_ok(&state, &u, b.server_id)?;
    super::require_capability(&state, &u, b.server_id, Capability::BackupsWrite)?;
    backups::delete(&state.db, backup_id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn verify(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let b = models::get_backup(&state.db, backup_id)?;
    access_ok(&state, &u, b.server_id)?;
    super::require_capability(&state, &u, b.server_id, Capability::BackupsRead)?;
    let ok = backups::verify(&state.db, backup_id)?;
    Ok(Json(serde_json::json!({ "ok": ok, "checksum_ok": ok })))
}

#[derive(Deserialize)]
pub struct CleanupReq {
    pub keep: i64,
}

pub async fn cleanup(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CleanupReq>,
) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, server_id)?;
    super::require_capability(&state, &u, server_id, Capability::BackupsWrite)?;
    let removed = backups::cleanup_old(&state.db, &state.cfg, server_id, req.keep).await?;
    Ok(ok(serde_json::json!({ "removed": removed })))
}
