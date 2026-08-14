//! Multi-node persistence models and scheduling decisions.
use crate::db::Db;
use crate::node_protocol::{NodeCapacity, NodeHeartbeat};
use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// Fallback staleness window for a node whose heartbeat cadence is unknown
/// (fresh boot, or a node that has not been seen since the panel restarted).
/// 3x the default daemon interval of 15s.
const DEFAULT_ONLINE_WINDOW_SECS: i64 = 45;

/// Lower bound for a capacity reservation's lifetime, in seconds. A reservation
/// must outlive the node's heartbeat interval so the daemon's next capacity
/// report reflects the newly provisioned workload before the claim lapses.
const MIN_RESERVATION_TTL_SECS: i64 = 120;

/// Observed heartbeat cadence per node id (seconds), learned from the gap
/// between consecutive heartbeats. `online()` sizes its staleness window from
/// this (3x, clamped), so a node whose daemon is configured with a longer
/// heartbeat interval is not falsely marked offline. In-memory by design: the
/// interval lives in the agent's own config, which the panel never persists.
static HEARTBEAT_CADENCE: LazyLock<Mutex<HashMap<i64, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Staleness window for one node's heartbeat: 3x the observed cadence, clamped
/// to sane bounds, falling back to the 45s default before any cadence is known.
fn online_window_secs(node_id: i64) -> i64 {
    let observed = HEARTBEAT_CADENCE
        .lock()
        .ok()
        .and_then(|m| m.get(&node_id).copied())
        .unwrap_or(0);
    if observed > 0 {
        (observed * 3).clamp(30, 900)
    } else {
        DEFAULT_ONLINE_WINDOW_SECS
    }
}

/// How long a capacity reservation stays live: long enough to span at least
/// one heartbeat report (2x the online window), never below the floor.
fn reservation_ttl_secs(node_id: i64) -> i64 {
    online_window_secs(node_id).saturating_mul(2).max(MIN_RESERVATION_TTL_SECS)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub public_url: String,
    #[serde(skip_serializing)]
    pub secret: String,
    #[serde(skip_serializing)]
    pub enrollment_token: Option<String>,
    pub enrolled: bool,
    pub enabled: bool,
    pub maintenance: bool,
    pub schedulable: bool,
    pub location: String,
    pub tags: Vec<String>,
    pub memory_limit_mb: i64,
    pub disk_limit_mb: i64,
    pub cpu_limit_percent: i64,
    pub memory_overallocate: i64,
    pub disk_overallocate: i64,
    pub daemon_version: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub capacity: NodeCapacity,
    pub last_heartbeat: Option<String>,
    pub last_error: String,
    pub created_at: String,
    pub updated_at: String,
    /// SHA-256 fingerprint of the agent's self-signed certificate, captured at
    /// enrollment. Empty means the node is still on plaintext HTTP.
    pub tls_fingerprint: String,
    /// Operator-seeded certificate fingerprint the enrollment must present
    /// (v16). NULL/empty = plain TOFU at first enrollment; a value means the
    /// operator has declared the identity, so the first enrollment (and every
    /// re-enrollment) must present exactly this fingerprint. Kept after
    /// enrollment so a rotated node re-enrolls against the same declared
    /// identity.
    pub expected_fingerprint: String,
    /// Automated cordon/drain state (v19): `''` = idle, `'hold'` = cordoned
    /// (no new placements, running workloads stay up), `'stop'` = cordoned
    /// with the node's running workloads being stopped. Placement refuses
    /// any node whose `drain_mode` is non-empty, independently of the
    /// schedulable/maintenance flags. `drain_reason` is the operator-
    /// supplied reason; `drain_deadline` the optional RFC3339 instant by
    /// which the drain must complete — an auto-lift deadline: the reconcile
    /// sweep clears the drain (recording a `drain_expired` node event) once
    /// it passes, with no stop escalation.
    pub drain_mode: String,
    pub drain_reason: String,
    pub drain_deadline: Option<String>,
}
impl Node {

    pub fn online(&self) -> bool {
        if !self.enabled || !self.enrolled {
            return false;
        }
        self.last_heartbeat
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|t| {
                t.with_timezone(&Utc)
                    > Utc::now() - Duration::seconds(online_window_secs(self.id))
            })
            .unwrap_or(false)
    }
    pub fn available_memory_mb(&self) -> i64 {
        let hard = if self.memory_limit_mb > 0 {
            self.memory_limit_mb
        } else {
            (self.capacity.memory_total / 1_048_576) as i64
        };
        let allowed = hard
            .saturating_mul(100 + self.memory_overallocate)
            .saturating_div(100);
        allowed.saturating_sub((self.capacity.memory_used / 1_048_576) as i64)
    }

    pub fn available_disk_mb(&self) -> i64 {
        let hard = if self.disk_limit_mb > 0 {
            self.disk_limit_mb
        } else {
            (self.capacity.disk_total / 1_048_576) as i64
        };
        let allowed = hard
            .saturating_mul(100 + self.disk_overallocate)
            .saturating_div(100);
        allowed.saturating_sub((self.capacity.disk_used / 1_048_576) as i64)
    }
}

fn from_row(r: &Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        public_url: r.get(3)?,
        secret: r.get(4)?,
        enrollment_token: r.get(5)?,
        enrolled: r.get::<_, i64>(6)? != 0,
        enabled: r.get::<_, i64>(7)? != 0,
        maintenance: r.get::<_, i64>(8)? != 0,
        schedulable: r.get::<_, i64>(9)? != 0,
        location: r.get(10)?,
        tags: serde_json::from_str(&r.get::<_, String>(11)?).unwrap_or_default(),
        memory_limit_mb: r.get(12)?,
        disk_limit_mb: r.get(13)?,
        cpu_limit_percent: r.get(14)?,
        memory_overallocate: r.get(15)?,
        disk_overallocate: r.get(16)?,
        daemon_version: r.get(17)?,
        hostname: r.get(18)?,
        os: r.get(19)?,
        arch: r.get(20)?,
        capacity: serde_json::from_str(&r.get::<_, String>(21)?).unwrap_or_default(),
        last_heartbeat: r.get(22)?,
        last_error: r.get(23)?,
        created_at: r.get(24)?,
        updated_at: r.get(25)?,
        tls_fingerprint: r.get(26)?,
        expected_fingerprint: r.get::<_, Option<String>>(27)?.unwrap_or_default(),
        drain_mode: r.get(28)?,
        drain_reason: r.get(29)?,
        drain_deadline: r.get(30)?,
    })
}

const COLS: &str = "id,uuid,name,public_url,secret,enrollment_token,enrolled,enabled,maintenance,schedulable,location,tags,memory_limit_mb,disk_limit_mb,cpu_limit_percent,memory_overallocate,disk_overallocate,daemon_version,hostname,os,arch,capacity_json,last_heartbeat,last_error,created_at,updated_at,tls_fingerprint,expected_fingerprint,drain_mode,drain_reason,drain_deadline";

/// Validate a node's public URL: http/https scheme and a non-empty host.
/// Returns the URL with trailing slashes trimmed, exactly as persisted.
pub fn validate_public_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim_end_matches('/');
    let parsed = url::Url::parse(trimmed).context("invalid node URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("node URL must use http or https");
    }
    if parsed.host_str().is_none() {
        bail!("node URL must include a host");
    }
    Ok(trimmed.to_string())
}

pub fn create(
    db: &Db,
    name: &str,
    public_url: &str,
    location: &str,
    tags: &[String],
) -> Result<Node> {
    let public_url = validate_public_url(public_url)?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let secret = crate::auth::random_token(48);
    let enrollment = crate::auth::random_token(32);
    let t = now();
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO nodes(uuid,name,public_url,secret,enrollment_token,location,tags,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
        params![uuid, name, public_url, secret, enrollment, location, serde_json::to_string(tags)?, t],
    )?;
    let id = conn.last_insert_rowid();
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE id=?1"),
        [id],
        from_row,
    )
    .map_err(Into::into)
}

pub fn list(db: &Db) -> Result<Vec<Node>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM nodes ORDER BY name"))?;
    let rows = stmt.query_map([], from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(db: &Db, id: i64) -> Result<Node> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE id=?1"),
        [id],
        from_row,
    )
    .context("node not found")
}

pub fn get_by_uuid(db: &Db, uuid: &str) -> Result<Node> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE uuid=?1"),
        [uuid],
        from_row,
    )
    .context("node not found")
}

pub fn get_by_name(db: &Db, name: &str) -> Result<Node> {
    let conn = db.get()?;
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE name=?1"),
        [name],
        from_row,
    )
    .context("node not found")
}

/// Normalize and strictly validate a TLS certificate fingerprint before it is
/// pinned. Empty stays empty (plaintext agent); anything else must be exactly
/// 64 lowercase hex characters (a SHA-256 digest). Keeps the stored column and
/// the pinned-client cache under one invariant: `""` or 64 hex.
fn pinned_fingerprint(raw: &str) -> Result<String> {
    let normalized = crate::tls::normalize_fingerprint(raw);
    if normalized.is_empty() {
        return Ok(String::new());
    }
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid TLS fingerprint: expected 64 hex characters");
    }
    Ok(normalized)
}

pub fn enroll(db: &Db, token: &str, heartbeat: &NodeHeartbeat) -> Result<Node> {
    // One atomic UPDATE ... RETURNING: the row flips to enrolled and mints the
    // secret in the same statement, so two concurrent enrolls with the same
    // token cannot both observe enrolled=0 — the loser updates zero rows and
    // gets the token error instead of re-minting the secret and killing the
    // winner's HMAC. Fingerprint validation happens before the write so a
    // malformed pin never consumes the token.
    //
    // The api guard's fingerprint gate lives HERE too, as a WHERE predicate
    // on the same statement: the row may only flip when nothing is pinned and
    // nothing is seeded (plain TOFU), or the presented fingerprint matches
    // the pin or the operator-seeded `expected_fingerprint`. Re-evaluating
    // the gate atomically with the enrolled flip closes the check-then-act
    // race (FASE-4 LOW from RecheckFingerprint): a seed committed between
    // the api's guard read and this UPDATE — or a pin changing under the
    // read — still refuses an unseeded pin instead of TOFU-pinning it, and
    // the loser's token stays unconsumed. `expected_fingerprint` is
    // deliberately NOT part of the SET clause: it stays set, so the
    // operator's declared identity keeps gatekeeping every re-enrollment
    // (presented == pinned OR presented == expected_fingerprint).
    let secret = crate::auth::random_token(48);
    let t = now();
    let fingerprint = pinned_fingerprint(&heartbeat.tls_fingerprint)?;
    let conn = db.get()?;
    let node = conn
        .query_row(
            &format!(
                "UPDATE nodes SET enrolled=1,enrollment_token=NULL,secret=?1,daemon_version=?2,hostname=?3,os=?4,arch=?5,capacity_json=?6,last_heartbeat=?7,last_error='',tls_fingerprint=?8,updated_at=?7 \
                 WHERE enrollment_token=?9 AND enrolled=0 AND ((tls_fingerprint='' AND (expected_fingerprint IS NULL OR expected_fingerprint='')) OR tls_fingerprint=?8 OR (expected_fingerprint IS NOT NULL AND expected_fingerprint<>'' AND expected_fingerprint=?8)) RETURNING {COLS}"
            ),
            params![
                secret,
                heartbeat.daemon_version,
                heartbeat.hostname,
                heartbeat.os,
                heartbeat.arch,
                serde_json::to_string(&heartbeat.capacity)?,
                t,
                fingerprint,
                token,
            ],
            from_row,
        )
        .optional()?
        .context("invalid or used enrollment token")?;
    Ok(node)
}


/// Operator-seeded expected certificate fingerprint (v16 plaintext /
/// proxy-fronted enrollment path). `None` (or empty) clears the seed and
/// restores plain TOFU at the next enrollment; a value is normalized and
/// strictly validated (64 hex) before it is stored, so the column can never
/// hold a malformed fingerprint.
pub fn set_expected_fingerprint(
    db: &Db,
    id: i64,
    expected_fingerprint: Option<&str>,
) -> Result<()> {
    let fingerprint = match expected_fingerprint {
        None | Some("") => None,
        Some(raw) => {
            let fp = pinned_fingerprint(raw)?;
            if fp.is_empty() {
                None
            } else {
                Some(fp)
            }
        }
    };
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET expected_fingerprint=?1,updated_at=?2 WHERE id=?3",
        params![fingerprint, now(), id],
    )?;
    if changed == 0 {
        bail!("node not found");
    }
    Ok(())
}

pub fn heartbeat(db: &Db, uuid: &str, heartbeat: &NodeHeartbeat) -> Result<()> {
    let conn = db.get()?;
    // Piggybacked reservation sweep: every enrolled heartbeat is a natural,
    // rate-limited tick for dropping leaked capacity claims (the reserve and
    // release paths also expire on read, but a node that went dark stops
    // touching the table). Guarded create keeps the sweep a no-op on
    // databases that never made a reservation.
    ensure_reservations_table(&conn)?;
    expire_stale_reservations(&conn)?;
    let t = now();
    // Learn the daemon's cadence from the gap between consecutive heartbeats
    // so online() can size its staleness window per node instead of assuming
    // the fixed 15s default. Read before the UPDATE: the previous timestamp is
    // the one this beat is a gap away from.
    let prev: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT id,last_heartbeat FROM nodes WHERE uuid=?1",
            [uuid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let changed = conn.execute(
        "UPDATE nodes SET daemon_version=?1,hostname=?2,os=?3,arch=?4,capacity_json=?5,last_heartbeat=?6,last_error='',updated_at=?6 WHERE uuid=?7 AND enrolled=1",
        params![heartbeat.daemon_version, heartbeat.hostname, heartbeat.os, heartbeat.arch, serde_json::to_string(&heartbeat.capacity)?, t, uuid],
    )?;
    if changed == 0 {
        bail!("node not enrolled");
    }
    if let Some((node_id, Some(prev_ts))) = prev {
        if let Ok(prev_t) = chrono::DateTime::parse_from_rfc3339(&prev_ts) {
            let gap = (Utc::now() - prev_t.with_timezone(&Utc)).num_seconds();
            // Ignore implausible gaps (replays, clock jumps): keep the last
            // sane estimate instead of poisoning the window with an outlier.
            if (5..=3600).contains(&gap) {
                if let Ok(mut m) = HEARTBEAT_CADENCE.lock() {
                    m.insert(node_id, gap);
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    db: &Db,
    id: i64,
    name: &str,
    public_url: &str,
    enabled: bool,
    maintenance: bool,
    schedulable: bool,
    location: &str,
    tags: &[String],
    memory_limit_mb: i64,
    disk_limit_mb: i64,
    memory_overallocate: i64,
    disk_overallocate: i64,
) -> Result<Node> {
    if memory_limit_mb < 0 || disk_limit_mb < 0 || memory_overallocate < 0 || disk_overallocate < 0
    {
        bail!("resource limits must not be negative");
    }
    let public_url = validate_public_url(public_url)?;
    // `servers.node` stores the node's name at assignment time, so a rename
    // would silently orphan those assignments (delete()'s orphan check counts
    // by the stored name). Refuse to rename while servers are still assigned;
    // BEGIN IMMEDIATE keeps the read-check-update atomic across pooled
    // connections (the old global mutex used to serialize this).
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current_name: String = tx
        .query_row("SELECT name FROM nodes WHERE id=?1", [id], |r| r.get(0))
        .context("node not found")?;
    if name != current_name {
        let assigned: i64 = tx.query_row(
            "SELECT COUNT(*) FROM servers WHERE node=?1 AND deleted=0",
            [&current_name],
            |r| r.get(0),
        )?;
        if assigned > 0 {
            bail!("cannot rename node: {assigned} assigned server(s) would be orphaned");
        }
    }
    tx.execute(
        "UPDATE nodes SET name=?1,public_url=?2,enabled=?3,maintenance=?4,schedulable=?5,location=?6,tags=?7,memory_limit_mb=?8,disk_limit_mb=?9,memory_overallocate=?10,disk_overallocate=?11,updated_at=?12 WHERE id=?13",
        params![name, public_url, enabled as i64, maintenance as i64, schedulable as i64, location, serde_json::to_string(tags)?, memory_limit_mb, disk_limit_mb, memory_overallocate, disk_overallocate, now(), id],
    )?;
    let node = tx.query_row(&format!("SELECT {COLS} FROM nodes WHERE id=?1"), [id], from_row)?;
    tx.commit()?;
    Ok(node)
}

pub fn rotate_secret(db: &Db, id: i64) -> Result<(String, String)> {
    // Rotate the shared secret AND the enrollment token in one write, detaching
    // the node until it re-enrolls. The old secret stops verifying immediately.
    // Both fingerprints are intentionally KEPT by this UPDATE: the pinned TLS
    // fingerprint (re-enrollment with the same fingerprint, identity
    // unchanged, is accepted by the enroll guard) and the operator-seeded
    // expected_fingerprint (a rotated node re-enrolls against the same
    // declared identity). A different fingerprint is refused until
    // delete+recreate or an operator update of expected_fingerprint.
    let secret = crate::auth::random_token(48);
    let token = crate::auth::random_token(32);
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET secret=?1,enrollment_token=?2,enrolled=0,last_heartbeat=NULL,updated_at=?3 WHERE id=?4",
        params![secret, token, now(), id],
    )?;
    if changed == 0 {
        bail!("node not found");
    }
    Ok((secret, token))
}

pub fn regenerate_enrollment(db: &Db, id: i64) -> Result<String> {
    // Re-enrollment also rotates the secret: the previous HMAC is dead as soon
    // as the new token is minted, not merely blocked by enrolled=0.
    let (_, token) = rotate_secret(db, id)?;
    Ok(token)
}

/// Update the pinned TLS fingerprint on the authenticated heartbeat path.
/// Only accepted for enrolled nodes; the caller must have verified the HMAC.
/// The value is normalized and strictly validated before it is stored, so the
/// column can never hold a malformed pin.
pub fn set_tls_fingerprint(db: &Db, uuid: &str, fingerprint: &str) -> Result<()> {
    let fingerprint = pinned_fingerprint(fingerprint)?;
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET tls_fingerprint=?1,updated_at=?2 WHERE uuid=?3 AND enrolled=1",
        params![fingerprint, now(), uuid],
    )?;
    if changed == 0 {
        bail!("node not enrolled");
    }
    Ok(())
}

pub fn set_error(db: &Db, uuid: &str, error: &str) -> Result<()> {
    // Same enrolled-only guard as heartbeat()/set_tls_fingerprint(): an
    // unenrolled (or revoked) node must not be able to write its state.
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET last_error=?1,updated_at=?2 WHERE uuid=?3 AND enrolled=1",
        params![error, now(), uuid],
    )?;
    if changed == 0 {
        bail!("node not enrolled");
    }
    Ok(())
}

pub fn delete(db: &Db, id: i64) -> Result<()> {
    // Orphan check + delete must stay atomic across pooled connections;
    // BEGIN IMMEDIATE serializes writers like the old global mutex did.
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let assigned: i64 = tx.query_row("SELECT COUNT(*) FROM servers WHERE node=(SELECT name FROM nodes WHERE id=?1) AND deleted=0", [id], |r| r.get(0))?;
    if assigned > 0 {
        bail!("node has {assigned} assigned server(s)");
    }
    tx.execute("DELETE FROM nodes WHERE id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

pub fn record_event(
    db: &Db,
    node_id: i64,
    level: &str,
    kind: &str,
    message: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO node_events(node_id,level,kind,message,payload,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![node_id, level, kind, message, serde_json::to_string(payload)?, now()],
    )?;
    Ok(())
}

pub fn events(db: &Db, node_id: i64, limit: i64) -> Result<Vec<serde_json::Value>> {
    let conn = db.get()?;
    // Client-supplied limits are clamped: 0/negative and absurdly large values
    // must not yield empty results or unbounded scans.
    let limit = limit.clamp(1, 200);
    let mut stmt = conn.prepare("SELECT id,level,kind,message,payload,created_at FROM node_events WHERE node_id=?1 ORDER BY id DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![node_id, limit], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?, "level": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?,
            "message": r.get::<_, String>(3)?, "payload": serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(4)?).unwrap_or_default(),
            "created_at": r.get::<_, String>(5)?,
        }))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Create the capacity-reservation table on demand. The DDL is owned by the
/// db.rs migration ladder (v18 step); this is a guarded CREATE IF NOT
/// EXISTS fallback for pre-v18 databases, safe and idempotent to call
/// before every use.
fn ensure_reservations_table(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS node_reservations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            memory_mb INTEGER NOT NULL,
            disk_mb INTEGER NOT NULL,
            reserved_until TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
    )?;
    Ok(())
}

/// Drop reservations whose TTL has lapsed. The TTL is set so a reservation
/// outlives the node's next heartbeat report; anything older is a leaked claim
/// (create failed before commit without a release, or the node went dark).
fn expire_stale_reservations(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute("DELETE FROM node_reservations WHERE reserved_until < ?1", [now()])?;
    Ok(())
}

/// Memory/disk still claimed (unexpired) for a node by outstanding reservations.
fn reserved_usage(conn: &rusqlite::Connection, node_id: i64) -> Result<(i64, i64)> {
    let (mem, disk): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(memory_mb),0), COALESCE(SUM(disk_mb),0) \
         FROM node_reservations WHERE node_id=?1 AND reserved_until > ?2",
        params![node_id, now()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((mem, disk))
}

/// Pick the least-loaded candidate for a server of the given size, counting
/// both the capacity the agent last reported and any outstanding capacity
/// reservations. Runs against `tx` so callers can claim atomically.
fn pick_node_tx(
    tx: &rusqlite::Transaction,
    memory_mb: i64,
    disk_mb: i64,
    required_tags: &[String],
    location: Option<&str>,
) -> Result<Node> {
    let mut stmt = tx.prepare(&format!("SELECT {COLS} FROM nodes"))?;
    let nodes = stmt
        .query_map([], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut candidates: Vec<Node> = Vec::new();
    for n in nodes {
        if !n.online() || !n.enabled || !n.schedulable || n.maintenance || !n.drain_mode.is_empty()
        {
            continue;
        }
        if let Some(loc) = location {
            if !loc.is_empty() && n.location != loc {
                continue;
            }
        }
        if !required_tags.iter().all(|tag| n.tags.iter().any(|t| t == tag)) {
            continue;
        }
        let (r_mem, r_disk) = reserved_usage(tx, n.id)?;
        if n.available_memory_mb().saturating_sub(r_mem) < memory_mb {
            continue;
        }
        if n.available_disk_mb().saturating_sub(r_disk) < disk_mb {
            continue;
        }
        candidates.push(n);
    }
    candidates.sort_by(|a, b| {
        let a_score = a.capacity.cpu_percent
            + (a.capacity.memory_used as f64 / a.capacity.memory_total.max(1) as f64 * 100.0)
            + a.capacity.servers_running as f64 * 2.0;
        let b_score = b.capacity.cpu_percent
            + (b.capacity.memory_used as f64 / b.capacity.memory_total.max(1) as f64 * 100.0)
            + b.capacity.servers_running as f64 * 2.0;
        a_score.total_cmp(&b_score)
    });
    candidates
        .into_iter()
        .next()
        .context("no online node has enough capacity (nodes in drain are never selected)")
}

/// Dry-run placement: the same candidate rules as the real provisioning path
/// (including outstanding reservations), but without claiming anything. Used
/// by the advisory placement endpoint.
pub fn select_for_server(
    db: &Db,
    memory_mb: i64,
    disk_mb: i64,
    required_tags: &[String],
    location: Option<&str>,
) -> Result<Node> {
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure_reservations_table(&tx)?;
    expire_stale_reservations(&tx)?;
    let node = pick_node_tx(&tx, memory_mb, disk_mb, required_tags, location)?;
    tx.commit()?;
    Ok(node)
}

/// Claim `memory_mb`/`disk_mb` on the best fitting node, atomically with the
/// caller's transaction. The reservation row is what stops two concurrent
/// provisions from picking the same node while the agent's capacity report is
/// stale: it survives this transaction's commit (until its TTL) and is visible
/// to every later `select_for_server`/`reserve_capacity_tx`. On failure the
/// caller's transaction rolls back, which releases the claim with it.
///
/// Returns the chosen node and the reservation id (for an early release).
pub fn reserve_capacity_tx(
    tx: &rusqlite::Transaction,
    memory_mb: i64,
    disk_mb: i64,
    required_tags: &[String],
    location: Option<&str>,
) -> Result<(Node, i64)> {
    ensure_reservations_table(tx)?;
    expire_stale_reservations(tx)?;
    let node = pick_node_tx(tx, memory_mb, disk_mb, required_tags, location)?;
    let until = (Utc::now() + chrono::Duration::seconds(reservation_ttl_secs(node.id)))
        .to_rfc3339();
    tx.execute(
        "INSERT INTO node_reservations(node_id,memory_mb,disk_mb,reserved_until,created_at) \
         VALUES(?1,?2,?3,?4,?5)",
        params![node.id, memory_mb, disk_mb, until, now()],
    )?;
    Ok((node, tx.last_insert_rowid()))
}

/// Release a reservation early (e.g. after a failed provision). Idempotent:
/// a stale or already-released id is a no-op.
pub fn release_reservation(db: &Db, reservation_id: i64) -> Result<()> {
    // Idempotent-release path: the table may not exist yet on databases that
    // never claimed capacity, so ensure it before touching it (a stale or
    // already-released id is still a no-op).
    let conn = db.get()?;
    ensure_reservations_table(&conn)?;
    conn.execute("DELETE FROM node_reservations WHERE id=?1", [reservation_id])?;
    Ok(())
}

/// True when no live server on `node` uses `port` (allocation-aware).
pub fn port_available_on_node(db: &Db, node: &str, port: i64) -> Result<bool> {
    let conn = db.get()?;
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE node=?1 AND port=?2 AND deleted=0",
        params![node, port],
        |r| r.get(0),
    )?;
    Ok(used == 0)
}

/// Number of live (non-deleted) servers assigned to the node — the drain
/// `affected_count` the API reports.
pub fn server_count(db: &Db, node_name: &str) -> Result<i64> {
    let conn = db.get()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE node=?1 AND deleted=0",
        [node_name],
        |r| r.get(0),
    )?;
    Ok(n)
}

/// Cordon a node and record the drain intent. `mode` is `"hold"` (cordon
/// only) or `"stop"` (cordon + the operator stops the running workloads);
/// the API layer validates the mode before calling. `deadline` is an
/// RFC3339 instant, or `None` for no deadline. Returns the number of live
/// servers affected by the drain (the `affected_count` the API reports).
pub fn set_drain(
    db: &Db,
    id: i64,
    mode: &str,
    reason: &str,
    deadline: Option<&str>,
) -> Result<i64> {
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET schedulable=0, maintenance=1, drain_mode=?1, drain_reason=?2, drain_deadline=?3, updated_at=?4 WHERE id=?5",
        params![mode, reason, deadline, now(), id],
    )?;
    if changed == 0 {
        bail!("node not found");
    }
    let name: String = conn.query_row(
        "SELECT name FROM nodes WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    let affected: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE node=?1 AND deleted=0",
        [&name],
        |r| r.get(0),
    )?;
    Ok(affected)
}

/// Lift a drain: clear the drain state and restore scheduling. A no-op for
/// a node that is not draining; a missing node is an error.
pub fn clear_drain(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    let changed = conn.execute(
        "UPDATE nodes SET schedulable=1, maintenance=0, drain_mode='', drain_reason='', drain_deadline=NULL, updated_at=?1 WHERE id=?2",
        params![now(), id],
    )?;
    if changed == 0 {
        bail!("node not found");
    }
    Ok(())
}

/// Lift every drain whose deadline has passed, returning the affected nodes
/// as they were before the lift. `now` is the RFC3339 instant the sweep
/// compares against; the caller computes it once per sweep. This is an
/// auto-lift only — no stop escalation happens here.
///
/// The clear is atomic: a `BEGIN IMMEDIATE` transaction serializes against
/// concurrent `set_drain`/`clear_drain` writers, so a drain replaced with a
/// fresh future deadline between the SELECT and the UPDATE is never lifted.
/// After the commit, one `drain_expired` node event is recorded per affected
/// node.
pub fn clear_expired_drains(db: &Db, now: &str) -> Result<Vec<Node>> {
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Deadline comparison excludes NULL deadlines (NULL<=? is NULL), so a
    // drain without a deadline is never swept.
    let sql = format!("SELECT {COLS} FROM nodes WHERE drain_mode<>'' AND drain_deadline<=?1");
    // The statement borrows the transaction; scope it so the commit below can
    // consume `tx`.
    let expired = {
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(params![now], from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute(
        "UPDATE nodes SET schedulable=1, maintenance=0, drain_mode='', drain_reason='', drain_deadline=NULL, updated_at=?1 WHERE drain_mode<>'' AND drain_deadline<=?1",
        params![now],
    )?;
    tx.commit()?;
    for n in &expired {
        record_event(
            db,
            n.id,
            "info",
            "drain_expired",
            "drain deadline reached; drain lifted and scheduling restored",
            &serde_json::json!({
                "mode": n.drain_mode,
                "deadline": n.drain_deadline,
            }),
        )?;
    }
    Ok(expired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_protocol::{NodeCapacity, NodeHeartbeat};

    fn test_db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        // The connection keeps the unlinked file usable for the test's lifetime.
        let path = dir.path().join("nodes-test.db");
        let db = crate::db::open(path.to_str().unwrap()).unwrap();
        std::mem::forget(dir);
        db
    }

    fn heartbeat_fixture() -> NodeHeartbeat {
        NodeHeartbeat {
            daemon_version: "0.1.1-test".into(),
            hostname: "host".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            started_at: now(),
            capacity: NodeCapacity::default(),
            tls_fingerprint: String::new(),
        }
    }

    fn fp64() -> String {
        "ab12".repeat(16)
    }

    #[test]
    fn validate_public_url_contract() {
        assert_eq!(
            validate_public_url("https://node.example.com/").unwrap(),
            "https://node.example.com"
        );
        assert_eq!(
            validate_public_url("http://10.0.0.1:8081///").unwrap(),
            "http://10.0.0.1:8081"
        );
        assert!(validate_public_url("ftp://node.example.com").is_err());
        assert!(validate_public_url("https://").is_err());
        assert!(validate_public_url("not a url").is_err());
    }

    #[test]
    fn update_rejects_negative_limits_and_invalid_urls() {
        let db = test_db();
        let n = create(&db, "node-a", "https://node-a.example.com", "dc1", &[]).unwrap();
        let err = update(
            &db,
            n.id,
            "node-a",
            "https://node-a.example.com",
            true,
            false,
            true,
            "dc1",
            &[],
            -1,
            0,
            0,
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not be negative"));
        let err = update(
            &db,
            n.id,
            "node-a",
            "ftp://bad.example.com",
            true,
            false,
            true,
            "dc1",
            &[],
            0,
            0,
            0,
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("http or https"));
        // Failed updates leave the row untouched.
        let got = get(&db, n.id).unwrap();
        assert_eq!(got.public_url, "https://node-a.example.com");
        assert_eq!(got.memory_limit_mb, 0);
    }

    #[test]
    fn rotate_secret_atomically_revokes_and_rejects_missing() {
        let db = test_db();
        let n = create(&db, "node-b", "https://node-b.example.com", "", &[]).unwrap();
        let token = n.enrollment_token.clone().unwrap();
        let hb = heartbeat_fixture();
        let enrolled = enroll(&db, &token, &hb).unwrap();
        assert!(enrolled.enrolled);
        heartbeat(&db, &enrolled.uuid, &hb).unwrap();

        let (secret, new_token) = rotate_secret(&db, n.id).unwrap();
        assert!(!secret.is_empty());
        assert!(!new_token.is_empty());
        let after = get(&db, n.id).unwrap();
        assert!(!after.enrolled);
        assert_eq!(after.secret, secret);
        assert_eq!(after.enrollment_token.as_deref(), Some(new_token.as_str()));
        assert!(after.last_heartbeat.is_none());
        // The rotated node no longer heartbeats: enrolled=0 blocks the update.
        let err = heartbeat(&db, &enrolled.uuid, &hb).unwrap_err();
        assert!(err.to_string().contains("node not enrolled"));
        // Missing id is rejected loudly.
        let err = rotate_secret(&db, 999_999).unwrap_err();
        assert!(err.to_string().contains("node not found"));
    }

    #[test]
    fn regenerate_enrollment_revokes_same_as_rotate() {
        let db = test_db();
        let n = create(&db, "node-e", "https://node-e.example.com", "", &[]).unwrap();
        let hb = heartbeat_fixture();
        let enrolled = enroll(&db, n.enrollment_token.as_deref().unwrap(), &hb).unwrap();
        let old_secret = enrolled.secret.clone();
        let token = regenerate_enrollment(&db, n.id).unwrap();
        let after = get(&db, n.id).unwrap();
        assert!(!after.enrolled);
        assert_ne!(after.secret, old_secret);
        assert_eq!(after.enrollment_token.as_deref(), Some(token.as_str()));
        assert!(after.last_heartbeat.is_none());
    }

    #[test]
    fn enroll_mints_fresh_secret_and_consumes_token() {
        let db = test_db();
        let n = create(&db, "node-c", "https://node-c.example.com", "", &[]).unwrap();
        let original_secret = n.secret.clone();
        let token = n.enrollment_token.clone().unwrap();
        let mut hb = heartbeat_fixture();
        hb.tls_fingerprint = fp64();
        let enrolled = enroll(&db, &token, &hb).unwrap();
        assert!(enrolled.enrolled);
        assert!(enrolled.enrollment_token.is_none());
        assert_ne!(enrolled.secret, original_secret);
        assert_eq!(enrolled.tls_fingerprint, fp64());
        // The token is single-use: a second enrollment is rejected.
        let err = enroll(&db, &token, &hb).unwrap_err();
        assert!(err.to_string().contains("invalid or used enrollment token"));
        // Heartbeat is accepted now that the node is enrolled.
        heartbeat(&db, &enrolled.uuid, &hb).unwrap();
    }

    #[test]
    fn set_tls_fingerprint_only_for_enrolled() {
        let db = test_db();
        let n = create(&db, "node-d", "https://node-d.example.com", "", &[]).unwrap();
        let err = set_tls_fingerprint(&db, &n.uuid, &fp64()).unwrap_err();
        assert!(err.to_string().contains("node not enrolled"));
        let hb = heartbeat_fixture();
        enroll(&db, n.enrollment_token.as_deref().unwrap(), &hb).unwrap();
        set_tls_fingerprint(&db, &n.uuid, &fp64()).unwrap();
        assert_eq!(get(&db, n.id).unwrap().tls_fingerprint, fp64());
    }
    #[test]
    fn enroll_rejects_malformed_fingerprint_and_pins_normalized() {
        let db = test_db();
        let n = create(&db, "node-f", "https://node-f.example.com", "", &[]).unwrap();
        let mut hb = heartbeat_fixture();
        // Strict 64-hex validation gates what gets pinned at enrollment.
        hb.tls_fingerprint = "zz".repeat(32);
        let err = enroll(&db, n.enrollment_token.as_deref().unwrap(), &hb).unwrap_err();
        assert!(err.to_string().contains("64 hex"));
        // A colon-separated uppercase fingerprint is normalized before pinning.
        let n2 = create(&db, "node-g", "https://node-g.example.com", "", &[]).unwrap();
        let mut hb2 = heartbeat_fixture();
        let upper = fp64().to_ascii_uppercase();
        let colonned = upper
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join(":");
        hb2.tls_fingerprint = colonned;
        let enrolled = enroll(&db, n2.enrollment_token.as_deref().unwrap(), &hb2).unwrap();
        assert_eq!(enrolled.tls_fingerprint, fp64());
    }
    #[test]
    fn enroll_is_atomic_and_never_remints_secret_on_second_use() {
        let db = test_db();
        let n = create(&db, "node-i", "https://node-i.example.com", "", &[]).unwrap();
        let token = n.enrollment_token.clone().unwrap();
        let hb = heartbeat_fixture();
        let first = enroll(&db, &token, &hb).unwrap();
        // A second enrollment with the same token must fail AND must not
        // re-mint the secret under the first node (the old SELECT-then-UPDATE
        // race clobbered the winner's HMAC key).
        let err = enroll(&db, &token, &hb).unwrap_err();
        assert!(err.to_string().contains("invalid or used enrollment token"));
        assert_eq!(
            get(&db, n.id).unwrap().secret,
            first.secret,
            "losing enroll must not rotate the winner's secret"
        );
        heartbeat(&db, &first.uuid, &hb).unwrap();
    }

    /// FASE-4 LOW (RecheckFingerprint): the api handler pre-checks the
    /// fingerprint in its own SELECT; the authoritative gate is the WHERE
    /// predicate inside enroll's atomic UPDATE, which re-reads pin/seed at
    /// statement time. Simulate the race — an operator seeding
    /// `expected_fingerprint` between the guard read and the enroll write —
    /// by enrolling with the seed already committed and no prior guard: the
    /// mismatched pin must be refused, the token must survive, and the seed
    /// must remain for the seeded identity.
    #[test]
    fn enroll_refuses_seed_committed_after_guard_read() {
        let db = test_db();
        let n = create(&db, "node-race", "https://race.example.com", "", &[]).unwrap();
        let token = n.enrollment_token.clone().unwrap();
        let seeded = fp64();
        set_expected_fingerprint(&db, n.id, Some(&seeded)).unwrap();

        let mut hb = heartbeat_fixture();
        hb.tls_fingerprint = "cd34".repeat(16);
        let err = enroll(&db, &token, &hb).unwrap_err();
        assert!(err.to_string().contains("invalid or used enrollment token"));
        let after = get(&db, n.id).unwrap();
        assert!(!after.enrolled, "seeded gate must refuse the mismatched pin");
        assert_eq!(
            after.enrollment_token.as_deref(),
            Some(token.as_str()),
            "refused enroll must not consume the token"
        );
        assert_eq!(after.expected_fingerprint, seeded, "seed must survive");

        // The same token still enrolls the seeded identity.
        let mut hb2 = heartbeat_fixture();
        hb2.tls_fingerprint = seeded.clone();
        let enrolled = enroll(&db, &token, &hb2).unwrap();
        assert!(enrolled.enrolled);
        assert_eq!(enrolled.tls_fingerprint, seeded);
    }

    #[test]
    fn rename_refused_while_servers_assigned_prevents_orphans() {
        let db = test_db();
        let n = create(&db, "node-h", "https://node-h.example.com", "", &[]).unwrap();
        // Assign a server by node name, exactly like server creation does.
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO users (username,email,password_hash,created_at,updated_at) VALUES ('u','u@example.com','x','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blueprints (uuid,name,created_at,updated_at) VALUES ('bp-1','e','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO servers (uuid,name,user_id,blueprint_id,node,created_at,updated_at) VALUES ('srv-1','s1',1,1,'node-h','t','t')",
            [],
        )
        .unwrap();
        drop(conn);

        // Renaming a node that still has assigned servers must fail so
        // delete()'s name-based orphan check cannot be defeated.
        let err = update(
            &db,
            n.id,
            "renamed",
            "https://node-h.example.com",
            true,
            false,
            true,
            "",
            &[],
            0,
            0,
            0,
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("assigned server"));
        assert_eq!(get(&db, n.id).unwrap().name, "node-h");
        // Deleting with assigned servers is refused too.
        let err = delete(&db, n.id).unwrap_err();
        assert!(err.to_string().contains("assigned"));

        // Once the server is gone, the rename goes through.
        let conn = db.get().unwrap();
        conn.execute("DELETE FROM servers WHERE uuid='srv-1'", []).unwrap();
        drop(conn);
        let renamed = update(
            &db,
            n.id,
            "renamed",
            "https://node-h.example.com",
            true,
            false,
            true,
            "",
            &[],
            0,
            0,
            0,
            0,
        )
        .unwrap();
        assert_eq!(renamed.name, "renamed");
    }

    #[test]
    fn set_error_only_for_enrolled() {
        let db = test_db();
        let n = create(&db, "node-j", "https://node-j.example.com", "", &[]).unwrap();
        let err = set_error(&db, &n.uuid, "boom").unwrap_err();
        assert!(err.to_string().contains("node not enrolled"));
        enroll(&db, n.enrollment_token.as_deref().unwrap(), &heartbeat_fixture()).unwrap();
        set_error(&db, &n.uuid, "boom").unwrap();
        assert_eq!(get(&db, n.id).unwrap().last_error, "boom");
    }

    /// Enroll and heartbeat a node with a fixed capacity and configured limits
    /// so `available_*_mb()` and `online()` are deterministic.
    fn online_node_with_limits(db: &Db, name: &str, mem_mb: i64, disk_mb: i64) -> Node {
        let n = create(db, name, &format!("https://{name}.example.com"), "", &[]).unwrap();
        let mut hb = heartbeat_fixture();
        hb.capacity.memory_total = (mem_mb as u64).saturating_mul(1_048_576);
        hb.capacity.disk_total = (disk_mb as u64).saturating_mul(1_048_576);
        enroll(db, n.enrollment_token.as_deref().unwrap(), &hb).unwrap();
        heartbeat(db, &n.uuid, &hb).unwrap();
        update(
            db,
            n.id,
            name,
            &format!("https://{name}.example.com"),
            true,
            false,
            true,
            "",
            &[],
            mem_mb,
            disk_mb,
            0,
            0,
        )
        .unwrap()
    }

    #[test]
    fn reserve_capacity_prevents_oversubscription_until_released() {
        let db = test_db();
        // 8192 MB of memory: a single 5000 MB claim leaves only 3192 free, so
        // a second identical claim must be refused.
        let n = online_node_with_limits(&db, "node-cap", 8192, 16384);
        assert!(n.online());

        // First claim is granted; a second one inside the same transaction is
        // refused — the outstanding reservation counts against the stale
        // heartbeat capacity, exactly the race that previously let two
        // concurrent creates double-book a node.
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (node, rid) = reserve_capacity_tx(&tx, 5000, 8192, &[], None).unwrap();
        assert_eq!(node.id, n.id);
        assert!(rid > 0);
        let err = reserve_capacity_tx(&tx, 5000, 8192, &[], None).unwrap_err();
        assert!(err.to_string().contains("no online node has enough capacity"));
        // Rolling the transaction back releases the claim (the create path
        // relies on this when the quota/port checks fail before commit).
        drop(tx);

        // After the rollback the same claim succeeds and is committed.
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (node2, rid2) = reserve_capacity_tx(&tx, 5000, 8192, &[], None).unwrap();
        assert_eq!(node2.id, n.id);
        tx.commit().unwrap();

        // A committed reservation stays live: a fresh transaction still sees
        // the node as exhausted (this is what protects the window between the
        // row insert and the agent's next heartbeat report).
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        assert!(reserve_capacity_tx(&tx, 5000, 8192, &[], None).is_err());
        drop(tx);

        // An explicit release frees the capacity again.
        release_reservation(&db, rid2).unwrap();
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (node3, _) = reserve_capacity_tx(&tx, 5000, 8192, &[], None).unwrap();
        assert_eq!(node3.id, n.id);
        drop(tx);
    }

    #[test]
    fn events_limit_is_clamped() {
        let db = test_db();
        let n = create(&db, "node-evt", "https://evt.example.com", "", &[]).unwrap();
        for i in 0..250 {
            record_event(&db, n.id, "info", "k", &format!("m{i}"), &serde_json::json!({}))
                .unwrap();
        }
        assert_eq!(events(&db, n.id, 10_000).unwrap().len(), 200);
        assert_eq!(events(&db, n.id, 0).unwrap().len(), 1);
        assert_eq!(events(&db, n.id, -5).unwrap().len(), 1);
    }

    #[test]
    fn reservation_release_and_heartbeat_sweep_are_idempotent() {
        let db = test_db();
        // Release on a database that never made a reservation is a no-op:
        // the table is ensured on demand (LOW from RecheckCapacity).
        release_reservation(&db, 999_999).unwrap();

        // Heartbeat piggybacks the stale-claim sweep: a lapsed reservation is
        // dropped on the next enrolled beat.
        let n = online_node_with_limits(&db, "node-sweep", 8192, 16384);
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let (_, rid) = reserve_capacity_tx(&tx, 1000, 1024, &[], None).unwrap();
        tx.commit().unwrap();
        // Backdate the claim past its TTL so only the sweep can remove it.
        conn.execute(
            "UPDATE node_reservations SET reserved_until='2000-01-01T00:00:00+00:00' WHERE id=?1",
            [rid],
        )
        .unwrap();
        drop(conn);
        heartbeat(&db, &n.uuid, &heartbeat_fixture()).unwrap();
        let remaining: i64 = db
            .get()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM node_reservations WHERE id=?1",
                [rid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "heartbeat sweep must delete lapsed claims");
    }

    #[test]
    fn set_and_clear_drain_state() {
        let db = test_db();
        let n = online_node_with_limits(&db, "node-drain", 8192, 16384);
        assert_eq!(server_count(&db, "node-drain").unwrap(), 0);
        // Two servers on the node form the drain's affected set.
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO users (username,email,password_hash,created_at,updated_at) VALUES ('u','u@example.com','x','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blueprints (uuid,name,created_at,updated_at) VALUES ('bp-d','e','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO servers (uuid,name,user_id,blueprint_id,node,created_at,updated_at) VALUES ('srv-d1','s1',1,1,'node-drain','t','t'),('srv-d2','s2',1,1,'node-drain','t','t')",
            [],
        )
        .unwrap();
        drop(conn);

        let deadline = (Utc::now() + Duration::hours(1)).to_rfc3339();
        let affected = set_drain(&db, n.id, "hold", "rack upgrade", Some(&deadline)).unwrap();
        assert_eq!(affected, 2, "both live servers must count as affected");
        let drained = get(&db, n.id).unwrap();
        assert!(!drained.schedulable, "drain must cordon the node");
        assert!(drained.maintenance, "drain must mark the node in maintenance");
        assert_eq!(drained.drain_mode, "hold");
        assert_eq!(drained.drain_reason, "rack upgrade");
        assert_eq!(drained.drain_deadline.as_deref(), Some(deadline.as_str()));

        // clear_drain restores scheduling and wipes the drain state.
        clear_drain(&db, n.id).unwrap();
        let restored = get(&db, n.id).unwrap();
        assert!(restored.schedulable);
        assert!(!restored.maintenance);
        assert_eq!(restored.drain_mode, "");
        assert_eq!(restored.drain_reason, "");
        assert!(restored.drain_deadline.is_none());

        // Missing nodes are rejected loudly on both paths.
        assert!(set_drain(&db, 999_999, "hold", "x", None).is_err());
        assert!(clear_drain(&db, 999_999).is_err());
    }

    #[test]
    fn expired_drain_deadline_is_auto_lifted_with_event() {
        let db = test_db();
        let n = online_node_with_limits(&db, "node-exp", 8192, 16384);
        let past = (Utc::now() - Duration::minutes(5)).to_rfc3339();
        set_drain(&db, n.id, "hold", "stale", Some(&past)).unwrap();

        let cleared = clear_expired_drains(&db, &Utc::now().to_rfc3339()).unwrap();
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].id, n.id);
        let after = get(&db, n.id).unwrap();
        assert!(after.schedulable, "expired drain must be lifted");
        assert!(!after.maintenance, "lift must clear the maintenance flag");
        assert_eq!(after.drain_mode, "");
        assert!(after.drain_deadline.is_none());
        let evs = events(&db, n.id, 10).unwrap();
        assert!(
            evs.iter().any(|e| e["kind"] == "drain_expired"),
            "lift must record a drain_expired node event: {evs:?}"
        );
    }

    #[test]
    fn future_and_absent_deadlines_survive_the_sweep() {
        let db = test_db();
        let fut = online_node_with_limits(&db, "node-fut", 8192, 16384);
        let none = online_node_with_limits(&db, "node-none", 8192, 16384);
        let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
        set_drain(&db, fut.id, "hold", "later", Some(&future)).unwrap();
        set_drain(&db, none.id, "hold", "no-deadline", None).unwrap();

        let cleared = clear_expired_drains(&db, &Utc::now().to_rfc3339()).unwrap();
        assert!(cleared.is_empty(), "nothing must be swept: {cleared:?}");
        let after_fut = get(&db, fut.id).unwrap();
        assert!(!after_fut.schedulable, "future deadline must stay drained");
        let after_none = get(&db, none.id).unwrap();
        assert!(!after_none.schedulable, "no deadline must stay drained");
        assert!(events(&db, fut.id, 10).unwrap().is_empty());
        assert!(events(&db, none.id, 10).unwrap().is_empty());
    }

    #[test]
    fn replaced_drain_with_fresh_future_deadline_is_not_swept() {
        let db = test_db();
        let n = online_node_with_limits(&db, "node-repl", 8192, 16384);
        let past = (Utc::now() - Duration::minutes(5)).to_rfc3339();
        set_drain(&db, n.id, "hold", "old", Some(&past)).unwrap();
        // The operator replaces the drain with a fresh future deadline before
        // the sweep runs; the stale past deadline must not linger.
        clear_drain(&db, n.id).unwrap();
        let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
        set_drain(&db, n.id, "stop", "new", Some(&future)).unwrap();

        let cleared = clear_expired_drains(&db, &Utc::now().to_rfc3339()).unwrap();
        assert!(cleared.is_empty(), "a fresh future deadline must not be swept");
        let after = get(&db, n.id).unwrap();
        assert!(!after.schedulable, "the replacement drain must stay active");
        assert_eq!(after.drain_mode, "stop");
        assert_eq!(after.drain_deadline.as_deref(), Some(future.as_str()));
    }

    #[test]
    fn placement_refuses_drained_nodes() {
        let db = test_db();
        let a = online_node_with_limits(&db, "node-a", 8192, 16384);
        let b = online_node_with_limits(&db, "node-b", 8192, 16384);

        // Both schedulable: a pick succeeds (either candidate).
        let picked = select_for_server(&db, 1000, 1024, &[], None).unwrap();
        assert!(picked.id == a.id || picked.id == b.id);

        // Draining one node must not silently place onto it: the other is
        // chosen instead.
        set_drain(&db, a.id, "hold", "maintenance", None).unwrap();
        let picked = select_for_server(&db, 1000, 1024, &[], None).unwrap();
        assert_eq!(picked.id, b.id, "placement must skip a drained node");

        // Draining the last candidate leaves no placement at all, with a
        // reason that names the drain rule — and the real provisioning path
        // refuses identically.
        set_drain(&db, b.id, "stop", "reboot", None).unwrap();
        let err = select_for_server(&db, 1000, 1024, &[], None).unwrap_err();
        assert!(
            err.to_string().contains("drain"),
            "placement error must name the drain rule: {err}"
        );
        let mut conn = db.get().unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let err = reserve_capacity_tx(&tx, 1000, 1024, &[], None).unwrap_err();
        assert!(err.to_string().contains("drain"));
        drop(tx);
    }
}