//! API layer: shared state, error type, extractors.
pub mod backups;
pub mod blueprints;
pub mod console;
pub mod databases;
pub mod files;
pub mod metrics;
pub mod keys;
pub mod nodes;
pub mod schedules;
pub mod servers;
pub mod settings;
pub mod sites;
pub mod system;
pub mod users;
pub mod webhooks;

use crate::capability::Capability;
use crate::config::Config;
use crate::db::Db;
use crate::models::User;
use crate::services::{proc, ConsoleHub, Monitor};
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
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
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message, "status": self.status.as_u16() });
        (self.status, Json(body)).into_response()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::bad_request(format!("io error: {e}"))
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
        tracing::warn!("api error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        tracing::warn!("db error: {e}");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::bad_request(format!("invalid json: {e}"))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ---------------- Auth extractor ----------------

/// Extract the authenticated user from a session cookie.
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
                let (mut user, scope) = crate::services::keys::authenticate(&state.db, raw)
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
        let user = crate::auth::user_from_session(&state.db, &raw)
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
            if !scope.wildcard || !scope.server_ids.is_empty() {
                return Err(ApiError::forbidden(
                    "scoped API keys cannot access admin endpoints",
                ));
            }
        }
        Ok(AdminUser(u))
    }
}

// ---------------- Helpers ----------------

/// JSON success wrapper.
pub fn ok<T: serde::Serialize>(v: T) -> Json<serde_json::Value> {
    Json(serde_json::json!(v))
}

/// Standard JSON envelope: { success, data } or plain.
pub fn data(v: serde_json::Value) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "data": v }))
}

/// Enforce a typed capability on a server-scoped route.
pub fn require_capability(
    state: &AppState,
    user: &User,
    server_id: i64,
    capability: Capability,
) -> ApiResult<()> {
    if crate::models::user_has_capability(&state.db, user, server_id, capability)? {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!(
            "missing server capability: {}",
            capability.as_str()
        )))
    }
}

pub fn client_ip(parts: &Parts) -> String {
    parts
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
