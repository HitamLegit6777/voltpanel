//! VoltPanel — lightweight, Docker-free game/server/web hosting panel.
//!
//! Single Rust binary: web UI + REST API + process manager + scheduler.
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(clippy::too_many_arguments)]

pub mod api;
pub mod auth;
pub mod capability;
pub mod config;
pub mod db;
pub mod isolation;
pub mod models;
pub mod node_protocol;
pub mod nodes;
pub mod services;
pub mod tls;
pub mod web;

use anyhow::Result;
use axum::routing::{delete, get, patch, post, put};
use axum::Router;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn config_path() -> String {
    std::env::var("VOLTPANEL_CONFIG").unwrap_or_else(|_| "config.toml".into())
}

fn load_config() -> anyhow::Result<config::Config> {
    let configured = std::env::var_os("VOLTPANEL_CONFIG").is_some();
    let path = config_path();
    let path = std::path::Path::new(&path);
    if path.exists() {
        config::Config::load(path)
    } else if configured {
        anyhow::bail!("configured file does not exist: {}", path.display())
    } else {
        // No config file: silently booting with defaults is only safe for a
        // fresh data dir. A non-empty one holds a previous install's state —
        // defaulting would point at the wrong directory, create a second
        // panel database, and strand the real one.
        let defaults = config::Config::default();
        let data_dir = &defaults.general.data_dir;
        let non_empty = std::fs::read_dir(data_dir)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if non_empty {
            anyhow::bail!(
                "no config file at {} and data dir {} is not empty; refusing to boot with defaults (create config.toml or set VOLTPANEL_CONFIG)",
                path.display(),
                data_dir.display()
            );
        }
        Ok(defaults)
}
}

pub static SETTINGS: std::sync::LazyLock<config::Config> = std::sync::LazyLock::new(|| {
    load_config().unwrap_or_else(|e| panic!("failed to load {}: {e}", config_path()))
});

tokio::task_local! {
    /// Correlation id minted per request; read by the trace span and by
    /// NodeClient's outbound headers.
    pub static REQUEST_ID: String;
}
/// Startup bootstrap for servers hosted by this panel instance.
///
/// Remote-node servers are skipped entirely: their lifecycle belongs to the
/// node, which may still be running them after a panel restart. Marking them
/// `offline` here would desync the panel from a live workload.
///
/// A local server whose sandbox cannot be prepared logs and is skipped: one
/// broken directory must not abort the whole boot (the panel stays up, the
/// operator sees the warning and fixes the server).
fn bootstrap_startup_servers(
    db: &db::Db,
    procs: &Arc<services::proc::ProcManager>,
    monitor: &Arc<services::Monitor>,
    servers: &[models::Server],
) {
    for s in servers {
        if !is_local_node(&s.node) {
            continue;
        }
        if let Err(e) = bootstrap_server(db, procs, monitor, s) {
            tracing::warn!(
                "boot: server {} ({}) skipped after bootstrap failure: {e:#}",
                s.id,
                s.uuid
            );
        }
    }
}

fn bootstrap_server(
    db: &db::Db,
    procs: &Arc<services::proc::ProcManager>,
    monitor: &Arc<services::Monitor>,
    s: &models::Server,
) -> Result<()> {
    procs.register_limits(s.id, s.memory_mb, s.cpu_percent);
    monitor.set_limit(services::ServerLimits {
        server_id: s.id,
        memory_mb: s.memory_mb as u64,
        cpu_percent: s.cpu_percent as u64,
        bandwidth_rx: 0,
        bandwidth_tx: 0,
    });
    crate::isolation::cleanup_orphans(&s.uuid);
    let root = services::proc::server_dir(s);
    crate::isolation::prepare_root(&root, &s.uuid)?;
    crate::isolation::own_tree(&root, &s.uuid)?;
    // A `running` row left over from a previous panel lifetime is stale
    // now that this panel is booting; no local workload actually survived.
    if should_clear_stale_running(&s.node, &s.status, s.suspended) {
        let _ = models::set_server_status(db, s.id, "offline");
    }
    Ok(())
}

fn is_local_node(node: &str) -> bool {
    node == "local"
}

/// A `running` row needs clearing only when the panel itself hosts the
/// server. Remote rows are left for the node's own reconciliation.
fn should_clear_stale_running(node: &str, status: &str, suspended: bool) -> bool {
    is_local_node(node) && status == "running" && !suspended
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("voltpanel {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("check-config") | Some("--check-config") => {
            let path = args
                .windows(2)
                .find(|pair| pair[0] == "--config")
                .map(|pair| pair[1].clone())
                .or_else(|| std::env::var("VOLTPANEL_CONFIG").ok())
                .unwrap_or_else(|| "config.toml".into());
            let cfg = config::Config::load(std::path::Path::new(&path))?;
            println!("valid config: {} (listen {})", path, cfg.web.listen);
            return Ok(());
        }
        Some("reset-password") => {
            let username = args.get(2).map(String::as_str).unwrap_or("admin");
            if !args.iter().any(|arg| arg == "--password-stdin") {
                anyhow::bail!("reset-password requires --password-stdin");
            }
            let cfg = load_config()?;
            let mut password = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut password)?;
            while password.ends_with(['\n', '\r']) {
                password.pop();
            }
            reset_password(&cfg, username, &password)?;
            println!("password reset for {username}; all sessions revoked");
            return Ok(());
        }
        Some(other) => {
            anyhow::bail!(
                "unknown command '{other}' (supported: --version, check-config, reset-password)"
            )
        }
        None => {}
    }
    unsafe {
        libc::umask(0o077);
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let path = config_path();
        if std::path::Path::new(&path).exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let cfg = SETTINGS.clone();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&cfg.general.log_level)),
        )
        .init();
    models::set_audit_enabled(cfg.features.enable_audit_log);
    models::set_audit_retention_days(cfg.general.audit_retention_days);
    cfg.ensure_dirs()?;
    // Relocate pre-existing Data Lab files before any workload can run.
    services::databases::migrate_legacy_storage(&cfg.paths.servers_dir, &cfg.paths.datalab_dir);
    let db = db::open(&cfg.general.data_dir.join("voltpanel.db").to_string_lossy())?;
    // Reclaim `.restore-*` / `.previous-*` dirs a crash mid-restore can leave
    // behind. Safe at this point: no restore can be in flight before the
    // router is up. (Also sweeps stray renameat2-probe files.)
    let removed = services::backups::recover_stale_dirs(&cfg)?;
    if removed > 0 {
        tracing::info!("boot: removed {removed} stale restore dir(s)");
    }

    let hub = Arc::new(services::console::ConsoleHub::new(cfg.clone()));
    let procs = Arc::new(services::proc::ProcManager::new(
        db.clone(),
        hub.clone(),
        cfg.paths.datalab_dir.clone(),
    ));
    let notifier = Arc::new(services::proc::Notifier::new());
    let monitor = Arc::new(services::Monitor::new());
    let running = Arc::new(AtomicBool::new(true));
    let node_client = Arc::new(services::node::NodeClient::new()?);
    let node_nonces = Arc::new(services::node::NonceCache::new());

    // Console watcher engine: evaluates operator-defined patterns against
    // completed runtime lines and dispatches notify/restart/stop/command. It
    // holds a `Weak` back-edge to the hub (the hub owns the engine), so it is
    // built after both exist and injected once via `set_engine`.
    let watcher_engine = Arc::new(services::watcher::WatcherEngine::new(
        db.clone(),
        notifier.clone(),
        Arc::downgrade(&hub),
        procs.clone(),
        node_client.clone(),
        tokio::runtime::Handle::current(),
    ));
    hub.set_engine(watcher_engine.clone());

    // seed
    seed(&db, &cfg)?;

    let state = api::AppState {
        db: db.clone(),
        cfg: cfg.clone(),
        procs: procs.clone(),
        hub: hub.clone(),
        notifier: notifier.clone(),
        monitor: monitor.clone(),
        node_client: node_client.clone(),
        node_nonces: node_nonces.clone(),
        running: running.clone(),
        watcher_engine: watcher_engine.clone(),
    };

    // Register local limits + reconcile local workload state. Remote-node
    // servers keep whatever status their node reports — a panel restart must
    // not mark a live remote workload "offline".
    bootstrap_startup_servers(&db, &procs, &monitor, &models::list_servers(&db, None, false)?);

    services::spawn_background(
        db.clone(),
        cfg.clone(),
        procs.clone(),
        monitor.clone(),
        notifier.clone(),
        hub.clone(),
        node_client.clone(),
        running.clone(),
    )
    .await;

    // ---- router ----
    // Observatory self-metrics: pin the counter epoch to boot time so the
    // reported uptime/since cover the whole process lifetime, not just the
    // first request.
    LazyLock::force(&REQUEST_COUNTERS);
    let app = build_router(state);

    // ---- host-routing gateway (optional) ----
    // `[sites].listen` unset → disabled. A bind failure on a configured
    // address fails fast: the panel refuses to boot with a misconfigured
    // gateway. A later runtime serve error is logged by the task and the
    // panel keeps serving.
    let gateway_task = services::gateway::Gateway::new(db.clone(), cfg.clone())
        .start(running.clone())
        .await?;

    let addr = cfg.web.listen;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // `shutdown` resolves the instant a signal arrives so the server stops
    // accepting new connections immediately; the workload drain runs
    // concurrently with the in-flight HTTP drain and is awaited below.
    let (shutdown, drain) = shutdown_signal(running.clone(), procs.clone());
    let serve_result = if cfg.web.tls_self_signed {
        let sans = tls::default_sans(&cfg.web.tls_extra_sans);
        let material = tls::ensure_material(&cfg.general.data_dir.join("tls"), &sans)?;
        tracing::info!(
            "{} listening on https://{addr} (cert fingerprint {})",
            cfg.general.instance_name,
            material.fingerprint
        );
        tls::serve_tls(listener, app, tls::server_config(&material)?, shutdown).await
    } else {
        tracing::info!("{} listening on http://{addr}", cfg.general.instance_name);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(anyhow::Error::from)
    };
    // Bound the drain even if serving itself failed: workloads must not be
    // left SIGTERM-less when the accept loop dies for another reason.
    if let Err(e) = drain.await {
        tracing::warn!("workload drain task failed: {e}");
    }
    serve_result?;
    // The gateway shares the `running` flag, so it stops accepting new
    // connections and drains concurrently with the panel; join it so
    // shutdown completes gracefully before the process exits.
    if let Some(task) = gateway_task {
        let _ = task.await;
    }
    Ok(())
}

/// Returns (1) a future that resolves the moment a shutdown signal arrives —
/// so the HTTP/TLS server stops accepting new connections immediately — and
/// (2) a handle to the concurrently-running workload drain: SIGTERM every
/// local workload, a bounded join, then a last-resort SIGKILL sweep.
fn shutdown_signal(
    running: Arc<AtomicBool>,
    procs: Arc<services::proc::ProcManager>,
) -> (impl Future<Output = ()> + Send + 'static, tokio::task::JoinHandle<()>) {
    // The drain must not start until the shutdown signal fires: its first act
    // sets `ProcManager::stopped`, which permanently blocks new server starts.
    // A oneshot fires the drain exactly once, when SIGTERM/SIGINT arrives.
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let signal = async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down: draining HTTP connections and workloads");
        running.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(());
    };
    let drain = async move {
        let _ = rx.await;
        procs.set_stop();
        // Graceful stop: ProcManager::stop SIGTERMs the whole process group,
        // then gives it a 10s drain window before its own last-resort SIGKILL
        // sweep. Only local workloads live in `procs`; remote-node servers
        // are untouched.
        for (id, _) in procs.all() {
            let _ = procs.stop(id);
        }
        // Bounded join: wait out the drain window (or until every pid is
        // gone), then SIGKILL anything that is still alive as a final sweep.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(11);
        while std::time::Instant::now() < deadline {
            if procs
                .all()
                .iter()
                .all(|(_, ps)| ps.pid.lock().is_none())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        for (id, _) in procs.all() {
            let _ = procs.kill(id);
        }
    };
    let drain_handle = tokio::spawn(drain);
    (signal, drain_handle)
}

fn reset_password(cfg: &config::Config, username: &str, password: &str) -> Result<()> {
    if password.len() < cfg.security.password_min_len {
        anyhow::bail!(
            "password must contain at least {} characters",
            cfg.security.password_min_len
        );
    }
    if password.len() > 1024 {
        anyhow::bail!("password must contain at most 1024 characters");
    }
    let db = db::open(&cfg.general.data_dir.join("voltpanel.db").to_string_lossy())?;
    let user = models::get_user_by_name(&db, username)?;
    let hash = auth::hash_password(cfg, password)?;
    models::set_password(&db, user.id, &hash)?;
    auth::revoke_all_user_sessions(&db, user.id, None)?;
    Ok(())
}

/// Content-Security-Policy for every response (unless a proxy already set
/// one). The inline theme pre-paint script in `templates/index.html` runs via
/// its sha256 hash — computed from the exact bytes between `<script>` and
/// `</script>`; any whitespace change there invalidates it and the
/// `csp_hash_matches_inline_theme_script` test fails. Google Fonts hosts are
/// allowed because the SPA shell links them directly: the stylesheet comes
/// from fonts.googleapis.com (style-src), the font files from
/// fonts.gstatic.com (font-src). `unsafe-inline` is deliberately absent from
/// both script-src and style-src: the SPA carries no inline scripts or styles
/// (the theme pre-paint runs via its sha256 hash; UI styling lives in app.css
/// classes and CSSOM assignments in app.js).
const CSP: &str = "default-src 'self'; script-src 'self' 'sha256-K06epjT2diNrWdh4nFlRZsRp+fAvwDdkpmhZgrwrirU='; style-src 'self' https://fonts.googleapis.com; img-src 'self' data:; connect-src 'self'; font-src 'self' https://fonts.gstatic.com; object-src 'none'; base-uri 'self'; frame-ancestors 'none'";

// ---------------- Observatory: panel self-metrics ----------------
//
// Pterodactyl ships no panel-level observability (its telemetry is
// server-only). VoltPanel's Observatory answers with its own self-metrics:
// the `count_request` middleware below tallies every routed response into
// process-global atomics — per status class plus a per-minute rate ring —
// and GET /api/metrics/panel snapshots them. The hot path is lock-free:
// one or two Relaxed fetch_adds per request.
const RATE_BUCKETS: usize = 10;

/// Process-global HTTP request counters for the Observatory view.
pub struct RequestCounters {
    started_at: std::time::Instant,
    started_unix: i64,
    total: AtomicU64,
    ok: AtomicU64,
    client_err: AtomicU64,
    server_err: AtomicU64,
    /// Ring of per-minute request totals: `minute_stamp[i]` is the unix
    /// minute (epoch seconds / 60) whose count lives in `minute_count[i]`.
    /// A stamp mismatch means the slot is stale and reads as zero.
    minute_stamp: [AtomicI64; RATE_BUCKETS],
    minute_count: [AtomicU64; RATE_BUCKETS],
}

/// Point-in-time read of the counters, sized for the Observatory endpoint.
pub struct RequestSnapshot {
    pub uptime_secs: u64,
    pub since_unix: i64,
    pub total: u64,
    pub ok: u64,
    pub client_err: u64,
    pub server_err: u64,
    /// Requests in each of the last `RATE_BUCKETS` minutes, oldest first.
    pub per_minute: Vec<u64>,
}

impl RequestCounters {
    fn new() -> Self {
        RequestCounters {
            started_at: std::time::Instant::now(),
            started_unix: chrono::Utc::now().timestamp(),
            total: AtomicU64::new(0),
            ok: AtomicU64::new(0),
            client_err: AtomicU64::new(0),
            server_err: AtomicU64::new(0),
            minute_stamp: std::array::from_fn(|_| AtomicI64::new(0)),
            minute_count: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn now_minute() -> i64 {
        chrono::Utc::now().timestamp().div_euclid(60)
    }

    /// Count one completed response. Called by the router middleware on the
    /// response path; never short-circuits.
    pub fn record(&self, status: u16) {
        self.record_at(Self::now_minute(), status);
    }

    /// Testable core of `record`: `minute` is injected so ring advance and
    /// slot reuse are deterministic under test.
    fn record_at(&self, minute: i64, status: u16) {
        self.total.fetch_add(1, Ordering::Relaxed);
        match status {
            200..=299 => {
                self.ok.fetch_add(1, Ordering::Relaxed);
            }
            400..=499 => {
                self.client_err.fetch_add(1, Ordering::Relaxed);
            }
            500..=599 => {
                self.server_err.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let slot = minute.rem_euclid(RATE_BUCKETS as i64) as usize;
        // Claim the slot for this minute (reset-then-publish: a concurrent
        // snapshot that observes the new stamp is guaranteed to see the
        // reset count, so a stale minute never leaks into the ring).
        loop {
            if self.minute_stamp[slot].load(Ordering::Acquire) == minute {
                break;
            }
            self.minute_count[slot].store(0, Ordering::Relaxed);
            self.minute_stamp[slot].store(minute, Ordering::Release);
        }
        self.minute_count[slot].fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RequestSnapshot {
        self.snapshot_at(Self::now_minute())
    }

    fn snapshot_at(&self, minute: i64) -> RequestSnapshot {
        let mut per_minute = Vec::with_capacity(RATE_BUCKETS);
        for back in (0..RATE_BUCKETS).rev() {
            let m = minute - back as i64;
            let slot = m.rem_euclid(RATE_BUCKETS as i64) as usize;
            per_minute.push(if self.minute_stamp[slot].load(Ordering::Acquire) == m {
                self.minute_count[slot].load(Ordering::Relaxed)
            } else {
                0
            });
        }
        RequestSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs(),
            since_unix: self.started_unix,
            total: self.total.load(Ordering::Relaxed),
            ok: self.ok.load(Ordering::Relaxed),
            client_err: self.client_err.load(Ordering::Relaxed),
            server_err: self.server_err.load(Ordering::Relaxed),
            per_minute,
        }
    }
}

pub static REQUEST_COUNTERS: LazyLock<RequestCounters> = LazyLock::new(RequestCounters::new);

/// Observatory request-counter layer. Outermost: wraps every other layer so
/// fallback 404s and the host-allowlist / rate-limit rejections are tallied
/// too. Transparent — observes the response, never short-circuits.
pub async fn count_request(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let response = next.run(request).await;
    REQUEST_COUNTERS.record(response.status().as_u16());
    response
}

#[cfg(test)]
mod request_counters_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    #[test]
    fn record_tallies_status_classes_into_the_ring() {
        let c = RequestCounters::new();
        let snap = c.snapshot();
        assert_eq!(snap.total, 0);
        assert_eq!(snap.per_minute.len(), RATE_BUCKETS);
        assert!(snap.per_minute.iter().all(|&v| v == 0));

        c.record_at(1_000_000, 200);
        c.record_at(1_000_000, 201);
        c.record_at(1_000_000, 404);
        c.record_at(1_000_000, 500);
        let snap = c.snapshot_at(1_000_000);
        assert_eq!(snap.total, 4);
        assert_eq!(snap.ok, 2);
        assert_eq!(snap.client_err, 1);
        assert_eq!(snap.server_err, 1);
        assert_eq!(snap.per_minute.iter().sum::<u64>(), 4);
        assert_eq!(
            snap.per_minute[RATE_BUCKETS - 1],
            4,
            "the current minute lands in the newest slot"
        );
        assert_eq!(
            snap.per_minute[RATE_BUCKETS - 2],
            0,
            "older minutes stay zero"
        );
    }

    #[test]
    fn unclassified_statuses_count_as_total_only() {
        let c = RequestCounters::new();
        c.record_at(5, 301);
        c.record_at(5, 600);
        let snap = c.snapshot_at(5);
        assert_eq!(snap.total, 2);
        assert_eq!(snap.ok + snap.client_err + snap.server_err, 0);
    }

    #[test]
    fn ring_rotates_and_reused_slots_do_not_bleed() {
        let c = RequestCounters::new();
        c.record_at(1000, 200); // slot 0
        c.record_at(1001, 200); // slot 1
        c.record_at(1010, 200); // slot 0 again, ten minutes later
        let snap = c.snapshot_at(1010);
        assert_eq!(snap.per_minute[0], 1, "minute 1001 keeps its own slot");
        assert_eq!(snap.per_minute[9], 1, "current minute (1010) owns the reused slot");
        assert_eq!(snap.per_minute.iter().sum::<u64>(), 2);
    }

    #[tokio::test]
    async fn middleware_tallies_status_classes_end_to_end() {
        let app = Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route("/broken", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(axum::middleware::from_fn(count_request));
        let before = REQUEST_COUNTERS.snapshot();
        for (uri, want) in [("/ok", 200), ("/nope", 404), ("/broken", 500)] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status().as_u16(), want);
        }
        let after = REQUEST_COUNTERS.snapshot();
        assert_eq!(after.total - before.total, 3);
        assert_eq!(after.ok - before.ok, 1);
        assert_eq!(after.client_err - before.client_err, 1);
        assert_eq!(after.server_err - before.server_err, 1);
    }
}
fn build_router(state: api::AppState) -> Router {
    let max_body = state
        .cfg
        .web
        .max_body_mb
        .saturating_mul(1_048_576)
        .min(usize::MAX as u64) as usize;
    use crate::api::*;
    Router::new()
        // ------- auth/users -------
        .route("/api/login", post(users::login).layer(axum::middleware::from_fn_with_state(state.clone(), users::pre_login_rate_limit)))
        .route("/api/logout", post(users::logout))
        .route("/api/me", get(users::me))
        .route("/api/profile", post(users::update_profile))
        .route("/api/password", post(users::change_password))
        .route("/api/2fa/setup", get(users::setup_2fa))
        .route("/api/2fa/confirm", post(users::confirm_2fa))
        .route("/api/2fa/disable", post(users::disable_2fa))
        .route("/api/2fa/recovery/regenerate", post(users::regenerate_recovery_codes))
        // ------- servers -------
        .route("/api/servers", get(servers::list))
        .route("/api/servers/all", get(servers::admin_list_all))
        .route("/api/servers", post(servers::create).layer(axum::middleware::from_fn_with_state(state.clone(), api::idempotency)))
        .route("/api/servers/:id", get(servers::get))
        .route("/api/servers/:id", patch(servers::update))
        .route("/api/servers/:id", delete(servers::delete))
        .route("/api/servers/:id/purge", delete(servers::purge))
        .route("/api/servers/:id/power", post(servers::power))
        .route("/api/servers/:id/install", post(servers::install))
        .route("/api/servers/:id/suspend", post(servers::suspend))
        .route("/api/servers/:id/unsuspend", post(servers::unsuspend))
        .route("/api/servers/:id/variables", post(servers::update_vars))
        .route("/api/servers/:id/stats", get(servers::stats))
        .route("/api/servers/:id/command", post(servers::send_command))
        // subusers
        .route("/api/servers/:id/subusers", get(servers::list_subusers))
        .route("/api/servers/:id/subusers", post(servers::add_subuser))
        .route(
            "/api/servers/:id/subusers/:sub_id",
            delete(servers::remove_subuser),
        )
        // ------- squads (org workspaces, admin) -------
        .route("/api/admin/squads", get(servers::squad_list))
        .route("/api/admin/squads", post(servers::squad_create))
        .route("/api/admin/squads/:id", get(servers::squad_get))
        .route("/api/admin/squads/:id", patch(servers::squad_update))
        .route("/api/admin/squads/:id", delete(servers::squad_delete))
        .route("/api/admin/squads/:id/members", post(servers::squad_add_member))
        .route(
            "/api/admin/squads/:id/members/:uid",
            patch(servers::squad_update_member).delete(servers::squad_remove_member),
        )
        .route(
            "/api/admin/squads/:id/servers",
            put(servers::squad_set_servers).post(servers::squad_add_server),
        )
        .route(
            "/api/admin/squads/:id/servers/:sid",
            delete(servers::squad_remove_server),
        )
        // ------- allocations (self-service) -------
        .route("/api/servers/:id/allocations", get(allocations::list))
        .route("/api/servers/:id/allocations", post(allocations::add))
        .route(
            "/api/servers/:id/allocations/:alloc_id",
            patch(allocations::update).delete(allocations::remove),
        )
        // ------- console -------
        .route("/api/servers/:id/console/history", get(console::history))
        .route("/api/servers/:id/console/stream", get(console::stream))
        .route("/api/servers/:id/console/clear", post(console::clear))
        .route(
            "/api/servers/:id/console/command",
            post(console::send_command),
        )
        .route("/api/servers/:id/console/log", get(console::log_download))
        .route("/api/servers/:id/console/crash", get(console::crash_info))
        .route(
            "/api/servers/:id/console/crash-policy",
            patch(console::crash_policy),
        )
        // ------- console watchers -------
        .route(
            "/api/servers/:id/console/watchers",
            get(watchers::list).post(watchers::create),
        )
        .route(
            "/api/servers/:id/console/watchers/:watcher_id",
            put(watchers::update).delete(watchers::delete),
        )
        // ------- files -------
        .route("/api/servers/:id/files", get(files::list))
        .route("/api/servers/:id/files/read", get(files::read))
        .route("/api/servers/:id/files/write", post(files::write))
        .route(
            "/api/servers/:id/files/upload",
            post(files::upload_multipart),
        )
        .route("/api/servers/:id/files/upload_b64", post(files::upload_b64))
        .route("/api/servers/:id/files/download", get(files::download))
        .route("/api/servers/:id/files/download_multi", get(files::download_multi))
        .route("/api/servers/:id/files/rename", post(files::rename))
        .route("/api/servers/:id/files/copy", post(files::copy))
        .route("/api/servers/:id/files/move", post(files::move_files))
        .route("/api/servers/:id/files/delete", post(files::delete))
        .route("/api/servers/:id/files/chmod", post(files::chmod))
        .route("/api/servers/:id/files/mkdir", post(files::mkdir))
        .route("/api/servers/:id/files/touch", post(files::touch))
        .route(
            "/api/servers/:id/files/archive",
            post(files::create_archive),
        )
        .route("/api/servers/:id/files/extract", post(files::extract))
        .route("/api/servers/:id/files/summary", get(files::summary))
        .route("/api/servers/:id/files/exists", get(files::exists_check))
        // ------- files: remote URL pull -------
        .route("/api/servers/:id/files/pull", post(files::pull))
        .route(
            "/api/servers/:id/files/pull/:transfer_id",
            get(files::pull_status),
        )
        .route(
            "/api/servers/:id/files/pull/:transfer_id",
            delete(files::pull_cancel),
        )
        // ------- websites (vhosts/proxy) -------
        .route("/api/servers/:id/sites", get(sites::list))
        .route("/api/servers/:id/sites", post(sites::create))
        .route("/api/servers/:id/sites/:site_id", get(sites::get))
        .route("/api/servers/:id/sites/:site_id", patch(sites::update))
        .route("/api/servers/:id/sites/:site_id", delete(sites::delete))
        .route(
            "/api/servers/:id/sites/:site_id/toggle",
            post(sites::toggle),
        )
        // ------- metrics -------
        .route("/api/servers/:id/metrics", get(metrics::series))
        .route("/api/servers/:id/metrics/summary", get(metrics::summary))
        .route("/api/metrics/panel", get(metrics::panel))
        // ------- activity -------
        .route("/api/activity", get(activity::user_activity))
        .route("/api/servers/:id/activity", get(activity::server_activity))
        // ------- workload blueprints -------
        .route("/api/blueprints", get(blueprints::list))
        .route("/api/blueprints/:id", get(blueprints::get))
        .route("/api/blueprints", post(blueprints::create))
        .route("/api/blueprints/:id", patch(blueprints::update))
        .route("/api/blueprints/:id", delete(blueprints::delete))
        .route("/api/blueprints/import", post(blueprints::import))
        .route("/api/blueprints/:id/export", get(blueprints::export))
        .route("/api/blueprints/categories", get(blueprints::categories))
        .route("/api/blueprints/:id/revisions", get(blueprints::revisions))
        .route(
            "/api/blueprints/:id/revisions/:version",
            get(blueprints::revision_detail),
        )
        .route("/api/blueprints/:id/rollback", post(blueprints::rollback))
        .route("/api/blueprints/:id/drift", get(blueprints::drift))
        .route("/api/servers/:id/blueprint/pin", post(blueprints::pin))
        .route("/api/blueprints/registry", get(blueprints::registry_list))
        .route(
            "/api/blueprints/registry/publish",
            post(blueprints::registry_publish),
        )
        .route(
            "/api/blueprints/registry/import",
            post(blueprints::registry_import),
        )
        .route(
            "/api/blueprints/registry/package/:id/:version",
            get(blueprints::registry_package_get),
        )
        // ------- schedules -------
        .route("/api/servers/:id/schedules", get(schedules::list))
        .route("/api/servers/:id/schedules", post(schedules::create))
        .route("/api/schedules/:id", patch(schedules::update))
        .route("/api/schedules/:id", delete(schedules::delete))
        .route("/api/schedules/:id/tasks", post(schedules::add_task))
        .route(
            "/api/schedules/:id/tasks/:task_id",
            delete(schedules::remove_task),
        )
        .route(
            "/api/servers/:server_id/schedules/:id/runs",
            get(schedules::runs),
        )
        .route(
            "/api/schedules/:id/toggle/:enabled",
            post(schedules::toggle),
        )
        .route("/api/schedules/:id/run", post(schedules::run_now).layer(axum::middleware::from_fn_with_state(state.clone(), api::idempotency)))
        // ------- backups -------
        .route("/api/servers/:id/backups", get(backups::list))
        .route("/api/servers/:id/backups", post(backups::create).layer(axum::middleware::from_fn_with_state(state.clone(), api::idempotency)))
        .route("/api/backups/:id/download", get(backups::download))
        .route("/api/backups/:id/restore", post(backups::restore).layer(axum::middleware::from_fn_with_state(state.clone(), api::idempotency)))
        .route("/api/backups/:id/delete", delete(backups::delete))
        .route("/api/backups/:id/verify", get(backups::verify))
        .route("/api/backups/:id/lock", post(backups::lock))
        .route("/api/servers/:id/backups/cleanup", post(backups::cleanup))
        .route("/api/backups/mirror/sync", post(backups::mirror_sync))
        // ------- databases -------
        .route("/api/servers/:id/databases", get(databases::list))
        .route("/api/servers/:id/databases", post(databases::create))
        .route(
            "/api/servers/:id/databases/:name/exec",
            post(databases::exec),
        )
        .route(
            "/api/servers/:id/databases/:name/query",
            post(databases::query),
        )
        .route(
            "/api/servers/:id/databases/:name/tables",
            get(databases::tables),
        )
        .route("/api/servers/:id/databases/:name", delete(databases::drop))
        .route(
            "/api/servers/:id/databases/:name/export",
            get(databases::export),
        )
        .route(
            "/api/servers/:id/databases/:name/import",
            post(databases::import),
        )
        .route(
            "/api/servers/:id/databases/:dbid/credentials",
            get(databases::credentials),
        )
        // ------- keys -------
        .route("/api/keys", get(keys::list))
        .route("/api/keys", post(keys::create))
        .route("/api/keys/:id", delete(keys::delete))
        .route("/api/keys/:id/revoke", post(keys::revoke))
        // ------- webhooks -------
        .route("/api/webhooks", get(webhooks::list))
        .route("/api/webhooks", post(webhooks::create))
        .route("/api/settings/registry", get(settings::registry_signing_status))
        .route(
            "/api/settings/registry/signing-key",
            post(settings::registry_set_signing_key),
        )
        .route("/api/webhooks/:id", get(webhooks::get))
        .route("/api/webhooks/:id", patch(webhooks::update))
        .route("/api/webhooks/:id", delete(webhooks::delete))
        .route("/api/webhooks/:id/toggle", post(webhooks::toggle))
        .route("/api/webhooks/:id/deliveries", get(webhooks::deliveries))
        .route("/api/webhooks/:id/test", post(webhooks::test))
        // ------- settings/system -------
        .route("/api/settings/public", get(settings::public))
        .route("/api/settings/limits", post(settings::update_limits))
        .route("/api/audit", get(settings::audit_logs))
        .route("/api/notifications", get(settings::notifications))
        .route("/api/notifications/stream", get(settings::notifications_stream))
        .route(
            "/api/notifications/:id/read",
            post(settings::notifications_read),
        )
        .route(
            "/api/notifications/clear",
            post(settings::notifications_clear),
        )
        .route("/api/system/stats", get(system::node_stats))
        .route("/api/system/health", get(system::health))
        .route("/api/system/version", get(system::version))
        .route("/api/system/isolation", get(system::isolation))
        .route("/api/system/ports/free", get(system::free_ports))
        .route("/api/system/allocations", get(system::allocations))
        .route(
            "/api/system/ports/:server_id/:port",
            post(system::assign_port),
        )
        .route(
            "/api/system/ports/:server_id/:port",
            delete(system::remove_port),
        )
        .route("/api/system/rate-limits", get(system::rate_limits_status))
        .route("/api/capabilities", get(system::capabilities))
        .route("/api/meta", get(api::meta))
        .route("/api/meta/openapi.json", get(api::openapi))
        // ------- multi-node -------
        .route("/api/nodes", get(nodes::list))
        .route("/api/nodes", post(nodes::create))
        .route("/api/nodes/transfer", post(nodes::transfer_server))
        .route("/api/nodes/placement", post(nodes::placement))
        .route("/api/nodes/:id", get(nodes::get))
        .route("/api/nodes/:id", patch(nodes::update))
        .route("/api/nodes/:id", delete(nodes::delete))
        .route("/api/nodes/:id/test", post(nodes::test_connection))
        .route("/api/nodes/:id/rotate-secret", post(nodes::rotate_secret))
        .route("/api/nodes/:id/drain", post(nodes::drain))
        .route("/api/nodes/:id/drain", delete(nodes::clear_drain))
        .route(
            "/api/nodes/:id/enrollment",
            post(nodes::regenerate_enrollment),
        )
        .route("/api/node/enroll", post(nodes::enroll))
        .route("/api/node/heartbeat", post(nodes::heartbeat))
        .route(
            "/api/system/rate-limits/reset",
            post(system::reset_rate_limits),
        )
        .route(
            "/api/system/servers/:server_id/limits",
            post(system::set_server_limits),
        )
        .route("/api/system/live-stats", get(system::live_stats))
        // ------- admin users -------
        .route("/api/admin/users", get(users::admin_list_users))
        .route("/api/admin/users", post(users::admin_create_user))
        .route("/api/admin/users/:id", patch(users::admin_update_user))
        .route("/api/admin/users/:id", delete(users::admin_delete_user))
        .route("/api/admin/users/:id", get(users::admin_get_user))
        .route(
            "/api/admin/users/:id/2fa/reset",
            post(users::admin_reset_2fa),
        )
        .route("/api/admin/sessions", get(users::admin_sessions))
        .route(
            "/api/admin/sessions/:token_prefix",
            delete(users::admin_revoke_session),
        )
        // ------- assets + SPA -------
        .route("/static/css/app.css", get(web::app_css))
        .route("/static/js/icons.js", get(web::icons_js))
        .route("/static/js/app.js", get(web::app_js))
        .route("/static/img/favicon.svg", get(web::favicon_svg))
        .route("/", get(web::index))
        .fallback(get(web::spa_fallback))
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::CONTENT_SECURITY_POLICY,
                axum::http::HeaderValue::from_static(CSP),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::X_CONTENT_TYPE_OPTIONS,
                axum::http::HeaderValue::from_static("nosniff"),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::X_FRAME_OPTIONS,
                axum::http::HeaderValue::from_static("DENY"),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                axum::http::header::REFERRER_POLICY,
                axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
            ),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::hsts,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::same_origin_mutations,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::api_mutation_rate_limit,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::host_allowlist,
        ))
        // TraceLayer is the outermost request-observing layer: it wraps
        // `same_origin_mutations` and the body limit, so every response —
        // including their 403s and 413s — is logged with the span. The
        // transparent `thread_request_id` middleware sits above it only
        // because it must mint the id before the span opens; the span and
        // NodeClient's outbound header both read it from `REQUEST_ID`.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<axum::body::Body>| {
                    let request_id = crate::REQUEST_ID
                        .try_with(|id| id.clone())
                        .unwrap_or_else(|_| Uuid::new_v4().to_string());
                    tracing::span!(
                        tracing::Level::INFO,
                        "http_request",
                        request_id = %request_id,
                        method = %request.method(),
                        uri = %request.uri(),
                    )
                })
                .on_response(
                    |response: &axum::http::Response<axum::body::Body>,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        let status = response.status().as_u16();
                        if status >= 500 {
                            tracing::error!(
                                parent: span,
                                status,
                                latency = ?latency,
                                "http request completed"
                            );
                        } else if status >= 400 {
                            tracing::warn!(
                                parent: span,
                                status,
                                latency = ?latency,
                                "http request completed"
                            );
                        } else {
                            tracing::info!(
                                parent: span,
                                status,
                                latency = ?latency,
                                "http request completed"
                            );
                        }
                    },
                ),
        )
        .layer(axum::middleware::from_fn(api::thread_request_id))
        // Outermost: the Observatory counter layer sees every response,
        // including fallback 404s and the allowlist/rate-limit rejections.
        .layer(axum::middleware::from_fn(count_request))
        .with_state(state)
}
fn seed(db: &db::Db, cfg: &config::Config) -> Result<()> {
    // admin user
    if models::get_user_by_name(db, "admin").is_err() {
        let password =
            std::env::var("VOLTPANEL_ADMIN_PASSWORD").unwrap_or_else(|_| auth::random_token(18));
        let hash = auth::hash_password(cfg, &password)?;
        models::create_user(
            db,
            "admin",
            "admin@voltpanel.local",
            &hash,
            true,
            "en",
            "dark",
        )?;
        // Never log credentials to the tracing pipeline (journald): the
        // one-time bootstrap credential goes to stdout only.
        println!("FIRST-RUN ADMIN CREDENTIAL — username=admin password={password} — change it immediately");
    }
    // Built-in VoltSpec blueprints.
    if models::list_blueprints(db)?.is_empty() {
        seed_blueprints(db)?;
    }
    Ok(())
}

fn seed_blueprints(db: &db::Db) -> Result<()> {
    let blueprints: Vec<serde_json::Value> = vec![
        serde_json::json!({
            "name": "Node.js", "description": "Run any Node.js application from the workspace root",
            "author": "voltpanel", "category": "web", "runtime_hint": "node",
            "startup": "node ${input.NODE_ARGS} ${input.ENTRYPOINT}",
            "stop": "^C",
            "variables": [
                {"name": "Entry Point", "description": "Main JS file", "env_var": "ENTRYPOINT", "default_value": "index.js", "required": true, "kind": {"type": "path", "max_len": 255}},
                {"name": "Node Args", "description": "Extra node arguments", "env_var": "NODE_ARGS", "default_value": "", "kind": {"type": "text", "max_len": 255}}
            ],
            "install": {"script": "if [ -f package.json ]; then npm install --omit=dev; fi"}
        }),
        serde_json::json!({
            "name": "Python", "description": "Python 3 application runner with optional venv",
            "author": "voltpanel", "category": "generic", "runtime_hint": "python",
            "startup": "${input.PYTHON_BIN} ${input.PY_ARGS} ${input.ENTRYPOINT}",
            "stop": "^C",
            "variables": [
                {"name": "Entry Point", "description": "Main python file", "env_var": "ENTRYPOINT", "default_value": "main.py", "required": true, "kind": {"type": "path", "max_len": 255}},
                {"name": "Python Binary", "description": "Interpreter to launch", "env_var": "PYTHON_BIN", "default_value": "python3", "required": true, "kind": {"type": "choice", "options": ["python3", "python3.11", "python3.12", "./.venv/bin/python"]}},
                {"name": "Python Args", "description": "Extra interpreter arguments", "env_var": "PY_ARGS", "default_value": "", "kind": {"type": "text", "max_len": 255}}
            ],
            "install": {"script": "if [ -f requirements.txt ]; then python3 -m venv .venv && ./.venv/bin/pip install --no-input -r requirements.txt; fi"}
        }),
        serde_json::json!({
            "name": "Minecraft Java", "description": "Minecraft: Java Edition server on the host JVM",
            "author": "voltpanel", "category": "game", "runtime_hint": "java",
            "startup": "java -Xms${input.MEMORY}M -Xmx${input.MEMORY}M -jar ${input.SERVER_JAR} nogui",
            "stop": "stop",
            "variables": [
                {"name": "Server Jar", "description": "Jar file to run", "env_var": "SERVER_JAR", "default_value": "server.jar", "required": true, "kind": {"type": "path", "max_len": 255}},
                {"name": "Server Jar URL", "description": "Download source used when the jar is missing", "env_var": "SERVER_JAR_URL", "default_value": "", "kind": {"type": "url"}},
                {"name": "Memory (MB)", "description": "Heap size", "env_var": "MEMORY", "default_value": "1024", "required": true, "kind": {"type": "number", "min": 128.0, "max": 32768.0}}
            ],
            "install": {"script": "if [ ! -f \"$SERVER_JAR\" ] && [ -n \"$SERVER_JAR_URL\" ]; then curl -fsSL -o \"$SERVER_JAR\" \"$SERVER_JAR_URL\"; fi\necho eula=true > eula.txt"}
        }),
        serde_json::json!({
            "name": "Terraria", "description": "Terraria dedicated server; upload the server build into the workspace first",
            "author": "voltpanel", "category": "game", "runtime_hint": "mono",
            "startup": "${input.SERVER_BIN} -port ${workspace.port} -autocreate 2 -worldname ${input.WORLD}",
            "stop": "exit",
            "variables": [
                {"name": "Server Binary", "description": "Launcher inside the workspace", "env_var": "SERVER_BIN", "default_value": "./TerrariaServer.bin.x86_64", "required": true, "kind": {"type": "path", "max_len": 255}},
                {"name": "World Name", "description": "World file", "env_var": "WORLD", "default_value": "world", "required": true, "kind": {"type": "text", "max_len": 64, "pattern": "^[A-Za-z0-9_-]+$"}}
            ],
            "install": {"script": "if [ -f \"$SERVER_BIN\" ]; then chmod +x \"$SERVER_BIN\"; fi"}
        }),
        serde_json::json!({
            "name": "Static Site", "description": "Serve a static site directory over HTTP",
            "author": "voltpanel", "category": "web", "runtime_hint": "python",
            "startup": "python3 -m http.server ${workspace.port} --bind 0.0.0.0 --directory ${input.WEB_ROOT}",
            "stop": "^C",
            "variables": [
                {"name": "Web Root", "description": "Directory served to visitors", "env_var": "WEB_ROOT", "default_value": "public", "required": true, "kind": {"type": "path", "max_len": 255}}
            ],
            "install": {"script": "mkdir -p \"$WEB_ROOT\"\nif [ ! -f \"$WEB_ROOT/index.html\" ]; then echo '<h1>VoltPanel</h1>' > \"$WEB_ROOT/index.html\"; fi"}
        }),
        serde_json::json!({
            "name": "Redis", "description": "Redis in-memory store bound to the workspace port",
            "author": "voltpanel", "category": "database", "runtime_hint": "redis",
            "startup": "redis-server --port ${workspace.port} --bind 0.0.0.0 --dir . --save ''",
            "stop": "shutdown",
            "variables": []
        }),
    ];
    for blueprint in blueprints {
        let name = blueprint["name"].as_str().unwrap().to_string();
        let description = blueprint["description"].as_str().unwrap_or("").to_string();
        let author = blueprint["author"]
            .as_str()
            .unwrap_or("voltpanel")
            .to_string();
        let category = blueprint["category"]
            .as_str()
            .unwrap_or("generic")
            .to_string();
        let runtime_hint = blueprint["runtime_hint"]
            .as_str()
            .unwrap_or("native")
            .to_string();
        let startup = blueprint["startup"].as_str().unwrap_or("").to_string();
        let stop = blueprint["stop"].as_str().unwrap_or("stop").to_string();
        let vars: Vec<crate::models::BlueprintInput> =
            serde_json::from_value(blueprint["variables"].clone()).unwrap_or_default();
        let install = blueprint["install"]["script"]
            .as_str()
            .map(|s| s.to_string());
        models::create_blueprint(
            db,
            &uuid::Uuid::new_v4().to_string(),
            &name,
            &description,
            &author,
            &category,
            &runtime_hint,
            &startup,
            None,
            install.as_deref(),
            &vars,
            &stop,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn reset_password_updates_hash_and_revokes_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.general.data_dir = temp.path().to_path_buf();
        cfg.security.argon2_cost = 1;
        cfg.security.argon2_mem_kib = 8 * 1024;
        let db = db::open(&temp.path().join("voltpanel.db").to_string_lossy()).unwrap();
        let old_hash = auth::hash_password(&cfg, "old-password").unwrap();
        let user_id = models::create_user(
            &db,
            "admin",
            "admin@example.com",
            &old_hash,
            true,
            "en",
            "dark",
        )
        .unwrap();
        let (session, _) =
            auth::create_session(&db, &cfg, user_id, "test", "127.0.0.1", false).unwrap();

        reset_password(&cfg, "admin", "new-password").unwrap();

        let stored: String = db
            .get().unwrap()
            .query_row(
                "SELECT password_hash FROM users WHERE id=?1",
                [user_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(auth::verify_password(&stored, "new-password"));
        assert!(!auth::verify_password(&stored, "old-password"));
        assert!(auth::user_from_session(&db, &session).is_err());
    }

    #[test]
    fn reset_password_rejects_short_password() {
        let cfg = config::Config::default();
        let error = reset_password(&cfg, "admin", "short").unwrap_err();
        assert!(error.to_string().contains("at least"));
    }

    #[test]
    fn startup_bootstrap_leaves_remote_servers_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.general.data_dir = temp.path().to_path_buf();
        cfg.paths.datalab_dir = temp.path().join("datalab");
        let db = db::open(&temp.path().join("voltpanel.db").to_string_lossy()).unwrap();
        let user_id = models::create_user(
            &db,
            "owner",
            "owner@example.com",
            "unused-hash",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let blueprint_id = models::create_blueprint(
            &db,
            "bp-1",
            "generic",
            "",
            "",
            "utility",
            "generic",
            "echo hi",
            None,
            None,
            &[],
            "",
        )
        .unwrap();
        let server_id = models::create_server(
            &db,
            "srv-1",
            "remote box",
            user_id,
            blueprint_id,
            "generic",
            "echo hi",
            1024,
            8192,
            100,
            0,
        )
        .unwrap();
        models::set_server_node(&db, server_id, "edge-1").unwrap();
        models::set_server_status(&db, server_id, "running").unwrap();

        let hub = Arc::new(services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(services::proc::ProcManager::new(
            db.clone(),
            hub,
            cfg.paths.datalab_dir.clone(),
        ));
        let monitor = Arc::new(services::Monitor::new());
        let servers = models::list_servers(&db, None, false).unwrap();
        bootstrap_startup_servers(&db, &procs, &monitor, &servers);

        let s = models::get_server(&db, server_id).unwrap();
        assert_eq!(
            s.status, "running",
            "remote-node server must not be marked offline by a panel restart"
        );
    }

    #[test]
    fn stale_running_clear_applies_only_to_local_servers() {
        // Local, running, not suspended -> stale, clear it.
        assert!(should_clear_stale_running("local", "running", false));
        // Local but suspended -> deliberately stopped, leave alone.
        assert!(!should_clear_stale_running("local", "running", true));
        // Local but already offline -> nothing to clear.
        assert!(!should_clear_stale_running("local", "offline", false));
        // Remote node, running -> the node owns it; never touch.
        assert!(!should_clear_stale_running("edge-1", "running", false));
    }

    #[test]
    fn csp_hash_matches_inline_theme_script() {
        // The CSP embeds the sha256 hash of the exact inline script bytes
        // between `<script>` and `</script>` in templates/index.html. Any
        // whitespace change to that script breaks the hash (and silently
        // blocks the theme pre-paint), so re-derive it here from the real
        // template and fail if the header drifted.
        let html = include_str!("../templates/index.html");
        let script = html
            .split("<script>")
            .nth(1)
            .and_then(|rest| rest.split("</script>").next())
            .expect("inline theme script present in index.html");
        let digest = <sha2::Sha256 as sha2::Digest>::digest(script.as_bytes());
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest);
        assert!(
            CSP.contains(&format!("'sha256-{encoded}'")),
            "inline theme script changed; update the script-src sha256 hash in CSP"
        );
    }
}