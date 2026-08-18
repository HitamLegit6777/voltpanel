//! `voltd` — lightweight VoltPanel execution agent.
//! Commands:
//! - `voltd join <panel-url> <token> [--public-url URL] [--listen ADDR]`
//! - `voltd serve [--config PATH]`
use anyhow::{bail, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMessage};
use tower::ServiceExt;
use tracing_subscriber::EnvFilter;
use voltpanel::node_daemon::{DaemonConfig, DaemonRuntime};
use voltpanel::node_protocol::{
    self, AgentUpdateManifest, ChannelRequest, ChannelResponse, ConsoleCommand, ConsoleSnapshot,
    FileOperation, FileWriteRequest, NodeApiResponse, NodeHeartbeat, NodeTerminalRequest,
    NodeTerminalResponse, PowerAction, PowerRequest, SignedHeaders,
};

/// Panel-side correlation header (mirrors `services/node.rs`); not part of
/// the HMAC canonical string — informational only, never trusted.
const REQUEST_ID_HEADER: &str = "x-volt-request-id";

#[derive(Clone)]
struct DaemonState {
    runtime: DaemonRuntime,
    nonces: Arc<NonceStore>,
}

/// Bounded replay ledger for the signed-request API.
///
/// `check_and_insert` records each (node, nonce) pair seen; a duplicate pair
/// is a replay and rejected. The map is pruned lazily, at most once per
/// [`NONCE_PRUNE_INTERVAL_SECS`], with a compare-exchange gate so only one
/// caller pays the full-map scan. The prune floor (`now - 2*skew`) is below
/// every timestamp the verifier still accepts (+- `MAX_CLOCK_SKEW_SECS`), so
/// a pair that could still be replayed is never pruned early: the 90-second
/// replay window is exactly the one the old per-request pruning enforced.
///
/// The ledger is in-memory and dies with the process, so a restart would
/// reopen the replay window for requests captured before it. To close most of
/// that hole cheaply, the newest accepted timestamp per node (a high-water
/// mark) is persisted on a timer and at shutdown; after a restart, a request
/// signed at or before the loaded watermark (minus a small grace) is treated
/// as an unverifiable pre-restart capture and rejected even though its nonce
/// is unknown. The grace keeps the check from rejecting legitimate
/// same-second concurrent requests, which legitimately share the newest
/// second-granularity timestamp.
struct NonceStore {
    seen: dashmap::DashMap<String, i64>,
    last_prune: AtomicI64,
    /// Serializes compaction sweeps: a sweep's snapshot + retain must never
    /// overlap another's, or a nonce accepted mid-sweep is evicted right
    /// after `check_and_insert` returned `false`, reopening the replay window.
    compacting: AtomicBool,
    /// Per-node newest accepted timestamp, used to reject pre-restart
    /// captures once the nonce ledger is gone.
    watermarks: dashmap::DashMap<String, i64>,
    /// Live-entry bound; production uses [`NONCE_CAP`], tests shrink it.
    cap: usize,
    path: Option<std::path::PathBuf>,
}

/// Minimum seconds between full-map stale scans. Bounds the scan cost to at
/// most one request per interval while keeping replay rejection independent
/// of when the last prune ran. Also the cadence for watermark persistence.
const NONCE_PRUNE_INTERVAL_SECS: i64 = 30;

/// Hard bound on live (node, nonce) pairs. The time-based prune alone is not
/// enough: within a single replay window an attacker can mint arbitrarily
/// many well-formed (node, nonce) pairs — `verify_pending` is format-only —
/// so `check_and_insert` compacts once the cap is exceeded, exactly like the
/// panel's `NonceCache`. Compaction evicts only from the in-memory ledger:
/// the persisted watermark file is never touched, so the pre-restart replay
/// semantics are unchanged.
const NONCE_CAP: usize = 100_000;
/// Entries retired per compaction: a quarter of the cap (never less than the
/// current overflow), so a compaction frees a large batch and the next one
/// only happens after that many new inserts.
const NONCE_RETIRE_FRACTION: usize = 4;

/// Seconds of tolerance below the persisted watermark before a request is
/// classified as a pre-restart replay. Requests are signed with
/// second-granularity timestamps, so concurrent requests can legitimately
/// carry the newest timestamp; requiring `ts + grace <= watermark` keeps them
/// accepted while still blocking replayed captures older than the grace.
const REPLAY_WATERMARK_GRACE_SECS: i64 = 5;

impl NonceStore {
    fn new(path: Option<std::path::PathBuf>) -> Self {
        let watermarks = dashmap::DashMap::new();
        if let Some(path) = &path {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    match serde_json::from_str::<std::collections::HashMap<String, i64>>(&text) {
                        Ok(map) => {
                            for (node, ts) in map {
                                watermarks.insert(node, ts);
                            }
                        }
                        Err(e) => tracing::warn!(
                            "ignoring corrupt replay watermark {}: {e}",
                            path.display()
                        ),
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("cannot read replay watermark {}: {e}", path.display()),
            }
        }
        Self {
            seen: dashmap::DashMap::new(),
            last_prune: AtomicI64::new(0),
            compacting: AtomicBool::new(false),
            watermarks,
            cap: NONCE_CAP,
            path,
        }
    }

    #[cfg(test)]
    fn with_cap(cap: usize) -> Self {
        Self {
            seen: dashmap::DashMap::new(),
            last_prune: AtomicI64::new(0),
            compacting: AtomicBool::new(false),
            watermarks: dashmap::DashMap::new(),
            cap,
            path: None,
        }
    }

    /// Record a (node, nonce) pair at `ts`; returns `true` when the pair was
    /// already seen, or when `ts` sits at or below the node's persisted
    /// high-water mark (minus grace) and is therefore an unverifiable
    /// pre-restart capture. Pruning never gates the replay check: a duplicate
    /// is rejected even when no prune ran since the original insert.
    fn check_and_insert(&self, node_id: &str, nonce: &str, ts: i64, now: i64) -> bool {
        let last = self.last_prune.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= NONCE_PRUNE_INTERVAL_SECS
            && self
                .last_prune
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let floor = now - node_protocol::MAX_CLOCK_SKEW_SECS * 2;
            self.seen.retain(|_, stored| *stored >= floor);
        }
        let key = format!("{node_id}:{nonce}");
        let fresh = self.seen.insert(key.clone(), ts).is_none();
        // Compact only after the insert, and only when the cap is actually
        // exceeded: the new pair is then counted in the sweep, so the newest
        // entries stay while the oldest ones are retired.
        if self.seen.len() > self.cap {
            self.compact();
        }
        // Re-assert the pair unconditionally once any sweep has finished. A
        // concurrent sweep may have snapshotted its keep-set before this
        // insert and then evicted the pair; it may also have shrunk the map
        // below the cap while this caller was between the insert and the
        // `len > cap` check, in which case `compact` above never ran and the
        // pair would otherwise stay evicted. The re-insert is idempotent when
        // no sweep raced (same key, same timestamp), so an accepted nonce
        // never loses its replay guard.
        self.seen.insert(key, ts);
        if !fresh {
            return true;
        }
        if self
            .watermarks
            .get(node_id)
            .is_some_and(|w| *w >= ts.saturating_add(REPLAY_WATERMARK_GRACE_SECS))
        {
            return true;
        }
        self.watermarks
            .entry(node_id.to_string())
            .and_modify(|w| *w = (*w).max(ts))
            .or_insert(ts);
        false
    }

    /// Evict the oldest entries (by timestamp) once the ledger overflows the
    /// cap. A batch of at least the overflow, up to a quarter of the cap, is
    /// retired per sweep, so the bound stays hard while the cost amortizes.
    /// Eviction touches only the in-memory `seen` set — the persisted
    /// watermark file keeps its semantics untouched.
    ///
    /// Only one sweep runs at a time. A contender waits for the in-flight
    /// sweep to finish before returning, so the caller's re-insert (see
    /// `check_and_insert`) always lands after that sweep's retain pass.
    fn compact(&self) {
        if self
            .compacting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            while self.compacting.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            return;
        }
        let over = self.seen.len().saturating_sub(self.cap);
        if over > 0 {
            let retire = (self.cap / NONCE_RETIRE_FRACTION).max(over);
            let mut entries: Vec<(String, i64)> = self
                .seen
                .iter()
                .map(|e| (e.key().clone(), *e.value()))
                .collect();
            entries.sort_unstable_by_key(|(_, ts)| *ts);
            let keep: std::collections::HashSet<_> =
                entries.into_iter().skip(retire).map(|(k, _)| k).collect();
            self.seen.retain(|k, _| keep.contains(k));
        }
        self.compacting.store(false, Ordering::Release);
    }

    /// Write the per-node high-water marks atomically (temp file + rename).
    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let map: std::collections::HashMap<String, i64> = self
            .watermarks
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string(&map)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug)]
struct DaemonError {
    status: StatusCode,
    message: String,
}
impl DaemonError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    fn auth(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg.into(),
        }
    }
}
impl From<anyhow::Error> for DaemonError {
    fn from(v: anyhow::Error) -> Self {
        Self::bad(v.to_string())
    }
}
impl From<std::io::Error> for DaemonError {
    fn from(v: std::io::Error) -> Self {
        Self::bad(v.to_string())
    }
}
impl From<serde_json::Error> for DaemonError {
    fn from(v: serde_json::Error) -> Self {
        Self::bad(v.to_string())
    }
}
impl IntoResponse for DaemonError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(NodeApiResponse::<serde_json::Value>::failure(self.message)),
        )
            .into_response()
    }
}
type DResult<T> = Result<T, DaemonError>;

#[tokio::main]
async fn main() -> Result<()> {
    unsafe {
        libc::umask(0o077);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("voltd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("check-config") | Some("--check-config") => {
            let path = config_arg(&args)?;
            let config = DaemonConfig::load(&path)?;
            let _: SocketAddr = config.listen.parse().context("invalid listen address")?;
            if config.plaintext
                && !voltpanel::node_daemon::listen_is_loopback(&config.listen)
                && !config.allow_http_bind
            {
                bail!(
                    "unsafe plaintext bind: {} would serve the agent API without TLS on {}; re-run `voltd join --listen 127.0.0.1:8081` for loopback-only, or add --allow-http to opt in",
                    path.display(),
                    config.listen
                );
            }
            println!(
                "valid config: {} (listen {})",
                path.display(),
                config.listen
            );
            Ok(())
        }
        Some("join") => join(&args[2..]).await,
        Some("serve") | None => serve(config_arg(&args)?).await,
        Some("help") | Some("--help") | Some("-h") => {
            usage();
            Ok(())
        }
        Some(v) => bail!("unknown command '{v}' (run `voltd help`)"),
    }
}
fn usage() {
    println!("voltd - VoltPanel execution agent\n\n  voltd --version\n  voltd check-config --config FILE\n  voltd join <panel-url> <token> [--public-url URL] [--listen 127.0.0.1:8081] [--data DIR] [--config FILE] [--allow-http] [--plaintext] [--no-start]\n  voltd serve [--config FILE]\n\njoin writes secure agent configuration automatically. The enrollment token may also be supplied via the VOLTD_TOKEN environment variable (argv wins). Plaintext enrollment and non-loopback plaintext agent binds both require --allow-http; without it the agent API defaults to loopback-only. --no-start enrolls without starting the agent.");
}

fn config_arg(args: &[String]) -> Result<PathBuf> {
    option(args, "--config")
        .map(PathBuf::from)
        .or_else(|| std::env::var("VOLTD_CONFIG").ok().map(PathBuf::from))
        .or_else(|| dirs_home().map(|p| p.join(".config/voltpanel/voltd.toml")))
        .context("cannot determine config path")
}
fn option(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|v| v[0] == name).map(|v| v[1].clone())
}
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// HTTP client for panel-bound requests.
///
/// When the panel runs its own self-signed certificate the operator passes its
/// fingerprint, and the agent trusts exactly that certificate; otherwise the
/// normal WebPKI roots apply and a real domain certificate validates as usual.
fn panel_client(panel_fingerprint: &str, timeout_secs: u64) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(timeout_secs));
    let fp = voltpanel::tls::normalize_fingerprint(panel_fingerprint);
    if fp.is_empty() {
        return Ok(builder.build()?);
    }
    let cfg = voltpanel::tls::pinned_client_config(&fp)?;
    Ok(builder.use_preconfigured_tls((*cfg).clone()).build()?)
}

/// Resolve the enrollment token without mistaking the first option for a
/// positional token. The explicit positional form is only valid immediately
/// after the panel URL; otherwise the environment fallback is used.
fn enrollment_token(args: &[String], env: Option<String>) -> Option<String> {
    args.get(1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .or(env)
}

async fn join(args: &[String]) -> Result<()> {
    let panel_url = args
        .first()
        .context("missing panel URL")?
        .trim_end_matches('/')
        .to_string();
    let parsed_panel = url::Url::parse(&panel_url).context("invalid panel URL")?;
    let loopback = parsed_panel
        .host_str()
        .map(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if parsed_panel.scheme() != "https" && !loopback && !args.iter().any(|v| v == "--allow-http") {
        bail!("refusing plaintext enrollment; use https:// or pass --allow-http only on a trusted private network");
    }
    if parsed_panel.scheme() != "https" {
        tracing::warn!(
            "enrolling over plaintext http; the enrollment token and node secret will cross the network unencrypted — the panel must be configured to accept such enrollments, and a MITM on the link can still capture the token (prefer https://)"
        );
    }
    let token = enrollment_token(args, std::env::var("VOLTD_TOKEN").ok())
        .context("missing enrollment token")?;
    // The agent is outbound-only: it never binds a public API or presents an
    // endpoint certificate. Legacy flags remain parseable in stored configs,
    // but enrollment always advertises the empty outbound endpoint identity.
    let plaintext = true;
    let listen = "127.0.0.1:8081".to_string();
    let allow_http_bind = false;
    let data_dir = option(args, "--data")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|p| p.join(".local/share/voltd")))
        .context("cannot determine data directory")?;
    let config_path = config_arg(args)?;
    let public_url = String::new();
    let panel_fingerprint = option(args, "--panel-fingerprint")
        .map(|f| voltpanel::tls::normalize_fingerprint(&f))
        .unwrap_or_default();
    let heartbeat = heartbeat_value(&DaemonRuntime::new(DaemonConfig {
        listen: listen.clone(),
        data_dir: data_dir.clone(),
        panel_url: panel_url.clone(),
        public_url: String::new(),
        node_id: String::new(),
        secret: String::new(),
        heartbeat_interval_secs: 15,
        max_upload_mb: 256,
        plaintext,
        allow_http_bind,
        admin_terminal: args.iter().any(|value| value == "--enable-admin-terminal"),
        panel_fingerprint: panel_fingerprint.clone(),
    })?);
    let client = panel_client(&panel_fingerprint, 20)?;
    let response = client
        .post(format!("{panel_url}/api/node/enroll"))
        .json(&serde_json::json!({
            "token": token,
            "heartbeat": heartbeat,
        }))
        .send()
        .await?;
    let status = response.status();
    let body: serde_json::Value = response.json().await?;
    if !status.is_success() {
        bail!(
            "panel rejected enrollment: {}",
            body.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
        );
    }
    let node_id = body["node_id"]
        .as_str()
        .context("panel response missing node_id")?
        .to_string();
    let secret = body["secret"]
        .as_str()
        .context("panel response missing secret")?
        .to_string();
    let interval = body["heartbeat_interval_secs"].as_u64().unwrap_or(15);
    let config = DaemonConfig {
        listen,
        data_dir,
        panel_url,
        public_url: public_url.clone(),
        node_id,
        secret,
        heartbeat_interval_secs: interval,
        max_upload_mb: 256,
        plaintext,
        allow_http_bind,
        admin_terminal: args.iter().any(|value| value == "--enable-admin-terminal"),
        panel_fingerprint,
    };
    config.save(&config_path)?;
    println!(
        "Node enrolled successfully.\n  config: {}\n  mode: outbound-only\n  node id: {}",
        config_path.display(),
        config.node_id
    );
    if args.iter().any(|v| v == "--no-start") {
        return Ok(());
    }
    println!("Starting execution agent...");
    serve(config_path).await
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let config = DaemonConfig::load(&config_path)
        .with_context(|| format!("run `voltd join` first or create {}", config_path.display()))?;
    if config.node_id.is_empty() || config.secret.is_empty() {
        bail!("execution agent is not enrolled; run `voltd join`");
    }
    if config.plaintext
        && !voltpanel::node_daemon::listen_is_loopback(&config.listen)
        && !config.allow_http_bind
    {
        bail!(
            "refusing to serve the plaintext agent API on non-loopback {} without an explicit opt-in; re-run `voltd join --listen 127.0.0.1:8081` for loopback-only, or pass --allow-http to bind plaintext on a trusted network",
            config.listen
        );
    }
    let max_body = config
        .max_upload_mb
        .saturating_mul(1_048_576)
        .saturating_mul(2)
        .min(usize::MAX as u64) as usize;
    let runtime = DaemonRuntime::new(config)?;
    recover_agent_update(&runtime.config.data_dir)?;
    let nonces = Arc::new(NonceStore::new(Some(
        runtime.config.data_dir.join("replay-watermark.json"),
    )));
    let state = DaemonState {
        runtime: runtime.clone(),
        nonces: nonces.clone(),
    };
    // Persist the replay watermark on a timer so a crash loses at most the
    // accepts since the last write (bounded by the interval); the final write
    // happens after the server stops below.
    let persist_nonces = nonces.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                NONCE_PRUNE_INTERVAL_SECS as u64,
            ))
            .await;
            if let Err(e) = persist_nonces.persist() {
                tracing::warn!("failed to persist replay watermark: {e}");
            }
        }
    });
    tokio::spawn(heartbeat_loop(runtime.clone()));
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/servers", post(provision))
        .route("/v1/servers/:uuid", delete(remove))
        .route("/v1/servers/:uuid/console/clear", post(clear_console))
        .route("/v1/servers/:uuid/power", post(power))
        .route("/v1/servers/:uuid/stats", get(stats))
        .route("/v1/servers/:uuid/command", post(command))
        .route("/v1/servers/:uuid/install", post(install))
        .route("/v1/servers/:uuid/files/operation", post(file_operation))
        .route("/v1/servers/:uuid/console", get(console))
        .route("/v1/servers/:uuid/files", get(files))
        .route(
            "/v1/servers/:uuid/files/content",
            get(read_file).post(write_file),
        )
        .route(
            "/v1/servers/:uuid/snapshot",
            get(snapshot).post(restore_snapshot),
        )
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            sign_responses,
        ))
        .with_state(state);
    tokio::spawn(command_channel_loop(runtime.clone(), app.clone()));
    // No inbound listener: every panel command enters through the authenticated
    // outbound WebSocket and dispatches into the same Axum router in-process.
    tracing::info!("voltd {} running outbound-only", runtime.config.node_id);
    shutdown(runtime).await;
    nonces.persist()?;
    Ok(())
}

async fn shutdown(runtime: DaemonRuntime) {
    let notify = runtime.shutdown_notify();
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
            _ = notify.notified() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = notify.notified() => {},
    }
    // Graceful tenant shutdown: SIGTERM every running server process group
    // (stop_command first when configured), wait up to the grace window for
    // them to exit, then force-kill stragglers. PDEATHSIG remains the crash
    // backstop only — a graceful daemon stop must not SIGKILL every tenant.
    runtime
        .shutdown_servers(std::time::Duration::from_secs(10))
        .await;
}
fn update_paths(data_dir: &std::path::Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| anyhow::anyhow!("agent binary has no parent"))?;
    Ok((
        parent.join(".voltd.candidate"),
        parent.join("voltd.stable"),
        data_dir.join("updates/pending"),
    ))
}

fn recover_agent_update(data_dir: &std::path::Path) -> Result<()> {
    let (_, stable, pending) = update_paths(data_dir)?;
    if !pending.exists() || !stable.exists() {
        return Ok(());
    }
    let attempts = std::fs::read_to_string(&pending)
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0);
    if attempts == 0 {
        std::fs::write(&pending, b"1")?;
        return Ok(());
    }
    let current = std::env::current_exe()?;
    let failed = current.with_file_name("voltd.failed");
    let _ = std::fs::remove_file(&failed);
    std::fs::rename(&current, &failed)?;
    std::fs::rename(&stable, &current)?;
    std::fs::remove_file(&pending)?;
    tracing::error!("updated agent failed before health confirmation; restored stable binary");
    std::process::exit(75)
}

fn rollback_agent(data_dir: &std::path::Path) -> Result<()> {
    let (_, stable, pending) = update_paths(data_dir)?;
    if !stable.exists() {
        bail!("no stable agent binary is available")
    }
    let current = std::env::current_exe()?;
    let failed = current.with_file_name("voltd.rolled-back");
    let _ = std::fs::remove_file(&failed);
    std::fs::rename(&current, &failed)?;
    std::fs::rename(&stable, &current)?;
    let _ = std::fs::remove_file(pending);
    std::process::exit(75)
}

async fn apply_agent_update(runtime: &DaemonRuntime, manifest: AgentUpdateManifest) -> Result<()> {
    use sha2::{Digest, Sha256};
    node_protocol::verify_update_manifest(&runtime.config.secret, &manifest)?;
    if manifest.version == env!("CARGO_PKG_VERSION") {
        return Ok(());
    }
    let url = url::Url::parse(&manifest.url)?;
    if url.scheme() != "https" {
        bail!("agent update URL must use HTTPS")
    }
    let client = panel_client(&runtime.config.panel_fingerprint, 120)?;
    let response = client.get(url).send().await?.error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.len() > 128 * 1024 * 1024 {
        bail!("agent update binary exceeds 128 MiB")
    }
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != manifest.sha256.to_ascii_lowercase() {
        bail!("agent update checksum mismatch")
    }
    let (candidate, stable, pending) = update_paths(&runtime.config.data_dir)?;
    std::fs::create_dir_all(candidate.parent().expect("update parent"))?;
    std::fs::write(&candidate, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755))?;
    }
    let output = tokio::process::Command::new(&candidate)
        .arg("--version")
        .output()
        .await?;
    let identity = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || identity.trim() != format!("voltd {}", manifest.version) {
        bail!("agent update identity check failed")
    }
    let current = std::env::current_exe()?;
    std::fs::copy(&current, &stable)?;
    if let Some(parent) = pending.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pending, b"0")?;
    std::fs::rename(&candidate, &current)?;
    // systemd restarts the process; pending is cleared only after the new
    // binary reconnects and completes its first health cycle.
    std::process::exit(75)
}

async fn command_channel_loop(runtime: DaemonRuntime, app: Router) {
    let mut delay = std::time::Duration::from_secs(1);
    loop {
        let connected_at = std::time::Instant::now();
        if let Err(error) = command_channel_session(&runtime, app.clone()).await {
            tracing::warn!("command channel disconnected: {error}");
        }
        if connected_at.elapsed() >= std::time::Duration::from_secs(30) {
            delay = std::time::Duration::from_secs(1);
        } else {
            delay = (delay * 2).min(std::time::Duration::from_secs(30));
        }
        tokio::time::sleep(delay).await;
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    Ok((output, truncated))
}

async fn execute_terminal_command(body: &[u8]) -> Result<NodeTerminalResponse> {
    const MAX_OUTPUT: usize = 256 * 1024;
    let request: NodeTerminalRequest = serde_json::from_slice(body)?;
    let correlation_id = request.correlation_id.clone();
    if request.correlation_id.len() != 32
        || !request
            .correlation_id
            .bytes()
            .all(|value| value.is_ascii_hexdigit())
    {
        bail!("terminal correlation_id must be 32 hex characters")
    }
    if request.command.trim().is_empty() || request.command.len() > 8192 {
        bail!("terminal command must contain 1..8192 bytes")
    }
    let timeout = std::time::Duration::from_secs(request.timeout_secs.clamp(1, 120));
    let mut command = tokio::process::Command::new("/bin/bash");
    command
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(&request.command)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .current_dir("/")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command.as_std_mut(), 0);
    let mut child = command.spawn()?;
    let child_pid = child.id();
    let stdout = child.stdout.take().context("terminal stdout unavailable")?;
    let stderr = child.stderr.take().context("terminal stderr unavailable")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_OUTPUT));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_OUTPUT));
    let timed_out = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => {
            result?;
            false
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                if let Ok(pid) = i32::try_from(pid) {
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            true
        }
    };
    let (stdout, stdout_truncated) = stdout_task.await??;
    let (stderr, stderr_truncated) = stderr_task.await??;
    let status = child.try_wait()?;
    Ok(NodeTerminalResponse {
        exit_code: status.and_then(|value| value.code()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: if timed_out && stderr.is_empty() {
            "command timed out; process group killed".into()
        } else {
            String::from_utf8_lossy(&stderr).into_owned()
        },
        correlation_id,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

fn terminal_slots(enabled: bool) -> Arc<tokio::sync::Semaphore> {
    Arc::new(tokio::sync::Semaphore::new(if enabled { 2 } else { 0 }))
}

async fn command_channel_session(runtime: &DaemonRuntime, app: Router) -> Result<()> {
    let panel = runtime.config.panel_url.trim_end_matches('/');
    let ws_url = if let Some(rest) = panel.strip_prefix("https://") {
        format!("wss://{rest}/api/node/channel")
    } else if let Some(rest) = panel.strip_prefix("http://") {
        format!("ws://{rest}/api/node/channel")
    } else {
        bail!("panel URL must use http or https")
    };
    let signed = node_protocol::sign(
        &runtime.config.secret,
        "GET",
        "/api/node/channel",
        &[],
        &runtime.config.node_id,
    )?;
    let mut request = ws_url.into_client_request()?;
    for (name, value) in [
        (node_protocol::NODE_ID_HEADER, signed.node_id.as_str()),
        (
            node_protocol::TIMESTAMP_HEADER,
            &signed.timestamp.to_string(),
        ),
        (node_protocol::NONCE_HEADER, signed.nonce.as_str()),
        (node_protocol::SIGNATURE_HEADER, signed.signature.as_str()),
    ] {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes())?,
            axum::http::HeaderValue::from_str(value)?,
        );
    }
    let fingerprint = voltpanel::tls::normalize_fingerprint(&runtime.config.panel_fingerprint);
    let connector = if fingerprint.is_empty() {
        None
    } else {
        Some(tokio_tungstenite::Connector::Rustls(
            voltpanel::tls::pinned_client_config(&fingerprint)?,
        ))
    };
    let (socket, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector).await?;
    tracing::info!("outbound command channel connected");
    let (mut sink, mut source) = socket.split();
    let (_, _, pending) = update_paths(&runtime.config.data_dir)?;
    if pending.exists() {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Err(error) = std::fs::remove_file(&pending) {
                tracing::warn!("failed to confirm healthy agent update: {error}");
            } else {
                tracing::info!("agent update confirmed healthy; rollback marker cleared");
            }
        });
    }
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<ChannelResponse>(64);
    let terminal_slots = terminal_slots(runtime.config.admin_terminal);
    loop {
        tokio::select! {
            response = response_rx.recv() => {
                let Some(response) = response else { break };
                sink.send(WsMessage::Text(serde_json::to_string(&response)?)).await?;
            }
            message = source.next() => {
                let Some(message) = message else { break };
                match message? {
                    WsMessage::Text(text) => {
                        let request: ChannelRequest = serde_json::from_str(&text)?;
                        if request.method == "ROLLBACK" && request.path == "/v1/agent/update" {
                            rollback_agent(&runtime.config.data_dir)?;
                        }
                        if request.method == "UPDATE" && request.path == "/v1/agent/update" {
                            let body = base64::engine::general_purpose::STANDARD.decode(&request.body_b64)?;
                            let manifest: AgentUpdateManifest = serde_json::from_slice(&body)?;
                            apply_agent_update(runtime, manifest).await?;
                            continue;
                        }
                        let tx = response_tx.clone();
                        if request.method == "TERMINAL" && request.path == "/v1/agent/terminal" {
                            let permit = terminal_slots.clone().try_acquire_owned();
                            let Ok(permit) = permit else {
                                let _ = tx.send(ChannelResponse { id: request.id, status: 429, body_b64: String::new(), error: "too many concurrent terminal commands".into() }).await;
                                continue;
                            };
                            tokio::spawn(async move {
                                let _permit = permit;
                                let id = request.id;
                                let response = match base64::engine::general_purpose::STANDARD.decode(&request.body_b64) {
                                    Ok(body) => match execute_terminal_command(&body).await {
                                        Ok(value) => ChannelResponse { id, status: 200, body_b64: base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&value).unwrap_or_default()), error: String::new() },
                                        Err(error) => ChannelResponse { id, status: 400, body_b64: String::new(), error: error.to_string() },
                                    },
                                    Err(error) => ChannelResponse { id, status: 400, body_b64: String::new(), error: error.to_string() },
                                };
                                let _ = tx.send(response).await;
                            });
                            continue;
                        }
                        let app = app.clone();
                        let secret = runtime.config.secret.clone();
                        let node_id = runtime.config.node_id.clone();
                        tokio::spawn(async move {
                            let response = execute_channel_request(app, request, &secret, &node_id).await;
                            let _ = tx.send(response).await;
                        });
                    }
                    WsMessage::Ping(payload) => sink.send(WsMessage::Pong(payload)).await?,
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    bail!("panel closed command channel")
}
async fn execute_channel_request(
    app: Router,
    request: ChannelRequest,
    secret: &str,
    node_id: &str,
) -> ChannelResponse {
    let id = request.id.clone();
    let result = async {
        let method = axum::http::Method::from_bytes(request.method.as_bytes())?;
        let body = base64::engine::general_purpose::STANDARD.decode(&request.body_b64)?;
        let path = request.path.parse::<axum::http::Uri>()?;
        let signed =
            node_protocol::sign(secret, method.as_str(), &path.to_string(), &body, node_id)?;
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(node_protocol::NODE_ID_HEADER, node_id)
            .header(
                node_protocol::TIMESTAMP_HEADER,
                signed.timestamp.to_string(),
            )
            .header(node_protocol::NONCE_HEADER, signed.nonce)
            .header(node_protocol::SIGNATURE_HEADER, signed.signature)
            .body(Body::from(body))?;
        let response = app.oneshot(req).await?;
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await?.to_bytes();
        Ok::<_, anyhow::Error>((status, bytes))
    }
    .await;
    match result {
        Ok((status, bytes)) => ChannelResponse {
            id,
            status,
            body_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
            error: String::new(),
        },
        Err(error) => ChannelResponse {
            id,
            status: 500,
            body_b64: String::new(),
            error: error.to_string(),
        },
    }
}

async fn heartbeat_loop(runtime: DaemonRuntime) {
    let client = match panel_client(&runtime.config.panel_fingerprint, 15) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("heartbeat client: {e}");
            return;
        }
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(
        runtime.config.heartbeat_interval_secs.max(5),
    ));
    loop {
        tick.tick().await;
        let value = heartbeat_value(&runtime);
        let body = match serde_json::to_vec(&value) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("heartbeat encode: {e}");
                continue;
            }
        };
        let signed = match node_protocol::sign(
            &runtime.config.secret,
            "POST",
            "/api/node/heartbeat",
            &body,
            &runtime.config.node_id,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("heartbeat sign: {e}");
                continue;
            }
        };
        let result = client
            .post(format!(
                "{}/api/node/heartbeat",
                runtime.config.panel_url.trim_end_matches('/')
            ))
            .header(node_protocol::NODE_ID_HEADER, &signed.node_id)
            .header(
                node_protocol::TIMESTAMP_HEADER,
                signed.timestamp.to_string(),
            )
            .header(node_protocol::NONCE_HEADER, &signed.nonce)
            .header(node_protocol::SIGNATURE_HEADER, &signed.signature)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await;
        match result {
            Ok(v) if v.status().is_success() => {}
            Ok(v) => tracing::warn!("heartbeat rejected: {}", v.status()),
            Err(e) => tracing::warn!("heartbeat failed: {e}"),
        }
    }
}

fn heartbeat_value(runtime: &DaemonRuntime) -> NodeHeartbeat {
    NodeHeartbeat {
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        hostname: hostname::get()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        started_at: runtime.started_at.to_rfc3339(),
        capacity: runtime.capacity(),
        tls_fingerprint: String::new(),
        admin_terminal: runtime.config.admin_terminal,
    }
}

async fn authenticated(
    state: &DaemonState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
) -> DResult<()> {
    let node_id = h(headers, node_protocol::NODE_ID_HEADER)?;
    if node_id != state.runtime.config.node_id {
        return Err(DaemonError::auth("wrong node identity"));
    }
    let signed = SignedHeaders {
        node_id,
        timestamp: h(headers, node_protocol::TIMESTAMP_HEADER)?
            .parse()
            .map_err(|_| DaemonError::auth("bad timestamp"))?,
        nonce: h(headers, node_protocol::NONCE_HEADER)?,
        signature: h(headers, node_protocol::SIGNATURE_HEADER)?,
    };
    let now = chrono::Utc::now().timestamp();
    node_protocol::verify(
        &state.runtime.config.secret,
        method,
        path,
        body,
        &signed,
        now,
    )
    .map_err(|e| DaemonError::auth(e.to_string()))?;
    if state
        .nonces
        .check_and_insert(&signed.node_id, &signed.nonce, signed.timestamp, now)
    {
        return Err(DaemonError::auth("replayed request"));
    }
    let rid = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.chars()
                .filter(|c| c.is_ascii_hexdigit() || *c == '-')
                .take(64)
                .collect::<String>()
        })
        .filter(|s| s.len() >= 8);
    match rid {
        Some(r) => tracing::info!(request_id = %r, path = %path, "request authenticated"),
        None => tracing::info!(path = %path, "request authenticated"),
    }
    Ok(())
}
fn h(headers: &HeaderMap, key: &str) -> DResult<String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or_else(|| DaemonError::auth(format!("missing {key}")))
}

/// Sign every response with the shared secret before it leaves the daemon,
/// closing the unauthenticated panel<-node channel: without this, a MITM on a
/// plaintext link can forge any envelope (provision success, stats, file
/// contents, snapshot) because the panel parses the envelope without checking
/// an HMAC.
///
/// The canonical string covers `NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY)`
/// where the nonce is echoed from the request and the body is the handler's
/// envelope serialized WITHOUT the `signature`/`nonce` fields (the constructors
/// emit them only after signing, as `None`). The signature is appended to the
/// raw envelope JSON so the signed bytes are byte-exact on the wire.
///
/// Only requests carrying a request nonce get signed responses: that is every
/// request the panel makes (`sign()` always mints one) and exactly the set the
/// panel will verify. Requests without a nonce (unauthenticated probes) get
/// plain unsigned error envelopes, which the panel never parses as its own.
async fn sign_responses(State(state): State<DaemonState>, req: Request, next: Next) -> Response {
    // The raw request URI (path + query, exactly as the panel signed it) and
    // the request method must be echoed into the response canonical string.
    let path = req.uri().to_string();
    let method = req.method().as_str().to_string();
    let nonce = req
        .headers()
        .get(node_protocol::NONCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned);
    // A streaming request (raw snapshot/restore bodies) gets the capability
    // header echoed on every response so the panel can tell a streaming agent
    // from a pre-streaming one even on error envelopes.
    let streaming = req.headers().contains_key(node_protocol::STREAM_HEADER);
    let resp = next.run(req).await;
    let Some(nonce) = nonce else {
        return resp;
    };
    let status = resp.status().as_u16();
    let (mut parts, body) = resp.into_parts();
    // A streaming snapshot response is already signed by its handler (the
    // HMAC rides in a tail footer of the raw body, which this middleware
    // cannot buffer without losing the entire point of streaming). The
    // discriminator is the `x-volt-stream` response header alongside the
    // octet-stream content type: restore responses also carry the header (a
    // capability echo) but are small JSON envelopes that must still be
    // signed here.
    if parts.headers.contains_key(node_protocol::STREAM_HEADER)
        && parts
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("application/octet-stream"))
    {
        return Response::from_parts(parts, body);
    }
    if streaming {
        parts.headers.insert(
            node_protocol::STREAM_HEADER,
            axum::http::HeaderValue::from_static("1"),
        );
    }
    // The rebuilt body is complete bytes; a content-length computed for the
    // pre-signature envelope would truncate or corrupt the longer wire form,
    // so drop any stale header and let hyper recompute it from the body.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        tracing::warn!("response body unreadable; leaving response unsigned");
        return Response::from_parts(parts, Body::empty());
    };
    match sign_response_body(&state, &method, &path, status, &nonce, &bytes) {
        Some(signed) => Response::from_parts(parts, Body::from(signed)),
        // Not a JSON object envelope (should not happen on these routes):
        // pass the body through untouched rather than mangling it.
        None => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// Append `signature`/`nonce` to the envelope bytes, signing exactly the
/// handler's serialization. Returns None when the body is not a JSON object
/// ending in `}` (nothing to append to).
fn sign_response_body(
    state: &DaemonState,
    method: &str,
    path: &str,
    status: u16,
    nonce: &str,
    body: &[u8],
) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    if !s.ends_with('}') {
        return None;
    }
    let signature = node_protocol::sign_response(
        &state.runtime.config.secret,
        method,
        path,
        status,
        nonce,
        body,
        &state.runtime.config.node_id,
    )
    .ok()?;
    Some(format!(
        "{},\"signature\":\"{}\",\"nonce\":\"{}\"}}",
        &s[..s.len() - 1],
        signature,
        nonce
    ))
}

async fn health(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<serde_json::Value>>> {
    authenticated(&state, &headers, "GET", "/v1/health", &[]).await?;
    Ok(Json(NodeApiResponse::success(
        serde_json::json!({ "node_id": state.runtime.config.node_id, "version": env!("CARGO_PKG_VERSION"), "capacity": state.runtime.capacity(), "isolation": voltpanel::isolation::probe(&voltpanel::isolation::IsolationConfig::default()) }),
    )))
}
async fn provision(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<voltpanel::node_protocol::RemoteServerStats>>> {
    authenticated(&state, &headers, "POST", "/v1/servers", &body).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.provision(serde_json::from_slice(&body)?)?,
    )))
}
async fn remove(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}");
    authenticated(&state, &headers, "DELETE", &path, &[]).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.remove_server(&uuid)?,
    )))
}
async fn power(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<voltpanel::node_protocol::RemoteServerStats>>> {
    let path = format!("/v1/servers/{uuid}/power");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    let req: PowerRequest = serde_json::from_slice(&body)?;
    let value = match req.action {
        PowerAction::Start => state.runtime.start(&uuid)?,
        PowerAction::Stop => state.runtime.stop(&uuid, false)?,
        PowerAction::Kill => state.runtime.stop(&uuid, true)?,
        PowerAction::Restart => state.runtime.restart(&uuid).await?,
    };
    Ok(Json(NodeApiResponse::success(value)))
}
async fn install(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/install");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    let req: voltpanel::node_protocol::InstallRequest = serde_json::from_slice(&body)?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.install(&uuid, &req.script)?,
    )))
}

async fn stats(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<voltpanel::node_protocol::RemoteServerStats>>> {
    let path = format!("/v1/servers/{uuid}/stats");
    authenticated(&state, &headers, "GET", &path, &[]).await?;
    Ok(Json(NodeApiResponse::success(state.runtime.stats(&uuid)?)))
}
async fn command(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/command");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    let req: ConsoleCommand = serde_json::from_slice(&body)?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.command(&uuid, &req.command).await?,
    )))
}
#[derive(Deserialize)]
struct CursorQuery {
    #[serde(default)]
    cursor: u64,
}
async fn console(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    Query(q): Query<CursorQuery>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<ConsoleSnapshot>>> {
    let path = format!("/v1/servers/{uuid}/console?cursor={}", q.cursor);
    authenticated(&state, &headers, "GET", &path, &[]).await?;
    let (lines, cursor) = state.runtime.console(&uuid, q.cursor)?;
    Ok(Json(NodeApiResponse::success(ConsoleSnapshot {
        lines,
        cursor,
    })))
}
async fn clear_console(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/console/clear");
    authenticated(&state, &headers, "POST", &path, &[]).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.clear_console(&uuid)?,
    )))
}
#[derive(Deserialize)]
struct PathQuery {
    #[serde(default = "root_path")]
    path: String,
}
fn root_path() -> String {
    "/".into()
}
async fn files(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<Vec<voltpanel::node_protocol::RemoteFileEntry>>>> {
    let encoded: String = url::form_urlencoded::byte_serialize(q.path.as_bytes()).collect();
    let path = format!("/v1/servers/{uuid}/files?path={encoded}");
    authenticated(&state, &headers, "GET", &path, &[]).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.list_files(&uuid, &q.path)?,
    )))
}
async fn snapshot(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/v1/servers/{uuid}/snapshot");
    if let Err(error) = authenticated(&state, &headers, "GET", &path, &[]).await {
        return error.into_response();
    }
    if headers.contains_key(node_protocol::STREAM_HEADER) {
        return stream_snapshot(&state, &uuid, &path, &headers);
    }
    match state.runtime.snapshot(&uuid) {
        Ok(snap) => Json(NodeApiResponse::success(snap)).into_response(),
        Err(error) => DaemonError::from(error).into_response(),
    }
}

/// Stream a server's snapshot archive (tar.gz) directly into the response
/// body, signed with a tail footer instead of the JSON envelope: the archive
/// is produced and transmitted in one bounded pass, so the response never
/// materializes in RAM. The footer carries an HMAC over
/// `NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(ARCHIVE)` — the same
/// canonical string the envelope path signs — so the panel's verification
/// guarantees are unchanged. The request nonce is bound cryptographically
/// (it is an HMAC input) rather than echoed on the wire: a captured footer
/// cannot verify against any other request.
///
/// The per-server snapshot lock is held inside the producer thread for the
/// whole production run, exactly as the buffered path holds it; a concurrent
/// snapshot/restore on the same node can never interleave with the walk.
fn stream_snapshot(state: &DaemonState, uuid: &str, path: &str, headers: &HeaderMap) -> Response {
    let Some(nonce) = headers
        .get(node_protocol::NONCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
    else {
        return DaemonError::auth("missing nonce").into_response();
    };
    let Some(proc) = state.runtime.process(uuid).ok() else {
        return DaemonError::bad("unknown server").into_response();
    };
    // Common-case early-out so "server must be stopped" returns a clean
    // signed envelope; the producer re-checks under the snapshot lock to
    // close the race with a concurrent start.
    if proc.pid.lock().is_some() {
        return Json(NodeApiResponse::<bool>::failure(
            "server must be stopped before snapshot",
        ))
        .into_response();
    }
    let runtime = state.runtime.clone();
    let secret = runtime.config.secret.clone();
    let node_id = runtime.config.node_id.clone();
    let uuid = uuid.to_string();
    let path = path.to_string();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, anyhow::Error>>(8);
    tokio::task::spawn_blocking(move || {
        let _snapshot = proc.snapshot_lock.lock();
        if proc.pid.lock().is_some() {
            let _ = tx
                .blocking_send(Err(anyhow::anyhow!(
                    "server must be stopped before snapshot"
                )))
                .is_ok();
            return;
        }
        let mut sink = ChunkSink {
            tx,
            buf: Vec::with_capacity(64 * 1024),
        };
        match runtime.write_archive_to(&uuid, &mut sink) {
            Ok((_size, sha)) => {
                match node_protocol::sign_stream_response(
                    &secret, "GET", &path, 200, &nonce, &sha, &node_id,
                ) {
                    Ok(signature) => {
                        // The hash and signature are complete exactly when the
                        // archive ends; the footer rides as the final body
                        // chunk (see node_protocol::STREAM_FOOTER_LEN).
                        let _ =
                            sink.tx
                                .blocking_send(Ok(Bytes::from(node_protocol::stream_footer(
                                    &signature,
                                ))));
                    }
                    Err(error) => {
                        let _ = sink.tx.blocking_send(Err(error));
                    }
                }
            }
            Err(error) => {
                let _ = sink.tx.blocking_send(Err(error));
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        node_protocol::STREAM_HEADER,
        axum::http::HeaderValue::from_static("1"),
    );
    response
}

async fn restore_snapshot(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    req: Request,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/snapshot");
    if headers.contains_key(node_protocol::STREAM_HEADER) {
        return restore_snapshot_streaming(&state, &uuid, &path, &headers, req).await;
    }
    // Legacy: the request body is the base64 JSON envelope. The `Request`
    // extractor bypasses DefaultBodyLimit, so enforce the same cap here.
    let max_body = state
        .runtime
        .config
        .max_upload_mb
        .saturating_mul(1_048_576)
        .saturating_mul(2)
        .min(usize::MAX as u64) as usize;
    let body = axum::body::to_bytes(req.into_body(), max_body)
        .await
        .map_err(|e| DaemonError::bad(e.to_string()))?;
    authenticated(&state, &headers, "POST", &path, &body).await?;
    Ok(Json(NodeApiResponse::success(
        state
            .runtime
            .restore_snapshot(&uuid, serde_json::from_slice(&body)?)?,
    )))
}

/// Streaming restore: the request body is the raw tar.gz archive, consumed
/// straight into the decoder (never buffered). The request signature is
/// verified in two stages — structural checks + replay rejection up front,
/// then the body MAC once the archive has been fully consumed — because the
/// MAC covers SHA-256 of the whole body, which is only known at the end. The
/// archive is applied (atomically swapped into the live dir) only after the
/// MAC passes; a failed or forged stream leaves the live dir untouched.
async fn restore_snapshot_streaming(
    state: &DaemonState,
    uuid: &str,
    path: &str,
    headers: &HeaderMap,
    req: Request,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let now = chrono::Utc::now().timestamp();
    let signed = SignedHeaders {
        node_id: h(headers, node_protocol::NODE_ID_HEADER)?,
        timestamp: h(headers, node_protocol::TIMESTAMP_HEADER)?
            .parse()
            .map_err(|_| DaemonError::auth("bad timestamp"))?,
        nonce: h(headers, node_protocol::NONCE_HEADER)?,
        signature: h(headers, node_protocol::SIGNATURE_HEADER)?,
    };
    let pending = node_protocol::verify_pending(&signed, now)
        .map_err(|e| DaemonError::auth(e.to_string()))?;
    if state
        .nonces
        .check_and_insert(&pending.node_id, &pending.nonce, pending.timestamp, now)
    {
        return Err(DaemonError::auth("replayed request"));
    }
    let body = req.into_body();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, anyhow::Error>>(8);
    let runtime = state.runtime.clone();
    let secret = runtime.config.secret.clone();
    let method = "POST";
    let path = path.to_string();
    let uuid = uuid.to_string();
    let pending = pending.clone();
    let extractor = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let reader = BodyChannelReader {
            rx,
            buf: Bytes::new(),
            pos: 0,
        };
        runtime.restore_snapshot_stream(&uuid, reader, move |sha| {
            node_protocol::complete_verify(&secret, method, &path, &pending, &sha)
        })
    });
    // Pump the async request body into the extractor's channel. When the
    // extractor bails early (cap, corruption), the channel closes and the
    // pump stops; the response may then be a connection error instead of an
    // envelope, which the panel treats as a failed restore either way.
    let mut data = body.into_data_stream();
    while let Some(chunk) = futures::StreamExt::next(&mut data).await {
        match chunk {
            Ok(bytes) => {
                if tx.send(Ok(bytes)).await.is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(anyhow::anyhow!(error))).await;
                break;
            }
        }
    }
    drop(tx);
    match extractor.await {
        Ok(Ok(ok)) => Ok(Json(NodeApiResponse::success(ok))),
        Ok(Err(error)) => Err(DaemonError::bad(error.to_string())),
        Err(join) => Err(DaemonError::bad(format!("restore worker failed: {join}"))),
    }
}

/// `Write` adapter that forwards the gzip archive to the response-body channel
/// in bounded chunks (64 KiB). The producer thread writes through this; the
/// async side wraps the channel receiver in a `Body`.
struct ChunkSink {
    tx: tokio::sync::mpsc::Sender<Result<Bytes, anyhow::Error>>,
    buf: Vec<u8>,
}

impl std::io::Write for ChunkSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 64 * 1024 {
            let chunk = std::mem::take(&mut self.buf);
            self.tx
                .blocking_send(Ok(Bytes::from(chunk)))
                .map_err(std::io::Error::other)?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            self.tx
                .blocking_send(Ok(Bytes::from(chunk)))
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    }
}

/// `Read` adapter that pulls the streamed request body from the async pump's
/// channel, so the synchronous restore/extract code sees a plain reader.
struct BodyChannelReader {
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, anyhow::Error>>,
    buf: Bytes,
    pos: usize,
}

impl std::io::Read for BodyChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.pos < self.buf.len() {
                let n = std::cmp::min(out.len(), self.buf.len() - self.pos);
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(Ok(bytes)) => {
                    self.buf = bytes;
                    self.pos = 0;
                }
                Some(Err(error)) => return Err(std::io::Error::other(error)),
                None => return Ok(0),
            }
        }
    }
}
async fn read_file(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> DResult<Json<NodeApiResponse<serde_json::Value>>> {
    let encoded: String = url::form_urlencoded::byte_serialize(q.path.as_bytes()).collect();
    let path = format!("/v1/servers/{uuid}/files/content?path={encoded}");
    authenticated(&state, &headers, "GET", &path, &[]).await?;
    let bytes = state.runtime.read_file(
        &uuid,
        &q.path,
        state.runtime.config.max_upload_mb * 1_048_576,
    )?;
    Ok(Json(NodeApiResponse::success(
        serde_json::json!({ "content_b64": base64::engine::general_purpose::STANDARD.encode(&bytes), "size": bytes.len() }),
    )))
}
async fn file_operation(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<serde_json::Value>>> {
    let path = format!("/v1/servers/{uuid}/files/operation");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    let operation: FileOperation = serde_json::from_slice(&body)?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.file_operation(&uuid, operation)?,
    )))
}

async fn write_file(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/files/content");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    let req: FileWriteRequest = serde_json::from_slice(&body)?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(req.content_b64)
        .map_err(|e| DaemonError::bad(e.to_string()))?;
    Ok(Json(NodeApiResponse::success(
        state
            .runtime
            .write_file(&uuid, &req.path, &data, req.append)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_token_prefers_argv_over_env() {
        let with_argv = vec![
            "https://panel.example".to_string(),
            "argv-token".to_string(),
        ];
        assert_eq!(
            enrollment_token(&with_argv, Some("env-token".to_string())).as_deref(),
            Some("argv-token"),
            "explicit argv token must win over the environment fallback"
        );
        let bare = vec!["https://panel.example".to_string()];
        assert_eq!(
            enrollment_token(&bare, Some("env-token".to_string())).as_deref(),
            Some("env-token"),
            "the environment fallback must kick in when argv has no token"
        );
        assert_eq!(enrollment_token(&bare, None), None);
        let options_only = vec![
            "https://panel.example".to_string(),
            "--public-url".to_string(),
            String::new(),
        ];
        assert_eq!(
            enrollment_token(&options_only, Some("env-token".to_string())).as_deref(),
            Some("env-token"),
            "join options must not be mistaken for a positional token"
        );
    }

    #[test]
    fn duplicate_nonce_rejected_without_a_prune_between() {
        let s = NonceStore::new(None);
        let now = 1_000_000;
        // First insert: `last_prune` starts at 0, so the initial prune runs.
        assert!(!s.check_and_insert("node", "n1", now, now));
        // 5s later the 30s gate has not elapsed: no prune ran, yet the
        // duplicate must still be rejected — replay protection cannot depend
        // on pruning.
        assert!(s.check_and_insert("node", "n1", now, now + 5));
        // Same nonce, valid shifted timestamp: still the same pair, rejected.
        assert!(s.check_and_insert("node", "n1", now + 10, now + 10));
        // A fresh nonce inside the same interval is accepted.
        assert!(!s.check_and_insert("node", "n2", now + 10, now + 10));
    }

    #[test]
    fn stale_nonces_pruned_and_duplicates_still_rejected() {
        let s = NonceStore::new(None);
        let mut now = 1_000_000;
        assert!(!s.check_and_insert("node", "old", now, now));
        // Advance past the prune gate and the replay window: the next insert
        // prunes with floor `now - 2*skew`, dropping the old pair.
        now += NONCE_PRUNE_INTERVAL_SECS + node_protocol::MAX_CLOCK_SKEW_SECS * 2;
        assert!(!s.check_and_insert("node", "new", now, now));
        // Pruned: the stale pair is no longer seen and can be re-recorded.
        assert!(!s.check_and_insert("node", "old", now, now));
        // The fresh pair is still tracked and rejected as a duplicate.
        assert!(s.check_and_insert("node", "new", now, now + 1));
    }

    #[test]
    fn stale_entry_survives_until_gate_and_duplicate_rejected() {
        let s = NonceStore::new(None);
        let now = 1_000_000;
        assert!(!s.check_and_insert("node", "a", now - 5_000, now));
        // 10s later: inside the gate, no prune runs, so the stale entry still
        // blocks its duplicate — identical to the old per-request pruning,
        // which only ever ran immediately before the insert.
        assert!(s.check_and_insert("node", "a", now - 5_000, now + 10));
    }

    #[test]
    fn watermark_survives_restart_and_blocks_pre_restart_captures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watermark.json");
        let s = NonceStore::new(Some(path.clone()));
        let now = 1_000_000;
        // Accept requests; the watermark advances to the newest accepted ts.
        assert!(!s.check_and_insert("node", "n1", now, now));
        assert!(!s.check_and_insert("node", "n2", now + 2, now + 2));
        s.persist().unwrap();

        // Restart: the nonce ledger is gone, the watermark is loaded.
        let s2 = NonceStore::new(Some(path));
        // A capture signed before the watermark (outside the grace window) is
        // a replay even though its nonce is unknown to the fresh ledger.
        assert!(s2.check_and_insert("node", "old", now - 10, now));
        // Fresh requests sign above the watermark and are accepted.
        assert!(!s2.check_and_insert("node", "n3", now + 60, now + 60));
        // Same-second concurrency sits inside the grace window: accepted.
        assert!(!s2.check_and_insert("node", "n4", now + 60, now + 61));
    }

    #[test]
    fn nonce_ledger_capped_and_oldest_evicted() {
        // cap=4, retire = max(4/4, overflow) = 1 entry per sweep.
        let s = NonceStore::with_cap(4);
        let now = 1_000_000;
        for i in 0..4 {
            assert!(!s.check_and_insert("node", &format!("n{i}"), now + i, now + i));
        }
        assert_eq!(s.seen.len(), 4);
        // The fifth insert exceeds the cap and compacts, retiring the oldest
        // pair (n0) while keeping the newest ones.
        assert!(!s.check_and_insert("node", "n4", now + 4, now + 4));
        assert_eq!(s.seen.len(), 4);
        // The evicted pair is no longer seen and can be re-recorded…
        assert!(!s.check_and_insert("node", "n0", now + 5, now + 5));
        // …while the retained pairs still reject their duplicates.
        assert!(s.check_and_insert("node", "n4", now + 6, now + 6));
        assert!(s.check_and_insert("node", "n3", now + 6, now + 6));
        // The hard bound holds across a flood of fresh nonces.
        for i in 0..50 {
            assert!(!s.check_and_insert(
                "node",
                &format!("flood-{i}"),
                now + 100 + i,
                now + 100 + i
            ));
        }
        assert!(s.seen.len() <= 4);
    }

    #[test]
    fn watermark_grace_allows_same_second_requests() {
        let s = NonceStore::new(None);
        let now = 1_000_000;
        assert!(!s.check_and_insert("node", "n1", now, now));
        assert!(!s.check_and_insert("node", "n2", now, now + 1));
        assert!(s.check_and_insert("node", "n3", now - REPLAY_WATERMARK_GRACE_SECS - 1, now + 2));
    }

    fn terminal_request(command: &str, timeout_secs: u64) -> Vec<u8> {
        serde_json::to_vec(&NodeTerminalRequest {
            correlation_id: "0123456789abcdef0123456789abcdef".into(),
            command: command.into(),
            timeout_secs,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn troubleshooting_terminal_captures_output_and_exit() {
        let response = execute_terminal_command(&terminal_request(
            "printf stdout; printf stderr >&2; exit 7",
            5,
        ))
        .await
        .unwrap();
        assert_eq!(response.exit_code, Some(7));
        assert_eq!(response.stdout, "stdout");
        assert_eq!(response.stderr, "stderr");
        assert_eq!(response.correlation_id, "0123456789abcdef0123456789abcdef");
        assert!(!response.timed_out);
        assert!(!response.truncated);
    }

    #[tokio::test]
    async fn troubleshooting_terminal_rejects_empty_command() {
        assert!(execute_terminal_command(&terminal_request("  ", 5))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn troubleshooting_terminal_rejects_bad_correlation() {
        let body = serde_json::to_vec(&NodeTerminalRequest {
            correlation_id: "bad".into(),
            command: "true".into(),
            timeout_secs: 5,
        })
        .unwrap();
        assert!(execute_terminal_command(&body).await.is_err());
    }

    #[tokio::test]
    async fn troubleshooting_terminal_times_out() {
        let started = std::time::Instant::now();
        let response = execute_terminal_command(&terminal_request("sleep 5", 1))
            .await
            .unwrap();
        assert!(response.timed_out);
        assert_eq!(response.exit_code, None);
        assert!(response.stderr.contains("process group killed"));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[tokio::test]
    async fn troubleshooting_terminal_has_two_concurrent_slots() {
        let slots = terminal_slots(true);
        let first = slots.clone().try_acquire_owned().unwrap();
        let second = slots.clone().try_acquire_owned().unwrap();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(first);
        assert!(slots.try_acquire_owned().is_ok());
        drop(second);
    }

    #[tokio::test]
    async fn troubleshooting_terminal_caps_large_output() {
        let response = execute_terminal_command(&terminal_request(
            "head -c 300000 /dev/zero | tr '\\0' x",
            5,
        ))
        .await
        .unwrap();
        assert_eq!(response.stdout.len(), 256 * 1024);
        assert!(response.truncated);
        assert!(!response.timed_out);
    }

    #[tokio::test]
    async fn troubleshooting_terminal_disabled_has_no_slots() {
        assert!(terminal_slots(false).try_acquire_owned().is_err());
    }
    #[tokio::test]
    async fn outbound_channel_dispatches_without_listener() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = DaemonRuntime::new(DaemonConfig {
            data_dir: dir.path().to_path_buf(),
            node_id: "11111111-1111-4111-8111-111111111111".into(),
            secret: "secret-a".into(),
            ..DaemonConfig::default()
        })
        .unwrap();
        let state = DaemonState {
            runtime: runtime.clone(),
            nonces: Arc::new(NonceStore::new(None)),
        };
        let app = Router::new()
            .route("/v1/health", get(health))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                sign_responses,
            ))
            .with_state(state);
        let response = execute_channel_request(
            app,
            ChannelRequest {
                id: "command-1".into(),
                method: "GET".into(),
                path: "/v1/health".into(),
                body_b64: String::new(),
            },
            &runtime.config.secret,
            &runtime.config.node_id,
        )
        .await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(
                &base64::engine::general_purpose::STANDARD
                    .decode(&response.body_b64)
                    .unwrap()
            )
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(response.body_b64)
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(
            body["data"]["node_id"],
            "11111111-1111-4111-8111-111111111111"
        );
    }
}
