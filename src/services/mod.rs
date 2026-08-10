//! Services layer: workloads, blueprints, console, storage, flows,
//! snapshots, data labs, web publishing, signals, and resource monitoring.
pub mod backups;
pub mod blueprint;
pub mod console;
pub mod databases;
pub mod files;
pub mod keys;
pub mod metrics;
pub mod node;
pub mod proc;
pub mod scheduler;
pub mod webhooks;
pub mod websites;

use crate::config::Config;
use crate::db::{blocking, Db};
use crate::models;
pub use console::ConsoleHub;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

pub use proc::{Notification, Notifier, ProcManager, ProcessInfo};

// Run a closure on Tokio's blocking pool, passing the pool itself as the
// argument so its body can call pool-based models without capturing a `Db`
// (which would force a per-closure move). Never unwrapped: a join failure
// surfaces as a `db worker failed` error. See `crate::db::blocking`.

// ---------------- Resource monitor (per-server limits + auto-kill) ----------------

#[derive(Clone, Debug)]
pub struct ServerLimits {
    pub server_id: i64,
    pub memory_mb: u64,
    pub cpu_percent: u64,
    pub bandwidth_rx: u64,
    pub bandwidth_tx: u64,
}

pub struct Monitor {
    limits: Mutex<HashMap<i64, ServerLimits>>,
    last_bandwidth: Mutex<HashMap<i64, (u64, u64)>>,
    overs: Mutex<HashMap<i64, u32>>,
    /// Shared per-server sample cache: refreshed by the 5s sweep, read by the
    /// API layer so list endpoints never walk disk/proc on a request path.
    samples: Mutex<HashMap<i64, (ProcessInfo, Instant)>>,
}
impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    pub fn new() -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            last_bandwidth: Mutex::new(HashMap::new()),
            overs: Mutex::new(HashMap::new()),
            samples: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_limit(&self, limits: ServerLimits) {
        self.limits.lock().insert(limits.server_id, limits);
    }

    pub fn remove_limit(&self, server_id: i64) {
        self.limits.lock().remove(&server_id);
        self.last_bandwidth.lock().remove(&server_id);
        self.overs.lock().remove(&server_id);
        self.samples.lock().remove(&server_id);
    }

    #[cfg(test)]
    pub fn limit_for_test(&self, server_id: i64) -> Option<ServerLimits> {
        self.limits.lock().get(&server_id).cloned()
    }

    /// Called every ~5s: refresh the shared per-server sample cache, then
    /// enforce memory/cpu/bandwidth caps, auto-kill + notify.
    ///
    /// One `list_servers` replaces the old per-limit `get_server` calls, and
    /// the `cached_info` pass reuses samples younger than 2s (an API fallback
    /// may have just walked that server) while refreshing everything else into
    /// the cache the list endpoint reads — blocking disk/proc walks never run
    /// on a request anymore.
    pub fn sweep(&self, db: &Db, procs: &ProcManager, notifier: &Notifier) {
        let limits = self.limits.lock().clone();
        let Ok(servers) = models::list_servers(db, None, false) else {
            return;
        };
        for server in &servers {
            let sid = server.id;
            let info = self.cached_info(procs, server, Duration::from_secs(2));
            let Some(lim) = limits.get(&sid) else {
                continue;
            };
            if info.status != "running" {
                continue;
            }
            let mut over = false;
            let mut reason = String::new();
            if lim.memory_mb > 0 && info.memory_bytes > lim.memory_mb * 1024 * 1024 {
                over = true;
                reason = format!(
                    "memory {}MB > {}MB",
                    info.memory_bytes / 1024 / 1024,
                    lim.memory_mb
                );
            }
            if lim.cpu_percent > 0 && info.cpu_percent > lim.cpu_percent as f64 {
                over = true;
                reason = format!("cpu {:.1}% > {}%", info.cpu_percent, lim.cpu_percent);
            }
            // bandwidth delta
            if lim.bandwidth_rx > 0 || lim.bandwidth_tx > 0 {
                let prev = self.last_bandwidth.lock().get(&sid).copied();
                let (prx, ptx) = prev.unwrap_or((info.bandwidth_rx_bytes, info.bandwidth_tx_bytes));
                let drx = info.bandwidth_rx_bytes.saturating_sub(prx);
                let dtx = info.bandwidth_tx_bytes.saturating_sub(ptx);
                self.last_bandwidth
                    .lock()
                    .insert(sid, (info.bandwidth_rx_bytes, info.bandwidth_tx_bytes));
                if lim.bandwidth_rx > 0 && drx > lim.bandwidth_rx {
                    over = true;
                    reason = format!("rx {}KB > {}KB", drx / 1024, lim.bandwidth_rx / 1024);
                }
                if lim.bandwidth_tx > 0 && dtx > lim.bandwidth_tx {
                    over = true;
                    reason = format!("tx {}KB > {}KB", dtx / 1024, lim.bandwidth_tx / 1024);
                }
            }
            if over {
                let n = *self.overs.lock().entry(sid).or_insert(0) + 1;
                self.overs.lock().insert(sid, n);
                notifier.notify(
                    "warn",
                    &format!("Server '{}' over limit", server.name),
                    &format!("{reason} (strike {n})"),
                    Some(sid),
                );
                if n >= 3 {
                    // auto-kill after 3 strikes
                    notifier.notify(
                        "error",
                        &format!("Server '{}' killed", server.name),
                        &format!("exceeded limits: {reason}"),
                        Some(sid),
                    );
                    self.overs.lock().remove(&sid);
                    let _ = procs.stop(sid);
                }
            } else {
                self.overs.lock().remove(&sid);
            }
        }
    }

    /// Read the shared per-server sample cache, computing (and caching) a
    /// fresh sample only on a miss or when the entry is older than `max_age`.
    /// The 5s sweep keeps every server warm; the API layer reads with a 5s
    /// window and only falls back to a direct walk when the cache is cold.
    pub fn cached_info(
        &self,
        procs: &ProcManager,
        server: &models::Server,
        max_age: Duration,
    ) -> ProcessInfo {
        let now = Instant::now();
        {
            let samples = self.samples.lock();
            if let Some((info, ts)) = samples.get(&server.id) {
                if now.duration_since(*ts) < max_age {
                    return info.clone();
                }
            }
        }
        let info = procs.info(server);
        self.samples.lock().insert(server.id, (info.clone(), now));
        info
    }
}

// ---------------- Node stats ----------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct NodeStats {
    pub cpu_total: f64,
    pub cpu_used: f64,
    pub cpu_percent: f64,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_percent: f64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub disk_percent: f64,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub uptime_secs: u64,
    pub processes: u64,
}

pub fn node_stats() -> NodeStats {
    use sysinfo::{CpuRefreshKind, Disks, MemoryRefreshKind, RefreshKind, System};
    let mut sys = System::new_with_specifics(
        RefreshKind::everything()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything()),
    );
    sys.refresh_cpu();
    sys.refresh_memory();
    sys.refresh_processes();
    let cpu = sys.global_cpu_info();
    let disks = Disks::new_with_refreshed_list();
    let mut disk_total = 0u64;
    let mut disk_used = 0u64;
    for d in &disks {
        disk_total += d.total_space();
        disk_used += d.total_space().saturating_sub(d.available_space());
    }
    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let uptime = System::uptime();
    let load = System::load_average();
    let processes = sys.processes().len() as u64;
    NodeStats {
        cpu_total: cpu.frequency() as f64,
        cpu_used: cpu.cpu_usage() as f64,
        cpu_percent: cpu.cpu_usage() as f64,
        mem_total,
        mem_used,
        mem_percent: if mem_total > 0 {
            mem_used as f64 / mem_total as f64 * 100.0
        } else {
            0.0
        },
        disk_total,
        disk_used,
        disk_percent: if disk_total > 0 {
            disk_used as f64 / disk_total as f64 * 100.0
        } else {
            0.0
        },
        load_1: load.one,
        load_5: load.five,
        load_15: load.fifteen,
        uptime_secs: uptime,
        processes,
    }
}

// Panel alert kinds currently active, for edge-triggered `panel.alert`
// webhook emission (see `metrics::emit_panel_alerts`): a kind is emitted
// only on the inactive->active transition, and is removed from the set when
// it recovers, which re-arms it for the next occurrence. Recovery is not an
// event.
static ACTIVE_PANEL_ALERTS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

// ---------------- Background task spawner ----------------

pub async fn spawn_background(
    db: Db,
    cfg: Config,
    procs: Arc<ProcManager>,
    monitor: Arc<Monitor>,
    notifier: Arc<Notifier>,
    hub: Arc<console::ConsoleHub>,
    node_client: Arc<crate::services::node::NodeClient>,
    running: Arc<AtomicBool>,
) {
    // resource monitor sweep every 5s
    {
        let db = db.clone();
        let procs = procs.clone();
        let monitor = monitor.clone();
        let notifier = notifier.clone();
        let running = running.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let db = db.clone();
                let procs = procs.clone();
                let monitor = monitor.clone();
                let notifier = notifier.clone();
                // Sampling walks disk and /proc: run on the blocking pool so
                // the async worker stays free for request handling.
                let _ = tokio::task::spawn_blocking(move || {
                    monitor.sweep(&db, &procs, &notifier)
                })
                .await;
            }
        });
    }
    // scheduler every 10s
    {
        let db = db.clone();
        let running = running.clone();
        let scheduler_procs = procs.clone();
        let scheduler_hub = hub.clone();
        let scheduler_notifier = notifier.clone();
        let scheduler_node_client = node_client.clone();
        tokio::spawn(async move {
            scheduler::run_loop(scheduler::Scheduler {
                db: db.clone(),
                procs: scheduler_procs,
                hub: scheduler_hub,
                notifier: scheduler_notifier,
                node_client: scheduler_node_client,
                running: running.clone(),
            })
            .await;
        });
    }
    // Reconcile server state from every online remote node.
    {
        let db = db.clone();
        let running = running.clone();
        tokio::spawn(async move {
            let client = match crate::services::node::NodeClient::new() {
                Ok(v) => v,
                Err(_) => return,
            };
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let servers =
                    match blocking(db.clone(), |db| crate::models::list_servers(&db, None, false))
                        .await
                    {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                for server in servers.into_iter().filter(|s| s.node != "local") {
                    let node_name = server.node.clone();
                    let node =
                        match blocking(db.clone(), move |db| crate::nodes::get_by_name(&db, &node_name))
                            .await
                        {
                            Ok(v) if v.online() => v,
                            _ => continue,
                        };
                    match client.stats(&node, &server.uuid).await {
                        Ok(stats) => {
                            let state = stats.state.clone();
                            let _ = blocking(db.clone(), move |db| {
                                crate::models::set_server_status(&db, server.id, &state)
                            })
                            .await;
                        }
                        Err(e) => {
                            let uuid = node.uuid.clone();
                            let msg = e.to_string();
                            let _ = blocking(db.clone(), move |db| {
                                crate::nodes::set_error(&db, &uuid, &msg)
                            })
                            .await;
                        }
                    }
                }
            }
        });
    }
    // Telemetry: sample running servers every 30s; prune samples older than
    // the 7-day retention horizon once an hour.
    {
        const SAMPLE_EVERY_SECS: u64 = 30;
        const PRUNE_EVERY_SECS: u64 = 3600;
        const RETENTION_SECS: i64 = 7 * 86400;
        let db = db.clone();
        let cfg = cfg.clone();
        let procs = procs.clone();
        let running = running.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(SAMPLE_EVERY_SECS));
            // After a stall, Burst would fire a same-second sample storm
            // (colliding timestamps on the (server_id, ts) PK); Skip keeps the
            // cadence, mirroring the scheduler loop.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut since_prune = 0u64;
            loop {
                tick.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let procs = procs.clone();
                let _ = blocking(db.clone(), move |db| metrics::sample_running(&db, &procs)).await;
                // Panel self-health on the same cadence: evaluate and emit
                // `panel.alert` for newly-active kinds (edge-triggered via
                let cfg = cfg.clone();
                let _ = blocking(db.clone(), move |db| {
                    Ok(metrics::emit_panel_alerts(
                        &db,
                        &cfg,
                        &mut ACTIVE_PANEL_ALERTS.lock(),
                    ))
                })
                .await;
                since_prune += SAMPLE_EVERY_SECS;
                if since_prune >= PRUNE_EVERY_SECS {
                    since_prune = 0;
                    let cutoff = chrono::Utc::now().timestamp() - RETENTION_SECS;
                    let _ = blocking(db.clone(), move |db| metrics::prune(&db, cutoff)).await;
                }
            }
        });
    }
    // Webhook delivery dispatcher every 5s; the retention prune of terminal
    // deliveries runs on the same loop once a minute (bounded batches, so the
    // write lock is held per 5000-row delete, never across a sweep).
    {
        const DELIVERY_PRUNE_EVERY_SECS: u64 = 60;
        const DELIVERY_RETENTION_DAYS: i64 = 14;
        let db = db.clone();
        let notifier = notifier.clone();
        let running = running.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            let mut since_prune = 0u64;
            loop {
                tick.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                webhooks::dispatch_due(&db, &notifier, 50).await;
                since_prune += 5;
                if since_prune >= DELIVERY_PRUNE_EVERY_SECS {
                    since_prune = 0;
                    let cutoff = (chrono::Utc::now() - chrono::TimeDelta::days(DELIVERY_RETENTION_DAYS))
                        .to_rfc3339();
                    let _ = blocking(db.clone(), move |db| {
                        webhooks::prune_deliveries(&db, &cutoff)
                    })
                    .await;
                }
            }
        });
    }
    let _ = cfg;
}