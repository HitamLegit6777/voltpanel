//! Per-server allocation self-service (gap G4): each server's own
//! network/allocation surface — primary, per-port notes, add/detach —
//! exposed as CRUD under `/api/servers/{id}/allocations`.
//!
//! Admin-side allocation management stays in `system.rs`; this module is the
//! server-scoped half, gated by `allocation.read` / `allocation.write`.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::{blocking, Db};
use crate::models::{self, User};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread (see servers.rs
// for the full contract). `blocking` runs the pool-based `models` calls on
// Tokio's blocking pool; the sync helpers below (`allocation_limit`,
// `allocation_count`) are only ever invoked from inside those closures.
// Never hold a pooled connection across an `.await` — split into separate
// blocking units instead.

/// Per-server allocation cap. No such limit existed before this piece; the
/// value is admin-tunable via the settings key
/// `limits.max_allocations_per_server` and clamped to 1..=64 here so a bad
/// setting can never disable the guard.
const DEFAULT_ALLOCATION_LIMIT: i64 = 16;

fn allocation_limit(db: &Db) -> i64 {
    models::get_setting(db, "limits.max_allocations_per_server")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.clamp(1, 64))
        .unwrap_or(DEFAULT_ALLOCATION_LIMIT)
}

async fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, server_id))
        .await
        .map_err(|_| ApiError::not_found("server not found"))?;
    let user = user.clone();
    let sid = s.id;
    if !blocking(state.db.clone(), move |db| models::user_has_server_access(&db, &user, sid)).await?
    {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

/// IDOR guard: the allocation in the path must belong to the server in the
/// path. A foreign id is answered with the same 404 as a missing one, so a
/// caller cannot probe whether another server holds a given allocation id.
async fn owned_allocation(db: &Db, server_id: i64, alloc_id: i64) -> ApiResult<models::Allocation> {
    let a = blocking(db.clone(), move |db| models::get_allocation(&db, alloc_id))
        .await
        .map_err(|_| ApiError::not_found("allocation not found"))?;
    if a.server_id != server_id {
        return Err(ApiError::not_found("allocation not found"));
    }
    Ok(a)
}

fn allocation_count(db: &Db, server_id: i64) -> i64 {
    models::ports_for_server(db, server_id)
        .map(|ports| ports.len() as i64)
        .unwrap_or(0)
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::AllocationRead).await?;
    let allocs = blocking(state.db.clone(), move |db| models::list_allocations(&db, server_id)).await?;
    Ok(data(serde_json::to_value(allocs)?))
}

#[derive(Deserialize)]
pub struct AddReq {
    pub port: i64,
    #[serde(default)]
    pub notes: String,
}

pub async fn add(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<AddReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::AllocationWrite).await?;
    models::validate_port(req.port).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let (limit, count) = blocking(state.db.clone(), move |db| {
        let limit = allocation_limit(&db);
        let count = allocation_count(&db, server_id);
        Ok((limit, count))
    })
    .await?;
    if count >= limit {
        return Err(ApiError::bad_request(format!(
            "allocation limit reached: this server may hold at most {limit} ports; detach one first"
        )));
    }
    if crate::api::system::port_in_use(req.port) {
        return Err(ApiError::bad_request(format!(
            "port {} is already in use on the host",
            req.port
        )));
    }
    let port = req.port;
    let notes = req.notes.trim().to_string();
    let uid = u.id;
    let s_name = s.name.clone();
    let alloc_id = blocking(state.db.clone(), move |db| {
        models::add_allocation(&db, server_id, port, &notes)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let alloc = blocking(state.db.clone(), move |db| {
        let alloc = models::get_allocation(&db, alloc_id)?;
        models::audit_scoped(
            &db,
            Some(uid),
            "allocation_create",
            &s_name,
            "",
            &format!("port={}", port),
            Some(server_id),
        )?;
        Ok(alloc)
    })
    .await?;
    Ok(data(serde_json::to_value(alloc)?))
}

#[derive(Deserialize)]
pub struct PatchReq {
    pub notes: Option<String>,
    /// `true` promotes this allocation to primary (demoting the current one).
    pub primary: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, alloc_id)): Path<(i64, i64)>,
    Json(req): Json<PatchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::AllocationWrite).await?;
    owned_allocation(&state.db, server_id, alloc_id).await?;
    let notes = req.notes.as_deref().map(str::to_string);
    let primary = req.primary == Some(true);
    let uid = u.id;
    let s_name = s.name.clone();
    let a = blocking(state.db.clone(), move |db| {
        if let Some(notes) = &notes {
            models::set_allocation_notes(&db, alloc_id, notes)?;
        }
        if primary {
            models::set_primary_allocation(&db, alloc_id)?;
        }
        let a = models::get_allocation(&db, alloc_id)?;
        models::audit_scoped(
            &db,
            Some(uid),
            if primary {
                "allocation_promote"
            } else {
                "allocation_update"
            },
            &s_name,
            "",
            &format!("port={}", a.port),
            Some(server_id),
        )?;
        Ok(a)
    })
    .await?;
    Ok(data(serde_json::to_value(a)?))
}

pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, alloc_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::AllocationWrite).await?;
    let a = owned_allocation(&state.db, server_id, alloc_id).await?;
    let uid = u.id;
    let s_name = s.name.clone();
    let port = a.port;
    blocking(state.db.clone(), move |db| models::remove_allocation(&db, alloc_id))
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            "allocation_delete",
            &s_name,
            "",
            &format!("port={}", port),
            Some(server_id),
        )
    })
    .await?;
    Ok(ok(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, patch};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

    // Ports for tests: high range so a busy CI host is unlikely to have them
    // bound (the add handler rejects ports already in use on the host).
    const P1: i64 = 51_001;
    const P2: i64 = 51_002;
    const P3: i64 = 51_003;

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        cfg.paths.datalab_dir = tmp.path().join("datalab");
        let hub = Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(crate::services::proc::ProcManager::new(
            db.clone(),
            hub.clone(),
            cfg.paths.datalab_dir.clone(),
        ));
        let watcher_engine = Arc::new(crate::services::watcher::WatcherEngine::new(
            db.clone(),
            Arc::new(crate::services::proc::Notifier::new()),
            Arc::downgrade(&hub),
            procs.clone(),
            Arc::new(crate::services::node::NodeClient::new().unwrap()),
            tokio::runtime::Handle::current(),
        ));
        let state = AppState {
            db,
            cfg,
            procs,
            hub,
            notifier: Arc::new(crate::services::proc::Notifier::new()),
            monitor: Arc::new(crate::services::Monitor::new()),
            node_client: Arc::new(crate::services::node::NodeClient::new().unwrap()),
            node_nonces: Arc::new(crate::services::node::NonceCache::default()),
            running: Arc::new(AtomicBool::new(true)),
            watcher_engine,
        };
        (tmp, state)
    }

    /// A root admin user with a live session cookie plus a server it owns.
    fn seed(state: &AppState, uuid: &str) -> (i64, String) {
        let user_id = models::create_user(
            &state.db,
            &format!("u-{uuid}"),
            &format!("{uuid}@x.io"),
            "h",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let blueprint_id = models::create_blueprint(
            &state.db,
            &format!("bp-{uuid}"),
            "bp",
            "",
            "a",
            "game",
            "generic",
            "echo",
            None,
            None,
            &[],
            "stop",
        )
        .unwrap();
        let server_id = models::create_server(
            &state.db, uuid, "srv", user_id, blueprint_id, "generic", "echo", 512, 1024, 100, 0,
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db, &state.cfg, user_id, "test-agent", "127.0.0.1", false,
        )
        .unwrap();
        (server_id, format!("vp_session={raw}"))
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/servers/:id/allocations", get(list).post(add))
            .route(
                "/api/servers/:id/allocations/:alloc_id",
                patch(update).delete(remove),
            )
            .with_state(state)
    }

    async fn request(
        state: AppState,
        method: &str,
        uri: &str,
        cookie: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri).header("cookie", cookie);
        let payload = body.map(|b| b.to_string());
        if payload.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let req = builder.body(Body::from(payload.unwrap_or_default())).unwrap();
        let response = router(state).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null))
        };
        (status, value)
    }

    #[tokio::test]
    async fn cross_server_alloc_id_rejected_and_not_mutated() {
        let (_tmp, state) = test_state();
        let (server_a, cookie) = seed(&state, "uuid-a");
        let (server_b, _cookie_b) = seed(&state, "uuid-b");
        // A holds P1 (primary) + P2; B holds P3.
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_a}/allocations"),
            &cookie,
            Some(json!({ "port": P1 })),
        )
        .await;
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_a}/allocations"),
            &cookie,
            Some(json!({ "port": P2 })),
        )
        .await;
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_b}/allocations"),
            &cookie,
            Some(json!({ "port": P3 })),
        )
        .await;
        let a_alloc = models::list_allocations(&state.db, server_a).unwrap();
        let foreign = a_alloc[0].id; // an allocation owned by server A

        // PATCH on B's path with A's allocation id must 404 and touch nothing.
        let (status, body) = request(
            state.clone(),
            "PATCH",
            &format!("/api/servers/{server_b}/allocations/{foreign}"),
            &cookie,
            Some(json!({ "primary": true })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].is_string());

        // DELETE on B's path with A's allocation id must 404 and touch nothing.
        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_b}/allocations/{foreign}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let after = models::list_allocations(&state.db, server_a).unwrap();
        assert_eq!(after.len(), 2, "server A lost an allocation to a foreign path");
        assert!(after.iter().any(|a| a.id == foreign), "foreign id was mutated");
        let b_after = models::list_allocations(&state.db, server_b).unwrap();
        assert_eq!(b_after.len(), 1, "server B gained an allocation");
        // None of A's allocations were promoted by the attempted PATCH.
        assert!(after.iter().filter(|a| a.is_primary).count() == 1);
    }

    #[tokio::test]
    async fn per_server_limit_enforced() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-limit");
        models::set_setting(&state.db, "limits.max_allocations_per_server", "1").unwrap();

        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P1 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P2 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("limit"));

        let allocs = models::list_allocations(&state.db, server_id).unwrap();
        assert_eq!(allocs.len(), 1, "over-limit allocation was attached");
    }

    #[tokio::test]
    async fn detaching_primary_rejected() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-primary");
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P1 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let primary_id = body["data"]["id"].as_i64().unwrap();
        assert!(body["data"]["is_primary"].as_bool().unwrap());

        let (status, body) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_id}/allocations/{primary_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("primary"),
            "error must tell the operator to promote another port first: {}",
            body["error"]
        );

        let allocs = models::list_allocations(&state.db, server_id).unwrap();
        assert_eq!(allocs.len(), 1, "primary was detached despite the refusal");
    }

    #[tokio::test]
    async fn promote_then_detach_succeeds() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-promote");
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P1, "notes": "old primary" })),
        )
        .await;
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P2, "notes": "new primary" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let alloc2_id = body["data"]["id"].as_i64().unwrap();
        assert!(!body["data"]["is_primary"].as_bool().unwrap(), "first allocation stays primary");

        // Promote P2 and give P1 a fresh note in the same PATCH.
        let (status, body) = request(
            state.clone(),
            "PATCH",
            &format!("/api/servers/{server_id}/allocations/{alloc2_id}"),
            &cookie,
            Some(json!({ "primary": true, "notes": "main port now" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["data"]["is_primary"].as_bool().unwrap());
        assert_eq!(body["data"]["notes"], "main port now");

        let allocs = models::list_allocations(&state.db, server_id).unwrap();
        assert_eq!(allocs.len(), 2);
        assert_eq!(allocs[0].id, alloc2_id, "promoted allocation sorts first");
        assert!(allocs[0].is_primary);
        let old_primary_id = allocs[1].id;

        // Now the old primary can be detached; the promoted one cannot.
        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_id}/allocations/{old_primary_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_id}/allocations/{alloc2_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("primary"));

        let allocs = models::list_allocations(&state.db, server_id).unwrap();
        assert_eq!(allocs.len(), 1);
        assert!(allocs[0].is_primary);
        // `servers.port` follows the promoted allocation.
        let server = models::get_server(&state.db, server_id).unwrap();
        assert_eq!(server.port, Some(P2));
    }

    #[tokio::test]
    async fn list_flags_primary_and_reports_notes() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-list");
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P1, "notes": "web" })),
        )
        .await;
        let (_, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": P2 })),
        )
        .await;
        let (status, body) = request(
            state.clone(),
            "GET",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert!(data[0]["is_primary"].as_bool().unwrap(), "primary sorts first");
        assert_eq!(data[0]["port"].as_i64().unwrap(), P1);
        assert_eq!(data[0]["notes"].as_str().unwrap(), "web");
        assert!(data[0]["id"].is_i64() && data[0]["server_id"].is_i64());
    }

    #[tokio::test]
    async fn unauthenticated_request_rejected() {
        let (_tmp, state) = test_state();
        let (server_id, _) = seed(&state, "uuid-auth");
        let (status, _) = request(
            state,
            "GET",
            &format!("/api/servers/{server_id}/allocations"),
            "",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Workloads run without `CAP_NET_BIND_SERVICE`, so a privileged port
    /// would be accepted here and then fail at bind time inside the sandbox.
    #[tokio::test]
    async fn privileged_ports_are_rejected_at_the_boundary() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-priv");
        for port in [0, 22, 1023] {
            let (status, body) = request(
                state.clone(),
                "POST",
                &format!("/api/servers/{server_id}/allocations"),
                &cookie,
                Some(json!({ "port": port })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "port {port} must be denied");
            assert!(body["error"].as_str().unwrap().contains("1024"));
        }
        // 1024 is the first bindable port and must still be accepted.
        let (status, _) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/allocations"),
            &cookie,
            Some(json!({ "port": 1024 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}