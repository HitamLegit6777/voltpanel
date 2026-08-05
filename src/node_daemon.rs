//! Runtime used by the standalone `voltd` execution agent.
use crate::node_protocol::{
    NodeCapacity, ProvisionRequest, RemoteFileEntry, RemoteServerStats, ServerSpec,
};
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
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
        let text =
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        fs::write(path, toml::to_string_pretty(self)?)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            self.data_dir.clone(),
            self.servers_dir(),
            self.logs_dir(),
            self.meta_dir(),
        ] {
            fs::create_dir_all(&dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn servers_dir(&self) -> PathBuf {
        self.data_dir.join("servers")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.data_dir.join("meta")
    }
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
    pub operation: AtomicBool,
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
            spec: Mutex::new(spec),
            child: Mutex::new(None),
            stdin: Mutex::new(None),
            pid: Mutex::new(None),
            cgroup: Mutex::new(None),
            network: Mutex::new(None),
            state: Mutex::new("offline".into()),
            started: Mutex::new(None),
            exit_code: Mutex::new(None),
            operation: AtomicBool::new(false),
            restart_count: AtomicU64::new(0),
            last_cpu_ticks: AtomicU64::new(0),
            last_sample_ms: AtomicU64::new(now_ms()),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
            console: Mutex::new(VecDeque::with_capacity(1000)),
            console_cursor: AtomicU64::new(0),
            console_tx,
        }
    }

    fn append_console(&self, text: String) {
        if text.is_empty() {
            return;
        }
        let cursor = self.console_cursor.fetch_add(1, Ordering::Relaxed) + 1;
        let mut buf = self.console.lock();
        if buf.len() >= 1000 {
            buf.pop_front();
        }
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
            config: Arc::new(config),
            processes: Arc::new(DashMap::new()),
            started_at: chrono::Utc::now(),
            shutdown: Arc::new(Notify::new()),
        };
        rt.load_specs()?;
        Ok(rt)
    }

    fn load_specs(&self) -> Result<()> {
        let dir = self.config.meta_dir();
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            if let Ok(spec) = serde_json::from_slice::<ServerSpec>(&bytes) {
                crate::isolation::cleanup_orphans(&spec.uuid);
                let root = self.config.servers_dir().join(&spec.uuid);
                crate::isolation::prepare_root(&root, &spec.uuid)?;
                crate::isolation::own_tree(&root, &spec.uuid)?;
                self.processes
                    .insert(spec.uuid.clone(), Arc::new(ManagedProcess::new(spec)));
            }
        }
        Ok(())
    }

    fn persist_spec(&self, spec: &ServerSpec) -> Result<()> {
        fs::write(
            self.config.meta_dir().join(format!("{}.json", spec.uuid)),
            serde_json::to_vec_pretty(spec)?,
        )?;
        Ok(())
    }

    pub fn provision(&self, req: ProvisionRequest) -> Result<RemoteServerStats> {
        validate_uuid(&req.spec.uuid)?;
        let root = self.server_root(&req.spec.uuid)?;
        fs::create_dir_all(&root)?;
        self.persist_spec(&req.spec)?;
        for file in req.files {
            let target = safe_join(&root, &file.path)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let bytes = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                file.content_b64,
            )?;
            fs::write(&target, bytes)?;
            if let Some(mode) = file.mode {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            }
        }
        crate::isolation::own_tree(&root, &req.spec.uuid)?;
        let uuid = req.spec.uuid.clone();
        let proc = self
            .processes
            .entry(uuid.clone())
            .or_insert_with(|| Arc::new(ManagedProcess::new(req.spec.clone())))
            .clone();
        *proc.spec.lock() = req.spec;
        self.stats(&uuid)
    }

    pub fn remove_server(&self, uuid: &str) -> Result<bool> {
        let proc = self.process(uuid)?;
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&proc.operation)?;
        if proc.pid.lock().is_some() {
            bail!("server must be stopped before deletion")
        }
        self.processes.remove(uuid);
        let root = self.server_root(uuid)?;
        if root.exists() {
            fs::remove_dir_all(root)?;
        }
        let meta = self.config.meta_dir().join(format!("{uuid}.json"));
        if meta.exists() {
            fs::remove_file(meta)?;
        }
        Ok(true)
    }

    pub fn start(&self, uuid: &str) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&proc.operation)?;
        if proc.pid.lock().is_some() {
            bail!("server already running");
        }
        let spec = proc.spec.lock().clone();
        let root = self.server_root(uuid)?;
        crate::isolation::prepare_root(&root, uuid)?;
        let isolation = crate::isolation::IsolationConfig::default();
        let limits = crate::isolation::Limits {
            memory_bytes: spec.memory_mb * 1_048_576,
            cpu_percent: spec.cpu_percent,
            pids_max: crate::isolation::DEFAULT_PIDS_MAX,
        };
        let cgroup = crate::isolation::Cgroup::create(&isolation, uuid, &limits)?;
        let mut cmd =
            crate::isolation::sandbox_command(&isolation, &root, uuid, &spec.startup, &limits)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.env("SERVER_UUID", &spec.uuid)
            .env("SERVER_NAME", &spec.name)
            .env("SERVER_MEMORY", spec.memory_mb.to_string());
        let mut child = cmd.spawn().context("sandbox spawn failed")?;
        let pid = child.id();
        let ports = if spec.ports.is_empty() {
            spec.port.into_iter().collect::<Vec<_>>()
        } else {
            spec.ports.clone()
        };
        let network = match crate::isolation::NetworkLease::configure(pid, uuid, &ports) {
            Ok(value) => value,
            Err(error) => {
                let _ = child.kill();
                return Err(error);
            }
        };
        *proc.network.lock() = Some(network);
        if let Err(error) = cgroup.attach(pid) {
            let _ = child.kill();
            return Err(error);
        }
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
            let mut child = match proc.child.lock().take() {
                Some(c) => c,
                None => return,
            };
            let exit = tokio::task::spawn_blocking(move || child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            proc.network.lock().take();
            *proc.pid.lock() = None;
            *proc.stdin.lock() = None;
            if let Some(cgroup) = proc.cgroup.lock().as_ref() {
                let _ = cgroup.kill_all();
            }
            let code = exit.and_then(|s| s.code()).unwrap_or(-1);
            *proc.exit_code.lock() = Some(code);
            let requested = proc.stopping.load(Ordering::Relaxed);
            *proc.state.lock() = if requested {
                "offline".into()
            } else {
                "crashed".into()
            };
            let auto = proc.spec.lock().auto_restart;
            if !requested && auto {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if proc.stopping.load(Ordering::Relaxed) || proc.pid.lock().is_some() {
                    return;
                }
                proc.restart_count.fetch_add(1, Ordering::Relaxed);
                let uuid = proc.spec.lock().uuid.clone();
                let _ = rt.start(&uuid);
            }
        });
    }

    pub fn stop(&self, uuid: &str, force: bool) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&proc.operation)?;
        proc.stopping.store(true, Ordering::Relaxed);
        let pid = *proc.pid.lock();
        if force {
            if let Some(cgroup) = proc.cgroup.lock().as_ref() {
                let _ = cgroup.kill_all();
            }
        }
        let Some(pid) = pid else {
            *proc.state.lock() = "offline".into();
            return self.stats(uuid);
        };
        *proc.state.lock() = "stopping".into();
        unsafe {
            libc::kill(
                -(pid as i32),
                if force { libc::SIGKILL } else { libc::SIGTERM },
            );
            libc::kill(
                pid as i32,
                if force { libc::SIGKILL } else { libc::SIGTERM },
            );
        }
        if !force {
            let proc2 = proc.clone();
            let captured = pid;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if *proc2.pid.lock() == Some(captured) {
                    if let Some(cgroup) = proc2.cgroup.lock().as_ref() {
                        let _ = cgroup.kill_all();
                    }
                    unsafe {
                        libc::kill(-(captured as i32), libc::SIGKILL);
                        libc::kill(captured as i32, libc::SIGKILL);
                    }
                }
            });
        }
        self.stats(uuid)
    }

    pub async fn restart(&self, uuid: &str) -> Result<RemoteServerStats> {
        let _ = self.stop(uuid, false)?;
        for _ in 0..100 {
            if self.process(uuid)?.pid.lock().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.start(uuid)
    }

    pub fn command(&self, uuid: &str, command: &str) -> Result<bool> {
        let proc = self.process(uuid)?;
        let mut stdin = proc.stdin.lock();
        let input = stdin
            .as_mut()
            .context("server not running or stdin unavailable")?;
        input.write_all(command.as_bytes())?;
        if !command.ends_with('\n') {
            input.write_all(b"\n")?;
        }
        input.flush()?;
        proc.tx_bytes
            .fetch_add(command.len() as u64, Ordering::Relaxed);
        Ok(true)
    }

    pub fn stats(&self, uuid: &str) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        let pid = *proc.pid.lock();
        let (cpu, memory) = pid
            .map(|p| process_tree_usage(p, &proc))
            .unwrap_or((0.0, 0));
        let state = proc.state.lock().clone();
        let uptime_secs = proc
            .started
            .lock()
            .map(|i| i.elapsed().as_secs())
            .unwrap_or(0);
        let exit_code = *proc.exit_code.lock();
        Ok(RemoteServerStats {
            uuid: uuid.into(),
            state,
            pid,
            cpu_percent: cpu,
            memory_bytes: memory,
            disk_bytes: dir_size(&self.server_root(uuid)?),
            network_rx_bytes: proc.rx_bytes.load(Ordering::Relaxed),
            network_tx_bytes: proc.tx_bytes.load(Ordering::Relaxed),
            uptime_secs,
            restart_count: proc.restart_count.load(Ordering::Relaxed),
            exit_code,
        })
    }

    pub fn console(&self, uuid: &str, after: u64) -> Result<(Vec<String>, u64)> {
        let proc = self.process(uuid)?;
        let buf = proc.console.lock();
        let lines = buf
            .iter()
            .filter(|(cursor, _)| *cursor > after)
            .map(|(_, line)| line.clone())
            .collect();
        Ok((lines, proc.console_cursor.load(Ordering::Relaxed)))
    }
    pub fn clear_console(&self, uuid: &str) -> Result<bool> {
        let proc = self.process(uuid)?;
        proc.console.lock().clear();
        proc.console_cursor.store(0, Ordering::Relaxed);
        Ok(true)
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
                path: format!(
                    "/{}",
                    entry
                        .path()
                        .strip_prefix(&root)
                        .unwrap_or(&entry.path())
                        .to_string_lossy()
                ),
                is_dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                modified: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
                mode: meta.permissions().mode(),
            });
        }
        values.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(values)
    }

    pub fn read_file(&self, uuid: &str, path: &str, max_bytes: u64) -> Result<Vec<u8>> {
        secure_read(&self.server_root(uuid)?, path, max_bytes)
    }

    pub fn write_file(&self, uuid: &str, path: &str, data: &[u8], append: bool) -> Result<bool> {
        if data.len() as u64 > self.config.max_upload_mb * 1_048_576 {
            bail!("upload too large")
        }
        secure_write(&self.server_root(uuid)?, path, data, append)?;
        Ok(true)
    }

    pub fn file_operation(
        &self,
        uuid: &str,
        operation: crate::node_protocol::FileOperation,
    ) -> Result<bool> {
        use crate::node_protocol::FileOperation;
        let root = self.server_root(uuid)?;
        match operation {
            FileOperation::Mkdir { path } => secure_mkdir(&root, &path)?,
            FileOperation::Touch { path } => secure_write(&root, &path, &[], true)?,
            FileOperation::Rename { from, to } => secure_rename(&root, &from, &to)?,
            FileOperation::Copy { from, to } => {
                let max = self.config.max_upload_mb * 1_048_576;
                let data = secure_read(&root, &from, max)
                    .context("directory copy is not supported by secure copy")?;
                secure_write(&root, &to, &data, false)?;
            }
            FileOperation::Move { from, destination } => {
                let name = Path::new(&from)
                    .file_name()
                    .context("invalid filename")?
                    .to_string_lossy();
                let to = format!("{}/{}", destination.trim_end_matches('/'), name);
                secure_rename(&root, &from, &to)?;
            }
            FileOperation::Delete { path } => secure_delete(&root, &path)?,
            FileOperation::Chmod { path, mode } => secure_chmod(&root, &path, mode)?,
        }
        Ok(true)
    }

    pub fn capacity(&self) -> NodeCapacity {
        use sysinfo::{Disks, System};
        let mut sys = System::new_all();
        sys.refresh_all();
        let disks = Disks::new_with_refreshed_list();
        let disk_total = disks.iter().map(|d| d.total_space()).sum::<u64>();
        let disk_used = disks
            .iter()
            .map(|d| d.total_space().saturating_sub(d.available_space()))
            .sum::<u64>();
        let load = System::load_average();
        NodeCapacity {
            memory_total: sys.total_memory(),
            memory_used: sys.used_memory(),
            disk_total,
            disk_used,
            cpu_percent: sys.global_cpu_info().cpu_usage() as f64,
            cpu_threads: sys.cpus().len(),
            load_1: load.one,
            load_5: load.five,
            load_15: load.fifteen,
            servers_running: self
                .processes
                .iter()
                .filter(|p| p.pid.lock().is_some())
                .count(),
            servers_total: self.processes.len(),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }
    pub fn snapshot(&self, uuid: &str) -> Result<crate::node_protocol::SnapshotResponse> {
        use sha2::{Digest, Sha256};
        if self.process(uuid)?.pid.lock().is_some() {
            bail!("server must be stopped before snapshot")
        }
        let root = self.server_root(uuid)?;
        let temp = tempfile::NamedTempFile::new_in(&self.config.data_dir)?;
        let encoder = flate2::write::GzEncoder::new(temp.reopen()?, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.file_type().is_symlink() {
                continue;
            }
            let rel = entry.path().strip_prefix(&root)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            if meta.is_dir() {
                tar.append_dir(rel, entry.path())?
            } else if meta.is_file() {
                tar.append_path_with_name(entry.path(), rel)?
            }
        }
        let encoder = tar.into_inner()?;
        encoder.finish()?;
        let size = fs::metadata(temp.path())?.len();
        let max = self.config.max_upload_mb * 1_048_576;
        if size > max {
            bail!("snapshot exceeds {} MiB limit", self.config.max_upload_mb)
        }
        let bytes = fs::read(temp.path())?;
        let checksum = hex::encode(Sha256::digest(&bytes));
        Ok(crate::node_protocol::SnapshotResponse {
            archive_b64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
            size_bytes: bytes.len() as u64,
            checksum,
        })
    }

    pub fn restore_snapshot(
        &self,
        uuid: &str,
        req: crate::node_protocol::RestoreSnapshotRequest,
    ) -> Result<bool> {
        use sha2::{Digest, Sha256};
        if self.process(uuid)?.pid.lock().is_some() {
            bail!("server must be stopped before restore");
        }
        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, req.archive_b64)?;
        if hex::encode(Sha256::digest(&bytes)) != req.checksum {
            bail!("snapshot checksum mismatch");
        }
        let root = self.server_root(uuid)?;
        let validate = flate2::read::GzDecoder::new(bytes.as_slice());
        for entry in tar::Archive::new(validate).entries()? {
            let entry = entry?;
            let kind = entry.header().entry_type();
            if kind.is_symlink() || kind.is_hard_link() {
                bail!("snapshot contains forbidden link entry")
            };
            let path = entry.path()?;
            if path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
            {
                bail!("snapshot path traversal")
            };
        }
        let parent = root.parent().context("server root has no parent")?;
        let staging = parent.join(format!(".restore-{uuid}-{}", uuid::Uuid::new_v4().simple()));
        let previous = parent.join(format!(
            ".previous-{uuid}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&staging)?;
        let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        if let Err(error) = tar::Archive::new(decoder).unpack(&staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error.into());
        }
        crate::isolation::prepare_root(&staging, uuid)?;
        crate::isolation::own_tree(&staging, uuid)?;
        if root.exists() {
            fs::rename(&root, &previous)?;
        }
        if let Err(error) = fs::rename(&staging, &root) {
            if previous.exists() {
                let _ = fs::rename(&previous, &root);
            }
            return Err(error.into());
        }
        if previous.exists() {
            let _ = fs::remove_dir_all(previous);
        }
        Ok(true)
    }

    pub fn shutdown_notify(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    fn process(&self, uuid: &str) -> Result<Arc<ManagedProcess>> {
        self.processes
            .get(uuid)
            .map(|v| v.clone())
            .context("server not provisioned")
    }

    fn server_root(&self, uuid: &str) -> Result<PathBuf> {
        validate_uuid(uuid)?;
        Ok(self.config.servers_dir().join(uuid))
    }
}

fn validate_uuid(uuid: &str) -> Result<()> {
    if uuid.is_empty()
        || uuid.len() > 64
        || !uuid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid server uuid");
    }
    Ok(())
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;

fn openat2_beneath(root: &Path, relative: &str, flags: i32, mode: u32) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let root_file = fs::File::open(root)?;
    let rel = relative.trim_start_matches('/');
    let rel = if rel.is_empty() { "." } else { rel };
    let c = std::ffi::CString::new(rel)?;
    let how = OpenHow {
        flags: flags as u64,
        mode: mode as u64,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root_file.as_raw_fd(),
            c.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        ) as i32
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("secure open {relative}"));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

fn secure_mkdir_parents(root: &Path, relative: &str, uid: u32, gid: u32) -> Result<()> {
    let path = Path::new(relative.trim_start_matches('/'));
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut built = PathBuf::new();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            bail!("invalid path component")
        };
        built.push(name);
        let rel = built.to_string_lossy();
        match openat2_beneath(
            root,
            &rel,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        ) {
            Ok(_) => {}
            Err(_) => {
                let full = root.join(&built);
                fs::create_dir(&full)?;
                let c = std::ffi::CString::new(full.as_os_str().as_encoded_bytes())?;
                if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
                    return Err(std::io::Error::last_os_error()).context("chown directory");
                }
            }
        }
    }
    Ok(())
}

fn secure_read(root: &Path, relative: &str, max: u64) -> Result<Vec<u8>> {
    let mut file = openat2_beneath(root, relative, libc::O_RDONLY | libc::O_CLOEXEC, 0)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("not a regular file")
    }
    if meta.len() > max {
        bail!("file exceeds read limit")
    }
    let mut out = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut out)?;
    Ok(out)
}

fn secure_write(root: &Path, relative: &str, data: &[u8], append: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    let meta = fs::metadata(root)?;
    secure_mkdir_parents(root, relative, meta.uid(), meta.gid())?;
    let flags = libc::O_WRONLY
        | libc::O_CREAT
        | libc::O_CLOEXEC
        | if append {
            libc::O_APPEND
        } else {
            libc::O_TRUNC
        };
    let mut file = openat2_beneath(root, relative, flags, 0o640)?;
    if unsafe { libc::fchown(file.as_raw_fd(), meta.uid(), meta.gid()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("chown file");
    };
    file.write_all(data)?;
    Ok(())
}
fn secure_parent(
    root: &Path,
    relative: &str,
    create: bool,
) -> Result<(fs::File, std::ffi::CString)> {
    let path = Path::new(relative.trim_start_matches('/'));
    let name = path.file_name().context("path has no filename")?;
    if create {
        let meta = fs::metadata(root)?;
        secure_mkdir_parents(root, relative, meta.uid(), meta.gid())?;
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let dir = openat2_beneath(
        root,
        &parent.to_string_lossy(),
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    )?;
    Ok((dir, std::ffi::CString::new(name.as_encoded_bytes())?))
}

fn secure_delete(root: &Path, relative: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let (dir, name) = secure_parent(root, relative, false)?;
    let first = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if first == 0 {
        return Ok(());
    }
    let second = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if second != 0 {
        return Err(std::io::Error::last_os_error()).context("secure delete");
    }
    Ok(())
}

fn secure_rename(root: &Path, from: &str, to: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let (from_dir, from_name) = secure_parent(root, from, false)?;
    let (to_dir, to_name) = secure_parent(root, to, true)?;
    if unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from_name.as_ptr(),
            to_dir.as_raw_fd(),
            to_name.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("secure rename");
    }
    Ok(())
}

fn secure_mkdir(root: &Path, relative: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let meta = fs::metadata(root)?;
    let (dir, name) = secure_parent(root, relative, true)?;
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o750) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let file = openat2_beneath(
        root,
        relative,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        0,
    )?;
    if unsafe { libc::fchown(file.as_raw_fd(), meta.uid(), meta.gid()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("chown directory");
    }
    Ok(())
}

fn secure_chmod(root: &Path, relative: &str, mode: u32) -> Result<()> {
    use std::os::fd::AsRawFd;
    let file = openat2_beneath(
        root,
        relative,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK,
        0,
    )
    .or_else(|_| {
        openat2_beneath(
            root,
            relative,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )
    })?;
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(std::io::Error::last_os_error()).context("secure chmod");
    }
    Ok(())
}

pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("server root {} missing", root.display()))?;
    let mut out = canonical_root.clone();
    for part in Path::new(relative.trim_start_matches('/')).components() {
        match part {
            Component::Normal(v) => {
                out.push(v);
                if let Ok(meta) = fs::symlink_metadata(&out) {
                    if meta.file_type().is_symlink() {
                        bail!("symlink traversal rejected at {}", out.display());
                    }
                }
            }
            Component::CurDir => {}
            _ => bail!("path traversal rejected"),
        }
    }
    if !out.starts_with(&canonical_root) {
        bail!("path escapes server root");
    }
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
                    if stderr {
                        text = format!("[stderr] {text}");
                    }
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
        if let Some((rss, cpu)) = read_proc(current) {
            memory = memory.saturating_add(rss);
            ticks = ticks.saturating_add(cpu);
        }
        if let Ok(children) = fs::read_to_string(format!("/proc/{current}/task/{current}/children"))
        {
            stack.extend(
                children
                    .split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
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
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    #[test]
    fn secure_io_rejects_symlink_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        assert!(secure_read(&root, "escape", 1024).is_err());
        assert!(secure_write(&root, "escape", b"pwned", false).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
    }
}
