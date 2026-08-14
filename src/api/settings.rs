//! Panel settings endpoints + audit logs.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::models;
use crate::db::blocking;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive};
use axum::response::Sse;
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread. Module-owned
// SQL rides `Db::call(|conn| ...)`; pool-based `models` functions (they do
// their own `pool.get()` and cannot run inside a `db.call` closure without a
// nested checkout) ride `blocking(...)` on Tokio's blocking pool. One rule:
// never hold a pooled connection across an `.await`.

/// Public panel settings (no secrets).
pub async fn public(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "instance_name": state.cfg.general.instance_name,
        "locale": state.cfg.general.locale,
        "features": {
            "backups": state.cfg.features.enable_backups,
            "databases": state.cfg.features.enable_databases,
            "schedules": state.cfg.features.enable_schedules,
            "api_keys": state.cfg.features.enable_api_keys,
            "2fa": state.cfg.features.enable_2fa,
            "websites": state.cfg.features.enable_websites,
        },
        "limits": {
            "max_memory_mb": state.cfg.limits.max_memory_mb,
            "max_servers_per_user": state.cfg.limits.max_servers_per_user,
        }
    })))
}

#[derive(Deserialize)]
pub struct LimitsReq {
    pub default_memory_mb: Option<u64>,
    pub default_disk_mb: Option<u64>,
    pub default_cpu_percent: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub max_servers_per_user: Option<u64>,
}

/// Update node-wide resource limits at runtime (persisted in settings table).
pub async fn update_limits(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<LimitsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stored = blocking(state.db.clone(), move |db| {
        Ok([
            models::get_setting(&db, "limits.default_memory_mb").ok().flatten(),
            models::get_setting(&db, "limits.default_disk_mb").ok().flatten(),
            models::get_setting(&db, "limits.default_cpu_percent").ok().flatten(),
            models::get_setting(&db, "limits.max_memory_mb").ok().flatten(),
            models::get_setting(&db, "limits.max_servers_per_user").ok().flatten(),
        ])
    })
    .await
    .unwrap_or_default();
    let [s_mem, s_disk, s_cpu, s_max_mem, s_max_servers] = stored;
    let current = |v: Option<String>, fallback: u64| -> u64 {
        v.and_then(|value| value.parse().ok()).unwrap_or(fallback)
    };
    let default_memory = req
        .default_memory_mb
        .unwrap_or_else(|| current(s_mem, state.cfg.limits.default_memory_mb));
    let default_disk = req
        .default_disk_mb
        .unwrap_or_else(|| current(s_disk, state.cfg.limits.default_disk_mb));
    let default_cpu = req
        .default_cpu_percent
        .unwrap_or_else(|| current(s_cpu, state.cfg.limits.default_cpu_percent));
    let max_memory = req
        .max_memory_mb
        .unwrap_or_else(|| current(s_max_mem, state.cfg.limits.max_memory_mb));
    let max_servers = req
        .max_servers_per_user
        .unwrap_or_else(|| current(s_max_servers, state.cfg.limits.max_servers_per_user));
    if default_memory == 0
        || default_memory > max_memory
        || default_disk == 0
        || !(1..=10_000).contains(&default_cpu)
        || max_servers == 0
    {
        return Err(ApiError::bad_request("invalid resource limit combination"));
    }
    let (dm, dd, dc, mm, ms) = (
        req.default_memory_mb,
        req.default_disk_mb,
        req.default_cpu_percent,
        req.max_memory_mb,
        req.max_servers_per_user,
    );
    blocking(state.db.clone(), move |db| {
        if let Some(v) = dm {
            models::set_setting(&db, "limits.default_memory_mb", &v.to_string())?;
        }
        if let Some(v) = dd {
            models::set_setting(&db, "limits.default_disk_mb", &v.to_string())?;
        }
        if let Some(v) = dc {
            models::set_setting(&db, "limits.default_cpu_percent", &v.to_string())?;
        }
        if let Some(v) = mm {
            models::set_setting(&db, "limits.max_memory_mb", &v.to_string())?;
        }
        if let Some(v) = ms {
            models::set_setting(&db, "limits.max_servers_per_user", &v.to_string())?;
        }
        Ok(())
    })
    .await?;
    Ok(ok(json!({ "ok": true })))
}

// ---------------- Audit logs ----------------

pub async fn audit_logs(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let logs = blocking(state.db.clone(), move |db| {
        models::list_audit_logs(&db, 500)
    })
    .await?;
    Ok(data(serde_json::to_value(logs)?))
}

// ---------------- Notifications ----------------

/// List the notification ring (newest last) plus the unread count, so the
/// shell badge and drawer render from a single round-trip. Entry fields:
/// `id`, `level`, `title`, `message`, `server_id`, `created_at`, `read_at`
/// (null until marked), `link` (optional panel deep link).
pub async fn notifications(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let entries = state.notifier.history();
    let unread_count = entries.iter().filter(|n| n.read_at.is_none()).count();
    Ok(data(json!({ "entries": entries, "unread_count": unread_count })))
}

pub async fn notifications_read(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    if !state.notifier.mark_read(id) {
        return Err(ApiError::not_found(format!("notification {id} not found")));
    }
    Ok(ok(json!({ "ok": true })))
}

pub async fn notifications_clear(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    state.notifier.clear();
    Ok(ok(json!({ "ok": true })))
}

/// Live notification feed over SSE (`notification` events, each carrying the
/// full entry JSON and an `id:` equal to the notification id). Ends on
/// graceful shutdown — the process-wide `running` flag — like the console
/// stream, so the client's EventSource auto-reconnect restores the feed.
pub async fn notifications_stream(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Sse<Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>>> {
    let mut rx = state.notifier.subscribe();
    let running = state.running.clone();
    let stream = async_stream::stream! {
        loop {
            let recv = tokio::select! {
                recv = rx.recv() => recv,
                _ = wait_shutdown(&running) => break,
            };
            match recv {
                Some(n) => {
                    let payload = serde_json::to_string(&n).unwrap_or_default();
                    yield Ok(Event::default()
                        .event("notification")
                        .id(n.id.to_string())
                        .data(payload));
                }
                None => break, // sender dropped: notifier is gone
            }
        }
    };
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(stream);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

/// Complete once graceful shutdown begins: the process-wide `running` flag
/// clears, polled every 100 ms. Long-lived SSE streams must not hold axum's
/// drain open, so when this completes the stream simply ends.
async fn wait_shutdown(running: &Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------- VoltSpec Registry signing ----------------

/// Signing posture of the VoltSpec registry. The public key is exactly that —
/// public — so any authenticated user may read it; only the key itself is
/// private to the settings table.
pub async fn registry_signing_status(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    use crate::services::blueprint as bp;
    let seed = blocking(state.db.clone(), move |db| {
        models::get_setting(&db, "registry.signing_key")
    })
    .await?
    .filter(|s| !s.trim().is_empty());
    let Some(seed) = seed else {
        return Ok(Json(json!({
            "enabled": false,
            "public_key": serde_json::Value::Null,
            "fingerprint": serde_json::Value::Null,
        })));
    };
    let key = bp::signing_key_from_hex(&seed).map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("registry signing key is misconfigured: {e}"),
        )
    })?;
    let pk = bp::public_key_hex(&key);
    Ok(Json(json!({
        "enabled": true,
        "public_key": pk,
        "fingerprint": bp::public_key_fingerprint(&pk),
    })))
}

#[derive(Deserialize)]
pub struct RegistrySigningKeyReq {
    /// Hex-encoded 32-byte ed25519 seed to set. `null` generates a fresh key;
    /// an empty string disables signing.
    pub key: Option<String>,
}

/// POST /api/settings/registry/signing-key — set, generate, or clear the
/// publisher signing key. The key is stored in the settings table (never in
/// config), so it can be rotated at runtime without a restart.
///
/// When the request generates a fresh key (`key: null`), the seed is returned
/// in the response exactly once; it is never persisted anywhere but the
/// settings table and is never echoed again. Subsequent status calls return
/// only the public key and fingerprint.
pub async fn registry_set_signing_key(
    State(state): State<AppState>,
    a: AdminUser,
    Json(req): Json<RegistrySigningKeyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    use crate::services::blueprint as bp;
    let (seed, generated, cleared) = match req.key {
        Some(k) if k.trim().is_empty() => (String::new(), false, true),
        Some(k) => {
            let k = k.trim().to_string();
            bp::signing_key_from_hex(&k).map_err(|e| ApiError::bad_request(e.to_string()))?;
            (k, false, false)
        }
        None => {
            let mut csprng = rand::rngs::OsRng;
            (
                hex::encode(ed25519_dalek::SigningKey::generate(&mut csprng).to_bytes()),
                true,
                false,
            )
        }
    };
    let seed_store = seed.clone();
    let admin_id = a.0.id;
    blocking(state.db.clone(), move |db| {
        models::set_setting(&db, "registry.signing_key", &seed_store)?;
        models::audit(
            &db,
            Some(admin_id),
            "registry.signing_key",
            if cleared { "cleared" } else { "set" },
            "",
            "",
        )?;
        Ok(())
    })
    .await?;
    let key = bp::signing_key_from_hex(&seed).ok();
    let pk = key.as_ref().map(bp::public_key_hex);
    let mut out = json!({
        "enabled": !cleared,
        "generated": generated,
        "cleared": cleared,
        "public_key": pk,
        "fingerprint": pk.as_deref().map(bp::public_key_fingerprint).unwrap_or_default(),
    });
    if generated {
        // One-shot backup channel: the seed is private material that exists
        // only in the settings table, so this response is the sole chance to
        // capture it. Never include it on set/clear or status paths.
        out["seed"] = json!(seed);
        out["store_this_seed_now"] =
            json!("Store this seed now; it will never be returned again.");
    }
    Ok(Json(out))
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

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

    /// A root-admin user with a live session cookie.
    fn seed_admin(state: &AppState) -> (i64, String) {
        let user_id =
            models::create_user(&state.db, "admin", "admin@x.io", "h", true, "en", "dark").unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            user_id,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        (user_id, format!("vp_session={raw}"))
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/settings/registry", get(registry_signing_status))
            .route(
                "/api/settings/registry/signing-key",
                post(registry_set_signing_key),
            )
            .route("/api/notifications", get(notifications))
            .route("/api/notifications/stream", get(notifications_stream))
            .route(
                "/api/notifications/:id/read",
                post(notifications_read),
            )
            .route("/api/notifications/clear", post(notifications_clear))
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

    fn is_32byte_hex(s: &str) -> bool {
        hex::decode(s).map(|b| b.len() == 32).unwrap_or(false)
    }

    #[tokio::test]
    async fn generate_returns_seed_exactly_once_then_status_omits_it() {
        let (_tmp, state) = test_state();
        let (_admin_id, cookie) = seed_admin(&state);

        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/settings/registry/signing-key",
            &cookie,
            Some(json!({ "key": null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["generated"], true);
        assert_eq!(body["cleared"], false);
        assert_eq!(body["enabled"], true);
        let seed = body["seed"].as_str().expect("seed returned on generate");
        assert!(is_32byte_hex(seed), "seed must be 32 bytes of hex");
        assert!(
            body["store_this_seed_now"]
                .as_str()
                .is_some_and(|w| !w.is_empty()),
            "prominent one-shot warning field present"
        );
        assert!(
            body["public_key"].as_str().is_some_and(|p| !p.is_empty()),
            "public key still returned alongside the seed"
        );

        // Status after generate: public key + fingerprint only, never the seed.
        let (status, body) = request(
            state.clone(),
            "GET",
            "/api/settings/registry",
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], true);
        assert!(body.get("seed").is_none(), "status must never expose the seed");
        assert!(
            body.get("store_this_seed_now").is_none(),
            "warning is generate-response-only"
        );
        assert!(
            body["fingerprint"].as_str().is_some_and(|f| !f.is_empty()),
            "fingerprint returned for the generated key"
        );

        // The seed is private material in the settings table only — reachable
        // from storage, never from the API surface.
        let stored = models::get_setting(&state.db, "registry.signing_key").unwrap();
        assert_eq!(stored.as_deref(), Some(seed));

        // Audit records the action, never the seed.
        let logs = models::list_audit_logs(&state.db, 50).unwrap();
        assert!(
            logs.iter()
                .all(|l| !format!("{} {} {}", l.target, l.ip, l.details).contains(seed)),
            "audit entry must not contain the seed"
        );
        assert!(logs.iter().any(|l| l.action == "registry.signing_key"));

        // A second generate mints a fresh key and returns its seed once too.
        let (_status, body) = request(
            state.clone(),
            "POST",
            "/api/settings/registry/signing-key",
            &cookie,
            Some(json!({ "key": null })),
        )
        .await;
        let seed2 = body["seed"].as_str().expect("seed returned on every generate");
        assert_ne!(seed, seed2, "generating again must rotate the key");
    }

    #[tokio::test]
    async fn set_and_clear_keep_their_shape() {
        let (_tmp, state) = test_state();
        let (_admin_id, cookie) = seed_admin(&state);

        // Set an explicit key: no seed echo, no warning.
        let known = hex::encode([7u8; 32]);
        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/settings/registry/signing-key",
            &cookie,
            Some(json!({ "key": known })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["generated"], false);
        assert_eq!(body["cleared"], false);
        assert_eq!(body["enabled"], true);
        assert!(body.get("seed").is_none(), "set must not echo the seed");
        assert!(body.get("store_this_seed_now").is_none());
        assert!(
            body["public_key"].as_str().is_some_and(|p| !p.is_empty()),
            "public key derived from the explicit seed"
        );

        // Invalid hex is rejected before any mutation.
        let (status, _body) = request(
            state.clone(),
            "POST",
            "/api/settings/registry/signing-key",
            &cookie,
            Some(json!({ "key": "zz" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let stored = models::get_setting(&state.db, "registry.signing_key").unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(known.as_str()),
            "bad set must not clobber the existing key"
        );

        // Clear: signing disabled, still no seed anywhere.
        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/settings/registry/signing-key",
            &cookie,
            Some(json!({ "key": "" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cleared"], true);
        assert_eq!(body["enabled"], false);
        assert!(body.get("seed").is_none(), "clear must not carry a seed");

        let (status, body) =
            request(state.clone(), "GET", "/api/settings/registry", &cookie, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["enabled"], false);
        assert!(body["public_key"].is_null());
    }
    #[tokio::test]
    async fn notifications_list_mark_read_and_clear() {
        let (_tmp, state) = test_state();
        let (_admin_id, cookie) = seed_admin(&state);
        state.notifier.notify_link(
            "error",
            "boom",
            "details",
            Some(7),
            Some("#/server/7".into()),
        );
        state.notifier.notify("info", "ok", "done", None);

        // List: entries plus unread_count in one envelope.
        let (status, body) = request(state.clone(), "GET", "/api/notifications", &cookie, None).await;
        assert_eq!(status, StatusCode::OK);
        let data = &body["data"];
        assert_eq!(data["unread_count"], 2);
        let entries = data["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["title"], "boom");
        assert_eq!(entries[0]["server_id"], 7);
        assert_eq!(entries[0]["link"], "#/server/7");
        assert!(entries[0]["id"].is_u64());
        assert!(entries[0]["read_at"].is_null(), "fresh entries are unread");
        let id = entries[0]["id"].as_u64().unwrap();

        // Mark one read: unread drops, read_at stamps.
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/notifications/{id}/read"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        let (status, body) = request(state.clone(), "GET", "/api/notifications", &cookie, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["unread_count"], 1);
        assert!(
            body["data"]["entries"][0]["read_at"].is_string(),
            "read entry carries a timestamp"
        );

        // Unknown id → 404, history untouched.
        let (status, _body) = request(
            state.clone(),
            "POST",
            "/api/notifications/999999/read",
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_status, body) = request(state.clone(), "GET", "/api/notifications", &cookie, None).await;
        assert_eq!(body["data"]["unread_count"], 1);

        // Clear all: history empties and unread drops to zero.
        let (status, body) = request(
            state.clone(),
            "POST",
            "/api/notifications/clear",
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        let (status, body) = request(state.clone(), "GET", "/api/notifications", &cookie, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["entries"].as_array().unwrap().len(), 0);
        assert_eq!(body["data"]["unread_count"], 0);
    }

    #[tokio::test]
    async fn notifications_stream_serves_sse_to_admin() {
        let (_tmp, state) = test_state();
        let (_admin_id, cookie) = seed_admin(&state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/notifications/stream")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("");
        assert!(
            ct.contains("text/event-stream"),
            "stream must be served as SSE, got: {ct}"
        );
    }
}