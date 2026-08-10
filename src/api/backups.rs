//! Backup endpoints.
use super::{client_ip, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, User};
use crate::services::backups;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use anyhow::Context;
use std::net::SocketAddr;

// ---- DB execution off the async worker ----
//
// Pool-based `models`/`nodes`/`blueprint` calls and the sync `services::backups`
// helpers must not run on a Tokio worker thread; `blocking(...)` runs them on
// Tokio's blocking pool (see src/api/servers.rs for the full contract). The
// async `services::backups` fns (create/restore/delete/cleanup/mirror_sync)
// already run their heavy work on the blocking pool internally, so they are
// awaited directly. This module owns no direct SQL, so `Db::call` is unused.

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

fn require_backups_enabled(cfg: &crate::config::Config) -> ApiResult<()> {
    if cfg.features.enable_backups {
        Ok(())
    } else {
        Err(ApiError::not_found("backups are disabled"))
    }
}

/// Reject an ignore list for REMOTE workspaces. The node archives its own
/// root (`voltd`'s `snapshot` walks the whole server dir; the wire request is
/// `GET /v1/servers/{uuid}/snapshot` with no body and no ignore parameter),
/// so patterns cannot be honored remotely — reject rather than silently
/// capture paths the operator asked to exclude. Honoring them requires a
/// protocol change: see `local://report-files.md`.
fn check_remote_ignore(node: &str, ignore: &str) -> ApiResult<()> {
    if node != "local" && !crate::services::files::IgnoreList::parse(ignore)?.is_empty() {
        return Err(ApiError::bad_request(
            "ignore patterns are only supported for local workspaces",
        ));
    }
    Ok(())
}

/// Strip characters that must not appear in a Content-Disposition header
/// value (CR/LF would inject header lines and panic the builder).
fn sanitize_attachment(name: &str) -> String {
    name.chars()
        .filter(|&c| !c.is_control() && c != '"' && c != '\\')
        .collect()
}

/// Stop a remote server and wait until the daemon reports no PID, so create
/// and restore never act mid-shutdown. The stop/poll/timeout semantics live
/// once on `NodeClient::stop_and_wait` (shared with the scheduler); a server
/// that is still alive when the wait budget runs out is an error, and the
/// caller must not proceed to snapshot.
async fn stop_and_wait(state: &AppState, node: &crate::nodes::Node, uuid: &str) -> ApiResult<()> {
    state
        .node_client
        .stop_and_wait(node, uuid, 100, std::time::Duration::from_millis(100))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::BackupsRead).await?;
    let backups = blocking(state.db.clone(), move |db| {
        models::list_backups(&db, server_id)
    })
    .await?;
    // Mirror health is panel-global (the mirror is one store for all servers)
    // but surfaced on every backups page. The mirror path itself is never
    // serialized — like Backup.path, it would leak the host layout.
    Ok(Json(serde_json::json!({
        "data": serde_json::to_value(backups)?,
        "mirror": {
            "enabled": state.cfg.backups.mirror.enabled,
            "status": backups::mirror_status(&state.cfg),
        },
    })))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: Option<String>,
    /// Newline-separated glob patterns excluded from the archive.
    #[serde(default)]
    pub ignore: String,
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let server = access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::BackupsWrite).await?;
    let name = req
        .name
        .unwrap_or_else(|| format!("backup-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));
    let ignore = req.ignore;
    check_remote_ignore(&server.node, &ignore)?;
    if server.node != "local" {
        // Same per-server serialization as the local path (services::backups
        // locks create/restore/cleanup/delete internally); this branch bypasses
        // those fns, so it takes the shared lock itself.
        let _guard = crate::services::backups::server_op_lock(server.id).await;
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let was_running = server.status == "running";
        if was_running {
            stop_and_wait(&state, &node, &server.uuid).await?;
        }
        let snapshot_result = state.node_client.snapshot_stream(&node, &server.uuid).await;
        // Always try to restart a server we stopped, and never swallow the
        // restart result: a backup that leaves the server off must fail the
        // run, not report success.
        let restart_result = if was_running {
            Some(
                state
                    .node_client
                    .power(
                        &node,
                        &server.uuid,
                        crate::node_protocol::PowerAction::Start,
                    )
                    .await,
            )
        } else {
            None
        };
        let snapshot = snapshot_result?;
        if let Some(res) = restart_result {
            res?;
        }
        // The streaming client enforces the archive bound itself: the raw
        // streaming receiver caps the body at MAX_EXTRACT_TOTAL_BYTES (plus
        // the fixed signature footer), and the legacy base64-envelope
        // fallback at MAX_REMOTE_ARCHIVE_B64_CHARS — so a buggy or hostile
        // node can never force a multi-GB allocation here. Re-check the
        // declared size as defense in depth; the client already guarantees it.
        if snapshot.size_bytes > crate::services::backups::MAX_EXTRACT_TOTAL_BYTES {
            return Err(ApiError::bad_request(format!(
                "remote snapshot archive too large ({} bytes, max {}); refusing",
                snapshot.size_bytes,
                crate::services::backups::MAX_EXTRACT_TOTAL_BYTES
            )));
        }
        let uuid = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(&state.cfg.paths.backups_dir)?;
        let path = state.cfg.paths.backups_dir.join(format!("{uuid}.tar.gz"));
        // The archive arrives as a temp file on disk (raw streamed body, or
        // the decoded legacy envelope — identical shape). Move it into place
        // on the blocking pool: a multi-GB archive must never stall tokio
        // workers. The temp file lives for the copy because `snapshot` is
        // moved into the closure.
        let temp_path = snapshot.archive.path().to_path_buf();
        let copy_path = path.clone();
        let (size_bytes, checksum) = tokio::task::spawn_blocking(move || {
            std::fs::copy(&temp_path, &copy_path)?;
            Ok::<_, anyhow::Error>((snapshot.size_bytes, snapshot.checksum))
        })
        .await
        .context("remote backup archive copy")??;
        let backup_name = name.clone();
        let path_str = path.to_string_lossy().into_owned();
        let size_db = size_bytes as i64;
        let checksum_db = checksum.clone();
        let sid = server.id;
        let id = blocking(state.db.clone(), move |db| {
            models::create_backup(
                &db,
                &uuid,
                sid,
                &backup_name,
                &path_str,
                size_db,
                &checksum_db,
                "tar.gz",
                "",
            )
        })
        .await?;
        // Mirror the remote archive too: it lands in backups_dir like any
        // local backup, so it must reach the mirror the same way. The op
        // lock is held (taken above); failures are warn-only. The copy +
        // retention trim are heavy blocking fs work, so run them on the
        // blocking pool as the local path does — a large archive must never
        // stall tokio workers. A panicked worker is warn-only too: the
        // primary archive is already durable, mirror is best-effort.
        let mirror_cfg = state.cfg.clone();
        let mirror_server = server.clone();
        let mirror_path = path.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || {
            crate::services::backups::mirror_archive(&mirror_cfg, &mirror_server, &mirror_path);
        })
        .await
        {
            tracing::warn!(server_id = server.id, "mirror worker panicked: {e}");
        }
        return Ok(Json(
            serde_json::json!({"id":id,"size":size_bytes,"checksum":checksum,"remote":true}),
        ));
    }
    let was_running = server.status == "running";
    // Stop a live local server while its directory is archived, matching the
    // remote path: a running workload mutates files mid-tar, which yields an
    // internally inconsistent archive. The server comes back afterwards even
    // when the backup itself fails, so a failed backup never leaves a running
    // server parked (mirrors the restore handler).
    if was_running {
        state.procs.stop(server_id)?;
        // stop() only signals SIGTERM; wait (bounded) for the reaper to clear
        // the pid before archiving, mirroring the remote stop_and_wait.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        loop {
            let gone = match state.procs.state(server_id) {
                None => true,
                Some(ps) => ps.pid.lock().is_none(),
            };
            if gone {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server did not stop before backup",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let result = backups::create(&state.db, &state.cfg, server_id, &name, &ignore).await;
    if was_running {
        if let Ok(srv) = blocking(state.db.clone(), move |db| {
            models::get_server(&db, server_id)
        })
        .await
        {
            let srv2 = srv.clone();
            if let Ok((cmd, env)) = blocking(state.db.clone(), move |db| {
                Ok((
                    crate::services::blueprint::resolve_startup(&db, &srv2)?,
                    crate::services::blueprint::env_for_server(&db, &srv2),
                ))
            })
            .await
            {
                let _ = state.procs.start(&srv, &cmd, &env, state.notifier.clone());
            }
        }
    }
    let (id, size, checksum) = result?;
    Ok(Json(
        serde_json::json!({"id":id,"size":size,"checksum":checksum,"remote":false}),
    ))
}

pub async fn download(
    State(state): State<AppState>,
    user: AuthUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(backup_id): Path<i64>,
) -> ApiResult<Response> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let b = blocking(state.db.clone(), move |db| {
        models::get_backup(&db, backup_id)
    })
    .await?;
    access_ok(&state, u, b.server_id).await?;
    super::require_capability(&state, &user, b.server_id, Capability::BackupsRead).await?;
    // Downloads move data off the panel: record who pulled which backup, from
    // where, and for which server.
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let uid = u.id;
    let sid = b.server_id;
    let bname = b.name.clone();
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            "backup_download",
            &bname,
            &ip,
            "",
            Some(sid),
        )
    })
    .await?;
    let (name, file, size) = blocking(state.db.clone(), move |db| {
        backups::download(&db, backup_id)
    })
    .await?;
    let content_type = if name.ends_with(".tar.gz") {
        "application/gzip"
    } else {
        "application/zip"
    };
    // Stream the archive from disk with a Content-Length, so multi-GB
    // backups never materialize in RAM.
    let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", sanitize_attachment(&name)),
        )
        .header(header::CONTENT_LENGTH, size.to_string())
        .body(axum::body::Body::from_stream(stream))
        .unwrap())
}

pub async fn restore(
    State(state): State<AppState>,
    user: AuthUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let b = blocking(state.db.clone(), move |db| {
        models::get_backup(&db, backup_id)
    })
    .await?;
    let server = access_ok(&state, u, b.server_id).await?;
    super::require_capability(&state, &user, b.server_id, Capability::BackupsWrite).await?;
    // Never restore a backup whose on-disk bytes no longer match the recorded
    // checksum (corrupted or tampered archive).
    if !blocking(state.db.clone(), move |db| backups::verify(&db, backup_id)).await? {
        return Err(ApiError::bad_request(
            "backup checksum mismatch; refusing restore",
        ));
    }
    // A restore overwrites server state — record actor, IP, and target server
    // before the operation begins.
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let uid = u.id;
    let sid = b.server_id;
    let bname = b.name.clone();
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(&db, Some(uid), "backup_restore", &bname, &ip, "", Some(sid))
    })
    .await?;
    let was_running = server.status == "running";
    if server.node != "local" {
        // Same per-server serialization as the local path; this branch bypasses
        // services::backups::restore, so it takes the shared lock itself.
        let _guard = crate::services::backups::server_op_lock(b.server_id).await;
        let node_name = server.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await?;
        let mut stopped = false;
        if was_running {
            // Real stopped-state wait, not a fixed sleep: snapshotting a
            // half-stopped server captures a corrupt state.
            stop_and_wait(&state, &node, &server.uuid).await?;
            stopped = true;
        }
        // The client streams the archive straight from disk (raw body), so
        // the panel never materializes it in RAM; only the legacy fallback
        // for pre-streaming agents base64-wraps it in memory. Refuse anything
        // beyond the local extraction total cap up front, so that fallback
        // can never force a matching multi-GB allocation.
        if std::fs::metadata(&b.path)?.len() > crate::services::backups::MAX_EXTRACT_TOTAL_BYTES {
            return Err(ApiError::bad_request(
                "backup archive too large for remote restore; refusing",
            ));
        }
        let result = state
            .node_client
            .restore_snapshot_stream(&node, &server.uuid, std::path::Path::new(&b.path))
            .await;
        // Restore the prior run state even when the restore itself fails, so
        // a failed restore never leaves a running server parked.
        if stopped {
            let _ = state
                .node_client
                .power(
                    &node,
                    &server.uuid,
                    crate::node_protocol::PowerAction::Start,
                )
                .await;
        }
        result?;
    } else {
        state.procs.stop(b.server_id)?;
        let result = backups::restore(&state.db, &state.cfg, backup_id).await;
        // Restore the prior run state even when the restore itself fails: a
        // server that was running comes back up, best-effort.
        if was_running {
            let sid = b.server_id;
            if let Ok(srv) =
                blocking(state.db.clone(), move |db| models::get_server(&db, sid)).await
            {
                let srv2 = srv.clone();
                if let Ok((cmd, env)) = blocking(state.db.clone(), move |db| {
                    Ok((
                        crate::services::blueprint::resolve_startup(&db, &srv2)?,
                        crate::services::blueprint::env_for_server(&db, &srv2),
                    ))
                })
                .await
                {
                    let _ = state.procs.start(&srv, &cmd, &env, state.notifier.clone());
                }
            }
        }
        result?;
    }
    Ok(ok(serde_json::json!({"ok":true,"node":server.node})))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let b = blocking(state.db.clone(), move |db| {
        models::get_backup(&db, backup_id)
    })
    .await?;
    access_ok(&state, u, b.server_id).await?;
    super::require_capability(&state, &user, b.server_id, Capability::BackupsWrite).await?;
    // A locked backup is operator-pinned; refusing here keeps the failure a
    // clear 409 instead of the generic 500 the service-level bail maps to.
    if b.is_locked {
        return Err(ApiError::conflict(
            "backup is locked; unlock it before deleting",
        ));
    }
    backups::delete(&state.db, backup_id).await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn verify(
    State(state): State<AppState>,
    user: AuthUser,
    Path(backup_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let b = blocking(state.db.clone(), move |db| {
        models::get_backup(&db, backup_id)
    })
    .await?;
    access_ok(&state, u, b.server_id).await?;
    super::require_capability(&state, &user, b.server_id, Capability::BackupsRead).await?;
    let ok = blocking(state.db.clone(), move |db| backups::verify(&db, backup_id)).await?;
    Ok(Json(serde_json::json!({ "ok": ok, "checksum_ok": ok })))
}

#[derive(Deserialize)]
pub struct LockReq {
    pub locked: bool,
}

/// Pin or unpin a backup. A locked backup is skipped by rotation and refuses
/// deletion until it is unlocked again.
pub async fn lock(
    State(state): State<AppState>,
    user: AuthUser,
    Path(backup_id): Path<i64>,
    Json(req): Json<LockReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    let b = blocking(state.db.clone(), move |db| {
        models::get_backup(&db, backup_id)
    })
    .await?;
    access_ok(&state, u, b.server_id).await?;
    super::require_capability(&state, &user, b.server_id, Capability::BackupsWrite).await?;
    let locked = req.locked;
    blocking(state.db.clone(), move |db| {
        models::set_backup_locked(&db, backup_id, locked)
    })
    .await?;
    let uid = u.id;
    let sid = b.server_id;
    let bname = b.name.clone();
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            if locked {
                "backup_lock"
            } else {
                "backup_unlock"
            },
            &bname,
            "",
            "",
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "locked": req.locked })))
}

#[derive(Deserialize)]
pub struct CleanupReq {
    pub keep: i64,
}

pub async fn cleanup(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CleanupReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    require_backups_enabled(&state.cfg)?;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::BackupsWrite).await?;
    if req.keep < 0 {
        return Err(ApiError::bad_request("keep must be zero or greater"));
    }
    let removed = backups::cleanup_old(&state.db, &state.cfg, server_id, req.keep).await?;
    Ok(ok(serde_json::json!({ "removed": removed })))
}

/// Admin: re-sync the offsite mirror from the primary backup store — copy
/// every archive the DB records that is missing from the mirror, then
/// enforce mirror retention. Idempotent; audited as `backup_mirror_sync`.
pub async fn mirror_sync(
    State(state): State<AppState>,
    a: AdminUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    require_backups_enabled(&state.cfg)?;
    if !state.cfg.backups.mirror.enabled {
        return Err(ApiError::bad_request(
            "backups.mirror is not enabled; enable it in config.toml first",
        ));
    }
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let admin_id = a.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(admin_id),
            "backup_mirror_sync",
            "mirror",
            &ip,
            "",
            None,
        )
    })
    .await?;
    let report = backups::mirror_sync(&state.db, &state.cfg).await?;
    Ok(ok(serde_json::json!({
        "ok": true,
        "mirror_status": backups::mirror_status(&state.cfg),
        "servers": report.servers,
        "copied": report.copied,
        "removed": report.removed,
        "failed": report.failed,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_backups_enabled_gates_every_handler() {
        let mut cfg = crate::config::Config::default();
        cfg.features.enable_backups = false;
        assert!(require_backups_enabled(&cfg).is_err());
        cfg.features.enable_backups = true;
        assert!(require_backups_enabled(&cfg).is_ok());
    }

    #[test]
    fn sanitize_attachment_strips_header_unsafe_chars() {
        assert_eq!(sanitize_attachment("backup-1.tar.gz"), "backup-1.tar.gz");
        assert_eq!(sanitize_attachment("a\r\nX-Injected: 1"), "aX-Injected: 1");
        assert_eq!(sanitize_attachment("a\"b"), "ab");
    }

    #[test]
    fn remote_ignore_rejected_local_allowed() {
        assert!(check_remote_ignore("local", "").is_ok());
        assert!(check_remote_ignore("local", "*.log").is_ok());
        assert!(check_remote_ignore("node1", "").is_ok());
        // local nodes skip the parse here (services::backups::create validates
        // patterns when it archives); remote nodes parse eagerly and reject
        assert!(check_remote_ignore("local", "abc[def").is_ok());
        assert!(check_remote_ignore("node1", "abc[def").is_err());
    }
}
