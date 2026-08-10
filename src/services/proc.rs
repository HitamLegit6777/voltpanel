//! Process manager: spawn/kill/status for server processes, resource limits,
//! /proc-based resource sampling, bandwidth accounting, auto-kill on overage.
use crate::db::Db;
use crate::models::{self, Server};
use crate::services::webhooks;
use voltpanel::node_daemon::Utf8Carry;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Notify};

// ---------------- Notifications ----------------

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub id: u64,
    pub level: String, // info | warn | error | success
    pub title: String,
    pub message: String,
    pub server_id: Option<i64>,
    pub created_at: String,
}

#[derive(Default)]
pub struct Notifier {
    next_id: AtomicU64,
    subs: Mutex<Vec<mpsc::Sender<Notification>>>,
    history: Mutex<Vec<Notification>>,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(&self, level: &str, title: &str, message: &str, server_id: Option<i64>) {
        let n = Notification {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            level: level.to_string(),
            title: title.to_string(),
            message: message.to_string(),
            server_id,
            created_at: Utc::now().to_rfc3339(),
        };
        {
            let mut hist = self.history.lock();
            hist.push(n.clone());
            const MAX_HISTORY: usize = 200;
            if hist.len() > MAX_HISTORY {
                let drop = hist.len() - MAX_HISTORY;
                hist.drain(..drop);
            }
        }
        let mut subs = self.subs.lock();
        subs.retain(|s| s.try_send(n.clone()).is_ok());
    }

    pub fn subscribe(&self) -> mpsc::Receiver<Notification> {
        let (tx, rx) = mpsc::channel(256);
        self.subs.lock().push(tx);
        rx
    }

    pub fn history(&self) -> Vec<Notification> {
        self.history.lock().clone()
    }

    pub fn clear(&self) {
        self.history.lock().clear();
    }
}

// ---------------- Process state ----------------

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub pid: Option<u32>,
    pub status: String,
    pub started_at: Option<String>,
    pub exit_code: Option<i32>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub memory_percent: f64,
    pub bandwidth_rx_bytes: u64,
    pub bandwidth_tx_bytes: u64,
    pub disk_usage_bytes: u64,
    pub uptime_secs: u64,
}

#[derive(Default)]
pub struct ProcessState {
    pub child: Mutex<Option<Child>>,
    pub pid: Mutex<Option<u32>>,
    pub stdin: Mutex<Option<ChildStdin>>,
    pub cgroup: Mutex<Option<crate::isolation::Cgroup>>,
    pub network: Mutex<Option<crate::isolation::NetworkLease>>,
    pub status: Mutex<String>,
    pub started_at: Mutex<Option<String>>,
    pub exit_code: Mutex<Option<i32>>,
    pub stop_issued: AtomicBool,
    pub read_total: AtomicU64,
    pub write_total: AtomicU64,
    pub operation: AtomicBool,
    pub last_cpu_read: AtomicU64,
    pub last_cpu_time: AtomicU64,
    /// Handle of the std thread currently reaping the running child. Present
    /// only while a child is (or was) running; used by start() to wait out a
    /// draining stop before re-owning the state.
    pub reaper: Mutex<Option<std::thread::JoinHandle<()>>>,
}

pub struct ProcManager {
    pub db: Db,
    pub hub: Arc<crate::services::console::ConsoleHub>,
    pub procs: DashMap<i64, Arc<ProcessState>>,
    fs_cache: Mutex<HashMap<i64, (u64, std::time::Instant)>>,
    stop_evt: Arc<Notify>,
    stopped: AtomicBool,
}

// ---------------- Crash classification (G8) ----------------

/// Why a process exited. Operator intent (a stop/kill with `stop_issued`
/// latched) is NEVER a crash, regardless of the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitKind {
    /// The exit was operator-requested (stop or kill): never a crash.
    Requested,
    /// Zero exit code under the default policy: a clean, expected exit.
    Clean,
    /// A crash, with a human-readable reason for the console and notifier.
    Crash(String),
}

/// Classify an exit per the crash policy. Mirrors the detect-clean-exit-as-crash
/// toggle: `code 0` is clean by default, but an unrequested clean exit counts
/// as a crash when the operator enabled detection. Nonzero codes and
/// signal-kills (no code) are always crashes.
pub fn classify_exit(code: Option<i32>, stop_issued: bool, detect_clean_exit: bool) -> ExitKind {
    if stop_issued {
        return ExitKind::Requested;
    }
    match code {
        Some(0) if !detect_clean_exit => ExitKind::Clean,
        Some(0) => ExitKind::Crash("clean exit treated as crash (crash policy)".into()),
        Some(n) => ExitKind::Crash(format!("exited with code {n}")),
        None => ExitKind::Crash("killed by signal".into()),
    }
}

/// Exponential backoff between crash restarts: 5s, 10s, 20s, … capped at 60s,
/// indexed by the restarts the current burst already consumed. The cap keeps a
/// pathological loop from stalling the panel while still spacing restarts out.
pub fn crash_backoff(burst_restarts: i64) -> std::time::Duration {
    let secs = 5i64.saturating_mul(1 << burst_restarts.clamp(0, 4));
    std::time::Duration::from_secs(secs.min(60) as u64)
}

impl ProcManager {
    pub fn new(db: Db, hub: Arc<crate::services::console::ConsoleHub>) -> Self {
        Self {
            db,
            hub,
            procs: DashMap::new(),
            fs_cache: Mutex::new(HashMap::new()),
            stop_evt: Arc::new(Notify::new()),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn set_stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.stop_evt.notify_waiters();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    pub fn stop_watch(&self) -> &Notify {
        &self.stop_evt
    }

    /// Per-server resource limits are enforced by the monitor service
    /// (`Monitor::set_limit`, wired up in main.rs). This registration used
    /// to write the never-read `cfg_limits` map; it is kept only as the
    /// startup wiring entry point.
    pub fn register_limits(&self, _server_id: i64, _mem_mb: i64, _cpu_pct: i64) {}
    pub fn remove_limits(&self, server_id: i64) {
        self.fs_cache.lock().remove(&server_id);
        if let Some((_, p)) = self.procs.remove(&server_id) {
            *p.stdin.lock() = None;
            p.stop_issued.store(true, Ordering::Relaxed);
            let pid = *p.pid.lock();
            if let Some(cgroup) = p.cgroup.lock().as_ref() {
                let _ = cgroup.kill_all();
            }
            if let Some(pid) = pid {
                kill_daemon_group(pid);
            }
            // Deliberately NOT clearing pid here: the reaper thread owns the
            // final cleanup (network lease drop, cgroup dir, pid clear) once
            // the child exits. Clearing pid first would make the reaper's
            // ownership check bail and leak those resources.
            *p.status.lock() = "stopped".into();
        }
    }

    /// Snapshot of all live process entries.
    pub fn all(&self) -> Vec<(i64, Arc<ProcessState>)> {
        self.procs
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect()
    }

    pub fn state(&self, server_id: i64) -> Option<Arc<ProcessState>> {
        self.procs.get(&server_id).map(|p| p.clone())
    }

    pub fn info(&self, server: &Server) -> ProcessInfo {
        let disk = {
            let now = std::time::Instant::now();
            let cached = self.fs_cache.lock().get(&server.id).copied();
            match cached {
                Some((size, t)) if now.duration_since(t).as_secs() < 60 => size,
                _ => {
                    let size = fs_usage(&server_dir(server)).unwrap_or(0);
                    self.fs_cache.lock().insert(server.id, (size, now));
                    size
                }
            }
        };
        let Some(ps) = self.state(server.id) else {
            return ProcessInfo {
                pid: None,
                status: "offline".into(),
                started_at: None,
                exit_code: None,
                cpu_percent: 0.0,
                memory_bytes: 0,
                memory_percent: 0.0,
                bandwidth_rx_bytes: 0,
                bandwidth_tx_bytes: 0,
                disk_usage_bytes: disk,
                uptime_secs: 0,
            };
        };
        let status = ps.status.lock().clone();
        let pid = *ps.pid.lock();
        let (cpu, mem) = self.sample(server);
        let started_at = ps.started_at.lock().clone();
        let exit_code = *ps.exit_code.lock();
        let rx = ps.read_total.load(Ordering::Relaxed);
        let tx = ps.write_total.load(Ordering::Relaxed);
        let uptime_secs = started_at
            .as_deref()
            .and_then(|t| {
                chrono::DateTime::parse_from_rfc3339(t)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            })
            .map(|t| (Utc::now() - t).num_seconds().max(0) as u64)
            .unwrap_or(0);
        ProcessInfo {
            pid,
            status: status.clone(),
            started_at,
            exit_code,
            cpu_percent: cpu,
            memory_bytes: mem,
            memory_percent: mem_percent(mem, server.memory_mb as u64),
            bandwidth_rx_bytes: rx,
            bandwidth_tx_bytes: tx,
            disk_usage_bytes: disk,
            uptime_secs,
        }
    }

    pub fn sample(&self, server: &Server) -> (f64, u64) {
        let Some(ps) = self.state(server.id) else {
            return (0.0, 0);
        };
        let Some(pid) = *ps.pid.lock() else {
            return (0.0, 0);
        };
        let mut total_mem: u64 = 0;
        let mut total_time: u64 = 0;
        let mut total_procs: u64 = 0;
        walk_children(pid, &mut total_mem, &mut total_time, &mut total_procs);
        let prev_time = ps.last_cpu_time.swap(total_time, Ordering::Relaxed);
        let prev_read = ps.last_cpu_read.swap(now_ticks(), Ordering::Relaxed);
        let now = now_ticks();
        let dt = now.saturating_sub(prev_read).max(1);
        let dtime = total_time.saturating_sub(prev_time);
        let cpu = if dt > 0 {
            (dtime as f64 / dt as f64) * 100.0
        } else {
            0.0
        };
        (if cpu.is_finite() { cpu } else { 0.0 }, total_mem)
    }

    /// Start a server process. Uses std::process + prlimit for resource caps
    /// (no pre_exec / no fork magic), readers forwarded to the console hub.
    /// Explicit starts (API start/restart, scheduled tasks) are never refused
    /// by a completed manual stop: the user's action is the latest intent
    /// once the previous drain has been joined. See [`Self::gate_start`].
    pub fn start(
        &self,
        server: &Server,
        startup_cmd: &str,
        env: &[(String, String)],
        notifier: Arc<Notifier>,
    ) -> Result<()> {
        self.start_impl(server, startup_cmd, env, notifier, false)
    }

    /// Decide whether a (re)start may proceed, waiting out any in-flight stop
    /// drain first. Callers MUST hold the operation guard: stop()/kill() flip
    /// `stop_issued` under that same guard, so the two reads here are stable.
    ///
    /// `stop_issued` is a sticky latch — set by stop()/kill(), cleared only by
    /// the successful respawn in start_impl. While it is set, an auto-restart
    /// (which carries no newer user intent) is refused: the user's stop wins
    /// even when a crash-restart task was already sleeping on an earlier
    /// exit. An explicit start is newer intent than the stop once the drain
    /// has been joined, so it passes.
    fn gate_start(&self, ps: &ProcessState, suppress_if_stopped: bool) -> Result<()> {
        if ps.stop_issued.load(Ordering::Relaxed) {
            // A stop() may still be draining the previous child on its reaper
            // thread (SIGTERM sent, pid not yet cleared). Join it — bounded
            // by the stop grace window — so a restart re-owns a fully reaped
            // state. Take the handle and drop the lock BEFORE joining: the
            // reaper clears its own slot at exit under the same lock, so
            // joining while holding it would be a self-deadlock.
            let stale = ps.reaper.lock().take();
            if let Some(reaper) = stale {
                let _ = reaper.join();
            }
            if suppress_if_stopped && ps.stop_issued.load(Ordering::Relaxed) {
                bail!("server is stopping")
            }
        }
        if ps.pid.lock().is_some() {
            bail!("server already running")
        }
        Ok(())
    }

    /// Shared start path; `start()` is the explicit-intent entry point.
    /// `suppress_if_stopped` marks the auto-restart intent (see
    /// [`Self::gate_start`]) and is set only by restart_if_needed.
    fn start_impl(
        &self,
        server: &Server,
        startup_cmd: &str,
        env: &[(String, String)],
        notifier: Arc<Notifier>,
        suppress_if_stopped: bool,
    ) -> Result<()> {
        if self.stopped.load(Ordering::Relaxed) {
            bail!("panel shutting down")
        }
        let ps = self
            .procs
            .entry(server.id)
            .or_insert_with(|| Arc::new(ProcessState::default()))
            .clone();
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&ps.operation)?;
        self.gate_start(&ps, suppress_if_stopped)?;
        let dir = server_dir(server);
        crate::isolation::prepare_root(&dir, &server.uuid)?;
        crate::isolation::own_tree(&dir, &server.uuid)?;
        let isolation = crate::isolation::IsolationConfig::default();
        let limits = crate::isolation::Limits {
            memory_bytes: server.memory_mb as u64 * 1_048_576,
            cpu_percent: server.cpu_percent as u64,
            pids_max: crate::isolation::DEFAULT_PIDS_MAX,
        };
        let cgroup = crate::isolation::Cgroup::create(&isolation, &server.uuid, &limits)?;
        let mut cmd = crate::isolation::sandbox_command(
            &isolation,
            &dir,
            &server.uuid,
            startup_cmd,
            &limits,
        )?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        // Die with the panel: without PDEATHSIG a panel SIGKILL strands the
        // bwrap -> setpriv -> sh tree, its cgroup dir, and the veth/nft
        // lease — the reaper only cleans up on the graceful path. Boot-time
        // sweeps for pre-existing orphans belong to the daemon startup
        // module, not here.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    // Parent died between fork and prctl: never outlive it.
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().context("failed to spawn isolated process")?;
        let pid = child.id();
        // Every failure after spawn must tear down what the child may already
        // have built: kill the whole process group (the bwrap wrapper forks a
        // setpriv/sh/payload tree, so killing only the leader leaks it),
        // drain the cgroup, and wait() the leader so the panel never
        // accumulates a zombie per failed spawn. NetworkLease::configure
        // scrubs its own partial veth/nft state on error.
        if let Err(error) = cgroup.attach(pid) {
            reap_failed_spawn(child);
            let _ = cgroup.kill_all();
            return Err(error);
        }
        let Some(stdout) = child.stdout.take() else {
            reap_failed_spawn(child);
            let _ = cgroup.kill_all();
            bail!("no stdout");
        };
        let Some(stderr) = child.stderr.take() else {
            reap_failed_spawn(child);
            let _ = cgroup.kill_all();
            bail!("no stderr");
        };
        let stdin = child.stdin.take();
        let ports = match crate::models::ports_for_server(&self.db, server.id) {
            Ok(v) => v
                .into_iter()
                .filter_map(|p| u16::try_from(p).ok())
                .collect::<Vec<_>>(),
            Err(error) => {
                reap_failed_spawn(child);
                let _ = cgroup.kill_all();
                return Err(error);
            }
        };
        let network = match crate::isolation::NetworkLease::configure(pid, &server.uuid, &ports) {
            Ok(v) => v,
            Err(error) => {
                reap_failed_spawn(child);
                let _ = cgroup.kill_all();
                return Err(error);
            }
        };
        *ps.cgroup.lock() = Some(cgroup);
        *ps.network.lock() = Some(network);
        *ps.status.lock() = "running".into();
        *ps.started_at.lock() = Some(Utc::now().to_rfc3339());
        *ps.exit_code.lock() = None;
        // A successful spawn clears the stop latch: running again is newer
        // intent than the stop that preceded it.
        ps.stop_issued.store(false, Ordering::Relaxed);
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.stdin.lock() = stdin;
        ps.last_cpu_read.store(now_ticks(), Ordering::Relaxed);
        ps.last_cpu_time.store(0, Ordering::Relaxed);
        let _ = models::set_server_status(&self.db, server.id, "running");
        // Lifecycle event: a successful spawn (explicit start or
        // auto-restart) is a server.start; the reaper emits the
        // complementary stop/crash later.
        emit_server_event(&self.db, server.id, "server.start", "running", None);
        // An operator-initiated start begins a fresh crash burst: manual
        // recovery must never inherit stale budget debt. Auto-restarts
        // (suppress_if_stopped) deliberately keep the burst counting, which
        // is what bounds a crash loop.
        if !suppress_if_stopped {
            let _ = models::reset_crash_window(&self.db, server.id);
        }

        // readers forward output into the app-wide console hub. The channel
        // is bounded: the hub consumer does blocking log-file writes, so an
        // unbounded queue would buffer a chatty server's whole stdout in RAM
        // when the disk stalls. pump_stream/forward_console apply the
        // drop-to-latest policy (see OUTPUT_CHANNEL_CAP).
        let hub = self.hub.clone();
        let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_CAP);
        let (tx_err, rx_err) = mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_CAP);
        let sid = server.id;
        let ps_out = ps.clone();
        std::thread::spawn(move || pump_stream(stdout, sid, ps_out, tx_out));
        let ps_err = ps.clone();
        std::thread::spawn(move || pump_stream(stderr, sid, ps_err, tx_err));
        tokio::spawn(forward_console(rx_out, hub.clone(), sid));
        tokio::spawn(forward_console(rx_err, hub.clone(), sid));

        // Reap on a dedicated std thread: Child::wait blocks for the child's
        // whole lifetime, so it must never occupy a Tokio worker. The async
        // restart path is bounced back onto the runtime via a oneshot.
        let m = self.snapshot();
        let sid = server.id;
        let srv = server.clone();
        let not = notifier.clone();
        let st = ps.clone();
        let (tx_done, rx_done) = oneshot::channel::<Option<i32>>();
        let m_reaper = m.clone();
        let reaper = std::thread::spawn(move || reap_child(st, m_reaper, sid, tx_done));
        *ps.reaper.lock() = Some(reaper);
        tokio::spawn(async move {
            if let Ok(Some(code)) = rx_done.await {
                m.restart_if_needed(&srv, not, Some(code)).await;
            }
        });
        Ok(())
    }

    /// Cheap Arc snapshot of the manager's shared state, for handing the
    /// blocking reaper/restart work to dedicated std threads.
    fn snapshot(&self) -> Arc<ProcManager> {
        Arc::new(ProcManager {
            db: self.db.clone(),
            hub: self.hub.clone(),
            procs: self.procs.clone(),
            fs_cache: Mutex::new(self.fs_cache.lock().clone()),
            stop_evt: self.stop_evt.clone(),
            stopped: AtomicBool::new(self.stopped.load(Ordering::Relaxed)),
        })
    }

    /// Handle an unrequested process exit: classify it, apply the crash
    /// policy, and restart within the burst budget or leave the server in a
    /// terminal `crashed` state. The oneshot that invokes this only fires
    /// with `Some(code)` when `stop_issued` was clear at reap time, and the
    /// post-backoff re-check below cancels any restart racing a newer stop,
    /// so an operator-initiated stop/kill is never treated as a crash.
    async fn restart_if_needed(&self, server: &Server, notifier: Arc<Notifier>, code: Option<i32>) {
        let srv = match models::get_server(&self.db, server.id) {
            Ok(s) => s,
            Err(_) => return,
        };
        // Classify against the live policy (not the stale spawn-time row).
        let ExitKind::Crash(reason) =
            classify_exit(code, false, srv.crash_detect_clean_exit)
        else {
            // Clean exit under the default policy: no restart, no crash state.
            return;
        };
        if !srv.auto_restart {
            // Terminal crash: record the reason, notify, do not restart.
            let _ = models::mark_crashed(&self.db, srv.id, &reason);
            notifier.notify(
                "error",
                &format!("Server '{}' crashed", srv.name),
                &reason,
                Some(srv.id),
            );
            return;
        }
        if srv.suspended {
            return;
        }
        // Budget: consume one slot or give up. `consume_crash_budget` resets
        // a stale burst (the server survived long enough to be stable) and
        // returns Exhausted once the burst used `crash_restart_budget`
        // restarts; each crash inside a hot window strictly increases the
        // consumed count, so the loop terminates after at most `budget`
        // restarts and lands here.
        let used = match models::consume_crash_budget(&self.db, srv.id, srv.crash_restart_budget) {
            Ok(models::CrashBudget::Allowed(used)) => used,
            Ok(models::CrashBudget::Exhausted(used)) => {
                let reason = format!(
                    "crash restart budget exhausted ({used}/{} in burst)",
                    srv.crash_restart_budget
                );
                let _ = models::mark_crashed(&self.db, srv.id, &reason);
                notifier.notify(
                    "error",
                    &format!("Server '{}' crashed", srv.name),
                    &reason,
                    Some(srv.id),
                );
                return;
            }
            Err(_) => return,
        };
        // Backoff grows with the burst so a loop slows down instead of
        // hammering the fabric.
        tokio::time::sleep(crash_backoff(used)).await;
        if self
            .state(server.id)
            .map(|s| s.stop_issued.load(Ordering::Relaxed))
            .unwrap_or(true)
        {
            // An operator stop landed during the backoff: the restart is
            // cancelled and this exit was never a crash restart — refund the
            // budget slot it consumed.
            refund_crash_slot(&self.db, server.id, used);
            return;
        }
        let srv = match models::get_server(&self.db, server.id) {
            Ok(s) => s,
            Err(_) => return,
        };
        if !srv.auto_restart || srv.suspended {
            // Config flipped during the backoff: again a cancelled restart,
            // so the consumed slot must not stay spent.
            refund_crash_slot(&self.db, server.id, used);
            return;
        }
        let _ = models::bump_restart_count(&self.db, srv.id);
        let cmd = crate::services::blueprint::resolve_startup(&self.db, &srv);
        let env = crate::services::blueprint::env_for_server(&self.db, &srv);
        match cmd {
            Ok(cmd) => {
                notifier.notify(
                    "info",
                    &format!("Restarting '{}'", srv.name),
                    &format!("{reason}; backoff {}", crash_backoff(used).as_secs()),
                    Some(srv.id),
                );
                // start_impl does blocking work (reaper drain join, cgroup
                // setup, veth/nft network config); keep it off the Tokio
                // worker that runs the restart task.
                let m = self.snapshot();
                let srv2 = srv.clone();
                let not2 = notifier.clone();
                match tokio::task::spawn_blocking(move || {
                    m.start_impl(&srv2, &cmd, &env, not2, true)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        notifier.notify("error", "Restart failed", &e.to_string(), Some(srv.id))
                    }
                    Err(e) => notifier.notify("error", "Restart failed", &e.to_string(), Some(srv.id)),
                }
            }
            Err(e) => notifier.notify("error", "Restart failed", &e.to_string(), Some(srv.id)),
        }
    }

    /// Graceful stop (SIGTERM, then SIGKILL after a 10s grace window). The
    /// liveness polling runs on a dedicated std thread so a slow shutdown
    /// never blocks a Tokio worker. Final state is applied by the reaper
    /// thread once the child exits; stop_issued prevents any auto-restart.
    pub fn stop(&self, server_id: i64) -> Result<()> {
        let Some(ps) = self.state(server_id) else {
            return Ok(());
        };
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&ps.operation)?;
        if ps.stop_issued.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        *ps.status.lock() = "stopping".into();
        let _ = models::set_server_status(&self.db, server_id, "stopping");
        *ps.stdin.lock() = None;
        let pid = *ps.pid.lock();
        let Some(pid) = pid else {
            // Nothing running: finalize immediately, no drain needed.
            *ps.status.lock() = "stopped".into();
            *ps.exit_code.lock() = Some(0);
            *ps.started_at.lock() = None;
            let _ = models::set_server_status(&self.db, server_id, "offline");
            return Ok(());
        };
        // Signal the whole group: the sh leader may die but children must stop too.
        unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        let drain = ps.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            loop {
                // Once the leader exits, the reaper clears the pid and owns
                // the remaining cleanup (its cgroup kill_all sweeps the
                // stragglers). Staying in this loop would SIGKILL the old
                // group up to 10s later — and if the OS recycled the leader's
                // pid for a freshly restarted server, that kill would land on
                // the new process group.
                //
                // The pid lock is held across the recycle check AND the
                // SIGKILL: releasing it between them would open a window in
                // which the leader dies, its pid is recycled, and the kill
                // lands on a fresh process group. The lock drops before the
                // sleep so the reaper can still clear the pid (which is
                // exactly what breaks this loop).
                let guard = drain.pid.lock();
                if *guard != Some(pid) {
                    break;
                }
                let group_alive = unsafe { libc::kill(-(pid as i32), 0) == 0 };
                if !group_alive {
                    break;
                }
                if t0.elapsed().as_secs() > 10 {
                    kill_daemon_group(pid);
                    if let Some(cgroup) = drain.cgroup.lock().as_ref() {
                        let _ = cgroup.kill_all();
                    }
                    break;
                }
                drop(guard);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        Ok(())
    }

    /// Immediate kill, no grace (used by monitor auto-kill / kill action).
    /// Marks stop_issued so the reaper never treats this as a crash and never
    /// auto-restarts; the reaper (not this call) clears the pid and releases
    /// the network lease / cgroup after the child exits.
    pub fn kill(&self, server_id: i64) -> Result<()> {
        let Some(ps) = self.state(server_id) else {
            return Ok(());
        };
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&ps.operation)?;
        *ps.stdin.lock() = None;
        let pid = *ps.pid.lock();
        let Some(pid) = pid else {
            return Ok(());
        };
        ps.stop_issued.store(true, Ordering::Relaxed);
        if let Some(cgroup) = ps.cgroup.lock().as_ref() {
            let _ = cgroup.kill_all();
        }
        kill_daemon_group(pid);
        *ps.status.lock() = "stopped".into();
        *ps.started_at.lock() = None;
        let _ = models::set_server_status(&self.db, server_id, "offline");
        Ok(())
    }

    pub fn send_input(&self, server_id: i64, line: &str) -> Result<()> {
        let Some(ps) = self.state(server_id) else {
            bail!("server not running");
        };
        let mut stdin = ps.stdin.lock();
        let Some(s) = stdin.as_mut() else {
            bail!("server not running (no stdin)");
        };
        s.write_all(line.as_bytes())?;
        s.flush()?;
        Ok(())
    }
}

/// SIGKILL the whole process group led by `pid`, then the leader itself.
/// The sandbox spawns a bwrap -> setpriv -> sh tree, so leader-only kills
/// strand the descendants in their own group.
fn kill_daemon_group(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}
/// Return a crash-restart slot consumed by `consume_crash_budget` when the
/// restart it paid for was cancelled (operator stop or config flip during
/// the backoff): a restart that never ran must not count against the burst.
fn refund_crash_slot(db: &Db, id: i64, used: i64) {
    // Best-effort refund: a checkout or execute failure is logged and the
    // slot stays consumed rather than panicking on pool exhaustion.
    if let Ok(conn) = db.get() {
        let _ = conn.execute(
            "UPDATE servers SET crash_restarts=?1, updated_at=?2 WHERE id=?3",
            rusqlite::params![used, Utc::now().to_rfc3339(), id],
        );
    }
}
/// Enqueue a `server.*` lifecycle event for `sid` (best-effort, fire and
/// forget). The envelope carries the server identity (id/uuid/name) and a
/// timestamp; `extra` merges event-specific fields. Payloads are far under
/// the 64 KiB emit cap. A vanished server row (reap after server deletion)
/// yields `null` identity fields rather than suppressing the event.
fn emit_server_event(db: &Db, sid: i64, event: &str, status: &str, extra: Option<serde_json::Value>) {
    let srv = models::get_server_any(db, sid).ok();
    let mut payload = json!({
        "event": event,
        "server_id": sid,
        "uuid": srv.as_ref().map(|s| s.uuid.clone()),
        "server_name": srv.as_ref().map(|s| s.name.clone()),
        "status": status,
        "timestamp": Utc::now().to_rfc3339(),
    });
    if let (Some(serde_json::Value::Object(extra)), serde_json::Value::Object(base)) =
        (extra, &mut payload)
    {
        for (k, v) in extra {
            base.insert(k, v);
        }
    }
    webhooks::emit(db, event, Some(sid), payload);
}


/// Body of the reaper thread. Blocks on `Child::wait` (never on a Tokio
/// worker), then — only while this reaper still owns `ps` (same pid) —
/// releases the network lease, removes the cgroup, and applies the final
/// status. The oneshot carries the exit code so the runtime can decide about
/// an auto-restart; a `None` payload means the stop was requested.
fn reap_child(
    st: Arc<ProcessState>,
    m: Arc<ProcManager>,
    sid: i64,
    tx_done: oneshot::Sender<Option<i32>>,
) {
    let mut child = match st.child.lock().take() {
        Some(c) => c,
        None => return,
    };
    let my_pid = child.id();
    // Prefer the raw exit code; for signal deaths `code()` is None, so fall
    // back to the terminating signal recorded as 128+sig (shell convention)
    // instead of a lossy -1.
    let code = child
        .wait()
        .ok()
        .and_then(|s| s.code().or_else(|| s.signal().map(|sig| 128 + sig)));
    // Ownership check: only the current owner of ps (same pid) may release
    // the network lease, clean the cgroup, or clear the pid. A newer start()
    // may have taken over while we were blocked.
    if *st.pid.lock() == Some(my_pid) {
        st.network.lock().take();
        if let Some(cgroup) = st.cgroup.lock().take() {
            cgroup.remove();
        }
        *st.pid.lock() = None;
        *st.exit_code.lock() = Some(code.unwrap_or(-1));
        *st.started_at.lock() = None;
        if st.stop_issued.load(Ordering::Relaxed) {
            // Operator-requested stop/kill: never a crash, never a restart.
            *st.status.lock() = "stopped".into();
            let _ = models::set_server_status(&m.db, sid, "offline");
            // Operator-requested stop/kill finalization: this branch is the
            // single chokepoint for stop(), kill(), and remove_limits
            // teardown, so the event fires exactly once per stop.
            emit_server_event(&m.db, sid, "server.stop", "stopped", None);
            let _ = tx_done.send(None);
        } else {
            // Apply the crash policy to the exit code: a clean exit is
            // "stopped" unless the operator enabled clean-exit-as-crash, in
            // which case it is a crash like any other.
            let detect_clean_exit = models::get_server_any(&m.db, sid)
                .map(|s| s.crash_detect_clean_exit)
                .unwrap_or(false);
            match classify_exit(code, false, detect_clean_exit) {
                ExitKind::Crash(reason) => {
                    *st.status.lock() = "crashed".into();
                    let _ = models::mark_crashed(&m.db, sid, &reason);
                    // The crash is recorded right here — the reaper is the
                    // single chokepoint that classifies every unrequested
                    // exit. restart_if_needed's policy follow-ups re-mark the
                    // row but deliberately never re-emit, so one crash
                    // episode yields exactly one server.crash event.
                    emit_server_event(
                        &m.db,
                        sid,
                        "server.crash",
                        "crashed",
                        Some(json!({"reason": reason, "exit_code": code})),
                    );
                    let _ = tx_done.send(code);
                }
                _ => {
                    *st.status.lock() = "stopped".into();
                    let _ = models::set_server_status(&m.db, sid, "stopped");
                    let _ = tx_done.send(code);
                }
            }
        }
    }
    // Release our reaper slot only if a newer start() has not already
    // replaced it; otherwise the live handle would be clobbered.
    let mut slot = st.reaper.lock();
    if slot.as_ref().map(|h| h.thread().id()) == Some(std::thread::current().id()) {
        *slot = None;
    }
}

/// Cap on buffered console output per stream (chunks of up to 4 KiB, ~4 MiB
/// worst case per stream instead of unbounded RAM). When the console
/// consumer falls behind, new chunks are dropped (rate-limited warning) and
/// the consumer keeps only the freshest chunk per tick (drop-to-latest).
const OUTPUT_CHANNEL_CAP: usize = 1024;

/// Reap a child that failed to fully start: SIGKILL its group and leader,
/// then `wait()` it so the panel never accumulates a zombie per failed
/// spawn. The cgroup drain happens at the call site (no cgroup exists in
/// the test environment, keeping this unit-testable).
fn reap_failed_spawn(mut child: Child) {
    kill_daemon_group(child.id());
    let _ = child.wait();
}

fn pump_stream(mut stream: impl Read, sid: i64, ps: Arc<ProcessState>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    let mut dropped: u64 = 0;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                ps.read_total.fetch_add(n as u64, Ordering::Relaxed);
                match tx.try_send(buf[..n].to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Rate-limited: a chatty server on a slow disk must
                        // not spam the log with one warning per chunk.
                        dropped += 1;
                        if dropped == 1 || dropped.is_multiple_of(1000) {
                            tracing::warn!(
                                "console output backlog full for server {sid}: dropped {dropped} chunk(s)"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Forward console chunks from a pump thread to the console hub. Bounded by
/// `OUTPUT_CHANNEL_CAP`; each tick drains the whole backlog and appends only
/// the freshest chunk (drop-to-latest), so a stalled disk cannot lag the
/// live console or grow the queue without bound.
async fn forward_console(
    mut rx: mpsc::Receiver<Vec<u8>>,
    hub: Arc<crate::services::console::ConsoleHub>,
    sid: i64,
) {
    let mut carry = Utf8Carry::default();
    while let Some(chunk) = rx.recv().await {
        let mut latest = carry.push(&chunk);
        while let Ok(next) = rx.try_recv() {
            // Every drained chunk feeds the carry — even the dropped ones —
            // so a character split across a dropped boundary still decodes.
            latest = carry.push(&next);
        }
        hub.append(
            sid,
            &latest,
            crate::services::console::LineKind::Runtime,
        );
    }
}

fn now_ticks() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mem_percent(mem: u64, limit_mb: u64) -> f64 {
    if limit_mb == 0 {
        return 0.0;
    }
    (mem as f64) / (limit_mb as f64 * 1024.0 * 1024.0) * 100.0
}

pub fn server_dir(server: &Server) -> PathBuf {
    crate::SETTINGS.paths.servers_dir.join(&server.uuid)
}

pub fn fs_usage(dir: &Path) -> Result<u64> {
    let mut total: u64 = 0;
    for e in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if e.file_type().is_file() {
            total += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(total)
}

fn walk_children(pid: u32, mem: &mut u64, cpu_time: &mut u64, count: &mut u64) {
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        *count += 1;
        if let Some((m, t)) = read_proc_stat(p) {
            *mem += m;
            *cpu_time += t;
        }
        if let Ok(children) = read_proc_children(p) {
            stack.extend(children);
        }
    }
}

fn read_proc_stat(pid: u32) -> Option<(u64, u64)> {
    let path = format!("/proc/{pid}/stat");
    let mut s = String::new();
    std::fs::File::open(&path)
        .ok()?
        .read_to_string(&mut s)
        .ok()?;
    // comm may itself contain ')'; the field's closing paren is the LAST
    // one before the space-separated numeric fields, so rfind lands on it.
    let rest = s.rfind(')').and_then(|i| s.get(i + 1..))?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 22 {
        return None;
    }
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let rss_pages: u64 = fields.get(21)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some((rss_pages.saturating_mul(page_size), utime + stime))
}

fn read_proc_children(pid: u32) -> Result<Vec<u32>> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let s = std::fs::read_to_string(path)?;
    Ok(s.split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn test_manager() -> ProcManager {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(&tmp.path().join("t.db").to_string_lossy()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        let hub = crate::services::console::ConsoleHub::new(cfg);
        ProcManager::new(db, Arc::new(hub))
    }

    /// Spawn a long-lived child in its own process group, as start() does.
    fn spawn_sleep() -> Child {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep")
    }

    /// Reproduce start()'s reaper wiring: store the JoinHandle in the reaper
    /// slot (so reap_child's self-clear is exercised) and return the oneshot
    /// receiver the restart task would consume.
    fn spawn_reaper(
        ps: Arc<ProcessState>,
        m: &ProcManager,
        sid: i64,
    ) -> oneshot::Receiver<Option<i32>> {
        let (tx_done, rx_done) = oneshot::channel();
        let m2 = m.snapshot();
        let ps2 = ps.clone();
        let handle = std::thread::spawn(move || reap_child(ps2, m2, sid, tx_done));
        *ps.reaper.lock() = Some(handle);
        rx_done
    }

    fn wait_pid_cleared(ps: &ProcessState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while ps.pid.lock().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ps.pid.lock().is_none(), "reaper must clear pid");
    }

    // ---------------- Notifier ----------------

    #[test]
    fn notifier_history_is_bounded() {
        let n = Notifier::new();
        for i in 0..500 {
            n.notify("info", &format!("t{i}"), "m", None);
        }
        let h = n.history();
        assert_eq!(h.len(), 200, "history must be capped");
        assert_eq!(h.first().unwrap().title, "t300");
        assert_eq!(h.last().unwrap().title, "t499");
        // ids stay globally unique across the cap
        assert_eq!(h.last().unwrap().id, 499);
    }

    #[test]
    fn subscriber_receives_notification() {
        let n = Notifier::new();
        let mut rx = n.subscribe();
        n.notify("error", "boom", "details", Some(7));
        let got = rx.try_recv().unwrap();
        assert_eq!(got.title, "boom");
        assert_eq!(got.server_id, Some(7));
    }

    #[test]
    fn notifier_prunes_dead_subscribers() {
        let n = Notifier::new();
        let rx = n.subscribe();
        drop(rx);
        n.notify("info", "t", "m", None);
        assert!(n.subs.lock().is_empty(), "dead subscribers must be pruned");
    }

    fn join_reaper(ps: &ProcessState) {
        // Take the slot and release the lock before joining: the reaper
        // clears its own slot under the same lock at exit, so joining while
        // holding it would deadlock.
        let stale = ps.reaper.lock().take();
        if let Some(handle) = stale {
            handle.join().expect("reaper thread joins");
        }
    }

    #[test]
    fn stop_finalizes_when_nothing_running() {
        let m = test_manager();
        let ps = Arc::new(ProcessState::default());
        m.procs.insert(1, ps.clone());
        m.stop(1).unwrap();
        assert_eq!(*ps.status.lock(), "stopped");
        assert_eq!(*ps.exit_code.lock(), Some(0));
        assert!(ps.stop_issued.load(Ordering::Relaxed));
        assert!(ps.pid.lock().is_none());
    }

    #[test]
    fn stop_signals_child_and_reaper_applies_stopped_state() {
        let m = test_manager();
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        *ps.started_at.lock() = Some(Utc::now().to_rfc3339());
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);
        m.stop(1).unwrap();
        assert_eq!(*ps.status.lock(), "stopping");
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "stopped");
        assert_eq!(
            *ps.exit_code.lock(),
            Some(143),
            "killed by SIGTERM (15) => recorded as 128+15"
        );
        assert!(
            rx_done.try_recv().is_ok(),
            "reaper must signal completion after a requested stop"
        );
        assert!(
            ps.reaper.lock().is_none(),
            "reaper must release its own slot"
        );
    }

    #[test]
    fn kill_marks_stopped_and_reaper_cleans_up_without_restart_signal() {
        let m = test_manager();
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);

        m.kill(1).unwrap();
        assert!(ps.stop_issued.load(Ordering::Relaxed));
        assert_eq!(*ps.status.lock(), "stopped");
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "stopped");
        // stop_issued => reaper sends None, so the restart task must not fire.
        assert_eq!(
            rx_done.try_recv(),
            Ok(None),
            "requested stop must suppress auto-restart"
        );
    }

    #[test]
    fn reaper_marks_crash_and_reports_exit_code() {
        use std::os::unix::process::CommandExt;
        let m = test_manager();
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);

        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "crashed");
        assert_eq!(*ps.exit_code.lock(), Some(3));
        assert_eq!(
            rx_done.try_recv(),
            Ok(Some(3)),
            "unrequested exit must carry the code for restart handling"
        );
    }

    #[test]
    fn crash_reap_enqueues_server_crash_webhook_delivery() {
        use std::os::unix::process::CommandExt;
        let m = test_manager();
        let sid = insert_server(&m, false, false);
        // Subscribe a webhook to server.* scoped to this server so the crash
        // emit path (reap_child -> webhooks::emit -> deliveries) is covered.
        {
            let conn = m.db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"server.*\"]',?1,1,'now','now')",
                [sid],
            )
            .unwrap();
        }
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 3"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(sid, ps.clone());
        spawn_reaper(ps.clone(), &m, sid);

        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "crashed");

        // Exactly one server.crash delivery with the classified reason.
        let conn = m.db.get().unwrap();
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "server.crash");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], sid);
        assert_eq!(v["uuid"], "s");
        assert_eq!(v["status"], "crashed");
        assert_eq!(v["reason"], "exited with code 3");
        assert_eq!(v["exit_code"], 3);
    }

    #[test]
    fn stop_reap_enqueues_server_stop_webhook_delivery() {
        let m = test_manager();
        let sid = insert_server(&m, false, false);
        {
            let conn = m.db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"server.stop\"]',?1,1,'now','now')",
                [sid],
            )
            .unwrap();
        }
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(sid, ps.clone());
        spawn_reaper(ps.clone(), &m, sid);

        m.stop(sid).unwrap();
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "stopped");

        let conn = m.db.get().unwrap();
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "server.stop");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], sid);
        assert_eq!(v["status"], "stopped");
    }

    #[test]
    fn reaper_yields_cleanup_to_newer_owner() {
        let m = test_manager();
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);

        // A newer start() replaces the pid while the old reaper waits.
        *ps.pid.lock() = Some(999_999);
        kill_daemon_group(pid);
        let handle = ps.reaper.lock().take().expect("reaper slot held");
        handle.join().expect("reaper thread joins");

        // The stale reaper must not touch state owned by the newer process.
        assert_eq!(*ps.status.lock(), "running");
        assert_eq!(*ps.pid.lock(), Some(999_999));
        assert!(ps.network.lock().is_none());
        assert!(ps.cgroup.lock().is_none());
        assert!(
            ps.reaper.lock().is_none(),
            "reaper must still clear its slot"
        );
        assert!(
            rx_done.try_recv().is_err(),
            "stale reaper must stay silent on the oneshot"
        );
    }

    #[test]
    fn remove_limits_leaves_final_cleanup_to_reaper() {
        let m = test_manager();
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);

        m.remove_limits(1);
        assert!(m.state(1).is_none(), "entry must be removed");
        // remove_limits SIGKILLs; the reaper (not remove_limits) applies the
        // final state. If remove_limits cleared pid itself, the reaper's
        // ownership check would bail and leak network/cgroup here.
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "stopped");
        assert_eq!(rx_done.try_recv(), Ok(None));
        assert!(ps.reaper.lock().is_none());
    }
    #[test]
    fn failed_spawn_reaps_leader_without_zombie() {
        let child = spawn_sleep();
        let pid = child.id();
        // start()'s failure path: SIGKILL the group, then wait() — never a
        // bare drop of the Child handle, which would leave an unreaped
        // zombie that only a panel restart could collect.
        reap_failed_spawn(child);
        // Reaped => the pid no longer exists. An un-waited zombie still
        // answers kill(pid, 0) with success.
        let ret = unsafe { libc::kill(pid as i32, 0) };
        assert_eq!(ret, -1, "leader {pid} must be fully reaped");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "kill(pid, 0) must fail with ESRCH after reaping"
        );
    }

    #[test]
    fn start_gate_refuses_live_pid_and_suppressed_auto_restart() {
        let m = test_manager();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(424_242);
        ps.stop_issued.store(true, Ordering::Relaxed);
        m.procs.insert(1, ps.clone());

        // A child is still (or was just) running: no start may proceed,
        // regardless of intent.
        let err = m.gate_start(&ps, false).unwrap_err();
        assert!(err.to_string().contains("already running"));
        let err = m.gate_start(&ps, true).unwrap_err();
        assert!(err.to_string().contains("server is stopping"));
        assert_eq!(*ps.status.lock(), "", "the gate must not flip state");

        // Once the drain is gone (fully reaped stop), only the auto-restart
        // intent stays suppressed; the explicit start proceeds.
        *ps.pid.lock() = None;
        let err = m.gate_start(&ps, true).unwrap_err();
        assert!(
            err.to_string().contains("server is stopping"),
            "auto-restart must not resurrect a manually stopped server"
        );
        m.gate_start(&ps, false)
            .expect("manual start must pass once the stop drain is joined");
    }

    #[test]
    fn manual_restart_passes_gate_after_reaped_stop_without_deadlock() {
        // Full lifecycle: stop a live child, then let the start gate join the
        // still-draining reaper. The explicit start must pass (no deadlock,
        // no "server is stopping" poison) while the stop latch persists so an
        // auto-restart stays suppressed.
        let m = test_manager();
        let child = spawn_sleep();
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        spawn_reaper(ps.clone(), &m, 1);

        m.stop(1).unwrap();
        assert_eq!(*ps.status.lock(), "stopping");
        assert!(ps.stop_issued.load(Ordering::Relaxed));

        // Manual start right after stop: the gate takes the reaper slot (so
        // joining cannot self-deadlock), waits out the drain, and passes.
        m.gate_start(&ps, false)
            .expect("manual start after stop must join the drain and proceed");
        assert!(ps.pid.lock().is_none(), "reaper must have cleared the pid");
        assert!(
            ps.reaper.lock().is_none(),
            "the joined reaper must have released its slot"
        );
        assert!(
            ps.stop_issued.load(Ordering::Relaxed),
            "the stop latch must persist so auto-restart stays suppressed"
        );

        // Auto-restart intent on the same reaped state is refused.
        let err = m.gate_start(&ps, true).unwrap_err();
        assert!(err.to_string().contains("server is stopping"));
    }

    // ---------------- Crash classification & policy (G8) ----------------

    /// Insert a blueprint + server row so reaper/restart paths can read the
    /// crash policy from the DB (FK: servers.blueprint_id -> blueprints).
    fn insert_server(m: &ProcManager, auto_restart: bool, detect_clean_exit: bool) -> i64 {
        let conn = m.db.get().unwrap();
        conn.execute(
            "INSERT INTO blueprints(uuid,name,created_at,updated_at) VALUES('b','b','now','now')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users(username,email,password_hash,created_at,updated_at)
             VALUES('u','u@x','h','now','now')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO servers(uuid,name,user_id,blueprint_id,status,startup,auto_restart,crash_detect_clean_exit,crash_restart_budget,crash_restarts,created_at,updated_at) VALUES('s','srv',1,1,'running','echo hi',?1,?2,5,0,'now','now')",
            rusqlite::params![auto_restart as i64, detect_clean_exit as i64],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn classify_clean_exit_is_not_a_crash_by_default() {
        assert_eq!(
            classify_exit(Some(0), false, false),
            ExitKind::Clean,
            "zero exit must be clean under the default policy"
        );
    }

    #[test]
    fn classify_nonzero_and_signal_exits_are_crashes() {
        assert!(matches!(
            classify_exit(Some(3), false, false),
            ExitKind::Crash(_)
        ));
        assert!(matches!(
            classify_exit(None, false, false),
            ExitKind::Crash(_)
        ));
        assert!(matches!(
            classify_exit(Some(-1), false, false),
            ExitKind::Crash(_)
        ));
    }

    #[test]
    fn classify_operator_stop_is_never_a_crash() {
        // Even a nonzero/signal exit is Requested while stop_issued is set.
        assert_eq!(
            classify_exit(Some(3), true, false),
            ExitKind::Requested
        );
        assert_eq!(
            classify_exit(None, true, false),
            ExitKind::Requested
        );
        // ... and the clean-exit toggle never turns an operator stop into one.
        assert_eq!(
            classify_exit(Some(0), true, true),
            ExitKind::Requested
        );
    }

    #[test]
    fn classify_clean_exit_counts_as_crash_when_policy_enabled() {
        assert!(matches!(
            classify_exit(Some(0), false, true),
            ExitKind::Crash(_)
        ));
    }

    #[test]
    fn crash_backoff_grows_then_caps() {
        assert_eq!(crash_backoff(0), Duration::from_secs(5));
        assert_eq!(crash_backoff(1), Duration::from_secs(10));
        assert_eq!(crash_backoff(2), Duration::from_secs(20));
        assert_eq!(crash_backoff(3), Duration::from_secs(40));
        assert_eq!(crash_backoff(4), Duration::from_secs(60), "cap at 60s");
        assert_eq!(crash_backoff(9), Duration::from_secs(60), "never grows past cap");
    }

    #[tokio::test]
    async fn restart_budget_exhaustion_marks_crashed_and_stops() {
        // Budget 1, already consumed 1: the very next crash must give up —
        // no sleep, no spawn — and land in the terminal crashed state with
        // the reason recorded. This is the loop's termination branch.
        let m = test_manager();
        let sid = insert_server(&m, true, false);
        {
            let conn = m.db.get().unwrap();
            conn.execute(
                "UPDATE servers SET crash_restart_budget=1, crash_restarts=1 WHERE id=?1",
                [sid],
            )
            .unwrap();
        }
        let notifier = Arc::new(Notifier::default());
        let mut rx = notifier.subscribe();
        let server = models::get_server(&m.db, sid).unwrap();
        m.restart_if_needed(&server, notifier.clone(), Some(1)).await;
        let s = models::get_server(&m.db, sid).unwrap();
        assert_eq!(s.status, "crashed");
        assert!(
            s.crash_reason.contains("budget"),
            "reason must name the exhaustion: {}",
            s.crash_reason
        );
        assert_eq!(
            s.crash_restarts, 1,
            "an exhausted burst must not consume more slots"
        );
        let n = rx.try_recv().expect("crash notification expected");
        assert_eq!(n.level, "error");
        assert!(n.message.contains("budget"), "got: {}", n.message);
    }

    #[tokio::test]
    async fn restart_without_auto_restart_records_crash_but_never_restarts() {
        let m = test_manager();
        let sid = insert_server(&m, false, false);
        let notifier = Arc::new(Notifier::default());
        let mut rx = notifier.subscribe();
        let server = models::get_server(&m.db, sid).unwrap();
        m.restart_if_needed(&server, notifier.clone(), Some(2)).await;
        let s = models::get_server(&m.db, sid).unwrap();
        assert_eq!(s.status, "crashed");
        assert_eq!(s.crash_reason, "exited with code 2");
        assert_eq!(s.crash_restarts, 0, "no restart, so no budget consumed");
        assert_eq!(rx.try_recv().unwrap().level, "error");
    }

    #[test]
    fn reaper_clean_exit_default_is_stopped_not_crash() {
        use std::os::unix::process::CommandExt;
        let m = test_manager();
        insert_server(&m, false, false);
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(
            *ps.status.lock(),
            "stopped",
            "default policy: clean exit is a normal stop"
        );
        assert_eq!(rx_done.try_recv(), Ok(Some(0)));
    }

    #[test]
    fn reaper_clean_exit_counts_as_crash_when_policy_enabled() {
        use std::os::unix::process::CommandExt;
        let m = test_manager();
        let sid = insert_server(&m, false, true);
        let child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let ps = Arc::new(ProcessState::default());
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.status.lock() = "running".into();
        m.procs.insert(1, ps.clone());
        let mut rx_done = spawn_reaper(ps.clone(), &m, 1);
        wait_pid_cleared(&ps);
        join_reaper(&ps);
        assert_eq!(*ps.status.lock(), "crashed");
        let s = models::get_server(&m.db, sid).unwrap();
        assert_eq!(s.status, "crashed");
        assert!(
            s.crash_reason.contains("clean exit"),
            "reason must name the policy: {}",
            s.crash_reason
        );
        assert_eq!(rx_done.try_recv(), Ok(Some(0)));
    }
}