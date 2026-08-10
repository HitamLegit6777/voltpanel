//! Webhook endpoints: subscriptions and delivery history.

use super::{data, ok, AdminUser, ApiError, ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::db::blocking;
use crate::services::webhooks;

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread (see servers.rs
// for the full contract). `blocking` runs the pool-based `services::webhooks`
// functions on Tokio's blocking pool. Never hold a pooled connection across
// an `.await` — split into separate blocking units instead.
#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub server_id: Option<i64>,
    /// Opt in to a plain-http target URL. https is the default and always
    /// accepted; http URLs require this flag.
    #[serde(default)]
    pub allow_http: bool,
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub name: Option<String>,
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub secret: Option<String>,
    pub server_id: Option<Option<i64>>,
    pub enabled: Option<bool>,
    pub allow_http: Option<bool>,
}


#[derive(Deserialize)]
pub struct DeliveriesQuery {
    pub limit: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = blocking(state.db.clone(), move |db| webhooks::list(&db)).await?;
    Ok(data(json!(rows)))
}

pub async fn create(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let events = req.events.unwrap_or_else(|| vec!["*".to_string()]);
    webhooks::validate_events(&events).map_err(|e| ApiError::bad_request(e.to_string()))?;
    // Pre-flight with the requested opt-in so a http URL without the flag is
    // rejected here with a clear 400 (the service create mirrors it).
    webhooks::client_for_target_opts(&req.url, req.allow_http)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let name = req.name;
    let url = req.url;
    let server_id = req.server_id;
    let allow_http = req.allow_http;
    let wh = blocking(state.db.clone(), move |db| {
        webhooks::create(&db, &name, &url, &events, server_id, allow_http)
    })
    .await?;
    // The secret is generated once and shown only to the creator.
    let mut out = serde_json::to_value(&wh)?;
    out["secret"] = json!(wh.secret);
    Ok(data(out))
}

pub async fn get(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let wh = blocking(state.db.clone(), move |db| webhooks::get(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    Ok(data(json!(wh)))
}

pub async fn update(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let existing = blocking(state.db.clone(), move |db| webhooks::get(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    if let Some(events) = &req.events {
        webhooks::validate_events(events).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(url) = &req.url {
        // The effective flag is the patch value when present, otherwise what
        // the webhook already has: toggling allow_http off against a
        // plain-http URL is rejected in the same round trip.
        let allow_http = req.allow_http.unwrap_or(existing.allow_http);
        webhooks::client_for_target_opts(url, allow_http)
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let name = req.name;
    let url = req.url;
    let events = req.events;
    let secret = req.secret;
    let server_id = req.server_id;
    let enabled = req.enabled;
    let allow_http = req.allow_http;
    let wh = blocking(state.db.clone(), move |db| {
        let patch = webhooks::WebhookPatch {
            name: name.as_deref(),
            url: url.as_deref(),
            events: events.as_deref(),
            secret: secret.as_deref(),
            server_id,
            enabled,
            allow_http,
        };
        webhooks::update(&db, id, patch)
    })
    .await?;
    Ok(data(json!(wh)))
}

pub async fn delete(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(state.db.clone(), move |db| webhooks::delete(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let wh = blocking(state.db.clone(), move |db| webhooks::get(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    let enabled = !wh.enabled;
    let updated = blocking(state.db.clone(), move |db| webhooks::set_enabled(&db, id, enabled))
        .await?;
    Ok(data(json!({ "enabled": updated.enabled })))
}

pub async fn deliveries(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(state.db.clone(), move |db| webhooks::get(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows = blocking(state.db.clone(), move |db| webhooks::deliveries(&db, id, limit)).await?;
    Ok(data(json!(rows)))
}

/// Queue a synthetic delivery straight to this webhook. Deliberately bypasses
/// subscription matching: the operator picked the target, and no hook
/// subscribes to the synthetic test event.
pub async fn test(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let wh = blocking(state.db.clone(), move |db| webhooks::get(&db, id))
        .await
        .map_err(|_| ApiError::not_found("webhook not found"))?;
    if !wh.enabled {
        return Err(ApiError::bad_request("webhook is disabled"));
    }
    let payload = json!({
        "event": webhooks::TEST_EVENT,
        "webhook_id": wh.id,
        "sent_at": chrono::Utc::now().to_rfc3339(),
    });
    let wh_id = wh.id;
    blocking(state.db.clone(), move |db| {
        webhooks::enqueue_one(&db, wh_id, webhooks::TEST_EVENT, payload)
    })
    .await?;
    Ok(data(
        json!({ "enqueued": 1, "event": webhooks::TEST_EVENT }),
    ))
}