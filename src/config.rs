use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub general: General,
    pub web: Web,
    pub paths: Paths,
    pub limits: Limits,
    pub security: Security,
    pub features: Features,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct General {
    pub instance_name: String,
    pub locale: String,
    pub timezone: String,
    pub data_dir: PathBuf,
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Web {
    pub listen: SocketAddr,
    pub base_path: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paths {
    pub servers_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub blueprints_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub website_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    pub default_memory_mb: u64,
    pub default_disk_mb: u64,
    pub default_cpu_percent: u64,
    pub max_memory_mb: u64,
    pub max_servers_per_user: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Security {
    pub argon2_cost: u32,
    pub argon2_mem_kib: u32,
    pub jwt_secret: String,
    pub rate_limit_per_min: u64,
    pub password_min_len: usize,
    pub userland: String,
    pub allow_cross_server_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    pub enable_backups: bool,
    pub enable_databases: bool,
    pub enable_schedules: bool,
    pub enable_api_keys: bool,
    pub enable_2fa: bool,
    pub enable_websites: bool,
    pub enable_audit_log: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.resolve();
        Ok(cfg)
    }

    fn resolve(&mut self) {
        let base = self.general.data_dir.clone();
        if self.paths.servers_dir.is_relative() {
            self.paths.servers_dir = base.join(&self.paths.servers_dir);
        }
        if self.paths.backups_dir.is_relative() {
            self.paths.backups_dir = base.join(&self.paths.backups_dir);
        }
        if self.paths.blueprints_dir.is_relative() {
            self.paths.blueprints_dir = base.join(&self.paths.blueprints_dir);
        }
        if self.paths.logs_dir.is_relative() {
            self.paths.logs_dir = base.join(&self.paths.logs_dir);
        }
        if self.paths.website_dir.is_relative() {
            self.paths.website_dir = base.join(&self.paths.website_dir);
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.general.data_dir,
            &self.paths.servers_dir,
            &self.paths.backups_dir,
            &self.paths.blueprints_dir,
            &self.paths.logs_dir,
            &self.paths.website_dir,
        ] {
            std::fs::create_dir_all(d)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General {
                instance_name: "VoltPanel".into(),
                locale: "en".into(),
                timezone: "UTC".into(),
                data_dir: PathBuf::from("./data"),
                log_level: "info".into(),
            },
            web: Web {
                listen: "127.0.0.1:8080".parse().unwrap(),
                base_path: "/".into(),
                session_ttl_hours: 24,
                max_body_mb: 64,
                tls_self_signed: false,
                tls_extra_sans: Vec::new(),
            },
            paths: Paths {
                servers_dir: PathBuf::from("servers"),
                backups_dir: PathBuf::from("backups"),
                blueprints_dir: PathBuf::from("blueprints"),
                logs_dir: PathBuf::from("logs"),
                website_dir: PathBuf::from("websites"),
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
                jwt_secret: String::new(),
                rate_limit_per_min: 120,
                password_min_len: 8,
                userland: "nobody".into(),
                allow_cross_server_dir: false,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn load_preserves_public_listen_address() {
        let mut config = Config::default();
        config.web.listen = "0.0.0.0:9090".parse().unwrap();
        let text = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.web.listen, "0.0.0.0:9090".parse().unwrap());
    }
}
