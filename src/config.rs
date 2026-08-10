use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

/// Floor for `security.argon2_cost`: operators cannot configure weaker
/// parameters than this, so a mis-set config can never weaken every hash.
pub const MIN_ARGON2_COST: u32 = 3;
/// Floor for `security.argon2_mem_kib` — 19 MiB, the argon2 crate's own
/// recommended default memory cost.
pub const MIN_ARGON2_MEM_KIB: u32 = 19 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub web: Web,
    pub paths: Paths,
    pub limits: Limits,
    pub security: Security,
    pub features: Features,
    /// Offsite backup mirror settings. Default-disabled so existing configs
    /// without the section keep parsing.
    #[serde(default)]
    pub backups: Backups,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    pub instance_name: String,
    pub locale: String,
    pub data_dir: PathBuf,
    pub log_level: String,
    /// Days of audit history kept before the probabilistic pruner deletes it.
    /// The newest ~500 entries are always retained regardless.
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,
}

fn default_audit_retention_days() -> u64 {
    90
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    pub listen: SocketAddr,
    pub session_ttl_hours: u64,
    pub max_body_mb: u64,
    /// Serve HTTPS with a self-signed certificate generated under
    /// `<data_dir>/tls`. Meant for deployments with no domain name, where a
    /// node pins the panel's fingerprint instead of validating a chain.
    #[serde(default)]
    pub tls_self_signed: bool,
    /// Extra hostnames/IPs to place in the self-signed certificate, on top of
    /// the local hostname and loopback addresses.
    #[serde(default)]
    pub tls_extra_sans: Vec<String>,
    /// CIDR ranges of reverse proxies allowed to set `X-Forwarded-For` /
    /// `X-Forwarded-Proto`. Empty (default) means the socket peer address is
    /// always authoritative and forwarded headers are never trusted.
    #[serde(default)]
    pub trusted_proxies: Vec<IpNet>,
    /// Extra browser origins (scheme://host[:port]) accepted for
    /// cookie-authenticated mutations. Empty (default) keeps the same-host
    /// rule only.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Hostnames (with optional port) accepted in the `Host` header;
    /// everything else is rejected with 400. This closes the
    /// DNS-rebinding / host-header-injection gap where Origin is compared
    /// against the request's own Host header. Empty (default) is the derived
    /// mode: the listen address, loopback aliases and the machine hostname
    /// are accepted, plus any IP-literal Host (DNS rebinding needs a
    /// hostname Host, so IP literals are safe). Deployments reached by a
    /// public name (reverse proxy, LAN hostname) must list it here
    /// explicitly; once non-empty the allowlist is strict.
    #[serde(default)]
    pub hostnames: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub servers_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub blueprints_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub website_dir: PathBuf,
    /// Data Lab SQLite storage. Deliberately outside `servers_dir`: a server
    /// root is bind-mounted into the workload sandbox and chowned to the
    /// workload UID, so anything stored under it is workload-writable and can
    /// be replaced with a symlink pointing at the panel's own database.
    #[serde(default = "default_datalab_dir")]
    pub datalab_dir: PathBuf,
}

fn default_datalab_dir() -> PathBuf {
    PathBuf::from("datalab")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub default_memory_mb: u64,
    pub default_disk_mb: u64,
    pub default_cpu_percent: u64,
    pub max_memory_mb: u64,
    pub max_servers_per_user: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    pub argon2_cost: u32,
    pub argon2_mem_kib: u32,
    pub rate_limit_per_min: u64,
    pub password_min_len: usize,
    pub allow_cross_server_dir: bool,
    /// Master key for encrypting webhook secrets at rest (AES-256-GCM).
    /// Empty (the default) keeps webhook secrets in plaintext — the legacy
    /// behavior. When set, newly written secrets are encrypted with a fresh
    /// per-row nonce and stored as `v1:<base64>`, and existing plaintext rows
    /// are upgraded lazily on first read. Changing or losing this key makes
    /// stored secrets undecryptable and is reported loudly at load time.
    #[serde(default)]
    pub webhook_master_key: String,
    /// When true, node API responses that are not signed are rejected instead
    /// of being accepted with a per-node warning. False (the default) keeps
    /// pre-upgrade agents working while the fleet catches up.
    #[serde(default)]
    pub require_signed_node_responses: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    pub enable_backups: bool,
    pub enable_databases: bool,
    pub enable_schedules: bool,
    pub enable_api_keys: bool,
    pub enable_2fa: bool,
    pub enable_websites: bool,
    pub enable_audit_log: bool,
}
/// Offsite backup mirror: an independent second copy of every backup archive,
/// typically on a different disk or mount than `paths.backups_dir`. The mirror
/// is best-effort — a mirror failure never fails or alters the primary backup
/// — and restore-from-mirror is manual: copy the archive back into
/// `backups_dir`, then use the normal restore flow.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Backups {
    #[serde(default)]
    pub mirror: Mirror,
}


fn default_mirror_keep() -> u64 {
    10
}

/// Mirror configuration. `path` is a LOCAL directory the panel process can
/// write to — use a mount point (NFS/SMB/network disk) to make the copy
/// physically remote. A relative path resolves against the panel's working
/// directory, unlike `paths.*` (which are pinned under `data_dir`): the mirror
/// is deliberately offsite. Archives land at
/// `<path>/<server-uuid>/<backup-uuid>.tar.gz` as plain copies (never
/// hardlinks — the mirror must be an independent file to survive primary-store
/// loss, and hardlinks fail across mounts anyway). `keep` bounds the archives
/// retained per server in the mirror, oldest first; mirror trimming never
/// touches the primary backup store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    #[serde(default)]
    pub enabled: bool,
    /// Mirror root directory; required when `enabled` is true.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Archives retained per server in the mirror.
    #[serde(default = "default_mirror_keep")]
    pub keep: u64,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            keep: default_mirror_keep(),
        }
    }
}

/// A network range parsed from a CIDR string (`10.0.0.0/8`, `::1/128`).
/// A bare IP is treated as a single host (`/32` or `/128`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => {
                let addr: IpAddr = addr
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid IP address in CIDR \"{s}\""))?;
                let prefix: u8 = prefix
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid prefix in CIDR \"{s}\""))?;
                let max = if addr.is_ipv4() { 32 } else { 128 };
                if prefix > max {
                    bail!("prefix /{prefix} exceeds {max} bits in CIDR \"{s}\"");
                }
                (addr, prefix)
            }
            None => {
                let addr: IpAddr = s
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid IP address \"{s}\""))?;
                let prefix = if addr.is_ipv4() { 32 } else { 128 };
                (addr, prefix)
            }
        };
        Ok(Self { addr, prefix })
    }

    /// True when `ip` falls inside this network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                (u32::from(net) & mask) == (u32::from(ip) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let net = net.octets();
                let ip = ip.octets();
                let full = (self.prefix / 8) as usize;
                let rem = self.prefix % 8;
                net[..full] == ip[..full]
                    && (rem == 0 || (net[full] >> (8 - rem)) == (ip[full] >> (8 - rem)))
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for IpNet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl std::str::FromStr for IpNet {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl serde::Serialize for IpNet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for IpNet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.resolve()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.general.instance_name.trim().is_empty() || self.general.instance_name.len() > 128 {
            bail!("general.instance_name must contain 1..=128 characters");
        }
        if self.general.data_dir.as_os_str().is_empty() {
            bail!("general.data_dir must not be empty");
        }
        tracing_subscriber::EnvFilter::try_new(&self.general.log_level)
            .map_err(|e| anyhow::anyhow!("invalid general.log_level: {e}"))?;
        if self.web.session_ttl_hours == 0 || self.web.session_ttl_hours > (i64::MAX as u64) / 7 {
            bail!("web.session_ttl_hours is out of range");
        }
        if !(1..=4096).contains(&self.web.max_body_mb) {
            bail!("web.max_body_mb must be between 1 and 4096");
        }
        if self.limits.default_memory_mb == 0
            || self.limits.default_memory_mb > self.limits.max_memory_mb
        {
            bail!("limits.default_memory_mb must be positive and no greater than max_memory_mb");
        }
        if self.limits.default_disk_mb == 0 {
            bail!("limits.default_disk_mb must be positive");
        }
        if !(1..=10_000).contains(&self.limits.default_cpu_percent) {
            bail!("limits.default_cpu_percent must be between 1 and 10000");
        }
        if self.limits.max_servers_per_user == 0 {
            bail!("limits.max_servers_per_user must be positive");
        }
        if self.security.argon2_cost < MIN_ARGON2_COST {
            bail!("security.argon2_cost must be at least {MIN_ARGON2_COST}");
        }
        if self.security.argon2_mem_kib < MIN_ARGON2_MEM_KIB {
            bail!("security.argon2_mem_kib must be at least {MIN_ARGON2_MEM_KIB} (19 MiB)");
        }
        argon2::Params::new(
            self.security.argon2_mem_kib,
            self.security.argon2_cost,
            1,
            None,
        )
        .map_err(|e| anyhow::anyhow!("invalid Argon2 parameters: {e}"))?;
        if self.security.rate_limit_per_min == 0 {
            bail!("security.rate_limit_per_min must be positive");
        }
        if !(8..=1024).contains(&self.security.password_min_len) {
            bail!("security.password_min_len must be between 8 and 1024");
        }
        if self.backups.mirror.enabled {
            let empty = self
                .backups
                .mirror
                .path
                .as_ref()
                .is_none_or(|p| p.as_os_str().is_empty());
            if empty {
                bail!("backups.mirror.path is required when backups.mirror.enabled is true");
            }
            if self.backups.mirror.keep == 0 {
                bail!("backups.mirror.keep must be at least 1 when backups.mirror.enabled is true");
            }
        }
        Ok(())
    }

    /// Canonicalize the data directory and pin every derived path underneath
    /// it. `data_dir` is realpath'd when it exists and otherwise resolved
    /// against the current directory; relative derived paths join it, and an
    /// absolute derived path is only honored if it still sits inside
    /// `data_dir` — a derived path pointing outside would write user data
    /// away from the panel's database and backup root.
    fn resolve(&mut self) -> Result<()> {
        let base = canonicalize_dir(&self.general.data_dir)?;
        self.general.data_dir = base.clone();

        let derived: [(&str, &mut PathBuf); 6] = [
            ("servers_dir", &mut self.paths.servers_dir),
            ("backups_dir", &mut self.paths.backups_dir),
            ("blueprints_dir", &mut self.paths.blueprints_dir),
            ("logs_dir", &mut self.paths.logs_dir),
            ("website_dir", &mut self.paths.website_dir),
            ("datalab_dir", &mut self.paths.datalab_dir),
        ];
        for (name, field) in derived {
            if field.is_relative() {
                *field = base.join(&*field);
            } else {
                let normalized = absolute_normalized(field);
                if !normalized.starts_with(&base) {
                    bail!(
                        "paths.{name} ({}) must be inside general.data_dir ({})",
                        field.display(),
                        base.display()
                    );
                }
                *field = normalized;
            }
        }

        Ok(())
    }


    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.general.data_dir,
            &self.paths.servers_dir,
            &self.paths.backups_dir,
            &self.paths.blueprints_dir,
            &self.paths.logs_dir,
            &self.paths.website_dir,
            &self.paths.datalab_dir,
        ] {
            std::fs::create_dir_all(d)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

/// Resolve `p` to an absolute, normalized path. An existing directory is
/// realpath'd (symlinks resolved); a not-yet-created one is made absolute
/// against the current directory and lexically normalized (`.`/`..` folded),
/// since `canonicalize` requires the path to exist and `ensure_dirs` runs
/// after config load.
fn canonicalize_dir(p: &Path) -> Result<PathBuf> {
    if p.exists() {
        return Ok(std::fs::canonicalize(p)?);
    }
    Ok(absolute_normalized(p))
}

fn absolute_normalized(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| PathBuf::from(p))
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Derive the default `web.hostnames` allowlist from the listen address,
/// loopback aliases, the machine hostname, and the configured
/// `web.tls_extra_sans` (mirroring the self-signed certificate SANs). Used
/// by the host-allowlist middleware when `web.hostnames` is left empty: on
/// top of these defaults the middleware also accepts any IP-literal Host, so
/// LAN-IP deployments work out of the box. An explicit allowlist is strict
/// and must name every reachable host.
pub(crate) fn default_hostnames(listen: &SocketAddr, extra_sans: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |ip: IpAddr| {
        if ip.is_ipv6() {
            names.push(format!("[{ip}]"));
            names.push(format!("[{ip}]:{}", listen.port()));
        } else {
            names.push(ip.to_string());
            names.push(format!("{ip}:{}", listen.port()));
        }
    };
    push(listen.ip());
    if !listen.ip().is_loopback() {
        // Loopback aliases stay reachable: same-host reverse proxy and direct
        // local access must not require config.
        push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        push(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    }
    if let Ok(h) = hostname::get() {
        let h = h.to_string_lossy().into_owned();
        if !h.is_empty() {
            names.push(h.clone());
            names.push(format!("{h}:{}", listen.port()));
        }
    }
    // Fold `tls_extra_sans` into the derived allowlist: the operator asked
    // the certificate to cover these names, so the panel is deliberately
    // reachable as them — a request Host matching a cert SAN must not 400.
    for san in extra_sans {
        let san = san.trim();
        if san.is_empty() {
            continue;
        }
        match san.parse::<IpAddr>() {
            Ok(ip) if ip.is_ipv6() => {
                names.push(format!("[{ip}]"));
                names.push(format!("[{ip}]:{}", listen.port()));
            }
            Ok(ip) => {
                names.push(ip.to_string());
                names.push(format!("{ip}:{}", listen.port()));
            }
            Err(_) => {
                names.push(san.to_string());
                names.push(format!("{san}:{}", listen.port()));
            }
        }
    }
    names
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                instance_name: "VoltPanel".into(),
                locale: "en".into(),
                data_dir: PathBuf::from("./data"),
                log_level: "info".into(),
                audit_retention_days: 90,
            },
            web: Web {
                listen: "127.0.0.1:8080".parse().unwrap(),
                session_ttl_hours: 24,
                max_body_mb: 64,
                tls_self_signed: false,
                tls_extra_sans: Vec::new(),
                trusted_proxies: Vec::new(),
                allowed_origins: Vec::new(),
                hostnames: Vec::new(),
            },
            paths: Paths {
                servers_dir: PathBuf::from("servers"),
                backups_dir: PathBuf::from("backups"),
                blueprints_dir: PathBuf::from("blueprints"),
                logs_dir: PathBuf::from("logs"),
                website_dir: PathBuf::from("websites"),
                datalab_dir: default_datalab_dir(),
            },
            limits: Limits {
                default_memory_mb: 1024,
                default_disk_mb: 8192,
                default_cpu_percent: 100,
                max_memory_mb: 16384,
                max_servers_per_user: 16,
            },
            security: Security {
                argon2_cost: 3,
                argon2_mem_kib: 65536,
                rate_limit_per_min: 120,
                password_min_len: 8,
                allow_cross_server_dir: false,
                webhook_master_key: String::new(),
                require_signed_node_responses: false,
            },
            features: Features {
                enable_backups: true,
                enable_databases: true,
                enable_schedules: true,
                enable_api_keys: true,
                enable_2fa: true,
                enable_websites: true,
                enable_audit_log: true,
            },
            backups: Backups::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{default_hostnames, default_mirror_keep, Config, IpNet, MIN_ARGON2_COST, MIN_ARGON2_MEM_KIB};
    use std::path::PathBuf;

    #[test]
    fn load_preserves_public_listen_address() {
        let mut config = Config::default();
        config.web.listen = "0.0.0.0:9090".parse().unwrap();
        let text = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.web.listen, "0.0.0.0:9090".parse().unwrap());
    }

    #[test]
    fn rejects_values_that_break_runtime_contracts() {
        let mut config = Config::default();
        config.web.max_body_mb = 0;
        assert!(config.validate().is_err());
        config.web.max_body_mb = 64;
        config.security.argon2_mem_kib = 0;
        assert!(config.validate().is_err());
        config.security.argon2_mem_kib = 65_536;
        config.limits.default_memory_mb = config.limits.max_memory_mb + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_weak_argon2_parameters() {
        let mut config = Config::default();
        config.security.argon2_cost = MIN_ARGON2_COST - 1;
        assert!(
            config.validate().is_err(),
            "cost below the floor must be rejected"
        );
        config.security.argon2_cost = MIN_ARGON2_COST;
        config.security.argon2_mem_kib = MIN_ARGON2_MEM_KIB - 1;
        assert!(
            config.validate().is_err(),
            "memory below the floor must be rejected"
        );
        config.security.argon2_mem_kib = MIN_ARGON2_MEM_KIB;
        assert!(
            config.validate().is_ok(),
            "config at the floor must validate"
        );
    }
    #[test]
    fn rejects_unknown_config_keys() {
        // Obsolete keys written by older installers (timezone, base_path,
        // jwt_secret, userland) must fail loudly now that Config is
        // deny_unknown_fields — check-config refuses dead/typo'd keys.
        let mut text = toml::to_string(&Config::default()).unwrap();
        text.push_str("[general]\ntimezone = \"UTC\"\n");
        text.push_str("[web]\nbase_path = \"/\"\n");
        text.push_str("[security]\njwt_secret = \"x\"\nuserland = \"nobody\"\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }
    #[test]
    fn trusted_proxies_parse_and_match() {
        let mut cfg = Config::default();
        cfg.web.trusted_proxies = vec![
            "10.0.0.0/8".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
            "::1".parse().unwrap(),
        ];
        let text = toml::to_string(&cfg).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        let nets = &parsed.web.trusted_proxies;
        assert_eq!(nets.len(), 3);
        assert!(nets[0].contains("10.1.2.3".parse().unwrap()));
        assert!(nets[1].contains("127.0.0.1".parse().unwrap()));
        assert!(nets[2].contains("::1".parse().unwrap()));
        assert!(!nets[0].contains("11.0.0.1".parse().unwrap()));
        assert!(!nets[1].contains("127.0.0.2".parse().unwrap()));
        assert!(!nets[0].contains("::1".parse().unwrap()));
        assert!(!nets[2].contains("::2".parse().unwrap()));
        assert!(!nets[2].contains("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn rejects_malformed_cidrs() {
        // Malformed CIDRs fail cleanly at parse time (no panic)...
        assert!(IpNet::parse("10.0.0.0/33").is_err());
        assert!(IpNet::parse("not-an-ip").is_err());
        // ...and a config carrying them is rejected on deserialization.
        let base = toml::to_string(&Config::default()).unwrap();
        let text = base.replace(
            "trusted_proxies = []",
            "trusted_proxies = [\"10.0.0.0/33\"]",
        );
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "/33 prefix must be rejected"
        );
        let text = base.replace("trusted_proxies = []", "trusted_proxies = [\"not-an-ip\"]");
        assert!(
            toml::from_str::<Config>(&text).is_err(),
            "non-IP trusted proxy must be rejected"
        );
        // Positive control: the same edit with a valid CIDR parses fine, so
        // the rejections above come from the values, not the mechanism.
        let text = base.replace("trusted_proxies = []", "trusted_proxies = [\"10.0.0.0/8\"]");
        assert!(toml::from_str::<Config>(&text).is_ok());
    }

    #[test]
    fn resolve_pins_derived_paths_and_keeps_hostnames_empty() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        // A data dir that does not exist yet must still resolve (ensure_dirs
        // creates it later); canonicalize only applies to existing dirs.
        config.general.data_dir = temp.path().join("data");
        config.web.listen = "127.0.0.1:8080".parse().unwrap();
        config.resolve().unwrap();
        // Derived hostnames stay empty: `default_hostnames` feeds the
        // middleware's derived-mode checks (listen IP + loopback + machine
        // hostname, plus any IP-literal Host) without baking a fixed list
        // into the config.
        assert!(config.web.hostnames.is_empty());
        let derived = default_hostnames(&config.web.listen, &config.web.tls_extra_sans);
        assert!(derived.iter().any(|h| h == "127.0.0.1"));
        assert!(derived.iter().any(|h| h == "127.0.0.1:8080"));
        // Derived paths pinned under the (absolute) data dir.
        assert!(config.general.data_dir.is_absolute());
        assert!(config.paths.servers_dir.starts_with(&config.general.data_dir));
        assert!(config.paths.backups_dir.starts_with(&config.general.data_dir));
        // An absolute derived path outside data_dir is refused.
        config.paths.servers_dir = temp.path().join("elsewhere").join("servers");
        let err = config.resolve().unwrap_err();
        assert!(err.to_string().contains("inside general.data_dir"));
    }

    #[test]
    fn default_hostnames_fold_in_tls_extra_sans() {
        let mut config = Config::default();
        config.web.listen = "127.0.0.1:8080".parse().unwrap();
        config.web.tls_extra_sans = vec![
            "panel.example.com".to_string(),
            "10.0.0.7".to_string(),
            "2001:db8::5".to_string(),
            "  ".to_string(), // blanks are ignored, matching tls::default_sans
        ];
        let derived = default_hostnames(&config.web.listen, &config.web.tls_extra_sans);
        // Hostname SANs are accepted bare (any port) and on the listen port.
        assert!(derived.iter().any(|h| h == "panel.example.com"));
        assert!(derived.iter().any(|h| h == "panel.example.com:8080"));
        // IP-literal SANs get the same bracket/port treatment as the listen
        // address; v6 stays bracketed so `split_host_port` parses it.
        assert!(derived.iter().any(|h| h == "10.0.0.7"));
        assert!(derived.iter().any(|h| h == "[2001:db8::5]"));
        // The baseline entries still survive.
        assert!(derived.iter().any(|h| h == "127.0.0.1"));
        // No blank entries leak into the allowlist.
        assert!(!derived.iter().any(|h| h.trim().is_empty()));
    }

    #[test]
    fn security_defaults_keep_legacy_behavior() {
        let cfg = Config::default();
        assert!(
            cfg.security.webhook_master_key.is_empty(),
            "empty master key = encryption disabled"
        );
        assert!(
            !cfg.security.require_signed_node_responses,
            "unsigned node responses stay accepted by default"
        );
        // Old configs without the new keys still deserialize (serde defaults).
        let text = toml::to_string(&Config::default())
            .unwrap()
            .replace("require_signed_node_responses = false", "");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert!(parsed.security.webhook_master_key.is_empty());
        assert!(!parsed.security.require_signed_node_responses);
        // deny_unknown_fields still bites in the security section: a typo'd
        // key next to the new fields is rejected, not silently ignored.
        let text = toml::to_string(&Config::default()).unwrap().replace(
            "require_signed_node_responses = false",
            "webhook_master_ley = \"typo\"",
        );
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn explicit_hostnames_are_preserved() {
        let mut config = Config::default();
        config.web.hostnames = vec!["panel.example.com".to_string()];
        config.resolve().unwrap();
        assert_eq!(config.web.hostnames, vec!["panel.example.com".to_string()]);
    }

    #[test]
    fn mirror_config_defaults_and_validation() {
        // Defaults: disabled, no path, sane keep.
        let cfg = Config::default();
        assert!(!cfg.backups.mirror.enabled);
        assert!(cfg.backups.mirror.path.is_none());
        assert_eq!(cfg.backups.mirror.keep, default_mirror_keep());
        // A legacy config without the [backups] section still parses.
        let text = toml::to_string(&Config::default()).unwrap();
        let legacy = text.split("[backups]").next().unwrap().to_string();
        assert!(toml::from_str::<Config>(&legacy).is_ok());
        // Enabled mirror requires a non-empty path and keep >= 1.
        let mut cfg = Config::default();
        cfg.backups.mirror.enabled = true;
        assert!(cfg.validate().is_err(), "enabled mirror without path must fail");
        cfg.backups.mirror.path = Some(PathBuf::from("/mnt/backups"));
        assert!(cfg.validate().is_ok());
        cfg.backups.mirror.keep = 0;
        assert!(cfg.validate().is_err(), "keep 0 must fail");
        cfg.backups.mirror.keep = 1;
        assert!(cfg.validate().is_ok());
        // A disabled mirror with an unset path is fine (staging a config).
        let mut cfg = Config::default();
        cfg.backups.mirror.enabled = false;
        cfg.backups.mirror.path = None;
        assert!(cfg.validate().is_ok());
    }
}