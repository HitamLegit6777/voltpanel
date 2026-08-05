//! Panel settings endpoints + audit logs.
use super::{data, ok, ApiResult, AdminUser, AppState, AuthUser};
use crate::config::Config;
use crate::models;
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

/// Public panel settings (no secrets).
pub async fn public(State(state): State<AppState>, _u: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "instance_name": state.cfg.general.instance_name,
        "locale": state.cfg.general.locale,
        "features": {
            "backups": state.cfg.features.enable_backups,
            "databases": state.cfg.features.enable_databases,
            "schedules": state.cfg.features.enable_schedules,
            "api_keys": state.cfg.features.enable_api_keys,
            "2fa": state.cfg.features.enable_2fa,
            "websites": state.cfg.features.enable_websites,
        },
        "limits": {
            "max_memory_mb": state.cfg.limits.max_memory_mb,
            "max_servers_per_user": state.cfg.limits.max_servers_per_user,
        }
    })))
}

/// Read-only settings exposed to any logged-in user.
pub async fn get(State(state): State<AppState>, _u: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let settings = models::all_settings(&state.db)?;
    Ok(data(json!(settings)))
}

#[derive(Deserialize)]
pub struct SetReq {
    pub key: String,
    pub value: String,
}

pub async fn set(State(state): State<AppState>, _a: AdminUser, Json(req): Json<SetReq>) -> ApiResult<Json<serde_json::Value>> {
    models::set_setting(&state.db, &req.key, &req.value)?;
    Ok(ok(json!({ "ok": true })))
}

pub async fn config_view(State(state): State<AppState>, _a: AdminUser) -> ApiResult<Json<Config>> {
    Ok(Json(state.cfg.clone()))
}

#[derive(Deserialize)]
pub struct LimitsReq {
    pub default_memory_mb: Option<u64>,
    pub default_disk_mb: Option<u64>,
    pub default_cpu_percent: Option<u64>,
    pub max_memory_mb: Option<u64>,
    pub max_servers_per_user: Option<u64>,
}

/// Update node-wide resource limits at runtime (persisted in settings table).
pub async fn update_limits(State(state): State<AppState>, _a: AdminUser, Json(req): Json<LimitsReq>) -> ApiResult<Json<serde_json::Value>> {
    if let Some(v) = req.default_memory_mb {
        models::set_setting(&state.db, "limits.default_memory_mb", &v.to_string())?;
    }
    if let Some(v) = req.default_disk_mb {
        models::set_setting(&state.db, "limits.default_disk_mb", &v.to_string())?;
    }
    if let Some(v) = req.default_cpu_percent {
        models::set_setting(&state.db, "limits.default_cpu_percent", &v.to_string())?;
    }
    if let Some(v) = req.max_memory_mb {
        models::set_setting(&state.db, "limits.max_memory_mb", &v.to_string())?;
    }
    if let Some(v) = req.max_servers_per_user {
        models::set_setting(&state.db, "limits.max_servers_per_user", &v.to_string())?;
    }
    Ok(ok(json!({ "ok": true })))
}

// ---------------- Audit logs ----------------

pub async fn audit_logs(State(state): State<AppState>, _a: AdminUser) -> ApiResult<Json<serde_json::Value>> {
    let logs = models::list_audit_logs(&state.db, 500)?;
    Ok(data(serde_json::to_value(logs)?))
}

// ---------------- Notifications ----------------

pub async fn notifications(State(state): State<AppState>, _u: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(data(json!(state.notifier.history())))
}

pub async fn notifications_clear(State(state): State<AppState>, _u: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    state.notifier.clear();
    Ok(ok(json!({ "ok": true })))
}
