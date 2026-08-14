//! Telemetry: per-server resource time-series sampling, retention, and rollups.
use crate::db::Db;
use crate::services::proc::ProcManager;
use crate::services::webhooks;
use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub ts: i64,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

impl Sample {
    /// Map an agent's `RemoteServerStats` onto a telemetry row. Network
    /// counters are cumulative (mirroring the local `ProcessInfo` sampling);
    /// cpu/mem/disk are instantaneous gauges.
    pub fn from_remote(ts: i64, s: &crate::node_protocol::RemoteServerStats) -> Self {
        Self {
            ts,
            cpu_percent: s.cpu_percent,
            memory_bytes: s.memory_bytes,
            disk_bytes: s.disk_bytes,
            rx_bytes: s.network_rx_bytes,
            tx_bytes: s.network_tx_bytes,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub cpu_avg: f64,
    pub cpu_peak: f64,
    pub memory_avg: u64,
    pub memory_peak: u64,
    pub disk_peak: u64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub samples: usize,
}

pub fn record(db: &Db, server_id: i64, s: &Sample) -> Result<()> {
    let conn = db.get()?;
    conn.execute(
        "INSERT OR REPLACE INTO server_metrics
         (server_id, ts, cpu_percent, memory_bytes, disk_bytes, rx_bytes, tx_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            server_id,
            s.ts,
            s.cpu_percent,
            s.memory_bytes,
            s.disk_bytes,
            s.rx_bytes,
            s.tx_bytes
        ],
    )?;
    Ok(())
}

pub fn range(
    db: &Db,
    server_id: i64,
    since_ts: i64,
    until_ts: i64,
    max_points: usize,
) -> Result<Vec<Sample>> {
    if until_ts < since_ts {
        return Ok(Vec::new());
    }
    let max_points = max_points.max(1);
    // Bucket the window into at most `max_points` equal-width slots entirely
    // in SQL: one aggregate scan returns at most one row per populated bucket
    // instead of hauling every raw row into Rust to downsample. Empty buckets
    // emit no rows (GROUP BY only visits populated buckets), matching the old
    // `downsample` contract. `bucket` is at least 1 so a degenerate window
    // (span 0) collapses into a single row instead of dividing by zero.
    let span = until_ts - since_ts;
    // ceil(span/max_points) without overflow: span can be i64::MAX (the
    // test "all history" window), so add AFTER dividing, never before.
    let bucket = if span % max_points as i64 == 0 {
        span / max_points as i64
    } else {
        span / max_points as i64 + 1
    };
    let bucket = bucket.max(1);
    let conn = db.get()?;
    // rx/tx are monotonic cumulative counters, not gauges: per bucket the
    // meaningful value is the counter DELTA (last sample minus first), so a
    // chart shows traffic in the interval rather than a meaningless average
    // of absolute totals. A counter reset (process restart) inside the bucket
    // yields a negative delta here, clamped to 0 on read — it never drags a
    // bucket average down.
    let mut stmt = conn.prepare(
        "SELECT MIN(ts), AVG(cpu_percent), CAST(AVG(memory_bytes) AS INTEGER), \
         CAST(AVG(disk_bytes) AS INTEGER), MAX(drx), MAX(dtx) \
         FROM ( \
             SELECT ts, cpu_percent, memory_bytes, disk_bytes, \
                    LAST_VALUE(rx_bytes) OVER w - FIRST_VALUE(rx_bytes) OVER w AS drx, \
                    LAST_VALUE(tx_bytes) OVER w - FIRST_VALUE(tx_bytes) OVER w AS dtx, \
                    b \
             FROM ( \
                 SELECT ts, cpu_percent, memory_bytes, disk_bytes, rx_bytes, tx_bytes, \
                        (ts-?2)/?4 AS b \
                 FROM server_metrics \
                 WHERE server_id=?1 AND ts>=?2 AND ts<=?3 \
             ) \
             WINDOW w AS (PARTITION BY b ORDER BY ts \
                          ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
         ) \
         GROUP BY b \
         ORDER BY 1",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![server_id, since_ts, until_ts, bucket],
        |r| {
            Ok(Sample {
                ts: r.get(0)?,
                cpu_percent: r.get(1)?,
                memory_bytes: r.get(2)?,
                disk_bytes: r.get(3)?,
                rx_bytes: (r.get::<_, i64>(4)?.max(0)) as u64,
                tx_bytes: (r.get::<_, i64>(5)?.max(0)) as u64,
            })
        },
    )?;
    let mut out: Vec<Sample> = rows.collect::<std::result::Result<_, _>>()?;
    // When the span divides exactly, GROUP BY can emit max_points+1 buckets;
    // the Rust pass is the final cap on the response size.
    if out.len() > max_points {
        out = downsample(&out, max_points);
    }
    Ok(out)
}

/// Running per-bucket aggregation for `downsample`: gauge sums (averaged on
/// emit) plus first/last cumulative-counter readings (emitted as a delta).
/// A struct instead of a positional tuple keeps clippy's type_complexity gate
/// happy and the field meanings obvious.
struct BucketAcc {
    cpu: f64,
    mem: u64,
    disk: u64,
    rx_first: u64,
    rx_last: u64,
    tx_first: u64,
    tx_last: u64,
    ts: i64,
    count: usize,
}

/// Average samples into at most `max_points` equal-width time buckets over the
/// series span. Buckets with no samples are dropped — never emit zero rows.
/// cpu/mem/disk are gauges (averaged per bucket); rx/tx are cumulative
/// counters, so a bucket keeps its first and last readings and emits the
/// delta, mirroring the SQL aggregation in `range`.
fn downsample(samples: &[Sample], max_points: usize) -> Vec<Sample> {
    let n = samples.len();
    if n <= max_points || max_points == 0 {
        return samples.to_vec();
    }
    let first = samples[0].ts;
    let span = (samples[n - 1].ts - first).max(1) as f64;
    let mut acc: Vec<BucketAcc> = (0..max_points)
        .map(|_| BucketAcc {
            cpu: 0.0,
            mem: 0,
            disk: 0,
            rx_first: 0,
            rx_last: 0,
            tx_first: 0,
            tx_last: 0,
            ts: 0,
            count: 0,
        })
        .collect();
    for s in samples {
        let b = (((s.ts - first) as f64 / span) * max_points as f64) as usize;
        let e = &mut acc[b.min(max_points - 1)];
        e.cpu += s.cpu_percent;
        e.mem += s.memory_bytes;
        e.disk += s.disk_bytes;
        if e.count == 0 {
            e.rx_first = s.rx_bytes;
            e.tx_first = s.tx_bytes;
        }
        e.rx_last = s.rx_bytes;
        e.tx_last = s.tx_bytes;
        e.ts = s.ts;
        e.count += 1;
    }
    let mut out = Vec::with_capacity(max_points);
    for b in acc {
        if b.count == 0 {
            continue;
        }
        out.push(Sample {
            ts: b.ts,
            cpu_percent: b.cpu / b.count as f64,
            memory_bytes: b.mem / b.count as u64,
            disk_bytes: b.disk / b.count as u64,
            rx_bytes: b.rx_last.saturating_sub(b.rx_first),
            tx_bytes: b.tx_last.saturating_sub(b.tx_first),
        });
    }
    out
}

/// Total for a monotonic cumulative counter that may reset to a lower value
/// (e.g. a process restart): each monotonic segment contributes
/// `segment_end - segment_start`, and the segment sums survive resets.
/// `values` must be in chronological order.
fn counter_total(values: &[u64]) -> u64 {
    let mut total = 0u64;
    let mut segment_start: Option<u64> = None;
    let mut prev: Option<u64> = None;
    for &v in values {
        if let (Some(p), Some(start)) = (prev, segment_start) {
            if v < p {
                // Counter reset: close the previous segment, start a new one.
                total = total.saturating_add(p.saturating_sub(start));
                segment_start = Some(v);
            }
        } else {
            segment_start = Some(v);
        }
        prev = Some(v);
    }
    if let (Some(p), Some(start)) = (prev, segment_start) {
        total = total.saturating_add(p.saturating_sub(start));
    }
    total
}

    // One-off aggregate row; a named alias keeps clippy's type_complexity gate
    // happy without a bespoke struct for a single query.
    type Agg = (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<u64>,
        Option<u64>,
        i64,
    );
pub fn summary(db: &Db, server_id: i64, since_ts: i64) -> Result<Summary> {
    let conn = db.get()?;
    // avg/peak/count in one aggregate row — no full scan into Rust.
    let (cpu_avg, cpu_peak, mem_avg, mem_peak, disk_peak, count): Agg = conn.query_row(
        "SELECT AVG(cpu_percent), MAX(cpu_percent), AVG(memory_bytes), \
         MAX(memory_bytes), MAX(disk_bytes), COUNT(*) \
         FROM server_metrics WHERE server_id=?1 AND ts>=?2",
        rusqlite::params![server_id, since_ts],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    // The counter totals need every reading in chronological order to detect
    // resets (the Rust `counter_total` oracle); only the two counter columns
    // are hauled out, not the full 6-column row.
    let mut stmt = conn.prepare(
        "SELECT rx_bytes, tx_bytes FROM server_metrics \
         WHERE server_id=?1 AND ts>=?2 ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![server_id, since_ts], |r| {
        Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?))
    })?;
    let mut rxs: Vec<u64> = Vec::new();
    let mut txs: Vec<u64> = Vec::new();
    for row in rows {
        let (rx, tx) = row?;
        rxs.push(rx);
        txs.push(tx);
    }
    Ok(Summary {
        cpu_avg: cpu_avg.unwrap_or(0.0),
        cpu_peak: cpu_peak.unwrap_or(0.0),
        memory_avg: mem_avg.unwrap_or(0.0) as u64,
        memory_peak: mem_peak.unwrap_or(0),
        disk_peak: disk_peak.unwrap_or(0),
        rx_total: counter_total(&rxs),
        tx_total: counter_total(&txs),
        samples: count as usize,
    })
}

pub fn prune(db: &Db, older_than_ts: i64) -> Result<usize> {
    let conn = db.get()?;
    // Bounded batches: each DELETE (and its write lock) touches at most 5000
    // rows via the `ts` index, so a large backlog never stalls the sampler
    // insert with one unbounded statement.
    let mut total = 0usize;
    loop {
        let n = conn.execute(
            "DELETE FROM server_metrics \
             WHERE ts < ?1 \
               AND rowid IN (SELECT rowid FROM server_metrics WHERE ts < ?1 LIMIT 5000)",
            [older_than_ts],
        )?;
        total += n;
        if n == 0 {
            return Ok(total);
        }
    }
}

/// Snapshot every server whose process is currently alive and persist one row.
pub fn sample_running(db: &Db, procs: &ProcManager) -> Result<usize> {
    let servers = crate::models::list_servers(db, None, false)?;
    let now = chrono::Utc::now().timestamp();
    // One pool checkout, one transaction, one prepared statement for the whole
    // tick: per-row errors are isolated (warn + keep going) and the commit
    // persists only the successful rows.
    let mut conn = db.get()?;
    let tx = conn.transaction()?;
    let mut recorded = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO server_metrics \
             (server_id, ts, cpu_percent, memory_bytes, disk_bytes, rx_bytes, tx_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for server in servers {
            // Cheap running-set probe (DashMap + pid mutex) before the
            // expensive `info()` (fs_usage + /proc sampling). The state entry
            // may still vanish between probe and info, so the pid check after
            // is kept as the authoritative gate.
            let live = match procs.state(server.id) {
                Some(ps) => ps.pid.lock().is_some(),
                None => false,
            };
            if !live {
                continue;
            }
            let info = procs.info(&server);
            if info.pid.is_none() {
                continue; // died between the probe and the snapshot
            }
            let s = Sample {
                ts: now,
                cpu_percent: info.cpu_percent,
                memory_bytes: info.memory_bytes,
                disk_bytes: info.disk_usage_bytes,
                rx_bytes: info.bandwidth_rx_bytes,
                tx_bytes: info.bandwidth_tx_bytes,
            };
            if let Err(e) = stmt.execute(rusqlite::params![
                server.id,
                s.ts,
                s.cpu_percent,
                s.memory_bytes,
                s.disk_bytes,
                s.rx_bytes,
                s.tx_bytes
            ]) {
                tracing::warn!("metrics sample server {}: {e}", server.id);
                continue;
            }
            recorded += 1;
        }
    }
    tx.commit()?;
    Ok(recorded)
}
// ---------------- Panel self-metrics (Observatory) ----------------
//
// Pterodactyl's observability surface is server-only; the panel reports
// nothing about itself. The Observatory view's self-metrics are VoltPanel's
// own: process uptime + request counters (fed by the router middleware in
// main.rs), DB pool state, scheduler/webhook backlogs, and offsite-mirror
// health. All DB-shaped reads run on one pooled connection via the
// Db::call/blocking contract — never on a Tokio worker.

#[derive(Debug, Clone, Serialize)]
pub struct PanelSelfMetrics {
    pub uptime_secs: u64,
    pub pool: PoolMetrics,
    pub requests: RequestMetrics,
    pub scheduler: SchedulerMetrics,
    pub webhooks: WebhookMetrics,
    pub mirror: MirrorMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolMetrics {
    pub connections: u32,
    pub idle: u32,
    pub max: u32,
    /// Whether the pool has reached its connection cap (connections >= max).
    pub saturated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestMetrics {
    pub total: u64,
    pub ok: u64,
    pub client_err: u64,
    pub server_err: u64,
    pub since_unix: i64,
    /// Requests per minute for the last 10 minutes, oldest first.
    pub per_minute: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchedulerMetrics {
    pub pending_runs: i64,
    /// Unix seconds of the scheduler's last tick, fed by the process-global
    /// `scheduler::LAST_TICK` clock; `None` (field omitted) until the first
    /// tick has run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tick_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookMetrics {
    pub pending_deliveries: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MirrorMetrics {
    pub status: &'static str,
}

/// Snapshot the panel's own health for GET /api/metrics/panel.
pub fn panel_self_metrics(db: &Db, cfg: &crate::config::Config) -> Result<PanelSelfMetrics> {
    let counters = crate::REQUEST_COUNTERS.snapshot();
    let pool = db.state();
    let conn = db.get()?;
    let pending_runs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schedule_runs WHERE status='pending'",
        [],
        |r| r.get(0),
    )?;
    let pending_deliveries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE status='pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(PanelSelfMetrics {
        uptime_secs: counters.uptime_secs,
        pool: PoolMetrics {
            connections: pool.connections,
            idle: pool.idle_connections,
            max: db.max_size(),
            saturated: pool.connections >= db.max_size(),
        },
        requests: RequestMetrics {
            total: counters.total,
            ok: counters.ok,
            client_err: counters.client_err,
            server_err: counters.server_err,
            since_unix: counters.since_unix,
            per_minute: counters.per_minute,
        },
        scheduler: SchedulerMetrics {
            pending_runs,
            last_tick_at: {
                let tick = crate::services::scheduler::LAST_TICK.load(Ordering::Relaxed);
                if tick > 0 { Some(tick) } else { None }
            },
        },
        webhooks: WebhookMetrics { pending_deliveries },
        mirror: MirrorMetrics {
            status: crate::services::backups::mirror_status(cfg),
        },
    })
}
// ---------------- Panel health alerts (Observatory) ----------------
//
// Beyond reporting, the panel watches its own subsystems for conditions
// that need operator attention and surfaces them as `panel.alert` webhook
// events (edge-triggered: emitted once when a condition starts, silent
// while it persists, and re-armed after it recovers). Recovery is NOT
// emitted as an event — the operator sees the condition clear by the
// alert's absence. No existing panel does this; it is VoltPanel's own
// self-observability surface.

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PanelAlert {
    /// Alert kind: `pool.saturated`, `mirror.degraded`,
    /// `webhooks.backlog`, or `schedules.backlog`.
    pub kind: String,
    /// Whether the condition holds on this evaluation.
    pub active: bool,
}

/// Evaluate the panel's own health. Every call returns all four kinds with
/// their current `active` flags (deterministic order), so consumers can
/// diff against their previous view. Uses the same pooled-connection
/// pattern as `panel_self_metrics`: pool state from the shared state
/// handle, backlog counts from one pooled connection.
pub fn panel_health(db: &Db, cfg: &crate::config::Config) -> Result<Vec<PanelAlert>> {
    let pool = db.state();
    let conn = db.get()?;
    let pending_runs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schedule_runs WHERE status='pending'",
        [],
        |r| r.get(0),
    )?;
    let pending_deliveries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE status='pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(vec![
        PanelAlert {
            kind: "pool.saturated".to_string(),
            active: pool.connections >= db.max_size(),
        },
        PanelAlert {
            kind: "mirror.degraded".to_string(),
            active: crate::services::backups::mirror_status(cfg) == "degraded",
        },
        PanelAlert {
            kind: "webhooks.backlog".to_string(),
            active: pending_deliveries > 50,
        },
        PanelAlert {
            kind: "schedules.backlog".to_string(),
            active: pending_runs > 10,
        },
    ])
}

/// Edge-trigger core: which kinds are active in `current` but were not in
/// `previous`. Deterministic — follows `current`'s declaration order.
pub fn panel_alert_transitions(previous: &HashSet<String>, current: &[PanelAlert]) -> Vec<String> {
    current
        .iter()
        .filter(|a| a.active && !previous.contains(&a.kind))
        .map(|a| a.kind.clone())
        .collect()
}

/// Fold kinds whose emission actually enqueued a delivery: `enqueued`
/// returns the count reported by `webhooks::emit` for each kind, and a kind
/// with 0 enqueued deliveries (e.g. a transient DB failure) is left out so
/// the next tick re-attempts it. Returns the confirmed kinds, preserving
/// `newly`'s order.
fn confirmed_emits(newly: &[String], enqueued: impl Fn(&str) -> usize) -> Vec<String> {
    newly
        .iter()
        .filter(|kind| enqueued(kind.as_str()) > 0)
        .cloned()
        .collect()
}

/// Emit `panel.alert` for the newly-active kinds among `alerts` (global
/// scope, so global webhooks fire), then fold the current state into
/// `active`: recovered kinds are dropped from the set, which re-arms them.
/// Returns the kinds whose emission actually enqueued a delivery. A kind is
/// folded into `active` only when its `webhooks::emit` call enqueued > 0
/// deliveries — `emit` returns 0 on a transient DB failure, and marking the
/// kind active anyway would silently suppress the alert until it re-triggers.
/// A kind that enqueued 0 stays out of the set so the next tick re-attempts.
fn emit_panel_alert_transitions(
    db: &Db,
    active: &mut HashSet<String>,
    alerts: &[PanelAlert],
) -> Vec<String> {
    let newly = panel_alert_transitions(active, alerts);
    let still_active: HashSet<String> = alerts
        .iter()
        .filter(|a| a.active)
        .map(|a| a.kind.clone())
        .collect();
    active.retain(|k| still_active.contains(k));
    if newly.is_empty() {
        return newly;
    }
    let timestamp = Utc::now().to_rfc3339();
    let emitted = confirmed_emits(&newly, |kind| {
        webhooks::emit(
            db,
            "panel.alert",
            None,
            json!({
                "event": "panel.alert",
                "kind": kind,
                "timestamp": timestamp,
                "server_id": null,
            }),
        )
    });
    for kind in &emitted {
        active.insert(kind.clone());
    }
    emitted
}

/// Evaluate the panel's health and emit for any newly-active kinds. On an
/// evaluation error nothing is emitted and the active set is left untouched
/// (a transient failure neither clears nor re-arms alerts); the next tick
/// reconciles.
pub fn emit_panel_alerts(
    db: &Db,
    cfg: &crate::config::Config,
    active: &mut HashSet<String>,
) -> Vec<String> {
    match panel_health(db, cfg) {
        Ok(alerts) => emit_panel_alert_transitions(db, active, &alerts),
        Err(e) => {
            tracing::warn!("panel health evaluation failed: {e:#}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ts: i64, cpu: f64, mem: u64, disk: u64, rx: u64, tx: u64) -> Sample {
        Sample {
            ts,
            cpu_percent: cpu,
            memory_bytes: mem,
            disk_bytes: disk,
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    static DB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TestDb {
        db: Db,
        path: std::path::PathBuf,
        sid: i64,
    }

    impl TestDb {
        fn new() -> Self {
            let seq = DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voltpanel-metrics-test-{}-{}.db",
                std::process::id(),
                seq
            ));
            let _ = std::fs::remove_file(&path);
            let db = crate::db::open(path.to_str().unwrap()).unwrap();
            // server_metrics FKs servers, servers FKs users + blueprints.
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at)
                 VALUES('t','t@t','x','now','now')",
                [],
            )
            .unwrap();
            let uid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO blueprints(uuid,name,created_at,updated_at)
                 VALUES('b','b','now','now')",
                [],
            )
            .unwrap();
            let bid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s','s',?1,?2,'now','now')",
                rusqlite::params![uid, bid],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            drop(conn);
            TestDb { db, path, sid }
        }
    }
    impl Drop for TestDb {
        fn drop(&mut self) {
            // `db` drops first (declaration order), closing the connection
            // before the files are unlinked.
            let wal = self.path.with_extension("db-wal");
            let shm = self.path.with_extension("db-shm");
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(wal);
            let _ = std::fs::remove_file(shm);
        }
    }

    #[test]
    fn downsample_caps_output_at_max_points() {
        let series: Vec<Sample> = (0..1000)
            .map(|i| s(i * 10, i as f64, i as u64, i as u64, i as u64, i as u64))
            .collect();
        let out = downsample(&series, 120);
        assert!(out.len() <= 120);
        assert!(!out.is_empty());
        // Averages stay inside the source value range.
        for p in &out {
            assert!(p.cpu_percent >= 0.0 && p.cpu_percent <= 999.0);
        }
        // ts stays within the original span.
        assert!(out.first().unwrap().ts >= 0);
        assert!(out.last().unwrap().ts <= 9990);
    }

    #[test]
    fn downsample_skips_empty_buckets() {
        // Two clusters far apart with max_points=10: the empty middle buckets
        // must not produce zero-filled rows.
        let mut series: Vec<Sample> = (0..5).map(|i| s(i, 10.0, 100, 1000, 1, 1)).collect();
        series.extend((1000..1005).map(|i| s(i, 50.0, 500, 5000, 2, 2)));
        let out = downsample(&series, 8);
        assert_eq!(out.len(), 2, "only the two populated buckets survive");
        for p in &out {
            assert!(p.cpu_percent == 10.0 || p.cpu_percent == 50.0);
            assert!(p.memory_bytes > 0);
        }
    }

    #[test]
    fn downsample_under_limit_is_identity() {
        let series: Vec<Sample> = (0..5).map(|i| s(i, 1.0, 2, 3, 4, 5)).collect();
        let out = downsample(&series, 10);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].cpu_percent, 1.0);
    }

    #[test]
    fn summary_sums_monotonic_segments_across_counter_resets() {
        let db = TestDb::new();
        for (ts, rx, tx) in [(1, 100, 1000), (2, 150, 1500), (3, 50, 500), (4, 90, 900)] {
            record(&db.db, db.sid, &s(ts, 10.0, 64, 128, rx, tx)).unwrap();
        }
        let sum = summary(&db.db, db.sid, 0).unwrap();
        assert_eq!(sum.rx_total, 90);
        assert_eq!(sum.tx_total, 900);
        assert_eq!(sum.samples, 4);
        assert_eq!(sum.cpu_avg, 10.0);
        assert_eq!(sum.memory_peak, 64);
        assert_eq!(sum.disk_peak, 128);
    }

    #[test]
    fn summary_empty_window_is_zeroes() {
        let db = TestDb::new();
        let sum = summary(&db.db, db.sid, 0).unwrap();
        assert_eq!(sum.samples, 0);
        assert_eq!(sum.cpu_avg, 0.0);
        assert_eq!(sum.rx_total, 0);
    }

    #[test]
    fn range_orders_asc_and_downsamples() {
        let db = TestDb::new();
        for i in 0..500 {
            record(
                &db.db,
                db.sid,
                &s(i, i as f64, i as u64, i as u64, i as u64, i as u64),
            )
            .unwrap();
        }
        let out = range(&db.db, db.sid, 0, 999, 120).unwrap();
        assert!(out.len() <= 120);
        assert!(out.windows(2).all(|w| w[0].ts <= w[1].ts));
        // Respects the time bounds.
        let few = range(&db.db, db.sid, 10, 20, 100).unwrap();
        assert!(few.iter().all(|p| p.ts >= 10 && p.ts <= 20));
    }

    #[test]
    fn prune_deletes_old_rows() {
        let db = TestDb::new();
        for i in 0..10 {
            record(&db.db, db.sid, &s(i, 1.0, 2, 3, 4, 5)).unwrap();
        }
        let n = prune(&db.db, 5).unwrap();
        assert_eq!(n, 5);
        let out = range(&db.db, db.sid, 0, 100, 1000).unwrap();
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|p| p.ts >= 5));
    }

    #[test]
    fn counter_total_pure_cases() {
        assert_eq!(
            counter_total(&[42]),
            0,
            "a single reading is a zero-length segment"
        );
        assert_eq!(counter_total(&[100, 150]), 50);
        // Reset mid-window: segments (100→150) + (50→90).
        assert_eq!(counter_total(&[100, 150, 50, 90]), 90);
        // Two resets: (10→20) + (5→15) + (0→12).
        assert_eq!(counter_total(&[10, 20, 5, 15, 0, 12]), 32);
        // Flat first segment then reset: (5→5) + (1→3).
        assert_eq!(counter_total(&[5, 5, 5, 1, 3]), 2);
    }

    #[test]
    fn range_clamps_inverted_bounds_and_zero_points() {
        let db = TestDb::new();
        for i in 0..50 {
            record(&db.db, db.sid, &s(i, 1.0, 2, 3, 4, 5)).unwrap();
        }
        // until < since: empty result, no error.
        assert!(range(&db.db, db.sid, 100, 10, 120).unwrap().is_empty());
        // max_points is clamped to at least 1: 50 rows collapse into one bucket.
        assert_eq!(range(&db.db, db.sid, 0, 100, 0).unwrap().len(), 1);
        assert_eq!(range(&db.db, db.sid, 0, 100, 1).unwrap().len(), 1);
    }

    #[test]
    fn range_boundaries_are_inclusive() {
        let db = TestDb::new();
        for ts in [5, 10, 15, 20, 25] {
            record(&db.db, db.sid, &s(ts, 1.0, 2, 3, 4, 5)).unwrap();
        }
        // Empty window: no rows, no error.
        assert!(range(&db.db, db.sid, 30, 40, 10).unwrap().is_empty());
        // [since, until] is inclusive: exactly 10, 15, 20, ascending.
        let out = range(&db.db, db.sid, 10, 20, 10).unwrap();
        let tss: Vec<i64> = out.iter().map(|p| p.ts).collect();
        assert_eq!(tss, vec![10, 15, 20]);
        // Degenerate single-point window still returns the boundary row.
        let one = range(&db.db, db.sid, 15, 15, 10).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].ts, 15);
    }

    #[test]
    fn summary_window_slice_excludes_pre_window_traffic() {
        let db = TestDb::new();
        // Pre-window segment 0->100 (ts 1..2), reset at ts=10, in-window 50->90.
        for (ts, rx) in [(1, 0), (2, 100), (10, 50), (11, 90)] {
            record(&db.db, db.sid, &s(ts, 1.0, 2, 3, rx, rx)).unwrap();
        }
        // Whole window: segments (0->100) + (50->90) = 100 + 40 = 140.
        let all = summary(&db.db, db.sid, 0).unwrap();
        assert_eq!(all.rx_total, 140);
        assert_eq!(all.tx_total, 140);
        // Window opened at the reset: only the post-reset segment (50->90)=40
        // counts; pre-window traffic must not leak into the total.
        let tail = summary(&db.db, db.sid, 5).unwrap();
        assert_eq!(tail.rx_total, 40);
        assert_eq!(tail.samples, 2);
        // Window opened mid-segment treats the first in-window reading as the
        // segment start: (100,50,90) -> 0 + (50->90) = 40.
        let mid = summary(&db.db, db.sid, 2).unwrap();
        assert_eq!(mid.rx_total, 40);
    }

    #[test]
    fn counter_total_saturates_and_handles_zero_resets() {
        // Reset to zero, flat at zero, then progress: 50 + 0 + 100.
        assert_eq!(counter_total(&[100, 150, 0, 0, 100]), 150);
        // Near-u64::MAX: plain wrapping/panicking arithmetic would differ.
        assert_eq!(counter_total(&[u64::MAX - 10, u64::MAX, 0, 5]), 15);
        // Reset with no progress after it: zero.
        assert_eq!(counter_total(&[u64::MAX, 0]), 0);
    }

    #[test]
    fn sample_running_records_live_servers() {
        let db = TestDb::new();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir =
            std::env::temp_dir().join(format!("voltpanel-metrics-logs-{}", std::process::id()));
        cfg.paths.datalab_dir =
            std::env::temp_dir().join(format!("voltpanel-metrics-datalab-{}", std::process::id()));
        let hub = std::sync::Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = crate::services::proc::ProcManager::new(db.db.clone(), hub, cfg.paths.datalab_dir.clone());
        procs.procs.insert(
            db.sid,
            std::sync::Arc::new(crate::services::proc::ProcessState {
                pid: parking_lot::Mutex::new(Some(std::process::id())),
                read_total: std::sync::atomic::AtomicU64::new(1000),
                write_total: std::sync::atomic::AtomicU64::new(2000),
                ..Default::default()
            }),
        );
        let n = sample_running(&db.db, &procs).unwrap();
        assert_eq!(n, 1, "exactly the one live server is sampled");
        let out = range(&db.db, db.sid, 0, i64::MAX, 10).unwrap();
        assert_eq!(out.len(), 1);
        // A single cumulative reading has no delta: with no prior sample the
        // per-bucket counter delta is 0 (gauges still round-trip through cpu).
        assert_eq!(out[0].rx_bytes, 0);
        assert_eq!(out[0].tx_bytes, 0);
    }

    #[test]
    fn range_reports_counter_deltas_not_averages() {
        let db = TestDb::new();
        // Monotonic cumulative counters across two buckets of two samples each.
        record(&db.db, db.sid, &s(1, 10.0, 64, 128, 100, 1000)).unwrap();
        record(&db.db, db.sid, &s(2, 10.0, 64, 128, 130, 1300)).unwrap();
        record(&db.db, db.sid, &s(3, 10.0, 64, 128, 140, 1400)).unwrap();
        record(&db.db, db.sid, &s(4, 10.0, 64, 128, 200, 2000)).unwrap();
        // span=3, max_points=2 -> bucket 2: ts 1,2 in bucket 0; ts 3,4 in bucket 1.
        let out = range(&db.db, db.sid, 1, 4, 2).unwrap();
        assert_eq!(out.len(), 2);
        let deltas: Vec<(u64, u64)> = out.iter().map(|p| (p.rx_bytes, p.tx_bytes)).collect();
        assert_eq!(deltas, vec![(30, 300), (60, 600)]);
    }

    #[test]
    fn range_clamps_counter_resets_within_a_bucket() {
        let db = TestDb::new();
        // Reset mid-window: the delta would be negative; it must clamp to 0
        // rather than producing a meaningless negative "traffic" reading.
        record(&db.db, db.sid, &s(1, 1.0, 2, 3, 100, 1000)).unwrap();
        record(&db.db, db.sid, &s(2, 1.0, 2, 3, 50, 500)).unwrap();
        let out = range(&db.db, db.sid, 1, 2, 1).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rx_bytes, 0);
        assert_eq!(out[0].tx_bytes, 0);
    }

    fn alert(kind: &str, active: bool) -> PanelAlert {
        PanelAlert {
            kind: kind.to_string(),
            active,
        }
    }

    /// Deliveries recorded in the webhook bus, in order.
    fn deliveries(db: &Db) -> Vec<(String, String)> {
        let conn = db.get().unwrap();
        let mut stmt = conn
            .prepare("SELECT event, payload FROM webhook_deliveries ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// A global webhook subscribed to `panel.*` so `panel.alert` matches.
    fn add_panel_webhook(db: &Db) {
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO webhooks(uuid,name,url,secret,events,enabled,created_at,updated_at)
             VALUES('panel-w','panel','http://panel.invalid','','[\"panel.*\"]',1,'now','now')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn panel_alert_transitions_are_edge_triggered() {
        // Nothing active -> the active kinds are transitions, in order.
        let mut prev = HashSet::new();
        let current = vec![alert("pool.saturated", true), alert("mirror.degraded", false)];
        let t = panel_alert_transitions(&prev, &current);
        assert_eq!(t, vec!["pool.saturated"]);
        prev.extend(t.iter().cloned());
        // Same state again: not re-emitted while still active.
        assert!(panel_alert_transitions(&prev, &current).is_empty());
        // One recovers, another activates: only the new one transitions.
        let shifted = vec![alert("pool.saturated", false), alert("mirror.degraded", true)];
        assert_eq!(
            panel_alert_transitions(&prev, &shifted),
            vec!["mirror.degraded"]
        );
        // Recovered kind is re-armed: coming back active transitions again.
        assert_eq!(
            panel_alert_transitions(&HashSet::new(), &shifted),
            vec!["mirror.degraded"]
        );
    }

    #[test]
    fn panel_alert_emit_fires_once_per_transition() {
        let db = TestDb::new();
        add_panel_webhook(&db.db);
        let mut active = HashSet::new();
        // Inactive -> active: emitted exactly once, and the set is armed.
        let emitted =
            emit_panel_alert_transitions(&db.db, &mut active, &[alert("pool.saturated", true)]);
        assert_eq!(emitted, vec!["pool.saturated"]);
        assert_eq!(deliveries(&db.db).len(), 1);
        // Still active on the next tick: nothing re-emitted.
        assert!(emit_panel_alert_transitions(&db.db, &mut active, &[alert("pool.saturated", true)])
            .is_empty());
        assert_eq!(deliveries(&db.db).len(), 1);
        // Recovery: no event, and the set clears (re-arming the kind).
        assert!(emit_panel_alert_transitions(&db.db, &mut active, &[alert("pool.saturated", false)])
            .is_empty());
        assert!(active.is_empty());
        assert_eq!(deliveries(&db.db).len(), 1);
        // Re-degrade after recovery: emitted again.
        assert_eq!(
            emit_panel_alert_transitions(&db.db, &mut active, &[alert("pool.saturated", true)]),
            vec!["pool.saturated"]
        );
        assert_eq!(deliveries(&db.db).len(), 2);
    }

    #[test]
    fn panel_alert_payload_is_global_and_carries_kind() {
        let db = TestDb::new();
        add_panel_webhook(&db.db);
        let mut active = HashSet::new();
        emit_panel_alert_transitions(
            &db.db,
            &mut active,
            &[alert("webhooks.backlog", true), alert("schedules.backlog", true)],
        );
        let rows = deliveries(&db.db);
        assert_eq!(rows.len(), 2, "each newly-active kind emits one event");
        for (event, payload) in rows {
            assert_eq!(event, "panel.alert");
            let p: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(p["event"], "panel.alert");
            assert!(p["kind"].is_string());
            assert!(p["timestamp"].is_string());
            assert!(
                p["server_id"].is_null(),
                "panel alerts are global: server_id must be null"
            );
        }
        let kinds: Vec<String> = deliveries(&db.db)
            .into_iter()
            .map(|(_, p)| serde_json::from_str::<serde_json::Value>(&p).unwrap()["kind"].to_string())
            .collect();
        assert!(kinds.contains(&"\"webhooks.backlog\"".to_string()));
        assert!(kinds.contains(&"\"schedules.backlog\"".to_string()));
    }

    #[test]
    fn confirmed_emits_skips_zero_enqueued_kinds() {
        // A kind whose emit enqueued nothing (transient DB failure) is not
        // confirmed: it must stay out of the active set so the next tick
        // re-attempts it rather than silently suppressing the alert.
        let newly = vec![
            "pool.saturated".to_string(),
            "webhooks.backlog".to_string(),
        ];
        let confirmed = confirmed_emits(&newly, |k| if k == "pool.saturated" { 0 } else { 2 });
        assert_eq!(confirmed, vec!["webhooks.backlog".to_string()]);
        // Order is preserved.
        let confirmed = confirmed_emits(&newly, |_| 1);
        assert_eq!(confirmed, newly);
        // All failing: nothing confirmed, everything retried next tick.
        assert!(confirmed_emits(&newly, |_| 0).is_empty());
        assert!(confirmed_emits(&[], |_| 0).is_empty());
    }

    #[test]
    fn panel_health_flags_backlogs_via_counts() {
        let db = TestDb::new();
        let cfg = crate::config::Config::default();
        let conn = db.db.get().unwrap();
        conn.execute(
            "INSERT INTO webhooks(uuid,name,url,secret,events,enabled,created_at,updated_at)
             VALUES('w1','w','http://x','','[\"*\"]',1,'now','now')",
            [],
        )
        .unwrap();
        let wid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at)
             VALUES(?1,'s','* * * * *',1,'now')",
            rusqlite::params![db.sid],
        )
        .unwrap();
        let sch = conn.last_insert_rowid();
        // 51 pending deliveries -> webhooks.backlog; 11 pending runs ->
        // schedules.backlog (>50 / >10 are the alert bounds).
        for _ in 0..51 {
            conn.execute(
                "INSERT INTO webhook_deliveries(webhook_id,event,payload,status,created_at)
                 VALUES(?1,'x','{}','pending','now')",
                rusqlite::params![wid],
            )
            .unwrap();
        }
        for _ in 0..11 {
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status)
                 VALUES(?1,'now','pending')",
                rusqlite::params![sch],
            )
            .unwrap();
        }
        drop(conn);
        let alerts = panel_health(&db.db, &cfg).unwrap();
        let flags: std::collections::HashMap<&str, bool> = alerts
            .iter()
            .map(|a| (a.kind.as_str(), a.active))
            .collect();
        assert!(flags["webhooks.backlog"]);
        assert!(flags["schedules.backlog"]);
        // The evaluation always reports all four kinds (value of
        // `pool.saturated` is environment-dependent — a test pool may be
        // fully warmed); mirror defaults to disabled (never degraded).
        let kinds: Vec<&str> = alerts.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["pool.saturated", "mirror.degraded", "webhooks.backlog", "schedules.backlog"]
        );
        assert!(!flags["mirror.degraded"]);
        // A terminal (delivered) row must not count toward the backlog:
        // only status='pending' does.
        let conn = db.db.get().unwrap();
        conn.execute("DELETE FROM webhook_deliveries", []).unwrap();
        conn.execute(
            "INSERT INTO webhook_deliveries(webhook_id,event,payload,status,created_at)
             VALUES(?1,'x','{}','delivered','now')",
            rusqlite::params![wid],
        )
        .unwrap();
        drop(conn);
        let after = panel_health(&db.db, &cfg).unwrap();
        let delivered_only = after
            .iter()
            .find(|a| a.kind == "webhooks.backlog")
            .unwrap()
            .active;
        assert!(!delivered_only, "terminal deliveries never back up the queue");
    }
}