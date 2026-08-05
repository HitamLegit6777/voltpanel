//! Services layer: process management, console, eggs, files, scheduler,
//! backups, databases, websites, notifications, resource monitoring.
pub mod backups;
pub mod console;
pub mod databases;
pub mod egg;
pub mod files;
pub mod node;
pub mod proc;
pub mod scheduler;
pub mod websites;

use crate::config::Config;
use crate::db::Db;
use crate::models;
pub use console::ConsoleHub;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use proc::{Notification, Notifier, ProcManager, ProcessInfo};

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
}
impl Monitor {
    pub fn new() -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            last_bandwidth: Mutex::new(HashMap::new()),
            overs: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_limit(&self, limits: ServerLimits) {
        self.limits.lock().insert(limits.server_id, limits);
    }

    pub fn remove_limit(&self, server_id: i64) {
        self.limits.lock().remove(&server_id);
        self.last_bandwidth.lock().remove(&server_id);
        self.overs.lock().remove(&server_id);
    }

    /// Called every ~5s: enforce memory/cpu/bandwidth caps, auto-kill + notify.
    pub fn sweep(&self, db: &Db, procs: &ProcManager, notifier: &Notifier) {
        let limits = self.limits.lock().clone();
        for (sid, lim) in limits {
            let Ok(server) = models::get_server(db, sid) else {
                continue;
            };
            let info = procs.info(&server);
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
        load_1: 0.0,
        load_5: 0.0,
        load_15: 0.0,
        uptime_secs: uptime,
        processes,
    }
}

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
                monitor.sweep(&db, &procs, &notifier);
            }
        });
    }
    // scheduler every 10s
    {
        let db = db.clone();
        let running = running.clone();
        tokio::spawn(async move {
            scheduler::run_loop(scheduler::Scheduler {
                db: db.clone(),
                procs: procs.clone(),
                hub: hub.clone(),
                notifier: notifier.clone(),
                node_client: node_client.clone(),
                running: running.clone(),
            })
            .await;
        });
    }
    // periodic node stats cache
    {
        let running = running.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15));
            loop {
                tick.tick().await;
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                let _ = node_stats();
            }
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
                let servers = match crate::models::list_servers(&db, None, false) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                for server in servers.into_iter().filter(|s| s.node != "local") {
                    let node = match crate::nodes::get_by_name(&db, &server.node) {
                        Ok(v) if v.online() => v,
                        _ => continue,
                    };
                    match client.stats(&node, &server.uuid).await {
                        Ok(stats) => {
                            let _ = crate::models::set_server_status(&db, server.id, &stats.state);
                        }
                        Err(e) => {
                            let _ = crate::nodes::set_error(&db, &node.uuid, &e.to_string());
                        }
                    }
                }
            }
        });
    }
    let _ = cfg;
}
