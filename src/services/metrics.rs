//! Telemetry: per-server resource time-series sampling, retention, and rollups.
use crate::db::Db;
use crate::services::proc::ProcManager;
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub ts: i64,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
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
    let conn = db.lock();
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
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT ts, cpu_percent, memory_bytes, disk_bytes, rx_bytes, tx_bytes
         FROM server_metrics
         WHERE server_id=?1 AND ts>=?2 AND ts<=?3
         ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![server_id, since_ts, until_ts], |r| {
        Ok(Sample {
            ts: r.get(0)?,
            cpu_percent: r.get(1)?,
            memory_bytes: r.get(2)?,
            disk_bytes: r.get(3)?,
            rx_bytes: r.get(4)?,
            tx_bytes: r.get(5)?,
        })
    })?;
    let mut out: Vec<Sample> = rows.collect::<std::result::Result<_, _>>()?;
    if out.len() > max_points {
        out = downsample(&out, max_points);
    }
    Ok(out)
}

/// Average samples into at most `max_points` equal-width time buckets over the
/// series span. Buckets with no samples are dropped — never emit zero rows.
fn downsample(samples: &[Sample], max_points: usize) -> Vec<Sample> {
    let n = samples.len();
    if n <= max_points || max_points == 0 {
        return samples.to_vec();
    }
    let first = samples[0].ts;
    let span = (samples[n - 1].ts - first).max(1) as f64;
    let mut acc: Vec<(f64, u64, u64, u64, u64, i64, usize)> =
        vec![(0.0, 0, 0, 0, 0, 0, 0); max_points];
    for s in samples {
        let b = (((s.ts - first) as f64 / span) * max_points as f64) as usize;
        let e = &mut acc[b.min(max_points - 1)];
        e.0 += s.cpu_percent;
        e.1 += s.memory_bytes;
        e.2 += s.disk_bytes;
        e.3 += s.rx_bytes;
        e.4 += s.tx_bytes;
        e.5 = s.ts;
        e.6 += 1;
    }
    let mut out = Vec::with_capacity(max_points);
    for (cpu, mem, disk, rx, tx, ts, cnt) in acc {
        if cnt == 0 {
            continue;
        }
        out.push(Sample {
            ts,
            cpu_percent: cpu / cnt as f64,
            memory_bytes: mem / cnt as u64,
            disk_bytes: disk / cnt as u64,
            rx_bytes: rx / cnt as u64,
            tx_bytes: tx / cnt as u64,
        });
    }
    out
}

pub fn summary(db: &Db, server_id: i64, since_ts: i64) -> Result<Summary> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT cpu_percent, memory_bytes, disk_bytes, rx_bytes, tx_bytes
         FROM server_metrics
         WHERE server_id=?1 AND ts>=?2
         ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![server_id, since_ts], |r| {
        Ok((
            r.get::<_, f64>(0)?,
            r.get::<_, u64>(1)?,
            r.get::<_, u64>(2)?,
            r.get::<_, u64>(3)?,
            r.get::<_, u64>(4)?,
        ))
    })?;
    let mut cpu_sum = 0.0;
    let mut cpu_peak = 0.0;
    let mut mem_sum: u64 = 0;
    let mut mem_peak: u64 = 0;
    let mut disk_peak: u64 = 0;
    let mut rx_min = u64::MAX;
    let mut rx_max: u64 = 0;
    let mut tx_min = u64::MAX;
    let mut tx_max: u64 = 0;
    let mut count: usize = 0;
    for row in rows {
        let (cpu, mem, disk, rx, tx) = row?;
        cpu_sum += cpu;
        if cpu > cpu_peak {
            cpu_peak = cpu;
        }
        mem_sum += mem;
        if mem > mem_peak {
            mem_peak = mem;
        }
        if disk > disk_peak {
            disk_peak = disk;
        }
        if rx < rx_min {
            rx_min = rx;
        }
        if rx > rx_max {
            rx_max = rx;
        }
        if tx < tx_min {
            tx_min = tx;
        }
        if tx > tx_max {
            tx_max = tx;
        }
        count += 1;
    }
    Ok(Summary {
        cpu_avg: if count > 0 {
            cpu_sum / count as f64
        } else {
            0.0
        },
        cpu_peak,
        memory_avg: if count > 0 {
            mem_sum / count as u64
        } else {
            0
        },
        memory_peak: mem_peak,
        disk_peak,
        // rx/tx are cumulative counters: total transferred = MAX - MIN (a reset
        // mid-window would make naive last-first undercount).
        rx_total: if count >= 2 {
            rx_max.saturating_sub(rx_min)
        } else {
            0
        },
        tx_total: if count >= 2 {
            tx_max.saturating_sub(tx_min)
        } else {
            0
        },
        samples: count,
    })
}

pub fn prune(db: &Db, older_than_ts: i64) -> Result<usize> {
    let conn = db.lock();
    let n = conn.execute("DELETE FROM server_metrics WHERE ts < ?1", [older_than_ts])?;
    Ok(n)
}

/// Snapshot every server whose process is currently alive and persist one row.
pub fn sample_running(db: &Db, procs: &ProcManager) -> Result<usize> {
    let servers = crate::models::list_servers(db, None, false)?;
    let now = chrono::Utc::now().timestamp();
    let mut recorded = 0;
    for server in servers {
        let info = procs.info(&server);
        if info.pid.is_none() {
            continue; // offline — nothing to sample
        }
        let s = Sample {
            ts: now,
            cpu_percent: info.cpu_percent,
            memory_bytes: info.memory_bytes,
            disk_bytes: info.disk_usage_bytes,
            rx_bytes: info.bandwidth_rx_bytes,
            tx_bytes: info.bandwidth_tx_bytes,
        };
        record(db, server.id, &s)?;
        recorded += 1;
    }
    Ok(recorded)
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
            let conn = db.lock();
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
    fn summary_rx_tx_is_max_min() {
        let db = TestDb::new();
        // Counter reset mid-window (process restart): naive last-first would
        // undercount, MAX-MIN must not.
        for (ts, rx, tx) in [(1, 100, 1000), (2, 150, 1500), (3, 50, 500), (4, 90, 900)] {
            record(&db.db, db.sid, &s(ts, 10.0, 64, 128, rx, tx)).unwrap();
        }
        let sum = summary(&db.db, db.sid, 0).unwrap();
        assert_eq!(sum.rx_total, 150 - 50);
        assert_eq!(sum.tx_total, 1500 - 500);
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
            record(&db.db, db.sid, &s(i, i as f64, i as u64, i as u64, i as u64, i as u64)).unwrap();
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
}
