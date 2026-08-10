//! Database-backed data models + CRUD helpers.
use crate::capability::{Capability, Grant, Role};
use crate::db::Db;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

fn now() -> String {
    Utc::now().to_rfc3339()
}

// ---------------- User ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub avatar: String,
    pub language: String,
    pub theme: String,
    pub root_admin: bool,
    pub active: bool,
    #[serde(skip)]
    pub twofa_secret: Option<String>,
    pub twofa_enabled: bool,
    pub about: String,
    pub created_at: String,
    pub updated_at: String,
    /// Set only when the request authenticated with a scoped API key. Narrows
    /// every capability check to the key's grant; never crosses the API surface.
    #[serde(skip)]
    pub key_scope: Option<crate::services::keys::KeyScope>,
}

pub fn user_from_row(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        avatar: r.get(3)?,
        language: r.get(4)?,
        theme: r.get(5)?,
        root_admin: r.get::<_, i64>(6)? != 0,
        active: r.get::<_, i64>(7)? != 0,
        twofa_secret: r.get(8)?,
        twofa_enabled: r.get::<_, Option<String>>(8)?.is_some(),
        about: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        key_scope: None,
    })
}

pub fn get_user(db: &Db, id: i64) -> Result<User> {
    let conn = db.get()?;
    conn.query_row(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users WHERE id=?1",
        [id],
        user_from_row,
    )
    .context("user not found")
}

pub fn get_user_by_name(db: &Db, username: &str) -> Result<User> {
    let conn = db.get()?;
    conn.query_row(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users WHERE username=?1",
        [username],
        user_from_row,
    )
    .context("user not found")
}


pub fn list_users(db: &Db) -> Result<Vec<User>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT id,username,email,avatar,language,theme,root_admin,active,twofa_secret,about,created_at,updated_at FROM users ORDER BY id",
    )?;
    let rows = stmt.query_map([], user_from_row)?;
    let mut out = Vec::new();
    for u in rows {
        out.push(u?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub fn create_user(
    db: &Db,
    username: &str,
    email: &str,
    password_hash: &str,
    root_admin: bool,
    language: &str,
    theme: &str,
) -> Result<i64> {
    let conn = db.get()?;
    let t = now();
    conn.execute(
        "INSERT INTO users(username,email,password_hash,root_admin,language,theme,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
        params![username, email, password_hash, root_admin as i64, language, theme, t],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_user(db: &Db, u: &User) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE users SET email=?1, avatar=?2, language=?3, theme=?4, root_admin=?5, active=?6, about=?7, updated_at=?8 WHERE id=?9",
        params![
            u.email,
            u.avatar,
            u.language,
            u.theme,
            u.root_admin as i64,
            u.active as i64,
            u.about,
            now(),
            u.id
        ],
    )?;
    Ok(())
}

pub fn set_password(db: &Db, user_id: i64, hash: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE users SET password_hash=?1, updated_at=?2 WHERE id=?3",
        params![hash, now(), user_id],
    )?;
    Ok(())
}

pub fn set_twofa_secret(db: &Db, user_id: i64, secret: Option<&str>) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE users SET twofa_secret=?1, updated_at=?2 WHERE id=?3",
        params![secret, now(), user_id],
    )?;
    Ok(())
}

pub fn delete_user(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM users WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Blueprint ----------------

/// Typed constraint attached to a blueprint input. Replaces the pipe-rule DSL:
/// the shape of a value is declared once and enforced identically by the API,
/// the seeder, and the launch templater.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputKind {
    /// Free text, optionally length-capped and pattern-checked.
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    /// Numeric value with inclusive bounds.
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    /// Boolean rendered as a toggle.
    Bool,
    /// One of a fixed option set.
    Choice { options: Vec<String> },
    /// Workspace-relative path. Rejects absolute paths and `..` traversal so a
    /// value cannot escape the workspace when interpolated into a launch plan.
    Path {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_len: Option<usize>,
    },
    /// Absolute http(s) URL.
    Url,
}

impl Default for InputKind {
    fn default() -> Self {
        Self::Text {
            max_len: None,
            pattern: None,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueprintInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub env_var: String,
    #[serde(default)]
    pub default_value: String,
    #[serde(default = "default_true")]
    pub user_viewable: bool,
    #[serde(default = "default_true")]
    pub user_editable: bool,
    /// Empty values are rejected when set.
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub kind: InputKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub category: String,
    pub runtime_hint: String,
    pub startup: String,
    pub default_config: Option<String>,
    pub install_script: Option<String>,
    pub variables: Vec<BlueprintInput>,
    pub stop_command: String,
    pub created_at: String,
    pub updated_at: String,
}

fn blueprint_from_row(r: &Row) -> rusqlite::Result<Blueprint> {
    Ok(Blueprint {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        author: r.get(4)?,
        category: r.get(5)?,
        runtime_hint: r.get(6)?,
        startup: r.get(7)?,
        default_config: r.get(8)?,
        install_script: r.get(9)?,
        variables: {
            let raw: String = r.get(10)?;
            serde_json::from_str(&raw).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    10,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
        },
        stop_command: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

const BLUEPRINT_COLS: &str =
    "id,uuid,name,description,author,category,runtime_hint,startup,default_config,install_script,variables,stop_command,created_at,updated_at";

pub fn get_blueprint(db: &Db, id: i64) -> Result<Blueprint> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {BLUEPRINT_COLS} FROM blueprints WHERE id=?1"),
        [id],
        blueprint_from_row,
    )
    .context("blueprint not found")
}

pub fn list_blueprints(db: &Db) -> Result<Vec<Blueprint>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {BLUEPRINT_COLS} FROM blueprints ORDER BY name"
    ))?;
    let rows = stmt.query_map([], blueprint_from_row)?;
    let mut out = Vec::new();
    for e in rows {
        out.push(e?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub fn create_blueprint(
    db: &Db,
    uuid: &str,
    name: &str,
    description: &str,
    author: &str,
    category: &str,
    runtime_hint: &str,
    startup: &str,
    default_config: Option<&str>,
    install_script: Option<&str>,
    variables: &[BlueprintInput],
    stop_command: &str,
) -> Result<i64> {
    let conn = db.get()?;
    let t = now();
    conn.execute(
        "INSERT INTO blueprints(uuid,name,description,author,category,runtime_hint,startup,default_config,install_script,variables,stop_command,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
        params![
            uuid,
            name,
            description,
            author,
            category,
            runtime_hint,
            startup,
            default_config,
            install_script,
            serde_json::to_string(variables)?,
            stop_command,
            t
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_blueprint(db: &Db, blueprint: &Blueprint) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE blueprints SET name=?1, description=?2, author=?3, category=?4, runtime_hint=?5, startup=?6, default_config=?7, install_script=?8, variables=?9, stop_command=?10, updated_at=?11 WHERE id=?12",
        params![
            blueprint.name,
            blueprint.description,
            blueprint.author,
            blueprint.category,
            blueprint.runtime_hint,
            blueprint.startup,
            blueprint.default_config,
            blueprint.install_script,
            serde_json::to_string(&blueprint.variables)?,
            blueprint.stop_command,
            now(),
            blueprint.id
        ],
    )?;
    Ok(())
}

pub fn delete_blueprint(db: &Db, id: i64) -> Result<()> {
    // Count every server FK reference, including soft-deleted servers, so the
    // blueprint row is never orphaned by a later undelete/purge. Soft-deleted
    // servers keep blueprint_id pointing at this row. BEGIN IMMEDIATE keeps
    // the count-and-delete atomic across pooled connections (the old global
    // mutex used to serialize this).
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let used: i64 = tx.query_row(
        "SELECT COUNT(*) FROM servers WHERE blueprint_id=?1",
        [id],
        |r| r.get(0),
    )?;
    if used > 0 {
        bail!("blueprint is used by {used} workspace(s)");
    }
    tx.execute("DELETE FROM blueprints WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

/// Number of servers (including soft-deleted) that still reference a blueprint.
/// Used by the API to surface a clean 409 instead of a generic 500.
pub fn blueprint_references(db: &Db, id: i64) -> Result<i64> {
    let conn = db.get()?;
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE blueprint_id=?1",
        [id],
        |r| r.get(0),
    )?)
}

// ---------------- Server ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub user_id: i64,
    pub blueprint_id: i64,
    pub description: String,
    pub status: String,
    pub runtime_hint: String,
    pub startup: String,
    pub node: String,
    pub port: Option<i64>,
    pub memory_mb: i64,
    pub disk_mb: i64,
    pub cpu_percent: i64,
    pub suspended: bool,
    pub auto_restart: bool,
    pub restart_count: i64,
    /// Crash policy (G8): whether an unrequested clean exit is treated as a
    /// crash, per the detect-clean-exit-as-crash toggle.
    pub crash_detect_clean_exit: bool,
    /// Max auto-restarts one crash burst may consume before the server is
    /// left in a terminal `crashed` state. 0 disables crash restarts.
    pub crash_restart_budget: i64,
    /// Auto-restarts already consumed in the current crash burst.
    pub crash_restarts: i64,
    /// RFC3339 of the first crash of the current burst; '' = no burst.
    pub crash_window_start: String,
    /// Last exit classification, surfaced by the console crash endpoint.
    pub crash_reason: String,
    pub created_at: String,
    pub updated_at: String,
}

fn server_from_row(r: &Row) -> rusqlite::Result<Server> {
    Ok(Server {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        user_id: r.get(3)?,
        blueprint_id: r.get(4)?,
        description: r.get(5)?,
        status: r.get(6)?,
        runtime_hint: r.get(7)?,
        startup: r.get(8)?,
        node: r.get(9)?,
        port: r.get(10)?,
        memory_mb: r.get(11)?,
        disk_mb: r.get(12)?,
        cpu_percent: r.get(13)?,
        suspended: r.get::<_, i64>(14)? != 0,
        auto_restart: r.get::<_, i64>(15)? != 0,
        restart_count: r.get(16)?,
        crash_detect_clean_exit: r.get::<_, i64>(17)? != 0,
        crash_restart_budget: r.get(18)?,
        crash_restarts: r.get(19)?,
        crash_window_start: r.get(20)?,
        crash_reason: r.get(21)?,
        created_at: r.get(22)?,
        updated_at: r.get(23)?,
    })
}

const SERVER_COLS: &str =
    "id,uuid,name,user_id,blueprint_id,description,status,runtime_hint,startup,node,port,memory_mb,disk_mb,cpu_percent,suspended,auto_restart,restart_count,crash_detect_clean_exit,crash_restart_budget,crash_restarts,crash_window_start,crash_reason,created_at,updated_at";


pub fn get_server(db: &Db, id: i64) -> Result<Server> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {SERVER_COLS} FROM servers WHERE id=?1 AND deleted=0"),
        [id],
        server_from_row,
    )
    .context("server not found")
}

/// Fetch a server row regardless of soft-delete state. The admin purge path
/// uses this so a soft-deleted server can be physically removed instead of
/// 404-ing through the `deleted=0`-only lookup.
pub fn get_server_any(db: &Db, id: i64) -> Result<Server> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {SERVER_COLS} FROM servers WHERE id=?1"),
        [id],
        server_from_row,
    )
    .context("server not found")
}

pub fn set_server_node(db: &Db, id: i64, node: &str) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE allocations SET node=?1 WHERE server_id=?2",
        params![node, id],
    )?;
    tx.execute(
        "UPDATE servers SET node=?1,updated_at=?2 WHERE id=?3",
        params![node, now(), id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn servers_on_node(db: &Db, node: &str) -> Result<Vec<Server>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SERVER_COLS} FROM servers WHERE node=?1 AND deleted=0 ORDER BY name"
    ))?;
    let rows = stmt.query_map([node], server_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}
pub fn get_server_by_uuid(db: &Db, uuid: &str) -> Result<Server> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {SERVER_COLS} FROM servers WHERE uuid=?1 AND deleted=0"),
        [uuid],
        server_from_row,
    )
    .context("server not found")
}

pub fn list_servers(db: &Db, user_id: Option<i64>, include_deleted: bool) -> Result<Vec<Server>> {
    // `Some(uid)` lists by *access*, mirroring `user_has_server_access`: owned
    // workspaces OR workspaces the user is a team member on. Ownership-only
    // filtering hid every shared workspace from a subuser's `GET /api/servers`.
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let (sql, extra) = match user_id {
        Some(uid) => (
            format!(
                "SELECT {SERVER_COLS} FROM servers WHERE (user_id=?1 OR EXISTS(SELECT 1 FROM subusers WHERE subusers.server_id=servers.id AND subusers.user_id=?1) OR EXISTS(SELECT 1 FROM squad_members sm JOIN squad_servers ss ON ss.squad_id=sm.squad_id WHERE ss.server_id=servers.id AND sm.user_id=?1)){} ORDER BY name",
                if include_deleted {
                    ""
                } else {
                    " AND deleted=0"
                }
            ),
            vec![uid.to_string()],
        ),
        None => (
            format!(
                "SELECT {SERVER_COLS} FROM servers{} ORDER BY name",
                if include_deleted {
                    ""
                } else {
                    " WHERE deleted=0"
                }
            ),
            vec![],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(extra.iter()), server_from_row)?;
    let mut out = Vec::new();
    for s in rows {
        out.push(s?);
    }
    Ok(out)
}

pub fn count_servers(db: &Db) -> Result<i64> {
    let conn = db.get()?;
    Ok(
        conn.query_row("SELECT COUNT(*) FROM servers WHERE deleted=0", [], |r| {
            r.get(0)
        })?,
    )
}

/// Count servers still owned by a user that are NOT already soft-deleted.
/// Soft-deleted servers must not permanently block account deletion: the
/// `users(id) ON DELETE CASCADE` on `servers.user_id` removes their rows (and
/// dependent data) when the owner is deleted, and the admin purge path can
/// physically remove them first.
pub fn count_all_servers_by_user(db: &Db, user_id: i64) -> Result<i64> {
    let conn = db.get()?;
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE user_id=?1 AND deleted=0",
        [user_id],
        |r| r.get(0),
    )?)
}

#[allow(clippy::too_many_arguments)]
pub fn create_server(
    db: &Db,
    uuid: &str,
    name: &str,
    user_id: i64,
    blueprint_id: i64,
    runtime_hint: &str,
    startup: &str,
    memory_mb: i64,
    disk_mb: i64,
    cpu_percent: i64,
) -> Result<i64> {
    let conn = db.get()?;
    let t = now();
    conn.execute(
        "INSERT INTO servers(uuid,name,user_id,blueprint_id,runtime_hint,startup,memory_mb,disk_mb,cpu_percent,status,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'offline',?10,?10)",
        params![uuid, name, user_id, blueprint_id, runtime_hint, startup, memory_mb, disk_mb, cpu_percent, t],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_server(db: &Db, s: &Server) -> Result<()> {
    // Persist the whole editable struct, not the old 10-column subset that
    // silently dropped status/suspended/crash fields (an in-memory mutation
    // would never reach disk). `node`/`port` are deliberately excluded: they
    // are placement facts owned by set_server_node/allocations/transfer, and a
    // config snapshot must not resurrect a freed port or undo a transfer.
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET name=?1, description=?2, runtime_hint=?3, startup=?4, \
         user_id=?5, blueprint_id=?6, status=?7, memory_mb=?8, disk_mb=?9, cpu_percent=?10, \
         suspended=?11, auto_restart=?12, restart_count=?13, crash_detect_clean_exit=?14, \
         crash_restart_budget=?15, crash_restarts=?16, crash_window_start=?17, crash_reason=?18, \
         updated_at=?19 WHERE id=?20",
        params![
            s.name,
            s.description,
            s.runtime_hint,
            s.startup,
            s.user_id,
            s.blueprint_id,
            s.status,
            s.memory_mb,
            s.disk_mb,
            s.cpu_percent,
            s.suspended as i64,
            s.auto_restart as i64,
            s.restart_count,
            s.crash_detect_clean_exit as i64,
            s.crash_restart_budget,
            s.crash_restarts,
            s.crash_window_start,
            s.crash_reason,
            now(),
            s.id
        ],
    )?;
    Ok(())
}

pub fn set_server_status(db: &Db, id: i64, status: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET status=?1, updated_at=?2 WHERE id=?3",
        params![status, now(), id],
    )?;
    Ok(())
}

pub fn set_server_suspended(db: &Db, id: i64, suspended: bool) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET suspended=?1, updated_at=?2 WHERE id=?3",
        params![suspended as i64, now(), id],
    )?;
    Ok(())
}

pub fn bump_restart_count(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET restart_count=restart_count+1, updated_at=?1 WHERE id=?2",
        params![now(), id],
    )?;
    Ok(())
}
// ---------------- Crash policy (G8) ----------------

/// How long a crash burst stays "hot": a crash further apart than this from
/// the burst's first crash counts as a fresh burst with a fresh budget, so a
/// long-lived server that eventually dies nonzero gets a restart instead of
/// inheriting stale debt from a burst hours ago.
pub const CRASH_WINDOW_SECS: i64 = 60;

/// Outcome of consuming one crash-restart slot from a server's burst budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashBudget {
    /// A restart may proceed; `used` is how many restarts the burst already
    /// consumed before this one (the 0-based backoff index).
    Allowed(i64),
    /// The burst budget is exhausted; `used` is the count already consumed.
    Exhausted(i64),
}

/// Atomically advance a server's crash burst: expire a stale window, check
/// the budget, and (when a restart is allowed) consume one slot. The Db
/// mutex makes the read-modify-write atomic against concurrent callers.
///
/// Termination argument for the restart loop: every crash inside a hot window
/// either consumes one slot (`Allowed(used)` with `used < budget`) or returns
/// `Exhausted`; the consumed count strictly increases on every allowed
/// restart, so after at most `budget` restarts the next crash must return
/// `Exhausted`, at which point the caller leaves the server in the terminal
/// `crashed` state.
pub fn consume_crash_budget(db: &Db, id: i64, budget: i64) -> Result<CrashBudget> {
    let conn = db.get()?;
    let (used, window_start): (i64, String) = conn.query_row(
        "SELECT crash_restarts, crash_window_start FROM servers WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let expired = match chrono::DateTime::parse_from_rfc3339(&window_start) {
        Ok(t) => (Utc::now() - t.with_timezone(&Utc)).num_seconds() > CRASH_WINDOW_SECS,
        Err(_) => !window_start.is_empty(),
    };
    let effective = if expired { 0 } else { used };
    if effective >= budget {
        return Ok(CrashBudget::Exhausted(effective));
    }
    let ts = Utc::now().to_rfc3339();
    let new_window = if window_start.is_empty() || expired {
        ts.clone()
    } else {
        window_start
    };
    conn.execute(
        "UPDATE servers SET crash_restarts=?1, crash_window_start=?2, updated_at=?3 WHERE id=?4",
        params![effective + 1, new_window, ts, id],
    )?;
    Ok(CrashBudget::Allowed(effective))
}

/// Record a terminal crash: `crashed` status plus the classification reason
/// the console surfaces. The restart machinery leaves the server here once
/// the burst budget is exhausted (or when auto-restart is disabled).
pub fn mark_crashed(db: &Db, id: i64, reason: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET status='crashed', crash_reason=?1, updated_at=?2 WHERE id=?3",
        params![reason, now(), id],
    )?;
    Ok(())
}

/// Clear the crash burst (used slots, window, recorded reason). Called on an
/// operator-initiated start so a manual recovery never inherits stale burst
/// debt; auto-restarts deliberately do NOT reset it (that is what bounds the
/// crash loop).
pub fn reset_crash_window(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE servers SET crash_restarts=0, crash_window_start='', crash_reason='', updated_at=?1 WHERE id=?2",
        params![now(), id],
    )?;
    Ok(())
}

pub fn delete_server(db: &Db, id: i64) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM allocations WHERE server_id=?1", [id])?;
    tx.execute(
        "UPDATE servers SET deleted=1,port=NULL,updated_at=?1 WHERE id=?2",
        params![now(), id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn purge_server(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM servers WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Server variables ----------------

pub fn get_server_vars(db: &Db, server_id: i64) -> Result<Vec<(String, String)>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare("SELECT key,value FROM server_variables WHERE server_id=?1")?;
    let rows = stmt.query_map([server_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for v in rows {
        out.push(v?);
    }
    Ok(out)
}

pub fn set_server_var(
    db: &Db,
    server_id: i64,
    blueprint_id: i64,
    key: &str,
    value: &str,
) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO server_variables(server_id,blueprint_id,key,value) VALUES(?1,?2,?3,?4) ON CONFLICT(server_id,key) DO UPDATE SET value=?4",
        params![server_id, blueprint_id, key, value],
    )?;
    Ok(())
}

pub fn delete_server_vars(db: &Db, server_id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "DELETE FROM server_variables WHERE server_id=?1",
        [server_id],
    )?;
    Ok(())
}

pub fn replace_server_vars(
    db: &Db,
    server_id: i64,
    blueprint_id: i64,
    vars: &[(String, String)],
) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM server_variables WHERE server_id=?1",
        [server_id],
    )?;
    for (key, value) in vars {
        tx.execute(
            "INSERT INTO server_variables(server_id,blueprint_id,key,value) VALUES(?1,?2,?3,?4)",
            params![server_id, blueprint_id, key, value],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// ---------------- Subusers ----------------

pub fn list_subusers(db: &Db, server_id: i64) -> Result<Vec<(User, Grant)>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT u.id,u.username,u.email,u.avatar,u.language,u.theme,u.root_admin,u.active,u.twofa_secret,u.about,u.created_at,u.updated_at,s.permissions,s.role FROM subusers s JOIN users u ON u.id=s.user_id WHERE s.server_id=?1",
    )?;
    let rows = stmt.query_map([server_id], |r| {
        let perms: String = r.get(12)?;
        let role: String = r.get(13)?;
        Ok((user_from_row(r)?, Grant::from_stored(&role, &perms)))
    })?;
    let mut out = Vec::new();
    for v in rows {
        out.push(v?);
    }
    Ok(out)
}

pub fn add_subuser(db: &Db, server_id: i64, user_id: i64, grant: &Grant) -> Result<()> {
    // Plain INSERT with explicit pre-checks, all inside one IMMEDIATE
    // transaction: INSERT OR REPLACE silently shrunk an existing member's
    // grant (and churned the row id), and FK violations surfaced as 500s.
    // Re-adding an existing member is now a loud error the API maps to 409;
    // a missing user is a loud error the API maps to 404.
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let user_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1)",
        [user_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !user_exists {
        bail!("user not found");
    }
    let existing: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM subusers WHERE server_id=?1 AND user_id=?2)",
        params![server_id, user_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if existing {
        bail!("user is already a member of this server");
    }
    tx.execute(
        "INSERT INTO subusers(server_id,user_id,permissions,role) VALUES(?1,?2,?3,?4)",
        params![server_id, user_id, grant.to_json(), grant.role.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn remove_subuser(db: &Db, server_id: i64, user_id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "DELETE FROM subusers WHERE server_id=?1 AND user_id=?2",
        params![server_id, user_id],
    )?;
    Ok(())
}

pub fn user_has_server_access(db: &Db, user: &User, server_id: i64) -> Result<bool> {
    // A scoped API key reaches only servers named in its server_ids. Check
    // this before the ownership checks: the key must not grant access to
    // servers its owner could otherwise see (including root admins).
    if let Some(scope) = user.key_scope.as_ref() {
        if !scope.server_ids.is_empty() && !scope.server_ids.contains(&server_id) {
            return Ok(false);
        }
    }
    if user.root_admin {
        return Ok(true);
    }
    let conn = db.get()?;
    let mine: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE id=?1 AND user_id=?2 AND deleted=0",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    if mine > 0 {
        return Ok(true);
    }
    let sub: i64 = conn.query_row(
        "SELECT COUNT(*) FROM subusers WHERE server_id=?1 AND user_id=?2",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    if sub > 0 {
        return Ok(true);
    }
    ensure_squads_tables(&conn)?;
    let squad: i64 = conn.query_row(
        "SELECT COUNT(*) FROM squad_members sm JOIN squad_servers ss ON ss.squad_id=sm.squad_id \
         WHERE ss.server_id=?1 AND sm.user_id=?2",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    Ok(squad > 0)
}

/// Effective grant a user holds on a server, or `None` when they have no access.
///
/// When the request authenticated with a scoped API key, the underlying grant is
/// intersected with the key's scope: a key can only ever narrow its owner's rights,
/// never widen them — including for root admins.
pub fn server_grant(db: &Db, user: &User, server_id: i64) -> Result<Option<Grant>> {
    let grant = raw_server_grant(db, user, server_id)?;
    let Some(scope) = user.key_scope.as_ref() else {
        return Ok(grant);
    };
    Ok(grant.map(|g| {
        Grant::custom(
            g.capabilities()
                .filter(|cap| scope.allows(server_id, *cap))
                .collect::<Vec<_>>(),
        )
    }))
}

fn raw_server_grant(db: &Db, user: &User, server_id: i64) -> Result<Option<Grant>> {
    if user.root_admin {
        return Ok(Some(Grant::owner()));
    }
    let conn = db.get()?;
    let owner: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE id=?1 AND user_id=?2 AND deleted=0",
        params![server_id, user.id],
        |r| r.get(0),
    )?;
    if owner > 0 {
        return Ok(Some(Grant::owner()));
    }
    // Ownership outranks everything; between a subuser grant and a squad
    // grant the most permissive one wins — and incomparable sets fold to
    // their union so neither source can deny what the other allows.
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT role,permissions FROM subusers WHERE server_id=?1 AND user_id=?2",
            params![server_id, user.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let subuser = row.map(|(role, perms)| Grant::from_stored(&role, &perms));
    let squad = squad_grant(&conn, user.id, server_id)?;
    Ok(match (subuser, squad) {
        (Some(a), Some(b)) => Some(most_permissive(a, b)),
        (a, b) => a.or(b),
    })
}
pub fn user_has_capability(
    db: &Db,
    user: &User,
    server_id: i64,
    capability: Capability,
) -> Result<bool> {
    Ok(server_grant(db, user, server_id)?
        .map(|g| g.contains(capability))
        .unwrap_or(false))
}

// ---------------- Squads ----------------

/// Org-level workspace group. A squad resolves to ONE capability grant for
/// every server in the group: a member's role preset on the squad applies to
/// all grouped servers at once, instead of per-server subuser rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Squad {
    pub id: i64,
    pub name: String,
    pub created_by: i64,
    pub created_at: String,
}

/// A user's role inside one squad.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SquadMember {
    pub user_id: i64,
    pub username: String,
    pub role: Role,
}

/// Read-only membership view attached to user detail responses (`squads`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SquadMembership {
    pub id: i64,
    pub name: String,
    pub role: Role,
}

/// Ensure the squads tables exist. The DDL lives in the db.rs migration
/// ladder (v18 step); this is a memoized no-op fallback for pre-v18
/// databases that predate the fold-in. Guarded CREATE IF NOT EXISTS, so
/// calling it before every use is safe and idempotent — the same lazy
/// pattern `nodes::ensure_reservations_table` uses. Memoized per database
/// file: the DDL runs once per process per database instead of on every
/// access/grant check — `user_has_server_access` and `squad_grant` sit on
/// the per-request path. The key is the main database file (PRAGMA
/// database_list), because tests open a fresh DB per test and a single
/// process-global flag would leak the first database's state into every
/// later one. On a SQLITE error (e.g. the tables vanished after an external
/// change) the memoized state is reset and the DDL retried once, so a stale
/// failure is never cached.
const SQUADS_DDL: &str = "CREATE TABLE IF NOT EXISTS squads (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL,
        created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        created_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS squad_members (
        squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        PRIMARY KEY (squad_id, user_id)
    );
    CREATE TABLE IF NOT EXISTS squad_servers (
        squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
        server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        PRIMARY KEY (squad_id, server_id)
    );";

static SQUADS_TABLES_READY: std::sync::LazyLock<parking_lot::Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(HashSet::new()));

fn ensure_squads_tables(conn: &rusqlite::Connection) -> Result<()> {
    // Database identity: the main DB file. First row of PRAGMA database_list
    // is always `main`; column 2 is its file. (No in-memory DBs exist in this
    // codebase — an in-memory DB reports an empty file and would collapse
    // every such DB onto one key.)
    let file: String = conn.query_row("PRAGMA database_list", [], |r| r.get(2))?;
    let mut ready = SQUADS_TABLES_READY.lock();
    if ready.contains(&file) {
        return Ok(());
    }
    match conn.execute_batch(SQUADS_DDL) {
        Ok(()) => {
            ready.insert(file);
            Ok(())
        }
        Err(first) => {
            // Entry stays out of the set (never cache a failure); retry once
            // before surfacing — a transient lock or a table set replaced
            // mid-flight must not wedge the guard.
            match conn.execute_batch(SQUADS_DDL) {
                Ok(()) => {
                    ready.insert(file);
                    Ok(())
                }
                Err(_) => Err(first.into()),
            }
        }
    }
}

/// The more permissive of two grants: a superset wins outright; incomparable
/// sets (custom grants) fold to their union so nothing either grant allowed
/// is ever denied.
fn most_permissive(a: Grant, b: Grant) -> Grant {
    let ac: BTreeSet<_> = a.capabilities().collect();
    let bc: BTreeSet<_> = b.capabilities().collect();
    if ac.is_superset(&bc) {
        a
    } else if bc.is_superset(&ac) {
        b
    } else {
        Grant::custom(ac.union(&bc).copied())
    }
}

pub fn create_squad(db: &Db, name: &str, created_by: i64) -> Result<i64> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    conn.execute(
        "INSERT INTO squads(name,created_by,created_at) VALUES(?1,?2,?3)",
        params![name, created_by, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_squad(db: &Db, id: i64) -> Result<Squad> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    conn.query_row(
        "SELECT id,name,created_by,created_at FROM squads WHERE id=?1",
        [id],
        |r| {
            Ok(Squad {
                id: r.get(0)?,
                name: r.get(1)?,
                created_by: r.get(2)?,
                created_at: r.get(3)?,
            })
        },
    )
    .context("squad not found")
}

/// Every squad plus live member/server counts (servers counted live-only).
pub fn list_squads_with_counts(db: &Db) -> Result<Vec<(Squad, i64, i64)>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT sq.id,sq.name,sq.created_by,sq.created_at,
                (SELECT COUNT(*) FROM squad_members m WHERE m.squad_id=sq.id),
                (SELECT COUNT(*) FROM squad_servers ss JOIN servers s ON s.id=ss.server_id
                   WHERE ss.squad_id=sq.id AND s.deleted=0)
         FROM squads sq ORDER BY sq.name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            Squad {
                id: r.get(0)?,
                name: r.get(1)?,
                created_by: r.get(2)?,
                created_at: r.get(3)?,
            },
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for v in rows {
        out.push(v?);
    }
    Ok(out)
}

pub fn rename_squad(db: &Db, id: i64, name: &str) -> Result<()> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let n = conn.execute("UPDATE squads SET name=?1 WHERE id=?2", params![name, id])?;
    if n == 0 {
        bail!("squad not found");
    }
    Ok(())
}

/// Delete a squad; squad_members and squad_servers cascade with it.
pub fn delete_squad(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    conn.execute("DELETE FROM squads WHERE id=?1", [id])?;
    Ok(())
}

/// The actor's effective authority over a squad's membership: root admins and
/// the squad creator hold full (Manager) authority; anyone else is bounded by
/// the most permissive role they hold on the squad. Mirrors the subuser rule —
/// a grant is authority minted for another member, so it can never exceed the
/// grantor's own.
fn squad_authority(conn: &rusqlite::Connection, actor: &User, squad_id: i64) -> Result<Grant> {
    if actor.root_admin {
        return Ok(Grant::owner());
    }
    let created: i64 = conn.query_row(
        "SELECT COUNT(*) FROM squads WHERE id=?1 AND created_by=?2",
        params![squad_id, actor.id],
        |r| r.get(0),
    )?;
    if created > 0 {
        return Ok(Grant::owner());
    }
    let mut stmt = conn.prepare(
        "SELECT role FROM squad_members WHERE squad_id=?1 AND user_id=?2",
    )?;
    let roles: Vec<String> = stmt
        .query_map(params![squad_id, actor.id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut grant: Option<Grant> = None;
    for role in roles {
        let role = Role::from_str(&role).context("invalid squad role")?;
        let g = Grant::new(role, []);
        grant = Some(match grant {
            None => g,
            Some(cur) => most_permissive(cur, g),
        });
    }
    grant.ok_or_else(|| anyhow::anyhow!("no authority on this squad"))
}

/// True when `user_id` may manage squad `squad_id`: root admins, the squad
/// creator, and members holding the Manager preset (the top of the ladder)
/// all pass; lower presets and non-members return false. Squad member and
/// server-assignment endpoints gate on this, so a squad's own managers can
/// run them without root admin. The capability-subset check keeps `Custom`
/// (which implies nothing) from ever reading as manager authority.
pub fn squad_manager_authority(db: &Db, user_id: i64, squad_id: i64) -> Result<bool> {
    let user = get_user(db, user_id)?;
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    match squad_authority(&conn, &user, squad_id) {
        Ok(grant) => Ok(Role::Manager.capabilities().iter().all(|c| grant.contains(*c))),
        Err(e) if e.to_string() == "no authority on this squad" => Ok(false),
        Err(e) => Err(e),
    }
}

/// The caller's role on the squad, or None when they hold no membership.
/// Root admins and the squad creator report Manager even without a
/// membership row — they hold full authority over the squad, so `my_role`
/// should never understate what they can do.
pub fn squad_role_for_user(db: &Db, user_id: i64, squad_id: i64) -> Result<Option<Role>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let user = get_user(db, user_id)?;
    if user.root_admin {
        return Ok(Some(Role::Manager));
    }
    let created: i64 = conn.query_row(
        "SELECT COUNT(*) FROM squads WHERE id=?1 AND created_by=?2",
        params![squad_id, user_id],
        |r| r.get(0),
    )?;
    if created > 0 {
        return Ok(Some(Role::Manager));
    }
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM squad_members WHERE squad_id=?1 AND user_id=?2",
            params![squad_id, user_id],
            |r| r.get(0),
        )
        .optional()?;
    role.map(|r| Role::from_str(&r).context("invalid squad role"))
        .transpose()
}

/// The user id that created the squad, or None if it does not exist. Squad
/// deletion is gated on creator-or-root at the API layer.
pub fn squad_creator(db: &Db, squad_id: i64) -> Result<Option<i64>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    Ok(conn
        .query_row(
            "SELECT created_by FROM squads WHERE id=?1",
            [squad_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn squad_members(db: &Db, squad_id: i64) -> Result<Vec<SquadMember>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT sm.user_id,u.username,sm.role FROM squad_members sm JOIN users u ON u.id=sm.user_id \
         WHERE sm.squad_id=?1 ORDER BY u.username",
    )?;
    let rows = stmt.query_map([squad_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (user_id, username, role) = row?;
        out.push(SquadMember {
            user_id,
            username,
            role: Role::from_str(&role).context("invalid squad role")?,
        });
    }
    Ok(out)
}

pub fn add_squad_member(db: &Db, squad_id: i64, user_id: i64, role: Role, actor: &User) -> Result<()> {
    // Plain INSERT with explicit pre-checks inside one IMMEDIATE transaction,
    // mirroring add_subuser: FK violations and duplicates surface as loud
    // errors instead of 500s, and the anti-escalation check cannot race.
    let mut conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let squad_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM squads WHERE id=?1)",
        [squad_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !squad_exists {
        bail!("squad not found");
    }
    let user_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1)",
        [user_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !user_exists {
        bail!("user not found");
    }
    let existing: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM squad_members WHERE squad_id=?1 AND user_id=?2)",
        params![squad_id, user_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if existing {
        bail!("user is already a member of this squad");
    }
    let authority = squad_authority(&tx, actor, squad_id)?;
    let grant = Grant::new(role, []);
    if let Some(over) = grant.capabilities().find(|c| !authority.contains(*c)) {
        bail!("cannot grant a capability you do not hold: {}", over.as_str());
    }
    tx.execute(
        "INSERT INTO squad_members(squad_id,user_id,role) VALUES(?1,?2,?3)",
        params![squad_id, user_id, role.as_str()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn update_squad_member_role(
    db: &Db,
    squad_id: i64,
    user_id: i64,
    role: Role,
    actor: &User,
) -> Result<()> {
    let mut conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let member: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM squad_members WHERE squad_id=?1 AND user_id=?2)",
        params![squad_id, user_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !member {
        bail!("member not found");
    }
    let authority = squad_authority(&tx, actor, squad_id)?;
    let grant = Grant::new(role, []);
    if let Some(over) = grant.capabilities().find(|c| !authority.contains(*c)) {
        bail!("cannot grant a capability you do not hold: {}", over.as_str());
    }
    tx.execute(
        "UPDATE squad_members SET role=?1 WHERE squad_id=?2 AND user_id=?3",
        params![role.as_str(), squad_id, user_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn remove_squad_member(db: &Db, squad_id: i64, user_id: i64, actor: &User) -> Result<()> {
    // Authority check + target-role read + DELETE inside one IMMEDIATE
    // transaction (mirroring add/update_squad_member_role) so the
    // authority-vs-role comparison cannot race a concurrent membership change.
    let mut conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let authority = squad_authority(&tx, actor, squad_id)?;
    let target: Option<String> = tx
        .query_row(
            "SELECT role FROM squad_members WHERE squad_id=?1 AND user_id=?2",
            params![squad_id, user_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(target) = target else {
        bail!("member not found");
    };
    // A member may only be removed when the actor could have minted the
    // member's role itself (subuser remove mirror).
    let target_grant = Grant::new(Role::from_str(&target).context("invalid squad role")?, []);
    if let Some(over) = target_grant.capabilities().find(|c| !authority.contains(*c)) {
        bail!(
            "cannot remove a member with capabilities you do not hold: {}",
            over.as_str()
        );
    }
    tx.execute(
        "DELETE FROM squad_members WHERE squad_id=?1 AND user_id=?2",
        params![squad_id, user_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn squad_servers(db: &Db, squad_id: i64) -> Result<Vec<Server>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SERVER_COLS} FROM servers JOIN squad_servers ss ON ss.server_id=servers.id \
         WHERE ss.squad_id=?1 AND servers.deleted=0 ORDER BY servers.name"
    ))?;
    let rows = stmt.query_map([squad_id], server_from_row)?;
    let mut out = Vec::new();
    for s in rows {
        out.push(s?);
    }
    Ok(out)
}

pub fn add_squad_server(db: &Db, squad_id: i64, server_id: i64) -> Result<()> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let squad: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM squads WHERE id=?1)",
        [squad_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !squad {
        bail!("squad not found");
    }
    let server: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM servers WHERE id=?1 AND deleted=0)",
        [server_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !server {
        bail!("server not found");
    }
    let existing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM squad_servers WHERE squad_id=?1 AND server_id=?2)",
        params![squad_id, server_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if existing {
        bail!("server is already in this squad");
    }
    conn.execute(
        "INSERT INTO squad_servers(squad_id,server_id) VALUES(?1,?2)",
        params![squad_id, server_id],
    )?;
    Ok(())
}

pub fn remove_squad_server(db: &Db, squad_id: i64, server_id: i64) -> Result<()> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    conn.execute(
        "DELETE FROM squad_servers WHERE squad_id=?1 AND server_id=?2",
        params![squad_id, server_id],
    )?;
    Ok(())
}

/// Replace the squad's server set in one transaction: validate every id first
/// so a partial apply can never leave the set half-written.
pub fn set_squad_servers(db: &Db, squad_id: i64, server_ids: &[i64]) -> Result<()> {
    let mut conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let squad: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM squads WHERE id=?1)",
        [squad_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    if !squad {
        bail!("squad not found");
    }
    for id in server_ids {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM servers WHERE id=?1 AND deleted=0)",
            [id],
            |r| r.get::<_, i64>(0),
        )? != 0;
        if !exists {
            bail!("server not found");
        }
    }
    tx.execute("DELETE FROM squad_servers WHERE squad_id=?1", [squad_id])?;
    for id in server_ids {
        tx.execute(
            "INSERT INTO squad_servers(squad_id,server_id) VALUES(?1,?2)",
            params![squad_id, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Most permissive grant from the squads covering `server_id` for `user_id`.
fn squad_grant(conn: &rusqlite::Connection, user_id: i64, server_id: i64) -> Result<Option<Grant>> {
    ensure_squads_tables(conn)?;
    let mut stmt = conn.prepare(
        "SELECT sm.role FROM squad_members sm JOIN squad_servers ss ON ss.squad_id=sm.squad_id \
         WHERE sm.user_id=?1 AND ss.server_id=?2",
    )?;
    let roles: Vec<String> = stmt
        .query_map(params![user_id, server_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut grant: Option<Grant> = None;
    for role in roles {
        let role = Role::from_str(&role).context("invalid squad role")?;
        let g = Grant::new(role, []);
        grant = Some(match grant {
            None => g,
            Some(cur) => most_permissive(cur, g),
        });
    }
    Ok(grant)
}

pub fn squad_memberships_for(db: &Db, user_id: i64) -> Result<Vec<SquadMembership>> {
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let mut stmt = conn.prepare(
        "SELECT sq.id,sq.name,sm.role FROM squad_members sm JOIN squads sq ON sq.id=sm.squad_id \
         WHERE sm.user_id=?1 ORDER BY sq.name",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, role) = row?;
        out.push(SquadMembership {
            id,
            name,
            role: Role::from_str(&role).context("invalid squad role")?,
        });
    }
    Ok(out)
}

/// Batch variant for list responses: one query covers every requested user.
pub fn squad_memberships_for_users(
    db: &Db,
    user_ids: &[i64],
) -> Result<HashMap<i64, Vec<SquadMembership>>> {
    let mut out: HashMap<i64, Vec<SquadMembership>> =
        user_ids.iter().map(|&id| (id, Vec::new())).collect();
    if user_ids.is_empty() {
        return Ok(out);
    }
    let conn = db.get()?;
    ensure_squads_tables(&conn)?;
    let placeholders = vec!["?"; user_ids.len()].join(",");
    let sql = format!(
        "SELECT sm.user_id,sq.id,sq.name,sm.role FROM squad_members sm JOIN squads sq ON sq.id=sm.squad_id \
         WHERE sm.user_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(user_ids), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (uid, id, name, role) = row?;
        out.entry(uid).or_default().push(SquadMembership {
            id,
            name,
            role: Role::from_str(&role).context("invalid squad role")?,
        });
    }
    Ok(out)
}

// ---------------- Backup ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub id: i64,
    pub uuid: String,
    pub server_id: i64,
    pub name: String,
    /// Absolute filesystem path; never serialized — the download route resolves
    /// it server-side, so a list response cannot leak the host layout.
    #[serde(skip_serializing)]
    pub path: String,
    pub size_bytes: i64,
    pub checksum: String,
    pub format: String,
    pub created_at: String,
    /// Locked backups are exempt from rotation and refuse deletion until
    /// explicitly unlocked.
    pub is_locked: bool,
    /// Newline-separated glob patterns excluded when the archive was built.
    pub ignored_files: String,
}

const BACKUP_COLUMNS: &str =
    "id,uuid,server_id,name,path,size_bytes,checksum,format,created_at,is_locked,ignored_files";

fn backup_from_row(r: &Row) -> rusqlite::Result<Backup> {
    Ok(Backup {
        id: r.get(0)?,
        uuid: r.get(1)?,
        server_id: r.get(2)?,
        name: r.get(3)?,
        path: r.get(4)?,
        size_bytes: r.get(5)?,
        checksum: r.get(6)?,
        format: r.get(7)?,
        created_at: r.get(8)?,
        is_locked: r.get::<_, i64>(9)? != 0,
        ignored_files: r.get(10)?,
    })
}

pub fn list_backups(db: &Db, server_id: i64) -> Result<Vec<Backup>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {BACKUP_COLUMNS} FROM backups WHERE server_id=?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map([server_id], backup_from_row)?;
    let mut out = Vec::new();
    for b in rows {
        out.push(b?);
    }
    Ok(out)
}

pub fn get_backup(db: &Db, id: i64) -> Result<Backup> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {BACKUP_COLUMNS} FROM backups WHERE id=?1"),
        [id],
        backup_from_row,
    )
    .context("backup not found")
}

pub fn create_backup(
    db: &Db,
    uuid: &str,
    server_id: i64,
    name: &str,
    path: &str,
    size_bytes: i64,
    checksum: &str,
    format: &str,
    ignored_files: &str,
) -> Result<i64> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO backups(uuid,server_id,name,path,size_bytes,checksum,format,created_at,ignored_files) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            uuid,
            server_id,
            name,
            path,
            size_bytes,
            checksum,
            format,
            now(),
            ignored_files
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Lock or unlock a backup. Locked backups survive rotation and refuse
/// deletion, so this is the only way to make one deletable again.
pub fn set_backup_locked(db: &Db, id: i64, locked: bool) -> Result<()> {
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE backups SET is_locked=?1 WHERE id=?2",
        params![i64::from(locked), id],
    )?;
    if changed == 0 {
        bail!("backup not found");
    }
    Ok(())
}

/// Delete a backup row, refusing while it is locked. BEGIN IMMEDIATE keeps
/// the lock-check-and-delete atomic across pooled connections: a concurrent
/// locker can no longer slip between the SELECT and the DELETE.
pub fn delete_backup(db: &Db, id: i64) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let locked: bool = tx
        .query_row("SELECT is_locked FROM backups WHERE id=?1", [id], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .map(|v| v != 0)
        .context("backup not found")?;
    if locked {
        bail!("backup is locked; unlock it before deleting");
    }
    tx.execute("DELETE FROM backups WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

/// Oldest-first unlocked backups beyond `keep`, the rotation candidates.
pub fn rotation_candidates(db: &Db, server_id: i64, keep: usize) -> Result<Vec<Backup>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {BACKUP_COLUMNS} FROM backups WHERE server_id=?1 AND is_locked=0 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map([server_id], backup_from_row)?;
    let mut all = Vec::new();
    for b in rows {
        all.push(b?);
    }
    if all.len() <= keep {
        return Ok(Vec::new());
    }
    let mut out = all.split_off(keep);
    out.reverse();
    Ok(out)
}

// ---------------- Database ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub db_type: String,
    pub host: String,
    pub port: i64,
    pub db_name: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password: String,
    pub max_conns: i64,
    pub created_at: String,
}

fn database_from_row(r: &Row) -> rusqlite::Result<Database> {
    Ok(Database {
        id: r.get(0)?,
        server_id: r.get(1)?,
        name: r.get(2)?,
        db_type: r.get(3)?,
        host: r.get(4)?,
        port: r.get(5)?,
        db_name: r.get(6)?,
        username: r.get(7)?,
        password: r.get(8)?,
        max_conns: r.get(9)?,
        created_at: r.get(10)?,
    })
}

pub fn list_databases(db: &Db, server_id: i64) -> Result<Vec<Database>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare("SELECT id,server_id,name,db_type,host,port,db_name,username,password,max_conns,created_at FROM databases WHERE server_id=?1 ORDER BY name")?;
    let rows = stmt.query_map([server_id], database_from_row)?;
    let mut out = Vec::new();
    for d in rows {
        out.push(d?);
    }
    Ok(out)
}

pub fn get_database(db: &Db, id: i64) -> Result<Database> {
    let conn = db.get()?;
    Ok(conn.query_row(
        "SELECT id,server_id,name,db_type,host,port,db_name,username,password,max_conns,created_at FROM databases WHERE id=?1",
        [id],
        database_from_row,
    )?)
}


// ---------------- Schedule ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: i64,
    pub schedule_id: i64,
    pub action: String,
    pub payload: String,
    pub sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: i64,
    pub server_id: i64,
    pub name: String,
    pub cron_expr: String,
    pub enabled: bool,
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub tasks: Vec<ScheduleTask>,
}

fn schedule_from_row(r: &Row) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: r.get(0)?,
        server_id: r.get(1)?,
        name: r.get(2)?,
        cron_expr: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        last_run_at: r.get(5)?,
        next_run_at: r.get(6)?,
        created_at: r.get(7)?,
        tasks: vec![],
    })
}

pub fn list_schedules(db: &Db, server_id: i64) -> Result<Vec<Schedule>> {
    // One JOIN query with client-side grouping instead of a per-schedule task
    // lookup (N+1): schedules ORDER BY id, tasks ORDER BY sequence.
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT s.id,s.server_id,s.name,s.cron_expr,s.enabled,s.last_run_at,s.next_run_at,s.created_at, \
                t.id,t.schedule_id,t.action,t.payload,t.sequence \
         FROM schedules s LEFT JOIN schedule_tasks t ON t.schedule_id=s.id \
         WHERE s.server_id=?1 ORDER BY s.id, t.sequence, t.id",
    )?;
    let rows = stmt.query_map([server_id], |r| {
        let schedule = Schedule {
            id: r.get(0)?,
            server_id: r.get(1)?,
            name: r.get(2)?,
            cron_expr: r.get(3)?,
            enabled: r.get::<_, i64>(4)? != 0,
            last_run_at: r.get(5)?,
            next_run_at: r.get(6)?,
            created_at: r.get(7)?,
            tasks: vec![],
        };
        // NULL task columns = schedule with no tasks (LEFT JOIN miss).
        let task: Option<ScheduleTask> = match r.get::<_, Option<i64>>(8)? {
            Some(id) => Some(ScheduleTask {
                id,
                schedule_id: r.get(9)?,
                action: r.get(10)?,
                payload: r.get(11)?,
                sequence: r.get(12)?,
            }),
            None => None,
        };
        Ok((schedule, task))
    })?;
    let mut out: Vec<Schedule> = Vec::new();
    for row in rows {
        let (s, task) = row?;
        if let Some(last) = out.last_mut() {
            if last.id == s.id {
                if let Some(t) = task {
                    last.tasks.push(t);
                }
                continue;
            }
        }
        let mut s = s;
        if let Some(t) = task {
            s.tasks.push(t);
        }
        out.push(s);
    }
    Ok(out)
}

pub fn get_schedule(db: &Db, id: i64) -> Result<Schedule> {
    let conn = db.get()?;
    let mut s = conn
        .query_row(
            "SELECT id,server_id,name,cron_expr,enabled,last_run_at,next_run_at,created_at FROM schedules WHERE id=?1",
            [id],
            schedule_from_row,
        )
        .context("schedule not found")?;
    s.tasks = list_schedule_tasks_conn(&conn, s.id)?;
    Ok(s)
}

fn list_schedule_tasks_conn(
    conn: &rusqlite::Connection,
    schedule_id: i64,
) -> Result<Vec<ScheduleTask>> {
    let mut stmt = conn.prepare("SELECT id,schedule_id,action,payload,sequence FROM schedule_tasks WHERE schedule_id=?1 ORDER BY sequence, id")?;
    let rows = stmt.query_map([schedule_id], |r| {
        Ok(ScheduleTask {
            id: r.get(0)?,
            schedule_id: r.get(1)?,
            action: r.get(2)?,
            payload: r.get(3)?,
            sequence: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for t in rows {
        out.push(t?);
    }
    Ok(out)
}

/// Create a schedule and all of its tasks in one transaction so a
/// mid-operation failure can never leave a partial schedule/task set behind.
/// Tasks are `(action, payload, sequence)` triples.
pub fn create_schedule_with_tasks(
    db: &Db,
    server_id: i64,
    name: &str,
    cron_expr: &str,
    enabled: bool,
    max_retries: i64,
    retry_backoff_s: i64,
    only_when_online: bool,
    tasks: &[(String, String, i64)],
) -> Result<i64> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at,max_retries,retry_backoff_s,only_when_online) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![server_id, name, cron_expr, enabled as i64, now(), max_retries, retry_backoff_s, only_when_online as i64],
    )?;
    let id = tx.last_insert_rowid();
    for (action, payload, sequence) in tasks {
        tx.execute(
            "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence) VALUES(?1,?2,?3,?4)",
            params![id, action, payload, sequence],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

pub fn update_schedule(db: &Db, s: &Schedule) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE schedules SET name=?1, cron_expr=?2, enabled=?3 WHERE id=?4",
        params![s.name, s.cron_expr, s.enabled as i64, s.id],
    )?;
    Ok(())
}

pub fn set_schedule_next(db: &Db, id: i64, next: Option<&str>) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE schedules SET next_run_at=?1 WHERE id=?2",
        params![next, id],
    )?;
    Ok(())
}

pub fn set_schedule_last(db: &Db, id: i64, last: Option<&str>) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE schedules SET last_run_at=?1 WHERE id=?2",
        params![last, id],
    )?;
    Ok(())
}

pub fn delete_schedule(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM schedules WHERE id=?1", [id])?;
    Ok(())
}

pub fn add_schedule_task(
    db: &Db,
    schedule_id: i64,
    action: &str,
    payload: &str,
    sequence: i64,
) -> Result<i64> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence) VALUES(?1,?2,?3,?4)",
        params![schedule_id, action, payload, sequence],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete_schedule_task(db: &Db, schedule_id: i64, id: i64) -> Result<bool> {
    let conn = db.get()?;
    let n = conn.execute(
        "DELETE FROM schedule_tasks WHERE id=?1 AND schedule_id=?2",
        params![id, schedule_id],
    )?;
    Ok(n > 0)
}



pub fn touch_api_key(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE api_keys SET last_used=?1 WHERE id=?2",
        params![now(), id],
    )?;
    Ok(())
}


// ---------------- Website ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Website {
    pub id: i64,
    pub server_id: i64,
    pub domain: String,
    pub root_dir: String,
    pub port: Option<i64>,
    pub proxy_type: String,
    pub ssl: bool,
    pub enabled: bool,
    pub created_at: String,
}

fn website_from_row(r: &Row) -> rusqlite::Result<Website> {
    Ok(Website {
        id: r.get(0)?,
        server_id: r.get(1)?,
        domain: r.get(2)?,
        root_dir: r.get(3)?,
        port: r.get(4)?,
        proxy_type: r.get(5)?,
        ssl: r.get::<_, i64>(6)? != 0,
        enabled: r.get::<_, i64>(7)? != 0,
        created_at: r.get(8)?,
    })
}

pub fn list_websites(db: &Db, server_id: i64) -> Result<Vec<Website>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare("SELECT id,server_id,domain,root_dir,port,proxy_type,ssl,enabled,created_at FROM websites WHERE server_id=?1 ORDER BY id")?;
    let rows = stmt.query_map([server_id], website_from_row)?;
    let mut out = Vec::new();
    for w in rows {
        out.push(w?);
    }
    Ok(out)
}

pub fn create_website(
    db: &Db,
    server_id: i64,
    domain: &str,
    root_dir: &str,
    proxy_type: &str,
    ssl: bool,
) -> Result<i64> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO websites(server_id,domain,root_dir,proxy_type,ssl,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![server_id, domain, root_dir, proxy_type, ssl as i64, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_website(db: &Db, w: &Website) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "UPDATE websites SET domain=?1, root_dir=?2, port=?3, proxy_type=?4, ssl=?5, enabled=?6 WHERE id=?7",
        params![w.domain, w.root_dir, w.port, w.proxy_type, w.ssl as i64, w.enabled as i64, w.id],
    )?;
    Ok(())
}

pub fn delete_website(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM websites WHERE id=?1", [id])?;
    Ok(())
}

// ---------------- Audit log ----------------

static AUDIT_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_audit_enabled(enabled: bool) {
    AUDIT_ENABLED.store(enabled, Ordering::Relaxed);
}

static AUDIT_RETENTION_DAYS: AtomicU64 = AtomicU64::new(90);

/// Set the audit-log retention window from config at boot.
pub fn set_audit_retention_days(days: u64) {
    AUDIT_RETENTION_DAYS.store(days, Ordering::Relaxed);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLog {
    pub id: i64,
    pub user_id: Option<i64>,
    /// Actor name resolved at read time; `None` once the user row is gone.
    pub username: Option<String>,
    pub action: String,
    pub target: String,
    pub ip: String,
    pub details: String,
    pub created_at: String,
    /// Set when the entry belongs to one workspace, so a server-scoped feed
    /// filters on an index instead of parsing `target`.
    pub server_id: Option<i64>,
}

const AUDIT_SELECT: &str = "SELECT a.id,a.user_id,u.username,a.action,a.target,a.ip,a.details,a.created_at,a.server_id FROM audit_logs a LEFT JOIN users u ON u.id = a.user_id";

fn audit_from_row(r: &Row) -> rusqlite::Result<AuditLog> {
    Ok(AuditLog {
        id: r.get(0)?,
        user_id: r.get(1)?,
        username: r.get(2)?,
        action: r.get(3)?,
        target: r.get(4)?,
        ip: r.get(5)?,
        details: r.get(6)?,
        created_at: r.get(7)?,
        server_id: r.get(8)?,
    })
}

pub fn list_audit_logs(db: &Db, limit: i64) -> Result<Vec<AuditLog>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!("{AUDIT_SELECT} ORDER BY a.id DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit], audit_from_row)?;
    let mut out = Vec::new();
    for a in rows {
        out.push(a?);
    }
    Ok(out)
}

/// Activity feed for one actor, newest first.
pub fn list_user_activity(db: &Db, user_id: i64, limit: i64) -> Result<Vec<AuditLog>> {
    let conn = db.get()?;
    let mut stmt =
        conn.prepare(&format!("{AUDIT_SELECT} WHERE a.user_id=?1 ORDER BY a.id DESC LIMIT ?2"))?;
    let rows = stmt.query_map(params![user_id, limit], audit_from_row)?;
    let mut out = Vec::new();
    for a in rows {
        out.push(a?);
    }
    Ok(out)
}

/// Activity feed for one workspace, newest first.
pub fn list_server_activity(db: &Db, server_id: i64, limit: i64) -> Result<Vec<AuditLog>> {
    let conn = db.get()?;
    let mut stmt =
        conn.prepare(&format!("{AUDIT_SELECT} WHERE a.server_id=?1 ORDER BY a.id DESC LIMIT ?2"))?;
    let rows = stmt.query_map(params![server_id, limit], audit_from_row)?;
    let mut out = Vec::new();
    for a in rows {
        out.push(a?);
    }
    Ok(out)
}

pub fn audit(
    db: &Db,
    user_id: Option<i64>,
    action: &str,
    target: &str,
    ip: &str,
    details: &str,
) -> Result<()> {
    audit_scoped(db, user_id, action, target, ip, details, None)
}

/// Record an entry tied to one workspace so the server activity feed can find
/// it by index rather than by parsing `target`.
pub fn audit_scoped(
    db: &Db,
    user_id: Option<i64>,
    action: &str,
    target: &str,
    ip: &str,
    details: &str,
    server_id: Option<i64>,
) -> Result<()> {
    if !AUDIT_ENABLED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO audit_logs(user_id,action,target,ip,details,created_at,server_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![user_id, action, target, ip, details, now(), server_id],
    )?;
    // Probabilistic prune: ~1/1000 inserts pays for the scan, so steady-state
    // growth stays bounded without a background job. Best-effort — a failed
    // prune must never fail the insert.
    if rand::random::<u32>().is_multiple_of(1000) {
        let _ = prune_audit_logs(db, AUDIT_RETENTION_DAYS.load(Ordering::Relaxed));
    }
    Ok(())
}

/// Delete audit rows older than `retention_days`, always keeping the newest
/// ~500 entries so a freshly-lowered retention cannot wipe recent history.
/// `created_at` is RFC3339 from [`now`], so a lexicographic compare is a
/// chronological one. Returns the number of rows removed.
pub fn prune_audit_logs(db: &Db, retention_days: u64) -> Result<usize> {
    let conn = db.get()?;
    let cutoff = (Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
    let n = conn.execute(
        "DELETE FROM audit_logs WHERE created_at < ?1 AND id NOT IN \
         (SELECT id FROM audit_logs ORDER BY id DESC LIMIT 500)",
        [cutoff],
    )?;
    Ok(n)
}

// ---------------- Settings ----------------

pub fn get_setting(db: &Db, key: &str) -> Result<Option<String>> {
    let conn = db.get()?;
    let v = conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    Ok(v)
}

pub fn set_setting(db: &Db, key: &str, value: &str) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=?2",
        params![key, value],
    )?;
    Ok(())
}

pub fn all_settings(db: &Db) -> Result<Vec<(String, String)>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare("SELECT key,value FROM settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for s in rows {
        out.push(s?);
    }
    Ok(out)
}

// ---------------- Allocations ----------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation {
    pub id: i64,
    pub server_id: i64,
    pub port: i64,
    pub node: String,
    pub assigned_at: String,
    pub notes: String,
    /// Exactly one allocation per server is primary; it mirrors `servers.port`
    /// and is the endpoint handed to the workload as its main port.
    pub is_primary: bool,
}

const ALLOCATION_COLUMNS: &str = "id,server_id,port,node,assigned_at,notes,is_primary";

fn allocation_from_row(r: &Row) -> rusqlite::Result<Allocation> {
    Ok(Allocation {
        id: r.get(0)?,
        server_id: r.get(1)?,
        port: r.get(2)?,
        node: r.get(3)?,
        assigned_at: r.get(4)?,
        notes: r.get(5)?,
        is_primary: r.get::<_, i64>(6)? != 0,
    })
}

pub fn list_allocations(db: &Db, server_id: i64) -> Result<Vec<Allocation>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ALLOCATION_COLUMNS} FROM allocations WHERE server_id=?1 ORDER BY is_primary DESC, port"
    ))?;
    let rows = stmt.query_map([server_id], allocation_from_row)?;
    let mut out = Vec::new();
    for a in rows {
        out.push(a?);
    }
    Ok(out)
}

pub fn get_allocation(db: &Db, id: i64) -> Result<Allocation> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {ALLOCATION_COLUMNS} FROM allocations WHERE id=?1"),
        [id],
        allocation_from_row,
    )
    .context("allocation not found")
}

/// Ports a workload can actually bind. Workloads are launched via `setpriv`
/// with `--bounding-set=-all --ambient-caps=-all` (see `isolation.rs`), so they
/// never hold `CAP_NET_BIND_SERVICE`; a privileged port would be accepted here
/// and then fail at bind time inside the sandbox. Reject it at the boundary.
pub const MIN_WORKLOAD_PORT: i64 = 1024;
pub const MAX_WORKLOAD_PORT: i64 = 65_535;

/// Validate an operator-supplied port before it reaches the allocations table.
pub fn validate_port(port: i64) -> Result<()> {
    if !(MIN_WORKLOAD_PORT..=MAX_WORKLOAD_PORT).contains(&port) {
        bail!("port must be between {MIN_WORKLOAD_PORT} and {MAX_WORKLOAD_PORT}");
    }
    Ok(())
}

/// Attach `port` to a workspace. The first allocation a server receives becomes
/// its primary, which keeps `servers.port` in step without a second call.
pub fn allocate_port(db: &Db, server_id: i64, port: i64) -> Result<()> {
    add_allocation(db, server_id, port, "").map(|_| ())
}

/// Attach `port` with operator notes, returning the allocation id.
pub fn add_allocation(db: &Db, server_id: i64, port: i64, notes: &str) -> Result<i64> {
    validate_port(port)?;
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (node, deleted): (String, bool) = tx.query_row(
        "SELECT node, deleted FROM servers WHERE id=?1",
        [server_id],
        |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0)),
    )?;
    if deleted {
        bail!("cannot allocate a port to a deleted workspace");
    }
    let existing = tx
        .query_row(
            "SELECT id, server_id FROM allocations WHERE node=?1 AND port=?2",
            params![node, port],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    let has_primary: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM allocations WHERE server_id=?1 AND is_primary=1)",
        [server_id],
        |r| r.get::<_, i64>(0),
    )? != 0;
    let id = match existing {
        Some((_, owner)) if owner != server_id => {
            bail!("port {port} is already allocated on {node}")
        }
        Some((id, _)) => {
            if !notes.is_empty() {
                tx.execute("UPDATE allocations SET notes=?1 WHERE id=?2", params![notes, id])?;
            }
            id
        }
        None => {
            tx.execute(
                "INSERT INTO allocations(server_id,port,assigned_at,node,notes,is_primary) VALUES(?1,?2,?3,?4,?5,?6)",
                params![server_id, port, now(), node, notes, i64::from(!has_primary)],
            )?;
            tx.last_insert_rowid()
        }
    };
    // The primary allocation and `servers.port` are one fact stored twice; a
    // newly elected primary must update both inside this transaction.
    if !has_primary {
        tx.execute(
            "UPDATE servers SET port=?1,updated_at=?2 WHERE id=?3",
            params![port, now(), server_id],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

/// Promote an allocation to primary, demoting the previous one and moving
/// `servers.port` with it.
pub fn set_primary_allocation(db: &Db, alloc_id: i64) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    let (server_id, port): (i64, i64) = tx
        .query_row(
            "SELECT server_id, port FROM allocations WHERE id=?1",
            [alloc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("allocation not found")?;
    tx.execute(
        "UPDATE allocations SET is_primary = (id = ?1) WHERE server_id=?2",
        params![alloc_id, server_id],
    )?;
    tx.execute(
        "UPDATE servers SET port=?1,updated_at=?2 WHERE id=?3",
        params![port, now(), server_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn set_allocation_notes(db: &Db, alloc_id: i64, notes: &str) -> Result<()> {
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE allocations SET notes=?1 WHERE id=?2",
        params![notes, alloc_id],
    )?;
    if changed == 0 {
        bail!("allocation not found");
    }
    Ok(())
}

/// Detach a single allocation. The primary is refused: a workload would lose
/// the port it is running on, so callers must promote another one first.
pub fn remove_allocation(db: &Db, alloc_id: i64) -> Result<()> {
    let conn = db.get()?;
    let is_primary: bool = conn
        .query_row(
            "SELECT is_primary FROM allocations WHERE id=?1",
            [alloc_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v != 0)
        .context("allocation not found")?;
    if is_primary {
        bail!("cannot remove the primary allocation; promote another port first");
    }
    conn.execute("DELETE FROM allocations WHERE id=?1", [alloc_id])?;
    Ok(())
}

pub fn free_ports(db: &Db, server_id: i64) -> Result<()> {
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM allocations WHERE server_id=?1", [server_id])?;
    tx.execute(
        "UPDATE servers SET port=NULL,updated_at=?1 WHERE id=?2",
        params![now(), server_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn ports_for_server(db: &Db, server_id: i64) -> Result<Vec<i64>> {
    let conn = db.get()?;
    let mut stmt = conn
        .prepare("SELECT port FROM allocations WHERE server_id=?1 ORDER BY is_primary DESC, port")?;
    let rows = stmt.query_map([server_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for p in rows {
        out.push(p?);
    }
    Ok(out)
}

// ---------------- Rate limits ----------------

pub fn bump_rate_limit(db: &Db, key: &str, window_start: i64) -> Result<i64> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO rate_limits(key,window_start,count) VALUES(?1,?2,1) ON CONFLICT(key) DO UPDATE SET count=CASE WHEN rate_limits.window_start=?2 THEN count+1 ELSE 1 END, window_start=?2",
        params![key, window_start],
    )?;
    let c: i64 = conn.query_row("SELECT count FROM rate_limits WHERE key=?1", [key], |r| {
        r.get(0)
    })?;
    Ok(c)
}

pub fn reset_rate_limits(db: &Db) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM rate_limits", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;
    use crate::services::keys::KeyScope;

    static DB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TestDb {
        db: Db,
        path: std::path::PathBuf,
        owner_id: i64,
        server_id: i64,
        other_server_id: i64,
    }

    impl TestDb {
        fn new() -> Self {
            let seq = DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voltpanel-models-test-{}-{}.db",
                std::process::id(),
                seq
            ));
            let _ = std::fs::remove_file(&path);
            let db = crate::db::open(path.to_str().unwrap()).unwrap();
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at)
                 VALUES('owner','owner@t','x','now','now')",
                [],
            )
            .unwrap();
            let owner_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO blueprints(uuid,name,created_at,updated_at)
                 VALUES('b','b','now','now')",
                [],
            )
            .unwrap();
            let bid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s1','s1',?1,?2,'now','now')",
                params![owner_id, bid],
            )
            .unwrap();
            let server_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s2','s2',?1,?2,'now','now')",
                params![owner_id, bid],
            )
            .unwrap();
            let other_server_id = conn.last_insert_rowid();
            drop(conn);
            TestDb {
                db,
                path,
                owner_id,
                server_id,
                other_server_id,
            }
        }

        fn owner(&self) -> User {
            get_user(&self.db, self.owner_id).unwrap()
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let wal = self.path.with_extension("db-wal");
            let shm = self.path.with_extension("db-shm");
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(wal);
            let _ = std::fs::remove_file(shm);
        }
    }

    fn scope(caps: Vec<Capability>, wildcard: bool, server_ids: Vec<i64>) -> KeyScope {
        KeyScope {
            capabilities: caps,
            wildcard,
            server_ids,
        }
    }

    #[test]
    fn session_user_keeps_full_owner_grant() {
        let t = TestDb::new();
        let g = server_grant(&t.db, &t.owner(), t.server_id)
            .unwrap()
            .unwrap();
        assert!(g.contains(Capability::ControlStart));
        assert!(g.contains(Capability::FilesWrite));
    }

    #[test]
    fn scoped_key_narrows_owner_capabilities() {
        let t = TestDb::new();
        let mut u = t.owner();
        u.key_scope = Some(scope(vec![Capability::ConsoleRead], false, vec![]));
        let g = server_grant(&t.db, &u, t.server_id).unwrap().unwrap();
        assert!(g.contains(Capability::ConsoleRead));
        // Owner holds these, but the key does not grant them.
        assert!(!g.contains(Capability::FilesWrite));
        assert!(!g.contains(Capability::ControlStart));
    }

    #[test]
    fn scoped_key_cannot_reach_servers_outside_its_list() {
        let t = TestDb::new();
        let mut u = t.owner();
        u.key_scope = Some(scope(vec![], true, vec![t.server_id]));
        let allowed = server_grant(&t.db, &u, t.server_id).unwrap().unwrap();
        assert!(allowed.contains(Capability::ControlStart));
        let denied = server_grant(&t.db, &u, t.other_server_id).unwrap().unwrap();
        assert_eq!(denied.capabilities().count(), 0);
        assert!(!denied.contains(Capability::ConsoleRead));
    }

    #[test]
    fn scoped_key_cannot_widen_a_root_admin() {
        let t = TestDb::new();
        let mut admin = t.owner();
        admin.root_admin = true;
        admin.key_scope = Some(scope(vec![Capability::ConsoleRead], false, vec![]));
        // Root admin normally gets everything; the key still narrows it.
        let g = server_grant(&t.db, &admin, t.other_server_id)
            .unwrap()
            .unwrap();
        assert!(g.contains(Capability::ConsoleRead));
        assert!(!g.contains(Capability::FilesWrite));
    }

    #[test]
    fn full_authority_key_matches_a_session() {
        let t = TestDb::new();
        let mut u = t.owner();
        u.key_scope = Some(scope(vec![], true, vec![]));
        let session = server_grant(&t.db, &t.owner(), t.server_id)
            .unwrap()
            .unwrap();
        let keyed = server_grant(&t.db, &u, t.server_id).unwrap().unwrap();
        assert_eq!(
            session.capabilities().collect::<Vec<_>>(),
            keyed.capabilities().collect::<Vec<_>>()
        );
    }

    #[test]
    fn user_has_capability_honours_the_key_scope() {
        let t = TestDb::new();
        let mut u = t.owner();
        u.key_scope = Some(scope(vec![Capability::ConsoleRead], false, vec![]));
        assert!(user_has_capability(&t.db, &u, t.server_id, Capability::ConsoleRead).unwrap());
        assert!(!user_has_capability(&t.db, &u, t.server_id, Capability::FilesWrite).unwrap());
    }

    #[test]
    fn non_member_gains_nothing_from_a_wildcard_key() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "INSERT INTO users(username,email,password_hash,created_at,updated_at)
             VALUES('stranger','s@t','x','now','now')",
            [],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        drop(conn);
        let mut stranger = get_user(&t.db, sid).unwrap();
        stranger.key_scope = Some(scope(vec![], true, vec![]));
        assert!(server_grant(&t.db, &stranger, t.server_id)
            .unwrap()
            .is_none());
    }
    #[test]
    fn corrupt_blueprint_variables_fail_closed() {
        let t = TestDb::new();
        let blueprint_id: i64 = {
            let conn = t.db.get().unwrap();
            conn.execute(
                "UPDATE blueprints SET variables='not-json' WHERE uuid='b'",
                [],
            )
            .unwrap();
            conn.query_row("SELECT id FROM blueprints WHERE uuid='b'", [], |r| r.get(0))
                .unwrap()
        };

        let error = get_blueprint(&t.db, blueprint_id).unwrap_err();
        let sql_error = error
            .downcast_ref::<rusqlite::Error>()
            .expect("blueprint JSON failure must preserve the SQLite conversion error");
        assert!(matches!(
            sql_error,
            rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, _)
        ));
    }

    #[test]
    fn soft_deleted_servers_do_not_block_owner_deletion() {
        let t = TestDb::new();
        assert_eq!(count_all_servers_by_user(&t.db, t.owner_id).unwrap(), 2);
        delete_server(&t.db, t.server_id).unwrap();
        delete_server(&t.db, t.other_server_id).unwrap();
        // Soft-deleted rows still exist (so undelete/purge is possible) but
        // they must not count as workspaces blocking account deletion.
        let conn = t.db.get().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM servers WHERE user_id=?1",
                [t.owner_id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(remaining, 2, "soft-delete must keep the rows");
        assert_eq!(count_all_servers_by_user(&t.db, t.owner_id).unwrap(), 0);
    }

    #[test]
    fn purge_reaches_a_soft_deleted_server() {
        let t = TestDb::new();
        delete_server(&t.db, t.server_id).unwrap();
        assert!(
            get_server(&t.db, t.server_id).is_err(),
            "ordinary lookup must still hide soft-deleted rows"
        );
        let s = get_server_any(&t.db, t.server_id).unwrap();
        assert_eq!(s.id, t.server_id);
        purge_server(&t.db, t.server_id).unwrap();
        assert!(get_server_any(&t.db, t.server_id).is_err());
        let conn = t.db.get().unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM servers WHERE id=?1",
                [t.server_id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(rows, 0, "purge must physically remove the row");
    }

    #[test]
    fn deleting_owner_cascades_away_soft_deleted_servers() {
        let t = TestDb::new();
        delete_server(&t.db, t.server_id).unwrap();
        delete_user(&t.db, t.owner_id).unwrap();
        let conn = t.db.get().unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM servers WHERE user_id=?1",
                [t.owner_id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(rows, 0, "ON DELETE CASCADE must sweep soft-deleted rows");
        assert!(get_user(&t.db, t.owner_id).is_err());
    }

    #[test]
    fn create_schedule_with_tasks_commits_schedule_and_tasks_together() {
        let t = TestDb::new();
        let tasks = vec![
            ("start".to_string(), "".to_string(), 1),
            ("notify".to_string(), "ping".to_string(), 2),
        ];
        let id = create_schedule_with_tasks(
            &t.db,
            t.server_id,
            "nightly",
            "0 3 * * *",
            true,
            2,
            30,
            false,
            &tasks,
        )
        .unwrap();
        let got = get_schedule(&t.db, id).unwrap();
        let pairs: Vec<(String, String, i64)> = got
            .tasks
            .into_iter()
            .map(|x| (x.action, x.payload, x.sequence))
            .collect();
        assert_eq!(pairs, tasks);
    }

    #[test]
    fn create_schedule_with_tasks_leaves_nothing_on_failure() {
        let t = TestDb::new();
        // Nonexistent server_id -> FK violation on the schedule insert; the
        // transaction must roll back and leave no partial schedule/task rows.
        let tasks = vec![("start".to_string(), "".to_string(), 1)];
        assert!(create_schedule_with_tasks(
            &t.db,
            999_999,
            "bad",
            "0 3 * * *",
            true,
            0,
            0,
            false,
            &tasks,
        )
        .is_err());
        let conn = t.db.get().unwrap();
        let schedules: i64 = conn
            .query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get(0))
            .unwrap();
        let tasks: i64 = conn
            .query_row("SELECT COUNT(*) FROM schedule_tasks", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(schedules, 0, "failed create must leave no schedule row");
        assert_eq!(tasks, 0, "failed create must leave no task rows");
    }

    // ---------------- Crash budget (G8) ----------------

    #[test]
    fn crash_budget_consumes_slots_then_exhausts() {
        let t = TestDb::new();
        // budget 2, no burst yet
        let first = consume_crash_budget(&t.db, t.server_id, 2).unwrap();
        assert_eq!(first, CrashBudget::Allowed(0));
        let second = consume_crash_budget(&t.db, t.server_id, 2).unwrap();
        assert_eq!(second, CrashBudget::Allowed(1));
        // Third crash: budget exhausted, terminal; count stays at 2.
        let third = consume_crash_budget(&t.db, t.server_id, 2).unwrap();
        assert_eq!(third, CrashBudget::Exhausted(2));
        let conn = t.db.get().unwrap();
        let used: i64 = conn
            .query_row("SELECT crash_restarts FROM servers WHERE id=?1", [t.server_id], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(used, 2, "an exhausted burst must not keep consuming");
    }

    #[test]
    fn crash_budget_zero_disables_restarts() {
        let t = TestDb::new();
        assert_eq!(
            consume_crash_budget(&t.db, t.server_id, 0).unwrap(),
            CrashBudget::Exhausted(0),
            "budget 0 must refuse the very first restart"
        );
    }

    #[test]
    fn stale_crash_window_resets_the_burst() {
        let t = TestDb::new();
        // A burst started more than CRASH_WINDOW_SECS ago must not carry debt
        // into a fresh crash: the server survived long enough to be stable.
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE servers SET crash_restarts=7, crash_window_start=?1 WHERE id=?2",
            params![
                (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339(),
                t.server_id
            ],
        )
        .unwrap();
        drop(conn);
        let got = consume_crash_budget(&t.db, t.server_id, 2).unwrap();
        assert_eq!(got, CrashBudget::Allowed(0), "stale window must reset used");
        let conn = t.db.get().unwrap();
        let used: i64 = conn
            .query_row("SELECT crash_restarts FROM servers WHERE id=?1", [t.server_id], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(used, 1);
    }

    #[test]
    fn reset_crash_window_clears_burst_and_reason() {
        let t = TestDb::new();
        let _ = consume_crash_budget(&t.db, t.server_id, 2).unwrap();
        mark_crashed(&t.db, t.server_id, "exited with code 1").unwrap();
        reset_crash_window(&t.db, t.server_id).unwrap();
        let s = get_server(&t.db, t.server_id).unwrap();
        assert_eq!(s.crash_restarts, 0);
        assert_eq!(s.crash_window_start, "");
        assert_eq!(s.crash_reason, "");
        // status is left untouched: the caller decides (running after a start)
        assert_eq!(s.status, "crashed");
    }

    #[test]
    fn mark_crashed_records_status_and_reason() {
        let t = TestDb::new();
        mark_crashed(&t.db, t.server_id, "killed by signal").unwrap();
        let s = get_server(&t.db, t.server_id).unwrap();
        assert_eq!(s.status, "crashed");
        assert_eq!(s.crash_reason, "killed by signal");
    }

    #[test]
    fn validate_port_accepts_only_unprivileged_ports() {
        assert!(validate_port(1024).is_ok());
        assert!(validate_port(65_535).is_ok());
        for bad in [i64::MIN, -1, 0, 22, 1023, 65_536, i64::MAX] {
            assert!(validate_port(bad).is_err(), "port {bad} must be rejected");
        }
    }

    #[test]
    fn audit_prune_respects_retention_window() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        // The newest-500 guard is a floor: rows are only deleted once the
        // table exceeds it, so the fixture must pass 500 total rows.
        for _ in 0..590 {
            conn.execute(
                "INSERT INTO audit_logs(user_id,action,target,ip,details,created_at,server_id)
                 VALUES(1,'old','t','ip','d',?1,NULL)",
                [(Utc::now() - chrono::Duration::days(30)).to_rfc3339()],
            )
            .unwrap();
        }
        for _ in 0..10 {
            conn.execute(
                "INSERT INTO audit_logs(user_id,action,target,ip,details,created_at,server_id)
                 VALUES(1,'new','t','ip','d',?1,NULL)",
                [now()],
            )
            .unwrap();
        }
        let removed = prune_audit_logs(&t.db, 7).unwrap();
        assert_eq!(removed, 100, "490 of 590 old rows are past the window");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 500, "the newest 500 rows are always kept");
        let fresh: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_logs WHERE action='new'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fresh, 10, "rows inside the retention window never go");
    }
    #[test]
    fn audit_prune_always_keeps_newest_500() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        for i in 0..600 {
            conn.execute(
                "INSERT INTO audit_logs(user_id,action,target,ip,details,created_at,server_id)
                 VALUES(1,'x',?1,'ip','d',?2,NULL)",
                params![
                    format!("t{i}"),
                    (Utc::now() - chrono::Duration::days(30)).to_rfc3339()
                ],
            )
            .unwrap();
        }
        let removed = prune_audit_logs(&t.db, 7).unwrap();
        assert_eq!(removed, 100, "only the 500 newest survive the prune");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 500);
    }
    // ---------------- Squads ----------------

    fn seed_user(db: &Db, username: &str) -> i64 {
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO users(username,email,password_hash,created_at,updated_at)
             VALUES(?1,?2,'x','now','now')",
            params![username, format!("{username}@t")],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn squad_grant_resolves_to_squad_role_and_listing() {
        let t = TestDb::new();
        let member_id = seed_user(&t.db, "member");
        let manager_id = seed_user(&t.db, "manager");
        let squad_id = create_squad(&t.db, "Web Team", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, member_id, Role::Viewer, &t.owner()).unwrap();
        add_squad_server(&t.db, squad_id, t.server_id).unwrap();
        let member = get_user(&t.db, member_id).unwrap();
        let g = server_grant(&t.db, &member, t.server_id).unwrap().unwrap();
        assert!(g.contains(Capability::ConsoleRead));
        // Viewer preset carries no power actions.
        assert!(!g.contains(Capability::ControlStart));
        assert!(user_has_server_access(&t.db, &member, t.server_id).unwrap());
        assert!(user_has_capability(&t.db, &member, t.server_id, Capability::FilesRead).unwrap());
        assert!(!user_has_capability(&t.db, &member, t.server_id, Capability::FilesWrite).unwrap());
        // Servers outside the squad stay unreachable.
        assert!(!user_has_server_access(&t.db, &member, t.other_server_id).unwrap());
        // Listing mirrors access: the squad server shows up, the other does not.
        let listed = list_servers(&t.db, Some(member_id), false).unwrap();
        assert!(listed.iter().any(|s| s.id == t.server_id));
        assert!(!listed.iter().any(|s| s.id == t.other_server_id));
        // A Manager role on the same squad upgrades the grant.
        add_squad_member(&t.db, squad_id, manager_id, Role::Manager, &t.owner()).unwrap();
        let manager = get_user(&t.db, manager_id).unwrap();
        let mg = server_grant(&t.db, &manager, t.server_id).unwrap().unwrap();
        assert!(mg.contains(Capability::ControlKill));
        assert!(mg.contains(Capability::BackupsWrite));
    }

    #[test]
    fn squad_grant_most_permissive_wins_over_subuser() {
        let t = TestDb::new();
        let member_id = seed_user(&t.db, "member");
        let squad_id = create_squad(&t.db, "Ops", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, member_id, Role::Manager, &t.owner()).unwrap();
        add_squad_server(&t.db, squad_id, t.server_id).unwrap();
        // A weaker subuser grant must not shrink the squad grant.
        add_subuser(&t.db, t.server_id, member_id, &Grant::new(Role::Viewer, [])).unwrap();
        let member = get_user(&t.db, member_id).unwrap();
        let g = server_grant(&t.db, &member, t.server_id).unwrap().unwrap();
        assert!(g.contains(Capability::ControlKill));
        assert!(g.contains(Capability::BackupsWrite));
        // And the reverse: a stronger subuser grant beats the squad grant.
        let dev_id = seed_user(&t.db, "dev");
        let squad2 = create_squad(&t.db, "View", t.owner_id).unwrap();
        add_squad_member(&t.db, squad2, dev_id, Role::Viewer, &t.owner()).unwrap();
        add_squad_server(&t.db, squad2, t.server_id).unwrap();
        add_subuser(&t.db, t.server_id, dev_id, &Grant::new(Role::Developer, [])).unwrap();
        let dev = get_user(&t.db, dev_id).unwrap();
        let dg = server_grant(&t.db, &dev, t.server_id).unwrap().unwrap();
        assert!(dg.contains(Capability::FilesWrite));
        assert!(dg.contains(Capability::SubusersRead));
        assert!(!dg.contains(Capability::ControlKill));
    }

    #[test]
    fn squad_owner_grant_wins_over_squad_role() {
        // Ownership outranks any squad membership the owner holds on the same
        // server: the effective grant is the full owner set.
        let t = TestDb::new();
        let squad_id = create_squad(&t.db, "S", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, t.owner_id, Role::Viewer, &t.owner()).unwrap();
        add_squad_server(&t.db, squad_id, t.server_id).unwrap();
        let g = server_grant(&t.db, &t.owner(), t.server_id).unwrap().unwrap();
        assert!(g.contains(Capability::ControlKill));
        assert!(g.contains(Capability::StartupSecrets));
    }

    #[test]
    fn squad_member_anti_escalation() {
        let t = TestDb::new();
        let viewer_id = seed_user(&t.db, "viewer");
        let squad_id = create_squad(&t.db, "Team", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, viewer_id, Role::Viewer, &t.owner()).unwrap();
        let viewer = get_user(&t.db, viewer_id).unwrap();
        // A viewer cannot mint a Manager role.
        let victim = seed_user(&t.db, "victim");
        let err = add_squad_member(&t.db, squad_id, victim, Role::Manager, &viewer).unwrap_err();
        assert!(err.to_string().contains("cannot grant"));
        // A manager member can grant Developer, but not promote beyond itself.
        let mgr_id = seed_user(&t.db, "mgr");
        add_squad_member(&t.db, squad_id, mgr_id, Role::Manager, &t.owner()).unwrap();
        let mgr = get_user(&t.db, mgr_id).unwrap();
        add_squad_member(&t.db, squad_id, victim, Role::Developer, &mgr).unwrap();
        // A developer member cannot promote anyone to Manager (developer <
        // manager), while equal roles are always allowed.
        let dev_id = seed_user(&t.db, "dev");
        add_squad_member(&t.db, squad_id, dev_id, Role::Developer, &t.owner()).unwrap();
        let dev = get_user(&t.db, dev_id).unwrap();
        let err = update_squad_member_role(&t.db, squad_id, victim, Role::Manager, &dev).unwrap_err();
        assert!(err.to_string().contains("cannot grant"));
        update_squad_member_role(&t.db, squad_id, victim, Role::Developer, &dev).unwrap();
        update_squad_member_role(&t.db, squad_id, victim, Role::Manager, &mgr).unwrap();
        // A viewer cannot remove a manager member...
        let err = remove_squad_member(&t.db, squad_id, mgr_id, &viewer).unwrap_err();
        assert!(err.to_string().contains("cannot remove"));
        // ...but a manager can remove a member it could have minted.
        let low_id = seed_user(&t.db, "low");
        add_squad_member(&t.db, squad_id, low_id, Role::Viewer, &mgr).unwrap();
        remove_squad_member(&t.db, squad_id, low_id, &mgr).unwrap();
    }

    #[test]
    fn squad_delete_cascades_members_and_servers() {
        let t = TestDb::new();
        let member_id = seed_user(&t.db, "m");
        let squad_id = create_squad(&t.db, "Gone", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, member_id, Role::Viewer, &t.owner()).unwrap();
        add_squad_server(&t.db, squad_id, t.server_id).unwrap();
        delete_squad(&t.db, squad_id).unwrap();
        assert!(get_squad(&t.db, squad_id).is_err());
        let conn = t.db.get().unwrap();
        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM squad_members WHERE squad_id=?1",
                [squad_id],
                |r| r.get(0),
            )
            .unwrap();
        let servers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM squad_servers WHERE squad_id=?1",
                [squad_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(members, 0);
        assert_eq!(servers, 0);
        // The member keeps no residual access after the squad is gone.
        let member = get_user(&t.db, member_id).unwrap();
        assert!(!user_has_server_access(&t.db, &member, t.server_id).unwrap());
    }

    #[test]
    fn deleting_a_user_cascades_squad_membership() {
        let t = TestDb::new();
        let member_id = seed_user(&t.db, "m");
        let squad_id = create_squad(&t.db, "Team", t.owner_id).unwrap();
        add_squad_member(&t.db, squad_id, member_id, Role::Viewer, &t.owner()).unwrap();
        add_squad_server(&t.db, squad_id, t.server_id).unwrap();
        delete_user(&t.db, member_id).unwrap();
        let conn = t.db.get().unwrap();
        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM squad_members WHERE user_id=?1",
                [member_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(members, 0);
    }
}