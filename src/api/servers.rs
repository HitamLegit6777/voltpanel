//! Server management endpoints: CRUD, power actions, variables, subusers.
use super::{data, ok, ApiError, ApiResult, AdminUser, AppState, AuthUser};
use crate::models::{self, Server, User};
use crate::services::{self, egg};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

/// Runtime limit override from the settings table, falling back to config.
fn limit_override(db: &crate::db::Db, key: &str, fallback: u64) -> u64 {
    models::get_setting(db, key)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<Server> {
    let s = models::get_server(&state.db, server_id).map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn list(State(state): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let servers = models::list_servers(&state.db, Some(u.id), false)?;
    let mut out = Vec::new();
    for s in &servers {
        out.push(server_json(&state, s, &u));
    }
    Ok(Json(serde_json::json!({ "data": out })))
}

pub async fn admin_list_all(State(state): State<AppState>, _a: AdminUser) -> ApiResult<Json<serde_json::Value>> {
    let servers = models::list_servers(&state.db, None, false)?;
    let mut out = Vec::new();
    for s in &servers {
        out.push(server_json(&state, s, &User { id: 0, username: String::new(), email: String::new(), avatar: String::new(), language: String::new(), theme: String::new(), root_admin: true, active: true, twofa_secret: None, about: String::new(), created_at: String::new(), updated_at: String::new() }));
    }
    Ok(Json(serde_json::json!({ "data": out })))
}

fn server_json(state: &AppState, s: &Server, u: &User) -> serde_json::Value {
    let info = state.procs.info(s);
    let egg = models::get_egg(&state.db, s.egg_id).ok();
    serde_json::json!({
        "id": s.id,
        "uuid": s.uuid,
        "name": s.name,
        "description": s.description,
        "status": if s.suspended { "suspended" } else { &s.status },
        "node": s.node,
        "port": s.port,
        "memory_mb": s.memory_mb,
        "disk_mb": s.disk_mb,
        "user_id": s.user_id,
        "cpu_percent": s.cpu_percent,
        "egg": egg.map(|e| e.name).unwrap_or_default(),
        "egg_id": s.egg_id,
        "suspended": s.suspended,
        "auto_restart": s.auto_restart,
        "restart_count": s.restart_count,
        "created_at": s.created_at,
        "owner": u.id == s.user_id,
        "info": info,
        "subusers": models::list_subusers(&state.db, s.id).map(|s| s.len()).unwrap_or(0),
        "backup_count": models::list_backups(&state.db, s.id).map(|b| b.len()).unwrap_or(0),
    })
}

pub async fn get(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    let egg = models::get_egg(&state.db, s.egg_id)?;
    let vars: Vec<serde_json::Value> = egg::resolve_variables(&state.db, &s)?
        .into_iter()
        .map(|(v, val)| {
            serde_json::json!({
                "name": v.name,
                "description": v.description,
                "env_var": v.env_var,
                "value": val,
                "default": v.default_value,
                "user_viewable": v.user_viewable,
                "user_editable": v.user_editable,
                "rules": v.rules,
            })
        })
        .collect();
    let allocated = models::ports_for_server(&state.db, s.id)?;
    let websites = if state.cfg.features.enable_websites {
        models::list_websites(&state.db, s.id)?
    } else {
        vec![]
    };
    let databases = models::list_databases(&state.db, s.id)?;
    let subusers = models::list_subusers(&state.db, s.id)?;
    let schedules = models::list_schedules(&state.db, s.id)?;
    Ok(Json(serde_json::json!({
        "server": server_json(&state, &s, &u),
        "egg": egg,
        "variables": vars,
        "ports": allocated,
        "websites": websites,
        "databases": databases,
        "subusers": subusers.iter().map(|(su, perms)| serde_json::json!({
            "id": su.id, "username": su.username, "email": su.email, "permissions": perms
        })).collect::<Vec<_>>(),
        "schedules": schedules,
        "startup": s.startup,
        "docker_image": s.docker_image,
        "resolved_startup": egg::resolve_startup(&state.db, &s).unwrap_or_default(),
    })))
}

#[derive(Deserialize)]
pub struct CreateServerReq {
    pub name: String,
    pub user_id: i64,
    pub egg_id: i64,
    pub description: Option<String>,
    pub docker_image: Option<String>,
    pub memory_mb: Option<i64>,
    pub disk_mb: Option<i64>,
    pub cpu_percent: Option<i64>,
    pub threads: Option<String>,
    pub variables: Option<std::collections::HashMap<String, String>>,
    pub port: Option<i64>,
    pub start_on_create: bool,
    pub node: Option<String>,
    #[serde(default)]
    pub node_tags: Vec<String>,
    pub location: Option<String>,
}

pub async fn create(State(state): State<AppState>, a: AdminUser, Json(req): Json<CreateServerReq>) -> ApiResult<Json<serde_json::Value>> {
    let egg = models::get_egg(&state.db, req.egg_id)?;
    let default_mem = limit_override(&state.db, "limits.default_memory_mb", state.cfg.limits.default_memory_mb);
    let max_mem = limit_override(&state.db, "limits.max_memory_mb", state.cfg.limits.max_memory_mb);
    let mem = req.memory_mb.unwrap_or(default_mem as i64);
    if mem > max_mem as i64 {
        return Err(ApiError::bad_request("memory exceeds node max"));
    }
    let disk = req.disk_mb.unwrap_or(limit_override(&state.db, "limits.default_disk_mb", state.cfg.limits.default_disk_mb) as i64);
    let cpu = req.cpu_percent.unwrap_or(limit_override(&state.db, "limits.default_cpu_percent", state.cfg.limits.default_cpu_percent) as i64);
    let owner = models::get_user(&state.db, req.user_id)?;
    if !owner.root_admin {
        let n = models::count_servers_by_user(&state.db, owner.id)?;
        let max_per_user = limit_override(&state.db, "limits.max_servers_per_user", state.cfg.limits.max_servers_per_user);
        if n >= max_per_user as i64 {
            return Err(ApiError::bad_request("user reached server limit"));
        }
    }
    let chosen_node = match req.node.as_deref() {
        Some("local") | None if req.node_tags.is_empty() && req.location.as_deref().unwrap_or("").is_empty() => None,
        Some(name) if name != "auto" => Some(crate::nodes::get_by_name(&state.db, name)?),
        _ => Some(crate::nodes::select_for_server(&state.db, mem, disk, &req.node_tags, req.location.as_deref())?),
    };
    let target_node_name = chosen_node.as_ref().map(|n| n.name.as_str()).unwrap_or("local");
    if let Some(port) = req.port {
        if !crate::nodes::port_available_on_node(&state.db, target_node_name, port)? {
            return Err(ApiError::bad_request(format!("port {port} is already allocated on node {target_node_name}")));
        }
    }
    let uuid = uuid::Uuid::new_v4().to_string();
    let img = req.docker_image.clone().unwrap_or(egg.docker_image.clone());
    let startup = if egg.startup.is_empty() { String::new() } else { egg.startup.clone() };
    let id = models::create_server(&state.db, &uuid, &req.name, owner.id, egg.id, &img, &startup, mem, disk, cpu, req.threads.as_deref().unwrap_or(""))?;
    // variables
    for (k, v) in req.variables.unwrap_or_default() {
        models::set_server_var(&state.db, id, egg.id, &k, &v)?;
    }
    // default variables from egg
    for var in &egg.variables {
        if models::get_server_vars(&state.db, id)?.iter().all(|(k, _)| k != &var.env_var) {
            models::set_server_var(&state.db, id, egg.id, &var.env_var, &var.default_value)?;
        }
    }
    if let Some(port) = req.port {
        models::allocate_port(&state.db, id, port)?;
    }
    if let Some(node) = &chosen_node {
        if let Err(error) = models::set_server_node(&state.db, id, &node.name) {
            let _ = models::free_ports(&state.db, id);
            let _ = models::purge_server(&state.db, id);
            return Err(ApiError::bad_request(format!("node allocation conflict: {error}")));
        }
    }
    let srv = models::get_server(&state.db, id)?;
    if let Some(node) = &chosen_node {
        let files = egg::build_default_config(&state.db, &srv)?.map(|cfg| vec![crate::node_protocol::ProvisionFile {
            path: "config.json".into(),
            content_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, serde_json::to_vec_pretty(&cfg).unwrap_or_default()),
            mode: Some(0o644),
        }]).unwrap_or_default();
        let spec = crate::node_protocol::ServerSpec {
            uuid: srv.uuid.clone(), name: srv.name.clone(), startup: egg::resolve_startup(&state.db, &srv)?, stop_command: egg.stop_command.clone(),
            memory_mb: mem as u64, disk_mb: disk as u64, cpu_percent: cpu as u64,
            port: req.port.and_then(|p| u16::try_from(p).ok()), ports: models::ports_for_server(&state.db,srv.id)?.into_iter().filter_map(|p|u16::try_from(p).ok()).collect(), env: egg::env_for_server(&state.db, &srv), auto_restart: srv.auto_restart,
        };
        if let Err(e) = state.node_client.provision(node, &crate::node_protocol::ProvisionRequest { spec, files }).await {
            let _ = models::purge_server(&state.db, id);
            return Err(ApiError::new(axum::http::StatusCode::BAD_GATEWAY, format!("node provisioning failed: {e}")));
        }
    } else {
        std::fs::create_dir_all(services::proc::server_dir(&srv))?;
        if let Ok(Some(cfg)) = egg::build_default_config(&state.db, &srv) {
            let _ = services::files::write_file(&state.cfg, &srv, "config.json", &serde_json::to_vec_pretty(&cfg)?);
        }
    }
    state.monitor.set_limit(services::ServerLimits {
        server_id: id,
        memory_mb: mem as u64,
        cpu_percent: cpu as u64,
        bandwidth_rx: 0,
        bandwidth_tx: 0,
    });
    models::audit(&state.db, Some(a.0.id), "server_create", &format!("server #{id}"), "", &req.name)?;
    if req.start_on_create {
        let srv = models::get_server(&state.db, id)?;
        if let Some(node) = &chosen_node {
            state.node_client.power(node, &srv.uuid, crate::node_protocol::PowerAction::Start).await?;
        } else {
            let cmd = egg::resolve_startup(&state.db, &srv)?;
            let env = egg::env_for_server(&state.db, &srv);
            state.procs.start(&srv, &cmd, &env, state.notifier.clone())?;
        }
    }
    Ok(Json(server_json(&state, &models::get_server(&state.db, id)?, &owner)))
}

#[derive(Deserialize)]
pub struct UpdateServerReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub docker_image: Option<String>,
    pub startup: Option<String>,
    pub memory_mb: Option<i64>,
    pub disk_mb: Option<i64>,
    pub cpu_percent: Option<i64>,
    pub threads: Option<String>,
    pub auto_restart: Option<bool>,
    pub ignore_oom: Option<bool>,
}

pub async fn update(State(state): State<AppState>, _a: AdminUser, Path(id): Path<i64>, Json(req): Json<UpdateServerReq>) -> ApiResult<Json<serde_json::Value>> {
    let mut s = models::get_server(&state.db, id)?;
    if let Some(name) = req.name {
        s.name = name;
    }
    if let Some(desc) = req.description {
        s.description = desc;
    }
    if let Some(img) = req.docker_image {
        s.docker_image = img;
    }
    if let Some(startup) = req.startup {
        s.startup = startup;
    }
    if let Some(mem) = req.memory_mb {
        let max_mem = limit_override(&state.db, "limits.max_memory_mb", state.cfg.limits.max_memory_mb);
        if mem > max_mem as i64 {
            return Err(ApiError::bad_request("memory exceeds node max"));
        }
        s.memory_mb = mem;
    }
    if let Some(disk) = req.disk_mb {
        s.disk_mb = disk;
    }
    if let Some(cpu) = req.cpu_percent {
        s.cpu_percent = cpu;
    }
    if let Some(threads) = req.threads {
        s.threads = threads;
    }
    if let Some(ar) = req.auto_restart {
        s.auto_restart = ar;
    }
    if let Some(oom) = req.ignore_oom {
        s.ignore_oom = oom;
    }
    models::update_server(&state.db, &s)?;
    state.monitor.set_limit(services::ServerLimits {
        server_id: s.id,
        memory_mb: s.memory_mb as u64,
        cpu_percent: s.cpu_percent as u64,
        bandwidth_rx: 0,
        bandwidth_tx: 0,
    });
    Ok(Json(server_json(&state, &s, &User { id: 0, username: String::new(), email: String::new(), avatar: String::new(), language: String::new(), theme: String::new(), root_admin: true, active: true, twofa_secret: None, about: String::new(), created_at: String::new(), updated_at: String::new() })))
}

#[derive(Deserialize)]
pub struct UpdateVarsReq {
    pub variables: std::collections::HashMap<String, String>,
}

pub async fn update_vars(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<UpdateVarsReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"startup.update")?;
    let egg = models::get_egg(&state.db, s.egg_id)?;
    for (k, v) in &req.variables {
        let Some(var) = egg.variables.iter().find(|v| &v.env_var == k) else {
            return Err(ApiError::bad_request(format!("unknown variable {k}")));
        };
        if !var.user_editable && !u.root_admin {
            return Err(ApiError::forbidden("variable not editable"));
        }
        egg::validate_value(var, v)?;
        models::set_server_var(&state.db, s.id, egg.id, k, v)?;
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn delete(State(state): State<AppState>, a: AdminUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = models::get_server(&state.db, id)?;
    if s.node != "local" { let node=crate::nodes::get_by_name(&state.db,&s.node)?; let _=state.node_client.power(&node,&s.uuid,crate::node_protocol::PowerAction::Stop).await; }
    else { state.procs.stop(s.id)?; state.procs.remove_limits(s.id); state.monitor.remove_limit(s.id); crate::services::console::drop_server(&state.hub,s.id); }
    models::delete_server(&state.db,s.id)?;
    models::audit(&state.db,Some(a.0.id),"server_delete",&format!("server #{id}"),"",&format!("{} node={}",s.name,s.node))?;
    Ok(ok(serde_json::json!({"ok":true})))
}

/// Physical removal of dir + DB row (admin).
pub async fn purge(State(state): State<AppState>, a: AdminUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = models::get_server(&state.db, id)?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let _ = state.node_client.power(&node, &s.uuid, crate::node_protocol::PowerAction::Kill).await;
        state.node_client.delete_server(&node, &s.uuid).await?;
    } else {
        let _ = state.procs.stop(s.id);
        state.procs.remove_limits(s.id);
        state.monitor.remove_limit(s.id);
        crate::services::console::drop_server(&state.hub, s.id);
        for b in models::list_backups(&state.db, s.id)? { let _ = std::fs::remove_file(&b.path); }
        let _ = std::fs::remove_dir_all(services::proc::server_dir(&s));
    }
    models::free_ports(&state.db, s.id)?;
    models::delete_server_vars(&state.db, s.id)?;
    models::purge_server(&state.db, s.id)?;
    models::audit(&state.db, Some(a.0.id), "server_purge", &format!("server #{id}"), "", &format!("{} node={}", s.name, s.node))?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PowerReq {
    pub action: String, // start | stop | restart | kill
}

pub async fn power(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<PowerReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,&format!("control.{}",req.action))?;
    if s.suspended && !u.root_admin { return Err(ApiError::forbidden("server suspended")); }
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let action = match req.action.as_str() {
            "start" => crate::node_protocol::PowerAction::Start,
            "stop" => crate::node_protocol::PowerAction::Stop,
            "restart" => crate::node_protocol::PowerAction::Restart,
            "kill" => crate::node_protocol::PowerAction::Kill,
            other => return Err(ApiError::bad_request(format!("unknown action {other}"))),
        };
        let stats = state.node_client.power(&node, &s.uuid, action).await?;
        models::set_server_status(&state.db, s.id, &stats.state)?;
        models::audit(&state.db, Some(u.id), &format!("server_{}", req.action), &s.name, "", &format!("node={}", node.name))?;
        return Ok(Json(serde_json::json!({ "ok": true, "remote": true, "stats": stats })));
    }
    match req.action.as_str() {
        "start" => { let cmd = egg::resolve_startup(&state.db, &s)?; let env = egg::env_for_server(&state.db, &s); state.procs.start(&s, &cmd, &env, state.notifier.clone())?; }
        "stop" => state.procs.stop(s.id)?,
        "restart" => { state.procs.stop(s.id)?; tokio::time::sleep(std::time::Duration::from_millis(300)).await; let s = models::get_server(&state.db, s.id)?; let cmd = egg::resolve_startup(&state.db, &s)?; let env = egg::env_for_server(&state.db, &s); state.procs.start(&s, &cmd, &env, state.notifier.clone())?; }
        "kill" => state.procs.kill(s.id)?,
        other => return Err(ApiError::bad_request(format!("unknown action {other}"))),
    }
    models::audit(&state.db, Some(u.id), &format!("server_{}", req.action), &s.name, "", "node=local")?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct InstallReq {
    pub script: Option<String>,
}

/// Run the egg's install script (re-install).
pub async fn install(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<InstallReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"startup.install")?;
    if s.node != "local" {
        return Err(ApiError::new(axum::http::StatusCode::NOT_IMPLEMENTED, "remote egg installation must run through the node installer"));
    }
    let egg = models::get_egg(&state.db, s.egg_id)?;
    let mut egg = egg;
    if let Some(script) = req.script {
        if !u.root_admin { return Err(ApiError::forbidden("custom install scripts require administrator access")); }
        egg.install_script = Some(script);
    }
    tokio::task::spawn_blocking(move || egg::run_install(&state.db, &s, &egg, &state.notifier))
        .await
        .map_err(|e| ApiError::new(axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("install worker failed: {e}")))??;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn suspend(State(state): State<AppState>, _a: AdminUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s=models::get_server(&state.db,id)?;
    if s.node!="local" { let n=crate::nodes::get_by_name(&state.db,&s.node)?; let _=state.node_client.power(&n,&s.uuid,crate::node_protocol::PowerAction::Stop).await?; }
    else { state.procs.stop(s.id)?; }
    models::set_server_suspended(&state.db,s.id,true)?;
    Ok(ok(serde_json::json!({"ok":true})))
}

pub async fn unsuspend(State(state): State<AppState>, _a: AdminUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = models::get_server(&state.db, id)?;
    models::set_server_suspended(&state.db, s.id, false)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- Subusers ----------------

pub async fn list_subusers(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, id)?;
    let subs = models::list_subusers(&state.db, id)?;
    let out: Vec<serde_json::Value> = subs
        .into_iter()
        .map(|(su, perms)| serde_json::json!({ "id": su.id, "username": su.username, "email": su.email, "permissions": perms }))
        .collect();
    Ok(data(serde_json::json!(out)))
}

#[derive(Deserialize)]
pub struct AddSubuserReq {
    pub user_id: i64,
    pub permissions: Vec<String>,
}

pub async fn add_subuser(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<AddSubuserReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    if s.user_id != u.id && !u.root_admin {
        return Err(ApiError::forbidden("only owner can manage subusers"));
    }
    models::add_subuser(&state.db, id, req.user_id, &req.permissions)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn remove_subuser(State(state): State<AppState>, AuthUser(u): AuthUser, Path((id, sub_id)): Path<(i64, i64)>, ) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    if s.user_id != u.id && !u.root_admin {
        return Err(ApiError::forbidden("only owner can manage subusers"));
    }
    models::remove_subuser(&state.db, id, sub_id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- Stats ----------------

pub async fn stats(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        let info = state.node_client.stats(&node, &s.uuid).await?;
        return Ok(Json(serde_json::json!({
            "cpu": info.cpu_percent, "memory_bytes": info.memory_bytes, "memory_mb": info.memory_bytes / 1_048_576,
            "memory_limit_mb": s.memory_mb, "memory_percent": if s.memory_mb > 0 { info.memory_bytes as f64 / (s.memory_mb as f64 * 1_048_576.0) * 100.0 } else { 0.0 },
            "disk_bytes": info.disk_bytes, "disk_mb": info.disk_bytes / 1_048_576, "disk_limit_mb": s.disk_mb,
            "rx_bytes": info.network_rx_bytes, "tx_bytes": info.network_tx_bytes, "uptime_secs": info.uptime_secs,
            "pid": info.pid, "status": info.state, "node": node.name, "remote": true,
        })));
    }
    let info = state.procs.info(&s);
    let disk = services::proc::fs_usage(&services::proc::server_dir(&s)).unwrap_or(0);
    Ok(Json(serde_json::json!({ "cpu": info.cpu_percent, "memory_bytes": info.memory_bytes, "memory_mb": info.memory_bytes / 1_048_576,
        "memory_limit_mb": s.memory_mb, "memory_percent": info.memory_percent, "disk_bytes": disk, "disk_mb": disk / 1_048_576,
        "disk_limit_mb": s.disk_mb, "rx_bytes": info.bandwidth_rx_bytes, "tx_bytes": info.bandwidth_tx_bytes,
        "uptime_secs": info.uptime_secs, "pid": info.pid, "status": info.status, "node": "local", "remote": false })))
}

#[derive(Deserialize)]
pub struct SendCmdReq {
    pub command: String,
}

pub async fn send_command(State(state): State<AppState>, AuthUser(u): AuthUser, Path(id): Path<i64>, Json(req): Json<SendCmdReq>) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id)?;
    super::require_server_permission(&state,&u,id,"console.write")?;
    if s.node != "local" {
        let node = crate::nodes::get_by_name(&state.db, &s.node)?;
        state.node_client.command(&node, &s.uuid, &req.command).await?;
    } else { state.procs.send_input(s.id, &format!("{}\n", req.command))?; }
    Ok(ok(serde_json::json!({ "ok": true, "node": s.node })))
}
