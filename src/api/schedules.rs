//! Schedule endpoints.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::models::{self, User};
use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

fn access_ok(state: &AppState, user: &User, server_id: i64) -> ApiResult<crate::models::Server> {
    let s = models::get_server(&state.db, server_id)
        .map_err(|_| ApiError::not_found("server not found"))?;
    if !models::user_has_server_access(&state.db, user, s.id)? {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, server_id)?;
    super::require_server_permission(&state, &u, server_id, "schedule.read")?;
    let schedules = models::list_schedules(&state.db, server_id)?;
    Ok(data(serde_json::to_value(schedules)?))
}

#[derive(Deserialize)]
pub struct CreateScheduleReq {
    pub name: String,
    pub cron_expr: String,
    pub enabled: bool,
    pub tasks: Vec<ScheduleTaskReq>,
}

#[derive(Deserialize)]
pub struct ScheduleTaskReq {
    pub action: String,
    pub payload: Option<String>,
    pub sequence: i64,
}

pub async fn create(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateScheduleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    access_ok(&state, &u, server_id)?;
    super::require_server_permission(&state, &u, server_id, "schedule.write")?;
    for task in &req.tasks {
        if !matches!(
            task.action.as_str(),
            "start" | "stop" | "restart" | "kill" | "command" | "backup"
        ) {
            return Err(ApiError::bad_request(format!(
                "unsupported schedule action: {}",
                task.action
            )));
        }
    }
    crate::services::scheduler::parse_cron(&req.cron_expr)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let id = models::create_schedule(&state.db, server_id, &req.name, &req.cron_expr, req.enabled)?;
    for t in &req.tasks {
        models::add_schedule_task(
            &state.db,
            id,
            &t.action,
            t.payload.as_deref().unwrap_or(""),
            t.sequence,
        )?;
    }
    if req.enabled {
        let next = crate::services::scheduler::next_run(&req.cron_expr, chrono::Utc::now())?;
        models::set_schedule_next(&state.db, id, Some(&next.to_rfc3339()))?;
    }
    Ok(Json(serde_json::to_value(models::get_schedule(
        &state.db, id,
    )?)?))
}

#[derive(Deserialize)]
pub struct UpdateScheduleReq {
    pub name: Option<String>,
    pub cron_expr: Option<String>,
    pub enabled: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateScheduleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    if let Some(name) = req.name {
        sch.name = name;
    }
    if let Some(expr) = req.cron_expr {
        crate::services::scheduler::parse_cron(&expr)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        sch.cron_expr = expr;
    }
    if let Some(en) = req.enabled {
        sch.enabled = en;
    }
    models::update_schedule(&state.db, &sch)?;
    if sch.enabled {
        let next = crate::services::scheduler::next_run(&sch.cron_expr, chrono::Utc::now())?;
        models::set_schedule_next(&state.db, sch.id, Some(&next.to_rfc3339()))?;
    } else {
        models::set_schedule_next(&state.db, sch.id, None)?;
    }
    Ok(Json(serde_json::to_value(models::get_schedule(
        &state.db, id,
    )?)?))
}

pub async fn delete(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    models::delete_schedule(&state.db, id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AddTaskReq {
    pub action: String,
    pub payload: Option<String>,
    pub sequence: i64,
}

pub async fn add_task(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<AddTaskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    if !matches!(
        req.action.as_str(),
        "start" | "stop" | "restart" | "kill" | "command" | "backup"
    ) {
        return Err(ApiError::bad_request("unsupported schedule action"));
    }
    let _tid = models::add_schedule_task(
        &state.db,
        id,
        &req.action,
        req.payload.as_deref().unwrap_or(""),
        req.sequence,
    )?;
    Ok(Json(serde_json::to_value(models::get_schedule(
        &state.db, id,
    )?)?))
}
pub async fn remove_task(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, task_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    models::delete_schedule_task(&state.db, task_id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn runs(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.read")?;
    let conn = state.db.lock();
    let mut stmt = conn.prepare("SELECT id,schedule_id,triggered_at,status,log FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT 50")?;
    let rows = stmt.query_map([id], |r| {
        Ok(serde_json::json!({
            "id": r.get::<_, i64>(0)?,
            "schedule_id": r.get::<_, i64>(1)?,
            "triggered_at": r.get::<_, String>(2)?,
            "status": r.get::<_, String>(3)?,
            "log": r.get::<_, String>(4)?,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(data(serde_json::json!(out)))
}

pub async fn toggle(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, enabled)): Path<(i64, bool)>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    sch.enabled = enabled;
    models::update_schedule(&state.db, &sch)?;
    if enabled {
        let next = crate::services::scheduler::next_run(&sch.cron_expr, chrono::Utc::now())?;
        models::set_schedule_next(&state.db, sch.id, Some(&next.to_rfc3339()))?;
    } else {
        models::set_schedule_next(&state.db, sch.id, None)?;
    }
    Ok(ok(serde_json::json!({ "enabled": enabled })))
}

pub async fn run_now(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let sch = models::get_schedule(&state.db, id)?;
    access_ok(&state, &u, sch.server_id)?;
    super::require_server_permission(&state, &u, sch.server_id, "schedule.write")?;
    let server = models::get_server(&state.db, sch.server_id)?;
    if server.suspended {
        return Err(ApiError::forbidden("server suspended"));
    }
    let mut sch = sch;
    let scheduler = crate::services::scheduler::Scheduler {
        db: state.db.clone(),
        procs: state.procs.clone(),
        hub: state.hub.clone(),
        notifier: state.notifier.clone(),
        node_client: state.node_client.clone(),
        running: state.running.clone(),
    };
    scheduler.execute(&mut sch).await?;
    Ok(ok(serde_json::json!({ "started": true })))
}
