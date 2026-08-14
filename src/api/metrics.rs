//! Telemetry endpoints: time-series queries for a workspace.
use super::{data, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread (see servers.rs
// for the full contract). `blocking` runs the pool-based `services::metrics`
// queries on Tokio's blocking pool. Never hold a pooled connection across an
// `.await` — split into separate blocking units instead.

#[derive(Debug, Deserialize)]
pub struct Q {
    pub window: Option<String>,
    pub points: Option<usize>,
}

fn window_secs(window: &str) -> Option<i64> {
    match window {
        "1h" => Some(3600),
        "6h" => Some(6 * 3600),
        "24h" => Some(24 * 3600),
        "7d" => Some(7 * 86400),
        _ => None,
    }
}

pub async fn series(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<Q>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    let window = q.window.as_deref().unwrap_or("1h");
    let secs = window_secs(window).ok_or_else(|| ApiError::bad_request("unknown window"))?;
    let points = q.points.unwrap_or(120).clamp(10, 1000);
    let now = chrono::Utc::now().timestamp();
    let (rows, sum) = blocking(state.db.clone(), move |db| {
        let rows = crate::services::metrics::range(&db, id, now - secs, now, points)?;
        let sum = crate::services::metrics::summary(&db, id, now - secs)?;
        Ok((rows, sum))
    })
    .await?;
    Ok(data(serde_json::json!({
        "window": window,
        "points": points,
        "summary": sum,
        "series": rows,
    })))
}
pub async fn summary(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, id, Capability::ConsoleRead).await?;
    let now = chrono::Utc::now().timestamp();
    let sum = blocking(state.db.clone(), move |db| {
        crate::services::metrics::summary(&db, id, now - 24 * 3600)
    })
    .await?;
    Ok(data(serde_json::json!(sum)))
}

/// Panel self-metrics for the Observatory view (root-admin only). Everything
/// DB-shaped runs through `blocking` like the other handlers; the request
/// counters come from the process-global atomics fed by main.rs middleware.
pub async fn panel(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = state.cfg.clone();
    let m = blocking(state.db.clone(), move |db| {
        crate::services::metrics::panel_self_metrics(&db, &cfg)
    })
    .await?;
    Ok(data(serde_json::json!(m)))
}

#[cfg(test)]
mod tests {
    use crate::models;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
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

    fn seed(state: &AppState, username: &str, root: bool) -> String {
        let uid = models::create_user(
            &state.db,
            username,
            &format!("{username}@x.io"),
            "h",
            root,
            "en",
            "dark",
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db, &state.cfg, uid, "test-agent", "127.0.0.1", false,
        )
        .unwrap();
        format!("vp_session={raw}")
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/metrics/panel", get(panel))
            .with_state(state)
    }

    async fn get_panel(state: AppState, cookie: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("GET")
            .uri("/api/metrics/panel")
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let response = router(state).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null))
        };
        (status, value)
    }

    #[tokio::test]
    async fn panel_returns_self_metrics_shape_for_admin() {
        let (_tmp, state) = test_state();
        let cookie = seed(&state, "root", true);
        let (status, body) = get_panel(state.clone(), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let data = &body["data"];
        assert!(data["uptime_secs"].is_number());
        assert_eq!(data["pool"]["max"], 8, "POOL_MAX_SIZE");
        assert!(data["pool"]["connections"].is_number());
        assert!(data["pool"]["idle"].is_number());
        assert!(
            data["pool"]["saturated"].is_boolean(),
            "pool.saturated is a bool (connections >= max)"
        );
        assert!(data["requests"]["total"].is_number());
        assert!(data["requests"]["per_minute"].is_array());
        assert_eq!(
            data["requests"]["per_minute"].as_array().unwrap().len(),
            10,
            "the per-minute ring exposes exactly 10 buckets"
        );
        assert!(data["scheduler"]["pending_runs"].is_number());
        // last_tick_at is omitted until the scheduler loop has ticked; once a
        // tick has run it must be a positive unix timestamp.
        match data["scheduler"].get("last_tick_at") {
            None => {}
            Some(v) => assert!(
                v.is_u64() && v.as_u64().unwrap() > 0,
                "last_tick_at, when present, is a positive integer"
            ),
        }
        assert!(data["webhooks"]["pending_deliveries"].is_number());
        assert_eq!(data["mirror"]["status"], "disabled");
    }

    #[tokio::test]
    async fn panel_rejects_non_admin() {
        let (_tmp, state) = test_state();
        let cookie = seed(&state, "member", false);
        let (status, body) = get_panel(state.clone(), &cookie).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "admin only");
    }
}