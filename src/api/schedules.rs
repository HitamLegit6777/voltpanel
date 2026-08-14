//! Schedule endpoints.
use super::{data, ok, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, User};
use crate::services::scheduler::parse_schedule_tz;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;


#[derive(Deserialize)]
pub struct RunsQuery {
    /// 1-based page number; defaults to 1.
    pub page: Option<u32>,
    /// Page size; defaults to 50, capped at 100.
    pub limit: Option<u32>,
}

async fn access_ok(
    state: &AppState,
    user: &User,
    server_id: i64,
) -> ApiResult<crate::models::Server> {
    let s = blocking(state.db.clone(), move |db| {
        models::get_server(&db, server_id)
    })
    .await
    .map_err(|_| ApiError::not_found("server not found"))?;
    let user = user.clone();
    let sid = s.id;
    if !blocking(state.db.clone(), move |db| {
        models::user_has_server_access(&db, &user, sid)
    })
    .await?
    {
        return Err(ApiError::forbidden("no access to this server"));
    }
    Ok(s)
}

fn schedules_enabled(state: &AppState) -> ApiResult<()> {
    if state.cfg.features.enable_schedules {
        Ok(())
    } else {
        Err(ApiError::not_found("schedules are disabled"))
    }
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ScheduleRead).await?;
    let schedules =
        blocking(state.db.clone(), move |db| {
            crate::services::scheduler::schedule_list_json(&db, server_id)
        })
        .await?;
    Ok(data(schedules))
}

#[derive(Deserialize)]
pub struct CreateScheduleReq {
    pub name: String,
    pub cron_expr: String,
    pub enabled: bool,
    pub tasks: Vec<ScheduleTaskReq>,
    pub max_retries: Option<i64>,
    pub retry_backoff_s: Option<i64>,
    pub only_when_online: Option<bool>,
    /// Per-schedule timezone: "UTC" (default) or a fixed offset like
    /// "+05:30". Cron fires are evaluated against this local wall clock.
    pub timezone: Option<String>,
}

#[derive(Deserialize)]
pub struct ScheduleTaskReq {
    pub action: String,
    pub payload: Option<String>,
    pub sequence: i64,
    /// Flow Gate: canonical condition JSON attached to this task, e.g.
    /// `{"kind":"exit","task_index":0,"code":0}` or
    /// `{"kind":"signal","event":"site.updated","server_id":1,"timeout_s":60}`.
    /// Absent or `{"kind":"none"}` = the task runs unconditionally.
    pub condition: Option<serde_json::Value>,
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<CreateScheduleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ScheduleWrite).await?;
    let mut triples: Vec<(String, String, i64)> = Vec::with_capacity(req.tasks.len());
    let mut conds: Vec<Option<String>> = Vec::with_capacity(req.tasks.len());
    for (idx, task) in req.tasks.iter().enumerate() {
        // The chain orders tasks by (sequence, id), so a request position
        // only equals a chain position when the submitted sequences are
        // strictly increasing. Anything else would validate and attach an
        // exit gate against the wrong task — reject it up front.
        if idx > 0 && task.sequence <= req.tasks[idx - 1].sequence {
            return Err(ApiError::bad_request(
                "tasks must be in strictly increasing sequence order",
            ));
        }
        if !matches!(
            task.action.as_str(),
            "start" | "stop" | "restart" | "kill" | "command" | "backup" | "notify"
        ) {
            return Err(ApiError::bad_request(format!(
                "unsupported schedule action: {}",
                task.action
            )));
        }
        // Flow Gate: with sequences strictly increasing, the request index
        // IS the chain position, so an exit gate may only reference an
        // earlier task; keep the canonical JSON for the post-insert write.
        let cond = crate::services::scheduler::validate_condition(task.condition.as_ref(), idx)
            .map_err(|e| ApiError::bad_request(format!("task {idx}: {e}")))?;
        triples.push((
            task.action.clone(),
            task.payload.clone().unwrap_or_default(),
            task.sequence,
        ));
        conds.push(cond);
    }
    crate::services::scheduler::parse_cron(&req.cron_expr)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let max_retries = req.max_retries.unwrap_or(0);
    let retry_backoff_s = req.retry_backoff_s.unwrap_or(30);
    let only_when_online = req.only_when_online.unwrap_or(false);
    if max_retries < 0 || retry_backoff_s < 0 {
        return Err(ApiError::bad_request(
            "max_retries and retry_backoff_s must be >= 0",
        ));
    }
    // Compute the first fire BEFORE inserting so a bad expression or timezone
    // can never leave a committed schedule row behind a 500 (a retried create
    // would otherwise duplicate the schedule). next_run_at is set
    // best-effort: on failure the scheduler's first tick computes it from the
    // None branch, so the row is never wedged either.
    let next_at = if req.enabled {
        let tz = parse_schedule_tz(req.timezone.as_deref().unwrap_or("UTC"));
        Some(
            crate::services::scheduler::next_run_tz(
                &req.cron_expr,
                chrono::Utc::now(),
                tz,
            )?
            .to_rfc3339(),
        )
    } else {
        None
    };
    let id = blocking(state.db.clone(), move |db| {
        models::create_schedule_with_tasks(
            &db,
            server_id,
            &req.name,
            &req.cron_expr,
            req.enabled,
            max_retries,
            retry_backoff_s,
            only_when_online,
            &triples,
        )
    })
    .await?;
    // `models::create_schedule_with_tasks` predates gates, so the condition
    // cells are written here. Sequences were enforced strictly increasing
    // above, so (sequence, id) returns the tasks in request order and the
    // zip with `conds` is 1:1.
    if conds.iter().any(|c| c.is_some()) {
        state
            .db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id FROM schedule_tasks WHERE schedule_id=?1 ORDER BY sequence, id",
                )?;
                let tids: Vec<i64> = stmt
                    .query_map([id], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (tid, cond) in tids.iter().zip(conds.iter()) {
                    if let Some(c) = cond {
                        conn.execute(
                            "UPDATE schedule_tasks SET condition=?1 WHERE id=?2",
                            rusqlite::params![c, tid],
                        )?;
                    }
                }
                Ok(())
            })
            .await?;
    }
    if let Some(tz) = req.timezone.clone() {
        state
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE schedules SET schedule_tz=?1 WHERE id=?2",
                    rusqlite::params![tz, id],
                )
                .map(|_| ())
                .map_err(anyhow::Error::from)
            })
            .await?;
    }
    if let Some(next) = next_at {
        let next2 = next.clone();
        if let Err(e) = blocking(state.db.clone(), move |db| {
            models::set_schedule_next(&db, id, Some(&next2))
        })
        .await
        {
            tracing::warn!("schedule {id}: could not persist first fire ({e}); tick will recompute");
        }
    }
    Ok(Json(
        blocking(state.db.clone(), move |db| {
            crate::services::scheduler::schedule_json(&db, id)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
pub struct UpdateScheduleReq {
    pub name: Option<String>,
    pub cron_expr: Option<String>,
    pub enabled: Option<bool>,
    pub max_retries: Option<i64>,
    pub retry_backoff_s: Option<i64>,
    pub only_when_online: Option<bool>,
    pub timezone: Option<String>,
    /// Flow Gate: optional full replacement of the task chain. When present,
    /// the ENTIRE chain is validated up front and then swapped atomically in
    /// one transaction — the old tasks are only deleted once every submitted
    /// action, sequence and condition has been checked, so an invalid batch
    /// (e.g. an unknown gate kind) leaves the existing chain untouched.
    pub tasks: Option<Vec<ScheduleTaskReq>>,
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateScheduleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let mut sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
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
    if req.max_retries.is_some_and(|v| v < 0) || req.retry_backoff_s.is_some_and(|v| v < 0) {
        return Err(ApiError::bad_request(
            "max_retries and retry_backoff_s must be >= 0",
        ));
    }
    // Full task-chain replacement: validate EVERYTHING (actions, strictly
    // increasing sequences, and each condition gate) before any write — an
    // unknown gate kind must fail here, with the existing chain still intact
    // and untouched. The canonical conditions are computed once, up front.
    let task_batch: Option<Vec<models::ScheduleTaskReplace>> = match &req.tasks {
        None => None,
        Some(tasks) => {
            let mut batch = Vec::with_capacity(tasks.len());
            for (idx, task) in tasks.iter().enumerate() {
                if idx > 0 && task.sequence <= tasks[idx - 1].sequence {
                    return Err(ApiError::bad_request(
                        "tasks must be in strictly increasing sequence order",
                    ));
                }
                if !matches!(
                    task.action.as_str(),
                    "start" | "stop" | "restart" | "kill" | "command" | "backup" | "notify"
                ) {
                    return Err(ApiError::bad_request(format!(
                        "unsupported schedule action: {}",
                        task.action
                    )));
                }
                let cond =
                    crate::services::scheduler::validate_condition(task.condition.as_ref(), idx)
                        .map_err(|e| ApiError::bad_request(format!("task {idx}: {e}")))?;
                batch.push(models::ScheduleTaskReplace {
                    action: task.action.clone(),
                    payload: task.payload.clone().unwrap_or_default(),
                    sequence: task.sequence,
                    condition: cond,
                });
            }
            Some(batch)
        }
    };
    let sch_id = sch.id;
    let sch_for_update = sch.clone();
    blocking(state.db.clone(), move |db| {
        models::update_schedule(&db, &sch_for_update)
    })
    .await?;
    if let Some(batch) = task_batch {
        // Atomic swap: DELETE of the old chain and the INSERTs of the new one
        // (conditions included) share a single BEGIN IMMEDIATE transaction, so
        // a mid-insert failure rolls the old chain back untouched.
        let b = batch.clone();
        blocking(state.db.clone(), move |db| {
            models::replace_schedule_tasks(&db, sch_id, &b)
        })
        .await?;
    }
    let (max_retries, retry_backoff_s, only_when_online) =
        (req.max_retries, req.retry_backoff_s, req.only_when_online);
    blocking(state.db.clone(), move |db| {
        crate::services::scheduler::update_schedule_settings(
            &db,
            sch_id,
            max_retries,
            retry_backoff_s,
            only_when_online,
        )
    })
    .await?;
    if let Some(tz) = req.timezone.clone() {
        state
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE schedules SET schedule_tz=?1 WHERE id=?2",
                    rusqlite::params![tz, sch_id],
                )
                .map(|_| ())
                .map_err(anyhow::Error::from)
            })
            .await?;
    }
    if sch.enabled {
        let tz = crate::services::scheduler::parse_schedule_tz(
            req.timezone.as_deref().unwrap_or("UTC"),
        );
        let next = crate::services::scheduler::next_run_tz(
            &sch.cron_expr,
            chrono::Utc::now(),
            tz,
        )?;
        let next = next.to_rfc3339();
        blocking(state.db.clone(), move |db| {
            models::set_schedule_next(&db, sch_id, Some(&next))
        })
        .await?;
    } else {
        blocking(state.db.clone(), move |db| {
            models::set_schedule_next(&db, sch_id, None)
        })
        .await?;
    }
    Ok(Json(
        blocking(state.db.clone(), move |db| {
            crate::services::scheduler::schedule_json(&db, id)
        })
        .await?,
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
    blocking(state.db.clone(), move |db| models::delete_schedule(&db, id)).await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AddTaskReq {
    pub action: String,
    pub payload: Option<String>,
    pub sequence: i64,
    /// Flow Gate condition, as in `ScheduleTaskReq`.
    pub condition: Option<serde_json::Value>,
}

pub async fn add_task(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(req): Json<AddTaskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
    if !matches!(
        req.action.as_str(),
        "start" | "stop" | "restart" | "kill" | "command" | "backup" | "notify"
    ) {
        return Err(ApiError::bad_request("unsupported schedule action"));
    }
    // The new task's chain position is not fixed until it lands (its sequence
    // decides), so an exit gate is conservatively required to reference a
    // task that already exists — never one that may end up after it.
    let cond = crate::services::scheduler::validate_condition(
        req.condition.as_ref(),
        sch.tasks.len(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let tid = blocking(state.db.clone(), move |db| {
        models::add_schedule_task(
            &db,
            id,
            &req.action,
            req.payload.as_deref().unwrap_or(""),
            req.sequence,
        )
    })
    .await?;
    if let Some(c) = cond {
        state
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE schedule_tasks SET condition=?1 WHERE id=?2",
                    rusqlite::params![c, tid],
                )
                .map(|_| ())
                .map_err(anyhow::Error::from)
            })
            .await?;
    }
    Ok(Json(
        blocking(state.db.clone(), move |db| {
            crate::services::scheduler::schedule_json(&db, id)
        })
        .await?,
    ))
}
pub async fn remove_task(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, task_id)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
    if !blocking(state.db.clone(), move |db| {
        models::delete_schedule_task(&db, id, task_id)
    })
    .await?
    {
        return Err(ApiError::not_found("task not found"));
    }
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn runs(
    State(state): State<AppState>,
    user: AuthUser,
    Path((server_id, schedule_id)): Path<(i64, i64)>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    access_ok(&state, u, server_id).await?;
    super::require_capability(&state, &user, server_id, Capability::ScheduleRead).await?;
    let sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, schedule_id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    if sch.server_id != server_id {
        return Err(ApiError::not_found("schedule not found"));
    }
    let page = q.page.unwrap_or(1).max(1);
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * limit;
    let out = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare("SELECT id,schedule_id,triggered_at,status,log,attempt,finished_at FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT ?2 OFFSET ?3")?;
            let rows = stmt.query_map(
                rusqlite::params![schedule_id, i64::from(limit), i64::from(offset)],
                |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, i64>(0)?,
                        "schedule_id": r.get::<_, i64>(1)?,
                        "triggered_at": r.get::<_, String>(2)?,
                        "status": r.get::<_, String>(3)?,
                        "log": r.get::<_, String>(4)?,
                        "attempt": r.get::<_, Option<i64>>(5)?.unwrap_or(1),
                        "finished_at": r.get::<_, Option<String>>(6)?,
                    }))
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?;
    Ok(data(serde_json::json!(out)))
}

pub async fn toggle(
    State(state): State<AppState>,
    user: AuthUser,
    Path((id, enabled)): Path<(i64, bool)>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let mut sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
    sch.enabled = enabled;
    let sch_id = sch.id;
    let sch_for_update = sch.clone();
    blocking(state.db.clone(), move |db| {
        models::update_schedule(&db, &sch_for_update)
    })
    .await?;
    if enabled {
        let tz = blocking(state.db.clone(), move |db| {
            Ok::<_, anyhow::Error>(crate::services::scheduler::schedule_tz(&db, sch_id))
        })
        .await?;
        let next =
            crate::services::scheduler::next_run_tz(&sch.cron_expr, chrono::Utc::now(), tz)?;
        let next = next.to_rfc3339();
        blocking(state.db.clone(), move |db| {
            models::set_schedule_next(&db, sch_id, Some(&next))
        })
        .await?;
    } else {
        blocking(state.db.clone(), move |db| {
            models::set_schedule_next(&db, sch_id, None)
        })
        .await?;
    }
    Ok(ok(serde_json::json!({ "enabled": enabled })))
}

pub async fn run_now(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let u = &user.0;
    schedules_enabled(&state)?;
    let sch = blocking(state.db.clone(), move |db| models::get_schedule(&db, id))
        .await
        .map_err(|_| ApiError::not_found("schedule not found"))?;
    access_ok(&state, u, sch.server_id).await?;
    super::require_capability(&state, &user, sch.server_id, Capability::ScheduleWrite).await?;
    let server_id = sch.server_id;
    let server = blocking(state.db.clone(), move |db| models::get_server(&db, server_id))
        .await
        .map_err(|_| ApiError::not_found("server not found"))?;
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
    // The 409 is decided by the atomic claim inside execute, not by a separate
    // pre-check: a tick racing run_now between the two would otherwise both
    // pass and only one attempt may be active per schedule.
    match scheduler.execute(&mut sch).await? {
        crate::services::scheduler::ExecuteOutcome::Ran => {
            Ok(ok(serde_json::json!({ "started": true })))
        }
        crate::services::scheduler::ExecuteOutcome::AlreadyActive => Err(ApiError::new(
            StatusCode::CONFLICT,
            "schedule is already running",
        )),
        crate::services::scheduler::ExecuteOutcome::Skipped => {
            Ok(ok(serde_json::json!({ "started": false, "skipped": true })))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, patch};
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

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

    /// A root admin user with a live session cookie plus a server it owns.
    fn seed(state: &AppState, uuid: &str) -> (i64, String) {
        let user_id = models::create_user(
            &state.db,
            &format!("u-{uuid}"),
            &format!("{uuid}@x.io"),
            "h",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let blueprint_id = models::create_blueprint(
            &state.db,
            &format!("bp-{uuid}"),
            "bp",
            "",
            "a",
            "game",
            "generic",
            "echo",
            None,
            None,
            &[],
            "stop",
        )
        .unwrap();
        let server_id = models::create_server(
            &state.db, uuid, "srv", user_id, blueprint_id, "generic", "echo", 512, 1024, 100, 0,
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db, &state.cfg, user_id, "test-agent", "127.0.0.1", false,
        )
        .unwrap();
        (server_id, format!("vp_session={raw}"))
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/servers/:id/schedules", get(list).post(create))
            .route("/api/schedules/:id", patch(update))
            .with_state(state)
    }

    async fn request(
        state: AppState,
        method: &str,
        uri: &str,
        cookie: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri).header("cookie", cookie);
        let payload = body.map(|b| b.to_string());
        if payload.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let req = builder.body(Body::from(payload.unwrap_or_default())).unwrap();
        let response = router(state).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null))
        };
        (status, value)
    }

    /// Out-of-order sequences are rejected before anything is inserted: the
    /// chain orders tasks by (sequence, id), so a request index is not a
    /// chain position unless the sequences are strictly increasing. Here the
    /// exit gate on the sequence-2 task (request index 1) would attach to
    /// the sequence-1 task that actually lands at chain position 1.
    #[tokio::test]
    async fn create_rejects_out_of_order_tasks_with_exit_gate() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-ooo");
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/schedules"),
            &cookie,
            Some(serde_json::json!({
                "name": "bad",
                "cron_expr": "*/5 * * * *",
                "enabled": false,
                "tasks": [
                    { "action": "notify", "payload": "a", "sequence": 0 },
                    { "action": "notify", "payload": "b", "sequence": 2,
                      "condition": { "kind": "exit", "task_index": 0, "code": 0 } },
                    { "action": "notify", "payload": "c", "sequence": 1 },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
        // Rejection happens before insert: no schedule row may exist.
        let conn = state.db.get().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// A strictly ordered submission still works and the exit gate lands on
    /// the task whose request index equals its chain position.
    #[tokio::test]
    async fn create_accepts_strictly_ordered_tasks_with_exit_gate() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-ok");
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/schedules"),
            &cookie,
            Some(serde_json::json!({
                "name": "ok",
                "cron_expr": "*/5 * * * *",
                "enabled": false,
                "tasks": [
                    { "action": "notify", "payload": "a", "sequence": 0 },
                    { "action": "notify", "payload": "b", "sequence": 1,
                      "condition": { "kind": "exit", "task_index": 0, "code": 0 } },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let tasks = body["tasks"].as_array().expect("schedule serializes tasks");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["condition"], serde_json::Value::Null);
        assert_eq!(tasks[1]["condition"]["kind"], "exit");
        assert_eq!(tasks[1]["condition"]["task_index"], 0);
    }

    /// Flow Gate: an unknown gate kind in a PATCH `tasks` batch is rejected
    /// during validation — BEFORE the transaction — so the old task chain
    /// (actions, sequences and stored conditions) survives untouched.
    #[tokio::test]
    async fn update_rejects_unknown_gate_leaving_old_tasks_intact() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-unk");
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/schedules"),
            &cookie,
            Some(serde_json::json!({
                "name": "flow",
                "cron_expr": "*/5 * * * *",
                "enabled": false,
                "tasks": [
                    { "action": "notify", "payload": "a", "sequence": 1 },
                    { "action": "restart", "payload": "", "sequence": 2,
                      "condition": { "kind": "exit", "task_index": 0, "code": 0 } },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_i64().expect("created schedule id");
        let (status, _) = request(
            state.clone(),
            "PATCH",
            &format!("/api/schedules/{id}"),
            &cookie,
            Some(serde_json::json!({
                "tasks": [
                    { "action": "kill", "payload": "", "sequence": 1,
                      "condition": { "kind": "bogus" } },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The rejected batch must leave the old chain exactly as it was.
        let conn = state.db.get().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT action,payload,sequence,condition FROM schedule_tasks WHERE schedule_id=?1 ORDER BY sequence, id",
            )
            .unwrap();
        let rows: Vec<(String, String, i64, Option<String>)> = stmt
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        drop(stmt);
        drop(conn);
        assert_eq!(
            rows,
            vec![
                ("notify".to_string(), "a".to_string(), 1, None),
                (
                    "restart".to_string(),
                    "".to_string(),
                    2,
                    Some(r#"{"kind":"exit","task_index":0,"code":0}"#.to_string())
                ),
            ]
        );
    }

    /// Flow Gate: a valid PATCH `tasks` batch swaps the whole chain in one
    /// request — old tasks gone, new actions/sequences in request order, and
    /// each condition normalized to its canonical stored form.
    #[tokio::test]
    async fn update_replaces_tasks_atomically_preserving_order_and_conditions() {
        let (_tmp, state) = test_state();
        let (server_id, cookie) = seed(&state, "uuid-rep");
        let (status, body) = request(
            state.clone(),
            "POST",
            &format!("/api/servers/{server_id}/schedules"),
            &cookie,
            Some(serde_json::json!({
                "name": "flow",
                "cron_expr": "*/5 * * * *",
                "enabled": false,
                "tasks": [
                    { "action": "start", "payload": "", "sequence": 1 },
                    { "action": "notify", "payload": "old", "sequence": 2 },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = body["id"].as_i64().expect("created schedule id");
        let (status, body) = request(
            state.clone(),
            "PATCH",
            &format!("/api/schedules/{id}"),
            &cookie,
            Some(serde_json::json!({
                "name": "renamed",
                "tasks": [
                    { "action": "notify", "payload": "x", "sequence": 0,
                      "condition": { "kind": "none" } },
                    { "action": "command", "payload": "echo hi", "sequence": 1,
                      "condition": { "kind": "exit", "task_index": 0, "code": 3 } },
                ],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "renamed", "fields update alongside tasks");
        let tasks = body["tasks"].as_array().expect("schedule serializes tasks");
        assert_eq!(tasks.len(), 2, "old chain fully replaced, not appended");
        assert_eq!(tasks[0]["action"], "notify");
        assert_eq!(tasks[0]["payload"], "x");
        assert_eq!(tasks[0]["sequence"], 0);
        // {"kind":"none"} normalizes to no gate.
        assert_eq!(tasks[0]["condition"], serde_json::Value::Null);
        assert_eq!(tasks[1]["action"], "command");
        assert_eq!(tasks[1]["condition"]["kind"], "exit");
        assert_eq!(tasks[1]["condition"]["task_index"], 0);
        assert_eq!(tasks[1]["condition"]["code"], 3);
    }
}