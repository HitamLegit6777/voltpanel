//! `voltd` — lightweight VoltPanel node daemon.
//! Commands:
//! - `voltd join <panel-url> <token> [--public-url URL] [--listen ADDR]`
//! - `voltd serve [--config PATH]`
use anyhow::{bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use voltpanel::node_daemon::{DaemonConfig, DaemonRuntime};
use voltpanel::node_protocol::{
    self, ConsoleCommand, ConsoleSnapshot, FileOperation, FileWriteRequest, NodeApiResponse,
    NodeHeartbeat, PowerAction, PowerRequest, RestoreSnapshotRequest, SignedHeaders,
    SnapshotResponse,
};

#[derive(Clone)]
struct DaemonState {
    runtime: DaemonRuntime,
    nonces: Arc<dashmap::DashMap<String, i64>>,
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
    println!("voltd — VoltPanel node daemon\n\n  voltd join <panel-url> <token> [--public-url URL] [--listen 0.0.0.0:8081] [--data DIR] [--config FILE] [--allow-http] [--no-start]\n  voltd serve [--config FILE]\n\njoin writes secure configuration automatically. Non-loopback HTTP enrollment requires --allow-http. --no-start enrolls without starting the server.");
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
    let token = args.get(1).context("missing enrollment token")?.clone();
    let listen = option(args, "--listen").unwrap_or_else(|| "0.0.0.0:8081".into());
    let data_dir = option(args, "--data")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|p| p.join(".local/share/voltd")))
        .context("cannot determine data directory")?;
    let config_path = config_arg(args)?;
    let public_url = option(args, "--public-url").unwrap_or_else(|| {
        let host = local_ip().unwrap_or_else(|| "127.0.0.1".into());
        let port = listen.rsplit_once(':').map(|(_, p)| p).unwrap_or("8081");
        format!("http://{host}:{port}")
    });
    let heartbeat = heartbeat_value(&DaemonRuntime::new(DaemonConfig {
        listen: listen.clone(),
        data_dir: data_dir.clone(),
        panel_url: panel_url.clone(),
        node_id: String::new(),
        secret: String::new(),
        heartbeat_interval_secs: 15,
        max_upload_mb: 256,
    })?);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let response = client
        .post(format!("{panel_url}/api/node/enroll"))
        .json(&serde_json::json!({ "token": token, "heartbeat": heartbeat }))
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
        node_id,
        secret,
        heartbeat_interval_secs: interval,
        max_upload_mb: 256,
    };
    config.save(&config_path)?;
    println!(
        "Node enrolled successfully.\n  config: {}\n  public URL: {}\n  node id: {}",
        config_path.display(),
        public_url,
        config.node_id
    );
    if args.iter().any(|v| v == "--no-start") {
        return Ok(());
    }
    println!("Starting daemon...");
    serve(config_path).await
}

async fn serve(config_path: PathBuf) -> Result<()> {
    let config = DaemonConfig::load(&config_path)
        .with_context(|| format!("run `voltd join` first or create {}", config_path.display()))?;
    if config.node_id.is_empty() || config.secret.is_empty() {
        bail!("daemon is not enrolled; run `voltd join`");
    }
    let address: SocketAddr = config.listen.parse().context("invalid listen address")?;
    let max_body = config
        .max_upload_mb
        .saturating_mul(1_048_576)
        .saturating_mul(2)
        .min(usize::MAX as u64) as usize;
    let runtime = DaemonRuntime::new(config)?;
    let state = DaemonState {
        runtime: runtime.clone(),
        nonces: Arc::new(dashmap::DashMap::new()),
    };
    tokio::spawn(heartbeat_loop(runtime.clone()));
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/servers", post(provision))
        .route("/v1/servers/:uuid", delete(remove))
        .route("/v1/servers/:uuid/console/clear", post(clear_console))
        .route("/v1/servers/:uuid/power", post(power))
        .route("/v1/servers/:uuid/stats", get(stats))
        .route("/v1/servers/:uuid/command", post(command))
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
        .with_state(state);
    tracing::info!("voltd {} listening on {}", runtime.config.node_id, address);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(runtime))
        .await?;
    Ok(())
}

async fn shutdown(runtime: DaemonRuntime) {
    let signal = tokio::signal::ctrl_c();
    let notify = runtime.shutdown_notify();
    tokio::select! { _ = signal => {}, _ = notify.notified() => {} }
}

async fn heartbeat_loop(runtime: DaemonRuntime) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
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
    node_protocol::verify(
        &state.runtime.config.secret,
        method,
        path,
        body,
        &signed,
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| DaemonError::auth(e.to_string()))?;
    let key = format!("{}:{}", signed.node_id, signed.nonce);
    state
        .nonces
        .retain(|_, ts| *ts >= signed.timestamp - node_protocol::MAX_CLOCK_SKEW_SECS * 2);
    if state.nonces.insert(key, signed.timestamp).is_some() {
        return Err(DaemonError::auth("replayed request"));
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
        state.runtime.command(&uuid, &req.command)?,
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
) -> DResult<Json<NodeApiResponse<SnapshotResponse>>> {
    let path = format!("/v1/servers/{uuid}/snapshot");
    authenticated(&state, &headers, "GET", &path, &[]).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.snapshot(&uuid)?,
    )))
}
async fn restore_snapshot(
    State(state): State<DaemonState>,
    Path(uuid): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> DResult<Json<NodeApiResponse<bool>>> {
    let path = format!("/v1/servers/{uuid}/snapshot");
    authenticated(&state, &headers, "POST", &path, &body).await?;
    Ok(Json(NodeApiResponse::success(
        state.runtime.restore_snapshot(
            &uuid,
            serde_json::from_slice::<RestoreSnapshotRequest>(&body)?,
        )?,
    )))
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
) -> DResult<Json<NodeApiResponse<bool>>> {
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

fn local_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("1.1.1.1:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}
