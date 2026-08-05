//! VoltPanel — lightweight, Docker-free game/server/web hosting panel.
//!
//! Single Rust binary: web UI + REST API + process manager + scheduler.
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(clippy::too_many_arguments)]

pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod isolation;
pub mod models;
pub mod node_protocol;
pub mod nodes;
pub mod services;
pub mod web;

use anyhow::Result;
use axum::routing::{delete, get, patch, post};
use axum::Router;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

pub static SETTINGS: std::sync::LazyLock<config::Config> = std::sync::LazyLock::new(|| {
    let path = std::env::var("VOLTPANEL_CONFIG").unwrap_or_else(|_| "config.toml".into());
    config::Config::load(std::path::Path::new(&path)).unwrap_or_else(|e| {
        tracing::warn!("config load failed ({e}), using defaults");
        config::Config::default()
    })
});

#[tokio::main]
async fn main() -> Result<()> {
    unsafe { libc::umask(0o077); }
    { use std::os::unix::fs::PermissionsExt; let path=std::env::var("VOLTPANEL_CONFIG").unwrap_or_else(|_|"config.toml".into()); if std::path::Path::new(&path).exists(){std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600))?;} }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
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
            server_id: s.id, memory_mb: s.memory_mb as u64, cpu_percent: s.cpu_percent as u64, bandwidth_rx: 0, bandwidth_tx: 0,
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

    services::spawn_background(db.clone(), cfg.clone(), procs.clone(), monitor.clone(), notifier.clone(), hub.clone(), node_client.clone(), running.clone()).await;

    // ---- router ----
    let app = build_router(state);

    let addr = cfg.web.listen;
    tracing::info!("{} listening on http://{addr}", cfg.general.instance_name);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(running.clone(), procs.clone()))
    .await?;
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
    let max_body = state.cfg.web.max_body_mb.saturating_mul(1_048_576).min(usize::MAX as u64) as usize;
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
        .route("/api/servers/:id/subusers/:sub_id", delete(servers::remove_subuser))
        // ------- console -------
        .route("/api/servers/:id/console/history", get(console::history))
        .route("/api/servers/:id/console/stream", get(console::stream))
        .route("/api/servers/:id/console/clear", post(console::clear))
        .route("/api/servers/:id/console/command", post(console::send_command))
        .route("/api/servers/:id/console/log", get(console::log_download))
        // ------- files -------
        .route("/api/servers/:id/files", get(files::list))
        .route("/api/servers/:id/files/read", get(files::read))
        .route("/api/servers/:id/files/write", post(files::write))
        .route("/api/servers/:id/files/upload", post(files::upload_multipart))
        .route("/api/servers/:id/files/upload_b64", post(files::upload_b64))
        .route("/api/servers/:id/files/download", get(files::download))
        .route("/api/servers/:id/files/rename", post(files::rename))
        .route("/api/servers/:id/files/copy", post(files::copy))
        .route("/api/servers/:id/files/move", post(files::move_files))
        .route("/api/servers/:id/files/delete", post(files::delete))
        .route("/api/servers/:id/files/chmod", post(files::chmod))
        .route("/api/servers/:id/files/mkdir", post(files::mkdir))
        .route("/api/servers/:id/files/touch", post(files::touch))
        .route("/api/servers/:id/files/archive", post(files::create_archive))
        .route("/api/servers/:id/files/extract", post(files::extract))
        .route("/api/servers/:id/files/summary", get(files::summary))
        .route("/api/servers/:id/files/exists", get(files::exists_check))
        // ------- eggs -------
        .route("/api/eggs", get(eggs::list))
        .route("/api/eggs/:id", get(eggs::get))
        .route("/api/eggs", post(eggs::create))
        .route("/api/eggs/:id", patch(eggs::update))
        .route("/api/eggs/:id", delete(eggs::delete))
        .route("/api/eggs/import", post(eggs::import))
        .route("/api/eggs/:id/export", get(eggs::export))
        .route("/api/eggs/categories", get(eggs::categories))
        // ------- schedules -------
        .route("/api/servers/:id/schedules", get(schedules::list))
        .route("/api/servers/:id/schedules", post(schedules::create))
        .route("/api/schedules/:id", patch(schedules::update))
        .route("/api/schedules/:id", delete(schedules::delete))
        .route("/api/schedules/:id/tasks", post(schedules::add_task))
        .route("/api/schedules/:id/tasks/:task_id", delete(schedules::remove_task))
        .route("/api/schedules/:id/runs", get(schedules::runs))
        .route("/api/schedules/:id/toggle/:enabled", post(schedules::toggle))
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
        .route("/api/servers/:id/databases/:name/exec", post(databases::exec))
        .route("/api/servers/:id/databases/:name/query", post(databases::query))
        .route("/api/servers/:id/databases/:name/tables", get(databases::tables))
        .route("/api/servers/:id/databases/:name", delete(databases::drop))
        // ------- keys -------
        .route("/api/keys", get(keys::list))
        .route("/api/keys", post(keys::create))
        .route("/api/keys/:id", delete(keys::delete))
        // ------- settings/system -------
        .route("/api/settings/public", get(settings::public))
        .route("/api/settings", get(settings::get))
        .route("/api/settings", post(settings::set))
        .route("/api/settings/config", get(settings::config_view))
        .route("/api/settings/limits", post(settings::update_limits))
        .route("/api/audit", get(settings::audit_logs))
        .route("/api/notifications", get(settings::notifications))
        .route("/api/notifications/clear", post(settings::notifications_clear))
        .route("/api/system/stats", get(system::node_stats))
        .route("/api/system/health", get(system::health))
        .route("/api/system/version", get(system::version))
        .route("/api/system/isolation", get(system::isolation))
        .route("/api/system/ports/free", get(system::free_ports))
        .route("/api/system/allocations", get(system::allocations))
        .route("/api/system/ports/:server_id/:port", post(system::assign_port))
        .route("/api/system/ports/:server_id/:port", delete(system::remove_port))
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
        .route("/api/nodes/:id/enrollment", post(nodes::regenerate_enrollment))
        .route("/api/node/enroll", post(nodes::enroll))
        .route("/api/node/heartbeat", post(nodes::heartbeat))
        .route("/api/system/rate-limits/reset", post(system::reset_rate_limits))
        .route("/api/system/servers/:server_id/limits", post(system::set_server_limits))
        .route("/api/system/live-stats", get(system::live_stats))
        // ------- admin users -------
        .route("/api/admin/users", get(users::admin_list_users))
        .route("/api/admin/users", post(users::admin_create_user))
        .route("/api/admin/users/:id", patch(users::admin_update_user))
        .route("/api/admin/users/:id", delete(users::admin_delete_user))
        .route("/api/admin/sessions", get(users::admin_sessions))
        .route("/api/admin/sessions/:token_prefix", delete(users::admin_revoke_session))
        // ------- assets + SPA -------
        .nest_service("/static", web::static_dir())
        .route("/", get(web::index))
        .fallback(get(web::spa_fallback))
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(axum::http::header::X_CONTENT_TYPE_OPTIONS, axum::http::HeaderValue::from_static("nosniff")))
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(axum::http::header::X_FRAME_OPTIONS, axum::http::HeaderValue::from_static("DENY")))
        .layer(tower_http::set_header::SetResponseHeaderLayer::if_not_present(axum::http::header::REFERRER_POLICY, axum::http::HeaderValue::from_static("strict-origin-when-cross-origin")))
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(state)
}
fn seed(db: &db::Db, cfg: &config::Config) -> Result<()> {
    // admin user
    if models::get_user_by_name(db, "admin").is_err() {
        let password = std::env::var("VOLTPANEL_ADMIN_PASSWORD").unwrap_or_else(|_| auth::random_token(18));
        let hash = auth::hash_password(cfg, &password)?;
        models::create_user(db, "admin", "admin@voltpanel.local", &hash, true, "en", "dark")?;
        tracing::warn!("FIRST-RUN ADMIN CREDENTIAL — username=admin password={password} — change it immediately");
    }
    // eggs
    if models::list_eggs(db)?.is_empty() {
        seed_eggs(db)?;
    }
    Ok(())
}

fn seed_eggs(db: &db::Db) -> Result<()> {
    let eggs: Vec<(serde_json::Value, &str)> = vec![
        (serde_json::json!({
            "name": "Node.js", "description": "Run any Node.js application", "author": "voltpanel",
            "category": "web", "docker_image": "node:20-alpine",
            "startup": "node {{NODE_ARGS}} {{ENTRYPOINT}}",
            "stop": "stop",
            "variables": [
                {"name": "Entry Point", "description": "Main JS file", "env_var": "ENTRYPOINT", "default_value": "index.js", "user_viewable": true, "user_editable": true, "rules": "required|string|max:255"},
                {"name": "Node Args", "description": "Extra node arguments", "env_var": "NODE_ARGS", "default_value": "", "user_viewable": true, "user_editable": true, "rules": "string|max:255"}
            ]
        }), "node"),
        (serde_json::json!({
            "name": "Python", "description": "Python 3 application runner", "author": "voltpanel",
            "category": "generic", "docker_image": "python:3.11-slim",
            "startup": "python3 {{PY_ARGS}} {{ENTRYPOINT}}",
            "stop": "stop",
            "variables": [
                {"name": "Entry Point", "description": "Main python file", "env_var": "ENTRYPOINT", "default_value": "main.py", "user_viewable": true, "user_editable": true, "rules": "required|string|max:255"}
            ]
        }), "python"),
        (serde_json::json!({
            "name": "Minecraft Java", "description": "Minecraft: Java Edition server", "author": "voltpanel",
            "category": "game", "docker_image": "eclipse-temurin:21",
            "startup": "java -Xms{{MEMORY}}M -Xmx{{MEMORY}}M -jar {{SERVER_JAR}} nogui",
            "stop": "stop",
            "variables": [
                {"name": "Server Jar", "description": "Jar file to run", "env_var": "SERVER_JAR", "default_value": "server.jar", "user_viewable": true, "user_editable": true, "rules": "required|string|max:255"},
                {"name": "Memory (MB)", "description": "Heap size", "env_var": "MEMORY", "default_value": "1024", "user_viewable": true, "user_editable": true, "rules": "numeric|min:128|max:32768"}
            ],
            "install": {"script": "apt-get update -y && apt-get install -y wget && cd /mnt/server && if [ ! -f server.jar ]; then wget -O server.jar https://piston-data.mojang.com/v1/objects/8dd1a28015f228b9cac1499b248d8f5c8f4b8f8e/server.jar; fi"}
        }), "mc"),
        (serde_json::json!({
            "name": "Terraria", "description": "Terraria dedicated server", "author": "voltpanel",
            "category": "game", "docker_image": "mono:latest",
            "startup": "mono TerrariaServer.exe -port {{SERVER_PORT}} -autocreate 2 -worldname world",
            "stop": "exit",
            "variables": [
                {"name": "World Name", "description": "World file", "env_var": "WORLD", "default_value": "world.wld", "user_viewable": true, "user_editable": true, "rules": "required|string|max:255"}
            ]
        }), "terraria"),
        (serde_json::json!({
            "name": "Web Server", "description": "Static website hosting", "author": "voltpanel",
            "category": "web", "docker_image": "nginx:alpine",
            "startup": "nginx -g 'daemon off;'",
            "stop": "quit",
            "variables": []
        }), "web"),
        (serde_json::json!({
            "name": "Redis", "description": "Redis in-memory store", "author": "voltpanel",
            "category": "database", "docker_image": "redis:7",
            "startup": "redis-server --port {{SERVER_PORT}} --save ''",
            "stop": "shutdown",
            "variables": []
        }), "redis"),
    ];
    for (egg, _slug) in eggs {
        let name = egg["name"].as_str().unwrap().to_string();
        let description = egg["description"].as_str().unwrap_or("").to_string();
        let author = egg["author"].as_str().unwrap_or("voltpanel").to_string();
        let category = egg["category"].as_str().unwrap_or("generic").to_string();
        let image = egg["docker_image"].as_str().unwrap_or("alpine").to_string();
        let startup = egg["startup"].as_str().unwrap_or("").to_string();
        let stop = egg["stop"].as_str().unwrap_or("stop").to_string();
        let vars: Vec<crate::models::EggVariable> = egg["variables"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|v| crate::models::EggVariable {
                        name: v["name"].as_str().unwrap_or("").to_string(),
                        description: v["description"].as_str().unwrap_or("").to_string(),
                        env_var: v["env_var"].as_str().unwrap_or("").to_string(),
                        default_value: v["default_value"].as_str().unwrap_or("").to_string(),
                        user_viewable: v["user_viewable"].as_bool().unwrap_or(true),
                        user_editable: v["user_editable"].as_bool().unwrap_or(true),
                        rules: v["rules"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let install = egg["install"]["script"].as_str().map(|s| s.to_string());
        models::create_egg(db, &uuid::Uuid::new_v4().to_string(), &name, &description, &author, &category, &image, &startup, None, None, install.as_deref(), &vars, &stop)?;
    }
    Ok(())
}

