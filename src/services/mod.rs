//! Services layer: workloads, blueprints, console, storage, flows,
//! snapshots, data labs, web publishing, signals, and resource monitoring.
pub mod backups;
pub mod blueprint;
pub mod console;
pub mod databases;
pub mod files;
pub mod gateway;
pub mod keys;
pub mod metrics;
pub mod node;
pub mod proc;
pub mod scheduler;
pub mod watcher;
pub mod webhooks;
pub mod websites;

use crate::config::Config;
use crate::db::{blocking, Db};
use crate::models;
use crate::node_protocol::{PowerAction, RemoteServerStats};
use crate::nodes::Node;
use crate::services::node::NodeClient;
use futures::StreamExt;
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
/// Per-server resource caps enforced by the monitor sweep every 5s.
///
/// `bandwidth_rx`/`bandwidth_tx` are **bytes per 5s interval**: the sweep
/// compares the delta of the cumulative network counters between consecutive
/// sweeps against the cap. `0` disables the cap. `memory_mb`/`cpu_percent`
/// are also `0` = disabled (the API rejects an explicit `0` for those).
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
    /// enforce memory/cpu/bandwidth caps for **local** servers, auto-kill +
    /// notify. Remote servers are enforced by [`Self::sweep_remote`] from the
    /// async worker (their stats live on the agent node, not in `procs`).
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
            // Remote workloads run on agent nodes: sampling them here would
            // only cache bogus "offline" rows; `sweep_remote` enforces them
            // from `RemoteServerStats`.
            if server.node != "local" {
                continue;
            }
            let sid = server.id;
            let info = self.cached_info(procs, server, Duration::from_secs(2));
            let Some(lim) = limits.get(&sid) else {
                continue;
            };
            let prev = self.last_bandwidth.lock().get(&sid).copied();
            let (verdict, new_prev) = decide(lim, &LimitSample::from_info(&info), prev);
            if let Some(bw) = new_prev {
                self.last_bandwidth.lock().insert(sid, bw);
            }
            if let Some(reason) = self.settle(notifier, sid, &server.name, verdict) {
                notifier.notify(
                    "error",
                    &format!("Server '{}' killed", server.name),
                    &format!("exceeded limits: {reason}"),
                    Some(sid),
                );
                let _ = procs.stop(sid);
            }
        }
    }

    /// Remote enforcement pass for the 5s monitor sweep. Fetches fresh stats
    /// from the agent of every remote server that has limits and enforces the
    /// same memory/cpu/bandwidth caps as [`Self::sweep`], auto-killing via the
    /// agent's power endpoint after 3 strikes.
    ///
    /// Node HTTP calls run with bounded concurrency so one dead node's 20s
    /// request timeouts can never stall the rest of the sweep — each `stats`
    /// GET self-times out inside `NodeClient` (connect 5s, request 20s, one
    /// retry).
    pub async fn sweep_remote(&self, db: &Db, notifier: &Notifier, node_client: &NodeClient) {
        const CONCURRENCY: usize = 8;
        let limits = self.limits.lock().clone();
        let Ok(servers) = blocking(db.clone(), |db| models::list_servers(&db, None, false)).await
        else {
            return;
        };
        let nodes = match blocking(db.clone(), |db| crate::nodes::list(&db)).await {
            Ok(v) => v
                .into_iter()
                .filter(|n| n.online())
                .map(|n| (n.name.clone(), n))
                .collect::<HashMap<String, Node>>(),
            Err(_) => return,
        };
        let targets: Vec<(models::Server, Node)> = servers
            .into_iter()
            .filter(|s| s.node != "local" && limits.contains_key(&s.id))
            .filter_map(|s| nodes.get(&s.node).cloned().map(|n| (s, n)))
            .collect();
        futures::stream::iter(targets.into_iter().map(|(server, node)| {
            let client = node_client.clone();
            let lim = limits.get(&server.id).cloned();
            async move {
                let Some(lim) = lim else {
                    return;
                };
                match client.stats(&node, &server.uuid).await {
                    Ok(stats) => {
                        let prev = self.last_bandwidth.lock().get(&server.id).copied();
                        let (verdict, new_prev) =
                            decide(&lim, &LimitSample::from_remote(&stats), prev);
                        if let Some(bw) = new_prev {
                            self.last_bandwidth.lock().insert(server.id, bw);
                        }
                        if let Some(reason) =
                            self.settle(notifier, server.id, &server.name, verdict)
                        {
                            notifier.notify(
                                "error",
                                &format!("Server '{}' killed", server.name),
                                &format!("exceeded limits: {reason}"),
                                Some(server.id),
                            );
                            if let Err(e) = client
                                .power(&node, &server.uuid, PowerAction::Kill)
                                .await
                            {
                                tracing::warn!(server_id = server.id, node = %node.name, "monitor: remote auto-kill failed: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        // Node unreachable: enforcement is impossible this
                        // tick; the reconcile loop records the node error.
                        tracing::debug!(server_id = server.id, node = %node.name, "monitor: remote stats failed: {e}");
                    }
                }
            }
        }))
        .buffer_unordered(CONCURRENCY)
        .for_each(|()| async {})
        .await;
    }

    /// Fold one tick's verdict into the server's strike streak and notify on
    /// violations. Returns `Some(reason)` when the 3-strike auto-kill
    /// threshold was crossed this tick; the caller performs the actual kill
    /// (local `ProcManager::stop`, or the agent's `power kill` for remote).
    fn settle(
        &self,
        notifier: &Notifier,
        server_id: i64,
        name: &str,
        verdict: Verdict,
    ) -> Option<String> {
        match verdict {
            Verdict::Skip => None,
            Verdict::Ok => {
                self.overs.lock().remove(&server_id);
                None
            }
            Verdict::Over { reason } => {
                let n = *self.overs.lock().entry(server_id).or_insert(0) + 1;
                self.overs.lock().insert(server_id, n);
                notifier.notify(
                    "warn",
                    &format!("Server '{name}' over limit"),
                    &format!("{reason} (strike {n})"),
                    Some(server_id),
                );
                if n >= 3 {
                    self.overs.lock().remove(&server_id);
                    Some(reason)
                } else {
                    None
                }
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

// ---------------- Limit decision (shared local + remote) ----------------

/// One server's enforcement inputs for a sweep tick, normalized so the local
/// (`ProcessInfo`) and remote (`RemoteServerStats`) paths share one pure
/// decision function.
struct LimitSample {
    status: String,
    cpu_percent: f64,
    memory_bytes: u64,
    rx_bytes: u64,
    tx_bytes: u64,
}

impl LimitSample {
    fn from_info(info: &ProcessInfo) -> Self {
        Self {
            status: info.status.clone(),
            cpu_percent: info.cpu_percent,
            memory_bytes: info.memory_bytes,
            rx_bytes: info.bandwidth_rx_bytes,
            tx_bytes: info.bandwidth_tx_bytes,
        }
    }

    fn from_remote(stats: &RemoteServerStats) -> Self {
        Self {
            status: stats.state.clone(),
            cpu_percent: stats.cpu_percent,
            memory_bytes: stats.memory_bytes,
            rx_bytes: stats.network_rx_bytes,
            tx_bytes: stats.network_tx_bytes,
        }
    }
}

/// Outcome of one tick's limit comparison.
enum Verdict {
    /// Not running: nothing enforced this tick; any strike streak is kept.
    Skip,
    /// Under every cap: clears the strike streak.
    Ok,
    /// Over a cap: `reason` describes the violation.
    Over { reason: String },
}

/// Pure limit decision shared by the local and remote sweeps.
///
/// Bandwidth limits are cumulative-counter deltas expressed in **bytes per
/// 5s sweep interval**. `prev_bw` holds the previous tick's (rx, tx) counter
/// readings; the first sample for a server seeds the baseline and never trips
/// a bandwidth cap (a restart resets the counters, which `saturating_sub`
/// absorbs). The returned `Some((rx, tx))` is the new baseline to store — only
/// when a bandwidth cap is active and the server is running, mirroring the
/// legacy sweep.
fn decide(
    lim: &ServerLimits,
    s: &LimitSample,
    prev_bw: Option<(u64, u64)>,
) -> (Verdict, Option<(u64, u64)>) {
    if s.status != "running" {
        return (Verdict::Skip, None);
    }
    let mut over = false;
    let mut reason = String::new();
    if lim.memory_mb > 0 && s.memory_bytes > lim.memory_mb * 1024 * 1024 {
        over = true;
        reason = format!(
            "memory {}MB > {}MB",
            s.memory_bytes / 1024 / 1024,
            lim.memory_mb
        );
    }
    if lim.cpu_percent > 0 && s.cpu_percent > lim.cpu_percent as f64 {
        over = true;
        reason = format!("cpu {:.1}% > {}%", s.cpu_percent, lim.cpu_percent);
    }
    let new_prev = if lim.bandwidth_rx > 0 || lim.bandwidth_tx > 0 {
        let (prx, ptx) = prev_bw.unwrap_or((s.rx_bytes, s.tx_bytes));
        let drx = s.rx_bytes.saturating_sub(prx);
        let dtx = s.tx_bytes.saturating_sub(ptx);
        if lim.bandwidth_rx > 0 && drx > lim.bandwidth_rx {
            over = true;
            reason = format!("rx {}KB > {}KB", drx / 1024, lim.bandwidth_rx / 1024);
        }
        if lim.bandwidth_tx > 0 && dtx > lim.bandwidth_tx {
            over = true;
            reason = format!("tx {}KB > {}KB", dtx / 1024, lim.bandwidth_tx / 1024);
        }
        Some((s.rx_bytes, s.tx_bytes))
    } else {
        None
    };
    if over {
        (Verdict::Over { reason }, new_prev)
    } else {
        (Verdict::Ok, new_prev)
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
    // resource monitor sweep every 5s: local servers on the blocking pool
    // (disk//proc walks), remote servers via agent HTTP stats with bounded
    // concurrency inside `sweep_remote`.
    {
        let db = db.clone();
        let procs = procs.clone();
        let monitor = monitor.clone();
        let notifier = notifier.clone();
        let node_client = node_client.clone();
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
                // Local sampling walks disk and /proc: run on the blocking
                // pool so the async worker stays free for request handling.
                let monitor_local = monitor.clone();
                let db_local = db.clone();
                let procs_local = procs.clone();
                let notifier_local = notifier.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    monitor_local.sweep(&db_local, &procs_local, &notifier_local)
                })
                .await;
                // Remote servers: node stats are HTTP calls; sweep_remote
                // bounds concurrency so a dead node cannot stall the sweep.
                monitor.sweep_remote(&db, &notifier, &node_client).await;
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
                // Auto-lift any drain whose deadline has passed. Runs once
                // per loop; a failing sweep is logged, never fatal.
                if let Err(e) = blocking(db.clone(), |db| {
                    let now = chrono::Utc::now().to_rfc3339();
                    let cleared = crate::nodes::clear_expired_drains(&db, &now)?;
                    if !cleared.is_empty() {
                        tracing::info!(
                            count = cleared.len(),
                            "reconcile: auto-lifted expired node drain(s)"
                        );
                    }
                    Ok(())
                })
                .await
                {
                    tracing::warn!("reconcile: drain deadline sweep failed: {e}");
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
                            // Remote servers have no local process to sample
                            // in the 30s telemetry loop (that one is
                            // local-only), so this reconcile tick is the
                            // metrics source for them: persist one row via the
                            // same telemetry helpers the history API reads.
                            let sample =
                                metrics::Sample::from_remote(chrono::Utc::now().timestamp(), &stats);
                            let _ = blocking(db.clone(), move |db| {
                                crate::models::set_server_status(&db, server.id, &state)?;
                                if let Err(e) = metrics::record(&db, server.id, &sample) {
                                    tracing::warn!(
                                        "reconcile: metrics record server {}: {e}",
                                        server.id
                                    );
                                }
                                Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(mem: u64, cpu: u64, rx: u64, tx: u64) -> ServerLimits {
        ServerLimits {
            server_id: 1,
            memory_mb: mem,
            cpu_percent: cpu,
            bandwidth_rx: rx,
            bandwidth_tx: tx,
        }
    }

    fn running(cpu: f64, mem: u64, rx: u64, tx: u64) -> LimitSample {
        LimitSample {
            status: "running".into(),
            cpu_percent: cpu,
            memory_bytes: mem,
            rx_bytes: rx,
            tx_bytes: tx,
        }
    }

    #[test]
    fn decide_memory_and_cpu_caps() {
        let l = lim(1024, 100, 0, 0);
        // Under both caps.
        let (v, _) = decide(&l, &running(50.0, 512 * 1024 * 1024, 0, 0), None);
        assert!(matches!(v, Verdict::Ok));
        // Memory over.
        let (v, _) = decide(&l, &running(50.0, 1025 * 1024 * 1024, 0, 0), None);
        assert!(matches!(v, Verdict::Over { .. }));
        // CPU over.
        let (v, _) = decide(&l, &running(101.0, 0, 0, 0), None);
        assert!(matches!(v, Verdict::Over { .. }));
    }

    #[test]
    fn decide_zero_limits_never_over() {
        let l = lim(0, 0, 0, 0);
        let (v, new_prev) = decide(&l, &running(1e9, u64::MAX, u64::MAX, u64::MAX), None);
        assert!(matches!(v, Verdict::Ok));
        assert!(new_prev.is_none(), "no bandwidth cap => no baseline stored");
    }

    #[test]
    fn decide_non_running_skips_and_keeps_baseline() {
        let l = lim(1, 1, 1, 1);
        let s = LimitSample {
            status: "stopped".into(),
            cpu_percent: 99.0,
            memory_bytes: u64::MAX,
            rx_bytes: 0,
            tx_bytes: 0,
        };
        let (v, new_prev) = decide(&l, &s, Some((10, 10)));
        assert!(matches!(v, Verdict::Skip));
        assert!(new_prev.is_none(), "baseline untouched while not running");
    }

    #[test]
    fn decide_bandwidth_is_bytes_per_5s_delta() {
        let l = lim(0, 0, 1000, 0); // 1000 bytes per 5s interval
        // First sample seeds the baseline: cumulative 0 -> 1000.
        let (v, prev) = decide(&l, &running(0.0, 0, 1000, 50), None);
        assert!(matches!(v, Verdict::Ok), "first sample must not trip");
        let prev = prev.expect("bandwidth cap active => baseline stored");
        assert_eq!(prev, (1000, 50));
        // Next sweep: 2500 more bytes in the interval -> over the 1000 cap.
        let (v, _) = decide(&l, &running(0.0, 0, 3500, 50), Some(prev));
        assert!(matches!(v, Verdict::Over { .. }));
        // 900 bytes in the interval -> under.
        let (v, _) = decide(&l, &running(0.0, 0, 1900, 50), Some(prev));
        assert!(matches!(v, Verdict::Ok));
    }

    #[test]
    fn decide_counter_reset_absorbs_restart() {
        let l = lim(0, 0, 1000, 0);
        let (_, prev) = decide(&l, &running(0.0, 0, 10_000, 0), None);
        // Counters reset (workload restart): 10_000 -> 100 is not a 9900-byte
        // burst; saturating_sub must not trip the cap.
        let (v, _) = decide(&l, &running(0.0, 0, 100, 0), prev);
        assert!(matches!(v, Verdict::Ok));
    }

    #[test]
    fn settle_kills_after_three_strikes_and_clears_on_recovery() {
        let monitor = Monitor::new();
        let notifier = Notifier::new();
        let l = lim(1024, 0, 0, 0);
        let over = |m: &Monitor, n: &Notifier| -> Option<String> {
            let (v, _) = decide(&l, &running(50.0, 2048 * 1024 * 1024, 0, 0), None);
            m.settle(n, 7, "s7", v)
        };
        assert!(over(&monitor, &notifier).is_none(), "strike 1: warn only");
        assert!(over(&monitor, &notifier).is_none(), "strike 2: warn only");
        assert_eq!(
            over(&monitor, &notifier).as_deref(),
            Some("memory 2048MB > 1024MB"),
            "strike 3: auto-kill threshold crossed"
        );
        // Recovery clears the streak and re-arms it.
        let (v, _) = decide(&l, &running(50.0, 512 * 1024 * 1024, 0, 0), None);
        assert!(monitor.settle(&notifier, 7, "s7", v).is_none());
        assert!(monitor.overs.lock().get(&7).is_none());
    }

    #[test]
    fn settle_skip_keeps_strike_streak() {
        let monitor = Monitor::new();
        let notifier = Notifier::new();
        let l = lim(1024, 0, 0, 0);
        let (v, _) = decide(&l, &running(50.0, 2048 * 1024 * 1024, 0, 0), None);
        monitor.settle(&notifier, 9, "s9", v); // strike 1
        // Not running: verdict Skip must not clear the streak.
        let stopped = LimitSample {
            status: "stopped".into(),
            cpu_percent: 0.0,
            memory_bytes: 0,
            rx_bytes: 0,
            tx_bytes: 0,
        };
        let (v, _) = decide(&l, &stopped, Some((0, 0)));
        monitor.settle(&notifier, 9, "s9", v);
        assert_eq!(monitor.overs.lock().get(&9), Some(&1), "streak preserved");
    }
}