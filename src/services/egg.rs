//! Egg engine: variable resolution, startup command building, install runner.
use crate::db::Db;
use crate::models::{self, Egg, EggVariable, Server};
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use std::collections::HashMap;

/// Resolve all egg variables for a server, applying user overrides.
pub fn resolve_variables(db: &Db, server: &Server) -> Result<Vec<(EggVariable, String)>> {
    let egg = models::get_egg(db, server.egg_id)?;
    let overrides: HashMap<String, String> = models::get_server_vars(db, server.id)?
        .into_iter()
        .collect();
    let mut out = Vec::new();
    for var in &egg.variables {
        let value = overrides
            .get(&var.env_var)
            .cloned()
            .unwrap_or_else(|| var.default_value.clone());
        out.push((var.clone(), value));
    }
    Ok(out)
}

/// Build the final startup command by interpolating {{VAR}} and {{SERVER_*}} placeholders.
pub fn resolve_startup(db: &Db, server: &Server) -> Result<String> {
    let egg = models::get_egg(db, server.egg_id)?;
    let template = if server.startup.is_empty() {
        egg.startup.clone()
    } else {
        server.startup.clone()
    };
    let mut env = HashMap::new();
    for (v, val) in resolve_variables(db, server)? {
        env.insert(v.env_var.clone(), val);
    }
    env.insert("SERVER_NAME".into(), server.name.clone());
    env.insert("SERVER_UUID".into(), server.uuid.clone());
    env.insert("SERVER_MEMORY".into(), server.memory_mb.to_string());
    env.insert("SERVER_DISK".into(), server.disk_mb.to_string());
    env.insert("SERVER_CPU".into(), server.cpu_percent.to_string());
    env.insert(
        "SERVER_PORT".into(),
        server.port.map(|p| p.to_string()).unwrap_or_default(),
    );
    let re = Regex::new(r"\{\{\s*([A-Za-z0-9_]+)\s*\}\}").unwrap();
    let out = re.replace_all(&template, |caps: &regex::Captures| {
        env.get(&caps[1]).cloned().unwrap_or_default()
    });
    Ok(out.to_string())
}

/// Environment variables passed to the process (everything the egg declares + panel vars).
pub fn env_for_server(db: &Db, server: &Server) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Ok(vars) = resolve_variables(db, server) {
        for (v, val) in vars {
            if v.user_viewable || !val.is_empty() {
                env.push((v.env_var.clone(), val));
            }
        }
    }
    env.push((
        "HOME".into(),
        crate::services::proc::server_dir(server)
            .to_string_lossy()
            .to_string(),
    ));
    env.push((
        "PWD".into(),
        crate::services::proc::server_dir(server)
            .to_string_lossy()
            .to_string(),
    ));
    env.push(("SERVER_UUID".into(), server.uuid.clone()));
    env.push(("SERVER_NAME".into(), server.name.clone()));
    env.push(("SERVER_MEMORY".into(), server.memory_mb.to_string()));
    env.push(("SERVER_DISK".into(), server.disk_mb.to_string()));
    env.push(("SERVER_CPU".into(), server.cpu_percent.to_string()));
    env.push((
        "SERVER_PORT".into(),
        server.port.map(|p| p.to_string()).unwrap_or_default(),
    ));
    env
}

/// Validate a variable value against egg rules (min/max/required/regex).
pub fn validate_value(var: &EggVariable, value: &str) -> Result<()> {
    let rules: Vec<&str> = var.rules.split('|').map(|r| r.trim()).collect();
    let mut numeric = false;
    let mut min: Option<f64> = None;
    let mut max: Option<f64> = None;
    for r in &rules {
        match *r {
            "required" => {
                if value.trim().is_empty() {
                    bail!("{} is required", var.env_var);
                }
            }
            "numeric" => numeric = true,
            r if r.starts_with("min:") => {
                min = Some(
                    r[4..]
                        .trim()
                        .parse()
                        .map_err(|_| anyhow!("invalid min rule"))?,
                );
            }
            r if r.starts_with("max:") => {
                max = Some(
                    r[4..]
                        .trim()
                        .parse()
                        .map_err(|_| anyhow!("invalid max rule"))?,
                );
            }
            r if r.starts_with("regex:") => {
                let re = Regex::new(&r[6..]).map_err(|_| anyhow!("invalid regex rule"))?;
                if !re.is_match(value) {
                    bail!("{} does not match required pattern", var.env_var);
                }
            }
            _ => {}
        }
    }
    let fmt_num = |n: f64| -> String {
        if n.fract() == 0.0 {
            format!("{}", n as i64)
        } else {
            format!("{n}")
        }
    };
    if numeric {
        let v: f64 = value
            .trim()
            .parse()
            .map_err(|_| anyhow!("{} must be numeric", var.env_var))?;
        if let Some(mn) = min {
            if v < mn {
                bail!("{} must be at least {}", var.env_var, fmt_num(mn));
            }
        }
        if let Some(mx) = max {
            if v > mx {
                bail!("{} must be at most {}", var.env_var, fmt_num(mx));
            }
        }
    } else if let (Some(mn), Some(mx)) = (min, max) {
        let len = value.len() as f64;
        if len < mn {
            bail!(
                "{} must be at least {} characters",
                var.env_var,
                fmt_num(mn)
            );
        }
        if len > mx {
            bail!("{} must be at most {} characters", var.env_var, fmt_num(mx));
        }
    }
    Ok(())
}

/// Parse an egg JSON string into an Egg struct (used by egg import).
pub fn parse_egg_json(json: &str) -> Result<EggImport> {
    serde_json::from_str(json).map_err(|e| anyhow!("invalid egg json: {e}"))
}

#[derive(Debug, serde::Deserialize)]
pub struct EggImport {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub category: String,
    #[serde(default = "default_image")]
    pub docker_image: String,
    #[serde(default)]
    pub startup: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub install: Option<InstallScript>,
    #[serde(default)]
    pub variables: Vec<EggVariable>,
    #[serde(default = "default_stop")]
    pub stop: String,
}

fn default_image() -> String {
    "alpine:latest".into()
}
fn default_stop() -> String {
    "stop".into()
}

#[derive(Debug, serde::Deserialize)]
pub struct InstallScript {
    #[serde(default)]
    pub script: String,
}

/// Run the egg install script inside a server dir. Blocks until done.
pub fn run_install(
    db: &Db,
    server: &Server,
    egg: &Egg,
    notifier: &crate::services::proc::Notifier,
) -> Result<()> {
    let Some(script) = &egg.install_script else {
        return Ok(());
    };
    if script.trim().is_empty() {
        return Ok(());
    }
    let dir = crate::services::proc::server_dir(server);
    std::fs::create_dir_all(&dir)?;
    crate::isolation::prepare_root(&dir, &server.uuid)?;
    crate::isolation::own_tree(&dir, &server.uuid)?;
    let env = env_for_server(db, server);
    let isolation = crate::isolation::IsolationConfig::default();
    let limits = crate::isolation::Limits {
        memory_bytes: server.memory_mb.max(256) as u64 * 1_048_576,
        cpu_percent: server.cpu_percent.max(25) as u64,
        pids_max: 256,
    };
    let cgroup =
        crate::isolation::Cgroup::create(&isolation, &format!("{}-install", server.uuid), &limits)?;
    let mut cmd = crate::isolation::sandbox_command(
        &isolation,
        &dir,
        &format!("{}-install", server.uuid),
        script,
        &limits,
    )?;
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("sandboxed install failed to start: {e}"))?;
    let pid = child.id();
    cgroup.attach(pid)?;
    let network =
        crate::isolation::NetworkLease::configure(pid, &format!("{}-install", server.uuid), &[])?;
    let out = child.wait_with_output()?;
    drop(network);
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).to_string();
        notifier.notify(
            "error",
            &format!("Install failed for '{}'", server.name),
            &msg,
            Some(server.id),
        );
        bail!(
            "install script failed: {}",
            msg.lines().next().unwrap_or("unknown error")
        );
    }
    notifier.notify(
        "success",
        &format!("'{}' installed", server.name),
        "Install script completed",
        Some(server.id),
    );
    Ok(())
}

/// Build the default config JSON for an egg (substitutes variables).
pub fn build_default_config(db: &Db, server: &Server) -> Result<Option<serde_json::Value>> {
    let egg = models::get_egg(db, server.egg_id)?;
    let Some(cfg) = &egg.default_config else {
        return Ok(None);
    };
    let mut env = HashMap::new();
    for (v, val) in resolve_variables(db, server)? {
        env.insert(v.env_var.clone(), val);
    }
    env.insert(
        "SERVER_PORT".into(),
        server.port.map(|p| p.to_string()).unwrap_or_default(),
    );
    env.insert("SERVER_NAME".into(), server.name.clone());
    let mut text = cfg.clone();
    let re = Regex::new(r"\{\{\s*([A-Za-z0-9_]+)\s*\}\}").unwrap();
    text = re
        .replace_all(&text, |caps: &regex::Captures| {
            env.get(&caps[1]).cloned().unwrap_or_default()
        })
        .to_string();
    Ok(Some(
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    ))
}
