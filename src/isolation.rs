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
use std::io::Write;
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
    let cgroup_writable = probe_cgroup_write(&config.cgroup_root);
    let systemd_scope = which("systemd-run") && unsafe { libc::geteuid() } == 0;
    let user_namespace = false;
    let capabilities_dropped = true;
    let no_new_privs = true;
    let secure = !config.enabled
        || (bubblewrap
            && cgroup_v2
            && (cgroup_writable || systemd_scope)
            && capabilities_dropped
            && no_new_privs);
    let message = if secure {
        "mount/PID/IPC/UTS/network namespaces, zero capabilities, no-new-privs and cgroup limits available".into()
    } else {
        format!(
            "missing: {}{}{}",
            if !bubblewrap { "bubblewrap " } else { "" },
            if !cgroup_v2 { "cgroup-v2 " } else { "" },
            if !cgroup_writable && !systemd_scope {
                "cgroup-delegation "
            } else {
                ""
            }
        )
    };
    IsolationStatus {
        bubblewrap,
        cgroup_v2,
        cgroup_writable,
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
fn probe_cgroup_write(root: &Path) -> bool {
    let parent = root.parent().unwrap_or(Path::new("/sys/fs/cgroup"));
    let probe = parent.join(format!("vp-probe-{}", std::process::id()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let ok = probe.join("memory.max").exists()
                && probe.join("cpu.max").exists()
                && fs::write(probe.join("memory.max"), "max").is_ok();
            let _ = fs::remove_dir(&probe);
            ok
        }
        Err(_) => false,
    }
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(binary).is_file()))
        .unwrap_or(false)
}

pub fn server_identity(uuid: &str) -> (u32, u32) {
    let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
    let raw = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    let id = MIN_SERVER_UID + raw % UID_RANGE;
    (id, id)
}

pub fn prepare_root(root: &Path, uuid: &str) -> Result<(u32, u32)> {
    let _guard = IDENTITY_LOCK.lock();
    fs::create_dir_all(root)?;
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
        let (mut candidate, _) = server_identity(uuid);
        let start = candidate;
        while used.contains(&candidate) {
            candidate = MIN_SERVER_UID + (candidate - MIN_SERVER_UID + 1) % UID_RANGE;
            if candidate == start {
                bail!("server UID pool exhausted")
            }
        }
        candidate
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
        enable_here(&config.cgroup_root)?;
        fs::create_dir_all(&path).context("create server cgroup")?;
        write(
            &path.join("memory.max"),
            &limits.memory_bytes.max(16 * 1_048_576).to_string(),
        )?;
        write(&path.join("memory.swap.max"), "0")?;
        write(&path.join("memory.oom.group"), "1")?;
        write(&path.join("pids.max"), &limits.pids_max.max(32).to_string())?;
        let quota = limits.cpu_percent.max(1).saturating_mul(1000);
        write(&path.join("cpu.max"), &format!("{} 100000", quota))?;
        Ok(Self { path })
    }

    pub fn attach(&self, pid: u32) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        write(&self.path.join("cgroup.procs"), &pid.to_string()).context("attach process to cgroup")
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
        }
        Ok(())
    }

    pub fn remove(self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _ = self.kill_all();
        let _ = fs::remove_dir(&self.path);
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
    let available = fs::read_to_string(parent.join("cgroup.controllers")).unwrap_or_default();
    let wanted = ["cpu", "memory", "pids", "io"]
        .into_iter()
        .filter(|v| available.split_whitespace().any(|a| a == *v))
        .map(|v| format!("+{v}"))
        .collect::<Vec<_>>()
        .join(" ");
    if !wanted.is_empty() {
        let _ = write(&parent.join("cgroup.subtree_control"), &wanted);
    }
    Ok(())
}

fn enable_here(root: &Path) -> Result<()> {
    let available = fs::read_to_string(root.join("cgroup.controllers")).unwrap_or_default();
    let wanted = ["cpu", "memory", "pids", "io"]
        .into_iter()
        .filter(|v| available.split_whitespace().any(|a| a == *v))
        .map(|v| format!("+{v}"))
        .collect::<Vec<_>>()
        .join(" ");
    if !wanted.is_empty() {
        write(&root.join("cgroup.subtree_control"), &wanted)?;
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

pub fn sandbox_command(
    config: &IsolationConfig,
    root: &Path,
    uuid: &str,
    startup: &str,
    limits: &Limits,
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
        .arg("--tmpfs")
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
        .arg("container")
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
    command.arg("--setenv").arg("VOLTP_STARTUP").arg(startup)
        .arg("--").arg("/usr/bin/setpriv").arg(format!("--reuid={uid}")).arg(format!("--regid={gid}")).arg("--clear-groups")
        .arg("--no-new-privs").arg("--bounding-set=-all").arg("--inh-caps=-all").arg("--ambient-caps=-all")
        .arg("/bin/sh").arg("-c").arg("while [ ! -e /run/voltp-network-ready ]; do sleep 0.05; done; exec /bin/sh -c \"$VOLTP_STARTUP\"");
    Ok(command)
}

pub fn cleanup_orphans(uuid: &str) {
    let pattern = format!("vp-{}-*.scope", sanitize(uuid));
    if let Ok(output) = Command::new("systemctl")
        .args(["list-units", "--all", "--plain", "--no-legend", &pattern])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(unit) = line.split_whitespace().next() {
                let _ = Command::new("systemctl").args(["stop", unit]).status();
            }
        }
    }
    let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
    let suffix = hex::encode(&digest[..4]);
    let host_if = format!("vp{}", &suffix[..8]);
    let table = format!("vp{}", &suffix[..8]);
    let _ = Command::new("nft")
        .args(["delete", "table", "ip", &table])
        .status();
    let _ = Command::new("ip").args(["link", "del", &host_if]).status();
}

#[derive(Debug)]
pub struct NetworkLease {
    host_if: String,
    table: String,
}

impl NetworkLease {
    pub fn configure(pid: u32, uuid: &str, ports: &[u16]) -> Result<Self> {
        if unsafe { libc::geteuid() } != 0 {
            bail!("network isolation requires root privileges");
        }
        let digest = <sha2::Sha256 as sha2::Digest>::digest(uuid.as_bytes());
        let suffix = hex::encode(&digest[..4]);
        let host_if = format!("vp{}", &suffix[..8]);
        let table = format!("vp{}", &suffix[..8]);
        let octet2 = 64 + digest[0] % 120;
        let octet3 = digest[1];
        let host_ip = format!("10.{octet2}.{octet3}.1");
        let guest_ip = format!("10.{octet2}.{octet3}.2");
        let host_ns = fs::read_link("/proc/self/ns/net")?;
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
        run(Command::new("ip").args(["addr", "add", &format!("{host_ip}/30"), "dev", &host_if]))?;
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
        let _ = fs::write("/proc/sys/net/ipv4/ip_forward", "1");
        let _ = Command::new("nft")
            .args(["delete", "table", "ip", &table])
            .status();
        let exposed = ports.iter().map(|p| format!("tcp dport {p} dnat to {guest_ip}:{p};\n udp dport {p} dnat to {guest_ip}:{p};\n")).collect::<String>();
        let allow = ports.iter().map(|p| format!("ip daddr {guest_ip} tcp dport {p} accept;\n ip daddr {guest_ip} udp dport {p} accept;\n")).collect::<String>();
        let script = format!("table ip {table} {{\n chain input {{ type filter hook input priority -20; policy accept;\n iifname \"{host_if}\" ct state established,related accept\n iifname \"{host_if}\" drop\n }}\n chain forward {{ type filter hook forward priority -20; policy accept;\n ct state established,related accept\n iifname \"{host_if}\" ip daddr 10.0.0.0/8 drop\n iifname \"{host_if}\" ip daddr 172.16.0.0/12 drop\n iifname \"{host_if}\" ip daddr 192.168.0.0/16 drop\n iifname \"{host_if}\" ip daddr 169.254.0.0/16 drop\n {allow} ip daddr {guest_ip} drop\n }}\n chain prerouting {{ type nat hook prerouting priority dstnat; policy accept;\n {exposed} }}\n chain postrouting {{ type nat hook postrouting priority srcnat; policy accept;\n ip saddr {guest_ip} masquerade\n }}\n}}\n");
        nft(&script)?;
        run(Command::new("nsenter").args(["-t",&ns_pid.to_string(),"-m","--","/bin/sh","-c","printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\n' > /run/resolv.conf; mount --bind /run/resolv.conf /etc/resolv.conf; touch /run/voltp-network-ready"]))?;
        Ok(Self { host_if, table })
    }
}

impl Drop for NetworkLease {
    fn drop(&mut self) {
        let _ = Command::new("nft")
            .args(["delete", "table", "ip", &self.table])
            .status();
        let _ = Command::new("ip")
            .args(["link", "del", &self.host_if])
            .status();
    }
}
fn find_private_net_pid(root: u32, host_ns: &Path) -> Option<u32> {
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if fs::read_link(format!("/proc/{pid}/ns/net")).ok().as_deref() != Some(host_ns) {
            return Some(pid);
        }
        if let Ok(children) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
            stack.extend(
                children
                    .split_whitespace()
                    .filter_map(|v| v.parse::<u32>().ok()),
            );
        }
    }
    None
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
        let lease = NetworkLease::configure(child.id(), "sandbox-test-a", &[]).unwrap();
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
}
