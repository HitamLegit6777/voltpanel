//! Workspace management endpoints: lifecycle, launch inputs, team access, and placement.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::{power_capability, Capability, Grant, Role};
use crate::db::blocking;
use crate::models::{self, Server, User};
use crate::node_protocol::PowerAction;
use crate::services::{self, blueprint};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread. Three tools,
// one rule — never hold a pooled connection across an `.await`:
//   - `Db::call(|conn| ...)` for SQL this module owns (the create
//     transaction, the settings lookup below). The closure receives a pooled
//     connection and runs its SQL directly on it.
//   - `blocking(...)` for the pool-based `models`/`nodes`/`blueprint`
//     functions: they take `&Db` and do their own `pool.get()`, so they
//     cannot run inside a `db.call` closure (a nested checkout would double
//     the connection cost and can exhaust the 8-connection pool). They ride
//     Tokio's blocking pool via the shared [`crate::db::blocking`] helper —
//     the same idiom `install()` below and `services::backups` already use.
//   - `Db::run(|conn| ...)` for sync contexts that cannot await, e.g.
//     `CreateRollback::drop` below.
// ponytail: the `*_on` helpers mirror the handful of one-statement models
// queries this module needs inside closures (models is pool-based and frozen
// this iteration). When models grows conn-based variants, delete the mirrors
// and call those instead.


/// conn-based mirror of `models::get_setting` for `Db::call` closures.
fn get_setting_on(conn: &mut rusqlite::Connection, key: &str) -> anyhow::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

/// conn-based mirror of `models::free_ports` for `Db::run` (sync contexts).
fn free_ports_on(conn: &mut rusqlite::Connection, server_id: i64) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM allocations WHERE server_id=?1", [server_id])?;
    tx.execute(
        "UPDATE servers SET port=NULL,updated_at=?1 WHERE id=?2",
        params![chrono::Utc::now().to_rfc3339(), server_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// conn-based mirror of `models::delete_server_vars` for `Db::run`.
fn delete_server_vars_on(conn: &mut rusqlite::Connection, server_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM server_variables WHERE server_id=?1",
        [server_id],
    )?;
    Ok(())
}

/// conn-based mirror of `models::purge_server` for `Db::run`.
fn purge_server_on(conn: &mut rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    conn.execute("DELETE FROM servers WHERE id=?1", [id])?;
    Ok(())
}

/// Errors the create transaction needs to surface with a specific HTTP status
/// that a raw `rusqlite` error cannot carry.
#[derive(Debug)]
enum CreateTxError {
    Quota,
    PortConflict(String),
}
impl std::fmt::Display for CreateTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateTxError::Quota => write!(f, "owner reached workspace limit"),
            CreateTxError::PortConflict(msg) => write!(f, "{msg}"),
        }
    }
}
impl std::error::Error for CreateTxError {}

/// Runtime limit override from the settings table, falling back to config.
async fn limit_override(db: &crate::db::Db, key: &'static str, fallback: u64) -> u64 {
    db.call(move |conn| get_setting_on(conn, key))
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}
fn validate_resources(memory_mb: i64, disk_mb: i64, cpu_percent: i64) -> ApiResult<()> {
    if memory_mb <= 0 {
        return Err(ApiError::bad_request("memory_mb must be positive"));
    }
    if disk_mb <= 0 {
        return Err(ApiError::bad_request("disk_mb must be positive"));
    }
    if !(1..=10_000).contains(&cpu_percent) {
        return Err(ApiError::bad_request(
            "cpu_percent must be between 1 and 10000",
        ));
    }
    Ok(())
}

/// Workspace names are operator-facing labels shown in lists, audit entries,
/// and agent specs. Blank or oversized values break those surfaces, so the
/// trimmed value is validated here and is what callers must store.
fn validate_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be blank"));
    }
    if name.chars().count() > 128 {
        return Err(ApiError::bad_request(
            "name must contain at most 128 characters",
        ));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(ApiError::bad_request(
            "name must not contain control characters",
        ));
    }
    Ok(name.to_string())
}
struct CreateRollback {
    db: crate::db::Db,
    id: i64,
    committed: bool,
    monitor: Arc<services::Monitor>,
    node_client: Arc<crate::services::node::NodeClient>,
    /// Agent the row was provisioned on; set once provisioning begins so an
    /// early var failure never pokes an agent that never saw the server.
    node: Option<crate::nodes::Node>,
    uuid: String,
    /// Auto-placement capacity claim, released (best-effort) when the create
    /// fails after commit. On success it is deliberately KEPT: it protects the
    /// node until the agent's next heartbeat reports the new workload's usage.
    reservation: Option<i64>,
}
impl Drop for CreateRollback {
    fn drop(&mut self) {
        if !self.committed {
            self.monitor.remove_limit(self.id);
            if let Some(rid) = self.reservation {
                let _ = crate::nodes::release_reservation(&self.db, rid);
            }
            // Best-effort row cleanup via the sync `Db::run` variant: Drop
            // cannot await, so the mirrors run inline on the dropping thread.
            let _ = self.db.run(|conn| free_ports_on(conn, self.id));
            let _ = self.db.run(|conn| delete_server_vars_on(conn, self.id));
            let _ = self.db.run(|conn| purge_server_on(conn, self.id));
            // Best-effort agent cleanup: a failed provision or start may have
            // left files/cgroup on the agent, so ask it to delete the server
            // even though we are giving up the DB row. Detached because Drop
            // cannot await; a runtime without a live handle just skips it.
            if let (Some(node), Ok(handle)) = (&self.node, tokio::runtime::Handle::try_current()) {
                let client = self.node_client.clone();
                let node = node.clone();
                let uuid = self.uuid.clone();
                std::mem::drop(handle.spawn(async move {
                    let _ = client.delete_server(&node, &uuid).await;
                }));
            }
        }
    }
}

async fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<Server> {
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

/// Suspended servers are frozen: lifecycle, install, env updates and console
/// commands are refused for non-admins (root admins may still recover them).
fn ensure_operational(s: &Server, u: &User) -> ApiResult<()> {
    if s.suspended && !u.root_admin {
        return Err(ApiError::forbidden("server suspended"));
    }
    Ok(())
}

/// Lifecycle state conflicts (start on a running server, input to a stopped
/// one) are client errors, not server faults: surface them as 409.
fn lifecycle_error(e: anyhow::Error) -> ApiError {
    let msg = e.to_string();
    if msg.contains("already running")
        || msg.contains("not running")
        || msg.contains("server is stopping")
        || msg.contains("must be stopped")
    {
        ApiError::conflict(msg)
    } else {
        ApiError::from(e)
    }
}

/// Console stdin write failures map to the console API contract: a backlogged
/// per-server writer is 503, a child that never drains is 408, and any other
/// write failure (child exited, stdin unavailable) is 400.
fn stdin_error(e: crate::services::console::StdinError) -> ApiError {
    use axum::http::StatusCode;
    match e {
        crate::services::console::StdinError::Busy => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "console input backlog full; the server process may not be draining stdin",
        ),
        crate::services::console::StdinError::TimedOut => ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "console command write timed out",
        ),
        crate::services::console::StdinError::WriteFailed(msg) => {
            ApiError::bad_request(format!("console command failed: {msg}"))
        }
    }
}

/// Fleet list pagination. Applied after the scope filter (see `list`): the
/// page window bounds the per-row work (batching, cached sampling) and the
/// response size, which is where the endpoint's cost was — the SQL fetch of
/// all rows is cheap by comparison. SQL-side `LIMIT` pushdown would need a
/// `list_servers` limit parameter in models.rs, which is a fixed contract
/// this module does not own.
#[derive(Deserialize)]
pub struct ListQuery {
    pub page: Option<u64>,
    pub limit: Option<u64>,
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = u.root_admin;
    let uid = u.id;
    let mut servers = blocking(state.db.clone(), move |db| {
        if root {
            // Root admins hold every workspace, not just the ones they own or
            // are team members on — match `user_has_server_access`'s root
            // shortcut.
            models::list_servers(&db, None, false)
        } else {
            models::list_servers(&db, Some(uid), false)
        }
    })
    .await?;
    // A scoped API key sees only the servers named in its server_ids: the
    // wildcard capability narrows capabilities, never the server set, and an
    // empty server_ids means "every server the owner can see". Session and
    // browser users have no key scope and are untouched.
    if let Some(ids) = scoped_server_ids(&u) {
        servers.retain(|s| ids.contains(&s.id));
    }
    let total = servers.len();
    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let paged: Vec<Server> = servers
        .into_iter()
        .skip(((page - 1).saturating_mul(limit)) as usize)
        .take(limit as usize)
        .collect();
    // Three batched queries (blueprint names, subuser counts, backup counts)
    // replace the 3N per-server queries the list path used to run.
    let (paged, meta) = blocking(state.db.clone(), move |db| {
        let meta = ServerListMeta::load(&db, &paged);
        Ok((paged, meta))
    })
    .await?;
    let mut out = Vec::new();
    for s in &paged {
        out.push(server_json_meta(&state, s, &u, &meta));
    }
    Ok(Json(serde_json::json!({
        "data": out,
        "total": total,
        "page": page,
        "limit": limit,
    })))
}

/// The server_ids a key-scoped user is restricted to, or None when the user
/// may list everything `list_servers` already permits (session user, or a key
/// with empty server_ids).
fn scoped_server_ids(u: &User) -> Option<&[i64]> {
    u.key_scope
        .as_ref()
        .map(|scope| scope.server_ids.as_slice())
        .filter(|ids| !ids.is_empty())
}
pub async fn admin_list_all(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let (servers, meta) = blocking(state.db.clone(), move |db| {
        let servers = models::list_servers(&db, None, false)?;
        let meta = ServerListMeta::load(&db, &servers);
        Ok((servers, meta))
    })
    .await?;
    let mut out = Vec::new();
    for s in &servers {
        out.push(server_json_meta(&state, s, &admin, &meta));
    }
    Ok(Json(serde_json::json!({ "data": out })))
}

/// Per-server metadata for list rendering, batch-loaded with three queries
/// (blueprint names, subuser counts, backup counts) where the old per-row
/// path ran one query per server per metric and discarded the rows it fetched
/// just to take `.len()`.
///
/// Best-effort like the per-row code: a query failure degrades to the same
/// defaults it produced (empty blueprint name, zero counts).
#[derive(Default)]
struct ServerListMeta {
    blueprints: HashMap<i64, String>,
    subusers: HashMap<i64, usize>,
    backups: HashMap<i64, usize>,
}

impl ServerListMeta {
    fn load(db: &crate::db::Db, servers: &[Server]) -> Self {
        let mut meta = ServerListMeta {
            blueprints: HashMap::new(),
            subusers: HashMap::new(),
            backups: HashMap::new(),
        };
        if servers.is_empty() {
            return meta;
        }
        let Ok(conn) = db.get() else {
            return meta;
        };
        let ids: Vec<i64> = servers.iter().map(|s| s.id).collect();
        let blueprint_ids: Vec<i64> = servers.iter().map(|s| s.blueprint_id).collect();
        let in_clause = |n: usize| vec!["?"; n].join(",");
        // Blueprint names.
        if let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT id, name FROM blueprints WHERE id IN ({})",
            in_clause(blueprint_ids.len())
        )) {
            if let Ok(rows) = stmt
                .query_map(rusqlite::params_from_iter(blueprint_ids.iter()), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })
            {
                for row in rows.flatten() {
                    meta.blueprints.insert(row.0, row.1);
                }
            }
        }
        // Subuser counts (COUNT + GROUP BY instead of fetching full user rows).
        if let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT server_id, COUNT(*) FROM subusers WHERE server_id IN ({}) GROUP BY server_id",
            in_clause(ids.len())
        )) {
            if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }) {
                for row in rows.flatten() {
                    meta.subusers.insert(row.0, row.1 as usize);
                }
            }
        }
        // Backup counts, same shape.
        if let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT server_id, COUNT(*) FROM backups WHERE server_id IN ({}) GROUP BY server_id",
            in_clause(ids.len())
        )) {
            if let Ok(rows) = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }) {
                for row in rows.flatten() {
                    meta.backups.insert(row.0, row.1 as usize);
                }
            }
        }
        meta
    }
}

/// Single-server JSON: same shape as the batch path, with the batch metadata
/// built from just this server. Runtime info comes from the monitor's shared
/// sample cache (5s freshness) instead of a fresh blocking disk/proc walk.
async fn server_json(state: &AppState, s: &Server, u: &User) -> serde_json::Value {
    let s2 = s.clone();
    let meta = blocking(state.db.clone(), move |db| {
        Ok(ServerListMeta::load(&db, std::slice::from_ref(&s2)))
    })
    .await
    .unwrap_or_default();
    server_json_meta(state, s, u, &meta)
}

fn server_json_meta(
    state: &AppState,
    s: &Server,
    u: &User,
    meta: &ServerListMeta,
) -> serde_json::Value {
    // Cached sample from the 5s monitor sweep; only walks disk/proc when the
    // sweep has never run or the entry aged out (fallback path).
    let info = state
        .monitor
        .cached_info(&state.procs, s, Duration::from_secs(5));
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
        "blueprint": meta.blueprints.get(&s.blueprint_id).cloned().unwrap_or_default(),
        "blueprint_id": s.blueprint_id,
        "suspended": s.suspended,
        "auto_restart": s.auto_restart,
        "restart_count": s.restart_count,
        "created_at": s.created_at,
        "owner": u.id == s.user_id,
        "info": info,
        "subusers": meta.subusers.get(&s.id).copied().unwrap_or(0),
        "backup_count": meta.backups.get(&s.id).copied().unwrap_or(0),
    })
}

pub async fn get(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id).await?;

    let s_bp = s.blueprint_id;
    let s1 = s.clone();
    let u1 = u.clone();
    let (blueprint, cap_ok, vars) = blocking(state.db.clone(), move |db| {
        let blueprint = models::get_blueprint(&db, s_bp)?;
        let cap_ok = models::user_has_capability(&db, &u1, s1.id, Capability::StartupSecrets)?;
        let vars = blueprint::resolve_variables(&db, &s1)?;
        Ok((blueprint, cap_ok, vars))
    })
    .await?;
    let can_view_secrets = u.root_admin || u.id == s.user_id || cap_ok;
    let variables: Vec<serde_json::Value> = vars
        .into_iter()
        .filter(|(v, _)| v.user_viewable || can_view_secrets)
        .map(|(v, val)| {
            serde_json::json!({
                "name": v.name,
                "description": v.description,
                "env_var": v.env_var,
                "value": val,
                "default": v.default_value,
                "user_viewable": v.user_viewable,
                "user_editable": v.user_editable,
                "required": v.required,
                "kind": v.kind,
            })
        })
        .collect();
    let websites_enabled = state.cfg.features.enable_websites;
    let s2 = s.clone();
    let (allocated, websites, databases, subusers, schedules, resolved_launch) =
        blocking(state.db.clone(), move |db| {
            let allocated = models::ports_for_server(&db, s2.id)?;
            let websites = if websites_enabled {
                models::list_websites(&db, s2.id)?
            } else {
                vec![]
            };
            let databases = models::list_databases(&db, s2.id)?;
            let subusers = models::list_subusers(&db, s2.id)?;
            let schedules = models::list_schedules(&db, s2.id)?;
            let resolved_launch = if can_view_secrets {
                blueprint::resolve_startup(&db, &s2).unwrap_or_default()
            } else {
                String::new()
            };
            Ok((
                allocated,
                websites,
                databases,
                subusers,
                schedules,
                resolved_launch,
            ))
        })
        .await?;
    let server = server_json(&state, &s, &u).await;
    Ok(Json(serde_json::json!({
        "server": server,
        "blueprint": blueprint,
        "variables": variables,
        "ports": allocated,
        "websites": websites,
        "databases": databases,
        "subusers": subusers.iter().map(|(su, grant)| subuser_json(su, grant)).collect::<Vec<_>>(),
        "schedules": schedules,
        "launch_command": if can_view_secrets {
            s.startup.clone()
        } else {
            String::new()
        },
        "runtime_hint": s.runtime_hint,
        "resolved_launch": resolved_launch,
    })))
}

#[derive(Deserialize)]
pub struct CreateServerReq {
    pub name: String,
    pub user_id: i64,
    pub blueprint_id: i64,
    pub description: Option<String>,
    pub runtime_hint: Option<String>,
    pub memory_mb: Option<i64>,
    pub disk_mb: Option<i64>,
    pub cpu_percent: Option<i64>,
    pub variables: Option<std::collections::HashMap<String, String>>,
    pub port: Option<i64>,
    #[serde(default)]
    pub start_on_create: bool,
    pub node: Option<String>,
    #[serde(default)]
    pub node_tags: Vec<String>,
    pub location: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    a: AdminUser,
    Json(req): Json<CreateServerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = validate_name(&req.name)?;

    let bp_id_req = req.blueprint_id;
    let owner_id_req = req.user_id;
    let (blueprint, owner) = blocking(state.db.clone(), move |db| {
        Ok((
            models::get_blueprint(&db, bp_id_req)?,
            models::get_user(&db, owner_id_req)?,
        ))
    })
    .await?;
    let default_mem = limit_override(
        &state.db,
        "limits.default_memory_mb",
        state.cfg.limits.default_memory_mb,
    )
    .await;
    let max_mem = limit_override(
        &state.db,
        "limits.max_memory_mb",
        state.cfg.limits.max_memory_mb,
    )
    .await;
    let mem = req.memory_mb.unwrap_or(default_mem as i64);
    if mem > max_mem as i64 {
        return Err(ApiError::bad_request("memory exceeds fabric capacity"));
    }
    let disk = req.disk_mb.unwrap_or(
        limit_override(
            &state.db,
            "limits.default_disk_mb",
            state.cfg.limits.default_disk_mb,
        )
        .await as i64,
    );
    let cpu = req.cpu_percent.unwrap_or(
        limit_override(
            &state.db,
            "limits.default_cpu_percent",
            state.cfg.limits.default_cpu_percent,
        )
        .await as i64,
    );
    validate_resources(mem, disk, cpu)?;
    if let Some(port) = req.port {
        models::validate_port(port).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let max_per_user = limit_override(
        &state.db,
        "limits.max_servers_per_user",
        state.cfg.limits.max_servers_per_user,
    )
    .await;
    // An explicit node (or plain local) is resolved here; an "auto" request is
    // resolved inside the transaction below, where its capacity claim is made
    // atomically with the port reservation and the row insert. Resolving it
    // outside the lock is exactly the race this reservation closes: two
    // concurrent creates would both read the same stale heartbeat capacity and
    // pick the same node.
    let explicit_local = matches!(req.node.as_deref(), None | Some("local"))
        && req.node_tags.is_empty()
        && req.location.as_deref().unwrap_or("").is_empty();
    let chosen_node: Option<crate::nodes::Node> = if explicit_local {
        None
    } else {
        match req.node.as_deref() {
            Some(nodename) if nodename != "auto" => {
                let nodename = nodename.to_string();

                Some(
                    blocking(state.db.clone(), move |db| {
                        crate::nodes::get_by_name(&db, &nodename)
                    })
                    .await?,
                )
            }
            _ => None,
        }
    };
    let uuid = uuid::Uuid::new_v4().to_string();
    let runtime_hint = match req.runtime_hint.clone() {
        Some(hint) => {
            if hint.chars().count() > 256 {
                return Err(ApiError::bad_request(
                    "runtime_hint must not exceed 256 characters",
                ));
            }
            hint
        }
        None => blueprint.runtime_hint.clone(),
    };
    let launch_command = if blueprint.startup.is_empty() {
        String::new()
    } else {
        blueprint.startup.clone()
    };
    // Quota, node capacity claim, node assignment and the port reservation are
    // all claimed atomically in one BEGIN IMMEDIATE transaction: separate
    // check-then-act steps would let concurrent creates oversubscribe a node
    // or exceed the per-user limit. Immediate takes the write lock up front,
    // so pooled connections serialize exactly like the old global mutex did.
    // The whole unit runs inside `Db::call`, so the transaction never holds a
    // connection across an await.
    let rollback_uuid = uuid.clone();
    let owner_root = owner.root_admin;
    let owner_id = owner.id;
    let bp_id = blueprint.id;
    let port_req = req.port;
    let node_tags = req.node_tags.clone();
    let location = req.location.clone();
    let chosen = chosen_node.clone();
    let (id, reservation_id, chosen_node) = state
        .db
        .call(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if !owner_root {
                let n: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM servers WHERE user_id=?1 AND deleted=0",
                    [owner_id],
                    |r| r.get(0),
                )?;
                if n >= max_per_user as i64 {
                    return Err(anyhow::Error::new(CreateTxError::Quota));
                }
            }
            // Auto placement: claim memory/disk on the best node in THIS
            // transaction. The reservation row survives the commit (until its
            // TTL) so a concurrent create cannot double-book the same stale
            // capacity.
            let mut chosen_node = chosen;
            let mut reservation_id: Option<i64> = None;
            if chosen_node.is_none() && !explicit_local {
                let (node, rid) = crate::nodes::reserve_capacity_tx(
                    &tx,
                    mem,
                    disk,
                    &node_tags,
                    location.as_deref(),
                )?;
                reservation_id = Some(rid);
                chosen_node = Some(node);
            }
            let target_node_name = chosen_node
                .as_ref()
                .map(|n| n.name.as_str())
                .unwrap_or("local");
            if let Some(port) = port_req {
                // Check both tables the allocation path guards: live server
                // rows and any leftover allocations (add_allocation's unique
                // index).
                let used: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM servers WHERE node=?1 AND port=?2 AND deleted=0",
                    params![target_node_name, port],
                    |r| r.get(0),
                )?;
                let used_alloc: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM allocations WHERE node=?1 AND port=?2",
                    params![target_node_name, port],
                    |r| r.get(0),
                )?;
                if used > 0 || used_alloc > 0 {
                    return Err(anyhow::Error::new(CreateTxError::PortConflict(
                        format!("port {port} is already reserved on agent {target_node_name}"),
                    )));
                }
            }
            let ts = chrono::Utc::now().to_rfc3339();
            tx.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,runtime_hint,startup,memory_mb,disk_mb,cpu_percent,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'offline',?10,?10)",
                params![uuid, name, owner_id, bp_id, runtime_hint, launch_command, mem, disk, cpu, ts],
            )?;
            let id = tx.last_insert_rowid();
            if let Some(node) = &chosen_node {
                tx.execute(
                    "UPDATE servers SET node=?1,updated_at=?2 WHERE id=?3",
                    params![node.name, ts, id],
                )?;
            }
            if let Some(port) = port_req {
                // Node is set before the allocation insert, so the reservation
                // is keyed to the real (node, port) the agent will advertise.
                tx.execute(
                    "INSERT INTO allocations(server_id,port,assigned_at,node,notes,is_primary) VALUES(?1,?2,?3,?4,'',1)",
                    params![id, port, ts, target_node_name],
                )?;
                tx.execute(
                    "UPDATE servers SET port=?1,updated_at=?2 WHERE id=?3",
                    params![port, ts, id],
                )?;
            }
            tx.commit()?;
            Ok((id, reservation_id, chosen_node))
        })
        .await
        .map_err(|e| match e.downcast_ref::<CreateTxError>() {
            Some(CreateTxError::Quota) => ApiError::bad_request("owner reached workspace limit"),
            Some(CreateTxError::PortConflict(msg)) => ApiError::conflict(msg),
            None => ApiError::from(e),
        })?;
    let mut rollback = CreateRollback {
        db: state.db.clone(),
        id,
        committed: false,
        monitor: state.monitor.clone(),
        node_client: state.node_client.clone(),
        node: None,
        uuid: rollback_uuid,
        reservation: reservation_id,
    };
    // Blueprint input overrides, then defaults for unspecified inputs.
    let variables = req.variables.unwrap_or_default();
    let bp_vars = blueprint.variables.clone();

    blocking(state.db.clone(), move |db| {
        for (key, value) in variables {
            models::set_server_var(&db, id, bp_id, &key, &value)?;
        }
        let existing = models::get_server_vars(&db, id)?;
        for var in &bp_vars {
            if !existing.iter().any(|(k, _)| k == &var.env_var) {
                models::set_server_var(&db, id, bp_id, &var.env_var, &var.default_value)?;
            }
        }
        Ok(())
    })
    .await?;
    let srv = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
    let bp_stop = blueprint.stop_command.clone();
    if let Some(node) = &chosen_node {
        // From here the agent may hold server files, so rollback must ask it
        // to delete the workload too (best-effort, see CreateRollback).
        rollback.node = Some(node.clone());

        let srv2 = srv.clone();
        let (files, spec) = blocking(state.db.clone(), move |db| {
            let files = blueprint::build_default_config(&db, &srv2)?
                .map(|cfg| {
                    vec![crate::node_protocol::ProvisionFile {
                        path: "config.json".into(),
                        content_b64: base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            serde_json::to_vec_pretty(&cfg).unwrap_or_default(),
                        ),
                        mode: Some(0o644),
                    }]
                })
                .unwrap_or_default();
            let spec = crate::node_protocol::ServerSpec {
                uuid: srv2.uuid.clone(),
                name: srv2.name.clone(),
                startup: blueprint::resolve_startup(&db, &srv2)?,
                stop_command: bp_stop,
                memory_mb: mem as u64,
                disk_mb: disk as u64,
                cpu_percent: cpu as u64,
                port: port_req.and_then(|p| u16::try_from(p).ok()),
                ports: models::ports_for_server(&db, srv2.id)?
                    .into_iter()
                    .filter_map(|p| u16::try_from(p).ok())
                    .collect(),
                env: blueprint::env_for_server(&db, &srv2),
                auto_restart: srv2.auto_restart,
            };
            Ok((files, spec))
        })
        .await?;
        if let Err(e) = state
            .node_client
            .provision(
                node,
                &crate::node_protocol::ProvisionRequest { spec, files },
            )
            .await
        {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("node provisioning failed: {e}"),
            ));
        }
    } else {
        std::fs::create_dir_all(services::proc::server_dir(&srv))?;
        // Surface render/JSON errors from the declared default config on the
        // local path too, mirroring the remote branch above.
        let srv2 = srv.clone();
        if let Some(cfg) = blocking(state.db.clone(), move |db| {
            blueprint::build_default_config(&db, &srv2)
        })
        .await?
        {
            services::files::write_file(
                &state.cfg,
                &srv,
                "config.json",
                &serde_json::to_vec_pretty(&cfg)?,
            )?;
        }
    }
    state.monitor.set_limit(services::ServerLimits {
        server_id: id,
        memory_mb: mem as u64,
        cpu_percent: cpu as u64,
        bandwidth_rx: 0,
        bandwidth_tx: 0,
    });
    let admin_id = a.0.id;
    let req_name = req.name;

    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_create",
            &format!("server #{id}"),
            "",
            &req_name,
            Some(id),
        )
    })
    .await?;
    if req.start_on_create {
        let srv = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
        if let Some(node) = &chosen_node {
            state
                .node_client
                .power(node, &srv.uuid, crate::node_protocol::PowerAction::Start)
                .await?;
        } else {
            let srv2 = srv.clone();
            let (cmd, env) = blocking(state.db.clone(), move |db| {
                Ok((
                    blueprint::resolve_startup(&db, &srv2)?,
                    blueprint::env_for_server(&db, &srv2),
                ))
            })
            .await?;
            state
                .procs
                .start(&srv, &cmd, &env, state.notifier.clone())?;
        }
    }
    // Commit only after the optional start: a failed start rolls the whole
    // create back (agent workload, DB row, monitor limit).
    rollback.committed = true;

    let srv = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
    let server = server_json(&state, &srv, &owner).await;
    Ok(Json(server))
}

#[derive(Deserialize)]
pub struct UpdateServerReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub runtime_hint: Option<String>,
    pub launch_command: Option<String>,
    pub memory_mb: Option<i64>,
    pub disk_mb: Option<i64>,
    pub cpu_percent: Option<i64>,
    pub auto_restart: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateServerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut s = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
    let original = s.clone();
    let requested_mem = req.memory_mb.unwrap_or(s.memory_mb);
    let requested_disk = req.disk_mb.unwrap_or(s.disk_mb);
    let requested_cpu = req.cpu_percent.unwrap_or(s.cpu_percent);
    validate_resources(requested_mem, requested_disk, requested_cpu)?;
    if let Some(name) = req.name {
        s.name = validate_name(&name)?;
    }
    if let Some(desc) = req.description {
        if desc.chars().count() > 2048 {
            return Err(ApiError::bad_request(
                "description must not exceed 2048 characters",
            ));
        }
        s.description = desc;
    }
    if let Some(runtime_hint) = req.runtime_hint {
        if runtime_hint.chars().count() > 256 {
            return Err(ApiError::bad_request(
                "runtime_hint must not exceed 256 characters",
            ));
        }
        s.runtime_hint = runtime_hint;
    }
    if let Some(launch_command) = req.launch_command {
        if launch_command.chars().count() > 4096 {
            return Err(ApiError::bad_request(
                "launch_command must not exceed 4096 characters",
            ));
        }
        s.startup = launch_command;
    }
    if let Some(mem) = req.memory_mb {
        let max_mem = limit_override(
            &state.db,
            "limits.max_memory_mb",
            state.cfg.limits.max_memory_mb,
        )
        .await;
        if mem > max_mem as i64 {
            return Err(ApiError::bad_request("memory exceeds fabric capacity"));
        }
        s.memory_mb = mem;
    }
    if let Some(disk) = req.disk_mb {
        s.disk_mb = disk;
    }
    if let Some(cpu) = req.cpu_percent {
        s.cpu_percent = cpu;
    }
    if let Some(ar) = req.auto_restart {
        s.auto_restart = ar;
    }

    s = blocking(state.db.clone(), move |db| {
        models::update_server(&db, &s)?;
        Ok(s)
    })
    .await?;
    if s.node != "local" {
        let s2 = s.clone();
        let (node, spec) = blocking(state.db.clone(), move |db| {
            let node = crate::nodes::get_by_name(&db, &s2.node)?;
            let definition = models::get_blueprint(&db, s2.blueprint_id)?;
            let ports = models::ports_for_server(&db, s2.id)?
                .into_iter()
                .filter_map(|p| u16::try_from(p).ok())
                .collect();
            let spec = crate::node_protocol::ServerSpec {
                uuid: s2.uuid.clone(),
                name: s2.name.clone(),
                startup: blueprint::resolve_startup(&db, &s2)?,
                stop_command: definition.stop_command,
                memory_mb: s2.memory_mb as u64,
                disk_mb: s2.disk_mb as u64,
                cpu_percent: s2.cpu_percent as u64,
                port: s2.port.and_then(|p| u16::try_from(p).ok()),
                ports,
                env: blueprint::env_for_server(&db, &s2),
                auto_restart: s2.auto_restart,
            };
            Ok((node, spec))
        })
        .await?;
        if let Err(error) = state
            .node_client
            .provision(
                &node,
                &crate::node_protocol::ProvisionRequest {
                    spec,
                    files: vec![],
                },
            )
            .await
        {
            let original2 = original.clone();
            let _ = blocking(state.db.clone(), move |db| {
                models::update_server(&db, &original2)
            })
            .await;
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("agent rejected configuration: {error}"),
            ));
        }
    }
    state.monitor.set_limit(services::ServerLimits {
        server_id: s.id,
        memory_mb: s.memory_mb as u64,
        cpu_percent: s.cpu_percent as u64,
        bandwidth_rx: 0,
        bandwidth_tx: 0,
    });

    let audit_s = s.clone();
    let admin_id = admin.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_update",
            &format!("server #{id}"),
            "",
            &format!(
                "name={} memory={}MB disk={}MB cpu={}% auto_restart={}",
                audit_s.name,
                audit_s.memory_mb,
                audit_s.disk_mb,
                audit_s.cpu_percent,
                audit_s.auto_restart
            ),
            Some(audit_s.id),
        )
    })
    .await?;
    let server = server_json(&state, &s, &admin).await;
    Ok(Json(server))
}

#[derive(Deserialize)]
pub struct UpdateVarsReq {
    pub variables: std::collections::HashMap<String, String>,
}

pub async fn update_vars(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateVarsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::StartupUpdate).await?;
    ensure_operational(&s, u)?;

    let sid = s.id;
    let s_bp = s.blueprint_id;
    let (definition, old_vars) = blocking(state.db.clone(), move |db| {
        Ok((
            models::get_blueprint(&db, s_bp)?,
            models::get_server_vars(&db, sid)?,
        ))
    })
    .await?;
    // Validate every requested variable and fold them into one map BEFORE
    // touching the DB, so a bad entry cannot leave earlier entries applied.
    let mut new_vars = old_vars.clone();
    for (k, v) in &req.variables {
        let Some(var) = definition.variables.iter().find(|v| &v.env_var == k) else {
            return Err(ApiError::bad_request(format!("unknown variable {k}")));
        };
        if !var.user_editable && !u.root_admin {
            return Err(ApiError::forbidden("variable not editable"));
        }
        blueprint::validate_value(var, v)?;
        if let Some(entry) = new_vars.iter_mut().find(|(ek, _)| ek == k) {
            entry.1 = v.clone();
        } else {
            new_vars.push((k.clone(), v.clone()));
        }
    }
    // Apply the full map once, in a single transaction, so a concurrent
    // writer's variables are never wiped by a per-key apply.

    let bp_id = definition.id;
    let new_vars2 = new_vars.clone();
    blocking(state.db.clone(), move |db| {
        models::replace_server_vars(&db, sid, bp_id, &new_vars2)
    })
    .await?;
    if s.node != "local" {
        let s2 = s.clone();
        let (node, spec) = blocking(state.db.clone(), move |db| {
            let node = crate::nodes::get_by_name(&db, &s2.node)?;
            let ports = models::ports_for_server(&db, s2.id)?
                .into_iter()
                .filter_map(|p| u16::try_from(p).ok())
                .collect();
            let spec = crate::node_protocol::ServerSpec {
                uuid: s2.uuid.clone(),
                name: s2.name.clone(),
                startup: blueprint::resolve_startup(&db, &s2)?,
                stop_command: definition.stop_command.clone(),
                memory_mb: s2.memory_mb as u64,
                disk_mb: s2.disk_mb as u64,
                cpu_percent: s2.cpu_percent as u64,
                port: s2.port.and_then(|p| u16::try_from(p).ok()),
                ports,
                env: blueprint::env_for_server(&db, &s2),
                auto_restart: s2.auto_restart,
            };
            Ok((node, spec))
        })
        .await?;
        if let Err(error) = state
            .node_client
            .provision(
                &node,
                &crate::node_protocol::ProvisionRequest {
                    spec,
                    files: vec![],
                },
            )
            .await
        {
            let old_vars2 = old_vars.clone();
            let _ = blocking(state.db.clone(), move |db| {
                models::replace_server_vars(&db, sid, bp_id, &old_vars2)
            })
            .await;
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("agent rejected blueprint inputs: {error}"),
            ));
        }
    }

    let uid = u.id;
    let n_vars = req.variables.len();
    let sid = s.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            "server_update_vars",
            &format!("server #{id}"),
            "",
            &format!("{n_vars} variable(s) updated"),
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
    if s.node != "local" {
        let nname = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &nname)
        })
        .await?;
        // Stop-verify then remove on the agent: soft-deleting the row while
        // the agent still holds files/cgroup would orphan a running workload
        // (auto-restart keeps it alive) with no control-plane reference.
        let removed = match state
            .node_client
            .stop_and_wait(&node, &s.uuid, 100, std::time::Duration::from_millis(100))
            .await
        {
            Ok(()) => state.node_client.delete_server(&node, &s.uuid).await,
            Err(e) => Err(e),
        };
        if let Err(e) = removed {
            // Agent unreachable: keep the row so the workload stays referenced
            // and retryable, and record an explicit pending-delete event.
            let admin_id = a.0.id;
            let sid = s.id;
            let nname = node.name.clone();
            let audit_detail = format!("node={nname} unreachable: {e}");
            blocking(state.db.clone(), move |db| {
                models::audit_scoped(
                    &db,
                    Some(admin_id),
                    "server_delete_pending",
                    &format!("server #{id}"),
                    "",
                    &audit_detail,
                    Some(sid),
                )
            })
            .await?;
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("agent did not confirm deletion ({e}); server retained for retry"),
            ));
        }
        state.monitor.remove_limit(s.id);
    } else {
        state.procs.stop(s.id)?;
        state.procs.remove_limits(s.id);
        state.monitor.remove_limit(s.id);
        crate::services::console::drop_server(&state.hub, s.id);
    }

    let admin_id = a.0.id;
    let sid = s.id;
    let sname = s.name.clone();
    let snode = s.node.clone();
    blocking(state.db.clone(), move |db| {
        models::delete_server(&db, sid)?;
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_delete",
            &format!("server #{id}"),
            "",
            &format!("{sname} node={snode}"),
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({"ok":true})))
}

/// Physical removal of dir + DB row (admin).
pub async fn purge(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = blocking(state.db.clone(), move |db| models::get_server_any(&db, id)).await?;
    if s.node != "local" {
        // The node row may already be gone (a node holding only soft-deleted
        // servers can be removed, since the delete guard counts deleted=0
        // rows). Purge must still finish locally instead of 404-ing through a
        // mandatory node lookup.
        let s_node = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &s_node)
        })
        .await;
        if let Ok(node) = node {
            let _ = state
                .node_client
                .power(&node, &s.uuid, crate::node_protocol::PowerAction::Kill)
                .await;
            for _ in 0..50 {
                if state
                    .node_client
                    .stats(&node, &s.uuid)
                    .await
                    .map(|v| v.pid.is_none())
                    .unwrap_or(true)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            state.node_client.delete_server(&node, &s.uuid).await?;
        }
        state.monitor.remove_limit(s.id);
    } else {
        let _ = state.procs.stop(s.id);
        state.procs.remove_limits(s.id);
        state.monitor.remove_limit(s.id);
        crate::services::console::drop_server(&state.hub, s.id);

        let sid = s.id;
        for b in blocking(state.db.clone(), move |db| models::list_backups(&db, sid)).await? {
            let _ = std::fs::remove_file(&b.path);
        }
        let _ = std::fs::remove_dir_all(services::proc::server_dir(&s));
        // Data Lab storage lives outside the server root, so it needs its own
        // removal or it would outlive the workload it belongs to.
        let _ = std::fs::remove_dir_all(services::databases::db_dir(
            &s,
            &state.cfg.paths.datalab_dir,
        ));
    }

    let admin_id = a.0.id;
    let sid = s.id;
    let sname = s.name.clone();
    let snode = s.node.clone();
    blocking(state.db.clone(), move |db| {
        models::free_ports(&db, sid)?;
        models::delete_server_vars(&db, sid)?;
        models::purge_server(&db, sid)?;
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_purge",
            &format!("server #{id}"),
            "",
            &format!("{sname} node={snode}"),
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

/// Power actions arrive as a closed enum, so an unknown verb is rejected by
/// deserialization before any authorization or dispatch logic runs.
#[derive(Deserialize)]
pub struct PowerReq {
    pub action: PowerAction,
}

/// Launch a local server: resolve its startup command and env off the blocking
/// pool, then start the process. Shared by `power`'s Start and Restart arms.
async fn start_server(state: &AppState, srv: Server) -> ApiResult<()> {
    let srv2 = srv.clone();
    let (cmd, env) = blocking(state.db.clone(), move |db| {
        Ok((
            blueprint::resolve_startup(&db, &srv2)?,
            blueprint::env_for_server(&db, &srv2),
        ))
    })
    .await?;
    state
        .procs
        .start(&srv, &cmd, &env, state.notifier.clone())
        .map_err(lifecycle_error)?;
    Ok(())
}

pub async fn power(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<PowerReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, power_capability(req.action)).await?;
    ensure_operational(&s, u)?;
    let audit_action = format!("server_{}", req.action.as_str());
    if s.node != "local" {
        let nname = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &nname)
        })
        .await?;
        let stats = state
            .node_client
            .power(&node, &s.uuid, req.action)
            .await
            .map_err(lifecycle_error)?;

        let sid = s.id;
        let state_str = stats.state.clone();
        blocking(state.db.clone(), move |db| {
            models::set_server_status(&db, sid, &state_str)
        })
        .await?;

        let uid = u.id;
        let sname = s.name.clone();
        let nname = node.name.clone();
        let audit_action2 = audit_action.clone();
        blocking(state.db.clone(), move |db| {
            models::audit_scoped(
                &db,
                Some(uid),
                &audit_action2,
                &sname,
                "",
                &format!("node={nname}"),
                Some(sid),
            )
        })
        .await?;
        return Ok(Json(
            serde_json::json!({ "ok": true, "remote": true, "stats": stats }),
        ));
    }
    match req.action {
        PowerAction::Start => start_server(&state, s.clone()).await?,
        PowerAction::Stop => state.procs.stop(s.id)?,
        PowerAction::Restart => {
            state.procs.stop(s.id)?;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            // Re-read: a stop may have rewritten status/ports the startup uses.
            let srv = blocking(state.db.clone(), move |db| models::get_server(&db, s.id)).await?;
            start_server(&state, srv).await?;
        }
        PowerAction::Kill => state.procs.kill(s.id)?,
    }

    let uid = u.id;
    let sname = s.name.clone();
    let sid = s.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            &audit_action,
            &sname,
            "",
            "node=local",
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct InstallReq {
    pub script: Option<String>,
}

/// Run the blueprint's isolated setup plan.
pub async fn install(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<InstallReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::StartupInstall).await?;
    ensure_operational(&s, u)?;
    if s.node != "local" {
        return Err(ApiError::new(
            axum::http::StatusCode::NOT_IMPLEMENTED,
            "remote blueprint setup must run through the execution agent",
        ));
    }

    let s_bp = s.blueprint_id;
    let mut definition =
        blocking(state.db.clone(), move |db| models::get_blueprint(&db, s_bp)).await?;
    if let Some(script) = req.script {
        if !u.root_admin {
            return Err(ApiError::forbidden(
                "custom install scripts require administrator access",
            ));
        }
        definition.install_script = Some(script);
    }
    let install_db = state.db.clone();
    let emit_db = state.db.clone();
    let sid = s.id;
    let uuid = s.uuid.clone();
    let server_name = s.name.clone();
    tokio::task::spawn_blocking(move || {
        blueprint::run_install(&install_db, &s, &definition, &state.notifier, &state.hub)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("install worker failed: {e}"),
        )
    })??;
    // Lifecycle event: a completed setup plan is a server.install. The emit
    // is synchronous SQLite, so it rides the shared blocking helper like
    // every other pool-based call (best-effort: `webhooks::emit` never fails).
    blocking(emit_db, move |db| {
        crate::services::webhooks::emit(
            &db,
            "server.install",
            Some(sid),
            serde_json::json!({
                "event": "server.install",
                "server_id": sid,
                "uuid": uuid,
                "server_name": server_name,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }),
        );
        Ok(())
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn suspend(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;
    if s.node != "local" {
        let nname = s.node.clone();
        let n = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &nname)
        })
        .await?;
        let _ = state
            .node_client
            .power(&n, &s.uuid, crate::node_protocol::PowerAction::Stop)
            .await?;
    } else {
        state.procs.stop(s.id)?;
    }

    let admin_id = _a.0.id;
    let sid = s.id;
    let sname = s.name.clone();
    blocking(state.db.clone(), move |db| {
        models::set_server_suspended(&db, sid, true)?;
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_suspend",
            &format!("server #{id}"),
            "",
            &sname,
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({"ok":true})))
}

pub async fn unsuspend(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = blocking(state.db.clone(), move |db| models::get_server(&db, id)).await?;

    let admin_id = _a.0.id;
    let sid = s.id;
    let sname = s.name.clone();
    blocking(state.db.clone(), move |db| {
        models::set_server_suspended(&db, sid, false)?;
        models::audit_scoped(
            &db,
            Some(admin_id),
            "server_unsuspend",
            &format!("server #{id}"),
            "",
            &sname,
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- Subusers ----------------

/// Shared shape for a team member: identity plus their effective grant.
fn subuser_json(su: &User, grant: &crate::capability::Grant) -> serde_json::Value {
    serde_json::json!({
        "id": su.id,
        "username": su.username,
        "email": su.email,
        "role": grant.role.as_str(),
        "permissions": grant.names(),
    })
}

pub async fn list_subusers(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::SubusersRead).await?;

    let subs = blocking(state.db.clone(), move |db| models::list_subusers(&db, id)).await?;
    let out: Vec<serde_json::Value> = subs
        .iter()
        .map(|(su, grant)| subuser_json(su, grant))
        .collect();
    Ok(data(serde_json::json!(out)))
}

#[derive(Deserialize)]
pub struct AddSubuserReq {
    pub user_id: i64,
    /// Role preset; defaults to `custom` when omitted.
    #[serde(default)]
    pub role: Option<String>,
    /// Extra capabilities granted on top of the preset.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

pub async fn add_subuser(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<AddSubuserReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::SubusersWrite).await?;
    let role = match req.role.as_deref() {
        Some(r) => Role::from_str(r).map_err(|e| ApiError::bad_request(e.to_string()))?,
        None => Role::Custom,
    };
    let extra = req
        .capabilities
        .iter()
        .map(|c| Capability::from_str(c))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let grant = Grant::new(role, extra);
    if grant.capabilities().next().is_none() {
        return Err(ApiError::bad_request(
            "grant must include at least one capability",
        ));
    }
    // A delegate may never mint authority it does not itself hold; owners and
    // root admins already carry the full capability set so this is a no-op for them.
    if s.user_id != u.id && !u.root_admin {
        let u2 = user.0.clone();
        let mine = blocking(state.db.clone(), move |db| {
            models::server_grant(&db, &u2, id)
        })
        .await?
        .ok_or_else(|| ApiError::forbidden("no grant on this server"))?;
        if let Some(over) = grant.capabilities().find(|c| !mine.contains(*c)) {
            return Err(ApiError::forbidden(format!(
                "cannot grant a capability you do not hold: {}",
                over.as_str()
            )));
        }
    }

    let req_user_id = req.user_id;
    let grant2 = grant.clone();
    blocking(state.db.clone(), move |db| {
        models::add_subuser(&db, id, req_user_id, &grant2)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        // FK violations (unknown user) and duplicate memberships were
        // generic 500s; surface them as 404/409 like the rest of the API.
        if msg.contains("user not found") {
            ApiError::not_found(msg)
        } else if msg.contains("already a member") {
            ApiError::conflict(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let uid = u.id;
    let detail = format!("user #{} role={}", req.user_id, grant.role.as_str());
    let sid = s.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            "subuser_add",
            &format!("server #{id}"),
            "",
            &detail,
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn remove_subuser(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, sub_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::SubusersWrite).await?;
    // Anti-escalation mirror of add_subuser: a delegate may only remove a
    // member whose grant it could have minted itself. Owners and root admins
    // carry every capability, so the check is a no-op for them.
    if s.user_id != u.id && !u.root_admin {
        let u2 = user.0.clone();
        let mine = blocking(state.db.clone(), move |db| {
            models::server_grant(&db, &u2, id)
        })
        .await?
        .ok_or_else(|| ApiError::forbidden("no grant on this server"))?;

        let target = blocking(state.db.clone(), move |db| models::list_subusers(&db, id))
            .await?
            .into_iter()
            .find(|(su, _)| su.id == sub_id)
            .map(|(_, g)| g)
            .ok_or_else(|| ApiError::not_found("subuser not found"))?;
        // `find` returns the owned capability, so no borrow of `target`
        // survives the statement (the earlier E0597 came from holding the
        // iterator itself across the if-let).
        let over = target.capabilities().find(|c| !mine.contains(*c));
        if let Some(over) = over {
            return Err(ApiError::forbidden(format!(
                "cannot remove a member with capabilities you do not hold: {}",
                over.as_str()
            )));
        }
    }

    blocking(state.db.clone(), move |db| {
        models::remove_subuser(&db, id, sub_id)
    })
    .await?;

    let uid = u.id;
    let detail = format!("user #{sub_id}");
    let sid = s.id;
    blocking(state.db.clone(), move |db| {
        models::audit_scoped(
            &db,
            Some(uid),
            "subuser_remove",
            &format!("server #{id}"),
            "",
            &detail,
            Some(sid),
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- Squads (org workspaces) ----------------
//
// Squad CRUD surfaces. A squad grants every member its role preset on every
// grouped server at once. Member and server-assignment ops are open to any
// authenticated user holding manager authority on the squad (root admins,
// the squad creator, and Manager-preset members); the model still enforces
// the subuser anti-escalation rule — it refuses to mint (or remove) a role
// whose capabilities the actor does not hold on the squad. Creation stays
// admin-only; rename is manager-scoped; deletion requires the creator or a
// root admin. Squad ops are squad-scoped, not server-capability-scoped, so a
// scoped API key gets no extra narrowing here — the manager check is the
// gate (see `require_squad_manager`).

/// Squad member/server ops require manager authority on the squad: root
/// admins, the squad creator, and Manager-preset members. Non-managers get
/// 403 regardless of whether they are members. Squad ops are squad-scoped,
/// not server-capability-scoped, so an API key's capability/server_ids
/// filter does not narrow them further — the manager check is the gate.
async fn require_squad_manager(state: &AppState, user: &User, squad_id: i64) -> ApiResult<()> {
    let db = state.db.clone();
    let uid = user.id;
    let sid = squad_id;
    let ok = blocking(db, move |db| models::squad_manager_authority(&db, uid, sid)).await?;
    if ok {
        Ok(())
    } else {
        Err(ApiError::forbidden("not a squad manager"))
    }
}

/// A squad manager may only assign (or un-assign) servers they already hold
/// access to. `squad_grant` mints the member's preset role on every assigned
/// server, so letting a manager name an arbitrary panel server would mint
/// Manager caps (ControlKill/FilesWrite/DatabaseWrite/BackupsWrite/...) on
/// it with zero prior access — the per-server delegation gate `add_subuser`
/// enforces (SubusersWrite on target) must not be bypassed here. Root admins
/// pass automatically via `user_has_server_access`'s shortcut. All ids are
/// checked in one blocking unit so nothing is applied before every id passes.
async fn require_server_access(
    state: &AppState,
    user: &User,
    server_ids: &[i64],
) -> ApiResult<()> {
    let db = state.db.clone();
    let user = user.clone();
    let ids = server_ids.to_vec();
    let denied = blocking(db, move |db| {
        for sid in &ids {
            if !models::user_has_server_access(&db, &user, *sid)? {
                return Ok(Some(*sid));
            }
        }
        Ok(None)
    })
    .await?;
    match denied {
        None => Ok(()),
        Some(_sid) => Err(ApiError::forbidden("no access to this server")),
    }
}

#[derive(Deserialize)]
pub struct SquadCreateReq {
    pub name: String,
}

#[derive(Deserialize)]
pub struct SquadUpdateReq {
    pub name: String,
}

#[derive(Deserialize)]
pub struct SquadMemberReq {
    pub user_id: i64,
    pub role: String,
}

#[derive(Deserialize)]
pub struct SquadMemberRoleReq {
    pub role: String,
}

#[derive(Deserialize)]
pub struct SquadServerReq {
    pub server_id: i64,
}

#[derive(Deserialize)]
pub struct SquadServersReq {
    pub server_ids: Vec<i64>,
}

fn parse_squad_role(role: &str) -> ApiResult<Role> {
    let role = Role::from_str(role).map_err(|e| ApiError::bad_request(e.to_string()))?;
    if role == Role::Custom {
        return Err(ApiError::bad_request(
            "custom role is not valid for squads; use viewer, operator, developer, or manager",
        ));
    }
    Ok(role)
}

fn squad_json(sq: &models::Squad) -> serde_json::Value {
    serde_json::json!({
        "id": sq.id,
        "name": sq.name,
        "created_by": sq.created_by,
        "created_at": sq.created_at,
    })
}

pub async fn squad_list(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let squads = blocking(state.db.clone(), move |db| {
        models::list_squads_with_counts(&db)
    })
    .await?;
    let out: Vec<serde_json::Value> = squads
        .iter()
        .map(|(sq, member_count, server_count)| {
            let mut v = squad_json(sq);
            v["member_count"] = serde_json::json!(member_count);
            v["server_count"] = serde_json::json!(server_count);
            v
        })
        .collect();
    Ok(data(serde_json::json!(out)))
}

pub async fn squad_create(
    State(state): State<AppState>,
    a: AdminUser,
    Json(req): Json<SquadCreateReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }

    let n2 = name.clone();
    let id = blocking(state.db.clone(), move |db| {
        models::create_squad(&db, &n2, a.0.id)
    })
    .await?;

    let n2 = name.clone();
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(a.0.id),
            "squad.create",
            &format!("squad #{id}"),
            "",
            &n2,
        )
    })
    .await?;

    let sq = blocking(state.db.clone(), move |db| models::get_squad(&db, id)).await?;
    Ok((StatusCode::CREATED, data(squad_json(&sq))))
}

pub async fn squad_get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let sq = blocking(state.db.clone(), move |db| models::get_squad(&db, id))
        .await
        .map_err(|_| ApiError::not_found("squad not found"))?;

    let uid = user.0.id;
    let my_role =
        blocking(state.db.clone(), move |db| models::squad_role_for_user(&db, uid, id)).await?;
    let mut v = squad_json(&sq);
    // my_role: the caller's actual membership role; root admins and the squad
    // creator report "manager" (they hold full authority), non-members null.
    // The frontend derives canManage from my_role, so it stays in both views.
    v["my_role"] = match my_role.as_ref() {
        Some(role) => serde_json::json!(role.as_str()),
        None => serde_json::Value::Null,
    };
    // Roster disclosure guard: members/servers are only served to members —
    // anyone who reports a role (member, manager, creator, root). Outsiders
    // get the minimal {id, name, my_role: null} view with no roster arrays,
    // so sequential squad-id enumeration cannot leak usernames or server names.
    if my_role.is_some() {
        let (members, servers) = blocking(state.db.clone(), move |db| {
            Ok((
                models::squad_members(&db, id)?,
                models::squad_servers(&db, id)?,
            ))
        })
        .await?;
        v["members"] = serde_json::json!(
            members
                .iter()
                .map(|m| serde_json::json!({ "id": m.user_id, "username": m.username, "role": m.role.as_str() }))
                .collect::<Vec<_>>()
        );
        v["servers"] = serde_json::json!(servers
            .iter()
            .map(|s| serde_json::json!({ "id": s.id, "name": s.name }))
            .collect::<Vec<_>>());
    }
    Ok(data(v))
}

pub async fn squad_update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SquadUpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Renaming a squad changes its identity for every member, so manager
    // authority (which includes root admins and the creator) is required.
    require_squad_manager(&state, &user.0, id).await?;
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }

    let n2 = name.clone();
    blocking(state.db.clone(), move |db| {
        models::rename_squad(&db, id, &n2)
    })
    .await
    .map_err(|_| ApiError::not_found("squad not found"))?;

    let n2 = name.clone();
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.update",
            &format!("squad #{id}"),
            "",
            &n2,
        )
    })
    .await?;

    let sq = blocking(state.db.clone(), move |db| models::get_squad(&db, id))
        .await
        .map_err(|_| ApiError::not_found("squad not found"))?;
    Ok(data(squad_json(&sq)))
}

pub async fn squad_delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    // Verify existence first so the audit record always names a real squad.

    blocking(state.db.clone(), move |db| models::get_squad(&db, id))
        .await
        .map_err(|_| ApiError::not_found("squad not found"))?;

    // Deletion destroys the squad's shared membership, so only its creator or
    // a root admin may do it — managers (even Manager-preset ones) cannot.
    let uid = user.0.id;
    let creator = blocking(state.db.clone(), move |db| models::squad_creator(&db, id)).await?;
    if !user.0.root_admin && creator != Some(uid) {
        return Err(ApiError::forbidden(
            "only the squad creator or a root admin can delete this squad",
        ));
    }

    blocking(state.db.clone(), move |db| models::delete_squad(&db, id)).await?;

    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(uid),
            "squad.delete",
            &format!("squad #{id}"),
            "",
            "",
        )
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
pub async fn squad_add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SquadMemberReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    require_squad_manager(&state, &user.0, id).await?;
    let role = parse_squad_role(&req.role)?;

    let actor = user.0.clone();
    let req_user_id = req.user_id;
    blocking(state.db.clone(), move |db| {
        models::add_squad_member(&db, id, req_user_id, role, &actor)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("squad not found") || msg.contains("user not found") {
            ApiError::not_found(msg)
        } else if msg.contains("already a member") {
            ApiError::conflict(msg)
        } else if msg.contains("cannot grant") {
            ApiError::forbidden(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let detail = format!("user #{} role={}", req.user_id, role.as_str());
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.member.add",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;

    let user = blocking(state.db.clone(), move |db| {
        models::get_user(&db, req.user_id)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        data(serde_json::json!({
            "id": user.id,
            "username": user.username,
            "role": role.as_str(),
        })),
    ))
}

pub async fn squad_update_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
    Json(req): Json<SquadMemberRoleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_squad_manager(&state, &user.0, id).await?;
    let role = parse_squad_role(&req.role)?;

    let actor = user.0.clone();
    blocking(state.db.clone(), move |db| {
        models::update_squad_member_role(&db, id, uid, role, &actor)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("squad not found") || msg.contains("member not found") {
            ApiError::not_found(msg)
        } else if msg.contains("cannot grant") {
            ApiError::forbidden(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let detail = format!("user #{uid} role={}", role.as_str());
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.member.update",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;

    let user = blocking(state.db.clone(), move |db| models::get_user(&db, uid)).await?;
    Ok(data(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "role": role.as_str(),
    })))
}

pub async fn squad_remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, uid)): Path<(i64, i64)>,
) -> ApiResult<StatusCode> {
    require_squad_manager(&state, &user.0, id).await?;
    let actor = user.0.clone();
    blocking(state.db.clone(), move |db| {
        models::remove_squad_member(&db, id, uid, &actor)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("squad not found") || msg.contains("member not found") {
            ApiError::not_found(msg)
        } else if msg.contains("cannot remove") {
            ApiError::forbidden(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let detail = format!("user #{uid}");
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.member.remove",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn squad_add_server(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SquadServerReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    require_squad_manager(&state, &user.0, id).await?;
    // F1: a manager may only assign servers they already hold access to —
    // squad_grant would otherwise mint the Manager preset on any panel server.
    require_server_access(&state, &user.0, &[req.server_id]).await?;
    let req_server_id = req.server_id;
    blocking(state.db.clone(), move |db| {
        models::add_squad_server(&db, id, req_server_id)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("squad not found") || msg.contains("server not found") {
            ApiError::not_found(msg)
        } else if msg.contains("already in this squad") {
            ApiError::conflict(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let detail = format!("server #{} added", req.server_id);
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.update",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;

    let s = blocking(state.db.clone(), move |db| {
        models::get_server(&db, req.server_id)
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        data(serde_json::json!({ "server_id": s.id, "name": s.name })),
    ))
}

pub async fn squad_remove_server(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, sid)): Path<(i64, i64)>,
) -> ApiResult<StatusCode> {
    require_squad_manager(&state, &user.0, id).await?;
    // F1: removing a server the actor cannot access is refused too — a
    // manager must not un-assign servers they cannot see (mirrors the
    // add-side gate; root admins pass automatically).
    require_server_access(&state, &user.0, &[sid]).await?;
    // The squad must exist so the audit record always names a real squad;
    // removing a server that is not in it is otherwise a row-level no-op.

    blocking(state.db.clone(), move |db| models::get_squad(&db, id))
        .await
        .map_err(|_| ApiError::not_found("squad not found"))?;

    blocking(state.db.clone(), move |db| {
        models::remove_squad_server(&db, id, sid)
    })
    .await?;

    let detail = format!("server #{sid} removed");
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.update",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn squad_set_servers(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SquadServersReq>,
) -> ApiResult<Json<serde_json::Value>> {
    require_squad_manager(&state, &user.0, id).await?;
    // De-duplicate the request set; a duplicate id would trip the PK on insert.
    let mut ids = req.server_ids;
    ids.sort_unstable();
    ids.dedup();

    // F1: check every target id before applying — set_squad_servers is
    // atomic, so the access checks run first in one blocking unit.
    require_server_access(&state, &user.0, &ids).await?;

    let ids2 = ids.clone();
    blocking(state.db.clone(), move |db| {
        models::set_squad_servers(&db, id, &ids2)
    })
    .await
    .map_err(|e| {
        let msg = e.to_string();
        if msg.contains("squad not found") || msg.contains("server not found") {
            ApiError::not_found(msg)
        } else {
            ApiError::from(e)
        }
    })?;

    let detail = format!(
        "servers set to [{}]",
        ids.iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let actor_id = user.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(actor_id),
            "squad.update",
            &format!("squad #{id}"),
            "",
            &detail,
        )
    })
    .await?;
    Ok(data(serde_json::json!({ "server_ids": ids })))
}

// ---------------- Stats ----------------

pub async fn stats(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let s = access_ok(&state, &u, id).await?;
    if s.node != "local" {
        let nname = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &nname)
        })
        .await?;
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
    Ok(Json(
        serde_json::json!({ "cpu": info.cpu_percent, "memory_bytes": info.memory_bytes, "memory_mb": info.memory_bytes / 1_048_576,
        "memory_limit_mb": s.memory_mb, "memory_percent": info.memory_percent, "disk_bytes": disk, "disk_mb": disk / 1_048_576,
        "disk_limit_mb": s.disk_mb, "rx_bytes": info.bandwidth_rx_bytes, "tx_bytes": info.bandwidth_tx_bytes,
        "uptime_secs": info.uptime_secs, "pid": info.pid, "status": info.status, "node": "local", "remote": false }),
    ))
}

#[derive(Deserialize)]
pub struct SendCmdReq {
    pub command: String,
}

pub async fn send_command(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<SendCmdReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    let s = access_ok(&state, u, id).await?;
    super::require_capability(&state, &user, id, Capability::ConsoleWrite).await?;
    ensure_operational(&s, u)?;
    if req.command.trim().is_empty() {
        return Err(ApiError::bad_request("command must not be empty"));
    }
    if req.command.len() > 4096 {
        return Err(ApiError::bad_request("command must not exceed 4096 bytes"));
    }
    if s.node != "local" {
        let nname = s.node.clone();
        let node = blocking(state.db.clone(), move |db| {
            crate::nodes::get_by_name(&db, &nname)
        })
        .await?;
        state
            .node_client
            .command(&node, &s.uuid, &req.command)
            .await
            .map_err(lifecycle_error)?;
    } else {
        // Route through the hub's dedicated per-server stdin writer (one std
        // thread, bounded queue, per-command ack): a child that stops draining
        // its pipe can only wedge that writer thread, never this Tokio worker
        // or the stdin mutex. The writer thread is spawned on demand, so a
        // server with no hub entry yet still gets one; a stopped child fails
        // as WriteFailed ("server not running") instead of blocking here.
        state
            .hub
            .write_stdin(s.id, state.procs.clone(), format!("{}\n", req.command))
            .await
            .map_err(stdin_error)?;
    }
    Ok(ok(serde_json::json!({ "ok": true, "node": s.node })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::keys::KeyScope;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn server(id: i64) -> Server {
        Server {
            id,
            uuid: format!("u{id}"),
            name: format!("s{id}"),
            user_id: 1,
            blueprint_id: 1,
            description: String::new(),
            status: "offline".into(),
            runtime_hint: String::new(),
            startup: String::new(),
            node: "local".into(),
            port: None,
            memory_mb: 0,
            disk_mb: 0,
            cpu_percent: 0,
            suspended: false,
            auto_restart: false,
            restart_count: 0,
            crash_detect_clean_exit: false,
            crash_restart_budget: 5,
            crash_restarts: 0,
            crash_window_start: String::new(),
            crash_reason: String::new(),
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }
    fn user_with(scope: Option<KeyScope>) -> User {
        User {
            id: 1,
            username: "owner".into(),
            email: "owner@t".into(),
            avatar: String::new(),
            language: "en".into(),
            theme: "dark".into(),
            root_admin: false,
            active: true,
            twofa_secret: None,
            twofa_enabled: false,
            about: String::new(),
            created_at: "now".into(),
            updated_at: "now".into(),
            key_scope: scope,
        }
    }
    #[test]
    fn scoped_server_ids_restrict_the_list_to_named_servers() {
        let servers = vec![server(1), server(2), server(3)];
        let filter = |u: &User| {
            let mut s = servers.clone();
            if let Some(ids) = scoped_server_ids(u) {
                s.retain(|sv| ids.contains(&sv.id));
            }
            s
        };

        // Session user: no scope, sees everything.
        let session = user_with(None);
        assert_eq!(filter(&session).len(), 3);
        // Wildcard capability does NOT widen the server set: server_ids wins.
        let scoped = user_with(Some(KeyScope {
            capabilities: vec![],
            wildcard: true,
            server_ids: vec![1, 3],
        }));
        assert_eq!(
            filter(&scoped).iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        // Empty server_ids means every server the owner can see.
        let all = user_with(Some(KeyScope {
            capabilities: vec![],
            wildcard: false,
            server_ids: vec![],
        }));
        assert_eq!(filter(&all).len(), 3);
    }

    #[test]
    fn blank_names_are_rejected_and_valid_names_are_trimmed() {
        assert_eq!(validate_name("  web-01  ").unwrap(), "web-01");
        for blank in ["", "   ", "\t\n"] {
            let err = validate_name(blank).unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn name_length_boundary_counts_characters_not_bytes() {
        // 128 multi-byte chars is 384 bytes: a byte-length check would reject it.
        let ok = "é".repeat(128);
        assert_eq!(validate_name(&ok).unwrap(), ok);
        let too_long = "é".repeat(129);
        assert_eq!(
            validate_name(&too_long).unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn control_characters_are_rejected() {
        // Newlines would break audit lines and agent specs.
        assert_eq!(
            validate_name("web\n01").unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_name("web\u{0}01").unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    // ---- behavioral: monitor limit lifecycle on remote nodes ----

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        let hub = Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(crate::services::proc::ProcManager::new(
            db.clone(),
            hub.clone(),
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
        };
        (tmp, state)
    }

    /// A root admin user with a live session cookie plus a server it owns,
    /// optionally pinned to the named node.
    fn seed(state: &AppState, uuid: &str, node: Option<&str>) -> (i64, String) {
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
            &state.db,
            uuid,
            "srv",
            user_id,
            blueprint_id,
            "generic",
            "echo",
            512,
            1024,
            100,
        )
        .unwrap();
        if let Some(name) = node {
            models::set_server_node(&state.db, server_id, name).unwrap();
        }
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            user_id,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        (server_id, format!("vp_session={raw}"))
    }

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/api/servers/:id", axum::routing::delete(delete))
            .route("/api/servers/:id/purge", axum::routing::delete(purge))
            .with_state(state)
    }

    async fn request(
        state: AppState,
        method: &str,
        uri: &str,
        cookie: &str,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        let req = builder.body(Body::from("")).unwrap();
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
    async fn remote_delete_unreachable_agent_retains_row_and_records_pending() {
        let (_tmp, state) = test_state();
        crate::nodes::create(&state.db, "n1", "http://127.0.0.1:1", "dc", &[]).unwrap();
        let (server_id, cookie) = seed(&state, "uuid-rdel", Some("n1"));
        state.monitor.set_limit(services::ServerLimits {
            server_id,
            memory_mb: 512,
            cpu_percent: 100,
            bandwidth_rx: 0,
            bandwidth_tx: 0,
        });
        let (status, body) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_id}"),
            &cookie,
        )
        .await;
        // The unreachable node (port 1) must NOT soft-delete the row: the
        // workload may still run on the agent, so the control-plane reference
        // is kept for a retry and an explicit pending-delete event recorded.
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("retained for retry"));
        assert!(models::get_server(&state.db, server_id).is_ok());
        assert!(state.monitor.limit_for_test(server_id).is_some());
        let pending: i64 = state
            .db
            .get().unwrap()
            .query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE action='server_delete_pending' AND server_id=?1",
                [server_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
    }

    #[test]
    fn lifecycle_conflicts_map_to_409_but_generic_errors_stay_500() {
        assert_eq!(
            lifecycle_error(anyhow::anyhow!("server already running")).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            lifecycle_error(anyhow::anyhow!("server not running")).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            lifecycle_error(anyhow::anyhow!("boom")).status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn remote_purge_removes_monitor_limit_even_without_node_row() {
        let (_tmp, state) = test_state();
        // No nodes row on purpose: purge must finish locally (dropping the
        // monitor entry too) even when the node is already gone.
        let (server_id, cookie) = seed(&state, "uuid-rpurge", Some("ghost-node"));
        state.monitor.set_limit(services::ServerLimits {
            server_id,
            memory_mb: 512,
            cpu_percent: 100,
            bandwidth_rx: 0,
            bandwidth_tx: 0,
        });
        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/servers/{server_id}/purge"),
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(state.monitor.limit_for_test(server_id).is_none());
        assert!(models::get_server_any(&state.db, server_id).is_err());
    }

    #[tokio::test]
    async fn list_paginates_with_defaults_and_reports_total() {
        let (_tmp, state) = test_state();
        let user_id = models::create_user(
            &state.db,
            "page-owner",
            "page@x.io",
            "h",
            false,
            "en",
            "dark",
        )
        .unwrap();
        let blueprint_id = models::create_blueprint(
            &state.db,
            "bp-page",
            "page-bp",
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
        for i in 0..55 {
            models::create_server(
                &state.db,
                &format!("uuid-page-{i:02}"),
                &format!("srv-{i:02}"),
                user_id,
                blueprint_id,
                "generic",
                "echo",
                512,
                1024,
                100,
            )
            .unwrap();
        }
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            user_id,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        let cookie = format!("vp_session={raw}");
        let router = Router::new()
            .route("/api/servers", axum::routing::get(list))
            .with_state(state.clone());
        let call = |uri: &str| {
            let uri = uri.to_string();
            let router = router.clone();
            let cookie = cookie.clone();
            async move {
                let req = Request::builder()
                    .method("GET")
                    .uri(&uri)
                    .header("cookie", &cookie)
                    .body(Body::from(""))
                    .unwrap();
                let response = router.oneshot(req).await.unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                )
            }
        };

        // Defaults: page 1, limit 50 — 50 of 55 rows, total still 55.
        let (status, body) = call("/api/servers").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 55);
        assert_eq!(body["page"], 1);
        assert_eq!(body["limit"], 50);
        assert_eq!(body["data"].as_array().unwrap().len(), 50);
        assert_eq!(body["data"][0]["name"], "srv-00");

        // Page 2 carries the remainder.
        let (status, body) = call("/api/servers?page=2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["page"], 2);
        assert_eq!(body["data"].as_array().unwrap().len(), 5);
        assert_eq!(body["data"][0]["name"], "srv-50");

        // Explicit limit below the cap, and the cap clamps oversized ones.
        let (_, body) = call("/api/servers?page=1&limit=10").await;
        assert_eq!(body["limit"], 10);
        assert_eq!(body["data"].as_array().unwrap().len(), 10);
        let (_, body) = call("/api/servers?limit=999").await;
        assert_eq!(body["limit"], 200);

        // page=0 is coerced to page 1, never an underflow or empty window.
        let (_, body) = call("/api/servers?page=0").await;
        assert_eq!(body["page"], 1);
        assert_eq!(body["data"].as_array().unwrap().len(), 50);
    }
    // ---------------- Squads ----------------

    fn squad_router(state: AppState) -> Router {
        Router::new()
            .route(
                "/api/admin/squads",
                axum::routing::get(squad_list).post(squad_create),
            )
            .route(
                "/api/admin/squads/:id",
                axum::routing::get(squad_get)
                    .patch(squad_update)
                    .delete(squad_delete),
            )
            .route(
                "/api/admin/squads/:id/members",
                axum::routing::post(squad_add_member),
            )
            .route(
                "/api/admin/squads/:id/members/:uid",
                axum::routing::patch(squad_update_member).delete(squad_remove_member),
            )
            .route(
                "/api/admin/squads/:id/servers",
                axum::routing::put(squad_set_servers).post(squad_add_server),
            )
            .route(
                "/api/admin/squads/:id/servers/:sid",
                axum::routing::delete(squad_remove_server),
            )
            .route("/api/servers", axum::routing::get(list))
            .route("/api/servers/:id", axum::routing::get(get))
            .with_state(state)
    }

    async fn request_json(
        router: Router,
        method: &str,
        uri: &str,
        cookie: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("content-type", "application/json");
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let response = router.oneshot(req).await.unwrap();
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
    async fn squad_crud_audits_and_detail_shape() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "sq-crud", None);
        let member_id =
            models::create_user(&state.db, "sq-member", "sqm@x.io", "h", false, "en", "dark")
                .unwrap();
        let router = squad_router(state.clone());

        // Create.
        let (status, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &cookie,
            serde_json::json!({ "name": "Web Team" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let squad_id = body["data"]["id"].as_i64().unwrap();
        assert_eq!(body["data"]["name"], "Web Team");

        // List carries the empty counts.
        let (status, body) = request_json(
            router.clone(),
            "GET",
            "/api/admin/squads",
            &cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let item = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == serde_json::json!(squad_id))
            .unwrap();
        assert_eq!(item["member_count"], 0);
        assert_eq!(item["server_count"], 0);

        // Rename.
        let (status, body) = request_json(
            router.clone(),
            "PATCH",
            &format!("/api/admin/squads/{squad_id}"),
            &cookie,
            serde_json::json!({ "name": "Core Team" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["name"], "Core Team");

        // Add member.
        let (status, body) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "manager" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["data"]["role"], "manager");

        // Add server.
        let (status, body) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["data"]["server_id"], server_id);

        // Detail carries members + servers.
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["members"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"]["members"][0]["id"], member_id);
        assert_eq!(body["data"]["servers"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"]["servers"][0]["id"], server_id);

        // Member role update.
        let (status, body) = request_json(
            router.clone(),
            "PATCH",
            &format!("/api/admin/squads/{squad_id}/members/{member_id}"),
            &cookie,
            serde_json::json!({ "role": "developer" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["role"], "developer");

        // Remove member.
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}/members/{member_id}"),
            &cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Replace-all servers via PUT.
        let (status, body) = request_json(
            router.clone(),
            "PUT",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_ids": [server_id] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["server_ids"].as_array().unwrap().len(), 1);

        // Delete squad.
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}"),
            &cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Every pinned audit event was recorded (rename + add server +
        // replace-all = three squad.update rows).
        let conn = state.db.get().unwrap();
        let count = |action: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE action=?1",
                [action],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count("squad.create"), 1);
        assert_eq!(count("squad.update"), 3);
        assert_eq!(count("squad.delete"), 1);
        assert_eq!(count("squad.member.add"), 1);
        assert_eq!(count("squad.member.update"), 1);
        assert_eq!(count("squad.member.remove"), 1);

        // Deleted squad is gone.
        let (status, _) = request_json(
            router,
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn squad_errors_403_409_404_and_role_validation() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "sq-err", None);
        let router = squad_router(state.clone());

        // Non-admin is rejected at the door.
        let pleb_id =
            models::create_user(&state.db, "sq-pleb", "sqp@x.io", "h", false, "en", "dark")
                .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            pleb_id,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        let pleb_cookie = format!("vp_session={raw}");
        let (status, _) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &pleb_cookie,
            serde_json::json!({ "name": "x" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &cookie,
            serde_json::json!({ "name": "Team" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let squad_id = body["data"]["id"].as_i64().unwrap();
        let member_id =
            models::create_user(&state.db, "sq-m", "sqm2@x.io", "h", false, "en", "dark").unwrap();

        // Duplicate membership -> 409, unknown user -> 404.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("already a member"));
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": 999_999, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Unknown squad, and the custom role is not valid for squads.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads/424242/members",
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "custom" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("custom"));

        // Blank names are rejected.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &cookie,
            serde_json::json!({ "name": "   " }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Server add: unknown server -> 404, duplicate -> 409.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_id": 424_242 }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn squad_member_sees_grouped_server_in_listing_and_detail() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "sq-vis", None);
        let router = squad_router(state.clone());

        let (_, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &cookie,
            serde_json::json!({ "name": "Shared" }),
        )
        .await;
        let squad_id = body["data"]["id"].as_i64().unwrap();
        let member_id = models::create_user(
            &state.db,
            "sq-vis-member",
            "sqvm@x.io",
            "h",
            false,
            "en",
            "dark",
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            member_id,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        let member_cookie = format!("vp_session={raw}");
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &cookie,
            serde_json::json!({ "user_id": member_id, "role": "operator" }),
        )
        .await;
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;

        // The member's listing includes the grouped server...
        let (status, body) = request_json(
            router.clone(),
            "GET",
            "/api/servers",
            &member_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == serde_json::json!(server_id)));

        // ...and direct access resolves the squad role grant (not 403).
        let (status, _) = request_json(
            router,
            "GET",
            &format!("/api/servers/{server_id}"),
            &member_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    async fn request_json_auth(
        router: Router,
        method: &str,
        uri: &str,
        auth: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", auth)
            .header("content-type", "application/json");
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let response = router.oneshot(req).await.unwrap();
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

    fn squad_cookie(state: &AppState, uid: i64) -> String {
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            uid,
            "test-agent",
            "127.0.0.1",
            false,
        )
        .unwrap();
        format!("vp_session={raw}")
    }

    #[tokio::test]
    async fn squad_manager_can_manage_members_and_servers_but_not_delete() {
        let (_tmp, state) = test_state();
        let (server_id, root_cookie) = seed(&state, "sq-mgr", None);
        let router = squad_router(state.clone());

        let (_, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &root_cookie,
            serde_json::json!({ "name": "Mgr Team" }),
        )
        .await;
        let squad_id = body["data"]["id"].as_i64().unwrap();

        let mgr_id =
            models::create_user(&state.db, "sq-mgr2", "mg2@x.io", "h", false, "en", "dark")
                .unwrap();
        let dev_id =
            models::create_user(&state.db, "sq-dev2", "dv2@x.io", "h", false, "en", "dark")
                .unwrap();
        let mgr_cookie = squad_cookie(&state, mgr_id);
        let dev_cookie = squad_cookie(&state, dev_id);

        // Root seats a manager.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &root_cookie,
            serde_json::json!({ "user_id": mgr_id, "role": "manager" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        // F1: a manager may only assign servers they already hold access to —
        // seat the manager on the target server like add_subuser requires.
        models::add_subuser(&state.db, server_id, mgr_id, &Grant::new(Role::Viewer, [])).unwrap();

        // The manager adds a member at a role it holds (developer <= manager),
        // then demotes and removes them.
        let (status, body) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &mgr_cookie,
            serde_json::json!({ "user_id": dev_id, "role": "developer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["data"]["role"], "developer");
        let (status, _) = request_json(
            router.clone(),
            "PATCH",
            &format!("/api/admin/squads/{squad_id}/members/{dev_id}"),
            &mgr_cookie,
            serde_json::json!({ "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}/members/{dev_id}"),
            &mgr_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Re-add and drive the server set as the manager.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &mgr_cookie,
            serde_json::json!({ "user_id": dev_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &mgr_cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body) = request_json(
            router.clone(),
            "PUT",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &mgr_cookie,
            serde_json::json!({ "server_ids": [server_id] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["server_ids"].as_array().unwrap().len(), 1);
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}/servers/{server_id}"),
            &mgr_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        // F1: a manager cannot assign a server it has no access to (zero prior
        // access must not mint the Manager preset on it) — nor un-assign one.
        let (alien_server, _) = seed(&state, "sq-mgr-alien", None);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &mgr_cookie,
            serde_json::json!({ "server_id": alien_server }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "PUT",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &mgr_cookie,
            serde_json::json!({ "server_ids": [server_id, alien_server] }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // The rejected replace-all must not half-apply: the set stays empty
        // (the manager removed its only server above).
        assert!(
            models::squad_servers(&state.db, squad_id).unwrap().is_empty(),
            "forbidden set apply leaked a server assignment"
        );
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}/servers/{alien_server}"),
            &mgr_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Rename is manager-scoped (the squad identity is shared).
        let (status, body) = request_json(
            router.clone(),
            "PATCH",
            &format!("/api/admin/squads/{squad_id}"),
            &mgr_cookie,
            serde_json::json!({ "name": "Renamed by Manager" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["name"], "Renamed by Manager");

        // ...but a manager cannot delete the squad.
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}"),
            &mgr_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A plain member (viewer) is not a manager: every member/server op
        // 403s at the door, so no role can be minted above anyone's own.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &dev_cookie,
            serde_json::json!({ "user_id": mgr_id, "role": "manager" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}/members/{mgr_id}"),
            &dev_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &dev_cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "PATCH",
            &format!("/api/admin/squads/{squad_id}"),
            &dev_cookie,
            serde_json::json!({ "name": "nope" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Deletion stays creator-or-root: the root can delete it.
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{squad_id}"),
            &root_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn squad_get_my_role_and_creator_delete() {
        let (_tmp, state) = test_state();
        let (_, root_cookie) = seed(&state, "sq-role", None);
        let router = squad_router(state.clone());

        let (_, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &root_cookie,
            serde_json::json!({ "name": "Role Team" }),
        )
        .await;
        let squad_id = body["data"]["id"].as_i64().unwrap();

        // Root admin sees my_role=manager even without a membership row.
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &root_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["my_role"], "manager");

        let member_id =
            models::create_user(&state.db, "sq-role-m", "rm@x.io", "h", false, "en", "dark")
                .unwrap();
        let outsider_id =
            models::create_user(&state.db, "sq-role-o", "ro@x.io", "h", false, "en", "dark")
                .unwrap();
        let member_cookie = squad_cookie(&state, member_id);
        let outsider_cookie = squad_cookie(&state, outsider_id);

        // A member sees their actual role...
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &root_cookie,
            serde_json::json!({ "user_id": member_id, "role": "operator" }),
        )
        .await;
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &member_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["my_role"], "operator");

        // ...a non-member gets the minimal view: my_role null and NO roster
        // arrays (members/servers are a disclosure to members only)...
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &outsider_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["data"]["my_role"].is_null());
        assert!(
            body["data"].get("members").is_none(),
            "outsider must not see the members roster"
        );
        assert!(
            body["data"].get("servers").is_none(),
            "outsider must not see the servers roster"
        );
        assert_eq!(body["data"]["name"], "Role Team");

        // ...and a non-member cannot manage anything.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &outsider_cookie,
            serde_json::json!({ "user_id": member_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A non-root creator (seated via the model; create stays admin-only)
        // sees my_role=manager on its own squad and can delete it — but a
        // manager member cannot.
        let creator_id =
            models::create_user(&state.db, "sq-creator", "cr@x.io", "h", false, "en", "dark")
                .unwrap();
        let creator_cookie = squad_cookie(&state, creator_id);
        let owner_mgr_id =
            models::create_user(&state.db, "sq-own-mgr", "om@x.io", "h", false, "en", "dark")
                .unwrap();
        let owner_mgr_cookie = squad_cookie(&state, owner_mgr_id);
        let owned = models::create_squad(&state.db, "Creator Owned", creator_id).unwrap();
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{owned}/members"),
            &root_cookie,
            serde_json::json!({ "user_id": owner_mgr_id, "role": "manager" }),
        )
        .await;
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{owned}"),
            &creator_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["my_role"], "manager");
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{owned}"),
            &owner_mgr_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "DELETE",
            &format!("/api/admin/squads/{owned}"),
            &creator_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn squad_get_outsider_minimal_view_blocks_roster_and_ops() {
        let (_tmp, state) = test_state();
        let (server_id, root_cookie) = seed(&state, "sq-min", None);
        let router = squad_router(state.clone());

        let (_, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &root_cookie,
            serde_json::json!({ "name": "Min Team" }),
        )
        .await;
        let squad_id = body["data"]["id"].as_i64().unwrap();

        // Seed a member and a server so the squad has a non-empty roster.
        let member_id =
            models::create_user(&state.db, "sq-min-m", "minm@x.io", "h", false, "en", "dark")
                .unwrap();
        let outsider_id =
            models::create_user(&state.db, "sq-min-o", "mino@x.io", "h", false, "en", "dark")
                .unwrap();
        let outsider_cookie = squad_cookie(&state, outsider_id);
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &root_cookie,
            serde_json::json!({ "user_id": member_id, "role": "operator" }),
        )
        .await;
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &root_cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;

        // Outsider GET: {id, name, my_role:null} only — no members/servers.
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &outsider_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["id"], serde_json::json!(squad_id));
        assert_eq!(body["data"]["name"], "Min Team");
        assert!(body["data"]["my_role"].is_null());
        assert!(
            body["data"].get("members").is_none(),
            "outsider must not see the members roster"
        );
        assert!(
            body["data"].get("servers").is_none(),
            "outsider must not see the servers roster"
        );

        // Members still see the full roster.
        let member_cookie = squad_cookie(&state, member_id);
        let (status, body) = request_json(
            router.clone(),
            "GET",
            &format!("/api/admin/squads/{squad_id}"),
            &member_cookie,
            serde_json::json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["data"]["members"].is_array());
        assert!(body["data"]["servers"].is_array());

        // The outsider cannot add members or touch server assignment.
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &outsider_cookie,
            serde_json::json!({ "user_id": member_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &outsider_cookie,
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn squad_ops_accept_key_auth_but_only_for_squad_managers() {
        let (_tmp, state) = test_state();
        let (server_id, root_cookie) = seed(&state, "sq-key", None);
        let router = squad_router(state.clone());

        let (_, body) = request_json(
            router.clone(),
            "POST",
            "/api/admin/squads",
            &root_cookie,
            serde_json::json!({ "name": "Key Team" }),
        )
        .await;
        let squad_id = body["data"]["id"].as_i64().unwrap();

        let mgr_id =
            models::create_user(&state.db, "sq-key-m", "km@x.io", "h", false, "en", "dark")
                .unwrap();
        let pleb_id =
            models::create_user(&state.db, "sq-key-p", "kp@x.io", "h", false, "en", "dark")
                .unwrap();
        request_json(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &root_cookie,
            serde_json::json!({ "user_id": mgr_id, "role": "manager" }),
        )
        .await;

        // Scoped keys: the capability/server_ids filter does not narrow squad
        // ops (they are squad-scoped, not server-capability-scoped), so a
        // manager's key passes and a non-manager's key is rejected at the
        // manager gate.
        let (_, mgr_token) = crate::services::keys::create(
            &state.db,
            mgr_id,
            "mgr-key",
            &["console.read".to_string()],
            &[],
            None,
        )
        .unwrap();
        let (_, pleb_token) = crate::services::keys::create(
            &state.db,
            pleb_id,
            "pleb-key",
            &["console.read".to_string()],
            &[],
            None,
        )
        .unwrap();

        let (status, _) = request_json_auth(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/members"),
            &format!("Bearer {mgr_token}"),
            serde_json::json!({ "user_id": pleb_id, "role": "viewer" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, _) = request_json_auth(
            router.clone(),
            "POST",
            &format!("/api/admin/squads/{squad_id}/servers"),
            &format!("Bearer {pleb_token}"),
            serde_json::json!({ "server_id": server_id }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

}
