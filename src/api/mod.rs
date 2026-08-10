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
pub mod webhooks;

use crate::capability::Capability;
use crate::config::{default_hostnames, Config, IpNet};
use crate::db::Db;
use crate::models::User;
use crate::services::{proc, ConsoleHub, Monitor};
use axum::extract::{ConnectInfo, FromRef, FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
    let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
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

/// Mint one correlation id per request and expose it via the `REQUEST_ID`
/// task-local. Outermost layer: transparent (never short-circuits), so the
/// TraceLayer below it still observes every request and response.
pub async fn thread_request_id(request: Request, next: Next) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    crate::REQUEST_ID.scope(id, next.run(request)).await
}
pub fn ok<T: serde::Serialize>(v: T) -> Json<serde_json::Value> {
    Json(serde_json::json!(v))
}

/// Standard JSON envelope: { success, data } or plain.
pub fn data(v: serde_json::Value) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": v }))
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
    let Some(host) = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing Host header").into_response();
    };
    if state.cfg.web.hostnames.is_empty() {
        // Derived mode: IP-literal Hosts pass unconditionally; anything else
        // must match the derived defaults (listen address, loopback aliases,
        // machine hostname).
        if host_is_ip_literal(host) {
            return next.run(request).await;
        }
        let derived = default_hostnames(&state.cfg.web.listen, &state.cfg.web.tls_extra_sans);
        if derived
            .iter()
            .any(|candidate| hostname_matches(candidate, host))
        {
            return next.run(request).await;
        }
    } else if state
        .cfg
        .web
        .hostnames
        .iter()
        .any(|candidate| hostname_matches(candidate, host))
    {
        return next.run(request).await;
    }
    (StatusCode::BAD_REQUEST, "unknown Host header").into_response()
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
    let key = format!("api-mut:{ip}");
    match tokio::task::spawn_blocking(move || crate::auth::rate_limit(&db, &cfg, &key)).await {
        Ok(Ok(true)) => next.run(request).await,
        Ok(Ok(false)) => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many requests; try again later",
        )
        .into_response(),
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
}