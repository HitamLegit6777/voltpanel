//! Process manager: spawn/kill/status for server processes, resource limits,
//! /proc-based resource sampling, bandwidth accounting, auto-kill on overage.
use crate::db::Db;
use crate::models::{self, Server};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

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
        self.history.lock().push(n.clone());
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
}

pub struct ProcManager {
    pub db: Db,
    pub hub: Arc<crate::services::console::ConsoleHub>,
    pub procs: DashMap<i64, Arc<ProcessState>>,
    cfg_limits: Mutex<HashMap<i64, (i64, i64)>>,
    fs_cache: Mutex<HashMap<i64, (u64, std::time::Instant)>>,
    stop_evt: Arc<Notify>,
    stopped: AtomicBool,
}

impl ProcManager {
    pub fn new(db: Db, hub: Arc<crate::services::console::ConsoleHub>) -> Self {
        Self {
            db,
            hub,
            procs: DashMap::new(),
            cfg_limits: Mutex::new(HashMap::new()),
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

    pub fn register_limits(&self, server_id: i64, mem_mb: i64, cpu_pct: i64) {
        self.cfg_limits.lock().insert(server_id, (mem_mb, cpu_pct));
    }

    pub fn remove_limits(&self, server_id: i64) {
        self.cfg_limits.lock().remove(&server_id);
        self.fs_cache.lock().remove(&server_id);
        if let Some((_, p)) = self.procs.remove(&server_id) {
            *p.stdin.lock() = None;
            let pid = *p.pid.lock();
            if let Some(cgroup) = p.cgroup.lock().as_ref() {
                let _ = cgroup.kill_all();
            }
            if let Some(pid) = pid {
                unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
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
    pub fn start(
        &self,
        server: &Server,
        startup_cmd: &str,
        env: &[(String, String)],
        notifier: Arc<Notifier>,
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
        if ps.pid.lock().is_some() {
            bail!("server already running")
        }
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
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().context("failed to spawn isolated process")?;
        let pid = child.id();
        if let Err(error) = cgroup.attach(pid) {
            let _ = cgroup.kill_all();
            let _ = child.kill();
            return Err(error);
        }
        let stdout = child.stdout.take().context("no stdout")?;
        let stderr = child.stderr.take().context("no stderr")?;
        let stdin = child.stdin.take();
        let ports = crate::models::ports_for_server(&self.db, server.id)?
            .into_iter()
            .filter_map(|p| u16::try_from(p).ok())
            .collect::<Vec<_>>();
        let network = match crate::isolation::NetworkLease::configure(pid, &server.uuid, &ports) {
            Ok(v) => v,
            Err(error) => {
                let _ = cgroup.kill_all();
                let _ = child.kill();
                return Err(error);
            }
        };
        *ps.cgroup.lock() = Some(cgroup);
        *ps.network.lock() = Some(network);
        *ps.status.lock() = "running".into();
        *ps.started_at.lock() = Some(Utc::now().to_rfc3339());
        *ps.exit_code.lock() = None;
        ps.stop_issued.store(false, Ordering::Relaxed);
        *ps.pid.lock() = Some(pid);
        *ps.child.lock() = Some(child);
        *ps.stdin.lock() = stdin;
        ps.last_cpu_read.store(now_ticks(), Ordering::Relaxed);
        ps.last_cpu_time.store(0, Ordering::Relaxed);
        let _ = models::set_server_status(&self.db, server.id, "running");

        // readers forward output into the app-wide console hub
        let hub = self.hub.clone();
        let (tx_out, mut rx_out) = mpsc::unbounded_channel::<Vec<u8>>();
        let (tx_err, mut rx_err) = mpsc::unbounded_channel::<Vec<u8>>();
        let ps_out = ps.clone();
        std::thread::spawn(move || pump_stream(stdout, ps_out, tx_out));
        let ps_err = ps.clone();
        std::thread::spawn(move || pump_stream(stderr, ps_err, tx_err));
        let sid = server.id;
        let hub2 = hub.clone();
        tokio::spawn(async move {
            while let Some(chunk) = rx_out.recv().await {
                hub2.append(sid, &String::from_utf8_lossy(&chunk)).await;
            }
        });
        let sid = server.id;
        let hub3 = hub.clone();
        tokio::spawn(async move {
            while let Some(chunk) = rx_err.recv().await {
                hub3.append(sid, &String::from_utf8_lossy(&chunk)).await;
            }
        });

        // wait + auto-restart + exit notification
        let m = Arc::new(ProcManager {
            db: self.db.clone(),
            hub: self.hub.clone(),
            procs: self.procs.clone(),
            cfg_limits: Mutex::new(self.cfg_limits.lock().clone()),
            fs_cache: Mutex::new(self.fs_cache.lock().clone()),
            stop_evt: self.stop_evt.clone(),
            stopped: AtomicBool::new(self.stopped.load(Ordering::Relaxed)),
        });
        let sid = server.id;
        let srv = server.clone();
        let not = notifier.clone();
        let st = ps.clone();
        tokio::spawn(async move {
            let mut child = match st.child.lock().take() {
                Some(c) => c,
                None => return,
            };
            let my_pid = child.id();
            let code = child.wait().ok().and_then(|s| s.code());
            st.network.lock().take();
            // a newer start() (auto-restart) or stop()/kill() may have replaced us
            if *st.pid.lock() != Some(my_pid) {
                return;
            }
            *st.pid.lock() = None;
            *st.exit_code.lock() = Some(code.unwrap_or(-1));
            if !st.stop_issued.load(Ordering::Relaxed) {
                *st.status.lock() = if code == Some(0) {
                    "stopped".into()
                } else {
                    "crashed".into()
                };
                let _ = models::set_server_status(&m.db, sid, &st.status.lock());
                m.restart_if_needed(&srv, not, code).await;
            }
        });
        Ok(())
    }

    async fn restart_if_needed(&self, server: &Server, notifier: Arc<Notifier>, code: Option<i32>) {
        if !server.auto_restart {
            if let Some(code) = code {
                if code != 0 {
                    notifier.notify(
                        "error",
                        &format!("Server '{}' crashed", server.name),
                        &format!("Exit code {code}"),
                        Some(server.id),
                    );
                }
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if self
            .state(server.id)
            .map(|s| s.stop_issued.load(Ordering::Relaxed))
            .unwrap_or(true)
        {
            return;
        }
        let srv = match models::get_server(&self.db, server.id) {
            Ok(s) => s,
            Err(_) => return,
        };
        if !srv.auto_restart || srv.suspended {
            return;
        }
        let _ = models::bump_restart_count(&self.db, srv.id);
        let cmd = crate::services::egg::resolve_startup(&self.db, &srv);
        let env = crate::services::egg::env_for_server(&self.db, &srv);
        match cmd {
            Ok(cmd) => {
                notifier.notify(
                    "info",
                    &format!("Restarting '{}'", srv.name),
                    "Auto-restart triggered after exit",
                    Some(srv.id),
                );
                let _ = self.start(&srv, &cmd, &env, notifier.clone());
            }
            Err(e) => notifier.notify("error", "Restart failed", &e.to_string(), Some(srv.id)),
        }
    }

    /// Graceful stop (SIGTERM, then SIGKILL after grace window). Kills by PID so
    /// it works even after the wait task took ownership of the Child handle.
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
        if let Some(pid) = pid {
            // signal the whole group: the sh leader may die but children must stop too
            unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            let t0 = std::time::Instant::now();
            loop {
                let group_alive = unsafe { libc::kill(-(pid as i32), 0) == 0 };
                if !group_alive {
                    break;
                }
                if t0.elapsed().as_secs() > 10 {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    if let Some(cgroup) = ps.cgroup.lock().as_ref() {
                        let _ = cgroup.kill_all();
                    }
                    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            *ps.pid.lock() = None;
        }
        *ps.status.lock() = "stopped".into();
        *ps.exit_code.lock() = Some(0);
        *ps.started_at.lock() = None;
        let _ = models::set_server_status(&self.db, server_id, "offline");
        Ok(())
    }

    /// Immediate kill, no grace (used by monitor auto-kill / kill action).
    pub fn kill(&self, server_id: i64) -> Result<()> {
        let Some(ps) = self.state(server_id) else {
            return Ok(());
        };
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&ps.operation)?;
        *ps.stdin.lock() = None;
        let pid = *ps.pid.lock();
        if let Some(pid) = pid {
            if let Some(cgroup) = ps.cgroup.lock().as_ref() {
                let _ = cgroup.kill_all();
            }
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            *ps.pid.lock() = None;
        }
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

fn pump_stream(mut stream: impl Read, ps: Arc<ProcessState>, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                ps.read_total.fetch_add(n as u64, Ordering::Relaxed);
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
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
    let rest = s.split(')').nth(1)?;
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
