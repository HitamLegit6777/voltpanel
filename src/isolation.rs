//! Linux workload isolation shared by the control plane and execution agent.
//!
//! Security invariants:
//! - fail closed when bubblewrap/cgroup v2 is unavailable;
//! - unique host UID/GID and mode-0700 root per server;
//! - separate mount, PID, IPC, UTS and cgroup namespaces;
//! - host filesystem exposed read-only only for runtime paths;
//! - only `/home/container`, `/tmp`, `/run` are writable;
//! - cgroup v2 enforces memory, CPU and process count;
//! - host networking is intentionally shared so allocated game ports are reachable;
//!   collision prevention is therefore mandatory at the node scheduler.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct AtomicFlagGuard<'a>(&'a AtomicBool);
impl<'a> AtomicFlagGuard<'a> {
    pub fn acquire(flag: &'a AtomicBool) -> Result<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("operation already in progress"))?;
        Ok(Self(flag))
    }
}
impl Drop for AtomicFlagGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub const MIN_SERVER_UID: u32 = 200_000;
pub const UID_RANGE: u32 = 400_000;
pub const DEFAULT_PIDS_MAX: u64 = 512;

/// Where the server's Data Lab directory appears inside a sandbox, and the
/// environment variable that advertises it to the workload. The path lives on
/// bwrap's private root (not inside the workload-owned server tree), so a
/// workload can never pre-plant a symlink at the mount point.
pub const DATALAB_MOUNT_DIR: &str = "/data/.voltp/databases";
pub const DATALAB_ENV_VAR: &str = "VOLTP_DATALAB_DIR";
static IDENTITY_LOCK: std::sync::LazyLock<parking_lot::Mutex<()>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsolationConfig {
    pub enabled: bool,
    pub fail_closed: bool,
    pub cgroup_root: PathBuf,
    pub pids_max: u64,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fail_closed: true,
            cgroup_root: delegated_cgroup_root().join("voltpanel"),
            pids_max: DEFAULT_PIDS_MAX,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IsolationStatus {
    pub bubblewrap: bool,
    pub cgroup_v2: bool,
    pub cgroup_writable: bool,
    pub missing_controllers: Vec<String>,
    pub user_namespace: bool,
    pub capabilities_dropped: bool,
    pub no_new_privs: bool,
    pub systemd_scope: bool,
    pub fail_closed: bool,
    pub secure: bool,
    pub message: String,
}

pub fn probe(config: &IsolationConfig) -> IsolationStatus {
    let bubblewrap = which("bwrap");
    let cgroup_v2 = fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").is_ok();
    let (cgroup_writable, missing_controllers) = probe_cgroup_write(&config.cgroup_root);
    let systemd_scope = which("systemd-run") && unsafe { libc::geteuid() } == 0;
    let setpriv = which("setpriv") && unsafe { libc::geteuid() } == 0;
    // The sandbox never creates a user namespace (bwrap runs with the
    // daemon's UID), so this flag reports the sandbox's configuration rather
    // than a probed host capability; it is intentionally false.
    let user_namespace = false;
    // Measured, not assumed: the exact setpriv flag set sandbox_command passes
    // is exercised and the resulting process must report an empty effective
    // capability set and no_new_privs. Binary presence alone proves nothing.
    let (capabilities_dropped, no_new_privs) = if setpriv {
        probe_privdrop()
    } else {
        (false, false)
    };
    let secure = !config.enabled
        || (bubblewrap
            && cgroup_v2
            && (cgroup_writable || systemd_scope)
            && capabilities_dropped
            && no_new_privs);
    let message = if secure {
        "mount/PID/IPC/UTS/network namespaces, zero capabilities, no-new-privs and cgroup limits available".into()
    } else {
        let mut missing = String::new();
        if !bubblewrap {
            missing.push_str("bubblewrap ");
        }
        if !cgroup_v2 {
            missing.push_str("cgroup-v2 ");
        }
        if !cgroup_writable && !missing_controllers.is_empty() {
            missing.push_str(&format!(
                "cgroup-controllers({}) ",
                missing_controllers.join(",")
            ));
        }
        if !cgroup_writable && !systemd_scope {
            missing.push_str("cgroup-delegation ");
        }
        if !setpriv {
            missing.push_str("setpriv ");
        } else {
            if !capabilities_dropped {
                missing.push_str("privdrop ");
            }
            if !no_new_privs {
                missing.push_str("no-new-privs ");
            }
        }
        format!("missing: {missing}")
    };
    IsolationStatus {
        bubblewrap,
        cgroup_v2,
        cgroup_writable,
        missing_controllers,
        systemd_scope,
        user_namespace,
        capabilities_dropped,
        no_new_privs,
        fail_closed: config.fail_closed,
        secure,
        message,
    }
}

fn delegated_cgroup_root() -> PathBuf {
    let relative = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|v| {
            v.lines()
                .find_map(|line| line.strip_prefix("0::").map(ToOwned::to_owned))
        })
        .unwrap_or_else(|| "/".into());
    Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'))
}
const REQUIRED_CONTROLLER_FILES: [&str; 3] = ["memory.max", "cpu.max", "pids.max"];

/// Which of the required cgroup limit files is absent from `dir`.
fn missing_required_controllers(dir: &Path) -> Vec<String> {
    REQUIRED_CONTROLLER_FILES
        .iter()
        .filter(|f| !dir.join(f).exists())
        .map(|f| f.trim_end_matches(".max").to_string())
        .collect()
}

fn probe_cgroup_write(root: &Path) -> (bool, Vec<String>) {
    let parent = root.parent().unwrap_or(Path::new("/sys/fs/cgroup"));
    // Mirror what Cgroup::create does (enable controllers, then create a
    // child) so the probe reports the controllers a server cgroup would
    // actually get, not the delegation's raw state.
    let _ = enable_subtree_control(parent);
    let probe = parent.join(format!("vp-probe-{}", std::process::id()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let missing = missing_required_controllers(&probe);
            let writable = missing.is_empty()
                && REQUIRED_CONTROLLER_FILES.iter().all(|f| {
                    fs::read_to_string(probe.join(f))
                        .ok()
                        .and_then(|v| fs::write(probe.join(f), v.trim()).ok())
                        .is_some()
                });
            let _ = fs::remove_dir(&probe);
            (writable, missing)
        }
        Err(_) => (false, missing_required_controllers(parent)),
    }
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(binary).is_file()))
        .unwrap_or(false)
}
/// Functionally verifies that setpriv drops every capability and sets
/// no_new_privs the way `sandbox_command` relies on: run the exact flag set
/// and require the resulting process to report an empty effective capability
/// set and `NoNewPrivs: 1`.
fn probe_privdrop() -> (bool, bool) {
    let output = Command::new("setpriv")
        .args([
            "--no-new-privs",
            "--bounding-set=-all",
            "--inh-caps=-all",
            "--ambient-caps=-all",
            "--",
            "/bin/sh",
            "-c",
            "grep -E '^(CapEff|NoNewPrivs):' /proc/self/status",
        ])
        .output();
    let Ok(output) = output else {
        return (false, false);
    };
    if !output.status.success() {
        return (false, false);
    }
    let mut capabilities = false;
    let mut no_new_privs = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(v) = line.strip_prefix("CapEff:") {
            capabilities = v.trim() == "0000000000000000";
        } else if let Some(v) = line.strip_prefix("NoNewPrivs:") {
            no_new_privs = v.trim() == "1";
        }
    }
    (capabilities, no_new_privs)
}

pub fn server_identity(uuid: &str) -> (u32, u32) {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
    let raw = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let id = MIN_SERVER_UID + raw % UID_RANGE;
    (id, id)
}
/// Deterministic walk over the server UID pool: the first id at or after
/// `start` (wrapping) that no sibling root already owns. Pure so the
/// collision policy is unit-testable without touching the filesystem.
fn pick_uid(start: u32, used: &std::collections::HashSet<u32>) -> Option<u32> {
    let mut candidate = start;
    for _ in 0..UID_RANGE {
        if !used.contains(&candidate) {
            return Some(candidate);
        }
        candidate = MIN_SERVER_UID + (candidate - MIN_SERVER_UID + 1) % UID_RANGE;
    }
    None
}
/// Cross-process serialization of the server UID scan+chown. `IDENTITY_LOCK`
/// only covers one process, but the panel and a voltd agent share a host, so
/// an flock is needed to make scan-then-chown atomic across both. Root uses
/// a fixed /run lockfile; unprivileged users lock inside their own
/// servers_dir, where the scan itself lives.
struct IdentityLock {
    // Held only to keep the flock alive; never read, released on drop.
    #[allow(dead_code)]
    file: std::fs::File,
}
impl IdentityLock {
    fn acquire(servers_dir: &Path) -> Result<Self> {
        let path = if unsafe { libc::geteuid() } == 0 {
            let dir = Path::new("/run/voltpanel");
            fs::create_dir_all(dir).context("create /run/voltpanel")?;
            dir.join("uid.lock")
        } else {
            servers_dir.join(".uid.lock")
        };
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open identity lock {}", path.display()))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("flock identity lock");
        }
        Ok(Self { file })
    }
}
pub fn prepare_root(root: &Path, uuid: &str) -> Result<(u32, u32)> {
    let _guard = IDENTITY_LOCK.lock();
    fs::create_dir_all(root)?;
    // Serialize scan+chown across every VoltPanel process on this host: the
    // pool scan must not interleave with another process's chown or two
    // processes could allocate the same uid for different roots.
    let _cross_process = IdentityLock::acquire(root.parent().context("server root has no parent")?)?;
    use std::collections::HashSet;
    let mut used = HashSet::new();
    if let Some(parent) = root.parent() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry.path() == root {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if (MIN_SERVER_UID..MIN_SERVER_UID + UID_RANGE).contains(&meta.uid()) {
                    used.insert(meta.uid());
                }
            }
        }
    }
    let current = fs::metadata(root)?.uid();
    let uid = if (MIN_SERVER_UID..MIN_SERVER_UID + UID_RANGE).contains(&current)
        && !used.contains(&current)
    {
        current
    } else {
        let (candidate, _) = server_identity(uuid);
        pick_uid(candidate, &used).context("server UID pool exhausted")?
    };
    let gid = uid;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    if unsafe { libc::geteuid() } == 0 {
        let cpath = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())?;
        if unsafe { libc::chown(cpath.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("chown server root");
        }
    }
    Ok((uid, gid))
}

pub fn validate_root(root: &Path, _uuid: &str) -> Result<()> {
    let meta = fs::metadata(root)?;
    if meta.mode() & 0o077 != 0 {
        bail!("server root permissions are not private (expected 0700)")
    }
    if unsafe { libc::geteuid() } == 0
        && (meta.uid() < MIN_SERVER_UID
            || meta.uid() >= MIN_SERVER_UID + UID_RANGE
            || meta.uid() != meta.gid())
    {
        bail!("server root ownership is outside the isolated UID pool")
    }
    Ok(())
}

pub fn own_tree(root: &Path, _uuid: &str) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(());
    }
    let meta = fs::metadata(root)?;
    let (uid, gid) = (meta.uid(), meta.gid());
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let cpath = std::ffi::CString::new(entry.path().as_os_str().as_encoded_bytes())?;
        if unsafe { libc::lchown(cpath.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("chown server tree");
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub memory_bytes: u64,
    pub cpu_percent: u64,
    pub pids_max: u64,
}

#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub fn create(config: &IsolationConfig, uuid: &str, limits: &Limits) -> Result<Self> {
        if !config.enabled {
            return Ok(Self {
                path: PathBuf::new(),
            });
        }
        let status = probe(config);
        if !status.secure && config.fail_closed {
            bail!("isolation unavailable: {}", status.message);
        }
        if !status.cgroup_writable {
            if status.systemd_scope {
                return Ok(Self {
                    path: PathBuf::new(),
                });
            }
            bail!("no delegated cgroup or systemd scope available");
        }
        let path = config.cgroup_root.join(sanitize(uuid));
        fs::create_dir_all(&config.cgroup_root).context("create VoltPanel cgroup root")?;
        enable_controllers(&config.cgroup_root)?;
        // Enable controllers on the root before creating the server cgroup,
        // otherwise the limit files do not exist in the child. The server
        // directory itself must exist before any limit is written.
        enable_subtree_control(&config.cgroup_root)?;
        fs::create_dir_all(&path).context("create server cgroup")?;
        let mut scratch = CgroupScratch::new(path.clone());
        let missing = missing_required_controllers(&path);
        if !missing.is_empty() {
            bail!(
                "required cgroup controllers unavailable in {}: {}",
                path.display(),
                missing.join(", ")
            );
        }
        let memory = limits.memory_bytes.max(16 * 1_048_576).to_string();
        write(&path.join("memory.max"), &memory)?;
        verify_limit(&path.join("memory.max"), &memory)?;
        let pids = limits.pids_max.max(32).to_string();
        write(&path.join("pids.max"), &pids)?;
        verify_limit(&path.join("pids.max"), &pids)?;
        let quota = limits.cpu_percent.max(1).saturating_mul(1000);
        write(&path.join("cpu.max"), &format!("{quota} 100000"))?;
        verify_cpu_limit(&path.join("cpu.max"), quota)?;
        // Swap is optional: disable it only when the swap controller (and
        // thus memory.swap.max) exists. Absent swap support must not fail
        // server starts; present swap must be enforced off.
        write_swap_limit(&path)?;
        if path.join("memory.oom.group").exists() {
            write(&path.join("memory.oom.group"), "1")?;
        }
        scratch.disarm();
        Ok(Self { path })
    }

    pub fn attach(&self, pid: u32) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        // Attach the actual sandbox workload, not just the bwrap wrapper:
        // moving only the wrapper leaves the (already forked) payload in the
        // parent cgroup so its limits never apply. Keep walking and writing
        // the live subtree until every member verifiably lands in the cgroup:
        // a process forked between a walk and its writes would otherwise stay
        // in the daemon's cgroup forever. Membership is inherited by children,
        // so once the whole tree is in, later forks stay in too.
        let procs = self.path.join("cgroup.procs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_500);
        loop {
            let pids = descendant_subtree(pid);
            if pids.len() <= 1 && std::time::Instant::now() >= deadline {
                bail!("sandbox produced no descendant process to attach");
            }
            for (p, start) in &pids {
                // A pid recycled by the kernel since the walk would attach an
                // unrelated process to this server's cgroup; the starttime
                // snapshot taken during the walk makes the write safe.
                if process_starttime(*p) != Some(*start) {
                    continue;
                }
                if let Err(error) = write(&procs, &p.to_string()) {
                    // A pid that exited between the walk and the write races
                    // out of the picture; retried on the next pass. Anything
                    // else (EPERM, vanished cgroup) is a real failure.
                    let gone = error
                        .downcast_ref::<std::io::Error>()
                        .map(|e| e.raw_os_error() == Some(libc::ESRCH))
                        .unwrap_or(false);
                    if !gone {
                        return Err(error);
                    }
                }
            }
            let members = fs::read_to_string(&procs).unwrap_or_default();
            let live = pids
                .iter()
                .filter(|(p, start)| process_starttime(*p) == Some(*start))
                .map(|(p, _)| *p)
                .collect::<Vec<u32>>();
            let all_in = live.len() > 1
                && live.iter().all(|p| {
                    members
                        .split_whitespace()
                        .any(|m| m.parse::<u32>().ok() == Some(*p))
                });
            if all_in {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                if live.len() <= 1 {
                    bail!("sandbox produced no descendant process to attach");
                }
                bail!("sandbox workload could not be moved into the server cgroup");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metrics(&self) -> CgroupMetrics {
        if self.path.as_os_str().is_empty() {
            return CgroupMetrics::default();
        }
        CgroupMetrics {
            memory_current: read_u64(self.path.join("memory.current")),
            memory_events: fs::read_to_string(self.path.join("memory.events")).unwrap_or_default(),
            pids_current: read_u64(self.path.join("pids.current")),
            cpu_stat: fs::read_to_string(self.path.join("cpu.stat")).unwrap_or_default(),
        }
    }

    pub fn kill_all(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if self.path.join("cgroup.kill").exists() {
            write(&self.path.join("cgroup.kill"), "1")?;
            return Ok(());
        }
        // Pre-5.14 kernels lack cgroup.kill: signal every member directly.
        // The membership read is already stale by the time a signal lands:
        // a member that exited in between may have had its pid recycled onto
        // an unrelated host process, and SIGKILL as root would take out the
        // wrong process. Only signal a pid that is still listed in a fresh
        // cgroup.procs read and reports a stable starttime across two
        // immediate /proc reads.
        let procs = fs::read_to_string(self.path.join("cgroup.procs")).unwrap_or_default();
        for pid in procs.split_whitespace() {
            let Ok(pid) = pid.parse::<i32>() else { continue };
            let member = fs::read_to_string(self.path.join("cgroup.procs"))
                .map(|fresh| fresh.split_whitespace().any(|m| m.parse::<i32>() == Ok(pid)))
                .unwrap_or(false);
            let Some(start) = process_starttime(pid as u32) else { continue };
            if !member || process_starttime(pid as u32) != Some(start) {
                continue;
            }
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        Ok(())
    }

    pub fn remove(self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _ = self.kill_all();
        // cgroup.kill is asynchronous and cgroupfs only allows rmdir on an
        // empty cgroup, so retry with a bounded backoff instead of removing
        // the virtual files (which kernfs rejects with EPERM).
        for _ in 0..20 {
            match fs::remove_dir(&self.path) {
                Ok(()) => return,
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => return, // EPERM/ENOENT: nothing more to do here
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct CgroupMetrics {
    pub memory_current: u64,
    pub memory_events: String,
    pub pids_current: u64,
    pub cpu_stat: String,
}

fn enable_controllers(root: &Path) -> Result<()> {
    let parent = root.parent().context("cgroup root has no parent")?;
    enable_subtree_control(parent)
}

/// Enables `cpu`, `memory`, `pids` and `io` in `root`'s
/// `cgroup.subtree_control`, skipping controllers the kernel does not expose
/// and controllers that are already enabled (writing an enabled controller
/// back returns EINVAL). `io` is optional; the others are required and their
/// absence surfaces later as missing limit files.
fn enable_subtree_control(root: &Path) -> Result<()> {
    let available = fs::read_to_string(root.join("cgroup.controllers")).unwrap_or_default();
    let enabled = fs::read_to_string(root.join("cgroup.subtree_control")).unwrap_or_default();
    let wanted = ["cpu", "memory", "pids", "io"]
        .into_iter()
        .filter(|c| available.split_whitespace().any(|a| a == *c))
        .filter(|c| !enabled.split_whitespace().any(|a| a == *c))
        .map(|c| format!("+{c}"))
        .collect::<Vec<_>>();
    if !wanted.is_empty() {
        write(&root.join("cgroup.subtree_control"), &wanted.join(" "))?;
    }
    Ok(())
}
fn write(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).with_context(|| format!("write {}", path.display()))
}
fn read_u64(path: PathBuf) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}
fn parse_u64(v: &str) -> Option<u64> {
    v.parse::<u64>().ok()
}

/// Reads a limit file back and requires the kernel's value to match the
/// requested one, tolerating memory.max's floor-rounding of the written value
/// to a page boundary (writing 16777217 yields 16777216).
fn verify_limit(path: &Path, expected: &str) -> Result<()> {
    let actual = fs::read_to_string(path)
        .with_context(|| format!("read back {}", path.display()))?
        .trim()
        .to_string();
    let matches = actual == expected
        || matches!(
            (parse_u64(&actual), parse_u64(expected)),
            (Some(a), Some(e)) if a == e / 4096 * 4096
        );
    if !matches {
        bail!(
            "limit not applied: {} expected {expected:?} got {actual:?}",
            path.display()
        );
    }
    Ok(())
}

/// Verifies cpu.max took the requested quota. Modern kernels accept quotas
/// above the online-CPU ceiling unchanged; kernels that clamp are accepted
/// too, since the clamped value is the enforced one.
fn verify_cpu_limit(path: &Path, quota: u64) -> Result<()> {
    let actual = fs::read_to_string(path)
        .with_context(|| format!("read back {}", path.display()))?
        .trim()
        .to_string();
    let mut fields = actual.split_whitespace();
    let (Some(actual_quota), Some(period)) = (
        fields.next().and_then(parse_u64),
        fields.next().and_then(parse_u64),
    ) else {
        bail!("cpu.max unreadable: {actual:?}");
    };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as u64;
    let ceiling = cpus * period;
    if actual_quota != quota && !(quota > ceiling && actual_quota == ceiling) {
        bail!(
            "cpu limit not applied: {} expected quota {quota} got {actual:?}",
            path.display()
        );
    }
    Ok(())
}

/// Disables swap via memory.swap.max when the swap controller is present.
/// Returns whether the limit was applied; a kernel without the swap
/// controller simply cannot enforce it and that must not fail the start.
fn write_swap_limit(dir: &Path) -> Result<bool> {
    let path = dir.join("memory.swap.max");
    if !path.exists() {
        return Ok(false);
    }
    write(&path, "0")?;
    verify_limit(&path, "0")?;
    Ok(true)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Syscalls denied by the sandbox seccomp filter (x86_64 numbers, verified
/// against asm/unistd_64.h). Exposed for the compile-only unit test.
const SECCOMP_DENIED_SYSCALLS: [u32; 16] = [
    425, // io_uring_setup
    426, // io_uring_enter
    427, // io_uring_register
    323, // userfaultfd
    248, // add_key
    249, // request_key
    250, // keyctl
    321, // bpf
    298, // perf_event_open
    310, // process_vm_readv
    311, // process_vm_writev
    438, // pidfd_getfd
    304, // open_by_handle_at
    312, // kcmp
    203, // sched_setaffinity
    272, // unshare (denies CLONE_NEWUSER: no user namespaces inside the sandbox)
];

/// Serialized seccomp filter: a raw `struct sock_filter[]` array, exactly
/// what bwrap's `--seccomp FD` reads (bwrap appends its own sock_fprog
/// header, so the fd must hold only the instruction array, a multiple of 8).
/// Blocklist-first: everything not listed falls through to ALLOW.
fn seccomp_program() -> Vec<u8> {
    const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const BPF_JMP_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7FFF_0000;
    let mut program = Vec::with_capacity((SECCOMP_DENIED_SYSCALLS.len() + 6) * 8);
    let mut stmt = |code: u16, jt: u8, jf: u8, k: u32| {
        program.extend_from_slice(&code.to_ne_bytes());
        program.push(jt);
        program.push(jf);
        program.extend_from_slice(&k.to_ne_bytes());
    };
    // Gate on the audit arch first: the hardcoded numbers below are x86_64,
    // so any other ABI must be denied outright rather than silently unfiltered.
    stmt(BPF_LD_W_ABS, 0, 0, 4);
    stmt(BPF_JMP_JEQ_K, 1, 0, AUDIT_ARCH_X86_64);
    stmt(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32);
    stmt(BPF_LD_W_ABS, 0, 0, 0);
    // Each denied syscall jumps forward to the ERRNO tail; non-matches fall
    // through the remaining comparisons to the ALLOW instruction.
    for (i, nr) in SECCOMP_DENIED_SYSCALLS.iter().enumerate() {
        stmt(BPF_JMP_JEQ_K, (SECCOMP_DENIED_SYSCALLS.len() - i) as u8, 0, *nr);
    }
    stmt(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW);
    stmt(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | libc::EPERM as u32);
    program
}

/// Opens the memfd holding the seccomp filter, O_CLOEXEC cleared so the fd
/// survives bwrap's exec and is readable when it loads the program at
/// startup (bwrap then closes its own copy before exec'ing the payload).
fn seccomp_fd() -> Result<std::os::fd::OwnedFd> {
    let name = std::ffi::CString::new("vp-seccomp").expect("static name");
    let raw = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create seccomp filter");
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    file.write_all(&seccomp_program())
        .context("write seccomp filter")?;
    file.seek(std::io::SeekFrom::Start(0))
        .context("rewind seccomp filter")?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) } < 0 {
        return Err(std::io::Error::last_os_error()).context("clear CLOEXEC on seccomp fd");
    }
    Ok(file.into())
}
/// Retains the memfd backing `--seccomp FD` until the sandboxed process has
/// exec'd: bwrap loads the filter at startup and closes its own copy before
/// exec'ing the payload, so a given fd is only needed from `sandbox_command`
/// until the caller spawns the returned `Command` (both call sites, proc.rs
/// and node_daemon.rs, spawn synchronously right after building). The
/// registry is a bounded ring keeping the last `SECCOMP_MEMFD_RETENTION`
/// fds and dropping the oldest once full — a crash-looping restart therefore
/// cannot leak one memfd per launch until the process fd table (default soft
/// limit 1024) is exhausted, a daemon-wide DoS.
const SECCOMP_MEMFD_RETENTION: usize = 64;
static SECCOMP_MEMFDS: std::sync::LazyLock<parking_lot::Mutex<std::collections::VecDeque<std::os::fd::OwnedFd>>> =
    std::sync::LazyLock::new(|| {
        parking_lot::Mutex::new(std::collections::VecDeque::with_capacity(
            SECCOMP_MEMFD_RETENTION,
        ))
    });

/// Append the Data Lab bind to a bwrap command and hand the directory to the
/// workload UID. The source is the panel-owned `datalab_root/<uuid>` tree —
/// never a path under the workload-owned server root. Inside the sandbox the
/// mount lives at [`DATALAB_MOUNT_DIR`] on bwrap's private root, so a
/// workload cannot plant a symlink at the mount point, and the host
/// filesystem is otherwise invisible, so links inside the mount cannot
/// escape either. Extracted from the builder so the arg shape is
/// unit-testable without root privileges or bubblewrap.
fn append_datalab_bind(
    command: &mut Command,
    datalab_root: &Path,
    uuid: &str,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let src = datalab_root.join(uuid);
    fs::create_dir_all(&src).with_context(|| format!("cannot create {}", src.display()))?;
    fs::set_permissions(&src, fs::Permissions::from_mode(0o700))?;
    if unsafe { libc::geteuid() } == 0 {
        let cpath = std::ffi::CString::new(src.as_os_str().as_encoded_bytes())?;
        if unsafe { libc::chown(cpath.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error()).context("chown datalab directory");
        }
    }
    command
        .arg("--dir")
        .arg("/data")
        .arg("--dir")
        .arg(DATALAB_MOUNT_DIR)
        .arg("--bind")
        .arg(&src)
        .arg(DATALAB_MOUNT_DIR)
        .arg("--setenv")
        .arg(DATALAB_ENV_VAR)
        .arg(DATALAB_MOUNT_DIR);
    Ok(())
}

/// Build the sandboxed launch command without a Data Lab bind.
pub fn sandbox_command(
    config: &IsolationConfig,
    root: &Path,
    uuid: &str,
    startup: &str,
    limits: &Limits,
) -> Result<Command> {
    build_sandbox_command(config, root, uuid, startup, limits, None)
}

/// Like [`sandbox_command`], additionally binding the server's Data Lab
/// directory into the sandbox at [`DATALAB_MOUNT_DIR`] with workload-UID
/// ownership and setting [`DATALAB_ENV_VAR`] to the mount path.
///
/// `datalab_root` is the panel's Data Lab root (`cfg.paths.datalab_dir`); the
/// bound source is `datalab_root/<uuid>`, the directory
/// [`crate::services::databases::db_dir`] manages. The runtime launch path
/// (`services::proc`) adopts this once it can reach the panel config; the
/// blueprint install sandbox already passes it.
pub fn sandbox_command_with_datalab(
    config: &IsolationConfig,
    root: &Path,
    uuid: &str,
    startup: &str,
    limits: &Limits,
    datalab_root: &Path,
) -> Result<Command> {
    build_sandbox_command(config, root, uuid, startup, limits, Some(datalab_root))
}

fn build_sandbox_command(
    config: &IsolationConfig,
    root: &Path,
    uuid: &str,
    startup: &str,
    limits: &Limits,
    datalab: Option<&Path>,
) -> Result<Command> {
    if !config.enabled {
        let mut command = Command::new("sh");
        command.arg("-c").arg(startup).current_dir(root);
        return Ok(command);
    }
    let status = probe(config);
    if !status.secure && config.fail_closed {
        bail!("refusing unsandboxed launch: {}", status.message);
    }
    let (uid, gid) = prepare_root(root, uuid)?;
    validate_root(root, uuid)?;
    let mut command = if !status.cgroup_writable && status.systemd_scope {
        let mut command = Command::new("systemd-run");
        command
            .arg("--scope")
            .arg("--quiet")
            .arg("--collect")
            .arg("--unit")
            .arg(format!("vp-{}-{}", sanitize(uuid), std::process::id()))
            .arg("--property")
            .arg(format!(
                "MemoryMax={}",
                limits.memory_bytes.max(16 * 1_048_576)
            ))
            .arg("--property")
            .arg("MemorySwapMax=0")
            .arg("--property")
            .arg(format!("CPUQuota={}%", limits.cpu_percent.max(1)))
            .arg("--property")
            .arg(format!("TasksMax={}", limits.pids_max.max(32)))
            .arg("bwrap");
        command
    } else {
        Command::new("bwrap")
    };
    command
        .arg("--die-with-parent")
        .arg("--new-session")
        .arg("--unshare-pid")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--unshare-cgroup-try")
        .arg("--unshare-net")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp")
        // The payload uid is not root, so the default 0755 root-owned tmpfs
        // would leave its TMPDIR (exported as /tmp) unwritable.
        .arg("--chmod")
        .arg("1777")
        .arg("/tmp")
        .arg("--tmpfs")
        .arg("/run")
        .arg("--chmod")
        .arg("1777")
        .arg("/run")
        .arg("--dir")
        .arg("/home")
        .arg("--bind")
        .arg(root)
        .arg("/home/container")
        .arg("--chdir")
        .arg("/home/container")
        .arg("--setenv")
        .arg("HOME")
        .arg("/home/container")
        .arg("--setenv")
        .arg("PWD")
        .arg("/home/container")
        .arg("--setenv")
        .arg("TMPDIR")
        .arg("/tmp")
        .arg("--setenv")
        .arg("USER")
        .arg("container");
    if let Some(datalab) = datalab {
        append_datalab_bind(&mut command, datalab, uuid, uid, gid)?;
    }
    command
        .arg("--hostname")
        .arg(format!("vp-{}", &sanitize(uuid)[..12.min(uuid.len())]));
    for runtime in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
        let path = Path::new(runtime);
        if path.exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    for file in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/ssl",
        "/etc/ca-certificates",
    ] {
        let path = Path::new(file);
        if path.exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    // Deny a set of high-risk syscalls inside the sandbox via a compiled
    // cBPF blocklist (default-allow); bwrap installs it on itself before
    // exec, so the payload and every descendant inherit it. Callers hold
    // only `&Command` and may spawn much later, so the fd number baked into
    // the argument must stay valid until then: ownership moves into a
    // process-lifetime registry instead of leaking a fresh fd per launch.
    // A per-server `seccomp: off` escape hatch is a later iteration; there
    // is no config knob yet.
    let seccomp = seccomp_fd()?;
    command.arg("--seccomp").arg(seccomp.as_raw_fd().to_string());
    let mut fds = SECCOMP_MEMFDS.lock();
    if fds.len() == SECCOMP_MEMFD_RETENTION {
        fds.pop_front(); // oldest launch already exec'd; its fd is safe to close
    }
    fds.push_back(seccomp);
    command.arg("--setenv").arg("VOLTP_STARTUP").arg(startup)
        .arg("--").arg("/usr/bin/setpriv").arg(format!("--reuid={uid}")).arg(format!("--regid={gid}")).arg("--clear-groups")
        .arg("--no-new-privs").arg("--bounding-set=-all").arg("--inh-caps=-all").arg("--ambient-caps=-all")
        .arg("/bin/sh").arg("-c").arg("i=0; while [ $i -lt 200 ] && [ ! -e /run/voltp-network-ready ]; do i=$((i+1)); sleep 0.05; done; [ -e /run/voltp-network-ready ] || { echo 'voltp: network setup timed out' >&2; exit 1; }; exec /bin/sh -c \"$VOLTP_STARTUP\"");
    Ok(command)
}

pub fn cleanup_orphans(uuid: &str) {
    // Serialize the liveness check and the teardown under the same lock a
    // start's network setup holds (NetworkLease::configure): otherwise a
    // cleanup racing a concurrent start could see the not-yet-populated
    // cgroup and tear the veth/nft out from under the fresh workload.
    // SUBNET_LOCK only covers this process; the flock also serializes the
    // liveness check and teardown against a concurrent start from another
    // process. Cleanup is best-effort, so a lock failure is logged and the
    // in-process serialization still applies.
    let _net = match NetworkLock::acquire() {
        Ok(lock) => Some(lock),
        Err(error) => {
            eprintln!("cross-process network lock unavailable: {error:#}");
            None
        }
    };
    let _lock = SUBNET_LOCK.lock();
    // A non-empty server cgroup means the workload is still live (or being
    // started): leave its scope, veth and firewall state alone.
    let cgroup_procs = delegated_cgroup_root()
        .join("voltpanel")
        .join(sanitize(uuid))
        .join("cgroup.procs");
    let live = fs::read_to_string(cgroup_procs)
        .map(|procs| procs.split_whitespace().next().is_some())
        .unwrap_or(false);
    if live {
        return;
    }
    let pattern = format!("vp-{}-*.scope", sanitize(uuid));
    if let Ok(output) = Command::new("systemctl")
        .args(["list-units", "--all", "--plain", "--no-legend", &pattern])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(unit) = line.split_whitespace().next() {
                // Only stop scopes that are actually active; an inactive or
                // failed leftover needs no signal.
                let active = Command::new("systemctl")
                    .args(["is-active", "--quiet", unit])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if active {
                    let _ = Command::new("systemctl").args(["stop", unit]).status();
                }
            }
        }
    }
    let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
    let (host_if, table) = resource_names(&digest);
    let _ = Command::new("nft")
        .args(["delete", "table", "ip", &table])
        .status();
    let _ = Command::new("ip").args(["link", "del", &host_if]).status();
}

#[derive(Debug)]
pub struct NetworkLease {
    host_if: String,
    table: String,
    /// `/proc/sys/net/ipv4/ip_forward` value before this lease enabled it;
    /// restored on drop only once no sibling `vp*` veth remains on the host.
    ip_forward_prev: Option<String>,
}

/// Whether any sibling server's `vp*` veth remains on the host. Unknown
/// state (missing `ip`) conservatively reports live so forwarding is kept.
fn has_live_vp_veth() -> bool {
    let Ok(output) = Command::new("ip").args(["-o", "link", "show"]).output() else {
        return true;
    };
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|iface| iface.trim_end_matches(':').starts_with("vp"))
    })
}

impl NetworkLease {
    pub fn configure(pid: u32, uuid: &str, ports: &[u16], network_mbps: u64) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("network isolation requires root privileges");
        }
        let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
        let (host_if, table) = resource_names(&digest);
        let host_ns = fs::read_link("/proc/self/ns/net")?;
        let mut scrub = NetworkScrub::new(host_if.clone(), table.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let ns_pid = loop {
            if let Some(found) = find_private_net_pid(pid, &host_ns) {
                break found;
            }
            if std::time::Instant::now() >= deadline {
                bail!("sandbox did not enter a private network namespace");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // Allocate a collision-free /30 inside 10/8. The veth/nft names carry
        // 48 bits of identity, but a /30 subnet can encode only 16, so the
        // digest-derived index is checked against live vp* veths and walked
        // deterministically until free. The scan through the host-address add
        // is serialized so two concurrent starts cannot claim the same subnet.
        let (host_ip, guest_ip) = {
            // SUBNET_LOCK only covers this process; the flock also serializes
            // against a concurrent start from the panel or another agent.
            let _net = NetworkLock::acquire()?;
            let _lock = SUBNET_LOCK.lock();
            let (octet2, octet3) = allocate_subnet(&digest)?;
            let host_ip = format!("10.{octet2}.{octet3}.1");
            let guest_ip = format!("10.{octet2}.{octet3}.2");
            let _ = Command::new("ip").args(["link", "del", &host_if]).status();
            run(Command::new("ip").args([
                "link",
                "add",
                &host_if,
                "type",
                "veth",
                "peer",
                "name",
                "eth0",
                "netns",
                &ns_pid.to_string(),
            ]))?;
            run(Command::new("ip").args([
                "addr",
                "add",
                &format!("{host_ip}/30"),
                "dev",
                &host_if,
            ]))?;
            (host_ip, guest_ip)
        };
        run(Command::new("ip").args(["link", "set", &host_if, "up"]))?;
        run(Command::new("nsenter").args([
            "-t",
            &ns_pid.to_string(),
            "-n",
            "--",
            "ip",
            "addr",
            "add",
            &format!("{guest_ip}/30"),
            "dev",
            "eth0",
        ]))?;
        run(Command::new("nsenter").args([
            "-t",
            &ns_pid.to_string(),
            "-n",
            "--",
            "ip",
            "link",
            "set",
            "lo",
            "up",
        ]))?;
        run(Command::new("nsenter").args([
            "-t",
            &ns_pid.to_string(),
            "-n",
            "--",
            "ip",
            "link",
            "set",
            "eth0",
            "up",
        ]))?;
        run(Command::new("nsenter").args([
            "-t",
            &ns_pid.to_string(),
            "-n",
            "--",
            "ip",
            "route",
            "add",
            "default",
            "via",
            &host_ip,
        ]))?;
        let _ = Command::new("nft")
            .args(["delete", "table", "ip", &table])
            .status();
        let exposed = ports.iter().map(|p| format!("fib daddr type local tcp dport {p} dnat to {guest_ip}:{p};\n fib daddr type local udp dport {p} dnat to {guest_ip}:{p};\n")).collect::<String>();
        let allow = ports.iter().map(|p| format!("ip daddr {guest_ip} tcp dport {p} accept;\n ip daddr {guest_ip} udp dport {p} accept;\n")).collect::<String>();
        let script = format!("table ip {table} {{\n chain input {{ type filter hook input priority -20; policy accept;\n iifname \"{host_if}\" ip saddr != {guest_ip} drop\n iifname \"{host_if}\" ct state established,related accept\n iifname \"{host_if}\" drop\n }}\n chain forward {{ type filter hook forward priority -20; policy accept;\n iifname \"{host_if}\" ip saddr != {guest_ip} drop\n ct state established,related accept\n iifname \"{host_if}\" ip daddr 10.0.0.0/8 drop\n iifname \"{host_if}\" ip daddr 172.16.0.0/12 drop\n iifname \"{host_if}\" ip daddr 192.168.0.0/16 drop\n iifname \"{host_if}\" ip daddr 169.254.0.0/16 drop\n {allow} ip daddr {guest_ip} drop\n }}\n chain prerouting {{ type nat hook prerouting priority dstnat; policy accept;\n {exposed} }}\n chain postrouting {{ type nat hook postrouting priority srcnat; policy accept;\n ip saddr {guest_ip} masquerade\n }}\n}}\n");
        nft(&script)?;
        // Enable forwarding only after the firewall rules are live, so a
        // failed nft load cannot leave ip_forward on. Capture the previous
        // value first: the last lease to drop restores it.
        let ip_forward_prev = fs::read_to_string("/proc/sys/net/ipv4/ip_forward")
            .ok()
            .map(|value| value.trim().to_string());
        fs::write("/proc/sys/net/ipv4/ip_forward", "1").context("enable ip_forward")?;
        run(Command::new("nsenter").args(["-t",&ns_pid.to_string(),"-m","--","/bin/sh","-c","printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\n' > /run/resolv.conf; mount --bind /run/resolv.conf /etc/resolv.conf; touch /run/voltp-network-ready"]))?;
        apply_bandwidth_limit(&host_if, network_mbps);
        scrub.disarm();
        Ok(Self { host_if, table, ip_forward_prev })
    }
}

/// Apply a symmetric bandwidth cap (Mbps) to the host-side veth via `tc`.
/// 0 = unlimited (no-op). Best-effort: a missing `tc` binary or an unsupported
/// kernel logs a warning and leaves the workload unthrottled — the monitor's
/// 3-strike auto-kill remains the backstop.
fn apply_bandwidth_limit(host_if: &str, mbps: u64) {
    if mbps == 0 {
        return;
    }
    let rate = format!("{mbps}mbit");
    // ~10ms of line rate, floored at 64 kbit so `tc` accepts the burst.
    let burst = format!("{}kbit", (mbps * 100).max(64));
    let egress = Command::new("tc")
        .args(["qdisc", "replace", "dev", host_if, "root", "tbf", "rate", &rate, "burst", &burst, "latency", "50ms"])
        .status();
    let ingress = Command::new("tc")
        .args(["qdisc", "replace", "dev", host_if, "handle", "ffff:", "ingress"])
        .status();
    let police = Command::new("tc")
        .args(["filter", "replace", "dev", host_if, "parent", "ffff:", "protocol", "all", "prio", "1", "u32", "match", "u32", "0", "0", "police", "rate", &rate, "burst", &burst, "drop", "flowid", ":1"])
        .status();
    match (egress, ingress, police) {
        (Ok(a), Ok(b), Ok(c)) if a.success() && b.success() && c.success() => {}
        _ => eprintln!("network bandwidth throttle not applied on {host_if} ({mbps} Mbps): tc unavailable or unsupported"),
    }
}

impl Drop for NetworkLease {
    fn drop(&mut self) {
        remove_network(&self.host_if, &self.table);
        if let Some(prev) = &self.ip_forward_prev {
            // Ref-counted by liveness rather than a counter: only the last
            // lease to release restores the sysctl, so earlier drops never
            // break forwarding for a sibling that is still running.
            if !has_live_vp_veth() {
                if let Err(error) = fs::write("/proc/sys/net/ipv4/ip_forward", prev) {
                    eprintln!("restore ip_forward failed: {error:#}");
                }
            }
        }
    }
}
/// Deterministic per-server resource names for the nft table and veth pair:
/// 48 bits (12 hex chars) of the server's SHA-256, so two servers cannot
/// collide on the 32-bit suffix; `vp` + 12 hex fits IFNAMSIZ (16 bytes).
fn resource_names(digest: &[u8]) -> (String, String) {
    let hex = hex::encode(&digest[..6]);
    (format!("vp{hex}"), format!("vp{hex}"))
}
/// Last observed per-interface network usage, kept so a server whose veth
/// lease has been torn down (stopped server) keeps reporting the final
/// cumulative reading instead of dropping to 0 — the counters are monotonic
/// and every consumer (metrics deltas, bandwidth limits) relies on that.
/// Bounded to [`LAST_NET_USAGE_CAP`] entries, least-recently-written first,
/// so a fleet churning through server UUIDs cannot grow the map unbounded;
/// the `Instant` is refreshed on every live read, so running servers are
/// never evicted. Deleted servers are dropped from it once the entry ages
/// out (there is no safe explicit-cleanup hook: the lease teardown is also
/// the restart path that must keep last-known readings).
/// Per-interface last-known usage cache: host veth name -> (rx, tx, last
/// write time). Factored into an alias so the bounded [`LAST_NET_USAGE`]
/// static and its insert helper share one shape.
type NetUsageCache = std::collections::HashMap<String, (u64, u64, std::time::Instant)>;
const LAST_NET_USAGE_CAP: usize = 10_000;
static LAST_NET_USAGE: std::sync::LazyLock<parking_lot::Mutex<NetUsageCache>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(NetUsageCache::new()));

/// Insert `iface`'s cumulative `(rx, tx)` into `cache`, evicting the
/// least-recently-written entry once the cache would exceed `cap`. Pure so
/// the bound is unit-testable with a tiny cap; `network_usage_at` passes the
/// production [`LAST_NET_USAGE_CAP`]. The write time of a live read refreshes
/// the entry, so a server that keeps polling is never the eviction target —
/// the oldest entry is the longest-gone stopped server.
fn cache_net_usage(
    cache: &mut NetUsageCache,
    iface: String,
    usage: (u64, u64),
    cap: usize,
) {
    cache.insert(iface, (usage.0, usage.1, std::time::Instant::now()));
    if cache.len() <= cap {
        return;
    }
    let oldest = cache
        .iter()
        .min_by_key(|(_, (_, _, seen))| *seen)
        .map(|(key, _)| key.clone());
    if let Some(key) = oldest {
        cache.remove(&key);
    }
}

/// Raw cumulative byte counters of one host-side veth interface, read from
/// sysfs (`<root>/class/net/<iface>/statistics/{rx,tx}_bytes`). Returns `None`
/// when the interface is missing (lease gone) or unreadable. The `root`
/// parameter makes the reader pure and unit-testable with a temp dir; the
/// real callers pass `/sys`.
pub(crate) fn read_iface_stats(root: &Path, iface: &str) -> Option<(u64, u64)> {
    let stats = root.join("class/net").join(iface).join("statistics");
    let rx = fs::read_to_string(stats.join("rx_bytes")).ok()?.trim().parse().ok()?;
    let tx = fs::read_to_string(stats.join("tx_bytes")).ok()?.trim().parse().ok()?;
    Some((rx, tx))
}

/// Real per-server network traffic as cumulative counters, in server
/// perspective: `rx` is bytes the server received (ingress) and `tx` is bytes
/// the server sent (egress).
///
/// Direction mapping: the lease pairs `vp<hex>` (host netns) with `eth0`
/// (guest netns), so bytes the host interface *transmits* are bytes the guest
/// *receives* (server rx) and bytes the host interface *receives* are bytes
/// the guest *sent* (server tx) — the two are swapped on purpose. While the
/// lease exists the counters are read live from
/// `/sys/class/net/<host_if>/statistics`; on a missing interface the last
/// known reading is returned (see `LAST_NET_USAGE`), and a server never seen
/// reports 0.
pub fn network_usage(uuid: &str) -> (u64, u64) {
    network_usage_at(Path::new("/sys"), uuid)
}

fn network_usage_at(root: &Path, uuid: &str) -> (u64, u64) {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
    let (host_if, _table) = resource_names(&digest);
    let fresh = read_iface_stats(root, &host_if);
    let mut cache = LAST_NET_USAGE.lock();
    match fresh {
        Some((host_rx, host_tx)) => {
            // Host perspective -> server perspective (see doc comment above).
            let mapped = (host_tx, host_rx);
            cache_net_usage(&mut cache, host_if, mapped, LAST_NET_USAGE_CAP);
            mapped
        }
        None => cache.get(&host_if).map(|&(rx, tx, _)| (rx, tx)).unwrap_or((0, 0)),
    }
}
/// Serializes subnet allocation: the scan of live host interfaces and the
/// host-address add must be atomic with respect to other configures, or two
/// servers starting together could both claim the same free /30.
static SUBNET_LOCK: std::sync::LazyLock<parking_lot::Mutex<()>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

/// Cross-process serialization of the network lease critical sections
/// (`NetworkLease::configure`'s subnet scan+claim and `cleanup_orphans`'s
/// liveness check+teardown). `SUBNET_LOCK` only covers one process, but the
/// panel and a voltd agent share a host, so an flock is needed to make the
/// same sections atomic across both. Root locks a fixed /run lockfile;
/// unprivileged callers fall back to a temp-dir lock when /run is
/// unwritable. Acquired before `SUBNET_LOCK` at every site so lock ordering
/// (cross-process, then in-process) stays consistent and deadlock-free.
struct NetworkLock {
    // Held only to keep the flock alive; never read, released on drop.
    #[allow(dead_code)]
    file: std::fs::File,
}
impl NetworkLock {
    fn acquire() -> Result<Self> {
        let primary = Path::new("/run/voltp-network.lock");
        let (file, path) = match Self::open(primary) {
            Ok(file) => (file, primary.to_path_buf()),
            // /run is unwritable for an unprivileged panel process or inside
            // a read-only container; the temp-dir lock still serializes the
            // callers that can reach it. Only the /run lockfile coordinates
            // host-wide, which is exactly what network configuration needs
            // (configure bails without root).
            Err(_) => {
                let fallback = std::env::temp_dir().join("voltp-network.lock");
                let file = Self::open(&fallback)
                    .with_context(|| format!("open network lock {}", fallback.display()))?;
                (file, fallback)
            }
        };
        Self::wait_lock(file, &path)
    }

    fn open(path: &Path) -> std::io::Result<std::fs::File> {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
    }

    /// Blocking acquire with a bounded retry: poll LOCK_NB with exponential
    /// backoff for up to 10s, then fail instead of waiting forever. An open
    /// failure never falls back after this point — two processes picking
    /// different lock files would defeat the serialization.
    fn wait_lock(file: std::fs::File, path: &Path) -> Result<Self> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut backoff = std::time::Duration::from_millis(10);
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { file });
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock
                && error.kind() != std::io::ErrorKind::Interrupted
            {
                return Err(error).with_context(|| format!("flock network lock {}", path.display()));
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "timed out waiting for cross-process network lock {}",
                    path.display()
                );
            }
            std::thread::sleep(backoff);
            backoff = backoff
                .saturating_mul(2)
                .min(std::time::Duration::from_millis(200));
        }
    }
}

/// Fold the server's 48-bit digest to a 16-bit /30 subnet index inside 10/8.
/// 48 bits cannot fit in a /30, so uniqueness is enforced by
/// `allocate_subnet` against live leases; this only picks the deterministic
/// first choice for a given server.
fn subnet_index(digest: &[u8]) -> u16 {
    let v = u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], 0, 0,
    ]);
    ((v >> 16) ^ (v & 0xffff)) as u16
}

/// /30 subnet indices (10.a.b.0/30 -> (a << 8) | b) claimed by every host
/// interface address on 10/8, so a fresh allocation can avoid them. A veth
/// /30 inside the host's own LAN subnet would blackhole neighboring hosts'
/// traffic, so the whole prefix range is reserved, not just the address's
/// own /30; a /8 interface therefore marks all 65_536 indices.
fn live_subnets() -> std::collections::HashSet<u16> {
    let mut used = std::collections::HashSet::new();
    let Ok(output) = Command::new("ip")
        .args(["-o", "-4", "addr", "show"])
        .output()
    else {
        return used;
    };
    if !output.status.success() {
        return used;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 || fields.get(2) != Some(&"inet") {
            continue;
        }
        let Some((addr, prefix)) = fields[3].split_once('/') else {
            continue;
        };
        let mut octets = addr.split('.');
        let (Some(a), Some(b), Some(c), Some(_)) =
            (octets.next(), octets.next(), octets.next(), octets.next())
        else {
            continue;
        };
        let (Ok(a), Ok(b), Ok(c)) = (a.parse::<u16>(), b.parse::<u16>(), c.parse::<u16>()) else {
            continue;
        };
        let Ok(prefix) = prefix.parse::<u16>() else {
            continue;
        };
        if a != 10 {
            continue;
        }
        if prefix <= 8 {
            used.extend(0..=0xffff);
            continue;
        }
        // The /30 index is the 16-bit (second << 8 | third) octet pair; a
        // prefix pins the top `fixed` bits of that pair, leaving `free` low
        // bits free. Enumerating the anchor plus every low-bit offset marks
        // exactly the /30s the prefix covers (bounded by the 65_536 space).
        let fixed = (prefix - 8).min(16);
        let free = 16 - fixed;
        let base = (b << 8) | c;
        let anchor = base & (0xffffu16 << free);
        for offset in 0..(1usize << free) {
            used.insert(anchor | offset as u16);
        }
    }
    used
}

/// Pick the server's /30 subnet: start from the digest-derived index and walk
/// deterministically past any index a live host interface already claims.
/// The caller holds `SUBNET_LOCK` from the scan until the host address is
/// applied.
fn allocate_subnet(digest: &[u8]) -> Result<(u8, u8)> {
    let used = live_subnets();
    pick_subnet(subnet_index(digest), &used)
}

/// Deterministic walk over the /30 index space: the first index at or after
/// `start` (wrapping) that is not in `used`. Pure so the collision policy is
/// unit-testable without host network state.
fn pick_subnet(start: u16, used: &std::collections::HashSet<u16>) -> Result<(u8, u8)> {
    let mut index = start;
    for _ in 0..=0xffff {
        if !used.contains(&index) {
            return Ok(((index >> 8) as u8, (index & 0xff) as u8));
        }
        index = index.wrapping_add(1);
    }
    bail!("no free /30 subnet available for server network")
}

/// Caps for the bounded descendant walk: a crash-looping server spawning a
/// huge tree must not let cgroup/network attach spin or buffer unbounded work.
const DESC_MAX_DEPTH: usize = 12;
const DESC_MAX_NODES: usize = 4096;

/// /proc/<pid>/stat field 22 (starttime, in clock ticks since boot), parsed
/// after the closing `)` of comm, which may itself contain spaces or `)`.
/// This is the identity token for pid-reuse checks: a pid recycled by the
/// kernel carries a different starttime than the process that was walked.
fn process_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// Bounded DFS of the live process subtree rooted at `root` (root included),
/// read through `/proc/<pid>/task/<pid>/children`. Returns at most
/// `DESC_MAX_NODES` (pid, starttime) pairs and never descends deeper than
/// `DESC_MAX_DEPTH`. The starttime snapshot lets callers re-verify a pid
/// before acting on it, so a recycled pid is never mistaken for the server.
fn descendant_subtree(root: u32) -> Vec<(u32, u64)> {
    let mut stack = vec![(root, 0usize)];
    let mut out = Vec::new();
    let mut visited = 0usize;
    while let Some((pid, depth)) = stack.pop() {
        if visited >= DESC_MAX_NODES {
            break;
        }
        visited += 1;
        out.push((pid, process_starttime(pid).unwrap_or(0)));
        if depth < DESC_MAX_DEPTH {
            if let Ok(children) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
                // Push in reverse so the first-listed child is visited first.
                for child in children.split_whitespace().rev() {
                    if let Ok(child) = child.parse::<u32>() {
                        stack.push((child, depth + 1));
                    }
                }
            }
        }
    }
    out
}

fn find_private_net_pid(root: u32, host_ns: &Path) -> Option<u32> {
    descendant_subtree(root)
        .into_iter()
        .find(|(pid, start)| {
            // Re-verify identity before trusting the pid: a recycled pid's
            // netns read would return the wrong process's namespace.
            process_starttime(*pid) == Some(*start)
                && fs::read_link(format!("/proc/{pid}/ns/net"))
                    .ok()
                    .as_deref()
                    != Some(host_ns)
        })
        .map(|(pid, _)| pid)
}

/// Rolls back a partially-created server cgroup when limit writes fail, so an
/// error path cannot leak an empty (unlimited) cgroup between retries.
struct CgroupScratch {
    path: PathBuf,
    armed: bool,
}
impl CgroupScratch {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for CgroupScratch {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

/// Rolls back the veth pair and nft table when `NetworkLease::configure` fails
/// partway, so retries do not leak interfaces or firewall state.
struct NetworkScrub {
    host_if: String,
    table: String,
    armed: bool,
}
impl NetworkScrub {
    fn new(host_if: String, table: String) -> Self {
        Self {
            host_if,
            table,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for NetworkScrub {
    fn drop(&mut self) {
        if self.armed {
            remove_network(&self.host_if, &self.table);
        }
    }
}

fn remove_network(host_if: &str, table: &str) {
    let _ = Command::new("nft")
        .args(["delete", "table", "ip", table])
        .status();
    let _ = Command::new("ip").args(["link", "del", host_if]).status();
}

fn run(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "network setup command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn nft(script: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .context("nft stdin")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "nft rules failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn sandbox_hides_host_and_peer_paths() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server-a");
        prepare_root(&root, "sandbox-test-a").unwrap();
        own_tree(&root, "sandbox-test-a").unwrap();
        let limits = Limits {
            memory_bytes: 64 * 1_048_576,
            cpu_percent: 25,
            pids_max: 32,
        };
        let mut command = sandbox_command(&IsolationConfig::default(), &root, "sandbox-test-a", "test ! -e /etc/shadow && test ! -e /root && test ! -e /sys/fs/cgroup && test ! -e /home/peer && echo ISOLATED", &limits).unwrap();
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = command.spawn().unwrap();
        let lease = NetworkLease::configure(child.id(), "sandbox-test-a", &[], 0).unwrap();
        let output = child.wait_with_output().unwrap();
        drop(lease);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ISOLATED");
    }
    use super::*;
    #[test]
    fn identity_is_stable_and_separated() {
        assert_eq!(server_identity("a"), server_identity("a"));
        assert_ne!(server_identity("a"), server_identity("b"));
    }
    #[test]
    fn probe_reports_host_support() {
        let p = probe(&IsolationConfig::default());
        assert!(p.bubblewrap);
        assert!(p.cgroup_v2);
    }
    #[test]
    fn resource_names_expand_identity() {
        let a = <sha2::Sha256 as sha2::Digest>::digest(b"server-a");
        let b = <sha2::Sha256 as sha2::Digest>::digest(b"server-b");
        let (ifa, ta) = resource_names(&a);
        let (ifb, tb) = resource_names(&b);
        assert_eq!(ifa, ta);
        assert_eq!(ifb, tb);
        assert_ne!(ifa, ifb);
        assert_eq!(ifa.len(), 14, "vp + 12 hex must fit IFNAMSIZ");
        assert!(ifa.starts_with("vp"));
        assert!(ifa[2..].chars().all(|c| c.is_ascii_hexdigit()));
    }
    #[test]
    fn read_iface_stats_parses_sysfs_counters() {
        let temp = tempfile::tempdir().unwrap();
        let stats = temp.path().join("class/net/vpabcd/statistics");
        fs::create_dir_all(&stats).unwrap();
        fs::write(stats.join("rx_bytes"), "4096\n").unwrap();
        fs::write(stats.join("tx_bytes"), "8192\n").unwrap();
        assert_eq!(read_iface_stats(temp.path(), "vpabcd"), Some((4096, 8192)));
        // Missing interface (no lease) -> None.
        assert_eq!(read_iface_stats(temp.path(), "vpdead"), None);
    }
    #[test]
    fn network_usage_swaps_directions_and_caches_last_known() {
        let temp = tempfile::tempdir().unwrap();
        let uuid = "net-test-uuid-0001";
        let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
        let (host_if, _) = resource_names(&digest);
        // A server never seen reports 0.
        assert_eq!(network_usage_at(temp.path(), uuid), (0, 0));
        let stats = temp.path().join("class/net").join(&host_if).join("statistics");
        fs::create_dir_all(&stats).unwrap();
        fs::write(stats.join("rx_bytes"), "500\n").unwrap();
        fs::write(stats.join("tx_bytes"), "1000\n").unwrap();
        // Direction mapping: host tx = what the guest receives (server rx,
        // ingress); host rx = what the guest sent (server tx, egress).
        assert_eq!(network_usage_at(temp.path(), uuid), (1000, 500));
        // Lease torn down (interface gone): last known reading is kept so the
        // cumulative counters stay monotonic across a restart.
        fs::remove_dir_all(temp.path().join("class/net").join(&host_if)).unwrap();
        assert_eq!(network_usage_at(temp.path(), uuid), (1000, 500));
        // Cache hygiene: keep this test idempotent for re-runs.
        LAST_NET_USAGE.lock().remove(&host_if);
    }
    #[test]
    fn net_usage_cache_is_bounded_oldest_first() {
        let mut cache = std::collections::HashMap::new();
        let cap = 1_000;
        // Churn 1100 distinct veth names (unique interfaces) through the
        // cache: the bound must hold and eviction must hit the oldest entries.
        for i in 0..1_100u64 {
            cache_net_usage(&mut cache, format!("vp{i:012x}"), (i, i * 2), cap);
        }
        assert_eq!(cache.len(), cap);
        // The 1000 most-recently-written entries survive with their readings.
        assert_eq!(cache.get("vp0000000003e8").map(|&(rx, tx, _)| (rx, tx)), Some((1_000, 2_000)));
        assert!(cache.contains_key("vp00000000044b")); // i = 1099
        // The oldest 100 were evicted.
        assert!(!cache.contains_key("vp000000000000")); // i = 0
        assert!(!cache.contains_key("vp000000000063")); // i = 99
        // Re-writing an entry refreshes it, so a live polled server survives
        // and pushes the current oldest (i = 100) out instead.
        cache_net_usage(&mut cache, "vp000000000000".to_string(), (7, 8), cap);
        assert_eq!(cache.len(), cap);
        assert!(cache.contains_key("vp000000000000"));
        assert!(!cache.contains_key("vp000000000064")); // i = 100, now oldest
        // Evicted entries fall back to 0 on a later miss (server never seen
        // again), surviving ones keep their last-known reading.
        assert_eq!(cache.get("vp0000000003e8").map(|&(rx, tx, _)| (rx, tx)), Some((1_000, 2_000)));
    }
    #[test]
    fn sanitize_keeps_ascii_identifiers() {
        assert_eq!(
            sanitize("9f2a1c1e-2f0a-4c8e-9a4b-1234567890ab"),
            "9f2a1c1e-2f0a-4c8e-9a4b-1234567890ab"
        );
        assert_eq!(sanitize("a b/c*d"), "a_b_c_d");
    }
    #[test]
    fn descendant_subtree_visits_self_bounded() {
        let nodes = descendant_subtree(std::process::id());
        assert!(nodes.iter().any(|(pid, start)| {
            *pid == std::process::id() && *start == process_starttime(std::process::id()).unwrap()
        }));
        assert!(nodes.len() <= DESC_MAX_NODES);
    }
    #[test]
    fn seccomp_program_blocklists_denied_syscalls() {
        let bytes = seccomp_program();
        // bwrap requires the raw filter to be a whole multiple of 8 bytes.
        assert_eq!(bytes.len() % 8, 0);
        assert!(!bytes.is_empty());
        let insns = bytes.chunks_exact(8).collect::<Vec<_>>();
        let code = |insn: &[u8]| {
            (
                u16::from_ne_bytes([insn[0], insn[1]]),
                insn[2],
                insn[3],
                u32::from_ne_bytes([insn[4], insn[5], insn[6], insn[7]]),
            )
        };
        // Arch gate: load arch, compare against x86_64, deny otherwise.
        assert_eq!(code(insns[0]).0, 0x20); // BPF_LD|BPF_W|BPF_ABS
        let arch = code(insns[1]);
        assert_eq!(arch.0, 0x15); // BPF_JMP|BPF_JEQ|BPF_K
        assert_eq!(arch.3, 0xC000_003E); // AUDIT_ARCH_X86_64
        let arch_deny = code(insns[2]);
        assert_eq!(arch_deny.0, 0x06); // BPF_RET
        assert_eq!(arch_deny.3, 0x0005_0000 | libc::EPERM as u32); // ERRNO|EPERM
        // Deny block: one jeq per denied syscall, then ALLOW, then the ERRNO
        // tail the jeqs jump to.
        let deny = insns
            .iter()
            .skip(4)
            .take(SECCOMP_DENIED_SYSCALLS.len())
            .collect::<Vec<_>>();
        for (insn, nr) in deny.iter().zip(SECCOMP_DENIED_SYSCALLS) {
            let (c, jt, _, k) = code(insn);
            assert_eq!(c, 0x15, "deny rule must be a jeq");
            assert!(jt >= 1, "deny rule must jump to the ERRNO tail");
            assert_eq!(k, nr, "denied syscall number must match");
        }
        let allow = code(insns[4 + SECCOMP_DENIED_SYSCALLS.len()]);
        assert_eq!(allow.0, 0x06);
        assert_eq!(allow.3, 0x7FFF_0000); // SECCOMP_RET_ALLOW
        let deny_tail = code(insns[5 + SECCOMP_DENIED_SYSCALLS.len()]);
        assert_eq!(deny_tail.0, 0x06);
        assert_eq!(
            deny_tail.3,
            0x0005_0000 | libc::EPERM as u32,
            "tail must be SECCOMP_RET_ERRNO with EPERM data"
        );
        assert_eq!(
            insns.len(),
            SECCOMP_DENIED_SYSCALLS.len() + 6,
            "arch gate + nr load + deny list + ALLOW + ERRNO tail"
        );
    }
    #[test]
    fn identity_stays_in_pool() {
        let (uid, gid) = server_identity("pool-check");
        assert_eq!(uid, gid);
        assert!((MIN_SERVER_UID..MIN_SERVER_UID + UID_RANGE).contains(&uid));
    }
    #[test]
    fn pick_uid_skips_used_wraps_and_exhausts() {
        use std::collections::HashSet;
        let mut used = HashSet::new();
        used.insert(MIN_SERVER_UID);
        assert_eq!(pick_uid(MIN_SERVER_UID, &used), Some(MIN_SERVER_UID + 1));
        // Everything from the start to the end of the pool is used: the walk
        // wraps around to the first free id at the bottom of the pool.
        let tail = (MIN_SERVER_UID + 50..MIN_SERVER_UID + UID_RANGE).collect::<HashSet<_>>();
        assert_eq!(pick_uid(MIN_SERVER_UID + 50, &tail), Some(MIN_SERVER_UID));
        let full = (MIN_SERVER_UID..MIN_SERVER_UID + UID_RANGE).collect::<HashSet<_>>();
        assert_eq!(pick_uid(MIN_SERVER_UID, &full), None);
    }
    #[test]
    fn pick_subnet_skips_used_wraps_and_exhausts() {
        use std::collections::HashSet;
        let mut used = HashSet::new();
        used.insert(100);
        assert_eq!(pick_subnet(100, &used).unwrap(), (0, 101));
        let tail = (0xf000..=0xffff).collect::<HashSet<_>>();
        assert_eq!(pick_subnet(0xf000, &tail).unwrap(), (0, 0));
        let full = (0..=0xffff).collect::<HashSet<_>>();
        assert!(pick_subnet(0, &full).is_err());
    }
    #[test]
    fn subnet_index_is_stable() {
        use sha2::Digest;
        let a = sha2::Sha256::digest(b"server-a");
        let b = sha2::Sha256::digest(b"server-b");
        assert_eq!(subnet_index(&a), subnet_index(&a));
        assert_ne!(subnet_index(&a), subnet_index(&b));
    }
    #[test]
    fn swap_limit_skipped_when_controller_absent() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!write_swap_limit(temp.path()).unwrap());
        assert!(!temp.path().join("memory.swap.max").exists());
        let p = temp.path().join("memory.swap.max");
        fs::write(&p, "max").unwrap();
        assert!(write_swap_limit(temp.path()).unwrap());
        assert_eq!(fs::read_to_string(&p).unwrap().trim(), "0");
    }
    #[test]
    fn missing_controllers_detected() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(missing_required_controllers(temp.path()).len(), 3);
        for f in ["memory.max", "cpu.max", "pids.max"] {
            fs::write(temp.path().join(f), "x").unwrap();
        }
        assert!(missing_required_controllers(temp.path()).is_empty());
        fs::remove_file(temp.path().join("pids.max")).unwrap();
        assert_eq!(missing_required_controllers(temp.path()), vec!["pids"]);
    }
    #[test]
    fn verify_limit_accepts_page_rounded_values() {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path().join("memory.max");
        // The kernel floors a non-page-aligned write to the page boundary:
        // writing 16777217 yields 16777216, which must verify.
        fs::write(&p, "16777216").unwrap();
        verify_limit(&p, "16777217").unwrap(); // floored to a page
        verify_limit(&p, "16777216").unwrap();
        verify_limit(&p, "16777220").unwrap(); // same page, same enforced value
        assert!(verify_limit(&p, "16777215").is_err()); // floor is 16777212
        assert!(verify_limit(&p, "16781312").is_err()); // different page
    }
    #[test]
    fn scratch_guard_removes_partial_cgroup() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("server");
        fs::create_dir_all(&dir).unwrap();
        drop(CgroupScratch::new(dir.clone()));
        assert!(
            !dir.exists(),
            "armed scratch must remove the partial cgroup"
        );
        fs::create_dir_all(&dir).unwrap();
        let mut scratch = CgroupScratch::new(dir.clone());
        scratch.disarm();
        drop(scratch);
        assert!(dir.exists(), "disarmed scratch must keep the cgroup");
    }
    #[test]
    fn network_scrub_guard_drop_is_safe() {
        // remove_network shells out to nft/ip; nonexistent names are no-ops
        // and must never panic even when nft/ip are absent.
        drop(NetworkScrub::new("vp-nope".into(), "vp-nope".into()));
        let mut disarmed = NetworkScrub::new("vp-nope".into(), "vp-nope".into());
        disarmed.disarm();
        drop(disarmed);
    }
    #[test]
    fn attach_fails_closed_when_no_payload_descendant() {
        // A plain directory stands in for the cgroup: the membership logic is
        // identical, but our own pid has no forked children, so attach must
        // bail instead of accepting an empty cgroup.
        let temp = tempfile::tempdir().unwrap();
        let cgroup = Cgroup {
            path: temp.path().to_path_buf(),
        };
        let err = cgroup.attach(std::process::id()).unwrap_err();
        assert!(
            format!("{err:#}").contains("no descendant"),
            "unexpected error: {err:#}"
        );
    }
    #[test]
    fn prepare_root_allocates_distinct_stable_uids() {
        let temp = tempfile::tempdir().unwrap();
        let (uid_a1, gid_a1) = prepare_root(&temp.path().join("a"), "uid-test-a").unwrap();
        let (uid_a2, _) = prepare_root(&temp.path().join("a"), "uid-test-a").unwrap();
        let (uid_b, _) = prepare_root(&temp.path().join("b"), "uid-test-b").unwrap();
        assert_eq!(uid_a1, gid_a1);
        assert_eq!(uid_a1, uid_a2, "ownership must be stable across calls");
        assert_ne!(uid_a1, uid_b);
        for uid in [uid_a1, uid_a2, uid_b] {
            assert!((MIN_SERVER_UID..MIN_SERVER_UID + UID_RANGE).contains(&uid));
        }
    }
    #[test]
    fn prepare_root_avoids_sibling_pool_uid() {
        if unsafe { libc::geteuid() } != 0 {
            return; // collision probing depends on chown into the UID pool
        }
        let temp = tempfile::tempdir().unwrap();
        let (candidate, _) = server_identity("collision-probe");
        let sibling = temp.path().join("existing");
        fs::create_dir(&sibling).unwrap();
        let cpath = std::ffi::CString::new(sibling.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chown(cpath.as_ptr(), candidate, candidate) },
            0
        );
        let (uid, _) = prepare_root(&temp.path().join("new"), "collision-probe").unwrap();
        assert_ne!(
            uid, candidate,
            "must skip a uid already owned by a sibling root"
        );
    }
    #[test]
    fn prepare_root_serializes_concurrent_allocation() {
        if unsafe { libc::geteuid() } != 0 {
            return; // distinct uids only materialize when chown applies
        }
        let temp = tempfile::tempdir().unwrap();
        let mut handles = Vec::new();
        for i in 0..16 {
            let parent = temp.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                prepare_root(&parent.join(format!("s{i}")), "same-uuid")
                    .unwrap()
                    .0
            }));
        }
        let mut uids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        uids.sort_unstable();
        uids.dedup();
        assert_eq!(uids.len(), 16, "concurrent prepares must not collide");
    }
    #[test]
    fn cgroup_roundtrip_applies_limits_attach_kill_remove() {
        // Needs a writable cgroup v2 mount exposing cpu/memory/pids.
        let (writable, _) = probe_cgroup_write(Path::new("/sys/fs/cgroup/vp-itest-probe"));
        if unsafe { libc::geteuid() } != 0 || !writable {
            eprintln!("skipping: no writable cgroup v2 delegation in this environment");
            return;
        }
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // cgroupfs dirs only support rmdir on an empty cgroup.
                let _ = fs::remove_dir(&self.0);
            }
        }
        let root = Path::new("/sys/fs/cgroup").join(format!("vp-itest-{}", std::process::id()));
        let _cleanup = Cleanup(root.clone());
        let config = IsolationConfig {
            cgroup_root: root.clone(),
            ..IsolationConfig::default()
        };
        let limits = Limits {
            memory_bytes: 64 * 1_048_576,
            cpu_percent: 25,
            pids_max: 32,
        };
        let cgroup = Cgroup::create(&config, "roundtrip", &limits).unwrap();
        // Limits were written and verified: read them back.
        assert_eq!(read_u64(cgroup.path().join("memory.max")), 64 * 1_048_576);
        assert_eq!(read_u64(cgroup.path().join("pids.max")), 32);
        assert_eq!(
            fs::read_to_string(cgroup.path().join("cpu.max"))
                .unwrap()
                .trim(),
            "25000 100000"
        );
        assert_eq!(
            fs::read_to_string(cgroup.path().join("memory.swap.max"))
                .unwrap()
                .trim(),
            "0"
        );
        // Attach a real payload tree and verify every member landed inside.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60 & sleep 60")
            .spawn()
            .unwrap();
        cgroup.attach(child.id()).unwrap();
        let members = fs::read_to_string(cgroup.path().join("cgroup.procs")).unwrap();
        let count = members.split_whitespace().count();
        assert!(
            count >= 2,
            "payload subtree must be inside the cgroup, got {count}"
        );
        cgroup.kill_all().unwrap();
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let left = fs::read_to_string(cgroup.path().join("cgroup.procs")).unwrap_or_default();
            if left.trim().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cgroup not drained after kill_all: {left}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let path = cgroup.path().to_path_buf();
        cgroup.remove();
        assert!(!path.exists(), "removed cgroup must be gone");
    }

    /// The Data Lab bind must mount the panel-owned `datalab_root/<uuid>`
    /// tree read-write at `/data/.voltp/databases` and advertise it through
    /// `VOLTP_DATALAB_DIR` — never a path under the workload-owned root.
    #[test]
    fn datalab_bind_uses_panel_root_and_private_mount_path() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let mut command = Command::new("bwrap");
        append_datalab_bind(&mut command, &datalab_root, "srv-uuid", 1234, 1234).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // bwrap spells the bind as `--bind <src> <dest>`; check the whole
        // triple so the source (panel-owned datalab root + uuid) and the
        // destination (the private mount path) are both verified.
        let src = datalab_root.join("srv-uuid").to_string_lossy().to_string();
        let triples: Vec<(String, String, String)> = args
            .windows(3)
            .map(|w| (w[0].clone(), w[1].clone(), w[2].clone()))
            .collect();
        assert!(triples.contains(&("--bind".into(), src.clone(), DATALAB_MOUNT_DIR.into())));
        // The mount is advertised to the workload through the env var.
        let pairs: Vec<(String, String)> = args
            .windows(2)
            .map(|w| (w[0].clone(), w[1].clone()))
            .collect();
        assert!(pairs.contains(&("--setenv".into(), DATALAB_ENV_VAR.into())));
        // The source directory is created (chowned to the workload UID only
        // as root, which this test usually is not).
        assert!(datalab_root.join("srv-uuid").is_dir());
    }

    /// End-to-end: inside a real sandbox the Data Lab bind must be writable
    /// by the workload and land files in the panel-owned `datalab_root/<uuid>`
    /// directory, while the host filesystem stays invisible.
    #[test]
    fn datalab_bind_mounts_panel_directory_into_sandbox() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("server-a");
        let datalab_root = temp.path().join("datalab");
        prepare_root(&root, "datalab-mount-test").unwrap();
        own_tree(&root, "datalab-mount-test").unwrap();
        let limits = Limits {
            memory_bytes: 64 * 1_048_576,
            cpu_percent: 25,
            pids_max: 32,
        };
        let startup = format!(
            "touch {DATALAB_MOUNT_DIR}/probe && test -w {DATALAB_MOUNT_DIR} && echo MOUNT_OK"
        );
        let mut command = sandbox_command_with_datalab(
            &IsolationConfig::default(),
            &root,
            "datalab-mount-test",
            &startup,
            &limits,
            &datalab_root,
        )
        .unwrap();
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = command.spawn().unwrap();
        let lease = NetworkLease::configure(child.id(), "datalab-mount-test", &[], 0).unwrap();
        let output = child.wait_with_output().unwrap();
        drop(lease);
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "sandboxed payload failed: {out} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(out.contains("MOUNT_OK"), "payload output: {out}");
        // The file created through the mount landed in the panel-owned dir.
        assert!(datalab_root.join("datalab-mount-test").join("probe").exists());
    }
}