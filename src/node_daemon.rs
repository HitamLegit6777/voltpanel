//! Runtime used by the standalone `voltd` node daemon.
use crate::node_protocol::{NodeCapacity, ProvisionRequest, RemoteFileEntry, RemoteServerStats, ServerSpec};
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Notify};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub listen: String,
    pub data_dir: PathBuf,
    pub panel_url: String,
    pub node_id: String,
    pub secret: String,
    pub heartbeat_interval_secs: u64,
    pub max_upload_mb: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8081".into(),
            data_dir: PathBuf::from("./voltd-data"),
            panel_url: String::new(),
            node_id: String::new(),
            secret: String::new(),
            heartbeat_interval_secs: 15,
            max_upload_mb: 256,
        }
    }
}

impl DaemonConfig {
    pub fn load(path: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let text = fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?; }
        fs::write(path, toml::to_string_pretty(self)?)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [self.data_dir.clone(), self.servers_dir(), self.logs_dir(), self.meta_dir()] {
            fs::create_dir_all(&dir)?; fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn servers_dir(&self) -> PathBuf { self.data_dir.join("servers") }
    pub fn logs_dir(&self) -> PathBuf { self.data_dir.join("logs") }
    pub fn meta_dir(&self) -> PathBuf { self.data_dir.join("meta") }
}

#[derive(Debug)]
pub struct ManagedProcess {
    pub spec: Mutex<ServerSpec>,
    pub child: Mutex<Option<Child>>,
    pub stdin: Mutex<Option<ChildStdin>>,
    pub cgroup: Mutex<Option<crate::isolation::Cgroup>>,
    pub network: Mutex<Option<crate::isolation::NetworkLease>>,
    pub pid: Mutex<Option<u32>>,
    pub state: Mutex<String>,
    pub started: Mutex<Option<Instant>>,
    pub exit_code: Mutex<Option<i32>>,
    pub restart_count: AtomicU64,
    pub last_cpu_ticks: AtomicU64,
    pub last_sample_ms: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub stopping: AtomicBool,
    pub console: Mutex<VecDeque<(u64, String)>>,
    pub console_cursor: AtomicU64,
    pub console_tx: broadcast::Sender<(u64, String)>,
}

impl ManagedProcess {
    fn new(spec: ServerSpec) -> Self {
        let (console_tx, _) = broadcast::channel(1024);
        Self {
            spec: Mutex::new(spec), child: Mutex::new(None), stdin: Mutex::new(None), pid: Mutex::new(None), cgroup: Mutex::new(None), network: Mutex::new(None),
            state: Mutex::new("offline".into()), started: Mutex::new(None), exit_code: Mutex::new(None),
            restart_count: AtomicU64::new(0), last_cpu_ticks: AtomicU64::new(0), last_sample_ms: AtomicU64::new(now_ms()),
            rx_bytes: AtomicU64::new(0), tx_bytes: AtomicU64::new(0), stopping: AtomicBool::new(false),
            console: Mutex::new(VecDeque::with_capacity(1000)), console_cursor: AtomicU64::new(0), console_tx,
        }
    }

    fn append_console(&self, text: String) {
        if text.is_empty() { return; }
        let cursor = self.console_cursor.fetch_add(1, Ordering::Relaxed) + 1;
        let mut buf = self.console.lock();
        if buf.len() >= 1000 { buf.pop_front(); }
        buf.push_back((cursor, text.clone()));
        drop(buf);
        let _ = self.console_tx.send((cursor, text));
    }
}

#[derive(Clone)]
pub struct DaemonRuntime {
    pub config: Arc<DaemonConfig>,
    pub processes: Arc<DashMap<String, Arc<ManagedProcess>>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    shutdown: Arc<Notify>,
}

impl DaemonRuntime {
    pub fn new(config: DaemonConfig) -> Result<Self> {
        config.ensure_dirs()?;
        let rt = Self {
            config: Arc::new(config), processes: Arc::new(DashMap::new()), started_at: chrono::Utc::now(), shutdown: Arc::new(Notify::new()),
        };
        rt.load_specs()?;
        Ok(rt)
    }

    fn load_specs(&self) -> Result<()> {
        let dir = self.config.meta_dir();
        if !dir.exists() { return Ok(()); }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") { continue; }
            let bytes = fs::read(entry.path())?;
            if let Ok(spec) = serde_json::from_slice::<ServerSpec>(&bytes) {
                crate::isolation::cleanup_orphans(&spec.uuid);
                let root = self.config.servers_dir().join(&spec.uuid);
                crate::isolation::prepare_root(&root, &spec.uuid)?;
                crate::isolation::own_tree(&root, &spec.uuid)?;
                self.processes.insert(spec.uuid.clone(), Arc::new(ManagedProcess::new(spec)));
            }
        }
        Ok(())
    }

    fn persist_spec(&self, spec: &ServerSpec) -> Result<()> {
        fs::write(self.config.meta_dir().join(format!("{}.json", spec.uuid)), serde_json::to_vec_pretty(spec)?)?;
        Ok(())
    }

    pub fn provision(&self, req: ProvisionRequest) -> Result<RemoteServerStats> {
        validate_uuid(&req.spec.uuid)?;
        let root = self.server_root(&req.spec.uuid)?;
        fs::create_dir_all(&root)?;
        self.persist_spec(&req.spec)?;
        for file in req.files {
            let target = safe_join(&root, &file.path)?;
            if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, file.content_b64)?;
            fs::write(&target, bytes)?;
            if let Some(mode) = file.mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            }
        }
        crate::isolation::own_tree(&root, &req.spec.uuid)?;
        let uuid = req.spec.uuid.clone();
        let proc = self.processes.entry(uuid.clone()).or_insert_with(|| Arc::new(ManagedProcess::new(req.spec.clone()))).clone();
        *proc.spec.lock() = req.spec;
        self.stats(&uuid)
    }

    pub fn remove_server(&self, uuid: &str) -> Result<bool> {
        if let Some(proc) = self.processes.get(uuid) {
            if proc.pid.lock().is_some() { bail!("server must be stopped before deletion"); }
        }
        self.processes.remove(uuid);
        let root = self.server_root(uuid)?;
        if root.exists() { fs::remove_dir_all(root)?; }
        let meta = self.config.meta_dir().join(format!("{uuid}.json"));
        if meta.exists() { fs::remove_file(meta)?; }
        Ok(true)
    }

    pub fn start(&self, uuid: &str) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        if proc.pid.lock().is_some() { bail!("server already running"); }
        let spec = proc.spec.lock().clone();
        let root = self.server_root(uuid)?;
        crate::isolation::prepare_root(&root, uuid)?;
        let isolation = crate::isolation::IsolationConfig::default();
        let limits = crate::isolation::Limits { memory_bytes: spec.memory_mb * 1_048_576, cpu_percent: spec.cpu_percent, pids_max: crate::isolation::DEFAULT_PIDS_MAX };
        let cgroup = crate::isolation::Cgroup::create(&isolation, uuid, &limits)?;
        let mut cmd = crate::isolation::sandbox_command(&isolation, &root, uuid, &spec.startup, &limits)?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        for (k, v) in &spec.env { cmd.env(k, v); }
        cmd.env("SERVER_UUID", &spec.uuid).env("SERVER_NAME", &spec.name).env("SERVER_MEMORY", spec.memory_mb.to_string());
        let mut child = cmd.spawn().context("sandbox spawn failed")?;
        let pid = child.id();
        let ports = if spec.ports.is_empty() { spec.port.into_iter().collect::<Vec<_>>() } else { spec.ports.clone() };
        let network = match crate::isolation::NetworkLease::configure(pid, uuid, &ports) {
            Ok(value) => value,
            Err(error) => { let _ = child.kill(); return Err(error); }
        };
        *proc.network.lock() = Some(network);
        if let Err(error) = cgroup.attach(pid) { let _ = child.kill(); return Err(error); }
        *proc.cgroup.lock() = Some(cgroup);
        let stdout = child.stdout.take().context("missing stdout")?;
        let stderr = child.stderr.take().context("missing stderr")?;
        *proc.stdin.lock() = child.stdin.take();
        *proc.pid.lock() = Some(pid);
        *proc.state.lock() = "running".into();
        *proc.started.lock() = Some(Instant::now());
        *proc.exit_code.lock() = None;
        proc.stopping.store(false, Ordering::Relaxed);
        *proc.child.lock() = Some(child);
        spawn_reader(proc.clone(), stdout, false);
        spawn_reader(proc.clone(), stderr, true);
        self.spawn_waiter(proc.clone());
        self.stats(uuid)
    }

    fn spawn_waiter(&self, proc: Arc<ManagedProcess>) {
        let rt = self.clone();
        tokio::spawn(async move {
            let mut child = match proc.child.lock().take() { Some(c) => c, None => return };
            let exit = tokio::task::spawn_blocking(move || child.wait()).await.ok().and_then(Result::ok);
            proc.network.lock().take();
            *proc.pid.lock() = None;
            *proc.stdin.lock() = None;
            if let Some(cgroup) = proc.cgroup.lock().as_ref() { let _ = cgroup.kill_all(); }
            let code = exit.and_then(|s| s.code()).unwrap_or(-1);
            *proc.exit_code.lock() = Some(code);
            let requested = proc.stopping.load(Ordering::Relaxed);
            *proc.state.lock() = if requested { "offline".into() } else { "crashed".into() };
            let auto = proc.spec.lock().auto_restart;
            if !requested && auto {
                tokio::time::sleep(Duration::from_secs(5)).await;
                proc.restart_count.fetch_add(1, Ordering::Relaxed);
                let uuid = proc.spec.lock().uuid.clone();
                let _ = rt.start(&uuid);
            }
        });
    }

    pub fn stop(&self, uuid: &str, force: bool) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        proc.stopping.store(true, Ordering::Relaxed);
        let pid = *proc.pid.lock();
        if force { if let Some(cgroup) = proc.cgroup.lock().as_ref() { let _ = cgroup.kill_all(); } }
        let Some(pid) = pid else { *proc.state.lock() = "offline".into(); return self.stats(uuid); };
        *proc.state.lock() = "stopping".into();
        unsafe {
            libc::kill(-(pid as i32), if force { libc::SIGKILL } else { libc::SIGTERM });
            libc::kill(pid as i32, if force { libc::SIGKILL } else { libc::SIGTERM });
        }
        if !force {
            let proc2=proc.clone(); let captured=pid;
            tokio::spawn(async move { tokio::time::sleep(Duration::from_secs(10)).await; if *proc2.pid.lock()==Some(captured){ if let Some(cgroup)=proc2.cgroup.lock().as_ref(){let _=cgroup.kill_all();} unsafe{libc::kill(-(captured as i32),libc::SIGKILL);libc::kill(captured as i32,libc::SIGKILL);} } });
        }
        self.stats(uuid)
    }

    pub async fn restart(&self, uuid: &str) -> Result<RemoteServerStats> {
        let _ = self.stop(uuid, false)?;
        for _ in 0..100 {
            if self.process(uuid)?.pid.lock().is_none() { break; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.start(uuid)
    }

    pub fn command(&self, uuid: &str, command: &str) -> Result<bool> {
        let proc = self.process(uuid)?;
        let mut stdin = proc.stdin.lock();
        let input = stdin.as_mut().context("server not running or stdin unavailable")?;
        input.write_all(command.as_bytes())?;
        if !command.ends_with('\n') { input.write_all(b"\n")?; }
        input.flush()?;
        proc.tx_bytes.fetch_add(command.len() as u64, Ordering::Relaxed);
        Ok(true)
    }

    pub fn stats(&self, uuid: &str) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        let pid = *proc.pid.lock();
        let (cpu, memory) = pid.map(|p| process_tree_usage(p, &proc)).unwrap_or((0.0, 0));
        let state = proc.state.lock().clone();
        let uptime_secs = proc.started.lock().map(|i| i.elapsed().as_secs()).unwrap_or(0);
        let exit_code = *proc.exit_code.lock();
        Ok(RemoteServerStats {
            uuid: uuid.into(), state, pid, cpu_percent: cpu, memory_bytes: memory,
            disk_bytes: dir_size(&self.server_root(uuid)?), network_rx_bytes: proc.rx_bytes.load(Ordering::Relaxed),
            network_tx_bytes: proc.tx_bytes.load(Ordering::Relaxed), uptime_secs,
            restart_count: proc.restart_count.load(Ordering::Relaxed), exit_code,
        })
    }

    pub fn console(&self, uuid: &str, after: u64) -> Result<(Vec<String>, u64)> {
        let proc = self.process(uuid)?;
        let buf = proc.console.lock();
        let lines = buf.iter().filter(|(cursor, _)| *cursor > after).map(|(_, line)| line.clone()).collect();
        Ok((lines, proc.console_cursor.load(Ordering::Relaxed)))
    }

    pub fn list_files(&self, uuid: &str, path: &str) -> Result<Vec<RemoteFileEntry>> {
        let root = self.server_root(uuid)?;
        let dir = safe_join(&root, path)?;
        let mut values = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            use std::os::unix::fs::PermissionsExt;
            values.push(RemoteFileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: format!("/{}", entry.path().strip_prefix(&root).unwrap_or(&entry.path()).to_string_lossy()),
                is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() },
                modified: meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0),
                mode: meta.permissions().mode(),
            });
        }
        values.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(values)
    }

    pub fn read_file(&self, uuid: &str, path: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let target = safe_join(&self.server_root(uuid)?, path)?;
        let meta = fs::metadata(&target)?;
        if meta.len() > max_bytes { bail!("file exceeds read limit"); }
        Ok(fs::read(target)?)
    }

    pub fn write_file(&self, uuid: &str, path: &str, data: &[u8], append: bool) -> Result<bool> {
        if data.len() as u64 > self.config.max_upload_mb * 1_048_576 { bail!("upload too large"); }
        let target = safe_join(&self.server_root(uuid)?, path)?;
        if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
        if append {
            fs::OpenOptions::new().create(true).append(true).open(target)?.write_all(data)?;
        } else { fs::write(target, data)?; }
        Ok(true)
    }

    pub fn file_operation(&self, uuid: &str, operation: crate::node_protocol::FileOperation) -> Result<bool> {
        use crate::node_protocol::FileOperation;
        use std::os::unix::fs::PermissionsExt;
        let root = self.server_root(uuid)?;
        match operation {
            FileOperation::Mkdir { path } => fs::create_dir_all(safe_join(&root, &path)?)?,
            FileOperation::Touch { path } => { let target = safe_join(&root, &path)?; if let Some(p) = target.parent() { fs::create_dir_all(p)?; } fs::OpenOptions::new().create(true).append(true).open(target)?; },
            FileOperation::Rename { from, to } => fs::rename(safe_join(&root, &from)?, safe_join(&root, &to)?)?,
            FileOperation::Copy { from, to } => copy_recursive(&safe_join(&root, &from)?, &safe_join(&root, &to)?)?,
            FileOperation::Move { from, destination } => { let from = safe_join(&root, &from)?; let dir = safe_join(&root, &destination)?; fs::create_dir_all(&dir)?; fs::rename(&from, dir.join(from.file_name().context("invalid filename")?))?; },
            FileOperation::Delete { path } => { let target = safe_join(&root, &path)?; if target.is_dir() { fs::remove_dir_all(target)?; } else { fs::remove_file(target)?; } },
            FileOperation::Chmod { path, mode } => fs::set_permissions(safe_join(&root, &path)?, fs::Permissions::from_mode(mode))?,
        }
        Ok(true)
    }

    pub fn capacity(&self) -> NodeCapacity {
        use sysinfo::{Disks, System};
        let mut sys = System::new_all();
        sys.refresh_all();
        let disks = Disks::new_with_refreshed_list();
        let disk_total = disks.iter().map(|d| d.total_space()).sum::<u64>();
        let disk_used = disks.iter().map(|d| d.total_space().saturating_sub(d.available_space())).sum::<u64>();
        let load = System::load_average();
        NodeCapacity {
            memory_total: sys.total_memory(), memory_used: sys.used_memory(), disk_total, disk_used,
            cpu_percent: sys.global_cpu_info().cpu_usage() as f64, cpu_threads: sys.cpus().len(),
            load_1: load.one, load_5: load.five, load_15: load.fifteen,
            servers_running: self.processes.iter().filter(|p| p.pid.lock().is_some()).count(), servers_total: self.processes.len(),
        }
    }

    pub fn shutdown(&self) { self.shutdown.notify_waiters(); }
    pub fn snapshot(&self, uuid: &str) -> Result<crate::node_protocol::SnapshotResponse> {
        use sha2::{Digest, Sha256};
        let root = self.server_root(uuid)?;
        let temp = tempfile::NamedTempFile::new_in(&self.config.data_dir)?;
        let encoder = flate2::write::GzEncoder::new(temp.reopen()?, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        tar.append_dir_all(".", root)?;
        let encoder = tar.into_inner()?;
        encoder.finish()?;
        let bytes = fs::read(temp.path())?;
        let checksum = hex::encode(Sha256::digest(&bytes));
        Ok(crate::node_protocol::SnapshotResponse { archive_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes), size_bytes: bytes.len() as u64, checksum })
    }

    pub fn restore_snapshot(&self, uuid: &str, req: crate::node_protocol::RestoreSnapshotRequest) -> Result<bool> {
        use sha2::{Digest, Sha256};
        if self.process(uuid)?.pid.lock().is_some() { bail!("server must be stopped before restore"); }
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, req.archive_b64)?;
        if hex::encode(Sha256::digest(&bytes)) != req.checksum { bail!("snapshot checksum mismatch"); }
        let root=self.server_root(uuid)?;
        let validate=flate2::read::GzDecoder::new(bytes.as_slice());
        for entry in tar::Archive::new(validate).entries()? { let entry=entry?; let kind=entry.header().entry_type(); if kind.is_symlink()||kind.is_hard_link(){bail!("snapshot contains forbidden link entry")}; let path=entry.path()?; if path.components().any(|c|matches!(c,Component::ParentDir|Component::RootDir)){bail!("snapshot path traversal")}; }
        if root.exists(){fs::remove_dir_all(&root)?;} fs::create_dir_all(&root)?;
        let decoder=flate2::read::GzDecoder::new(bytes.as_slice());
        tar::Archive::new(decoder).unpack(&root)?;
        crate::isolation::prepare_root(&root,uuid)?;
        crate::isolation::own_tree(&root,uuid)?;
        Ok(true)
    }

    pub fn shutdown_notify(&self) -> Arc<Notify> { self.shutdown.clone() }

    fn process(&self, uuid: &str) -> Result<Arc<ManagedProcess>> {
        self.processes.get(uuid).map(|v| v.clone()).context("server not provisioned")
    }

    fn server_root(&self, uuid: &str) -> Result<PathBuf> {
        validate_uuid(uuid)?;
        Ok(self.config.servers_dir().join(uuid))
    }
}

fn validate_uuid(uuid: &str) -> Result<()> {
    if uuid.is_empty() || uuid.len() > 64 || !uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') { bail!("invalid server uuid"); }
    Ok(())
}

fn copy_recursive(source: &Path, target: &Path) -> Result<()> {
    if source.is_dir() {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &target.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = target.parent() { fs::create_dir_all(parent)?; }
        fs::copy(source, target)?;
    }
    Ok(())
}

pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let canonical_root = root.canonicalize().with_context(|| format!("server root {} missing", root.display()))?;
    let mut out = canonical_root.clone();
    for part in Path::new(relative.trim_start_matches('/')).components() {
        match part {
            Component::Normal(v) => {
                out.push(v);
                if let Ok(meta) = fs::symlink_metadata(&out) {
                    if meta.file_type().is_symlink() { bail!("symlink traversal rejected at {}", out.display()); }
                }
            }
            Component::CurDir => {},
            _ => bail!("path traversal rejected"),
        }
    }
    if !out.starts_with(&canonical_root) { bail!("path escapes server root"); }
    Ok(out)
}

fn spawn_reader<R: Read + Send + 'static>(proc: Arc<ManagedProcess>, mut reader: R, stderr: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    proc.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    let mut text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if stderr { text = format!("[stderr] {text}"); }
                    proc.append_console(text);
                }
            }
        }
    });
}

fn process_tree_usage(pid: u32, proc: &ManagedProcess) -> (f64, u64) {
    let mut memory = 0u64;
    let mut ticks = 0u64;
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some((rss, cpu)) = read_proc(current) { memory = memory.saturating_add(rss); ticks = ticks.saturating_add(cpu); }
        if let Ok(children) = fs::read_to_string(format!("/proc/{current}/task/{current}/children")) {
            stack.extend(children.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
        }
    }
    let now = now_ms();
    let previous_ticks = proc.last_cpu_ticks.swap(ticks, Ordering::Relaxed);
    let previous_ms = proc.last_sample_ms.swap(now, Ordering::Relaxed);
    let elapsed = now.saturating_sub(previous_ms).max(1);
    let delta = ticks.saturating_sub(previous_ticks);
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let cpu = delta as f64 / hz / (elapsed as f64 / 1000.0) * 100.0;
    (if cpu.is_finite() { cpu } else { 0.0 }, memory)
}

fn read_proc(pid: u32) -> Option<(u64, u64)> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    let rss_pages: u64 = fields.get(21)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    Some((rss_pages.saturating_mul(page), user + system))
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path).follow_links(false).into_iter().filter_map(Result::ok).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        fs::create_dir_all(&root).unwrap();
        assert!(safe_join(&root, "files/a.txt").unwrap().starts_with(&root));
        assert!(safe_join(&root, "../etc/passwd").is_err());
        assert!(safe_join(&root, "/../../root").is_err());
    }

    #[test]
    fn safe_join_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        assert!(safe_join(&root, "escape").is_err());
    }
}
