//! System endpoints: node stats, health, metrics, allocations.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::db;
use crate::models;
use crate::services;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

pub async fn node_stats(
    _state: State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let s = services::node_stats();
    Ok(Json(json!({
        "cpu": {
            "frequency_mhz": s.cpu_total,
            "usage_percent": s.cpu_percent,
        },
        "memory": {
            "total": s.mem_total,
            "used": s.mem_used,
            "percent": s.mem_percent,
        },
        "disk": {
            "total": s.disk_total,
            "used": s.disk_used,
            "percent": s.disk_percent,
        },
        "load": { "1": s.load_1, "5": s.load_5, "15": s.load_15 },
        "uptime_secs": s.uptime_secs,
        "processes": s.processes,
    })))
}

pub async fn health(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let integrity = db::integrity_check(&state.db).unwrap_or_else(|_| "unknown".into());
    let server_count = models::count_servers(&state.db).unwrap_or(-1);
    let isolation = crate::isolation::probe(&crate::isolation::IsolationConfig::default());
    Ok(Json(json!({
        "status": if integrity == "ok" && isolation.secure { "ok" } else { "degraded" },
        "db_integrity": integrity, "servers": server_count, "version": env!("CARGO_PKG_VERSION"), "isolation": isolation,
    })))
}

pub async fn isolation(
    State(_state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let config = crate::isolation::IsolationConfig::default();
    let status = crate::isolation::probe(&config);
    Ok(Json(serde_json::to_value(status)?))
}

pub async fn version(_state: State<AppState>, _u: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "voltpanel",
        "rust": std::env::var("RUST_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_RUST_VERSION").to_string()),
        "no_docker": true,
    })))
}

#[derive(Deserialize)]
pub struct PortRangeQuery {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Find free ports in a range (allocation table aware).
pub async fn free_ports(
    State(state): State<AppState>,
    _u: AuthUser,
    Query(q): Query<PortRangeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let start = q.start.unwrap_or(20000);
    let end = q.end.unwrap_or(30000);
    let conn = state.db.lock();
    let used: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT port FROM allocations")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r?);
        }
        v
    };
    drop(conn);
    let used_set: std::collections::HashSet<i64> = used.into_iter().collect();
    let mut free = Vec::new();
    for p in start..=end {
        if !used_set.contains(&p) && !port_in_use(p) {
            free.push(p);
        }
        if free.len() >= 20 {
            break;
        }
    }
    Ok(data(json!(free)))
}

fn port_in_use(port: i64) -> bool {
    use std::io::Read;
    // check /proc/net/tcp + tcp6 quickly
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(mut file) = std::fs::File::open(f) {
            let mut s = String::new();
            if file.read_to_string(&mut s).is_ok() {
                let hex_port = format!("{:04X}", port);
                for line in s.lines().skip(1) {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    if fields.len() > 1 && fields[1].ends_with(&hex_port) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub async fn allocations(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let mut stmt = conn.prepare("SELECT a.server_id, a.port, s.name FROM allocations a LEFT JOIN servers s ON s.id=a.server_id ORDER BY a.port")?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "server_id": r.get::<_, i64>(0)?,
            "port": r.get::<_, i64>(1)?,
            "server": r.get::<_, Option<String>>(2)?,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(data(json!(out)))
}

pub async fn assign_port(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((server_id, port)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    if port_in_use(port) {
        return Err(ApiError::bad_request("port already in use on host"));
    }
    models::allocate_port(&state.db, server_id, port)?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn remove_port(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((server_id, port)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    conn.execute(
        "DELETE FROM allocations WHERE server_id=?1 AND port=?2",
        rusqlite::params![server_id, port],
    )?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn rate_limits_status(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let mut stmt = conn
        .prepare("SELECT key, window_start, count FROM rate_limits ORDER BY count DESC LIMIT 50")?;
    let rows = stmt.query_map([], |r| {
        Ok(json!({
            "key": r.get::<_, String>(0)?,
            "window_start": r.get::<_, i64>(1)?,
            "count": r.get::<_, i64>(2)?,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(data(json!(out)))
}

pub async fn reset_rate_limits(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    models::reset_rate_limits(&state.db)?;
    Ok(ok(json!({ "ok": true })))
}

/// Per-server limit override (admin).
#[derive(Deserialize)]
pub struct LimitOverride {
    pub memory_mb: Option<u64>,
    pub cpu_percent: Option<u64>,
    pub bandwidth_rx: Option<u64>,
    pub bandwidth_tx: Option<u64>,
}

pub async fn set_server_limits(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(server_id): Path<i64>,
    Json(req): Json<LimitOverride>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    state.monitor.set_limit(services::ServerLimits {
        server_id,
        memory_mb: req.memory_mb.unwrap_or(s.memory_mb as u64),
        cpu_percent: req.cpu_percent.unwrap_or(s.cpu_percent as u64),
        bandwidth_rx: req.bandwidth_rx.unwrap_or(0),
        bandwidth_tx: req.bandwidth_tx.unwrap_or(0),
    });
    Ok(ok(json!({ "ok": true })))
}

/// Websocket-less live stats polling helper endpoint.
pub async fn live_stats(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let servers = models::list_servers(&state.db, None, false)?;
    let out: Vec<serde_json::Value> = servers
        .iter()
        .map(|s| {
            let info = state.procs.info(s);
            json!({
                "id": s.id,
                "status": s.status,
                "cpu": info.cpu_percent,
                "mem": info.memory_bytes,
                "rx": info.bandwidth_rx_bytes,
                "tx": info.bandwidth_tx_bytes,
            })
        })
        .collect();
    Ok(data(json!(out)))
}
