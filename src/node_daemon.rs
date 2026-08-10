//! Runtime used by the standalone `voltd` execution agent.
use crate::node_protocol::{
    NodeCapacity, ProvisionRequest, RemoteFileEntry, RemoteServerStats, ServerSpec,
};
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot, Notify};

/// Upper bound for a single stdin write (console command or stop_command).
const MAX_STDIN_WRITE_BYTES: usize = 64 * 1024;
/// Deadline for delivering a stdin write via a server's writer thread; a
/// full pipe or dead child cannot stall the caller.
const STOP_CMD_DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on queued-but-unwritten stdin commands per server. When the child
/// stops draining, the writer thread wedges on its current write and the
/// queue fills; later commands fail fast instead of backing up forever.
const STDIN_QUEUE_CAP: usize = 16;
/// Liveness grace after stop_command delivery before escalating to SIGTERM.
const STOP_CMD_GRACE: Duration = Duration::from_secs(5);
/// SIGTERM -> SIGKILL escalation window in stop().
const STOP_SIGKILL_GRACE: Duration = Duration::from_secs(10);
/// Crash auto-restart backoff: base, cap, and the consecutive-failure limit
/// after which the agent gives up until an operator acts.
const RESTART_BACKOFF_BASE_SECS: u64 = 5;
const RESTART_BACKOFF_MAX_SECS: u64 = 120;
const MAX_AUTO_RESTART_ATTEMPTS: u64 = 10;
/// Minimum interval between full dir_size walks per server.
const DIR_SIZE_CACHE_MS: u64 = 10_000;
/// Minimum interval between sysinfo refreshes in capacity().
const CAPACITY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Bounds for snapshot-restore extraction, mirroring the panel's archive
/// bounds (`services::backups`): entry count, per-file and total bytes.
const MAX_SNAPSHOT_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub listen: String,
    pub data_dir: PathBuf,
    pub panel_url: String,
    /// URL the panel dials this node on. Its host becomes a certificate SAN so
    /// the pinned certificate also validates by name, not just by fingerprint.
    #[serde(default)]
    pub public_url: String,
    pub node_id: String,
    pub secret: String,
    pub heartbeat_interval_secs: u64,
    pub max_upload_mb: u64,
    /// Serve the agent API in plaintext. Only sane behind a reverse proxy that
    /// terminates TLS itself; the panel then has no fingerprint to pin.
    #[serde(default)]
    pub plaintext: bool,
    /// SHA-256 fingerprint of the panel's self-signed certificate, captured at
    /// `voltd join`. Empty when the panel uses a publicly trusted certificate.
    #[serde(default)]
    pub panel_fingerprint: String,
    /// Permit serving the plaintext agent API on a non-loopback address.
    /// `voltd join --allow-http` sets it; persisted configs written before
    /// this field existed default to `false`.
    #[serde(default)]
    pub allow_http_bind: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8081".into(),
            data_dir: PathBuf::from("./voltd-data"),
            panel_url: String::new(),
            public_url: String::new(),
            node_id: String::new(),
            allow_http_bind: false,
            secret: String::new(),
            heartbeat_interval_secs: 15,
            max_upload_mb: 256,
            plaintext: false,
            panel_fingerprint: String::new(),
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
    pub fn tls_dir(&self) -> PathBuf {
        self.data_dir.join("tls")
    }
}

/// True when `listen` ("host:port") resolves to the loopback interface.
/// Accepts `localhost` as a hostname and any loopback IP (IPv4/IPv6).
pub fn listen_is_loopback(listen: &str) -> bool {
    if let Ok(addr) = listen.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    let host = listen
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(listen)
        .trim_start_matches('[')
        .trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[derive(Debug)]
pub struct ManagedProcess {
    pub spec: Mutex<ServerSpec>,
    pub child: Mutex<Option<Child>>,
    stdin_tx: Mutex<Option<mpsc::SyncSender<StdinCmd>>>,
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
    /// Cached dir_size (bytes) with a min-interval refresh, so stats() does
    /// not walk the whole server tree on every call.
    pub dir_cache_ms: AtomicU64,
    pub dir_cache_bytes: AtomicU64,
    /// Serializes snapshot/restore per server: both walk the whole tree and
    /// must never run concurrently against the same node.
    pub snapshot_lock: Mutex<()>,
}

impl ManagedProcess {
    fn new(spec: ServerSpec) -> Self {
        let (console_tx, _) = broadcast::channel(1024);
        Self {
            spec: Mutex::new(spec),
            child: Mutex::new(None),
            stdin_tx: Mutex::new(None),
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
            dir_cache_ms: AtomicU64::new(0),
            dir_cache_bytes: AtomicU64::new(0),
            snapshot_lock: Mutex::new(()),
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

/// One queued stdin command for a server's dedicated writer thread.
#[derive(Debug)]
struct StdinCmd {
    line: String,
    /// Delivers the write result back to the waiting caller; when the sender
    /// drops, the caller times out (the write was abandoned).
    ack: oneshot::Sender<Result<(), String>>,
}

/// Writer loop: owns the child's stdin for the whole life of the child and
/// serializes writes through a bounded queue, so no command can ever close
/// stdin (EOF) and a wedged child can only stall this one thread — never a
/// blocking-pool thread or the stdin mutex. Exits when the sender is dropped
/// (the waiter already reaped the child), releasing the handle post-exit.
fn stdin_writer_loop(mut stdin: ChildStdin, rx: mpsc::Receiver<StdinCmd>) {
    while let Ok(cmd) = rx.recv() {
        let res = (|| -> std::io::Result<()> {
            stdin.write_all(cmd.line.as_bytes())?;
            stdin.flush()?;
            Ok(())
        })()
        .map_err(|e| e.to_string());
        let _ = cmd.ack.send(res);
    }
}
#[derive(Clone)]
pub struct DaemonRuntime {
    pub config: Arc<DaemonConfig>,
    pub processes: Arc<DashMap<String, Arc<ManagedProcess>>>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    shutdown: Arc<Notify>,
    /// Cached sysinfo::System for capacity(): refreshed at most every
    /// CAPACITY_REFRESH_INTERVAL instead of rebuilt on every heartbeat.
    capacity_cache: Arc<Mutex<Option<(Instant, sysinfo::System)>>>,
}

impl DaemonRuntime {
    pub fn new(config: DaemonConfig) -> Result<Self> {
        config.ensure_dirs()?;
        // Startup recovery for `.restore-*` / `.previous-*` dirs a crash
        // mid-restore can leave behind (mirrors the panel's backups boot
        // sweep): pass 1 restores `.previous-<name>` asides whose live dir is
        // missing (the aside holds the only copy), pass 2 reclaims superseded
        // asides, stale staging dirs, and stray probe files.
        match recover_stale_dirs(&config.servers_dir()) {
            Ok(0) => {}
            Ok(actions) => tracing::info!("voltd: stale-restore recovery: {actions} action(s)"),
            Err(error) => tracing::warn!("voltd: stale-restore recovery failed: {error}"),
        }
        let rt = Self {
            config: Arc::new(config),
            processes: Arc::new(DashMap::new()),
            started_at: chrono::Utc::now(),
            shutdown: Arc::new(Notify::new()),
            capacity_cache: Arc::new(Mutex::new(None)),
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
            let bytes = match fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(
                        "voltd: cannot read spec {}: {error}; quarantining",
                        entry.path().display()
                    );
                    quarantine_spec(&entry.path());
                    continue;
                }
            };
            match serde_json::from_slice::<ServerSpec>(&bytes) {
                Ok(spec) => {
                    crate::isolation::cleanup_orphans(&spec.uuid);
                    let root = self.config.servers_dir().join(&spec.uuid);
                    crate::isolation::prepare_root(&root, &spec.uuid)?;
                    crate::isolation::own_tree(&root, &spec.uuid)?;
                    self.processes
                        .insert(spec.uuid.clone(), Arc::new(ManagedProcess::new(spec)));
                }
                Err(error) => {
                    tracing::warn!(
                        "voltd: unparseable spec {}: {error}; quarantining",
                        entry.path().display()
                    );
                    quarantine_spec(&entry.path());
                }
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
            let max = self.config.max_upload_mb.saturating_mul(1_048_576);
            let mut total = 0u64;
            // Decode everything up front and enforce the upload cap on both
            // the aggregate payload and each individual file before anything
            // reaches disk.
            let mut files = Vec::with_capacity(req.files.len());
            for file in req.files {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    &file.content_b64,
                )?;
                let len = bytes.len() as u64;
                if len > max {
                    bail!(
                        "file {} exceeds the {} MiB per-file upload limit",
                        file.path,
                        self.config.max_upload_mb
                    );
                }
                total = total.saturating_add(len);
                if total > max {
                    bail!(
                        "provision payload exceeds the {} MiB aggregate upload limit",
                        self.config.max_upload_mb
                    );
                }
                files.push((file.path, file.mode, bytes));
            }
            for (path, mode, bytes) in files {
                let target = safe_join(&root, &path)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, &bytes)?;
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt;
                    // Mask to 0o777: the panel's mode is advisory, and
                    // setuid/setgid/sticky bits from a provision payload are
                    // a privilege-escalation surface (defense in depth —
                    // extraction already masks, provision must too).
                    fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))?;
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
        // Sticky stop latch (mirrors services::proc::stop_issued): once a
        // stop() has run, only an explicit restart may bring the server back.
        // Auto-restart funnels through start(), so this single check makes it
        // impossible for a crash-restart to resurrect a server the operator
        // stopped — even one already sleeping through its backoff.
        if proc.stopping.load(Ordering::Relaxed) {
            bail!("server stop has been requested; use the restart action to start it again");
        }
        self.start_inner(&proc, uuid)
    }

    fn start_inner(&self, proc: &Arc<ManagedProcess>, uuid: &str) -> Result<RemoteServerStats> {
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
        // Die with the agent: without PDEATHSIG a daemon SIGKILL strands the
        // bwrap -> setpriv -> sh tree, its cgroup dir, and the veth/nft
        // lease — cleanup only runs on the graceful path otherwise (mirrors
        // services::proc::start).
        // The child writes its own pid into cgroup.procs before exec'ing
        // bwrap, so the wrapper and every descendant are born inside the
        // server cgroup: a fast tenant fork-bomb has no window outside the
        // limits (a post-spawn attach races a 1500ms deadline against a
        // 4096-descendant cap). The systemd-run path owns its scope and has
        // an empty cgroup path here; isolation-disabled launches too — both
        // skip the write.
        let attach_procs = (!cgroup.path().as_os_str().is_empty()).then(|| {
            std::ffi::CString::new(
                cgroup.path().join("cgroup.procs").to_string_lossy().as_bytes(),
            )
            .expect("cgroup path cannot contain a NUL")
        });
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() == 1 {
                    // Parent died between fork and prctl: never outlive it.
                    libc::_exit(1);
                }
                if let Some(procs) = &attach_procs {
                    // Post-fork in a multithreaded parent: only
                    // async-signal-safe calls here (heap allocation could
                    // deadlock on another thread's malloc lock).
                    let pid = libc::getpid();
                    let mut pidbuf = [0u8; 12];
                    let mut i = pidbuf.len() - 1;
                    pidbuf[i] = b'\n';
                    let mut n = pid as usize;
                    while n > 0 {
                        i -= 1;
                        pidbuf[i] = b'0' + (n % 10) as u8;
                        n /= 10;
                    }
                    let fd = libc::open(procs.as_ptr(), libc::O_WRONLY);
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let wrote = libc::write(
                        fd,
                        pidbuf[i..].as_ptr() as *const libc::c_void,
                        pidbuf.len() - i,
                    );
                    let write_error = std::io::Error::last_os_error();
                    libc::close(fd);
                    if wrote < 0 {
                        return Err(write_error);
                    }
                }
                Ok(())
            });
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.env("SERVER_UUID", &spec.uuid)
            .env("SERVER_NAME", &spec.name)
            .env("SERVER_MEMORY", spec.memory_mb.to_string());
        let mut child = cmd.spawn().context("sandbox spawn failed")?;
        let pid = child.id();
        // The wrapper already attached itself in pre_exec; this pass walks
        // the live subtree so the already-forked payload is verifiably in
        // the server cgroup too. Every failure after spawn tears down what
        // the child may already have built (mirrors
        // services::proc::reap_failed_spawn): kill the whole process group,
        // drain + remove the cgroup, reap the leader.
        if let Err(error) = cgroup.attach(pid) {
            reap_failed_spawn(cgroup, child);
            return Err(error);
        }
        let Some(stdout) = child.stdout.take() else {
            reap_failed_spawn(cgroup, child);
            bail!("missing stdout");
        };
        let Some(stderr) = child.stderr.take() else {
            reap_failed_spawn(cgroup, child);
            bail!("missing stderr");
        };
        let ports = if spec.ports.is_empty() {
            spec.port.into_iter().collect::<Vec<_>>()
        } else {
            spec.ports.clone()
        };
        let network = match crate::isolation::NetworkLease::configure(pid, uuid, &ports) {
            Ok(value) => value,
            Err(error) => {
                reap_failed_spawn(cgroup, child);
                return Err(error);
            }
        };
        *proc.network.lock() = Some(network);
        *proc.cgroup.lock() = Some(cgroup);
        // Hand the child's stdin to a dedicated writer thread; the handle
        // lives there until the child is reaped (see spawn_waiter), so a
        // console or stop command can never drop it and close stdin.
        if let Some(stdin) = child.stdin.take() {
            let (tx, rx) = mpsc::sync_channel(STDIN_QUEUE_CAP);
            std::thread::spawn(move || stdin_writer_loop(stdin, rx));
            *proc.stdin_tx.lock() = Some(tx);
        }
        *proc.pid.lock() = Some(pid);
        *proc.state.lock() = "running".into();
        *proc.started.lock() = Some(Instant::now());
        *proc.exit_code.lock() = None;
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
            // The child is reaped: close the writer queue so the writer
            // thread exits and releases the stdin handle. The handle is only
            // ever released here — never by a command write — so no stdin
            // EOF can kill a live child. Cleared before pid so a racing
            // start() can never see a live pid with a dead writer.
            proc.stdin_tx.lock().take();
            *proc.pid.lock() = None;
            // The child is reaped: kill any stragglers and release the cgroup
            // (kill_all + rmdir, mirroring reap_failed_spawn). A later start()
            // creates a fresh cgroup.
            if let Some(cgroup) = proc.cgroup.lock().take() {
                let _ = cgroup.kill_all();
                cgroup.remove();
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
                let uuid = proc.spec.lock().uuid.clone();
                let mut attempts = 0u64;
                loop {
                    attempts += 1;
                    tokio::time::sleep(restart_backoff(attempts)).await;
                    // A stop landed during the backoff (sticky latch) or
                    // someone else started the server: give up.
                    if proc.stopping.load(Ordering::Relaxed) || proc.pid.lock().is_some() {
                        return;
                    }
                    proc.restart_count.fetch_add(1, Ordering::Relaxed);
                    match rt.start(&uuid) {
                        Ok(_) => {
                            tracing::info!("voltd: auto-restarted server {uuid}");
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(
                                "voltd: auto-restart attempt {attempts} for {uuid} failed: {error:#}"
                            );
                            if attempts >= MAX_AUTO_RESTART_ATTEMPTS {
                                tracing::error!(
                                    "voltd: giving up auto-restart for {uuid} after {attempts} failed attempts; operator action required"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn stop(&self, uuid: &str, force: bool) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&proc.operation)?;
        self.stop_inner(&proc, uuid, force)
    }

    /// stop() under an already-held operation guard. restart() uses this so
    /// the whole stop -> drain -> start sequence stays serialized.
    fn stop_inner(
        &self,
        proc: &Arc<ManagedProcess>,
        uuid: &str,
        force: bool,
    ) -> Result<RemoteServerStats> {
        // Sticky stop latch: set here, cleared only by an explicit restart().
        // While set, auto-restart (which funnels through start()) is refused.
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
        if !force {
            let stop_cmd = proc.spec.lock().stop_command.clone();
            let proc2 = proc.clone();
            let uuid_owned = uuid.to_string();
            tokio::spawn(async move {
                // 1. stop_command first (when configured): queued on the
                //    server's stdin writer thread. A full pipe or dead child
                //    surfaces as Busy/Disconnected and escalates to SIGTERM
                //    without stalling the caller or closing stdin.
                if !stop_cmd.trim().is_empty() {
                    let delivered = tokio::time::timeout(
                        STOP_CMD_DELIVERY_TIMEOUT,
                        async {
                            let mut line = stop_cmd;
                            if !line.ends_with('\n') {
                                line.push('\n');
                            }
                            let Some(tx) = proc2.stdin_tx.lock().clone() else {
                                return;
                            };
                            let (ack_tx, ack_rx) = oneshot::channel();
                            let _ = tx.try_send(StdinCmd {
                                line,
                                ack: ack_tx,
                            });
                            let _ = ack_rx.await;
                        },
                    )
                    .await
                    .is_ok();
                    if !delivered {
                        tracing::warn!(
                            "voltd: stop_command delivery for {uuid_owned} timed out; escalating to SIGTERM"
                        );
                    }
                    // 2. Poll liveness so the stop_command gets a window to
                    //    shut the server down before SIGTERM lands.
                    let grace = Instant::now() + STOP_CMD_GRACE;
                    while Instant::now() < grace {
                        if *proc2.pid.lock() != Some(pid) {
                            return; // exited on its own
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
                // 3. SIGTERM the whole process group.
                if *proc2.pid.lock() == Some(pid) {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGTERM);
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
                // 4. SIGKILL last resort.
                tokio::time::sleep(STOP_SIGKILL_GRACE).await;
                if *proc2.pid.lock() == Some(pid) {
                    if let Some(cgroup) = proc2.cgroup.lock().as_ref() {
                        let _ = cgroup.kill_all();
                    }
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            });
        } else {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        self.stats(uuid)
    }

    pub async fn restart(&self, uuid: &str) -> Result<RemoteServerStats> {
        let proc = self.process(uuid)?;
        // Hold the operation guard across stop -> drain -> start so no
        // concurrent operation can interleave with the restart.
        let _operation = crate::isolation::AtomicFlagGuard::acquire(&proc.operation)?;
        self.stop_inner(&proc, uuid, false)?;
        let deadline =
            Instant::now() + STOP_CMD_GRACE + STOP_SIGKILL_GRACE + Duration::from_secs(1);
        while Instant::now() < deadline {
            if proc.pid.lock().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if proc.pid.lock().is_some() {
            bail!("server did not stop within the restart window");
        }
        // Explicit operator restart is newer intent than the stop: clear the
        // sticky latch so the respawn is allowed.
        proc.stopping.store(false, Ordering::Relaxed);
        self.start_inner(&proc, uuid)
    }

    pub async fn command(&self, uuid: &str, command: &str) -> Result<bool> {
        if command.len() > MAX_STDIN_WRITE_BYTES {
            bail!("command exceeds the {} byte limit", MAX_STDIN_WRITE_BYTES);
        }
        let proc = self.process(uuid)?;
        let mut line = command.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let bytes_len = line.len() as u64;
        let tx = proc.stdin_tx.lock().clone();
        let Some(tx) = tx else {
            bail!("server not running or stdin unavailable");
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        match tx.try_send(StdinCmd { line, ack: ack_tx }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                bail!("stdin backlog full; the server process may not be draining input")
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                bail!("server not running or stdin unavailable")
            }
        }
        match tokio::time::timeout(STOP_CMD_DELIVERY_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(()))) => {
                proc.tx_bytes.fetch_add(bytes_len, Ordering::Relaxed);
                Ok(true)
            }
            Ok(Ok(Err(error))) => bail!("stdin write failed: {error}"),
            Ok(Err(_)) => bail!("stdin writer went away"),
            Err(_) => bail!("stdin write timed out"),
        }
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
            disk_bytes: cached_dir_size(&proc, &self.server_root(uuid)?),
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
        use sysinfo::System;
        // Reuse one System across calls, refreshing at most every
        // CAPACITY_REFRESH_INTERVAL. The first call samples twice: the first
        // refresh after new_all() often reports zero CPU.
        let mut cache = self.capacity_cache.lock();
        match cache.as_mut() {
            Some((last, _)) if last.elapsed() < CAPACITY_REFRESH_INTERVAL => {}
            Some((last, sys)) => {
                sys.refresh_all();
                *last = Instant::now();
            }
            None => {
                let mut sys = System::new_all();
                sys.refresh_all();
                sys.refresh_all();
                *cache = Some((Instant::now(), sys));
            }
        }
        let sys = &cache.as_ref().expect("cache initialized above").1;
        // Disk totals are restricted to the filesystem that actually holds
        // the agent data (statvfs on data_dir), not summed across every
        // mounted filesystem.
        let (disk_total, disk_used) = fs_usage(&self.config.data_dir).unwrap_or((0, 0));
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
    /// Walk the server tree and write a tar.gz archive of it into `sink`,
    /// returning (compressed bytes written, SHA-256 hex of those bytes).
    /// Streaming: nothing but the current file is ever held in memory, and
    /// `sink` receives the gzip output as it is produced. Callers own the
    /// per-server snapshot lock and the stopped-server check (see `snapshot`
    /// and the streaming producer in `voltd`); this walks the tree only. The
    /// archive is hard-capped at [`MAX_SNAPSHOT_TOTAL_BYTES`] while producing
    /// (see [`HashCountWriter`]), symmetric with the panel's extraction bound,
    /// so production errors as soon as the bound is crossed instead of
    /// shipping an archive the panel would refuse.
    pub fn write_archive_to<W: Write>(&self, uuid: &str, sink: W) -> Result<(u64, String)> {
        let root = self.server_root(uuid)?;
        let mut counted = HashCountWriter {
            inner: sink,
            hasher: sha2::Sha256::new(),
            count: 0,
            max: MAX_SNAPSHOT_TOTAL_BYTES,
        };
        {
            let encoder = flate2::write::GzEncoder::new(&mut counted, flate2::Compression::fast());
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
        }
        // `finish` flushes the gzip trailer, but a caller-owned sink (e.g. the
        // response-body channel) may still hold buffered bytes; push them out.
        counted.inner.flush()?;
        Ok((counted.count, hex::encode(counted.hasher.finalize())))
    }

    /// Legacy buffered snapshot: build the archive to a temp file, then
    /// base64 it into the envelope. Bounded in RAM (the archive never loads
    /// into memory), and kept for pre-streaming panels; the streaming path
    /// writes the archive straight into the response body.
    pub fn snapshot(&self, uuid: &str) -> Result<crate::node_protocol::SnapshotResponse> {
        let proc = self.process(uuid)?;
        // Serialize snapshot/restore per server: both walk the whole tree,
        // so concurrent requests against the same node must not interleave.
        let _snapshot = proc.snapshot_lock.lock();
        if proc.pid.lock().is_some() {
            bail!("server must be stopped before snapshot")
        }
        let temp = tempfile::NamedTempFile::new_in(&self.config.data_dir)?;
        let (size, checksum) = self.write_archive_to(uuid, temp.reopen()?)?;
        let max = self.config.max_upload_mb * 1_048_576;
        if size > max {
            bail!("snapshot exceeds {} MiB limit", self.config.max_upload_mb)
        }
        // Stream the temp archive through base64 instead of materializing it
        // in RAM (fs::read + Engine::encode): at the upload cap the raw copy
        // alone is hundreds of MB per request, and a handful of concurrent
        // snapshots would OOM the agent.
        let mut archive_b64 = String::with_capacity(size as usize * 4 / 3 + 8);
        let mut encoder = base64::write::EncoderStringWriter::from_consumer(
            &mut archive_b64,
            &base64::engine::general_purpose::STANDARD,
        );
        let mut file = fs::File::open(temp.path())?;
        std::io::copy(&mut file, &mut encoder)?;
        encoder.into_inner();
        Ok(crate::node_protocol::SnapshotResponse {
            archive_b64,
            size_bytes: size,
            checksum,
        })
    }

    pub fn restore_snapshot(
        &self,
        uuid: &str,
        req: crate::node_protocol::RestoreSnapshotRequest,
    ) -> Result<bool> {
        let proc = self.process(uuid)?;
        // Same per-server serialization as snapshot(): never interleave a
        // restore with a snapshot or another restore on the same node. The
        // per-node lock is what keeps the (unavoidable, protocol-shaped)
        // request-body buffer bounded: one full archive in RAM at a time.
        let _snapshot = proc.snapshot_lock.lock();
        if proc.pid.lock().is_some() {
            bail!("server must be stopped before restore");
        }
        let root = self.server_root(uuid)?;
        // Decode-stream from the base64 request body into the gzip decoder
        // instead of materializing the decoded archive in RAM (Engine::decode
        // over the whole string). Verify the checksum on a first bounded pass
        // before touching the filesystem, then validate + extract.
        let max = self.config.max_upload_mb * 1_048_576;
        {
            let mut decoder = base64::read::DecoderReader::new(
                req.archive_b64.as_bytes(),
                &base64::engine::general_purpose::STANDARD,
            );
            let mut hasher = sha2::Sha256::new();
            let mut decoded: u64 = 0;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = decoder.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                decoded = decoded.saturating_add(n as u64);
                if decoded > max {
                    bail!(
                        "snapshot archive exceeds {} MiB limit",
                        self.config.max_upload_mb
                    );
                }
                hasher.update(&buf[..n]);
            }
            if hex::encode(hasher.finalize()) != req.checksum {
                bail!("snapshot checksum mismatch");
            }
        }
        let decode = || {
            base64::read::DecoderReader::new(
                req.archive_b64.as_bytes(),
                &base64::engine::general_purpose::STANDARD,
            )
        };
        let validate = flate2::read::GzDecoder::new(decode());
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
        fs::create_dir_all(&staging)?;
        let decoder = flate2::read::GzDecoder::new(decode());
        if let Err(error) = extract_snapshot_tar(decoder, &staging) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        crate::isolation::prepare_root(&staging, uuid)?;
        crate::isolation::own_tree(&staging, uuid)?;
        // Atomic swap (same scheme as services::backups::restore): exchange
        // the live dir and staging so the server dir is never absent, then
        // remove the superseded content from the staging name. On filesystems
        // without renameat2(RENAME_EXCHANGE), exchange_dirs falls back to the
        // two-rename dance and logs once.
        if root.exists() {
            exchange_dirs(&root, &staging)?;
            // Best-effort cleanup of the superseded content now living at the
            // staging name (parity with services::backups::restore). Failure
            // must not fail the restore — a leftover `.restore-*` dir is
            // reclaimed at startup by recover_stale_dirs.
            if let Err(error) = fs::remove_dir_all(&staging) {
                tracing::warn!(
                    "voltd: could not remove superseded server dir {}: {error}",
                    staging.display()
                );
            }
        } else {
            fs::rename(&staging, &root)?;
        }
        Ok(true)
    }

    /// Consume a raw tar.gz archive from `archive` (the streaming protocol's
    /// request body, never buffered) and restore it into the server's live
    /// directory. `verify` is called with the SHA-256 hex of the raw archive
    /// bytes once the body has been fully consumed and extracted to staging,
    /// and MUST return `Ok` before the atomic swap into the live dir runs:
    /// the streaming request MAC can only be checked once the whole body has
    /// been hashed, so the swap — the only irreversible step — is gated on it.
    /// A failed verification or a mid-stream error removes the staging dir
    /// and never touches the live dir.
    ///
    /// Caps are enforced while streaming: the raw body is bounded by
    /// [`MAX_SNAPSHOT_TOTAL_BYTES`] (symmetric with the producer's
    /// [`HashCountWriter`] cap and the panel's extraction bound — a remotely
    /// creatable archive must be remotely restorable) and extraction by the
    /// shared snapshot bounds inside [`extract_snapshot_tar`]. The legacy
    /// base64 path keeps its historical `max_upload_mb` bound.
    pub fn restore_snapshot_stream<R: Read>(
        &self,
        uuid: &str,
        archive: R,
        verify: impl FnOnce(String) -> Result<()>,
    ) -> Result<bool> {
        let proc = self.process(uuid)?;
        // Same per-server serialization as snapshot(): never interleave a
        // restore with a snapshot or another restore on the same node.
        let _snapshot = proc.snapshot_lock.lock();
        if proc.pid.lock().is_some() {
            bail!("server must be stopped before restore");
        }
        let root = self.server_root(uuid)?;
        let hashing = HashingReader::new(archive, MAX_SNAPSHOT_TOTAL_BYTES);
        let parent = root.parent().context("server root has no parent")?;
        let staging = parent.join(format!(".restore-{uuid}-{}", uuid::Uuid::new_v4().simple()));
        fs::create_dir_all(&staging)?;
        let decoder = flate2::read::GzDecoder::new(hashing);
        let sha = match extract_snapshot_tar(decoder, &staging) {
            Ok(decoder) => {
                // The whole body was consumed and extracted; recover the
                // hashing reader from the decoder to get the raw-archive hash.
                let (_, sha) = decoder.into_inner().into_parts();
                sha
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if let Err(error) = verify(sha) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = crate::isolation::prepare_root(&staging, uuid) {
            if let Err(cleanup) = fs::remove_dir_all(&staging) {
                tracing::warn!(
                    "voltd: could not remove staging dir {} after failed restore: {cleanup}",
                    staging.display()
                );
            }
            return Err(error);
        }
        if let Err(error) = crate::isolation::own_tree(&staging, uuid) {
            if let Err(cleanup) = fs::remove_dir_all(&staging) {
                tracing::warn!(
                    "voltd: could not remove staging dir {} after failed restore: {cleanup}",
                    staging.display()
                );
            }
            return Err(error);
        }
        // Atomic swap (same scheme as services::backups::restore): exchange
        // the live dir and staging so the server dir is never absent, then
        // remove the superseded content from the staging name. On filesystems
        // without renameat2(RENAME_EXCHANGE), exchange_dirs falls back to the
        // two-rename dance and logs once.
        if root.exists() {
            exchange_dirs(&root, &staging)?;
            // Best-effort cleanup of the superseded content now living at the
            // staging name (parity with services::backups::restore). Failure
            // must not fail the restore — a leftover `.restore-*` dir is
            // reclaimed at startup by recover_stale_dirs.
            if let Err(error) = fs::remove_dir_all(&staging) {
                tracing::warn!(
                    "voltd: could not remove superseded server dir {}: {error}",
                    staging.display()
                );
            }
        } else {
            fs::rename(&staging, &root)?;
        }
        Ok(true)
    }



    pub fn shutdown_notify(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Graceful daemon shutdown: request stop of every running server
    /// (stop_command first when configured, else SIGTERM), wait up to
    /// `grace` for them to exit, then force-kill stragglers. PDEATHSIG stays
    /// the crash backstop only — a graceful daemon stop must not SIGKILL
    /// every tenant.
    pub async fn shutdown_servers(&self, grace: Duration) {
        let uuids: Vec<String> = self
            .processes
            .iter()
            .filter(|p| p.pid.lock().is_some())
            .map(|p| p.spec.lock().uuid.clone())
            .collect();
        for uuid in &uuids {
            if let Err(error) = self.stop(uuid, false) {
                tracing::warn!("voltd: graceful stop of {uuid} failed: {error}");
            }
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            let any_running = self.processes.iter().any(|p| p.pid.lock().is_some());
            if !any_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        for uuid in &uuids {
            if self
                .processes
                .get(uuid)
                .map(|p| p.pid.lock().is_some())
                .unwrap_or(false)
            {
                tracing::warn!(
                    "voltd: {uuid} did not stop within the grace window; force-killing"
                );
                let _ = self.stop(uuid, true);
            }
        }
    }

    pub fn process(&self, uuid: &str) -> Result<Arc<ManagedProcess>> {
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

/// `Write` adapter that forwards bytes to an inner sink while counting them
/// and feeding them to a SHA-256 hasher. Used by [`DaemonRuntime::write_archive_to`]
/// so the compressed archive is hashed in the same single pass that produces
/// it — no second read of the bytes.
///
/// The count is also a hard production cap (mirror of the restore-side
/// [`HashingReader`]): once the archive exceeds [`MAX_SNAPSHOT_TOTAL_BYTES`]
/// the write errors and production aborts, so the streaming snapshot producer
/// can never ship an archive beyond the panel-side extraction bound — a
/// buggy or hostile panel would otherwise be served an unbounded body.
struct HashCountWriter<W: Write> {
    inner: W,
    hasher: sha2::Sha256,
    count: u64,
    max: u64,
}

impl<W: Write> Write for HashCountWriter<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = self.count.saturating_add(data.len() as u64);
        if n > self.max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot archive exceeds the 2048 MiB production cap",
            ));
        }
        self.hasher.update(data);
        self.inner.write_all(data)?;
        self.count = n;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `Read` adapter that hashes every byte it hands out and hard-caps the total:
/// the streaming restore enforces [`MAX_SNAPSHOT_TOTAL_BYTES`] on the raw
/// body as it is consumed, so an oversized archive is rejected mid-stream
/// before it can fill staging.
struct HashingReader<R: Read> {
    inner: R,
    hasher: sha2::Sha256,
    max: u64,
    count: u64,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R, max: u64) -> Self {
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            max,
            count: 0,
        }
    }
    fn into_parts(self) -> (R, String) {
        (self.inner, hex::encode(self.hasher.finalize()))
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        if n > 0 {
            self.count = self.count.saturating_add(n as u64);
            if self.count > self.max {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "snapshot archive exceeds upload limit",
                ));
            }
            self.hasher.update(&out[..n]);
        }
        Ok(n)
    }
}

/// Kill the whole process group (the bwrap wrapper forks a
/// setpriv/sh/payload tree, so killing only the leader leaks it), then wait()
/// the leader so a failed spawn never leaves a zombie behind, and finally
/// drain + remove the cgroup dir. Mirrors services::proc::reap_failed_spawn.
fn reap_failed_spawn(cgroup: crate::isolation::Cgroup, mut child: Child) {
    kill_daemon_group(child.id());
    let _ = child.wait();
    cgroup.remove(); // kill_all + rmdir with bounded backoff
}

fn kill_daemon_group(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

/// Extract a snapshot tar.gz into `staging`, bounded the same way as the
/// panel's archive extraction (`services::backups::extract_tar_gz`): entry
/// count, per-file and total byte caps, and file modes masked to 0o777 —
/// never honoring setuid/setgid/sticky from an archive header. Link entries
/// and path traversal are re-checked per entry (the caller validates first;
/// this is defense-in-depth). The archive is HMAC-authenticated from the
/// panel, so the bounded copy guards against a malicious or corrupted
/// payload, not a trusted one.
fn extract_snapshot_tar<R: Read>(
    mut decoder: flate2::read::GzDecoder<R>,
    staging: &Path,
) -> Result<flate2::read::GzDecoder<R>> {
    let mut tar = tar::Archive::new(&mut decoder);
    tar.set_unpack_xattrs(false);
    let mut total: u64 = 0;
    let mut count: usize = 0;
    for entry in tar.entries()? {
        count += 1;
        if count > MAX_SNAPSHOT_ARCHIVE_ENTRIES {
            bail!("snapshot has too many entries (max {MAX_SNAPSHOT_ARCHIVE_ENTRIES})");
        }
        let entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            bail!("snapshot contains forbidden link entry");
        }
        let rel = entry.path()?.to_string_lossy().to_string();
        if Path::new(&rel)
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            bail!("snapshot path traversal");
        }
        let path = staging.join(&rel);
        if kind.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }
        // Fail on the declared size before writing anything, then hard-cap
        // the bytes actually copied so a lying header cannot overflow.
        let declared = entry.size();
        if declared > MAX_SNAPSHOT_FILE_BYTES
            || total.saturating_add(declared) > MAX_SNAPSHOT_TOTAL_BYTES
        {
            bail!("snapshot entry exceeds extraction size limits");
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let cap = MAX_SNAPSHOT_FILE_BYTES.saturating_add(1).min(
            MAX_SNAPSHOT_TOTAL_BYTES
                .saturating_sub(total)
                .saturating_add(1),
        );
        let mode = entry.header().mode().ok();
        let copied = std::io::copy(&mut entry.take(cap), &mut f)?;
        if copied > MAX_SNAPSHOT_FILE_BYTES
            || total.saturating_add(copied) > MAX_SNAPSHOT_TOTAL_BYTES
        {
            bail!("snapshot entry exceeds extraction size limits");
        }
        total += copied;
        if let Some(mode) = mode {
            // masked: never honor setuid/setgid/sticky from an archive
            // header (privilege-escalation surface)
            let _ = f.set_permissions(fs::Permissions::from_mode(mode & 0o777));
        }
    }
    // Hand the decoder back so a caller can recover a reader that wraps the
    // raw archive (e.g. the streaming path's hashing reader).
    tar.into_inner();
    Ok(decoder)
}

/// Atomically exchange `a` and `b` (both must exist, on the same filesystem)
/// via `renameat2(RENAME_EXCHANGE)`, so a directory swap can never pass
/// through a state where the destination is absent.
fn renameat2_exchange(a: &Path, b: &Path) -> Result<()> {
    let ca =
        std::ffi::CString::new(a.as_os_str().as_bytes()).map_err(|_| anyhow::anyhow!("invalid path"))?;
    let cb =
        std::ffi::CString::new(b.as_os_str().as_bytes()).map_err(|_| anyhow::anyhow!("invalid path"))?;
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot exchange {} and {}", a.display(), b.display()));
    }
    Ok(())
}

/// Cache for [`exchange_dirs`]: support depends on the filesystem the first
/// exchange runs on (runtime input), hence OnceLock rather than LazyLock.
static RENAMEAT2_SUPPORTED: OnceLock<bool> = OnceLock::new();

/// Swap `a` and `b` so the destination is never absent mid-swap. Uses
/// `renameat2(RENAME_EXCHANGE)` where supported; filesystems that answer
/// EINVAL/ENOSYS (NFS, FUSE, older kernels) fall back to the non-atomic
/// two-rename dance and log once. Mirrors `services::backups::exchange_dirs`.
fn exchange_dirs(a: &Path, b: &Path) -> Result<()> {
    let parent = a.parent().unwrap_or(Path::new("."));
    if *RENAMEAT2_SUPPORTED.get_or_init(|| probe_renameat2(parent)) {
        match renameat2_exchange(a, b) {
            Ok(()) => return Ok(()),
            Err(error) if renameat2_unsupported(&error) => {
                tracing::warn!(
                    "renameat2(RENAME_EXCHANGE) unsupported on this filesystem ({error}); \
                     falling back to a non-atomic two-rename swap"
                );
            }
            Err(error) => return Err(error),
        }
    }
    exchange_dirs_fallback(a, b)
}

/// Probe support once by exchanging two throwaway files next to the first
/// real exchange (same filesystem, so the result is authoritative).
fn probe_renameat2(parent: &Path) -> bool {
    let t1 = parent.join(format!(".renameat2-probe-{}", uuid::Uuid::new_v4().simple()));
    let t2 = parent.join(format!(".renameat2-probe-{}", uuid::Uuid::new_v4().simple()));
    let supported = (|| {
        fs::write(&t1, b"x")?;
        fs::write(&t2, b"y")?;
        match renameat2_exchange(&t1, &t2) {
            Ok(()) => Ok(true),
            Err(error) if renameat2_unsupported(&error) => Ok(false),
            Err(error) => Err(error),
        }
    })();
    let _ = fs::remove_file(&t1);
    let _ = fs::remove_file(&t2);
    match supported {
        Ok(value) => value,
        // Unexpected probe failure (e.g. permissions): assume supported and
        // let a real exchange surface the actual error.
        Err(error) => {
            tracing::debug!("renameat2 probe failed in {}: {error}", parent.display());
            true
        }
    }
}

/// EINVAL/ENOSYS/EOPNOTSUPP from renameat2 mean "this filesystem has no
/// RENAME_EXCHANGE" — the fallback case, not a caller error.
fn renameat2_unsupported(error: &anyhow::Error) -> bool {
    matches!(
        error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::raw_os_error),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

/// Non-atomic fallback: rename `a` aside, rename `b` into place, roll back on
/// failure. A crash between the renames can leave the destination absent (the
/// exact failure the exchange path avoids) — acceptable only on filesystems
/// without `RENAME_EXCHANGE`; the deterministic `.previous-<name>` aside name
/// lets [`recover_stale_dirs`] tell a superseded leftover (safe to reclaim)
/// from the only surviving copy of a crashed swap (must be restored).
fn exchange_dirs_fallback(a: &Path, b: &Path) -> Result<()> {
    let parent = a.parent().unwrap_or(Path::new("."));
    let name = a
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("cannot derive aside name for dir swap")?;
    let aside = parent.join(format!(".previous-{name}"));
    // The live dir `a` still existing proves any pre-existing `.previous-<name>`
    // is a superseded leftover from an earlier completed swap whose cleanup
    // crashed — never the only copy — so it is safe to reclaim before reusing
    // the deterministic aside name.
    if aside.exists() {
        if let Err(error) = fs::remove_dir_all(&aside) {
            tracing::warn!(
                "removing superseded leftover {} before swap: {error}",
                aside.display()
            );
        }
    }
    fs::rename(a, &aside)?;
    if let Err(error) = fs::rename(b, a) {
        if let Err(rollback) = fs::rename(&aside, a) {
            tracing::error!(
                "rollback of superseded dir {} to {} failed: {rollback}",
                aside.display(),
                a.display()
            );
        }
        return Err(error).with_context(|| format!("cannot swap {} and {}", a.display(), b.display()));
    }
    if let Err(error) = fs::remove_dir_all(&aside) {
        tracing::warn!(
            "could not remove superseded dir {}: {error}",
            aside.display()
        );
    }
    Ok(())
}
/// Live server dir names are hyphenated (UUIDs, or test names); pre-change
/// aside names were `.previous-<32 hex>` with no dash. The dash is the
/// discriminator between a mappable new-style aside and an old-style leftover.
fn aside_targets_server_dir(name: &str) -> bool {
    name.contains('-')
}

/// Startup recovery for crash leftovers in the servers dir. Runs in two
/// passes:
///
/// 1. **Restore** any `.previous-<name>` whose live dir `<name>` is missing:
///    that is the crash window between the two renames of the non-atomic
///    fallback swap, where the aside holds the ONLY copy of the server's data
///    (NFS/FUSE filesystems without `RENAME_EXCHANGE`). Deleting it would
///    destroy the last surviving copy.
/// 2. **Reclaim** everything provably safe to drop: `.restore-*` staging dirs
///    (their content was freshly extracted from an archive that still exists,
///    so they are never the only copy), `.previous-*` dirs whose live dir is
///    present (a completed swap left them superseded), and stray
///    `.renameat2-probe-*` files from a crashed support probe.
///
/// Old-style `.previous-<random>` aside names (no dash, from versions before
/// the deterministic naming) cannot be mapped to a server dir from the
/// filesystem alone; they are left in place with a warning rather than risk
/// deleting the only copy of a crashed swap.
///
/// MUST be called exactly once at startup, before any restore is served
/// (mirrors the panel's `services::backups` boot sweep). Never concurrent
/// with a restore.
fn recover_stale_dirs(servers_dir: &Path) -> Result<usize> {
    if !servers_dir.exists() {
        return Ok(0);
    }
    let collect = |prefix: &str| -> Result<Vec<(PathBuf, String, fs::FileType)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(servers_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) {
                out.push((entry.path(), name, entry.file_type()?));
            }
        }
        Ok(out)
    };
    let mut actions = 0usize;
    // Pass 1 — restore before anything is deleted.
    for (path, name, file_type) in collect(".previous-")? {
        if !file_type.is_dir() {
            continue;
        }
        let target_name = &name[".previous-".len()..];
        let target = servers_dir.join(target_name);
        if target.exists() {
            continue; // completed swap; superseded — pass 2 reclaims it.
        }
        if !aside_targets_server_dir(target_name) {
            tracing::warn!(
                "recovery: {} has no matching server dir and no recoverable \
                 server name; leaving it for manual inspection",
                path.display()
            );
            actions += 1;
            continue;
        }
        match fs::rename(&path, &target) {
            Ok(()) => {
                tracing::warn!(
                    "recovery: RESTORED server dir {} from crash leftover {} \
                     (server dir was missing; the aside held the only copy)",
                    target.display(),
                    path.display()
                );
                actions += 1;
            }
            Err(error) => tracing::warn!(
                "recovery: could not restore server dir from {}: {error}",
                path.display()
            ),
        }
    }
    // Pass 2 — reclaim everything provably safe to drop.
    for (path, _name, file_type) in collect(".restore-")? {
        if file_type.is_dir() && fs::remove_dir_all(&path).is_ok() {
            tracing::warn!(
                "recovery: removed stale restore staging dir {}",
                path.display()
            );
            actions += 1;
        }
    }
    for (path, name, file_type) in collect(".previous-")? {
        let target = servers_dir.join(&name[".previous-".len()..]);
        if file_type.is_dir() && target.exists() && fs::remove_dir_all(&path).is_ok() {
            tracing::warn!(
                "recovery: removed superseded dir {} (live dir {} already in place)",
                path.display(),
                target.display()
            );
            actions += 1;
        }
    }
    for (path, _name, file_type) in collect(".renameat2-probe-")? {
        if file_type.is_file() && fs::remove_file(&path).is_ok() {
            actions += 1;
        }
    }
    Ok(actions)
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
/// Incremental UTF-8 decoder for byte-chunked console streams. A multi-byte
/// character split at a chunk boundary would otherwise decode as U+FFFD;
/// the carry keeps the incomplete trailing bytes (at most 3 — a UTF-8
/// sequence is at most 4 bytes) and prepends them to the next chunk.
#[derive(Default)]
pub struct Utf8Carry {
    pending: [u8; 4],
    len: usize,
}

impl Utf8Carry {
    /// Decode the next chunk; returns the text it completed and retains any
    /// incomplete trailing sequence for the next call.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() + self.len);
        let mut rest = bytes;
        if self.len > 0 {
            // Finish the carried sequence first: it starts with a valid lead
            // byte and only ever gains continuation bytes, so completing it
            // cannot produce an invalid sequence.
            let needed = utf8_seq_len(self.pending[0]) - self.len;
            let take = needed.min(rest.len());
            self.pending[self.len..self.len + take].copy_from_slice(&rest[..take]);
            self.len += take;
            rest = &rest[take..];
            if self.len < utf8_seq_len(self.pending[0]) {
                return out; // still incomplete — wait for the next chunk
            }
            match std::str::from_utf8(&self.pending[..self.len]) {
                Ok(s) => out.push_str(s),
                // Defensive: carried bytes are a valid lead + continuations.
                Err(_) => out.push('\u{FFFD}'),
            }
            self.len = 0;
        }
        let mut i = 0;
        while i < rest.len() {
            match std::str::from_utf8(&rest[i..]) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        out.push_str(
                            std::str::from_utf8(&rest[i..i + valid])
                                .expect("valid_up_to is a char boundary"),
                        );
                        i += valid;
                    }
                    match err.error_len() {
                        Some(bad) => {
                            out.push('\u{FFFD}');
                            i += bad;
                        }
                        None => {
                            // Input ended mid-sequence: carry the tail.
                            let tail = &rest[i..];
                            self.pending[..tail.len()].copy_from_slice(tail);
                            self.len = tail.len();
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}

/// Length in bytes of the UTF-8 sequence whose first byte is `lead`.
fn utf8_seq_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn spawn_reader<R: Read + Send + 'static>(proc: Arc<ManagedProcess>, mut reader: R, stderr: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut carry = Utf8Carry::default();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    proc.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    let mut text = carry.push(&buf[..n]);
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

/// Rename an unparseable spec aside as `<name>.corrupt` so the next boot
/// does not re-attempt it, while keeping it for operator inspection.
fn quarantine_spec(path: &Path) {
    let mut dest = path.as_os_str().to_os_string();
    dest.push(".corrupt");
    if let Err(error) = fs::rename(path, PathBuf::from(dest)) {
        tracing::warn!("voltd: could not quarantine {}: {error}", path.display());
    }
}

/// Exponential crash-restart backoff: 5s, 10s, 20s, ... capped at 2m.
fn restart_backoff(attempt: u64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    let secs = RESTART_BACKOFF_BASE_SECS
        .saturating_mul(1 << exponent)
        .min(RESTART_BACKOFF_MAX_SECS);
    Duration::from_secs(secs)
}

/// dir_size with a per-server min-interval cache: stats() runs on every
/// heartbeat and panel poll, so a full tree walk each time is wasteful.
fn cached_dir_size(proc: &ManagedProcess, root: &Path) -> u64 {
    let now = now_ms();
    let last = proc.dir_cache_ms.load(Ordering::Relaxed);
    if now.saturating_sub(last) < DIR_SIZE_CACHE_MS {
        return proc.dir_cache_bytes.load(Ordering::Relaxed);
    }
    let bytes = dir_size(root);
    proc.dir_cache_bytes.store(bytes, Ordering::Relaxed);
    proc.dir_cache_ms.store(now, Ordering::Relaxed);
    bytes
}

/// Total/used bytes on the filesystem holding `path` (statvfs), so capacity
/// reflects the agent's data volume — not every mounted filesystem.
fn fs_usage(path: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let total = st.f_blocks.saturating_mul(st.f_frsize);
    let available = st.f_bavail.saturating_mul(st.f_frsize);
    Some((total, total.saturating_sub(available)))
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

    #[test]
    fn listen_is_loopback_classifies() {
        for lp in ["localhost:8081", "127.0.0.1:8081", "[::1]:8081"] {
            assert!(listen_is_loopback(lp), "{lp} should be loopback");
        }
        for non in ["0.0.0.0:8081", "10.0.0.5:8081", "[fe80::1]:8081", "node.local:8081"] {
            assert!(!listen_is_loopback(non), "{non} should not be loopback");
        }
    }

    #[test]
    fn daemon_config_roundtrip_without_allow_http_bind() {
        let old = "listen = \"127.0.0.1:8081\"\ndata_dir = \"./voltd-data\"\n\
                   panel_url = \"\"\npublic_url = \"\"\nnode_id = \"n\"\nsecret = \"s\"\n\
                   heartbeat_interval_secs = 15\nmax_upload_mb = 256\nplaintext = true\n\
                   panel_fingerprint = \"fp\"\n";
        let cfg: DaemonConfig = toml::from_str(old).expect("old config without the field loads");
        assert!(!cfg.allow_http_bind, "missing field defaults to false");
        let back: DaemonConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(cfg.listen, back.listen);
        assert_eq!(cfg.plaintext, back.plaintext);
        assert!(!back.allow_http_bind);
    }

    #[test]
    fn utf8_carry_joins_split_characters() {
        // € = E2 82 AC, split across three separate pushes.
        let mut carry = Utf8Carry::default();
        assert_eq!(carry.push(&[0xE2]), "");
        assert_eq!(carry.push(&[0x82]), "");
        assert_eq!(carry.push(&[0xAC, b'!']), "€!");
        // Every possible boundary: feed a multi-char string one byte at a time.
        let mut carry = Utf8Carry::default();
        let s = "héllo wörld — fin";
        let mut out = String::new();
        for b in s.as_bytes() {
            out.push_str(&carry.push(&[*b]));
        }
        assert_eq!(out, s);
    }

    #[test]
    fn utf8_carry_handles_four_byte_and_invalid_bytes() {
        // 😀 = F0 9F 98 80 split mid-sequence.
        let mut carry = Utf8Carry::default();
        assert_eq!(carry.push(&[0xF0, 0x9F]), "");
        assert_eq!(carry.push(&[0x98, 0x80, b'\n']), "😀\n");
        // Invalid bytes still decode lossily, one replacement per bad run.
        let mut carry = Utf8Carry::default();
        assert_eq!(carry.push(b"ok\xFFbad"), "ok\u{FFFD}bad");
        assert_eq!(carry.push(b"a\xC3\x28z"), "a\u{FFFD}(z");
    }

    #[test]
    fn clear_console_keeps_cursor_monotonic() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            ..Default::default()
        };
        let rt = DaemonRuntime::new(config).unwrap();
        let uuid = "clear-test";
        rt.processes.insert(
            uuid.to_string(),
            Arc::new(ManagedProcess::new(ServerSpec {
                uuid: uuid.to_string(),
                name: "t".into(),
                startup: "sleep 1".into(),
                stop_command: String::new(),
                memory_mb: 64,
                disk_mb: 64,
                cpu_percent: 10,
                port: None,
                ports: vec![],
                env: vec![],
                auto_restart: false,
            })),
        );
        rt.processes
            .get(uuid)
            .unwrap()
            .append_console("before".into());
        let pre = rt.console(uuid, 0).unwrap().1;
        assert_eq!(pre, 1);
        rt.clear_console(uuid).unwrap();
        rt.processes
            .get(uuid)
            .unwrap()
            .append_console("after".into());
        // A viewer holding the pre-clear cursor must still see the new line:
        // rewinding the cursor would silently drop every post-clear line.
        let (lines, post) = rt.console(uuid, pre).unwrap();
        assert_eq!(lines, vec!["after"]);
        assert!(
            post >= pre,
            "cursor rewound after clear: {pre} -> {post}"
        );
    }

    #[test]
    fn restart_backoff_grows_exponentially_and_caps() {
        assert_eq!(restart_backoff(1), Duration::from_secs(5));
        assert_eq!(restart_backoff(2), Duration::from_secs(10));
        assert_eq!(restart_backoff(3), Duration::from_secs(20));
        assert_eq!(restart_backoff(4), Duration::from_secs(40));
        assert_eq!(restart_backoff(5), Duration::from_secs(80));
        assert_eq!(restart_backoff(6), Duration::from_secs(120));
        assert_eq!(restart_backoff(99), Duration::from_secs(120));
    }

    #[test]
    fn load_specs_quarantines_unparseable_meta() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            ..Default::default()
        };
        config.ensure_dirs().unwrap();
        let meta = config.meta_dir();
        fs::write(meta.join("bad1.json"), b"{ not json").unwrap();
        fs::write(meta.join("bad2.json"), b"[]").unwrap();
        let rt = DaemonRuntime::new(config).unwrap();
        assert!(rt.processes.is_empty());
        assert!(
            meta.join("bad1.json.corrupt").exists(),
            "unparseable spec must be quarantined"
        );
        assert!(meta.join("bad2.json.corrupt").exists());
        assert!(!meta.join("bad1.json").exists());
    }

    #[test]
    fn stdin_writer_delivers_commands_without_dropping_handle() {
        // `cat` echoes every stdin line to stdout. Two successive commands
        // through the same writer prove the handle is never taken and dropped
        // by a write: an EOF on stdin would end `cat` and the second command
        // would fail — the exact defect this writer replaces.
        let mut child = std::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(STDIN_QUEUE_CAP);
        std::thread::spawn(move || stdin_writer_loop(stdin, rx));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let send = |line: &str| {
            rt.block_on(async {
                let (ack_tx, ack_rx) = oneshot::channel();
                tx.try_send(StdinCmd {
                    line: line.into(),
                    ack: ack_tx,
                })
                .unwrap();
                ack_rx.await.unwrap().unwrap()
            })
        };
        send("one\n");
        send("two\n");
        send("three\n");
        // only then does `cat` see EOF and exit.
        drop(tx);
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let _ = child.wait();
        assert_eq!(out, "one\ntwo\nthree\n");
    }

    #[test]
    fn recovery_restores_aside_when_live_dir_missing() {
        // A `.previous-<name>` with no live `<name>` is the crash window of
        // the non-atomic fallback swap: the aside holds the only copy and
        // must be renamed back, not deleted.
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir_all(&servers).unwrap();
        let aside = servers.join(".previous-abc-123");
        fs::create_dir_all(aside.join("data")).unwrap();
        fs::write(aside.join("data/file.txt"), b"precious").unwrap();
        let actions = recover_stale_dirs(&servers).unwrap();
        assert_eq!(actions, 1);
        let live = servers.join("abc-123");
        assert!(
            live.join("data/file.txt").exists(),
            "aside must be restored into the live dir"
        );
        assert_eq!(fs::read(live.join("data/file.txt")).unwrap(), b"precious");
        assert!(!aside.exists(), "aside consumed by the restore");
    }

    #[test]
    fn recovery_reclaims_superseded_and_staging_leftovers() {
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir_all(&servers).unwrap();
        // Live dir present -> `.previous-<name>` is a completed-swap leftover.
        fs::create_dir_all(servers.join("abc-123")).unwrap();
        fs::create_dir_all(servers.join(".previous-abc-123")).unwrap();
        fs::create_dir_all(servers.join(".restore-abc-456")).unwrap();
        fs::write(servers.join(".renameat2-probe-deadbeef"), b"x").unwrap();
        let actions = recover_stale_dirs(&servers).unwrap();
        assert_eq!(actions, 3);
        assert!(servers.join("abc-123").exists());
        assert!(!servers.join(".previous-abc-123").exists());
        assert!(!servers.join(".restore-abc-456").exists());
        assert!(!servers.join(".renameat2-probe-deadbeef").exists());
    }

    #[test]
    fn recovery_leaves_unmappable_old_style_aside_alone() {
        // Pre-change aside name: `.previous-<32 hex>`, no dash, no live dir.
        // It cannot be mapped to a server dir, so recovery must keep it
        // rather than risk deleting the only copy of a crashed swap.
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir_all(&servers).unwrap();
        let aside = servers.join(".previous-0123456789abcdef0123456789abcdef");
        fs::create_dir_all(&aside).unwrap();
        fs::write(aside.join("data.txt"), b"only copy").unwrap();
        let actions = recover_stale_dirs(&servers).unwrap();
        assert_eq!(actions, 1, "unmappable aside counts as a warned action");
        assert!(aside.join("data.txt").exists(), "must not delete the aside");
    }

    #[test]
    fn fallback_exchange_uses_deterministic_aside_and_reclaims_it() {
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir_all(&servers).unwrap();
        let a = servers.join("abc-123");
        let b = servers.join(".restore-abc-rand");
        fs::create_dir_all(a.join("old")).unwrap();
        fs::create_dir_all(b.join("new")).unwrap();
        // A pre-existing aside under the deterministic name is a superseded
        // leftover (the live dir still exists), so the swap must reclaim it
        // before reusing the name — never treat it as the only copy.
        fs::create_dir_all(servers.join(".previous-abc-123")).unwrap();
        fs::write(servers.join(".previous-abc-123/stale.txt"), b"s").unwrap();
        exchange_dirs_fallback(&a, &b).unwrap();
        assert!(a.join("new").exists(), "live dir now holds staging content");
        assert!(
            !b.exists(),
            "staging name consumed by the rename into the live dir"
        );
        assert!(
            !a.join("old").exists(),
            "superseded content reclaimed with the aside"
        );
        assert!(
            !servers.join(".previous-abc-123").exists(),
            "aside reclaimed after the completed swap"
        );
        let leftovers: Vec<_> = fs::read_dir(&servers)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".previous-"))
            .collect();
        assert!(leftovers.is_empty(), "no stray aside survives the swap");
    }

    fn test_spec(uuid: &str) -> ServerSpec {
        ServerSpec {
            uuid: uuid.to_string(),
            name: "t".into(),
            startup: "sleep 1".into(),
            stop_command: String::new(),
            memory_mb: 64,
            disk_mb: 64,
            cpu_percent: 10,
            port: None,
            ports: vec![],
            env: vec![],
            auto_restart: false,
        }
    }

    #[test]
    fn snapshot_stream_and_restore_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            max_upload_mb: 4,
            ..Default::default()
        };
        let rt = DaemonRuntime::new(config).unwrap();
        let uuid = "snap-test";
        rt.processes
            .insert(uuid.to_string(), Arc::new(ManagedProcess::new(test_spec(uuid))));
        let root = rt.server_root(uuid).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello world").unwrap();
        fs::write(root.join("sub/b.bin"), vec![7u8; 100_000]).unwrap();
        fs::set_permissions(root.join("a.txt"), fs::Permissions::from_mode(0o751)).unwrap();

        // Streaming snapshot: the archive is produced in one pass into the
        // caller's sink, hashed as it goes.
        let mut archive = Vec::new();
        let (size, sha) = rt.write_archive_to(uuid, &mut archive).unwrap();
        assert_eq!(size as usize, archive.len());
        assert_eq!(sha.len(), 64);

        // Streaming restore: consume the raw archive, extracting into staging
        // and swapping only after the verification callback accepts the hash.
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let seen2 = seen.clone();
        let expected_sha = sha.clone();
        assert!(rt
            .restore_snapshot_stream(uuid, &archive[..], move |got| {
                *seen2.lock() = Some(got.clone());
                assert_eq!(got, expected_sha, "verifier must see the archive's SHA-256");
                Ok(())
            })
            .unwrap());
        assert_eq!(seen.lock().as_deref(), Some(sha.as_str()));
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello world");
        assert_eq!(fs::read(root.join("sub/b.bin")).unwrap(), vec![7u8; 100_000]);
        // The verified hash is of the raw archive bytes, so a downstream MAC
        // check can re-derive it from the same bytes.
        let mut hasher = sha2::Sha256::new();
        hasher.update(&archive);
        assert_eq!(hex::encode(hasher.finalize()), sha);
    }

    #[test]
    fn restore_stream_refuses_bad_verification_without_touching_live_dir() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            max_upload_mb: 4,
            ..Default::default()
        };
        let rt = DaemonRuntime::new(config).unwrap();
        let uuid = "restore-refuse";
        rt.processes
            .insert(uuid.to_string(), Arc::new(ManagedProcess::new(test_spec(uuid))));
        let root = rt.server_root(uuid).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.txt"), b"original").unwrap();
        let mut archive = Vec::new();
        rt.write_archive_to(uuid, &mut archive).unwrap();

        let err = rt
            .restore_snapshot_stream(uuid, &archive[..], |_| {
                anyhow::bail!("deferred MAC check failed")
            })
            .unwrap_err();
        assert!(err.to_string().contains("MAC check failed"));
        // Nothing was applied: the live dir is untouched and no staging
        // leftover survives.
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"original");
        let leftovers: Vec<_> = fs::read_dir(temp.path().join("servers"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".restore-"))
            .collect();
        assert!(leftovers.is_empty(), "failed restore must clean its staging");
    }

    #[test]
    fn restore_stream_body_reader_hard_caps_at_snapshot_bound() {
        // The streaming restore bounds the raw body by
        // `MAX_SNAPSHOT_TOTAL_BYTES` (parity with the producer and extraction
        // caps, and with what local restores accept) rather than the legacy
        // `max_upload_mb`. Exercised directly here (the real bound is 2 GiB);
        // a small `max` stands in for the constant.
        use std::io::Read as _;
        let mut inner = Vec::new();
        inner.extend_from_slice(&[0u8; 100]);
        let mut r = HashingReader::new(&inner[..], 64);
        let mut buf = [0u8; 32];
        r.read_exact(&mut buf).unwrap();
        r.read_exact(&mut buf).unwrap();
        // Third read crosses 64: the cap must trip mid-stream, not at EOF.
        let err = r
            .read_exact(&mut buf)
            .expect_err("crossing the cap must error mid-stream");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds upload limit"),
            "unexpected error: {err}"
        );
    }
    #[test]
    fn legacy_snapshot_matches_streaming_archive() {
        // Both snapshot paths must produce the same bytes + checksum for the
        // same tree: a panel that has not upgraded to the streaming protocol
        // must get exactly what the streaming panel would have received.
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            max_upload_mb: 4,
            ..Default::default()
        };
        let rt = DaemonRuntime::new(config).unwrap();
        let uuid = "snap-dual";
        rt.processes
            .insert(uuid.to_string(), Arc::new(ManagedProcess::new(test_spec(uuid))));
        let root = rt.server_root(uuid).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("f.txt"), b"dual-mode").unwrap();

        let mut streamed = Vec::new();
        let (size, sha) = rt.write_archive_to(uuid, &mut streamed).unwrap();
        let legacy = rt.snapshot(uuid).unwrap();
        assert_eq!(legacy.checksum, sha);
        assert_eq!(legacy.size_bytes, size);
        use base64::Engine;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(legacy.archive_b64)
                .unwrap(),
            streamed
        );
    }

    #[test]
    fn provision_masks_special_file_modes() {
        let temp = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            data_dir: temp.path().into(),
            max_upload_mb: 4,
            ..Default::default()
        };
        let rt = DaemonRuntime::new(config).unwrap();
        let uuid = "mode-test";
        let req = ProvisionRequest {
            spec: test_spec(uuid),
            files: vec![crate::node_protocol::ProvisionFile {
                path: "s.sh".into(),
                content_b64: {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.encode(b"#!/bin/sh\n")
                },
                mode: Some(0o4755), // setuid + rwxr-xr-x
            }],
        };
        rt.provision(req).unwrap();
        let mode = fs::metadata(rt.server_root(uuid).unwrap().join("s.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o7777,
            0o755,
            "setuid/setgid/sticky must be stripped from provisioned modes"
        );
    }

    #[test]
    fn archive_producer_hard_caps_the_snapshot_body() {
        // The streaming snapshot producer must refuse to emit an archive past
        // the panel-side extraction bound: `write_archive_to` wires
        // `HashCountWriter` with `MAX_SNAPSHOT_TOTAL_BYTES`, so production
        // errors the moment the cap is crossed instead of streaming an
        // unbounded body. Exercised directly here (the real bound is 2 GiB);
        // a small `max` stands in for the constant.
        use std::io::Write as _;
        let mut out = Vec::new();
        // The writer borrows `out` mutably, so it lives in its own scope;
        // once it drops, `out` is free for inspection.
        let count = {
            let mut w = HashCountWriter {
                inner: &mut out,
                hasher: sha2::Sha256::new(),
                count: 0,
                max: 64,
            };
            assert!(w.write_all(&[0u8; 64]).is_ok());
            let err = w
                .write_all(&[1u8; 8])
                .expect_err("crossing the cap must error mid-production");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                err.to_string().contains("production cap"),
                "unexpected error: {err}"
            );
            // Bytes up to the cap were forwarded + hashed; the overflow was not.
            w.count
        };
        assert_eq!(out.len(), 64);
        assert_eq!(count, 64);
    }
}
