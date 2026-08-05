//! Scheduler engine: cron parsing + task execution loop.
use crate::db::Db;
use crate::models::{self, Schedule, ScheduleTask};
use crate::services::{proc, ConsoleHub};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
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
        {
            let conn = self.db.lock();
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status) VALUES(?1,?2,'running')",
                rusqlite::params![sch.id, now.to_rfc3339()],
            )?;
        }

        self.notifier.notify(
            "info",
            &format!("Schedule '{}' triggered", sch.name),
            &format!("Running {} task(s)", sch.tasks.len()),
            Some(sch.server_id),
        );
        let mut log = String::new();
        for task in &sch.tasks {
            match self.run_task(sch.server_id, task).await {
                Ok(()) => log.push_str(&format!("[ok] {} {}\n", task.action, task.payload)),
                Err(e) => {
                    log.push_str(&format!("[err] {} {}: {e}\n", task.action, task.payload));
                    let _ = {
                        let conn = self.db.lock();
                        conn.execute(
                            "UPDATE schedule_runs SET status='failed', log=?1 WHERE schedule_id=?2 AND triggered_at=?3",
                            rusqlite::params![log, sch.id, now.to_rfc3339()],
                        )
                    };
                    self.notifier.notify(
                        "error",
                        &format!("Schedule '{}' failed", sch.name),
                        &e.to_string(),
                        Some(sch.server_id),
                    );
                    return Ok(());
                }
            }
        }
        let _ = {
            let conn = self.db.lock();
            conn.execute(
                "UPDATE schedule_runs SET status='success', log=?1 WHERE schedule_id=?2 AND triggered_at=?3",
                rusqlite::params![log, sch.id, now.to_rfc3339()],
            )
        };
        Ok(())
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
