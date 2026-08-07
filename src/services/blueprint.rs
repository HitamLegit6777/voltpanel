//! Blueprint engine: input resolution, launch plans, and sandboxed setup.
use crate::db::Db;
use crate::models::{self, Blueprint, BlueprintInput, InputKind, Server};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use regex::Regex;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;

/// Resolve blueprint inputs for a workspace, applying owner overrides.
pub fn resolve_variables(db: &Db, server: &Server) -> Result<Vec<(BlueprintInput, String)>> {
    let blueprint = models::get_blueprint(db, server.blueprint_id)?;
    let overrides: HashMap<String, String> = models::get_server_vars(db, server.id)?
        .into_iter()
        .collect();
    let mut out = Vec::new();
    for var in &blueprint.variables {
        let value = overrides
            .get(&var.env_var)
            .cloned()
            .unwrap_or_else(|| var.default_value.clone());
        out.push((var.clone(), value));
    }
    Ok(out)
}

/// Placeholder syntax: `${input.NAME}` for blueprint inputs and
/// `${workspace.field}` for workspace metadata. Unknown keys are an error so a
/// typo surfaces at resolve time instead of launching a mangled command.
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{\s*([a-z]+)\.([A-Za-z0-9_]+)\s*\}").unwrap());

/// Namespaced values available to launch plans and config templates.
fn template_context(db: &Db, server: &Server) -> Result<HashMap<String, String>> {
    let mut ctx = HashMap::new();
    for (v, val) in resolve_variables(db, server)? {
        ctx.insert(format!("input.{}", v.env_var), val);
    }
    ctx.insert("workspace.name".into(), server.name.clone());
    ctx.insert("workspace.uuid".into(), server.uuid.clone());
    ctx.insert("workspace.memory_mb".into(), server.memory_mb.to_string());
    ctx.insert("workspace.disk_mb".into(), server.disk_mb.to_string());
    ctx.insert(
        "workspace.cpu_percent".into(),
        server.cpu_percent.to_string(),
    );
    ctx.insert(
        "workspace.port".into(),
        server.port.map(|p| p.to_string()).unwrap_or_default(),
    );
    ctx.insert(
        "workspace.dir".into(),
        crate::services::proc::server_dir(server)
            .to_string_lossy()
            .to_string(),
    );
    Ok(ctx)
}

/// Substitute every `${ns.key}` placeholder from `ctx`.
fn render(template: &str, ctx: &HashMap<String, String>) -> Result<String> {
    let mut missing: Option<String> = None;
    let out = PLACEHOLDER.replace_all(template, |caps: &regex::Captures| {
        let key = format!("{}.{}", &caps[1], &caps[2]);
        match ctx.get(&key) {
            Some(v) => v.clone(),
            None => {
                missing.get_or_insert(key);
                String::new()
            }
        }
    });
    if let Some(key) = missing {
        bail!("unknown placeholder ${{{key}}}");
    }
    Ok(out.into_owned())
}

/// Build the final launch command by interpolating blueprint and workspace placeholders.
pub fn resolve_startup(db: &Db, server: &Server) -> Result<String> {
    let blueprint = models::get_blueprint(db, server.blueprint_id)?;
    let template = if server.startup.is_empty() {
        blueprint.startup.clone()
    } else {
        server.startup.clone()
    };
    render(&template, &template_context(db, server)?)
}

/// Environment passed to the process from blueprint inputs and workspace metadata.
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

/// Validate an input value against its declared type. Errors name the input so
/// the API surfaces something an operator can act on.
pub fn validate_value(var: &BlueprintInput, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        if var.required {
            bail!("{} is required", var.env_var);
        }
        return Ok(());
    }
    match &var.kind {
        InputKind::Text { max_len, pattern } => {
            if let Some(mx) = max_len {
                if value.chars().count() > *mx {
                    bail!("{} must be at most {} characters", var.env_var, mx);
                }
            }
            if let Some(p) = pattern {
                let re = Regex::new(p).map_err(|e| anyhow!("blueprint pattern invalid: {e}"))?;
                if !re.is_match(value) {
                    bail!("{} does not match required pattern", var.env_var);
                }
            }
        }
        InputKind::Number { min, max } => {
            let v: f64 = trimmed
                .parse()
                .map_err(|_| anyhow!("{} must be a number", var.env_var))?;
            if let Some(mn) = min {
                if v < *mn {
                    bail!("{} must be at least {}", var.env_var, fmt_num(*mn));
                }
            }
            if let Some(mx) = max {
                if v > *mx {
                    bail!("{} must be at most {}", var.env_var, fmt_num(*mx));
                }
            }
        }
        InputKind::Bool => {
            if !matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "true" | "false" | "1" | "0" | "yes" | "no" | "on" | "off"
            ) {
                bail!("{} must be a boolean", var.env_var);
            }
        }
        InputKind::Choice { options } => {
            if !options.iter().any(|o| o == trimmed) {
                bail!("{} must be one of: {}", var.env_var, options.join(", "));
            }
        }
        InputKind::Path { max_len } => {
            if let Some(mx) = max_len {
                if value.chars().count() > *mx {
                    bail!("{} must be at most {} characters", var.env_var, mx);
                }
            }
            validate_workspace_path(&var.env_var, trimmed)?;
        }
        InputKind::Url => {
            if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
                bail!("{} must be an http(s) URL", var.env_var);
            }
            if trimmed.contains(char::is_whitespace) {
                bail!("{} must not contain whitespace", var.env_var);
            }
        }
    }
    Ok(())
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// A path input is interpolated into a launch plan that runs with the workspace
/// as cwd, so anything that can leave the workspace is refused here.
fn validate_workspace_path(env_var: &str, value: &str) -> Result<()> {
    if value.starts_with('/') {
        bail!("{env_var} must be a workspace-relative path");
    }
    if value.starts_with('~') {
        bail!("{env_var} must not reference a home directory");
    }
    if value.contains('\0') {
        bail!("{env_var} contains an invalid character");
    }
    for part in value.split('/') {
        if part == ".." {
            bail!("{env_var} must not traverse outside the workspace");
        }
    }
    Ok(())
}

/// Parse a VoltPanel blueprint JSON document.
pub fn parse_blueprint_json(json: &str) -> Result<BlueprintImport> {
    serde_json::from_str(json).map_err(|e| anyhow!("invalid blueprint json: {e}"))
}

#[derive(Debug, serde::Deserialize)]
pub struct BlueprintImport {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub category: String,
    #[serde(default = "default_runtime")]
    pub runtime_hint: String,
    #[serde(default)]
    pub startup: String,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub install: Option<InstallScript>,
    #[serde(default)]
    pub variables: Vec<BlueprintInput>,
    #[serde(default = "default_stop")]
    pub stop: String,
}

fn default_runtime() -> String {
    "native".into()
}
fn default_stop() -> String {
    "stop".into()
}

#[derive(Debug, serde::Deserialize)]
pub struct InstallScript {
    #[serde(default)]
    pub script: String,
}

/// Run blueprint setup inside an isolated workspace. Blocks until done.
pub fn run_install(
    db: &Db,
    server: &Server,
    blueprint: &Blueprint,
    notifier: &crate::services::proc::Notifier,
) -> Result<()> {
    let Some(script) = &blueprint.install_script else {
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

/// Build the default workspace configuration declared by a blueprint.
pub fn build_default_config(db: &Db, server: &Server) -> Result<Option<serde_json::Value>> {
    let blueprint = models::get_blueprint(db, server.blueprint_id)?;
    let Some(cfg) = &blueprint.default_config else {
        return Ok(None);
    };
    let text = render(cfg, &template_context(db, server)?)?;
    Ok(Some(
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    ))
}

// ---------------- Versioning & drift ----------------

/// Snapshot JSON of a blueprint's mutable content. The field set is stable so
/// revisions stay comparable across time; rollback restores this exact shape.
fn content_doc(b: &Blueprint) -> serde_json::Value {
    serde_json::json!({
        "name": b.name,
        "description": b.description,
        "author": b.author,
        "category": b.category,
        "runtime_hint": b.runtime_hint,
        "startup": b.startup,
        "stop_command": b.stop_command,
        "default_config": b.default_config,
        "install_script": b.install_script,
        "variables": b.variables,
    })
}

/// Deterministic SHA-256 of a blueprint's content: name, description, category,
/// runtime_hint, startup, stop, default_config, install_script, variables.
/// id/version/uuid/timestamps are metadata and never hashed; variables are
/// serialized in declaration order, so equal content always yields the same
/// digest.
pub fn digest_of(b: &Blueprint) -> String {
    let doc = serde_json::json!({
        "name": b.name,
        "description": b.description,
        "category": b.category,
        "runtime_hint": b.runtime_hint,
        "startup": b.startup,
        "stop_command": b.stop_command,
        "default_config": b.default_config,
        "install_script": b.install_script,
        "variables": b.variables,
    });
    hex::encode(Sha256::digest(doc.to_string().as_bytes()))
}

/// Load a blueprint by id from any connection (plain or transactional).
fn blueprint_row(conn: &rusqlite::Connection, id: i64) -> Result<Option<Blueprint>> {
    let row = conn
        .query_row(
            "SELECT id,uuid,name,description,author,category,runtime_hint,startup,default_config,install_script,variables,stop_command,created_at,updated_at FROM blueprints WHERE id=?1",
            [id],
            |r| {
                Ok(Blueprint {
                    id: r.get(0)?,
                    uuid: r.get(1)?,
                    name: r.get(2)?,
                    description: r.get(3)?,
                    author: r.get(4)?,
                    category: r.get(5)?,
                    runtime_hint: r.get(6)?,
                    startup: r.get(7)?,
                    default_config: r.get(8)?,
                    install_script: r.get(9)?,
                    variables: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
                    stop_command: r.get(11)?,
                    created_at: r.get(12)?,
                    updated_at: r.get(13)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Capture the blueprint's current state as a new revision at the next version,
/// bumping `blueprints.version`, all in one transaction so history and counter
/// never diverge. Returns the new version.
pub fn snapshot(db: &Db, blueprint_id: i64, author: &str, note: &str) -> Result<i64> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let bp = blueprint_row(&tx, blueprint_id)?.ok_or_else(|| anyhow!("blueprint not found"))?;
    let version: i64 = tx.query_row(
        "SELECT version FROM blueprints WHERE id=?1",
        [blueprint_id],
        |r| r.get(0),
    )?;
    let next = version + 1;
    tx.execute(
        "INSERT INTO blueprint_revisions (blueprint_id, version, snapshot, digest, author, note, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            blueprint_id,
            next,
            content_doc(&bp).to_string(),
            digest_of(&bp),
            author,
            note,
            Utc::now().to_rfc3339()
        ],
    )?;
    tx.execute(
        "UPDATE blueprints SET version=?1 WHERE id=?2",
        params![next, blueprint_id],
    )?;
    tx.commit()?;
    Ok(next)
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionMeta {
    pub id: i64,
    pub version: i64,
    pub digest: String,
    pub author: String,
    pub note: String,
    pub created_at: String,
}

/// Revision list, newest first.
pub fn list_revisions(db: &Db, blueprint_id: i64) -> Result<Vec<RevisionMeta>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT id, version, digest, author, note, created_at FROM blueprint_revisions WHERE blueprint_id=?1 ORDER BY version DESC",
    )?;
    let rows = stmt.query_map([blueprint_id], |r| {
        Ok(RevisionMeta {
            id: r.get(0)?,
            version: r.get(1)?,
            digest: r.get(2)?,
            author: r.get(3)?,
            note: r.get(4)?,
            created_at: r.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Full snapshot JSON stored for a revision.
pub fn revision_snapshot(db: &Db, blueprint_id: i64, version: i64) -> Result<serde_json::Value> {
    let conn = db.lock();
    let snap: String = conn
        .query_row(
            "SELECT snapshot FROM blueprint_revisions WHERE blueprint_id=?1 AND version=?2",
            params![blueprint_id, version],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("revision {version} does not exist"))?;
    Ok(serde_json::from_str(&snap)?)
}

/// Restore a blueprint to a past revision. Current state is snapshotted first so
/// the rollback is itself undoable; version is bumped on restore. Returns the
/// new current version.
pub fn rollback(db: &Db, blueprint_id: i64, version: i64, author: &str) -> Result<i64> {
    rollback_inner(
        db,
        blueprint_id,
        version,
        author,
        &format!("rollback to v{version}"),
    )
}

/// `rollback` with an explicit note recorded on the undo snapshot.
pub fn rollback_with_note(
    db: &Db,
    blueprint_id: i64,
    version: i64,
    author: &str,
    note: &str,
) -> Result<i64> {
    rollback_inner(db, blueprint_id, version, author, note)
}

fn rollback_inner(
    db: &Db,
    blueprint_id: i64,
    version: i64,
    author: &str,
    note: &str,
) -> Result<i64> {
    let mut conn = db.lock();
    let tx = conn.transaction()?;
    let cur = blueprint_row(&tx, blueprint_id)?.ok_or_else(|| anyhow!("blueprint not found"))?;
    let cur_version: i64 = tx.query_row(
        "SELECT version FROM blueprints WHERE id=?1",
        [blueprint_id],
        |r| r.get(0),
    )?;
    let snap_json: String = tx
        .query_row(
            "SELECT snapshot FROM blueprint_revisions WHERE blueprint_id=?1 AND version=?2",
            params![blueprint_id, version],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("revision {version} does not exist"))?;
    let snap: serde_json::Value = serde_json::from_str(&snap_json)?;
    // Restore content fields; id/uuid/created_at keep their current values.
    let mut restored = cur.clone();
    restored.name = snap["name"].as_str().unwrap_or("").to_string();
    restored.description = snap["description"].as_str().unwrap_or("").to_string();
    restored.author = snap["author"].as_str().unwrap_or("").to_string();
    restored.category = snap["category"].as_str().unwrap_or("").to_string();
    restored.runtime_hint = snap["runtime_hint"].as_str().unwrap_or("").to_string();
    restored.startup = snap["startup"].as_str().unwrap_or("").to_string();
    restored.stop_command = snap["stop_command"].as_str().unwrap_or("").to_string();
    restored.default_config = snap["default_config"].as_str().map(String::from);
    restored.install_script = snap["install_script"].as_str().map(String::from);
    restored.variables = serde_json::from_value(snap["variables"].clone()).unwrap_or_default();
    let next = cur_version + 1;
    // Undo snapshot of the pre-rollback state.
    tx.execute(
        "INSERT INTO blueprint_revisions (blueprint_id, version, snapshot, digest, author, note, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            blueprint_id,
            next,
            content_doc(&cur).to_string(),
            digest_of(&cur),
            author,
            note,
            Utc::now().to_rfc3339()
        ],
    )?;
    tx.execute(
        "UPDATE blueprints SET name=?1, description=?2, author=?3, category=?4, runtime_hint=?5, startup=?6, default_config=?7, install_script=?8, variables=?9, stop_command=?10, updated_at=?11, version=?12 WHERE id=?13",
        params![
            restored.name,
            restored.description,
            restored.author,
            restored.category,
            restored.runtime_hint,
            restored.startup,
            restored.default_config,
            restored.install_script,
            serde_json::to_string(&restored.variables)?,
            restored.stop_command,
            Utc::now().to_rfc3339(),
            next,
            blueprint_id
        ],
    )?;
    tx.commit()?;
    Ok(next)
}

/// Per-server view of blueprint drift.
#[derive(Debug, Clone, Serialize)]
pub struct Drift {
    pub server_id: i64,
    pub server_name: String,
    pub pinned_version: i64,
    pub current_version: i64,
    pub fields: Vec<String>,
}

/// Snapshot keys compared for drift; must stay in sync with `content_doc`.
const DRIFT_KEYS: [&str; 10] = [
    "name",
    "description",
    "author",
    "category",
    "runtime_hint",
    "startup",
    "stop_command",
    "default_config",
    "install_script",
    "variables",
];

/// Keys whose values differ between a pinned revision and current content.
pub fn diff_revisions(pinned: &serde_json::Value, current: &serde_json::Value) -> Vec<String> {
    DRIFT_KEYS
        .iter()
        .filter(|k| pinned.get(**k) != current.get(**k))
        .map(|k| k.to_string())
        .collect()
}

/// Drift report for every server on a blueprint. Servers behind the current
/// version get the key-level diff; `blueprint_version = 0` marks an unpinned
/// server; in-sync servers are omitted.
pub fn drift_for_blueprint(db: &Db, blueprint_id: i64) -> Result<Vec<Drift>> {
    let conn = db.lock();
    let current_version: i64 = conn
        .query_row(
            "SELECT version FROM blueprints WHERE id=?1",
            [blueprint_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("blueprint not found"))?;
    let current = content_doc(&blueprint_row(&conn, blueprint_id)?.unwrap());
    let mut stmt = conn.prepare(
        "SELECT id, name, blueprint_version FROM servers WHERE blueprint_id=?1 AND deleted=0 ORDER BY name",
    )?;
    let rows = stmt.query_map([blueprint_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (server_id, server_name, pinned_version) = row?;
        if pinned_version == 0 {
            out.push(Drift {
                server_id,
                server_name,
                pinned_version: 0,
                current_version,
                fields: vec!["unpinned".into()],
            });
            continue;
        }
        if pinned_version >= current_version {
            continue; // not behind
        }
        let snap_json: Option<String> = conn
            .query_row(
                "SELECT snapshot FROM blueprint_revisions WHERE blueprint_id=?1 AND version=?2",
                params![blueprint_id, pinned_version],
                |r| r.get(0),
            )
            .optional()?;
        // Dangling pin (revision deleted): nothing to diff against, skip.
        let Some(snap_json) = snap_json else { continue };
        let pinned: serde_json::Value = serde_json::from_str(&snap_json)?;
        out.push(Drift {
            server_id,
            server_name,
            pinned_version,
            current_version,
            fields: diff_revisions(&pinned, &current),
        });
    }
    Ok(out)
}

/// Pin a server to a blueprint revision, or `0` to unpin. The version must be a
/// real revision of the server's blueprint so drift always has a snapshot to
/// diff against.
pub fn pin_server(db: &Db, server_id: i64, version: i64) -> Result<()> {
    let conn = db.lock();
    let blueprint_id: i64 = conn
        .query_row(
            "SELECT blueprint_id FROM servers WHERE id=?1 AND deleted=0",
            [server_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("server not found"))?;
    if version != 0 {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM blueprint_revisions WHERE blueprint_id=?1 AND version=?2",
            params![blueprint_id, version],
            |r| r.get(0),
        )?;
        if exists == 0 {
            bail!("revision {version} does not exist for this blueprint");
        }
    }
    conn.execute(
        "UPDATE servers SET blueprint_version=?1 WHERE id=?2",
        params![version, server_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp() -> Blueprint {
        Blueprint {
            id: 1,
            uuid: "u1".into(),
            name: "minecraft".into(),
            description: "vanilla server".into(),
            author: "alice".into(),
            category: "game".into(),
            runtime_hint: "native".into(),
            startup: "java -Xmx1G -jar server.jar nogui".into(),
            default_config: Some(r#"{"server.properties": {}}"#.into()),
            install_script: Some("apt-get install -y openjdk-17".into()),
            variables: vec![BlueprintInput {
                name: "Version".into(),
                description: String::new(),
                env_var: "MC_VERSION".into(),
                default_value: "1.20.1".into(),
                user_viewable: true,
                user_editable: true,
                required: false,
                kind: InputKind::Text {
                    max_len: None,
                    pattern: None,
                },
            }],
            stop_command: "stop".into(),
            created_at: "t1".into(),
            updated_at: "t2".into(),
        }
    }

    #[test]
    fn digest_is_deterministic() {
        assert_eq!(digest_of(&bp()), digest_of(&bp()));
    }

    #[test]
    fn digest_changes_with_variable_default() {
        let mut b = bp();
        let before = digest_of(&b);
        b.variables[0].default_value = "1.21".into();
        assert_ne!(before, digest_of(&b));
    }

    #[test]
    fn digest_ignores_metadata() {
        let mut b = bp();
        let before = digest_of(&b);
        b.id = 99;
        b.updated_at = "later".into();
        assert_eq!(before, digest_of(&b));
    }

    #[test]
    fn diff_revisions_reports_only_changed_keys() {
        let pinned = content_doc(&bp());
        let mut cur = bp();
        cur.variables[0].default_value = "1.21".into();
        cur.description = "spigot server".into();
        assert_eq!(
            diff_revisions(&pinned, &content_doc(&cur)),
            vec!["description".to_string(), "variables".to_string()]
        );
        assert!(diff_revisions(&content_doc(&bp()), &content_doc(&bp())).is_empty());
    }
}
