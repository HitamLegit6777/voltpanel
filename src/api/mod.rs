//! API layer: shared state, error type, extractors.
pub mod activity;
pub mod allocations;
pub mod backups;
pub mod blueprints;
pub mod console;
pub mod databases;
pub mod files;
pub mod keys;
pub mod metrics;
pub mod nodes;
pub mod schedules;
pub mod servers;
pub mod settings;
pub mod sites;
pub mod system;
pub mod users;
pub mod watchers;
pub mod webhooks;

use crate::capability::Capability;
use crate::config::{default_hostnames, Config, IpNet};
use crate::db::Db;
use crate::models::User;
use crate::services::{proc, ConsoleHub, Monitor};
use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use sha2::Digest;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub cfg: Config,
    pub procs: Arc<proc::ProcManager>,
    pub hub: Arc<ConsoleHub>,
    pub notifier: Arc<proc::Notifier>,
    pub monitor: Arc<Monitor>,
    pub node_client: Arc<crate::services::node::NodeClient>,
    pub node_nonces: Arc<crate::services::node::NonceCache>,
    pub running: Arc<AtomicBool>,
    pub watcher_engine: Arc<crate::services::watcher::WatcherEngine>,
}

impl FromRef<AppState> for Db {
    fn from_ref(s: &AppState) -> Self {
        s.db.clone()
    }
}
impl FromRef<AppState> for Config {
    fn from_ref(s: &AppState) -> Self {
        s.cfg.clone()
    }
}
impl FromRef<AppState> for Arc<proc::ProcManager> {
    fn from_ref(s: &AppState) -> Self {
        s.procs.clone()
    }
}
impl FromRef<AppState> for Arc<ConsoleHub> {
    fn from_ref(s: &AppState) -> Self {
        s.hub.clone()
    }
}
impl FromRef<AppState> for Arc<proc::Notifier> {
    fn from_ref(s: &AppState) -> Self {
        s.notifier.clone()
    }
}
impl FromRef<AppState> for Arc<Monitor> {
    fn from_ref(s: &AppState) -> Self {
        s.monitor.clone()
    }
}

// ---------------- Error type ----------------

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, msg)
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, msg)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        // 4xx/5xx responses were previously silent; surface a summary so
        // auth failures, rate-limit trips, and server errors are visible in
        // the request-id span. The message is the safe user-facing summary —
        // credentials never reach ApiError, and rate-limit keys are
        // user-id/IP, not secrets.
        if status.as_u16() >= 400 {
            tracing::warn!(status = status.as_u16(), error = %self.message, "api error response");
        }
        let body = serde_json::json!({ "error": self.message, "status": status.as_u16() });
        (status, Json(body)).into_response()
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        tracing::warn!("io error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

impl From<qrcode::types::QrError> for ApiError {
    fn from(e: qrcode::types::QrError) -> Self {
        ApiError::bad_request(format!("qr error: {e}"))
    }
}

impl From<image::ImageError> for ApiError {
    fn from(e: image::ImageError) -> Self {
        ApiError::bad_request(format!("image error: {e}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(node) = e.downcast_ref::<crate::services::node::NodeClientError>() {
            let status = match node.status.and_then(|v| StatusCode::from_u16(v).ok()) {
                Some(
                    v @ (StatusCode::BAD_REQUEST
                    | StatusCode::NOT_FOUND
                    | StatusCode::CONFLICT
                    | StatusCode::PAYLOAD_TOO_LARGE),
                ) => v,
                _ => StatusCode::BAD_GATEWAY,
            };
            return ApiError::new(status, node.message.clone());
        }
        // `models::get_*` wrap `QueryReturnedNoRows` with a "<thing> not found"
        // context. That is a missing resource, not a server fault.
        if let Some(rusqlite::Error::QueryReturnedNoRows) = e.downcast_ref::<rusqlite::Error>() {
            return ApiError::not_found(e.to_string());
        }
        tracing::warn!("api error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            return ApiError::not_found("not found");
        }
        tracing::warn!("db error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    }
}
impl From<r2d2::Error> for ApiError {
    fn from(e: r2d2::Error) -> Self {
        // Pool exhaustion / connection timeout: the request could not obtain
        // a database connection at all.
        tracing::warn!("db pool error: {e}");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database unavailable",
        )
    }
}
impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        if e.classify() == serde_json::error::Category::Io {
            // Io category means the failure was writing a response
            // (serialization), not the client's input.
            tracing::error!("response serialization failed: {e}");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
        } else {
            ApiError::bad_request(format!("invalid json: {e}"))
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ---------------- Auth extractor ----------------

/// Extract the authenticated user from a session cookie or API key.
///
/// Both lookup paths run on a blocking thread ([`tokio::task::spawn_blocking`]):
/// `user_from_session` and `keys::authenticate` each do `db.get()` plus
/// queries, so neither may run on a Tokio worker. The API-key path is gated
/// behind the `enable_api_keys` feature and a `Bearer` header.
/// `require_capability` (called from handlers) runs its grant lookup through
/// [`crate::db::blocking`], off the worker.
pub struct AuthUser(pub User);

#[async_trait::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if state.cfg.features.enable_api_keys {
            if let Some(raw) = parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
            {
                let db = state.db.clone();
                let raw = raw.to_string();
                let (mut user, scope) = tokio::task::spawn_blocking(move || {
                    crate::services::keys::authenticate(&db, &raw)
                })
                .await
                .map_err(|e| anyhow::anyhow!("db worker failed: {e}"))?
                .map_err(ApiError::from)?
                .ok_or_else(|| ApiError::unauthorized("invalid API key"))?;
                if !user.active {
                    return Err(ApiError::forbidden("account disabled"));
                }
                user.key_scope = Some(scope);
                return Ok(AuthUser(user));
            }
        }
        let cookie = parts
            .headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let raw = cookie
            .split(';')
            .map(|c| c.trim())
            .find(|c| c.starts_with(&format!("{}=", crate::auth::SESSION_COOKIE)))
            .and_then(|c| c.split_once('='))
            .map(|(_, v)| v.to_string());
        let Some(raw) = raw else {
            return Err(ApiError::unauthorized("not logged in"));
        };
        let db = state.db.clone();
        // `user_from_session` does `db.get()` plus queries; run it on a
        // blocking thread so the lookup never stalls a Tokio worker. A
        // JoinError is a server fault and maps to 500; a failed session
        // lookup stays a 401 as before.
        let user = tokio::task::spawn_blocking(move || {
            crate::auth::user_from_session(&db, &raw)
        })
        .await
        .map_err(|e| anyhow::anyhow!("db worker failed: {e}"))?
        .map_err(|_| ApiError::unauthorized("invalid or expired session"))?;
        if !user.active {
            return Err(ApiError::forbidden("account disabled"));
        }
        Ok(AuthUser(user))
    }
}

/// Extract the authenticated user, requiring root admin.
pub struct AdminUser(pub User);

#[async_trait::async_trait]
impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AuthUser(u) = AuthUser::from_request_parts(parts, state).await?;
        if !u.root_admin {
            return Err(ApiError::forbidden("admin only"));
        }
        // Admin routes are not server-scoped, so a key's server_ids/capability
        // filter cannot narrow them. Only a full-authority key may pass.
        if let Some(scope) = u.key_scope.as_ref() {
            if !scope.is_full_authority() {
                return Err(ApiError::forbidden(
                    "scoped API keys cannot access admin endpoints",
                ));
            }
        }
        Ok(AdminUser(u))
    }
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    let Some(origin_host) = url.host_str() else {
        return false;
    };
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let default_port = if url.scheme() == "https" { 443 } else { 80 };
    origin_host.eq_ignore_ascii_case(authority.host())
        && url.port_or_known_default() == Some(authority.port_u16().unwrap_or(default_port))
}

/// True when this request arrived over TLS: either the panel terminates TLS
/// natively, or a trusted proxy (loopback or a configured `trusted_proxies`
/// range) reports `X-Forwarded-Proto: https`.
fn request_is_tls(state: &AppState, peer: SocketAddr, headers: &HeaderMap) -> bool {
    state.cfg.web.tls_self_signed || request_is_https(peer, headers, &state.cfg.web.trusted_proxies)
}

/// Whether the socket peer may supply forwarded headers: loopback is always
/// trusted (a same-host reverse proxy is the classic deployment), plus any
/// configured `trusted_proxies` range.
fn peer_is_trusted(peer: SocketAddr, trusted_proxies: &[IpNet]) -> bool {
    peer.ip().is_loopback() || trusted_proxies.iter().any(|net| net.contains(peer.ip()))
}

/// True when a trusted peer reports `X-Forwarded-Proto: https`.
pub(super) fn request_is_https(
    peer: SocketAddr,
    headers: &HeaderMap,
    trusted_proxies: &[IpNet],
) -> bool {
    peer_is_trusted(peer, trusted_proxies)
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(',')
                    .next()
                    .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
            })
}

/// Cookie-authenticated mutations must come from the panel's own origin.
/// Bearer/HMAC clients do not carry the session cookie and remain usable
/// without browser Origin headers.
pub async fn same_origin_mutations(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method();
    let safe = matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if safe || !request.uri().path().starts_with("/api/") {
        return next.run(request).await;
    }
    let host = request_host(&request);
    let headers = request.headers();
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"))
    {
        return ApiError::forbidden("cross-site mutation rejected").into_response();
    }
    let has_session = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|cookies| {
            cookies.split(';').any(|cookie| {
                cookie
                    .trim()
                    .starts_with(&format!("{}=", crate::auth::SESSION_COOKIE))
            })
        });
    if !has_session {
        return next.run(request).await;
    }
    let Some(host) = host.as_deref() else {
        return ApiError::forbidden("missing Host header").into_response();
    };
    let Some(source) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get(axum::http::header::REFERER)
                .and_then(|v| v.to_str().ok())
        })
    else {
        return ApiError::forbidden("same-origin request required").into_response();
    };
    let origin_ok = {
        // Over TLS a plain-HTTP origin is never acceptable: a page served over
        // cleartext to the same host must not be able to mutate the TLS panel.
        if request_is_tls(&state, peer, headers) {
            let Ok(url) = url::Url::parse(source) else {
                return ApiError::forbidden("same-origin request required").into_response();
            };
            if url.scheme() != "https" {
                return ApiError::forbidden("same-origin request required").into_response();
            }
        }
        state.cfg.web.allowed_origins.iter().any(|allowed| allowed == source)
            || origin_matches_host(source, host)
    };
    if !origin_ok {
        return ApiError::forbidden("same-origin request required").into_response();
    }
    next.run(request).await
}

/// Response header echoing the per-request correlation id minted by
/// [`thread_request_id`] (exposed internally via the [`crate::REQUEST_ID`]
/// task-local). Present on every response, including 4xx/5xx and fallbacks.
pub const REQUEST_ID_HEADER: &str = "x-volt-request-id";

/// Mint one correlation id per request and expose it via the `REQUEST_ID`
/// task-local. Outermost layer: transparent (never short-circuits), so the
/// TraceLayer below it still observes every request and response.
pub async fn thread_request_id(request: Request, next: Next) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    let mut response = crate::REQUEST_ID.scope(id.clone(), next.run(request)).await;
    // Echo the id back to the client so request logs and API calls can be
    // correlated. A minted UUID is always a valid header value.
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(&id).expect("uuid is a valid header value"),
    );
    response
}
pub fn ok<T: serde::Serialize>(v: T) -> Json<serde_json::Value> {
    Json(serde_json::json!(v))
}

/// Standard JSON envelope: { success, data } or plain.
pub fn data(v: serde_json::Value) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": v }))
}

// ---------------- API metadata ----------------

/// Node agent protocol version spoken by this panel: the HMAC signing and
/// heartbeat shape contract with the `voltd` binary. Informational for API
/// clients; the enrolled agent version is the authoritative match check.
pub const NODE_PROTOCOL_VERSION: &str = "1";

/// Public panel metadata: version, feature flags, limits, rate-limit budget.
/// No secrets (matches the public settings endpoint); this is the discovery
/// entry point for API clients.
pub async fn meta(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "voltpanel",
        "version": env!("CARGO_PKG_VERSION"),
        "api": {
            "version": 1,
            "docs": "/api/meta/openapi.json",
        },
        "node_protocol": NODE_PROTOCOL_VERSION,
        "features": {
            "backups": state.cfg.features.enable_backups,
            "databases": state.cfg.features.enable_databases,
            "schedules": state.cfg.features.enable_schedules,
            "api_keys": state.cfg.features.enable_api_keys,
            "2fa": state.cfg.features.enable_2fa,
            "websites": state.cfg.features.enable_websites,
            "audit_log": state.cfg.features.enable_audit_log,
        },
        "limits": {
            "default_memory_mb": state.cfg.limits.default_memory_mb,
            "default_disk_mb": state.cfg.limits.default_disk_mb,
            "default_cpu_percent": state.cfg.limits.default_cpu_percent,
            "max_memory_mb": state.cfg.limits.max_memory_mb,
            "max_servers_per_user": state.cfg.limits.max_servers_per_user,
        },
        "rate_limit_per_min": state.cfg.security.rate_limit_per_min,
    }))
}

/// OpenAPI 3 document, compiled once at first use. Honest-but-minimal: every
/// listed path is real, operations carry tags, the shared error envelope,
/// and (for the idempotent mutations) the `Idempotency-Key` header
/// parameter. Path coverage is representative rather than exhaustive — the
/// router in main.rs remains the source of truth for the full surface.
static OPENAPI_DOC: LazyLock<serde_json::Value> = LazyLock::new(openapi_doc);

pub async fn openapi() -> Json<&'static serde_json::Value> {
    Json(&*OPENAPI_DOC)
}

/// Build one OpenAPI operation object: summary, tag, an optional success
/// response (JSON object schema), and the shared 4XX/5XX error references.
fn openapi_op(
    summary: &str,
    tag: &str,
    ok: Option<(u16, &str)>,
    params: &[serde_json::Value],
) -> serde_json::Value {
    let mut responses = serde_json::Map::new();
    if let Some((code, description)) = ok {
        responses.insert(
            code.to_string(),
            serde_json::json!({
                "description": description,
                "content": {
                    "application/json": { "schema": { "type": "object" } },
                },
            }),
        );
    }
    responses.insert(
        "4XX".to_string(),
        serde_json::json!({ "$ref": "#/components/responses/Error" }),
    );
    responses.insert(
        "5XX".to_string(),
        serde_json::json!({ "$ref": "#/components/responses/Error" }),
    );
    let mut op = serde_json::Map::new();
    op.insert(
        "summary".to_string(),
        serde_json::Value::String(summary.to_string()),
    );
    op.insert("tags".to_string(), serde_json::json!([tag]));
    if !params.is_empty() {
        op.insert(
            "parameters".to_string(),
            serde_json::Value::Array(params.to_vec()),
        );
    }
    op.insert("responses".to_string(), serde_json::Value::Object(responses));
    serde_json::Value::Object(op)
}

/// Shared header parameter for the replay-safe mutations. The middleware is
/// opt-in: a request without the header runs the mutation uncached.
fn idem_key_param() -> serde_json::Value {
    serde_json::json!({
        "name": "Idempotency-Key",
        "in": "header",
        "required": false,
        "description": "Replay-safe key: the first 2xx JSON response is cached for 10 minutes and replayed verbatim for retries with the same user/method/path/key. See docs/API.md.",
        "schema": { "type": "string" },
    })
}

/// Append the shared `Idempotency-Key` header parameter to an operation's
/// path parameters.
fn with_idem_key(params: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut all = params.to_vec();
    all.push(idem_key_param());
    all
}

/// [`openapi_op`] plus an optional JSON request body schema.
fn openapi_op_body(
    summary: &str,
    tag: &str,
    ok: Option<(u16, &str)>,
    params: &[serde_json::Value],
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut op = openapi_op(summary, tag, ok, params);
    if let Some(body) = body {
        op["requestBody"] = serde_json::json!({
            "required": true,
            "content": {
                "application/json": { "schema": body },
            },
        });
    }
    op
}

/// [`openapi_op`] for a raw-bytes upload (e.g. a database import): the
/// request body is `application/octet-stream`, not JSON.
fn openapi_op_upload(
    summary: &str,
    tag: &str,
    ok: Option<(u16, &str)>,
    params: &[serde_json::Value],
    description: &str,
) -> serde_json::Value {
    let mut op = openapi_op(summary, tag, ok, params);
    op["requestBody"] = serde_json::json!({
        "required": true,
        "description": description,
        "content": {
            "application/octet-stream": {
                "schema": { "type": "string", "format": "binary" },
            },
        },
    });
    op
}

/// [`openapi_op`] for a streaming/binary success: same error envelope, but
/// the success response advertises the real content type instead of JSON.
fn openapi_op_stream(
    summary: &str,
    tag: &str,
    ok: (u16, &str),
    params: &[serde_json::Value],
    content_type: &str,
) -> serde_json::Value {
    let mut op = openapi_op(summary, tag, Some(ok), params);
    let mut content = serde_json::Map::new();
    content.insert(
        content_type.to_string(),
        serde_json::json!({ "schema": { "type": "string", "format": "binary" } }),
    );
    op["responses"][ok.0.to_string()]["content"] = serde_json::Value::Object(content);
    op
}

fn path_param(name: &str, description: &str, ty: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "in": "path",
        "required": true,
        "description": description,
        "schema": { "type": ty },
    })
}

fn openapi_doc() -> serde_json::Value {
    let sid = path_param("id", "Server id", "integer");
    let bid = path_param("id", "Backup id", "integer");
    let schid = path_param("id", "Schedule id", "integer");
    let uid = path_param("id", "User id", "integer");
    let nid = path_param("id", "Node id", "integer");
    let ntfid = path_param("id", "Notification id", "integer");
    let siteid = path_param("site_id", "Site id", "integer");
    let watcherid = path_param("watcher_id", "Console watcher id", "integer");
    let dbname = path_param("name", "Database name", "string");
    let mut paths = serde_json::Map::new();
    paths.insert(
        "/api/meta".to_string(),
        serde_json::json!({
            "get": openapi_op("Panel metadata: version, feature flags, limits", "meta", Some((200, "Metadata")), &[]),
        }),
    );
    paths.insert(
        "/api/meta/openapi.json".to_string(),
        serde_json::json!({
            "get": openapi_op("This OpenAPI 3 document", "meta", Some((200, "OpenAPI document")), &[]),
        }),
    );
    paths.insert(
        "/api/login".to_string(),
        serde_json::json!({
            "post": openapi_op_body(
                "Authenticate with username and password; sets the session cookie",
                "auth",
                Some((200, "Authenticated")),
                &[],
                Some(serde_json::json!({
                    "type": "object",
                    "required": ["username", "password"],
                    "properties": {
                        "username": { "type": "string" },
                        "password": { "type": "string" },
                        "remember": { "type": "boolean", "description": "Extend the session beyond the default lifetime" },
                        "totp_code": { "type": "string", "description": "TOTP code; required when 2FA is enabled and no recovery code is used" },
                        "recovery_code": { "type": "string", "description": "Single-use 2FA recovery code; consumed on first use" },
                    },
                })),
            ),
        }),
    );
    paths.insert(
        "/api/logout".to_string(),
        serde_json::json!({
            "post": openapi_op("End the current session", "auth", Some((200, "Logged out")), &[]),
        }),
    );
    paths.insert(
        "/api/me".to_string(),
        serde_json::json!({
            "get": openapi_op("Current user profile", "auth", Some((200, "User profile")), &[]),
        }),
    );
    paths.insert(
        "/api/2fa/recovery/regenerate".to_string(),
        serde_json::json!({
            "post": openapi_op_body(
                "Rotate the caller's 2FA recovery codes (password + TOTP required)",
                "auth",
                Some((200, "New recovery codes")),
                &[],
                Some(serde_json::json!({
                    "type": "object",
                    "required": ["password", "code"],
                    "properties": {
                        "password": { "type": "string" },
                        "code": { "type": "string", "description": "Current TOTP code proving possession" },
                    },
                })),
            ),
        }),
    );
    paths.insert(
        "/api/admin/users/{id}/2fa/reset".to_string(),
        serde_json::json!({
            "post": openapi_op("Admin: disable another user's 2FA (root admin)", "auth", Some((200, "2FA disabled")), std::slice::from_ref(&uid)),
        }),
    );
    paths.insert(
        "/api/servers".to_string(),
        serde_json::json!({
            "get": openapi_op("List visible servers", "servers", Some((200, "Paginated server list")), &[]),
            "post": openapi_op_body("Create a server (idempotent with Idempotency-Key)", "servers", Some((201, "Created server")), &with_idem_key(&[]), Some(serde_json::json!({
                "type": "object",
                "required": ["name", "user_id", "blueprint_id"],
                "properties": {
                    "name": { "type": "string" },
                    "user_id": { "type": "integer" },
                    "blueprint_id": { "type": "integer" },
                    "description": { "type": "string" },
                    "runtime_hint": { "type": "string" },
                    "memory_mb": { "type": "integer" },
                    "disk_mb": { "type": "integer" },
                    "cpu_percent": { "type": "integer" },
                    "port": { "type": "integer" },
                    "start_on_create": { "type": "boolean" },
                    "node": { "type": "string" },
                    "location": { "type": "string" },
                },
            }))),
        }),
    );
    paths.insert(
        "/api/servers/{id}".to_string(),
        serde_json::json!({
            "get": openapi_op("Get a server", "servers", Some((200, "Server")), std::slice::from_ref(&sid)),
            "patch": openapi_op("Update a server", "servers", Some((200, "Updated server")), std::slice::from_ref(&sid)),
            "delete": openapi_op("Delete a server", "servers", Some((200, "Deleted")), std::slice::from_ref(&sid)),
        }),
    );
    paths.insert(
        "/api/servers/{id}/power".to_string(),
        serde_json::json!({
            "post": openapi_op("Power a server (start/stop/restart/kill)", "servers", Some((200, "Power state")), std::slice::from_ref(&sid)),
        }),
    );
    paths.insert(
        "/api/servers/{id}/sites".to_string(),
        serde_json::json!({
            "get": openapi_op("List website/vhost entries", "sites", Some((200, "Site list")), std::slice::from_ref(&sid)),
            "post": openapi_op("Create a site", "sites", Some((201, "Created site")), std::slice::from_ref(&sid)),
        }),
    );
    paths.insert(
        "/api/servers/{id}/sites/{site_id}".to_string(),
        serde_json::json!({
            "get": openapi_op("Get a site", "sites", Some((200, "Site")), &[sid.clone(), siteid.clone()]),
            "patch": openapi_op("Update a site", "sites", Some((200, "Updated site")), &[sid.clone(), siteid.clone()]),
            "delete": openapi_op("Delete a site", "sites", Some((200, "Deleted")), &[sid.clone(), siteid.clone()]),
        }),
    );
    paths.insert(
        "/api/servers/{id}/console/watchers".to_string(),
        serde_json::json!({
            "get": openapi_op("List console watchers", "console", Some((200, "Watcher list")), std::slice::from_ref(&sid)),
            "post": openapi_op("Create a console watcher", "console", Some((201, "Created watcher")), std::slice::from_ref(&sid)),
        }),
    );
    paths.insert(
        "/api/servers/{id}/console/watchers/{watcher_id}".to_string(),
        serde_json::json!({
            "put": openapi_op("Update a console watcher", "console", Some((200, "Updated watcher")), &[sid.clone(), watcherid.clone()]),
            "delete": openapi_op("Delete a console watcher", "console", Some((200, "Deleted")), &[sid.clone(), watcherid.clone()]),
        }),
    );
    paths.insert(
        "/api/servers/{id}/backups".to_string(),
        serde_json::json!({
            "get": openapi_op("List backups", "backups", Some((200, "Backup list")), std::slice::from_ref(&sid)),
            "post": openapi_op_body("Create a backup (idempotent with Idempotency-Key)", "backups", Some((201, "Created backup")), &with_idem_key(std::slice::from_ref(&sid)), Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Optional backup name; defaults to a timestamped name" },
                    "ignore": { "type": "string", "description": "Newline-separated glob patterns excluded from the archive" },
                },
            }))),
        }),
    );
    paths.insert(
        "/api/backups/{id}/restore".to_string(),
        serde_json::json!({
            "post": openapi_op("Restore a backup (idempotent with Idempotency-Key)", "backups", Some((200, "Restore queued")), &with_idem_key(std::slice::from_ref(&bid))),
        }),
    );
    paths.insert(
        "/api/backups/{id}/download".to_string(),
        serde_json::json!({
            "get": openapi_op_stream("Download a backup archive (streaming)", "backups", (200, "Archive stream"), std::slice::from_ref(&bid), "application/octet-stream"),
        }),
    );
    paths.insert(
        "/api/servers/{id}/databases/{name}/export".to_string(),
        serde_json::json!({
            "get": openapi_op_stream(
                "Download a consistent SQLite snapshot of one database (streaming)",
                "databases",
                (200, "Database snapshot"),
                &[sid.clone(), dbname.clone()],
                "application/octet-stream",
            ),
        }),
    );
    paths.insert(
        "/api/servers/{id}/databases/{name}/import".to_string(),
        serde_json::json!({
            "post": openapi_op_upload(
                "Replace a database with an uploaded .db file",
                "databases",
                Some((200, "Import result")),
                &[sid.clone(), dbname.clone()],
                "Raw SQLite database bytes; integrity-checked and bounded by the server's Data Lab quota",
            ),
        }),
    );
    paths.insert(
        "/api/schedules/{id}/run".to_string(),
        serde_json::json!({
            "post": openapi_op("Run a schedule now (idempotent with Idempotency-Key)", "schedules", Some((200, "Run queued")), &with_idem_key(&[schid])),
        }),
    );
    paths.insert(
        "/api/notifications".to_string(),
        serde_json::json!({
            "get": openapi_op("List admin notifications", "notifications", Some((200, "Notification list")), &[]),
        }),
    );
    paths.insert(
        "/api/notifications/stream".to_string(),
        serde_json::json!({
            "get": openapi_op_stream("Live admin notifications (SSE)", "notifications", (200, "Event stream"), &[], "text/event-stream"),
        }),
    );
    paths.insert(
        "/api/notifications/{id}/read".to_string(),
        serde_json::json!({
            "post": openapi_op("Mark a notification read", "notifications", Some((200, "Updated notification")), std::slice::from_ref(&ntfid)),
        }),
    );
    paths.insert(
        "/api/notifications/clear".to_string(),
        serde_json::json!({
            "post": openapi_op("Dismiss all notifications", "notifications", Some((200, "Cleared")), &[]),
        }),
    );
    paths.insert(
        "/api/nodes/{id}/drain".to_string(),
        serde_json::json!({
            "post": openapi_op("Drain a node: stop scheduling and evacuate workloads (admin)", "nodes", Some((200, "Drain started")), std::slice::from_ref(&nid)),
            "delete": openapi_op("Lift a drain and restore scheduling (admin)", "nodes", Some((200, "Drain cleared")), std::slice::from_ref(&nid)),
        }),
    );
    paths.insert(
        "/api/system/health".to_string(),
        serde_json::json!({
            "get": openapi_op("Panel health (admin)", "system", Some((200, "Health status")), &[]),
        }),
    );
    serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "VoltPanel API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "REST API for the VoltPanel control plane. Every response carries an `x-volt-request-id` correlation header. Errors use the envelope {\"error\": ..., \"status\": ...}. High-risk mutations accept an `Idempotency-Key` header (see docs/API.md).",
        },
        "servers": [{ "url": "/" }],
        "paths": serde_json::Value::Object(paths),
        "components": {
            "securitySchemes": {
                "cookieAuth": {
                    "type": "apiKey",
                    "in": "cookie",
                    "name": crate::auth::SESSION_COOKIE,
                    "description": "Session cookie set by /api/login. Cookie-authenticated mutations must also match the panel Origin.",
                },
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "API key (vp_...); requires the api_keys feature. Scoped keys cannot access admin endpoints.",
                },
            },
            "responses": {
                "Error": {
                    "description": "Standard error envelope",
                    "content": {
                        "application/json": {
                            "schema": { "$ref": "#/components/schemas/Error" },
                        },
                    },
                },
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error", "status"],
                    "properties": {
                        "error": { "type": "string", "description": "Human-readable failure summary" },
                        "status": { "type": "integer", "description": "HTTP status code" },
                    },
                },
            },
        },
        "security": [{ "cookieAuth": [] }],
    })
}

/// Enforce a typed capability on a server-scoped route.
///
/// The grant lookup runs on a blocking thread ([`crate::db::blocking`]):
/// `models::user_has_capability` does `db.get()` plus queries, so it must not
/// run on a Tokio worker. The `AuthUser`'s `User` is cloned into the closure
/// (`AuthUser` itself is not `Clone`); the error mapping is unchanged.
pub async fn require_capability(
    state: &AppState,
    user: &AuthUser,
    server_id: i64,
    capability: Capability,
) -> ApiResult<()> {
    let db = state.db.clone();
    let user = user.0.clone();
    let ok = crate::db::blocking(db, move |db| {
        crate::models::user_has_capability(&db, &user, server_id, capability)
    })
    .await?;
    if ok {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "missing server capability: {}",
            capability.as_str()
        )))
    }
}

/// Best-effort client IP for rate limiting and audit logs.
///
/// The socket peer address is authoritative unless it is a trusted proxy —
/// loopback always (a same-host reverse proxy is the classic deployment) or
/// a configured `trusted_proxies` range. Only then may `X-Forwarded-For` be
/// consulted, taking the rightmost hop that is not itself a trusted proxy.
/// With no `trusted_proxies` configured a non-loopback peer's forwarded
/// headers are ignored and the peer address is reported, exactly as before.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted_proxies: &[IpNet]) -> String {
    if !peer_is_trusted(peer, trusted_proxies) {
        return peer.ip().to_string();
    }
    let Some(xff) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    else {
        return peer.ip().to_string();
    };
    for hop in xff.split(',').rev() {
        let hop = hop.trim();
        if hop.is_empty() {
            continue;
        }
        let Ok(ip) = hop.parse::<IpAddr>() else {
            continue;
        };
        // Loopback hops are as trustworthy as loopback peers: a same-host
        // proxy appends itself, so the client is the next hop further out.
        if ip.is_loopback() || trusted_proxies.iter().any(|net| net.contains(ip)) {
            continue;
        }
        return ip.to_string();
    }
    peer.ip().to_string()
}

// ---------------- Host allowlist (DNS-rebinding defense) ----------------

/// Split a `Host` header value (or configured hostname) into host and optional
/// port. IPv6 literals stay bracketed: `[::1]:8080`.
fn split_host_port(value: &str) -> (&str, Option<&str>) {
    if value.starts_with('[') {
        let Some(close) = value.find(']') else {
            return (value, None);
        };
        let host = &value[..=close];
        let port = value[close + 1..].strip_prefix(':');
        (host, port)
    } else {
        match value.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                (host, Some(port))
            }
            _ => (value, None),
        }
    }
}

/// A configured hostname matches when the host part is equal (case-insensitive)
/// and, when the entry carries a port, the request port is identical. An entry
/// without a port accepts any port on that host.
fn hostname_matches(allowed: &str, host: &str) -> bool {
    let (allowed_host, allowed_port) = split_host_port(allowed);
    let (host_host, host_port) = split_host_port(host);
    if !allowed_host.eq_ignore_ascii_case(host_host) {
        return false;
    }
    match allowed_port {
        Some(configured) => host_port == Some(configured),
        None => true,
    }
}

/// Reject requests whose `Host` header is not on the configured allowlist
/// (`web.hostnames`, derived from the listen address when empty) with 400.
/// Without this, `same_origin_mutations` compares Origin against the request's
/// own Host header — a DNS-rebinding attacker controls both sides.
///
/// When `web.hostnames` is empty (the default) the derived allowlist — listen
/// address, loopback aliases, machine hostname — is in force, plus every
/// IP-literal Host: DNS rebinding requires a hostname Host (the attacker's
/// domain resolving to the victim IP), so an IP literal can never be a
/// rebinding vector and LAN-IP / `certbot-ip` deployments stay reachable out
/// of the box. An explicit `web.hostnames` is strict and names everything.
///
/// HMAC-protected node endpoints are exempt: nodes dial the panel by the URL
/// they were enrolled with (often a LAN IP that is not on the allowlist), and
/// their requests carry no browser cookies for a rebinding attack to ride on.
pub async fn host_allowlist(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().path().starts_with("/api/node/") {
        return next.run(request).await;
    }
    let Some(host) = request_host(&request) else {
        return ApiError::bad_request("missing Host header").into_response();
    };
    if state.cfg.web.hostnames.is_empty() {
        // Derived mode: IP-literal Hosts pass unconditionally; anything else
        // must match the derived defaults (listen address, loopback aliases,
        // machine hostname).
        if host_is_ip_literal(&host) {
            return next.run(request).await;
        }
        let derived = default_hostnames(&state.cfg.web.listen, &state.cfg.web.tls_extra_sans);
        if derived
            .iter()
            .any(|candidate| hostname_matches(candidate, &host))
        {
            return next.run(request).await;
        }
    } else if state
        .cfg
        .web
        .hostnames
        .iter()
        .any(|candidate| hostname_matches(candidate, &host))
    {
        return next.run(request).await;
    }
    ApiError::bad_request("unknown Host header").into_response()
}

/// True when the host part of a `Host` header value is an IP literal
/// (IPv4 or bracketed IPv6, with or without a port).
fn host_is_ip_literal(host: &str) -> bool {
    let (host_part, _) = split_host_port(host);
    let bare = host_part
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host_part);
    bare.parse::<IpAddr>().is_ok()
}

/// The effective Host for a request, bridging HTTP/1.1 and HTTP/2.
///
/// HTTP/1.1 carries the authority in the `Host` header; HTTP/2 drops that
/// header and moves the authority into the `:authority` pseudo-header, which
/// hyper surfaces as the request URI's authority. Reading `header::HOST`
/// alone rejects every HTTP/2 request (ALPN negotiates `h2` by default) with
/// a spurious "missing Host header". Prefer the header, fall back to the URI
/// authority so both protocol versions resolve the same value.
fn request_host(request: &axum::extract::Request) -> Option<String> {
    if let Some(host) = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        return Some(host.to_string());
    }
    request.uri().authority().map(|a| a.to_string())
}

// ---------------- Router-level mutation rate limit ----------------

/// Coarse flood gate for every non-GET `/api/*` request, keyed on the client
/// IP ([`client_ip`], so trusted proxies are honored). Login keeps its own
/// stricter per-account + per-IP buckets; this bucket catches everything else
/// (create/update/delete handlers) that previously had no router-level limit.
///
/// HMAC-protected node endpoints are exempt: they are not brute-forceable
/// through a browser, and rate-limiting them would throttle legitimate fleet
/// heartbeats when many nodes share one egress IP.
///
/// The bucket check runs on a blocking thread ([`tokio::task::spawn_blocking`]):
/// `rate_limit` does `db.get()` plus a synchronous write, so it must not run
/// on a Tokio worker. A failed check passes the request through rather than
/// taking the panel down with it; a JoinError from the blocking task is a
/// genuine server fault and surfaces as 500.
pub async fn api_mutation_rate_limit(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    let safe = matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if safe
        || !path.starts_with("/api/")
        || path == "/api/node/enroll"
        || path == "/api/node/heartbeat"
    {
        return next.run(request).await;
    }
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let db = state.db.clone();
    let cfg = state.cfg.clone();
    let limit = cfg.security.rate_limit_per_min;
    let key = format!("api-mut:{ip}");
    match tokio::task::spawn_blocking(move || crate::auth::rate_limit(&db, &cfg, &key)).await {
        Ok(Ok(true)) => {
            // Approved mutation: report the budget window. `Remaining` is
            // omitted because the bucket depth lives in auth.rs (out of
            // scope); Limit and Reset are the honest static approximation.
            let mut response = next.run(request).await;
            apply_rate_limit_headers(&mut response, limit, None);
            response
        }
        Ok(Ok(false)) => {
            // Denied: the bucket held fewer than one token, so zero remaining
            // is exact, not approximate.
            let mut response = ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many requests; try again later",
            )
            .into_response();
            apply_rate_limit_headers(&mut response, limit, Some(0));
            response
        }
        // A failed bucket check must not take the panel down with it.
        Ok(Err(e)) => {
            tracing::warn!("api rate limit check failed: {e:#}");
            next.run(request).await
        }
        // The blocking task itself failed (panic/shutdown): surface a 500
        // rather than silently disabling the flood gate.
        Err(e) => {
            tracing::warn!("api rate limit worker failed: {e}");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
                .into_response()
        }
    }
}

// ---------------- Rate-limit response headers ----------------

/// Standard rate-limit response headers (IETF
/// draft-ietf-httpapi-ratelimit-headers): `RateLimit-Limit`,
/// `RateLimit-Remaining`, `RateLimit-Reset`. `Reset` is the epoch second of
/// the next 60-second window boundary — the same fixed window the token
/// bucket in [`crate::auth::rate_limit`] refills against — and is a static
/// approximation by design: the bucket's exact depth is not exposed from
/// auth.rs, so `Remaining` is only set when it is provable (0 on a rejected
/// 429; omitted otherwise).
const RATE_LIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
const RATE_LIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
const RATE_LIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");

fn apply_rate_limit_headers(response: &mut Response, limit: u64, remaining: Option<u64>) {
    let reset = (Utc::now().timestamp() / 60 + 1) * 60;
    let headers = response.headers_mut();
    headers.insert(
        RATE_LIMIT_LIMIT,
        HeaderValue::from_str(&limit.to_string()).expect("u64 is a valid header value"),
    );
    headers.insert(
        RATE_LIMIT_RESET,
        HeaderValue::from_str(&reset.to_string()).expect("i64 is a valid header value"),
    );
    if let Some(remaining) = remaining {
        headers.insert(
            RATE_LIMIT_REMAINING,
            HeaderValue::from_str(&remaining.to_string()).expect("u64 is a valid header value"),
        );
    }
}

// ---------------- Idempotency keys ----------------

/// HTTP header carrying the client-chosen idempotency key.
const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// Cached entries live for 10 minutes — the longest reasonable client retry
/// horizon — and the table is capped at 10_000 entries. Once full of live
/// entries, new keys are refused (and execute uncached) rather than evicting
/// anything.
const IDEMPOTENCY_TTL_SECS: i64 = 600;
const IDEMPOTENCY_MAX_ENTRIES: usize = 10_000;
/// Request bodies beyond this cap are not hashed; the configured
/// `web.max_body_mb` (enforced by DefaultBodyLimit) bounds this in practice.
const IDEMPOTENCY_BODY_CAP: usize = 16 * 1024 * 1024;

/// Follower wait bound: a concurrent same-key request parks on the owner's
/// `Notify` and is normally woken the instant the owner finishes. This
/// timeout is a liveness backstop — a follower re-checks the cache after it
/// elapses, so a lost wakeup (or an owner that vanished without notifying)
/// can never hang a follower, and the owner's own runtime is the real bound.
const IDEMPOTENCY_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
struct IdemEntry {
    status: u16,
    body: Bytes,
    content_type: String,
    /// Safe response headers replayed verbatim with the body. Only
    /// allowlisted headers are stored ([`is_replayable_header`]):
    /// `Set-Cookie` is never cached, and `x-volt-request-id` is not cached
    /// because the outer [`thread_request_id`] layer mints a fresh one per
    /// request, replays included.
    headers: Vec<(HeaderName, HeaderValue)>,
    request_hash: String,
    created_at: i64,
}

/// A key whose request is currently executing. The map flips this slot to
/// [`IdemSlot::Complete`] when the owner finishes, or removes it when the
/// owner fails or is aborted, so followers treat the map as the source of
/// truth and the `Notify` purely as a wakeup hint.
struct IdemInFlight {
    request_hash: String,
    notify: Notify,
}

enum IdemSlot {
    Complete(IdemEntry),
    InFlight(Arc<IdemInFlight>),
}

/// What a request resolves to for the key: own it (install an in-flight slot
/// and run the handler), wait on the current owner, re-examine the map, or —
/// when the cache is at capacity — run the handler uncached with no slot.
/// Returned from the lock scope so the follower wait — an `.await` — never
/// runs while the std `MutexGuard` is in scope (the guard is `!Send`, which
/// would make the middleware future non-`Send`).
enum IdemDecision {
    Own(Arc<IdemInFlight>),
    Wait(Arc<IdemInFlight>),
    Retry,
    Uncached,
}

/// Process-global bounded replay cache. Deliberately not on `AppState`: the
/// ten AppState construction sites (production + tests) stay untouched. The
/// key embeds the user id, so entries can never leak across accounts; tests
/// mint unique keys, so parallel tests cannot collide.
static IDEM_CACHE: LazyLock<Mutex<HashMap<String, IdemSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII owner lease for an in-flight key. On every exit — success, failure,
/// or task abort (client disconnect, panic, cancellation) — it removes the
/// slot (only while the map still points at this exact slot) and wakes every
/// follower. Followers re-check the map after waking, so a removed slot
/// hands the key to exactly one retrying follower and an abandoned owner can
/// never leak its entry.
struct IdemOwnerGuard {
    cache_key: String,
    slot: Arc<IdemInFlight>,
}

impl Drop for IdemOwnerGuard {
    fn drop(&mut self) {
        let mut cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(cache.get(&self.cache_key), Some(IdemSlot::InFlight(s)) if Arc::ptr_eq(s, &self.slot))
        {
            cache.remove(&self.cache_key);
        }
        drop(cache);
        self.slot.notify.notify_waiters();
    }
}

/// Insert an entry honoring the size/TTL cap. Only expired completed entries
/// are ever pruned; live in-flight slots are never evicted (evicting one
/// while its owner still runs would let a follower re-execute the same
/// mutation) and the owner guard already removes every in-flight slot on
/// exit, so they cannot accumulate.
///
/// Returns whether the slot was inserted. When the map is still at the cap
/// after pruning, the insert is refused instead of clearing the cache — a
/// clear would hand the key to a retrying follower mid-flight and duplicate
/// a live owner's mutation. The caller runs that request uncached.
fn idem_insert_capped(
    cache: &mut HashMap<String, IdemSlot>,
    key: String,
    slot: IdemSlot,
    now: i64,
) -> bool {
    if cache.len() >= IDEMPOTENCY_MAX_ENTRIES {
        cache.retain(|_, s| match s {
            IdemSlot::Complete(entry) => now - entry.created_at < IDEMPOTENCY_TTL_SECS,
            IdemSlot::InFlight(_) => true,
        });
        if cache.len() >= IDEMPOTENCY_MAX_ENTRIES {
            // Still full of live entries: refuse rather than evict.
            return false;
        }
    }
    cache.insert(key, slot);
    true
}

/// Replay-safe mutations for high-risk POSTs.
///
/// Applied per-route to server creation, backup create/restore, and schedule
/// run-now. When the client sends an `Idempotency-Key` header, the first
/// response (status + JSON body + safe headers) is cached for
/// [`IDEMPOTENCY_TTL_SECS`]; a later request with the same
/// user + method + path + key replays the cached response without touching
/// the handler. A reused key with a *different* request body is a client bug
/// and answers 409.
///
/// Only successful 2xx `application/json` responses are cached — streaming
/// and binary responses pass through untouched, and 4xx/5xx errors are never
/// cached so neither a rejected request nor a transient failure can poison
/// the key. Requests whose body cannot be buffered (over the cap) answer
/// 413, matching what the handler would do.
///
/// Concurrent requests for the same key are serialized instead of both
/// executing: the first installs an in-flight slot and runs the handler;
/// followers with the same body hash wait on the slot's `Notify` and then
/// replay the cached response, so the handler runs exactly once. A different
/// body while the owner is in flight answers 409 immediately. When the
/// owner's response is not cacheable (4xx/5xx, non-JSON, unbufferable) or
/// the owner is aborted, the slot is removed and followers are woken; the
/// first to re-check takes ownership and retries, so every follower is
/// answered and a transient failure never poisons the key. When the cache is
/// at its 10_000-entry capacity, new keys execute uncached — exactly once
/// per request — rather than evicting live entries.
pub async fn idempotency(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(idem_key) = request
        .headers()
        .get(IDEMPOTENCY_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
    else {
        return next.run(request).await;
    };
    let (mut parts, body) = request.into_parts();
    let user_id = match AuthUser::from_request_parts(&mut parts, &state).await {
        Ok(user) => user.0.id,
        Err(_) => return next.run(Request::from_parts(parts, body)).await,
    };
    let method = parts.method.to_string();
    let path = parts.uri.path().to_string();
    let body_cap = state
        .cfg
        .web
        .max_body_mb
        .saturating_mul(1_048_576)
        .min(IDEMPOTENCY_BODY_CAP as u64) as usize;
    let bytes = match to_bytes(body, body_cap).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
                .into_response()
        }
    };
    let request_hash = hex::encode(sha2::Sha256::digest(&bytes));
    let cache_key = format!("{user_id}:{method}:{path}:{idem_key}");
    let now = Utc::now().timestamp();

    // Resolve the key: take ownership (install the in-flight slot) or join
    // as a follower. The map is the source of truth; the Notify is only a
    loop {
        // Resolve the key under the lock; the decision carries only owned
        // Arcs out, so no `.await` below ever runs with the (non-`Send`)
        // std `MutexGuard` in scope.
        let decision = {
            let mut cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
            match cache.get(&cache_key) {
                None => {
                    let slot = Arc::new(IdemInFlight {
                        request_hash: request_hash.clone(),
                        notify: Notify::new(),
                    });
                    if idem_insert_capped(
                        &mut cache,
                        cache_key.clone(),
                        IdemSlot::InFlight(slot.clone()),
                        now,
                    ) {
                        IdemDecision::Own(slot)
                    } else {
                        // At the capacity ceiling no slot could be installed:
                        // run uncached, exactly once, with no owner guard.
                        IdemDecision::Uncached
                    }
                }
                Some(IdemSlot::InFlight(slot)) if slot.request_hash == request_hash => {
                    IdemDecision::Wait(slot.clone())
                }
                Some(IdemSlot::InFlight(_)) => {
                    return ApiError::conflict(format!(
                        "idempotency key '{idem_key}' is in use by a concurrent request with a different body"
                    ))
                    .into_response();
                }
                Some(IdemSlot::Complete(entry)) => {
                    if now - entry.created_at < IDEMPOTENCY_TTL_SECS {
                        if entry.request_hash == request_hash {
                            return cached_idem_response(entry);
                        }
                        return ApiError::conflict(format!(
                            "idempotency key '{idem_key}' was already used with a different request body"
                        ))
                        .into_response();
                    }
                    // Expired entry: re-execute and overwrite below.
                    cache.remove(&cache_key);
                    IdemDecision::Retry
                }
            }
        };

        match decision {
            // Same body while the owner runs: wait for it. The Notified
            // future snapshots the notify_waiters counter at creation, so a
            // wake published after this point is never lost; the timeout is
            // a liveness backstop only.
            IdemDecision::Uncached => {
                return next.run(Request::from_parts(parts, Body::from(bytes))).await;
            }
            IdemDecision::Wait(slot) => {
                let _ = tokio::time::timeout(IDEMPOTENCY_WAIT_TIMEOUT, slot.notify.notified())
                    .await;
                continue;
            }
            IdemDecision::Retry => continue,
            // Owner: run the handler under the guard, then cache or hand off.
            IdemDecision::Own(slot) => {
                let _guard = IdemOwnerGuard {
                    cache_key: cache_key.clone(),
                    slot: slot.clone(),
                };
                let response = next.run(Request::from_parts(parts, Body::from(bytes))).await;
                let (response_parts, response_body) = response.into_parts();
                let status = response_parts.status;
                let content_type = response_parts
                    .headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                // Streaming/binary responses must not be buffered; only
                // successful 2xx responses are cached so neither a rejected
                // 4xx nor a transient 5xx poisons the key. Either way the
                // guard drop below removes the in-flight slot and wakes
                // followers, letting one of them retry.
                if !status.is_success() || !content_type.starts_with("application/json") {
                    return Response::from_parts(response_parts, response_body);
                }
                return match to_bytes(response_body, body_cap).await {
                    Ok(buf) => {
                        let headers = response_parts
                            .headers
                            .iter()
                            .filter(|(name, _)| is_replayable_header(name))
                            .map(|(name, value)| (name.clone(), value.clone()))
                            .collect();
                        let entry = IdemEntry {
                            status: status.as_u16(),
                            body: buf.clone(),
                            content_type,
                            headers,
                            request_hash,
                            created_at: now,
                        };
                        let mut cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
                        // At the cap the completion is not cached (documented
                        // degradation): the response still goes out and the
                        // owner guard removes the in-flight slot.
                        idem_insert_capped(&mut cache, cache_key, IdemSlot::Complete(entry), now);
                        drop(cache);
                        Response::from_parts(response_parts, Body::from(buf))
                    }
                    Err(_) => ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "response body could not be buffered",
                    )
                    .into_response(),
                };
            }
        }
    }
}

/// Response headers a replay may reproduce. The allowlist keeps `Set-Cookie`
/// out (replaying a cookie would resurrect a session) and
/// `x-volt-request-id` out (the outer [`thread_request_id`] layer mints a
/// fresh id per request). `Content-Type` is stored separately on the entry;
/// [`cached_idem_response`] replays `Location`/`Content-Location` verbatim.
fn is_replayable_header(name: &HeaderName) -> bool {
    matches!(name.as_str(), "location" | "content-location")
}

fn cached_idem_response(entry: &IdemEntry) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(entry.status).unwrap_or(StatusCode::OK))
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_str(entry.content_type.as_str())
                .expect("stored content-type is a valid header value"),
        );
    for (name, value) in &entry.headers {
        builder = builder.header(name.clone(), value.clone());
    }
    builder
        .body(Body::from(entry.body.clone()))
        .expect("idempotent replay response is constructible")
}

// ---------------- HSTS ----------------

/// Emit `Strict-Transport-Security` on every response served over TLS (native
/// or via a trusted proxy's `X-Forwarded-Proto`). Self-signed deployments get
/// a modest max-age; production TLS gets a year.
pub async fn hsts(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    if request_is_tls(&state, peer, &headers)
        && response
            .headers()
            .get(axum::http::header::STRICT_TRANSPORT_SECURITY)
            .is_none()
        {
        let value = if state.cfg.web.tls_self_signed {
            "max-age=86400"
        } else {
            "max-age=31536000"
        };
        response.headers_mut().insert(
            axum::http::header::STRICT_TRANSPORT_SECURITY,
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `models::get_*` report a missing row as `QueryReturnedNoRows` wrapped in
    /// a "<thing> not found" context. Handlers surface that with `?`, so the
    /// conversion decides whether operators see 404 or a bogus 500.
    #[test]
    fn missing_row_context_maps_to_404() {
        let err = anyhow::Error::from(rusqlite::Error::QueryReturnedNoRows)
            .context("server not found");
        let mapped = ApiError::from(err);
        assert_eq!(mapped.status, StatusCode::NOT_FOUND);
        assert!(mapped.message.contains("server not found"));
    }

    #[test]
    fn bare_missing_row_maps_to_404() {
        let mapped = ApiError::from(rusqlite::Error::QueryReturnedNoRows);
        assert_eq!(mapped.status, StatusCode::NOT_FOUND);
    }

    /// A real failure must stay a 500 and must not leak the internal message.
    #[test]
    fn other_errors_remain_internal_and_opaque() {
        let mapped = ApiError::from(anyhow::anyhow!("disk on fire: /secret/path"));
        assert_eq!(mapped.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped.message, "internal server error");

        let mapped = ApiError::from(rusqlite::Error::ExecuteReturnedResults);
        assert_eq!(mapped.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped.message, "database error");
    }

    #[test]
    fn client_ip_ignores_xff_from_untrusted_peer() {
        let headers = HeaderMap::new();
        let peer: SocketAddr = "198.51.100.9:45000".parse().unwrap();
        let mut spoofed = HeaderMap::new();
        spoofed.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        assert_eq!(
            client_ip(peer, &spoofed, &[]),
            "198.51.100.9",
            "remote peers must never influence the reported client IP"
        );
        assert_eq!(client_ip(peer, &headers, &[]), "198.51.100.9");
    }

    #[test]
    fn client_ip_trusts_rightmost_untrusted_xff_hop() {
        let trusted = vec![
            "127.0.0.1".parse().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
        ];
        let peer: SocketAddr = "10.0.0.5:45000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.7, 10.0.0.1, 10.0.0.5".parse().unwrap(),
        );
        assert_eq!(client_ip(peer, &headers, &trusted), "203.0.113.7");

        // Every listed hop trusted -> cannot recover a client, fall back to peer.
        let mut all_trusted = HeaderMap::new();
        all_trusted.insert(
            "x-forwarded-for",
            "10.0.0.1, 10.0.0.2".parse().unwrap(),
        );
        assert_eq!(client_ip(peer, &all_trusted, &trusted), "10.0.0.5");

        // Malformed hops are skipped, not trusted.
        let mut messy = HeaderMap::new();
        messy.insert("x-forwarded-for", "10.0.0.1, nope, 203.0.113.7".parse().unwrap());
        assert_eq!(client_ip(peer, &messy, &trusted), "203.0.113.7");
    }

    #[test]
    fn client_ip_honors_trusted_proxy_configuration() {
        // Trusted 10/8 proxy with the client two hops out: the rightmost
        // untrusted hop is reported.
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        let peer: SocketAddr = "10.0.0.5:45000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 10.0.0.5".parse().unwrap());
        assert_eq!(client_ip(peer, &headers, &trusted), "1.2.3.4");
        // Without trusted_proxies the peer is authoritative: forwarded
        // headers from a non-loopback peer are never honored.
        assert_eq!(client_ip(peer, &headers, &[]), "10.0.0.5");
    }

    #[test]
    fn io_errors_map_to_opaque_500() {
        let mapped = ApiError::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "/secret/path",
        ));
        assert_eq!(mapped.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped.message, "internal server error");
    }

    #[test]
    fn serde_json_parse_is_400_but_serialize_is_500() {
        let parse_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let mapped = ApiError::from(parse_err);
        assert_eq!(mapped.status, StatusCode::BAD_REQUEST);
        assert!(mapped.message.contains("invalid json"));

        let io_err = serde_json::Error::io(std::io::Error::other("sink full"));
        let mapped = ApiError::from(io_err);
        assert_eq!(mapped.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(mapped.message, "internal server error");
    }

    #[test]
    fn hostname_allowlist_matching() {
        // Exact host:port, case-insensitive host.
        assert!(hostname_matches("panel.example.com:8080", "PANEL.example.com:8080"));
        assert!(!hostname_matches("panel.example.com:8080", "panel.example.com:9090"));
        assert!(!hostname_matches("panel.example.com:8080", "evil.example.com:8080"));
        // Entry without a port accepts any port on that host.
        assert!(hostname_matches("panel.example.com", "panel.example.com"));
        assert!(hostname_matches("panel.example.com", "panel.example.com:8080"));
        assert!(!hostname_matches("panel.example.com", "panel.example.org:8080"));
        // IPv6 literals parse bracket-correctly.
        assert!(hostname_matches("[::1]:8080", "[::1]:8080"));
        assert!(hostname_matches("[::1]", "[::1]:8080"));
        assert!(!hostname_matches("[::1]:8080", "[::2]:8080"));
        // Bare IPv4.
        assert!(hostname_matches("127.0.0.1:8080", "127.0.0.1:8080"));
    }


    #[test]
    fn host_is_ip_literal_detects_ip_hosts() {
        // Bare and ported IPv4.
        assert!(host_is_ip_literal("127.0.0.1"));
        assert!(host_is_ip_literal("192.168.1.5:8080"));
        // Bracketed IPv6, with and without port.
        assert!(host_is_ip_literal("[::1]"));
        assert!(host_is_ip_literal("[::1]:8080"));
        assert!(host_is_ip_literal("[2001:db8::1]:8080"));
        // Hostnames are never IP literals — rebinding relies on them.
        assert!(!host_is_ip_literal("panel.example.com"));
        assert!(!host_is_ip_literal("PANEL.example.com:8080"));
    }
    #[test]
    fn split_host_port_handles_ipv6_and_ports() {
        assert_eq!(split_host_port("panel.example.com:8080"), ("panel.example.com", Some("8080")));
        assert_eq!(split_host_port("panel.example.com"), ("panel.example.com", None));
        assert_eq!(split_host_port("[::1]:8080"), ("[::1]", Some("8080")));
        assert_eq!(split_host_port("[::1]"), ("[::1]", None));
        assert_eq!(split_host_port("127.0.0.1:8080"), ("127.0.0.1", Some("8080")));
    }

    #[test]
    fn request_host_prefers_header_then_authority() {
        use axum::body::Body;
        // HTTP/1.1 shape: Host header present, no URI authority.
        let req = axum::http::Request::builder()
            .uri("/api/servers")
            .header(axum::http::header::HOST, "panel.example.com:8100")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&req).as_deref(), Some("panel.example.com:8100"));

        // HTTP/2 shape: no Host header, authority carried in the URI.
        let req = axum::http::Request::builder()
            .uri("https://panel.example.com:8100/api/servers")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&req).as_deref(), Some("panel.example.com:8100"));

        // Header wins when both are present (HTTP/1.1 authority lives in Host).
        let req = axum::http::Request::builder()
            .uri("https://authority.example.com/api/servers")
            .header(axum::http::header::HOST, "header.example.com")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&req).as_deref(), Some("header.example.com"));

        // Neither present -> None (relative URI, no Host).
        let req = axum::http::Request::builder()
            .uri("/api/servers")
            .body(Body::empty())
            .unwrap();
        assert_eq!(request_host(&req), None);
    }

    use std::sync::atomic::{AtomicI64, Ordering};
    // ---------------- API product surface ----------------

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        cfg.paths.datalab_dir = tmp.path().join("datalab");
        let hub = Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(crate::services::proc::ProcManager::new(
            db.clone(),
            hub.clone(),
            cfg.paths.datalab_dir.clone(),
        ));
        let watcher_engine = Arc::new(crate::services::watcher::WatcherEngine::new(
            db.clone(),
            Arc::new(crate::services::proc::Notifier::new()),
            Arc::downgrade(&hub),
            procs.clone(),
            Arc::new(crate::services::node::NodeClient::new().unwrap()),
            tokio::runtime::Handle::current(),
        ));
        let state = AppState {
            db,
            cfg,
            procs,
            hub,
            notifier: Arc::new(crate::services::proc::Notifier::new()),
            monitor: Arc::new(crate::services::Monitor::new()),
            node_client: Arc::new(crate::services::node::NodeClient::new().unwrap()),
            node_nonces: Arc::new(crate::services::node::NonceCache::default()),
            running: Arc::new(AtomicBool::new(true)),
            watcher_engine,
        };
        (tmp, state)
    }

    fn seed_user(state: &AppState) -> String {
        seed_user_named(state, "apiuser")
    }

    fn seed_user_named(state: &AppState, name: &str) -> String {
        let user_id = crate::models::create_user(
            &state.db,
            name,
            &format!("{name}@x.io"),
            "h",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            user_id,
            "api-tests",
            "127.0.0.1",
            false,
        )
        .unwrap();
        format!("{}={raw}", crate::auth::SESSION_COOKIE)
    }

    /// Seed a user and return both its id and session cookie — the at-cap
    /// test needs the id to reconstruct the middleware's composite cache key.
    fn seed_user_with_id(state: &AppState, name: &str) -> (i64, String) {
        let user_id = crate::models::create_user(
            &state.db,
            name,
            &format!("{name}@x.io"),
            "h",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db,
            &state.cfg,
            user_id,
            "api-tests",
            "127.0.0.1",
            false,
        )
        .unwrap();
        (user_id, format!("{}={raw}", crate::auth::SESSION_COOKIE))
    }

    /// Reconstruct the idempotency middleware's composite cache key
    /// (`{user}:{method}:{path}:{idem_key}`) for a POST /api/servers request.
    fn idem_cache_key(user_id: i64, idem_key: &str) -> String {
        format!("{user_id}:POST:/api/servers:{idem_key}")
    }

    #[tokio::test]
    async fn every_response_carries_the_request_id_header() {
        use axum::routing::get;
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route("/ok", get(|| async { "ok" }))
            .route("/broken", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))
            .layer(axum::middleware::from_fn(thread_request_id));
        for (uri, want) in [("/ok", 200), ("/broken", 500), ("/nope", 404)] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), want);
            let rid = response
                .headers()
                .get(REQUEST_ID_HEADER)
                .expect("every response carries x-volt-request-id")
                .to_str()
                .unwrap();
            assert!(!rid.is_empty());
            assert!(uuid::Uuid::parse_str(rid).is_ok(), "{rid} is not a uuid");
        }
    }

    static IDEM_TEST_CALLS: AtomicI64 = AtomicI64::new(0);

    async fn counting_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let call = IDEM_TEST_CALLS.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "call": call, "echo": body }))
    }

    async fn plain_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(body)
    }

    fn idem_router(state: AppState, handler: axum::routing::MethodRouter<AppState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/servers",
                handler.layer(axum::middleware::from_fn_with_state(state.clone(), idempotency)),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn idempotency_replays_cached_response_and_rejects_body_mismatch() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        use tower::ServiceExt;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        // Deterministic start: this is the only test that touches the
        // counter (the unauthenticated test uses a plain echo handler).
        IDEM_TEST_CALLS.store(0, Ordering::SeqCst);
        let app = idem_router(state, axum::routing::post(counting_echo));
        let key = format!("idem-replay-{}", uuid::Uuid::new_v4());
        let send = |app: axum::Router, body: &str, key: Option<&str>| {
            let mut builder = axum::http::Request::builder()
                .method("POST")
                .uri("/api/servers")
                .header("cookie", &cookie)
                .header("content-type", "application/json");
            if let Some(k) = key {
                builder = builder.header("idempotency-key", k);
            }
            let request = builder.body(Body::from(body.to_string())).unwrap();
            app.oneshot(request)
        };

        // First call executes the handler (call 0).
        let first = send(app.clone(), r#"{"name":"srv"}"#, Some(&key))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = to_bytes(first.into_body(), 1 << 20).await.unwrap();
        let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
        assert_eq!(first_json["call"], 0);

        // Replay: same key + same body returns the exact cached response and
        // never re-runs the handler.
        let replay = send(app.clone(), r#"{"name":"srv"}"#, Some(&key))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_body = to_bytes(replay.into_body(), 1 << 20).await.unwrap();
        assert_eq!(replay_body, first_body, "replay must return the cached body verbatim");
        let replay_json: serde_json::Value = serde_json::from_slice(&replay_body).unwrap();
        assert_eq!(replay_json["call"], 0, "handler must not run twice");

        // Same key, different body -> 409 conflict (handler not run).
        let conflict = send(app.clone(), r#"{"name":"other"}"#, Some(&key))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        // Without a key the handler always runs.
        let no_key = send(app.clone(), r#"{"name":"srv"}"#, None).await.unwrap();
        assert_eq!(no_key.status(), StatusCode::OK);
        let no_key_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(no_key.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(no_key_json["call"], 1, "no key means the handler runs");
    }

    #[tokio::test]
    async fn idempotency_passes_unauthenticated_requests_through() {
        use tower::ServiceExt;
        let (_tmp, state) = test_state();
        let app = idem_router(state, axum::routing::post(plain_echo));
        // No session cookie: the middleware must not cache or 409; the
        // handler's own auth path decides (here the echo handler runs).
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/servers")
                    .header("content-type", "application/json")
                    .header("idempotency-key", "no-auth-key")
                    .body(Body::from(r#"{"name":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ---------------- Idempotency concurrency ----------------

    // ---------------- Idempotency capacity & 4xx caching ----------------

    // Serializes the tests that depend on the shared cache's free capacity
    // (or that deliberately fill it to the cap): run against the
    // process-global IDEM_CACHE, they would corrupt each other's assertions.
    static IDEM_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    static CAP_OWNER_CALLS: AtomicI64 = AtomicI64::new(0);
    static CAP_OWNER_ENTERED: AtomicI64 = AtomicI64::new(0);
    /// Released by the at-cap test once it has asserted the live in-flight
    /// slots survived, so the owner holds its slot for exactly as long as the
    /// assertions need — no fixed sleep that a loaded test runner can outrun.
    static CAP_OWNER_RELEASE: LazyLock<Notify> = LazyLock::new(Notify::new);

    /// Gated echo used as a live owner in the at-cap test: it registers its
    /// in-flight slot, signals entry, then parks on `CAP_OWNER_RELEASE` until
    /// the test wakes it.
    async fn gated_cap_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let call = CAP_OWNER_CALLS.fetch_add(1, Ordering::SeqCst);
        let notified = CAP_OWNER_RELEASE.notified();
        CAP_OWNER_ENTERED.fetch_add(1, Ordering::SeqCst);
        notified.await;
        Json(serde_json::json!({ "call": call, "echo": body }))
    }

    static CAP_FRESH_CALLS: AtomicI64 = AtomicI64::new(0);

    /// Fast echo used for the fresh-key requests in the at-cap test.
    async fn cap_fresh_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let call = CAP_FRESH_CALLS.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "call": call, "echo": body }))
    }

    /// Live in-flight filler slots this test injects into the shared cache.
    /// Removed on drop — success or panic — so parallel tests never observe
    /// the artificial cap.
    struct IdemCacheFiller {
        keys: Vec<String>,
    }
    impl IdemCacheFiller {
        fn fill(prefix: &str, n: usize) -> Self {
            let keys: Vec<String> = (0..n).map(|i| format!("{prefix}-{i}")).collect();
            let mut cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
            for key in &keys {
                cache.insert(
                    key.clone(),
                    IdemSlot::InFlight(Arc::new(IdemInFlight {
                        request_hash: String::new(),
                        notify: Notify::new(),
                    })),
                );
            }
            IdemCacheFiller { keys }
        }
    }
    impl Drop for IdemCacheFiller {
        fn drop(&mut self) {
            let mut cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
            for key in &self.keys {
                cache.remove(key);
            }
        }
    }

    static CLIENT_ERROR_CALLS: AtomicI64 = AtomicI64::new(0);

    /// Handler that answers a JSON 400 after a short hold — a 4xx with a
    /// cacheable content-type, so only the status gate keeps it uncached.
    async fn gated_client_error_echo(Json(_body): Json<serde_json::Value>) -> Response {
        CLIENT_ERROR_CALLS.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "bad" }))).into_response()
    }

    static CONCURRENT_CALLS: AtomicI64 = AtomicI64::new(0);
    static CONCURRENT_ENTERED: AtomicI64 = AtomicI64::new(0);

    /// Counting echo that holds the response open briefly so concurrent
    /// followers can park behind the in-flight slot before it completes.
    async fn gated_counting_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let call = CONCURRENT_CALLS.fetch_add(1, Ordering::SeqCst);
        CONCURRENT_ENTERED.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        Json(serde_json::json!({ "call": call, "echo": body }))
    }

    static MISMATCH_CALLS: AtomicI64 = AtomicI64::new(0);
    static MISMATCH_ENTERED: AtomicI64 = AtomicI64::new(0);

    async fn gated_mismatch_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        MISMATCH_CALLS.fetch_add(1, Ordering::SeqCst);
        MISMATCH_ENTERED.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        Json(serde_json::json!({ "echo": body }))
    }

    static FAIL_CALLS: AtomicI64 = AtomicI64::new(0);

    /// Handler that fails after a short hold, used to prove a failed owner
    /// wakes followers instead of hanging them.
    async fn gated_failing_echo(Json(_body): Json<serde_json::Value>) -> Response {
        FAIL_CALLS.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }

    static ISOLATION_CALLS: AtomicI64 = AtomicI64::new(0);

    async fn isolation_echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let call = ISOLATION_CALLS.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "call": call, "echo": body }))
    }

    /// Echo with a Location and a cookie; the cookie and any request id must
    /// never be replayed.
    async fn located_echo(Json(body): Json<serde_json::Value>) -> Response {
        let json = serde_json::to_vec(&serde_json::json!({ "created": true, "echo": body })).unwrap();
        let mut response = Response::builder()
            .status(StatusCode::CREATED)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(json))
            .unwrap();
        response
            .headers_mut()
            .insert("location", HeaderValue::from_static("/api/servers/42"));
        response
            .headers_mut()
            .insert("set-cookie", HeaderValue::from_static("session=stale"));
        response
            .headers_mut()
            .insert(REQUEST_ID_HEADER, HeaderValue::from_static("stale-rid"));
        response
    }

    async fn idem_send(
        app: axum::Router,
        cookie: &str,
        body: &str,
        key: &str,
    ) -> axum::response::Response {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/servers")
            .header("cookie", cookie)
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(request).await.unwrap()
    }

    async fn idem_send_at(
        app: axum::Router,
        cookie: &str,
        method: &str,
        uri: &str,
        body: &str,
        key: &str,
    ) -> axum::response::Response {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie)
            .header("content-type", "application/json")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(request).await.unwrap()
    }

    async fn wait_until_entered(flag: &AtomicI64) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while flag.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(flag.load(Ordering::SeqCst), 1, "handler never entered");
    }

    #[tokio::test]
    async fn idempotency_serializes_concurrent_same_key_requests() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        CONCURRENT_CALLS.store(0, Ordering::SeqCst);
        CONCURRENT_ENTERED.store(0, Ordering::SeqCst);
        let app = idem_router(state, axum::routing::post(gated_counting_echo));
        let key = format!("idem-concurrent-{}", uuid::Uuid::new_v4());

        // Owner fires first and holds; followers must park, not execute.
        let owner = tokio::spawn({
            let app = app.clone();
            let cookie = cookie.clone();
            let key = key.clone();
            async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await }
        });
        wait_until_entered(&CONCURRENT_ENTERED).await;
        let followers: Vec<_> = (0..19)
            .map(|_| {
                let app = app.clone();
                let cookie = cookie.clone();
                let key = key.clone();
                tokio::spawn(async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await })
            })
            .collect();

        let mut responses = vec![owner.await.unwrap()];
        for follower in followers {
            responses.push(follower.await.unwrap());
        }
        let responses = {
            let mut out = Vec::with_capacity(responses.len());
            for response in responses {
                let status = response.status();
                let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
                out.push((status, body));
            }
            out
        };
        assert_eq!(
            CONCURRENT_CALLS.load(Ordering::SeqCst),
            1,
            "the underlying handler must execute exactly once for 20 concurrent same-key calls"
        );
        let (expected_status, expected) = &responses[0];
        for (status, body) in &responses[1..] {
            assert_eq!(status, expected_status);
            assert_eq!(
                body, expected,
                "replays must be byte-identical to the owner's response"
            );
        }
    }

    #[tokio::test]
    async fn idempotency_conflicts_on_different_body_while_in_flight() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        MISMATCH_CALLS.store(0, Ordering::SeqCst);
        MISMATCH_ENTERED.store(0, Ordering::SeqCst);
        let app = idem_router(state, axum::routing::post(gated_mismatch_echo));
        let key = format!("idem-mismatch-{}", uuid::Uuid::new_v4());

        let owner = tokio::spawn({
            let app = app.clone();
            let cookie = cookie.clone();
            let key = key.clone();
            async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await }
        });
        wait_until_entered(&MISMATCH_ENTERED).await;

        // A different body must 409 immediately, without waiting for the owner.
        let start = tokio::time::Instant::now();
        let conflict = idem_send(app.clone(), &cookie, r#"{"name":"other"}"#, &key).await;
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(250),
            "mismatch while in flight must answer 409 without waiting for the owner"
        );

        // The owner still completes normally and the mismatch never executed.
        assert_eq!(owner.await.unwrap().status(), StatusCode::OK);
        assert_eq!(MISMATCH_CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idempotency_failed_owner_wakes_followers_without_hanging() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        FAIL_CALLS.store(0, Ordering::SeqCst);
        let app = idem_router(state, axum::routing::post(gated_failing_echo));
        let key = format!("idem-fail-{}", uuid::Uuid::new_v4());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let app = app.clone();
                let cookie = cookie.clone();
                let key = key.clone();
                tokio::spawn(async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await })
            })
            .collect();

        let responses = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let mut responses = Vec::new();
            for handle in handles {
                responses.push(handle.await.unwrap());
            }
            responses
        })
        .await
        .expect("a failed owner must wake followers; the batch must not hang");

        for response in &responses {
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
        assert_eq!(
            FAIL_CALLS.load(Ordering::SeqCst),
            4,
            "each follower must receive its own answer after the owner's failure"
        );
        // A 5xx is never cached: the key must not be poisoned.
        let again = idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await;
        assert_eq!(again.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(FAIL_CALLS.load(Ordering::SeqCst), 5);
    }
    #[tokio::test]
    async fn idempotency_never_caches_4xx_and_wakes_followers() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        CLIENT_ERROR_CALLS.store(0, Ordering::SeqCst);
        let app = idem_router(state, axum::routing::post(gated_client_error_echo));
        let key = format!("idem-client-err-{}", uuid::Uuid::new_v4());

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let app = app.clone();
                let cookie = cookie.clone();
                let key = key.clone();
                tokio::spawn(async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await })
            })
            .collect();
        let responses = tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let mut responses = Vec::new();
            for handle in handles {
                responses.push(handle.await.unwrap());
            }
            responses
        })
        .await
        .expect("a 4xx owner must wake followers; the batch must not hang");

        for response in &responses {
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            CLIENT_ERROR_CALLS.load(Ordering::SeqCst),
            4,
            "a 4xx is never cached: each follower re-executes after the owner"
        );
        // A 4xx is not cached: the rejection does not poison the key.
        let again = idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await;
        assert_eq!(again.status(), StatusCode::BAD_REQUEST);
        assert_eq!(CLIENT_ERROR_CALLS.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn idempotency_at_cap_runs_uncached_without_clearing_inflight() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let (user_id, cookie) = seed_user_with_id(&state, "apiuser");
        CAP_OWNER_CALLS.store(0, Ordering::SeqCst);
        CAP_OWNER_ENTERED.store(0, Ordering::SeqCst);
        CAP_FRESH_CALLS.store(0, Ordering::SeqCst);
        let gated_app = idem_router(state.clone(), axum::routing::post(gated_cap_echo));
        let fresh_app = idem_router(state, axum::routing::post(cap_fresh_echo));
        let owner_key = format!("idem-cap-owner-{}", uuid::Uuid::new_v4());
        let fresh_key = format!("idem-cap-fresh-{}", uuid::Uuid::new_v4());

        // A real owner starts first so its in-flight slot is live in the map.
        let owner = tokio::spawn({
            let app = gated_app.clone();
            let cookie = cookie.clone();
            let key = owner_key.clone();
            async move { idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await }
        });
        wait_until_entered(&CAP_OWNER_ENTERED).await;

        // Fill the map to the cap with live in-flight slots while the owner
        // parks on its release signal, holding its in-flight slot open.
        let filler = IdemCacheFiller::fill(
            &format!("idem-cap-fill-{}", uuid::Uuid::new_v4()),
            IDEMPOTENCY_MAX_ENTRIES - 1,
        );

        // A fresh key at the cap runs the handler uncached — once per
        // request, never cached, never cleared.
        for _ in 0..2 {
            let response =
                idem_send(fresh_app.clone(), &cookie, r#"{"name":"srv"}"#, &fresh_key).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(
            CAP_FRESH_CALLS.load(Ordering::SeqCst),
            2,
            "at the cap each request executes; nothing is cached or cleared"
        );

        // The owner's in-flight slot and every filler survive the at-cap
        // insert; the uncached fresh key left no slot behind.
        {
            let cache = IDEM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                cache.contains_key(&idem_cache_key(user_id, &owner_key)),
                "the live owner's slot must never be cleared"
            );
            assert!(
                !cache.contains_key(&idem_cache_key(user_id, &fresh_key)),
                "the uncached path leaves no slot behind"
            );
            for key in &filler.keys {
                assert!(cache.contains_key(key), "in-flight slot {key} was cleared");
            }
        }

        // Assertions done: release the owner so it can complete.
        CAP_OWNER_RELEASE.notify_one();

        // The live owner is not duplicated and completes normally.
        assert_eq!(owner.await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            CAP_OWNER_CALLS.load(Ordering::SeqCst),
            1,
            "the owner executed exactly once"
        );

        // Degradation: the owner's completion could not be cached either (the
        // map is still full of live entries), so a retry with the same key
        // re-executes. Pre-arm the release so the retry's owner call (which
        // also parks on the signal) can complete.
        CAP_OWNER_RELEASE.notify_one();
        let retry = idem_send(gated_app, &cookie, r#"{"name":"srv"}"#, &owner_key).await;
        assert_eq!(retry.status(), StatusCode::OK);
        assert_eq!(CAP_OWNER_CALLS.load(Ordering::SeqCst), 2);
        drop(filler);
    }

    #[test]
    fn idem_insert_capped_at_cap_never_clears_or_evicts_live_entries() {
        let now = Utc::now().timestamp();
        let complete = |created: i64| {
            IdemSlot::Complete(IdemEntry {
                status: 200,
                body: Bytes::new(),
                content_type: "application/json".to_string(),
                headers: Vec::new(),
                request_hash: "h".to_string(),
                created_at: created,
            })
        };
        let in_flight = || {
            IdemSlot::InFlight(Arc::new(IdemInFlight {
                request_hash: String::new(),
                notify: Notify::new(),
            }))
        };

        // Cap filled with fresh completed entries: a new insert is refused
        // and nothing is cleared or evicted.
        let mut cache = HashMap::new();
        for i in 0..IDEMPOTENCY_MAX_ENTRIES {
            assert!(idem_insert_capped(&mut cache, format!("fresh-{i}"), complete(now), now));
        }
        assert_eq!(cache.len(), IDEMPOTENCY_MAX_ENTRIES);
        assert!(!idem_insert_capped(&mut cache, "late".to_string(), complete(now), now));
        assert_eq!(cache.len(), IDEMPOTENCY_MAX_ENTRIES);
        assert!(!cache.contains_key("late"));
        assert!(cache.contains_key("fresh-0") && cache.contains_key("fresh-9999"));

        // Cap filled with live in-flight slots: same refusal, same survival.
        let mut live = HashMap::new();
        for i in 0..IDEMPOTENCY_MAX_ENTRIES {
            assert!(idem_insert_capped(&mut live, format!("live-{i}"), in_flight(), now));
        }
        assert!(!idem_insert_capped(&mut live, "late2".to_string(), complete(now), now));
        assert_eq!(live.len(), IDEMPOTENCY_MAX_ENTRIES);
        assert!(live.contains_key("live-0") && live.contains_key("live-9999"));
        assert!(!live.contains_key("late2"));
    }

    #[test]
    fn idem_insert_capped_prunes_only_expired_completed_entries() {
        let now = Utc::now().timestamp();
        let expired = now - IDEMPOTENCY_TTL_SECS - 1;
        let complete = |created: i64, hash: &str| {
            IdemSlot::Complete(IdemEntry {
                status: 200,
                body: Bytes::new(),
                content_type: "application/json".to_string(),
                headers: Vec::new(),
                request_hash: hash.to_string(),
                created_at: created,
            })
        };

        let mut cache = HashMap::new();
        for i in 0..(IDEMPOTENCY_MAX_ENTRIES - 2) {
            assert!(idem_insert_capped(
                &mut cache,
                format!("expired-{i}"),
                complete(expired, "x"),
                now
            ));
        }
        // One fresh completed entry and one live in-flight slot round out the
        // cap.
        assert!(idem_insert_capped(&mut cache, "fresh".to_string(), complete(now, "fresh"), now));
        assert!(idem_insert_capped(
            &mut cache,
            "inflight".to_string(),
            IdemSlot::InFlight(Arc::new(IdemInFlight {
                request_hash: "f".to_string(),
                notify: Notify::new(),
            })),
            now
        ));
        assert_eq!(cache.len(), IDEMPOTENCY_MAX_ENTRIES);

        // An insert at the cap prunes only the expired completed entries; the
        // fresh completed entry and the live in-flight slot survive, and the
        // freed room admits the new key.
        assert!(idem_insert_capped(&mut cache, "new".to_string(), complete(now, "new"), now));
        assert!(cache.contains_key("fresh"));
        assert!(cache.contains_key("inflight"));
        assert!(cache.contains_key("new"));
        assert!(!cache.contains_key("expired-0"));
        assert_eq!(cache.len(), 3);
    }


    #[tokio::test]
    async fn idempotency_replay_preserves_location_but_never_cookies_or_request_ids() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie = seed_user(&state);
        let app = idem_router(state, axum::routing::post(located_echo));
        let key = format!("idem-headers-{}", uuid::Uuid::new_v4());

        let first = idem_send(app.clone(), &cookie, r#"{"name":"srv"}"#, &key).await;
        assert_eq!(first.status(), StatusCode::CREATED);
        assert_eq!(
            first.headers().get("location").unwrap().to_str().unwrap(),
            "/api/servers/42"
        );
        assert!(
            first.headers().contains_key("set-cookie"),
            "owner response carries a cookie"
        );

        let replay = idem_send(app, &cookie, r#"{"name":"srv"}"#, &key).await;
        assert_eq!(replay.status(), StatusCode::CREATED);
        assert_eq!(
            replay.headers().get("location").unwrap().to_str().unwrap(),
            "/api/servers/42",
            "Location must survive replay"
        );
        assert_eq!(
            replay
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            first
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
        );
        assert!(
            !replay.headers().contains_key("set-cookie"),
            "Set-Cookie must never be replayed"
        );
        assert!(
            !replay.headers().contains_key(REQUEST_ID_HEADER),
            "x-volt-request-id must be minted fresh per request, never replayed"
        );
    }

    #[tokio::test]
    async fn idempotency_cache_is_isolated_by_user_method_and_path() {
        let _guard = IDEM_TEST_LOCK.lock().await;
        let (_tmp, state) = test_state();
        let cookie_a = seed_user_named(&state, "isolation-a");
        let cookie_b = seed_user_named(&state, "isolation-b");
        ISOLATION_CALLS.store(0, Ordering::SeqCst);
        let echo = axum::routing::post(isolation_echo).layer(axum::middleware::from_fn_with_state(
            state.clone(),
            idempotency,
        ));
        let put = axum::routing::put(isolation_echo)
            .layer(axum::middleware::from_fn_with_state(state.clone(), idempotency));
        let app = axum::Router::new()
            .route("/api/a", echo.clone())
            .route("/api/b", echo)
            .route("/api/a", put)
            .with_state(state);
        let key = format!("idem-isolation-{}", uuid::Uuid::new_v4());
        let body = r#"{"name":"srv"}"#;

        let first = idem_send_at(app.clone(), &cookie_a, "POST", "/api/a", body, &key).await;
        assert_eq!(first.status(), StatusCode::OK);
        let replay = idem_send_at(app.clone(), &cookie_a, "POST", "/api/a", body, &key).await;
        assert_eq!(replay.status(), StatusCode::OK);
        // Same key + body, different user -> independent execution.
        let other_user = idem_send_at(app.clone(), &cookie_b, "POST", "/api/a", body, &key).await;
        assert_eq!(other_user.status(), StatusCode::OK);
        // Same user + path, different method.
        let other_method = idem_send_at(app.clone(), &cookie_a, "PUT", "/api/a", body, &key).await;
        assert_eq!(other_method.status(), StatusCode::OK);
        // Same user + method, different path.
        let other_path = idem_send_at(app.clone(), &cookie_a, "POST", "/api/b", body, &key).await;
        assert_eq!(other_path.status(), StatusCode::OK);

        assert_eq!(
            ISOLATION_CALLS.load(Ordering::SeqCst),
            4,
            "one execution per distinct user/method/path"
        );
        let replay_body: serde_json::Value =
            serde_json::from_slice(&to_bytes(replay.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(replay_body["call"], 0, "the replay returns the cached body");
    }

    #[test]
    fn openapi_document_lists_representative_paths_and_auth_schemes() {
        let doc = openapi_doc();
        assert_eq!(doc["openapi"], "3.0.3");
        assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
        let paths = doc["paths"].as_object().expect("paths is an object");
        for p in [
            "/api/login",
            "/api/servers",
            "/api/servers/{id}/backups",
            "/api/backups/{id}/restore",
            "/api/backups/{id}/download",
            "/api/schedules/{id}/run",
            "/api/meta",
            "/api/meta/openapi.json",
            // Iteration-11 families.
            "/api/2fa/recovery/regenerate",
            "/api/admin/users/{id}/2fa/reset",
            "/api/nodes/{id}/drain",
            "/api/servers/{id}/databases/{name}/export",
            "/api/servers/{id}/databases/{name}/import",
            "/api/notifications",
            "/api/notifications/stream",
            "/api/notifications/{id}/read",
            "/api/notifications/clear",
            "/api/servers/{id}/sites",
            "/api/servers/{id}/sites/{site_id}",
        ] {
            assert!(paths.contains_key(p), "openapi document is missing path {p}");
        }
        // Every new-family path must mount exactly the methods the router
        // registers, so a dropped route cannot hide behind a stale doc.
        let family_methods: &[(&str, &[&str])] = &[
            ("/api/2fa/recovery/regenerate", &["post"]),
            ("/api/admin/users/{id}/2fa/reset", &["post"]),
            ("/api/nodes/{id}/drain", &["delete", "post"]),
            ("/api/servers/{id}/databases/{name}/export", &["get"]),
            ("/api/servers/{id}/databases/{name}/import", &["post"]),
            ("/api/notifications", &["get"]),
            ("/api/notifications/stream", &["get"]),
            ("/api/notifications/{id}/read", &["post"]),
            ("/api/notifications/clear", &["post"]),
            ("/api/servers/{id}/sites", &["get", "post"]),
            ("/api/servers/{id}/sites/{site_id}", &["delete", "get", "patch"]),
        ];
        for (path, methods) in family_methods {
            let ops = paths[*path].as_object().expect("family path present");
            let mut mounted: Vec<&str> = ops.keys().map(String::as_str).collect();
            mounted.sort_unstable();
            assert_eq!(mounted, *methods, "{path} must mount exactly {methods:?}");
        }
        // Streaming families advertise a real content type, not JSON.
        let export = &paths["/api/servers/{id}/databases/{name}/export"]["get"]["responses"]["200"];
        assert_eq!(
            export["content"]["application/octet-stream"]["schema"]["format"],
            "binary"
        );
        let sse = &paths["/api/notifications/stream"]["get"]["responses"]["200"];
        assert!(sse["content"]["text/event-stream"].is_object());
        // Idempotent mutations document the replay-safe header.
        for p in [
            "/api/servers",
            "/api/servers/{id}/backups",
            "/api/backups/{id}/restore",
            "/api/schedules/{id}/run",
        ] {
            let params = &paths[p]["post"]["parameters"];
            assert!(
                params
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|q| q["name"] == "Idempotency-Key" && q["in"] == "header"),
                "{p} must document the Idempotency-Key header"
            );
        }
        // Login documents the 2FA recovery-code path.
        let login_schema =
            &paths["/api/login"]["post"]["requestBody"]["content"]["application/json"]["schema"];
        assert!(login_schema["properties"]["recovery_code"].is_object());
        let schemes = &doc["components"]["securitySchemes"];
        assert!(schemes["cookieAuth"].is_object(), "cookieAuth scheme present");
        assert!(schemes["bearerAuth"].is_object(), "bearerAuth scheme present");
        assert!(doc["components"]["schemas"]["Error"].is_object());
        // Every operation references the shared error envelope.
        for (path, ops) in paths {
            let ops = ops.as_object().unwrap();
            for (method, op) in ops {
                assert!(
                    op["responses"]["4XX"].is_object() && op["responses"]["5XX"].is_object(),
                    "{method} {path} must reference the error envelope"
                );
            }
        }
    }

    #[tokio::test]
    async fn meta_reports_version_features_limits_and_node_protocol() {
        let (_tmp, state) = test_state();
        let Json(meta) = meta(State(state)).await;
        assert_eq!(meta["name"], "voltpanel");
        assert_eq!(meta["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(meta["api"]["version"], 1);
        assert_eq!(meta["node_protocol"], NODE_PROTOCOL_VERSION);
        assert!(meta["features"]["backups"].is_boolean());
        assert!(meta["limits"]["max_servers_per_user"].is_number());
        assert!(meta["rate_limit_per_min"].is_number());
        assert_eq!(meta["api"]["docs"], "/api/meta/openapi.json");
    }

    #[test]
    fn rate_limit_headers_report_limit_and_reset() {
        let mut approved = StatusCode::OK.into_response();
        let before = Utc::now().timestamp();
        apply_rate_limit_headers(&mut approved, 120, None);
        let headers = approved.headers();
        assert_eq!(headers.get(RATE_LIMIT_LIMIT).unwrap().to_str().unwrap(), "120");
        assert!(headers.get(RATE_LIMIT_REMAINING).is_none(), "remaining omitted when unavailable");
        let reset: i64 = headers
            .get(RATE_LIMIT_RESET)
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(reset > before && reset <= before + 60, "reset is the next minute boundary");

        let mut denied = StatusCode::TOO_MANY_REQUESTS.into_response();
        apply_rate_limit_headers(&mut denied, 120, Some(0));
        assert_eq!(
            denied.headers().get(RATE_LIMIT_REMAINING).unwrap().to_str().unwrap(),
            "0",
            "a rejected request provably has nothing remaining"
        );
    }
}