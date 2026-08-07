//! Scheduler engine: cron parsing + task execution loop.
use crate::db::Db;
use crate::models::{self, Schedule, ScheduleTask};
use crate::services::{proc, ConsoleHub};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};

pub struct Scheduler {
    pub db: Db,
    pub procs: Arc<proc::ProcManager>,
    pub hub: Arc<ConsoleHub>,
    pub notifier: Arc<proc::Notifier>,
    pub node_client: Arc<crate::services::node::NodeClient>,
    pub running: Arc<AtomicBool>,
}

pub fn parse_cron(expr: &str) -> Result<cron::Schedule> {
    // cron 0.12 requires the 6-field form (seconds first); accept the common
    // 5-field form by defaulting seconds to 0.
    let normalized = if expr.split_whitespace().count() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    };
    cron::Schedule::from_str(&normalized).context("invalid cron expression")
}

pub fn next_run(expr: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let sched = parse_cron(expr)?;
    sched
        .after(&after)
        .next()
        .context("no future run time for expression")
}
/// Exponential retry delay: `base * 2^(attempt-1)`, saturated at i64::MAX.
pub fn backoff_secs(base: i64, attempt: i64) -> i64 {
    if base <= 0 {
        return 0;
    }
    let exp = attempt.saturating_sub(1);
    let factor = if exp >= 63 { i64::MAX } else { 1i64 << exp };
    base.saturating_mul(factor)
}

/// Whether a run may proceed given the schedule's settings and workspace liveness.
pub fn should_run(enabled: bool, only_when_online: bool, is_online: bool) -> bool {
    enabled && (!only_when_online || is_online)
}

struct RetryPolicy {
    max_retries: i64,
    backoff_s: i64,
    only_when_online: bool,
}

// models::Schedule predates the v7 retry/online columns, so read them here.
fn policy(db: &Db, schedule_id: i64) -> Result<RetryPolicy> {
    let conn = db.lock();
    let (max_retries, backoff_s, owo) = conn.query_row(
        "SELECT max_retries, retry_backoff_s, only_when_online FROM schedules WHERE id=?1",
        [schedule_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(RetryPolicy {
        max_retries,
        backoff_s,
        only_when_online: owo != 0,
    })
}

pub fn create_schedule(
    db: &Db,
    server_id: i64,
    name: &str,
    cron_expr: &str,
    enabled: bool,
    max_retries: i64,
    retry_backoff_s: i64,
    only_when_online: bool,
) -> Result<i64> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at,max_retries,retry_backoff_s,only_when_online) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            server_id,
            name,
            cron_expr,
            enabled as i64,
            Utc::now().to_rfc3339(),
            max_retries,
            retry_backoff_s,
            only_when_online as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_schedule_settings(
    db: &Db,
    id: i64,
    max_retries: Option<i64>,
    retry_backoff_s: Option<i64>,
    only_when_online: Option<bool>,
) -> Result<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE schedules SET max_retries=COALESCE(?1,max_retries), retry_backoff_s=COALESCE(?2,retry_backoff_s), only_when_online=COALESCE(?3,only_when_online) WHERE id=?4",
        rusqlite::params![
            max_retries,
            retry_backoff_s,
            only_when_online.map(|b| b as i64),
            id
        ],
    )?;
    Ok(())
}

fn add_policy(
    v: &mut serde_json::Value,
    conn: &rusqlite::Connection,
    schedule_id: i64,
) -> Result<()> {
    let (max_retries, backoff_s, owo) = conn.query_row(
        "SELECT max_retries, retry_backoff_s, only_when_online FROM schedules WHERE id=?1",
        [schedule_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("schedule is not a JSON object"))?;
    obj.insert("max_retries".to_string(), serde_json::json!(max_retries));
    obj.insert("retry_backoff_s".to_string(), serde_json::json!(backoff_s));
    obj.insert("only_when_online".to_string(), serde_json::json!(owo != 0));
    Ok(())
}

/// Serialize one schedule including the v7 retry/online columns.
pub fn schedule_json(db: &Db, id: i64) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(models::get_schedule(db, id)?)?;
    let conn = db.lock();
    add_policy(&mut v, &conn, id)?;
    Ok(v)
}

/// Serialize all schedules of a server including the v7 retry/online columns.
pub fn schedule_list_json(db: &Db, server_id: i64) -> Result<serde_json::Value> {
    let schedules = models::list_schedules(db, server_id)?;
    let mut pols: HashMap<i64, (i64, i64, i64)> = HashMap::new();
    {
        let conn = db.lock();
        let mut stmt = conn.prepare(
            "SELECT id,max_retries,retry_backoff_s,only_when_online FROM schedules WHERE server_id=?1",
        )?;
        let rows = stmt.query_map([server_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for r in rows {
            let (id, mr, bs, owo) = r?;
            pols.insert(id, (mr, bs, owo));
        }
    }
    let mut out = Vec::new();
    for s in schedules {
        let mut v = serde_json::to_value(&s)?;
        if let Some(&(mr, bs, owo)) = pols.get(&s.id) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("max_retries".to_string(), serde_json::json!(mr));
                obj.insert("retry_backoff_s".to_string(), serde_json::json!(bs));
                obj.insert("only_when_online".to_string(), serde_json::json!(owo != 0));
            }
        }
        out.push(v);
    }
    Ok(serde_json::json!(out))
}

pub async fn run_loop(sched: Scheduler) {
    let mut tick = interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if !sched.running.load(Ordering::Relaxed) {
            break;
        }
        if let Err(e) = sched.tick_once().await {
            tracing::warn!("scheduler tick: {e}");
        }
    }
}

impl Scheduler {
    async fn tick_once(&self) -> Result<()> {
        let now = Utc::now();
        // all enabled schedules whose next_run is due
        let ids: Vec<i64> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare("SELECT id FROM schedules WHERE enabled=1")?;
            let mapped = stmt.query_map([], |r| r.get::<_, i64>(0))?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        for id in ids {
            let mut sch = models::get_schedule(&self.db, id)?;
            let due = match &sch.next_run_at {
                Some(n) => DateTime::parse_from_rfc3339(n)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or(DateTime::<Utc>::MIN_UTC),
                None => {
                    // compute first next
                    match next_run(&sch.cron_expr, Utc::now()) {
                        Ok(n) => {
                            let _ =
                                models::set_schedule_next(&self.db, sch.id, Some(&n.to_rfc3339()));
                            n
                        }
                        Err(_) => continue,
                    }
                }
            };
            if now >= due {
                let _ = self.execute(&mut sch).await;
            }
        }
        // retry queue: pending attempts whose backoff delay has elapsed
        let retries: Vec<(i64, i64, i64)> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare(
                "SELECT id, schedule_id, attempt FROM schedule_runs WHERE status='pending' AND triggered_at <= ?1",
            )?;
            let mapped = stmt.query_map([now.to_rfc3339()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(1),
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        for (run_id, schedule_id, attempt) in retries {
            let Ok(sch) = models::get_schedule(&self.db, schedule_id) else {
                continue; // schedule vanished; leave the orphaned run alone
            };
            let Ok(pol) = policy(&self.db, schedule_id) else {
                continue;
            };
            if let Err(e) = self.run_attempt(&sch, run_id, attempt, &pol).await {
                tracing::warn!("scheduler retry {run_id}: {e}");
            }
        }
        Ok(())
    }

    pub async fn execute(&self, sch: &mut Schedule) -> Result<()> {
        let next = sch.cron_expr.clone();
        let now = Utc::now();
        let _ = models::set_schedule_last(&self.db, sch.id, Some(&now.to_rfc3339()));
        match next_run(&next, now) {
            Ok(n) => {
                let _ = models::set_schedule_next(&self.db, sch.id, Some(&n.to_rfc3339()));
            }
            Err(_) => {
                let _ = models::set_schedule_next(&self.db, sch.id, None);
            }
        }
        self.notifier.notify(
            "info",
            &format!("Schedule '{}' triggered", sch.name),
            &format!("Running {} task(s)", sch.tasks.len()),
            Some(sch.server_id),
        );
        let pol = policy(&self.db, sch.id)?;
        let run_id = {
            let conn = self.db.lock();
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt) VALUES(?1,?2,'running',1)",
                rusqlite::params![sch.id, now.to_rfc3339()],
            )?;
            conn.last_insert_rowid()
        };
        self.run_attempt(sch, run_id, 1, &pol).await
    }

    /// Execute the tasks for one run row; marks the row terminal on success,
    /// failure or skip, or requeues the run when retries remain.
    async fn run_attempt(
        &self,
        sch: &Schedule,
        run_id: i64,
        attempt: i64,
        pol: &RetryPolicy,
    ) -> Result<()> {
        if !should_run(
            true,
            pol.only_when_online,
            self.is_online(sch.server_id).await,
        ) {
            self.finish_run(run_id, "skipped", "workspace offline", true)?;
            self.notifier.notify(
                "info",
                &format!("Schedule '{}' skipped", sch.name),
                "workspace offline",
                Some(sch.server_id),
            );
            return Ok(());
        }
        let mut log = String::new();
        for task in &sch.tasks {
            match self.run_task(sch.server_id, task).await {
                Ok(()) => log.push_str(&format!("[ok] {} {}\n", task.action, task.payload)),
                Err(e) => {
                    log.push_str(&format!("[err] {} {}: {e}\n", task.action, task.payload));
                    if attempt <= pol.max_retries {
                        // not terminal: requeue as its own pending row after backoff
                        self.finish_run(run_id, "retry", &log, false)?;
                        let due = Utc::now()
                            + Duration::from_secs(backoff_secs(pol.backoff_s, attempt) as u64);
                        {
                            let conn = self.db.lock();
                            conn.execute(
                                "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt) VALUES(?1,?2,'pending',?3)",
                                rusqlite::params![sch.id, due.to_rfc3339(), attempt + 1],
                            )?;
                        }
                        self.notifier.notify(
                            "warning",
                            &format!(
                                "Schedule '{}' retrying ({}/{})",
                                sch.name,
                                attempt + 1,
                                pol.max_retries + 1
                            ),
                            &e.to_string(),
                            Some(sch.server_id),
                        );
                    } else {
                        self.finish_run(run_id, "failed", &log, true)?;
                        self.notifier.notify(
                            "error",
                            &format!("Schedule '{}' failed", sch.name),
                            &e.to_string(),
                            Some(sch.server_id),
                        );
                    }
                    return Ok(());
                }
            }
        }
        self.finish_run(run_id, "success", &log, true)?;
        Ok(())
    }

    fn finish_run(&self, run_id: i64, status: &str, log: &str, terminal: bool) -> Result<()> {
        let conn = self.db.lock();
        if terminal {
            conn.execute(
                "UPDATE schedule_runs SET status=?1, log=?2, finished_at=?3 WHERE id=?4",
                rusqlite::params![status, log, Utc::now().to_rfc3339(), run_id],
            )?;
        } else {
            conn.execute(
                "UPDATE schedule_runs SET status=?1, log=?2 WHERE id=?3",
                rusqlite::params![status, log, run_id],
            )?;
        }
        Ok(())
    }

    /// True when the workspace has a live process (local) or a live remote pid.
    async fn is_online(&self, server_id: i64) -> bool {
        let Ok(srv) = models::get_server(&self.db, server_id) else {
            return false;
        };
        if srv.node != "local" {
            let Ok(node) = crate::nodes::get_by_name(&self.db, &srv.node) else {
                return false;
            };
            return matches!(
                self.node_client.stats(&node, &srv.uuid).await,
                Ok(s) if s.pid.is_some()
            );
        }
        self.procs.state(server_id).is_some()
    }

    async fn run_task(&self, server_id: i64, task: &ScheduleTask) -> Result<()> {
        let srv = models::get_server(&self.db, server_id)?;
        if srv.node != "local" {
            let node = crate::nodes::get_by_name(&self.db, &srv.node)?;
            match task.action.as_str() {
                "start" => {
                    self.node_client
                        .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Start)
                        .await?;
                }
                "stop" => {
                    self.node_client
                        .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Stop)
                        .await?;
                }
                "restart" => {
                    self.node_client
                        .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Restart)
                        .await?;
                }
                "kill" => {
                    self.node_client
                        .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Kill)
                        .await?;
                }
                "command" => {
                    self.node_client
                        .command(&node, &srv.uuid, task.payload.trim_matches('\''))
                        .await?;
                }
                "notify" => {
                    self.notifier.notify(
                        "info",
                        "Scheduled notification",
                        &task.payload,
                        Some(server_id),
                    );
                }
                "backup" => {
                    let was_running = srv.status == "running";
                    if was_running {
                        self.node_client
                            .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Stop)
                            .await?;
                        for _ in 0..50 {
                            if self
                                .node_client
                                .stats(&node, &srv.uuid)
                                .await
                                .map(|s| s.pid.is_none())
                                .unwrap_or(true)
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    let snapshot_result = self.node_client.snapshot(&node, &srv.uuid).await;
                    if was_running {
                        let _ = self
                            .node_client
                            .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Start)
                            .await;
                    }
                    let snapshot = snapshot_result?;
                    let uuid = uuid::Uuid::new_v4().to_string();
                    std::fs::create_dir_all(&crate::SETTINGS.paths.backups_dir)?;
                    let path = crate::SETTINGS
                        .paths
                        .backups_dir
                        .join(format!("{uuid}.tar.gz"));
                    let bytes = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        snapshot.archive_b64,
                    )?;
                    std::fs::write(&path, &bytes)?;
                    crate::models::create_backup(
                        &self.db,
                        &uuid,
                        srv.id,
                        if task.payload.is_empty() {
                            "scheduled"
                        } else {
                            &task.payload
                        },
                        &path.to_string_lossy(),
                        bytes.len() as i64,
                        &snapshot.checksum,
                        "tar.gz",
                    )?;
                }
                other => bail!("unknown schedule action: {other}"),
            }
            return Ok(());
        }
        match task.action.as_str() {
            "start" => {
                let cmd = crate::services::blueprint::resolve_startup(&self.db, &srv)?;
                let env = crate::services::blueprint::env_for_server(&self.db, &srv);
                self.procs.start(&srv, &cmd, &env, self.notifier.clone())?;
            }
            "stop" => self.procs.stop(server_id)?,
            "restart" => {
                self.procs.stop(server_id)?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let srv = models::get_server(&self.db, server_id)?;
                let cmd = crate::services::blueprint::resolve_startup(&self.db, &srv)?;
                let env = crate::services::blueprint::env_for_server(&self.db, &srv);
                self.procs.start(&srv, &cmd, &env, self.notifier.clone())?;
            }
            "kill" => {
                self.procs.kill(server_id)?;
            }
            "command" => {
                let payload = task.payload.trim_start_matches("'").trim_end_matches("'");
                self.procs.send_input(server_id, &format!("{payload}\n"))?;
            }
            "notify" => {
                self.notifier.notify(
                    "info",
                    "Scheduled notification",
                    &task.payload,
                    Some(server_id),
                );
            }
            "backup" => {
                let name = if task.payload.is_empty() {
                    format!("scheduled-{}", Utc::now().format("%Y%m%d-%H%M%S"))
                } else {
                    task.payload.clone()
                };
                crate::services::backups::create(&self.db, &crate::SETTINGS, server_id, &name)
                    .await?;
            }
            other => bail!("unknown schedule action: {other}"),
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::{backoff_secs, should_run};

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_secs(30, 1), 30);
        assert_eq!(backoff_secs(30, 2), 60);
        assert_eq!(backoff_secs(30, 3), 120);
        assert_eq!(backoff_secs(30, 4), 240);
        assert_eq!(backoff_secs(10, 1), 10);
        assert_eq!(backoff_secs(10, 2), 20);
    }

    #[test]
    fn backoff_saturates() {
        assert_eq!(backoff_secs(1, 63), 1 << 62);
        assert_eq!(backoff_secs(1, 64), i64::MAX);
        assert_eq!(backoff_secs(i64::MAX, 2), i64::MAX);
        assert_eq!(backoff_secs(30, 100), i64::MAX);
        // non-positive base means "no backoff"
        assert_eq!(backoff_secs(0, 5), 0);
        assert_eq!(backoff_secs(-5, 3), 0);
    }

    #[test]
    fn should_run_predicate() {
        // enabled and no online gate: runs regardless of liveness
        assert!(should_run(true, false, false));
        assert!(should_run(true, false, true));
        // only_when_online requires a live process
        assert!(should_run(true, true, true));
        assert!(!should_run(true, true, false));
        // disabled never runs
        assert!(!should_run(false, false, true));
        assert!(!should_run(false, true, false));
    }
}
