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
use axum::routing::{delete, get, patch, post};
use axum::Router;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

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
        Ok(config::Config::default())
    }
}

pub static SETTINGS: std::sync::LazyLock<config::Config> = std::sync::LazyLock::new(|| {
    load_config().unwrap_or_else(|e| panic!("failed to load {}: {e}", config_path()))
});

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
        Some(other) => {
            anyhow::bail!("unknown command '{other}' (supported: --version, check-config)")
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cfg = SETTINGS.clone();
    cfg.ensure_dirs()?;
    let db = db::open(&cfg.general.data_dir.join("voltpanel.db").to_string_lossy())?;

    let hub = Arc::new(services::console::ConsoleHub::new(cfg.clone()));
    let procs = Arc::new(services::proc::ProcManager::new(db.clone(), hub.clone()));
    let notifier = Arc::new(services::proc::Notifier::new());
    let monitor = Arc::new(services::Monitor::new());
    let running = Arc::new(AtomicBool::new(true));
    let node_client = Arc::new(services::node::NodeClient::new()?);
    let node_nonces = Arc::new(services::node::NonceCache::new());

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
    };

    // load per-server limits + spawn any previously-running servers
    for s in models::list_servers(&db, None, false)? {
        procs.register_limits(s.id, s.memory_mb, s.cpu_percent);
        monitor.set_limit(services::ServerLimits {
            server_id: s.id,
            memory_mb: s.memory_mb as u64,
            cpu_percent: s.cpu_percent as u64,
            bandwidth_rx: 0,
            bandwidth_tx: 0,
        });
        if s.node == "local" {
            crate::isolation::cleanup_orphans(&s.uuid);
            let root = services::proc::server_dir(&s);
            crate::isolation::prepare_root(&root, &s.uuid)?;
            crate::isolation::own_tree(&root, &s.uuid)?;
        }
        // resume servers that were running (status stuck as running)
        if s.status == "running" && !s.suspended {
            let _ = models::set_server_status(&db, s.id, "offline");
        }
    }

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
    let app = build_router(state);

    let addr = cfg.web.listen;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let shutdown = shutdown_signal(running.clone(), procs.clone());
    if cfg.web.tls_self_signed {
        let sans = tls::default_sans(&cfg.web.tls_extra_sans);
        let material = tls::ensure_material(&cfg.general.data_dir.join("tls"), &sans)?;
        tracing::info!(
            "{} listening on https://{addr} (cert fingerprint {})",
            cfg.general.instance_name,
            material.fingerprint
        );
        tls::serve_tls(listener, app, tls::server_config(&material)?, shutdown).await?;
    } else {
        tracing::info!("{} listening on http://{addr}", cfg.general.instance_name);
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await?;
    }
    Ok(())
}

async fn shutdown_signal(running: Arc<AtomicBool>, procs: Arc<services::proc::ProcManager>) {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down, stopping all servers");
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    procs.set_stop();
    for (_, ps) in procs.all() {
        if let Some(pid) = *ps.pid.lock() {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
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
        .route("/api/login", post(users::login))
        .route("/api/logout", post(users::logout))
        .route("/api/me", get(users::me))
        .route("/api/profile", post(users::update_profile))
        .route("/api/password", post(users::change_password))
        .route("/api/2fa/setup", get(users::setup_2fa))
        .route("/api/2fa/confirm", post(users::confirm_2fa))
        .route("/api/2fa/disable", post(users::disable_2fa))
        // ------- servers -------
        .route("/api/servers", get(servers::list))
        .route("/api/servers/all", get(servers::admin_list_all))
        .route("/api/servers", post(servers::create))
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
        // ------- console -------
        .route("/api/servers/:id/console/history", get(console::history))
        .route("/api/servers/:id/console/stream", get(console::stream))
        .route("/api/servers/:id/console/clear", post(console::clear))
        .route(
            "/api/servers/:id/console/command",
            post(console::send_command),
        )
        .route("/api/servers/:id/console/log", get(console::log_download))
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
        .route("/api/schedules/:id/runs", get(schedules::runs))
        .route(
            "/api/schedules/:id/toggle/:enabled",
            post(schedules::toggle),
        )
        .route("/api/schedules/:id/run", post(schedules::run_now))
        // ------- backups -------
        .route("/api/servers/:id/backups", get(backups::list))
        .route("/api/servers/:id/backups", post(backups::create))
        .route("/api/backups/:id/download", get(backups::download))
        .route("/api/backups/:id/restore", post(backups::restore))
        .route("/api/backups/:id/delete", delete(backups::delete))
        .route("/api/backups/:id/verify", get(backups::verify))
        .route("/api/servers/:id/backups/cleanup", post(backups::cleanup))
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
        // ------- keys -------
        .route("/api/keys", get(keys::list))
        .route("/api/keys", post(keys::create))
        .route("/api/keys/:id", delete(keys::delete))
        .route("/api/keys/:id/revoke", post(keys::revoke))
        // ------- webhooks -------
        .route("/api/webhooks", get(webhooks::list))
        .route("/api/webhooks", post(webhooks::create))
        .route("/api/webhooks/:id", get(webhooks::get))
        .route("/api/webhooks/:id", patch(webhooks::update))
        .route("/api/webhooks/:id", delete(webhooks::delete))
        .route("/api/webhooks/:id/toggle", post(webhooks::toggle))
        .route("/api/webhooks/:id/deliveries", get(webhooks::deliveries))
        .route("/api/webhooks/:id/test", post(webhooks::test))
        // ------- settings/system -------
        .route("/api/settings/public", get(settings::public))
        .route("/api/settings", get(settings::get))
        .route("/api/settings", post(settings::set))
        .route("/api/settings/config", get(settings::config_view))
        .route("/api/settings/limits", post(settings::update_limits))
        .route("/api/audit", get(settings::audit_logs))
        .route("/api/notifications", get(settings::notifications))
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
        .route("/api/admin/sessions", get(users::admin_sessions))
        .route(
            "/api/admin/sessions/:token_prefix",
            delete(users::admin_revoke_session),
        )
        // ------- assets + SPA -------
        .route("/static/css/app.css", get(web::app_css))
        .route("/static/js/icons.js", get(web::icons_js))
        .route("/static/js/app.js", get(web::app_js))
        .route("/", get(web::index))
        .fallback(get(web::spa_fallback))
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
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
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
        tracing::warn!("FIRST-RUN ADMIN CREDENTIAL — username=admin password={password} — change it immediately");
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
