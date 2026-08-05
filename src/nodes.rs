//! Multi-node persistence models and scheduling decisions.
use crate::db::Db;
use crate::node_protocol::{NodeCapacity, NodeHeartbeat};
use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

fn now() -> String {
    Utc::now().to_rfc3339()
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
}

impl Node {
    pub fn online(&self) -> bool {
        if !self.enabled || !self.enrolled {
            return false;
        }
        self.last_heartbeat
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.with_timezone(&Utc) > Utc::now() - Duration::seconds(45))
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
    })
}

const COLS: &str = "id,uuid,name,public_url,secret,enrollment_token,enrolled,enabled,maintenance,schedulable,location,tags,memory_limit_mb,disk_limit_mb,cpu_limit_percent,memory_overallocate,disk_overallocate,daemon_version,hostname,os,arch,capacity_json,last_heartbeat,last_error,created_at,updated_at";

pub fn create(
    db: &Db,
    name: &str,
    public_url: &str,
    location: &str,
    tags: &[String],
) -> Result<Node> {
    let uuid = uuid::Uuid::new_v4().to_string();
    let secret = crate::auth::random_token(48);
    let enrollment = crate::auth::random_token(32);
    let t = now();
    let conn = db.lock();
    conn.execute(
        "INSERT INTO nodes(uuid,name,public_url,secret,enrollment_token,location,tags,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)",
        params![uuid, name, public_url.trim_end_matches('/'), secret, enrollment, location, serde_json::to_string(tags)?, t],
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
    let conn = db.lock();
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM nodes ORDER BY name"))?;
    let rows = stmt.query_map([], from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(db: &Db, id: i64) -> Result<Node> {
    let conn = db.lock();
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE id=?1"),
        [id],
        from_row,
    )
    .context("node not found")
}

pub fn get_by_uuid(db: &Db, uuid: &str) -> Result<Node> {
    let conn = db.lock();
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE uuid=?1"),
        [uuid],
        from_row,
    )
    .context("node not found")
}

pub fn get_by_name(db: &Db, name: &str) -> Result<Node> {
    let conn = db.lock();
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE name=?1"),
        [name],
        from_row,
    )
    .context("node not found")
}

pub fn enroll(db: &Db, token: &str, heartbeat: &NodeHeartbeat) -> Result<Node> {
    let conn = db.lock();
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM nodes WHERE enrollment_token=?1 AND enrolled=0",
            [token],
            |r| r.get(0),
        )
        .optional()?;
    let id = id.context("invalid or used enrollment token")?;
    let t = now();
    conn.execute(
        "UPDATE nodes SET enrolled=1,enrollment_token=NULL,daemon_version=?1,hostname=?2,os=?3,arch=?4,capacity_json=?5,last_heartbeat=?6,last_error='',updated_at=?6 WHERE id=?7",
        params![heartbeat.daemon_version, heartbeat.hostname, heartbeat.os, heartbeat.arch, serde_json::to_string(&heartbeat.capacity)?, t, id],
    )?;
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE id=?1"),
        [id],
        from_row,
    )
    .map_err(Into::into)
}

pub fn heartbeat(db: &Db, uuid: &str, heartbeat: &NodeHeartbeat) -> Result<()> {
    let conn = db.lock();
    let t = now();
    let changed = conn.execute(
        "UPDATE nodes SET daemon_version=?1,hostname=?2,os=?3,arch=?4,capacity_json=?5,last_heartbeat=?6,last_error='',updated_at=?6 WHERE uuid=?7 AND enrolled=1",
        params![heartbeat.daemon_version, heartbeat.hostname, heartbeat.os, heartbeat.arch, serde_json::to_string(&heartbeat.capacity)?, t, uuid],
    )?;
    if changed == 0 {
        bail!("node not enrolled");
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
    let conn = db.lock();
    conn.execute(
        "UPDATE nodes SET name=?1,public_url=?2,enabled=?3,maintenance=?4,schedulable=?5,location=?6,tags=?7,memory_limit_mb=?8,disk_limit_mb=?9,memory_overallocate=?10,disk_overallocate=?11,updated_at=?12 WHERE id=?13",
        params![name, public_url.trim_end_matches('/'), enabled as i64, maintenance as i64, schedulable as i64, location, serde_json::to_string(tags)?, memory_limit_mb, disk_limit_mb, memory_overallocate, disk_overallocate, now(), id],
    )?;
    conn.query_row(
        &format!("SELECT {COLS} FROM nodes WHERE id=?1"),
        [id],
        from_row,
    )
    .map_err(Into::into)
}

pub fn rotate_secret(db: &Db, id: i64) -> Result<String> {
    let secret = crate::auth::random_token(48);
    let conn = db.lock();
    conn.execute(
        "UPDATE nodes SET secret=?1,updated_at=?2 WHERE id=?3",
        params![secret, now(), id],
    )?;
    Ok(secret)
}

pub fn regenerate_enrollment(db: &Db, id: i64) -> Result<String> {
    let token = crate::auth::random_token(32);
    let conn = db.lock();
    conn.execute("UPDATE nodes SET enrollment_token=?1,enrolled=0,last_heartbeat=NULL,updated_at=?2 WHERE id=?3", params![token, now(), id])?;
    Ok(token)
}

pub fn set_error(db: &Db, uuid: &str, error: &str) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE nodes SET last_error=?1,updated_at=?2 WHERE uuid=?3",
        params![error, now(), uuid],
    )?;
    Ok(())
}

pub fn delete(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    let assigned: i64 = conn.query_row("SELECT COUNT(*) FROM servers WHERE node=(SELECT name FROM nodes WHERE id=?1) AND deleted=0", [id], |r| r.get(0))?;
    if assigned > 0 {
        bail!("node has {assigned} assigned server(s)");
    }
    conn.execute("DELETE FROM nodes WHERE id=?1", [id])?;
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
    let conn = db.lock();
    conn.execute(
        "INSERT INTO node_events(node_id,level,kind,message,payload,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
        params![node_id, level, kind, message, serde_json::to_string(payload)?, now()],
    )?;
    Ok(())
}

pub fn events(db: &Db, node_id: i64, limit: i64) -> Result<Vec<serde_json::Value>> {
    let conn = db.lock();
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

pub fn select_for_server(
    db: &Db,
    memory_mb: i64,
    disk_mb: i64,
    required_tags: &[String],
    location: Option<&str>,
) -> Result<Node> {
    let mut candidates: Vec<Node> = list(db)?
        .into_iter()
        .filter(|n| {
            n.online()
                && n.enabled
                && n.schedulable
                && !n.maintenance
                && location
                    .map(|loc| loc.is_empty() || n.location == loc)
                    .unwrap_or(true)
                && required_tags
                    .iter()
                    .all(|tag| n.tags.iter().any(|t| t == tag))
                && n.available_memory_mb() >= memory_mb
                && n.available_disk_mb() >= disk_mb
        })
        .collect();
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
        .context("no online node has enough capacity")
}

pub fn port_available_on_node(db: &Db, node: &str, port: i64) -> Result<bool> {
    let conn = db.lock();
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM servers WHERE node=?1 AND port=?2 AND deleted=0",
        params![node, port],
        |r| r.get(0),
    )?;
    Ok(used == 0)
}

pub fn next_free_port_on_node(
    db: &Db,
    node: &str,
    range_start: i64,
    range_end: i64,
) -> Result<i64> {
    for port in range_start..=range_end {
        if port_available_on_node(db, node, port)? {
            return Ok(port);
        }
    }
    bail!("no free port in range {range_start}-{range_end} on node {node}")
}
