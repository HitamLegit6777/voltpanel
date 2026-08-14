//! System endpoints: node stats, health, metrics, allocations.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::auth;
use crate::db::blocking;
use crate::models;
use crate::services;
use axum::extract::{Path, Query, State};
use axum::Json;
use parking_lot::Mutex;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use serde::Deserialize;
use serde_json::json;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread. Module-owned
// SQL rides `Db::call(|conn| ...)`; pool-based `models`/`nodes` functions
// (they do their own `pool.get()` and cannot run inside a `db.call` closure
// without a nested checkout) ride `blocking(...)` on Tokio's blocking pool.
// One rule: never hold a pooled connection across an `.await`.

/// Node stats rescan every process on the host via sysinfo (tens of ms per
/// call), so the admin stats endpoint throttles the scan behind a 5s TTL.
/// The pure `services::node_stats()` is untouched; only this route caches.
/// The guard is held across the scan: concurrent callers serialize behind the
/// in-flight computation instead of stampeding N identical scans.
fn cached_node_stats() -> services::NodeStats {
    static CACHE: LazyLock<Mutex<Option<(services::NodeStats, Instant)>>> =
        LazyLock::new(|| Mutex::new(None));
    let mut cache = CACHE.lock();
    if let Some((stats, ts)) = cache.as_ref() {
        if ts.elapsed() < Duration::from_secs(5) {
            return stats.clone();
        }
    }
    let stats = services::node_stats();
    *cache = Some((stats.clone(), Instant::now()));
    stats
}


pub async fn node_stats(
    _state: State<AppState>,
    _u: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let s = cached_node_stats();
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
    _u: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let (integrity, server_count) = blocking(state.db.clone(), move |db| {
        Ok((
            crate::db::integrity_check(&db).unwrap_or_else(|_| "unknown".into()),
            models::count_servers(&db).unwrap_or(-1),
        ))
    })
    .await?;
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
        "isolation": "native",
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
    _a: AdminUser,
    Query(q): Query<PortRangeQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let start = q.start.unwrap_or(20000);
    let end = q.end.unwrap_or(30000);
    if start < 1 || end > 65_535 || start > end || end - start > 10_000 {
        return Err(ApiError::bad_request(
            "port range must be ordered, within 1..=65535, and at most 10001 ports",
        ));
    }
    let used: Vec<i64> = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare("SELECT port FROM allocations")?;
            let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .await?;
    let used_set: std::collections::HashSet<i64> = used.into_iter().collect();
    let host_used = host_ports_in_use();
    let mut free = Vec::new();
    for p in start..=end {
        if !used_set.contains(&p) && !host_used.contains(&p) {
            free.push(p);
        }
        if free.len() >= 20 {
            break;
        }
    }
    Ok(data(json!(free)))
}

fn host_ports_in_use() -> std::collections::HashSet<i64> {
    use std::io::Read;
    let mut ports = std::collections::HashSet::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let mut text = String::new();
        if file.read_to_string(&mut text).is_err() {
            continue;
        }
        for line in text.lines().skip(1) {
            let Some(local) = line.split_whitespace().nth(1) else {
                continue;
            };
            let Some(port_hex) = local.rsplit(':').next() else {
                continue;
            };
            if let Ok(port) = i64::from_str_radix(port_hex, 16) {
                ports.insert(port);
            }
        }
    }
    ports
}

pub(crate) fn port_in_use(port: i64) -> bool {
    host_ports_in_use().contains(&port)
}

pub async fn allocations(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let out: Vec<serde_json::Value> = state
        .db
        .call(move |conn| {
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
            Ok(out)
        })
        .await?;
    Ok(data(json!(out)))
}

pub async fn assign_port(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((server_id, port)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(state.db.clone(), move |db| models::get_server(&db, server_id))
        .await
        .map_err(|_| ApiError::not_found("server not found"))?;
    if port_in_use(port) {
        return Err(ApiError::bad_request("port already in use on host"));
    }
    blocking(state.db.clone(), move |db| {
        models::allocate_port(&db, server_id, port)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn remove_port(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((server_id, port)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    // Route through the allocation machinery instead of a bare DELETE: the
    // primary port is guarded (a workload would lose the port it runs on) and
    // a missing allocation is a 404, not a silent no-op.
    let alloc = blocking(state.db.clone(), move |db| {
        models::list_allocations(&db, server_id)
    })
    .await?
    .into_iter()
    .find(|a| a.port == port)
    .ok_or_else(|| ApiError::not_found("allocation not found"))?;
    if alloc.is_primary {
        return Err(ApiError::conflict(
            "cannot remove the primary allocation; promote another port first",
        ));
    }
    blocking(state.db.clone(), move |db| {
        models::remove_allocation(&db, alloc.id)
    })
    .await?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn rate_limits_status(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let out: Vec<serde_json::Value> = state
        .db
        .call(move |conn| {
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
            Ok(out)
        })
        .await?;
    Ok(data(json!(out)))
}

pub async fn reset_rate_limits(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(state.db.clone(), move |db| models::reset_rate_limits(&db)).await?;
    auth::reset_rate_limits();
    Ok(ok(json!({ "ok": true })))
}

/// Per-server limit override (admin).
///
/// `memory_mb` is MiB, `cpu_percent` is percent. Bandwidth limits are
/// **bytes per 5s interval**: the monitor sweep samples every 5s and compares
/// the cumulative network-counter delta against the cap, so a `bandwidth_rx`
/// of 1_000_000 allows ~1 MB of ingress per 5s (~200 KB/s). `0` disables the
/// cap for that direction; the field type stays `u64`.
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
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, server_id))
        .await
        .map_err(|_| ApiError::not_found("server not found"))?;
    if req.memory_mb == Some(0) {
        return Err(ApiError::bad_request(
            "memory limit must be positive (0 would disable the cap)",
        ));
    }
    if req.cpu_percent == Some(0) {
        return Err(ApiError::bad_request("cpu limit must be positive"));
    }
    // Bandwidth caps are bytes per 5s sweep interval; 0 disables the cap.
    // Upper bound: 1 TiB per 5s (~200 GiB/s) — no real link approaches this,
    // so a larger value is a typo, not a config.
    const MAX_BANDWIDTH_PER_5S: u64 = 1 << 40;
    for (name, v) in [
        ("bandwidth_rx", req.bandwidth_rx),
        ("bandwidth_tx", req.bandwidth_tx),
    ] {
        if let Some(v) = v {
            if v > MAX_BANDWIDTH_PER_5S {
                return Err(ApiError::bad_request(format!(
                    "{name} must be at most {MAX_BANDWIDTH_PER_5S} bytes per 5s (0 disables the cap)"
                )));
            }
        }
    }
    // Guard the sweep's `memory_mb * 1024 * 1024` against overflow; the cap
    // itself is enforced against a live sample, so this is just a sane bound.
    if let Some(m) = req.memory_mb {
        if m > u64::MAX / (1024 * 1024) {
            return Err(ApiError::bad_request("memory limit too large"));
        }
    }
    let mut memory_mb = req.memory_mb.unwrap_or(s.memory_mb as u64);
    // Floor the memory cap at what the workload is live-using right now: a cap
    // below current usage would OOM-kill on the next enforcement tick. Best
    // effort — an unreachable agent or a raced read keeps the requested cap.
    let live_mem_mb: u64 = if s.node != "local" {
        let node_name = s.node.clone();
        match blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &node_name)
        })
        .await
        {
            Ok(node) => state
                .node_client
                .stats(&node, &s.uuid)
                .await
                .map(|i| i.memory_bytes / 1_048_576)
                .unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        state.procs.info(&s).memory_bytes / 1_048_576
    };
    memory_mb = memory_mb.max(live_mem_mb);
    state.monitor.set_limit(services::ServerLimits {
        server_id,
        memory_mb,
        cpu_percent: req.cpu_percent.unwrap_or(s.cpu_percent as u64),
        bandwidth_rx: req.bandwidth_rx.unwrap_or(0),
        bandwidth_tx: req.bandwidth_tx.unwrap_or(0),
    });
    Ok(ok(json!({
        "ok": true,
        "memory_mb": memory_mb,
        "cpu_percent": req.cpu_percent.unwrap_or(s.cpu_percent as u64),
        "bandwidth_rx": req.bandwidth_rx.unwrap_or(0),
        "bandwidth_tx": req.bandwidth_tx.unwrap_or(0),
        "bandwidth_unit": "bytes_per_5s",
    })))
}

/// Websocket-less live stats polling helper endpoint.
pub async fn live_stats(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let servers = blocking(state.db.clone(), move |db| {
        models::list_servers(&db, None, false)
    })
    .await?;
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

/// Grantable capability surface and the role presets built from it.
///
/// `capability.rs` documents this route as the discovery endpoint for
/// `describe()`; subuser editors read it instead of hardcoding the enum.
pub async fn capabilities(
    State(_state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    use crate::capability::{Capability, Role};
    let caps: Vec<serde_json::Value> = Capability::ALL
        .into_iter()
        .map(|c| {
            json!({
                "name": c.as_str(),
                "group": c.category(),
                "description": c.describe(),
            })
        })
        .collect();
    let roles: Vec<serde_json::Value> = Role::ALL
        .into_iter()
        .map(|r| {
            json!({
                "name": r.as_str(),
                "capabilities": r
                    .capabilities()
                    .into_iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(data(json!({ "capabilities": caps, "roles": roles })))
}