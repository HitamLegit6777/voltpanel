//! Webhook endpoints: subscriptions and delivery history.

use super::{data, ok, AdminUser, ApiError, ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::services::webhooks;

#[derive(Deserialize)]
pub struct CreateReq {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub events: Option<Vec<String>>,
    #[serde(default)]
    pub server_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateReq {
    pub name: Option<String>,
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub secret: Option<String>,
    pub server_id: Option<Option<i64>>,
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
pub struct DeliveriesQuery {
    pub limit: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = webhooks::list(&state.db)?;
    Ok(data(json!(rows)))
}

pub async fn create(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let events = req.events.unwrap_or_else(|| vec!["*".to_string()]);
    webhooks::validate_events(&events).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let wh = webhooks::create(&state.db, &req.name, &req.url, &events, req.server_id)?;
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
    let wh = webhooks::get(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    Ok(data(json!(wh)))
}

pub async fn update(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    webhooks::get(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    if let Some(events) = &req.events {
        webhooks::validate_events(events).map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let patch = webhooks::WebhookPatch {
        name: req.name.as_deref(),
        url: req.url.as_deref(),
        events: req.events.as_deref(),
        secret: req.secret.as_deref(),
        server_id: req.server_id,
        enabled: req.enabled,
    };
    let wh = webhooks::update(&state.db, id, patch)?;
    Ok(data(json!(wh)))
}

pub async fn delete(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    webhooks::delete(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn toggle(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let wh = webhooks::get(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    let updated = webhooks::set_enabled(&state.db, id, !wh.enabled)?;
    Ok(data(json!({ "enabled": updated.enabled })))
}

pub async fn deliveries(
    State(state): State<AppState>,
    AdminUser(_u): AdminUser,
    Path(id): Path<i64>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    webhooks::get(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    let rows = webhooks::deliveries(&state.db, id, q.limit.unwrap_or(50))?;
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
    let wh = webhooks::get(&state.db, id).map_err(|_| ApiError::not_found("webhook not found"))?;
    if !wh.enabled {
        return Err(ApiError::bad_request("webhook is disabled"));
    }
    let payload = json!({
        "event": webhooks::TEST_EVENT,
        "webhook_id": wh.id,
        "sent_at": chrono::Utc::now().to_rfc3339(),
    });
    webhooks::enqueue_one(&state.db, wh.id, webhooks::TEST_EVENT, payload)?;
    Ok(data(
        json!({ "enqueued": 1, "event": webhooks::TEST_EVENT }),
    ))
}
