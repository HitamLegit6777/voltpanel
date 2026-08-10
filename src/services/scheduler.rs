//! Scheduler engine: cron parsing + task execution loop.
use crate::db::{blocking, Db};
use crate::models::{self, Schedule, ScheduleTask};
use crate::services::{proc, webhooks, ConsoleHub};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use rand::Rng;
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;
use std::future::Future;
use parking_lot::Mutex;
use tokio::sync::Semaphore;
use tokio::time::{interval, Duration};

// Run a closure on Tokio's blocking pool, passing the pool itself as the
// argument so its body can call pool-based models without capturing a `Db`
// (which would force a per-closure move). Never unwrapped: a join failure
// surfaces as a `db worker failed` error. See `crate::db::blocking`.

/// Async variant of [`schedule_tz`]: same zero-offset fallback, but the
/// lookup runs on the blocking pool instead of a tokio worker.
async fn schedule_tz_of(db: Db, schedule_id: i64) -> FixedOffset {
    blocking(db, move |db| Ok::<FixedOffset, anyhow::Error>(schedule_tz(&db, schedule_id)))
        .await
        .unwrap_or_else(|_| FixedOffset::east_opt(0).expect("zero offset is valid"))
}

pub struct Scheduler {
    pub db: Db,
    pub procs: Arc<proc::ProcManager>,
    pub hub: Arc<ConsoleHub>,
    pub notifier: Arc<proc::Notifier>,
    pub node_client: Arc<crate::services::node::NodeClient>,
    pub running: Arc<AtomicBool>,
}

/// Concurrency cap for simultaneously executing tasks. Claimed attempts are
/// spawned onto their own tasks; this semaphore bounds how many task
/// executions may run at once so one slow task (long backup, wedged node)
/// never stalls the scheduler loop, the run_now handler, or every other
/// schedule. The permit is held only for a task's execution: a flow-gate
/// wait (a signal gate may block up to timeout_s, 3600s) holds no permit,
/// so concurrent gate waits can never exhaust the budget (cross-tenant DoS).
const MAX_CONCURRENT_ATTEMPTS: usize = 8;
/// Seconds a `running` row without a live in-flight tag may sit before the
/// periodic recovery pass reclaims it. Rows tagged by this process and
/// actively executing are never age-reclaimed.
const STALE_RUN_AGE_S: i64 = 5 * 60;
/// Seconds an offline-skipped fire may be retried (bounded catch-up) before
/// it is abandoned: `next_run_at` is left at the missed fire while the
/// workspace is offline, so the next tick re-tries it; once the fire ages
/// past this window it is dropped with one notification.
const OFFLINE_CATCHUP_WINDOW_S: i64 = 5 * 60;
/// Terminal schedule_runs (success/failed/retry) older than this many days
/// are pruned on a cadence so the runs table cannot grow without bound.
const RUN_RETENTION_DAYS: i64 = 30;

/// Process-wide scheduler state, shared by every `Scheduler` (there is at
/// most one scheduler loop per process) and by the free stale-run recovery
/// function. Holds the per-incarnation in-flight tag, the set of run ids
/// this process is actively executing, and the bounded-attempt semaphore.
struct SchedState {
    /// Unique per process incarnation: rows tagged with it are provably ours.
    tag: String,
    /// Run ids claimed by this process and currently executing. Recovery
    /// never reclaims a `running` row that is tagged and live here.
    live: Mutex<HashSet<i64>>,
    /// Bounds concurrently executing attempts (see MAX_CONCURRENT_ATTEMPTS).
    sem: Arc<Semaphore>,
}

static SCHED_STATE: LazyLock<SchedState> = LazyLock::new(|| SchedState {
    tag: format!("panel-{}", uuid::Uuid::new_v4()),
    live: Mutex::new(HashSet::new()),
    sem: Arc::new(Semaphore::new(MAX_CONCURRENT_ATTEMPTS)),
});

/// Unix seconds (UTC) of the most recent scheduler tick; 0 until the first
/// tick has run. Process-global because the Observatory self-metrics read it
/// from `services::metrics` without coupling to a `Scheduler` instance.
pub static LAST_TICK: AtomicU64 = AtomicU64::new(0);


/// Mark a claimed run as actively executing in this process.
fn mark_live(run_id: i64) {
    SCHED_STATE.live.lock().insert(run_id);
}

/// Mark a run as no longer executing. Safe to call more than once.
fn unmark_live(run_id: i64) {
    SCHED_STATE.live.lock().remove(&run_id);
}

/// Parse a stored schedule timezone: "UTC" (the default) or a fixed offset
/// such as "+05:30" / "-08:00". Anything unrecognized falls back to UTC so a
/// bad value can never wedge a schedule's timing.
pub fn parse_schedule_tz(s: &str) -> FixedOffset {
    let s = s.trim();
    let zero = FixedOffset::east_opt(0).expect("zero offset is valid");
    if s.is_empty() || s.eq_ignore_ascii_case("utc") || s.eq_ignore_ascii_case("z") {
        return zero;
    }
    s.parse::<FixedOffset>().unwrap_or(zero)
}

/// Read a schedule's timezone from its `schedule_tz` column.
pub fn schedule_tz(db: &Db, schedule_id: i64) -> FixedOffset {
    match db.get() {
        Ok(conn) => conn
            .query_row(
                "SELECT schedule_tz FROM schedules WHERE id=?1",
                [schedule_id],
                |r| r.get::<_, String>(0),
            )
            .map(|s| parse_schedule_tz(&s))
            .unwrap_or_else(|_| FixedOffset::east_opt(0).expect("zero offset is valid")),
        Err(_) => FixedOffset::east_opt(0).expect("zero offset is valid"),
    }
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
    let tz = FixedOffset::east_opt(0).expect("zero offset is valid");
    next_run_tz(expr, after, tz)
}

/// Next fire of `expr` after `after`, evaluated in the given fixed-offset
/// timezone: the cron fields match the schedule's local wall clock (e.g.
/// "30 9 * * *" with tz "+05:30" fires at 09:30 IST). The result is returned
/// in UTC.
pub fn next_run_tz(expr: &str, after: DateTime<Utc>, tz: FixedOffset) -> Result<DateTime<Utc>> {
    let sched = parse_cron(expr)?;
    let local = after.with_timezone(&tz);
    sched
        .after(&local)
        .next()
        .map(|d| d.with_timezone(&Utc))
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
/// A failed attempt requeues while retries remain. Attempts count from 1, so
/// `max_retries` is exactly the number of retries after the initial attempt.
fn retries_remain(attempt: i64, max_retries: i64) -> bool {
    attempt <= max_retries
}
/// Backoff delay in seconds, clamped so the resulting due time fits chrono's
/// `TimeDelta` (i64 nanoseconds, ~292 years). Raw `backoff_secs` saturates at
/// `i64::MAX`, which would overflow (and panic) when added to `Utc::now()`.
fn backoff_delay_s(base: i64, attempt: i64) -> u64 {
    const MAX_S: i64 = i64::MAX / 1_000_000_000;
    backoff_secs(base, attempt).clamp(0, MAX_S) as u64
}

/// Whether a run may proceed given the schedule's settings and workspace liveness.
pub fn should_run(enabled: bool, only_when_online: bool, is_online: bool) -> bool {
    enabled && (!only_when_online || is_online)
}
/// Whether a task actually executed or was deliberately skipped (e.g. a start
/// on a suspended server). Skipped tasks are not failures: the run completes,
/// but its log records the skip instead of a false success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskOutcome {
    Ran,
    Skipped,
}
/// What a scheduled fire did, so callers like `run_now` can distinguish a
/// fresh execution from a skipped or already-active attempt.
pub enum ExecuteOutcome {
    Ran,
    AlreadyActive,
    Skipped,
}

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_retries: i64,
    backoff_s: i64,
    only_when_online: bool,
}

// models::Schedule predates the v7 retry/online columns, so read them here.
fn policy(db: &Db, schedule_id: i64) -> Result<RetryPolicy> {
    let conn = db.get()?;
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

// ---------------------------------------------------------------------------
// Flow Gates (iteration 4): a schedule task may carry a condition that must
// hold before the task runs. `exit` gates on an earlier task's recorded exit
// code (the run's persisted `task_exits` map); `signal` waits for a matching
// webhook event to be emitted for the schedule's server — observed through
// the webhook bus's in-memory recent-event registry, subscription or not.
/// Parsed form of a `schedule_tasks.condition` gate. Serialized canonically
/// for storage: `{"kind":"exit","task_index":N,"code":C}` or
/// `{"kind":"signal","event":"site.updated","server_id":S,"timeout_s":T}`.
/// A signal gate always evaluates against the schedule's OWN server: the
/// stored `server_id` is accepted for schema compatibility but never lets a
/// schedule watch another server's webhook events (cross-server oracle).
/// No gate (NULL column, `{"kind":"none"}`, or an absent condition) means the
/// task runs unconditionally.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Gate {
    /// Wait for the earlier task at chain position `task_index` to have
    /// exited with `code` in this run chain.
    Exit { task_index: usize, code: i64 },
    /// Wait up to `timeout_s` (1..=3600) for webhook event `event` to be
    /// emitted for the schedule's own server — any emission counts, whether
    /// or not a webhook subscribes (a global emission counts for every
    /// server). The stored `server_id` is schema-compat baggage: evaluation
    /// always uses the schedule's server, never a foreign one.
    Signal { event: String, server_id: i64, timeout_s: u64 },
}

/// Strict parse of a gate value: unknown kinds and malformed fields are
/// rejected so a typo can never be silently stored as "no gate".
fn gate_from_value(v: &serde_json::Value) -> Result<Option<Gate>> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("gate condition must be a JSON object"))?;
    let kind = obj
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or_else(|| anyhow::anyhow!("gate condition missing \"kind\""))?;
    match kind {
        "none" => Ok(None),
        "exit" => {
            let task_index = obj
                .get("task_index")
                .and_then(|x| x.as_i64())
                .filter(|x| *x >= 0)
                .ok_or_else(|| anyhow::anyhow!("exit gate needs an integer task_index >= 0"))?;
            let code = obj
                .get("code")
                .and_then(|x| x.as_i64())
                .ok_or_else(|| anyhow::anyhow!("exit gate needs an integer code"))?;
            Ok(Some(Gate::Exit {
                task_index: task_index as usize,
                code,
            }))
        }
        "signal" => {
            let event = obj
                .get("event")
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("signal gate needs a non-empty event"))?;
            let server_id = obj
                .get("server_id")
                .and_then(|x| x.as_i64())
                .ok_or_else(|| anyhow::anyhow!("signal gate needs an integer server_id"))?;
            let timeout_s = obj
                .get("timeout_s")
                .and_then(|x| x.as_u64())
                .filter(|t| (1..=3600).contains(t))
                .ok_or_else(|| {
                    anyhow::anyhow!("signal gate timeout_s must be an integer in 1..=3600")
                })?;
            Ok(Some(Gate::Signal {
                event,
                server_id,
                timeout_s,
            }))
        }
        other => bail!("unknown gate kind: {other}"),
    }
}

/// Validate a task's `condition` and return the canonical JSON to store
/// (`None` = unconditional). `own_index` is the task's 0-based position in
/// the chain: an exit gate may only reference an earlier task. Used by the
/// schedules API on create / add-task.
pub fn validate_condition(
    cond: Option<&serde_json::Value>,
    own_index: usize,
) -> Result<Option<String>> {
    let Some(v) = cond else {
        return Ok(None);
    };
    let Some(gate) = gate_from_value(v)? else {
        return Ok(None); // {"kind":"none"} normalizes to no gate
    };
    if let Gate::Exit { task_index, .. } = gate {
        if task_index >= own_index {
            bail!(
                "exit gate task_index must reference an earlier task (index < {own_index})"
            );
        }
    }
    Ok(Some(serde_json::to_string(&gate)?))
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
    let conn = db.get()?;
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
    let conn = db.get()?;
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
    let (max_retries, backoff_s, owo, tz) = conn.query_row(
        "SELECT max_retries, retry_backoff_s, only_when_online, schedule_tz FROM schedules WHERE id=?1",
        [schedule_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("schedule is not a JSON object"))?;
    obj.insert("max_retries".to_string(), serde_json::json!(max_retries));
    obj.insert("retry_backoff_s".to_string(), serde_json::json!(backoff_s));
    obj.insert("only_when_online".to_string(), serde_json::json!(owo != 0));
    obj.insert("timezone".to_string(), serde_json::json!(tz));
    Ok(())
}

/// Merge each task's stored `condition` (parsed back to JSON) into the
/// `tasks` array of a serialized schedule, so the API surface exposes the
/// gate exactly as it was accepted. `models::ScheduleTask` predates gates,
/// so the column is attached here instead of touching the model.
fn enrich_task_conditions(
    v: &mut serde_json::Value,
    conn: &rusqlite::Connection,
    schedule_id: i64,
) -> Result<()> {
    let obj = v
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("schedule is not a JSON object"))?;
    let Some(tasks) = obj.get_mut("tasks").and_then(|t| t.as_array_mut()) else {
        return Ok(());
    };
    let mut conds: HashMap<i64, Option<serde_json::Value>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT id, condition FROM schedule_tasks WHERE schedule_id=?1")?;
    let rows = stmt.query_map([schedule_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for r in rows {
        let (tid, raw) = r?;
        // Stored values are canonical (API-validated); an unparsable cell
        // surfaces as null rather than failing the whole listing.
        conds.insert(tid, raw.and_then(|s| serde_json::from_str(&s).ok()));
    }
    for t in tasks {
        if let Some(id) = t.get("id").and_then(|x| x.as_i64()) {
            if let Some(obj) = t.as_object_mut() {
                let cond = conds.get(&id).and_then(|c| c.clone());
                obj.insert(
                    "condition".to_string(),
                    cond.unwrap_or(serde_json::Value::Null),
                );
            }
        }
    }
    Ok(())
}

/// Serialize one schedule including the v7 retry/online columns and each
/// task's flow-gate condition.
pub fn schedule_json(db: &Db, id: i64) -> Result<serde_json::Value> {
    let mut v = serde_json::to_value(models::get_schedule(db, id)?)?;
    let conn = db.get()?;
    add_policy(&mut v, &conn, id)?;
    enrich_task_conditions(&mut v, &conn, id)?;
    Ok(v)
}

/// Serialize all schedules of a server including the v7 retry/online columns
/// and each task's flow-gate condition.
pub fn schedule_list_json(db: &Db, server_id: i64) -> Result<serde_json::Value> {
    let schedules = models::list_schedules(db, server_id)?;
    let conn = db.get()?;
    let mut pols: HashMap<i64, (i64, i64, i64, String)> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id,max_retries,retry_backoff_s,only_when_online,schedule_tz FROM schedules WHERE server_id=?1",
        )?;
        let rows = stmt.query_map([server_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for r in rows {
            let (id, mr, bs, owo, tz) = r?;
            pols.insert(id, (mr, bs, owo, tz));
        }
    }
    let mut out = Vec::new();
    for s in schedules {
        let mut v = serde_json::to_value(&s)?;
        if let Some(&(mr, bs, owo, ref tz)) = pols.get(&s.id) {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("max_retries".to_string(), serde_json::json!(mr));
                obj.insert("retry_backoff_s".to_string(), serde_json::json!(bs));
                obj.insert("only_when_online".to_string(), serde_json::json!(owo != 0));
                obj.insert("timezone".to_string(), serde_json::json!(tz));
            }
        }
        enrich_task_conditions(&mut v, &conn, s.id)?;
        out.push(v);
    }
    Ok(serde_json::json!(out))
}

/// Reclaim rows orphaned as `running` by a crashed/restarted process — but
/// never a row this process is still actively executing. A run is live when
/// its `in_flight_tag` matches this incarnation and its id is in the live
/// set; a foreign or missing tag means the owning process is gone, and a
/// tagless row is reclaimed once it has aged past `STALE_RUN_AGE_S` (the
/// age gate keeps a just-crashed legacy row from being misjudged). Called
/// once before the loop and then on a cadence.
fn recover_stale_running(db: &Db) -> Result<usize> {
    let live = SCHED_STATE.live.lock().clone();
    let conn = db.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, triggered_at, in_flight_tag FROM schedule_runs WHERE status='running'",
    )?;
    let rows: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let now = Utc::now();
    let tag = SCHED_STATE.tag.as_str();
    let mut n = 0;
    for (id, triggered_at, in_flight_tag) in rows {
        let stale = match in_flight_tag.as_deref() {
            Some(t) if t == tag => !live.contains(&id),
            Some(_) => true,
            None => match DateTime::parse_from_rfc3339(&triggered_at) {
                Ok(t) => {
                    now.signed_duration_since(t.with_timezone(&Utc))
                        > chrono::Duration::seconds(STALE_RUN_AGE_S)
                }
                // Unparseable stored time: cannot prove liveness, reclaim.
                Err(_) => true,
            },
        };
        if stale {
            conn.execute(
                "UPDATE schedule_runs SET status='failed', finished_at=?1 WHERE id=?2",
                rusqlite::params![now.to_rfc3339(), id],
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Delete terminal run history older than `RUN_RETENTION_DAYS` so the runs
/// table cannot grow without bound. Pending rows are untouched (they still
/// owe an execution); `retry` rows carry no `finished_at`, so the claim time
/// is the fallback age anchor.
fn prune_terminal_runs(db: &Db) -> Result<usize> {
    let cutoff = (Utc::now() - chrono::Duration::days(RUN_RETENTION_DAYS)).to_rfc3339();
    let conn = db.get()?;
    let n = conn.execute(
        "DELETE FROM schedule_runs
         WHERE status IN ('success', 'failed', 'retry')
           AND COALESCE(finished_at, triggered_at) < ?1",
        [cutoff],
    )?;
    Ok(n)
}

/// Atomically claim a fresh attempt for a schedule: insert a `running` row only
/// when no other `running` or `pending` attempt exists for that schedule (at
/// most one active attempt, enforced even when run_now races the background
/// loop; a queued retry must execute before a fresh fire may start, so a
/// failed run is never duplicated). Returns the new run id, or `None` when
/// another attempt is already active or a retry is outstanding.
fn claim_fresh_run(
    conn: &rusqlite::Connection,
    schedule_id: i64,
    now: &str,
) -> Result<Option<i64>> {
    let n = conn.execute(
        "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt,in_flight_tag)
         SELECT ?1, ?2, 'running', 1, ?3
         WHERE NOT EXISTS (
             SELECT 1 FROM schedule_runs
             WHERE schedule_id = ?1 AND status IN ('running', 'pending')
         )",
        rusqlite::params![schedule_id, now, SCHED_STATE.tag.as_str()],
    )?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(conn.last_insert_rowid()))
}

/// Atomically claim a pending retry as `running`, refusing when the schedule
/// is disabled, already has another `running` attempt, or has vanished (a
/// deleted schedule's subquery yields NULL and matches nothing). Also turns an
/// unclaimed crash into a stale `running` row that recovery marks failed.
fn claim_pending_retry(conn: &rusqlite::Connection, run_id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE schedule_runs SET status='running', in_flight_tag=?2
         WHERE id = ?1 AND status = 'pending'
           AND (SELECT enabled FROM schedules
                WHERE id = (SELECT schedule_id FROM schedule_runs WHERE id = ?1)) = 1
           AND NOT EXISTS (
             SELECT 1 FROM schedule_runs
             WHERE schedule_id = (SELECT schedule_id FROM schedule_runs WHERE id = ?1)
               AND id <> ?1 AND status = 'running'
           )",
        rusqlite::params![run_id, SCHED_STATE.tag.as_str()],
    )?;
    Ok(n > 0)
}

pub async fn run_loop(sched: Scheduler) {
    let _ = blocking(sched.db.clone(), |db| recover_stale_running(&db)).await;
    let _ = blocking(sched.db.clone(), |db| prune_terminal_runs(&db)).await;
    // Jitter the first tick so a restart does not re-fire every due schedule
    // in lockstep (thundering herd on the DB and node APIs). The RNG must be
    // consumed before the sleep: ThreadRng is not Send and would poison the
    // run_loop future (which tokio::spawn requires).
    let jitter_ms = rand::thread_rng().gen_range(0..=10_000u64);
    tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
    let mut tick = interval(Duration::from_secs(10));
    // A tick that stalls longer than the interval must not re-fire in a burst
    // once it resumes; skip the missed ticks instead.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Defense in depth: a DB error or panic-guard failure can leave a run row
    // 'running' (fresh fires and retries both refuse to claim), which would
    // wedge the schedule until restart. Recover stale rows on a cadence.
    const STALE_RECOVERY_EVERY_TICKS: u64 = 30; // 30 ticks = 5 minutes
    let mut tick_count: u64 = 0;
    loop {
        tick.tick().await;
        if !sched.running.load(Ordering::Relaxed) {
            break;
        }
        tick_count += 1;
        if tick_count.is_multiple_of(STALE_RECOVERY_EVERY_TICKS) {
            if let Err(e) = blocking(sched.db.clone(), |db| recover_stale_running(&db)).await {
                tracing::warn!("scheduler stale-run recovery: {e}");
            }
            if let Err(e) = blocking(sched.db.clone(), |db| prune_terminal_runs(&db)).await {
                tracing::warn!("scheduler run-history prune: {e}");
            }
        }
        // Background loop is gated by the feature flag toggled in the API.
        if !crate::SETTINGS.features.enable_schedules {
            continue;
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
        // Publish the tick clock before any work: a tick counts even when
        // its schedule queries fail, so `last_tick_at` tracks liveness.
        LAST_TICK.store(now.timestamp().max(0) as u64, Ordering::Relaxed);
        let ids: Vec<i64> = self
            .db
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM schedules WHERE enabled=1")?;
                let mapped = stmt.query_map([], |r| r.get::<_, i64>(0))?;
                let rows: rusqlite::Result<Vec<i64>> = mapped.collect();
                Ok(rows?)
            })
            .await?;
        for id in ids {
            let mut sch = blocking(self.db.clone(), move |db| models::get_schedule(&db, id)).await?;
            let due = match &sch.next_run_at {
                Some(n) => match DateTime::parse_from_rfc3339(n) {
                    Ok(d) => d.with_timezone(&Utc),
                    Err(_) => {
                        // Corrupt stored time: recompute from now instead of
                        // treating the schedule as due on every tick.
                        tracing::warn!(
                            "schedule {} has corrupt next_run_at {:?}; recomputing",
                            sch.id,
                            n
                        );
                        match next_run_tz(
                            &sch.cron_expr,
                            Utc::now(),
                            schedule_tz_of(self.db.clone(), sch.id).await,
                        ) {
                            Ok(n) => {
                                let next = n.to_rfc3339();
                                let _ = blocking(self.db.clone(), move |db| {
                                    models::set_schedule_next(&db, sch.id, Some(&next))
                                })
                                .await;
                                n
                            }
                            Err(_) => continue,
                        }
                    }
                },
                None => {
                    // compute first next
                    match next_run_tz(
                        &sch.cron_expr,
                        Utc::now(),
                        schedule_tz_of(self.db.clone(), sch.id).await,
                    ) {
                        Ok(n) => {
                            let next = n.to_rfc3339();
                            let _ = blocking(self.db.clone(), move |db| {
                                models::set_schedule_next(&db, sch.id, Some(&next))
                            })
                            .await;
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
        let retries: Vec<(i64, i64, i64)> = self
            .db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, schedule_id, attempt FROM schedule_runs WHERE status='pending' AND triggered_at <= ?1 ORDER BY id LIMIT 50",
                )?;
                let mapped = stmt.query_map([now.to_rfc3339()], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?.unwrap_or(1),
                    ))
                })?;
                // LIMIT bounds the SQL fetch; take() caps the slice as well.
                let rows: rusqlite::Result<Vec<(i64, i64, i64)>> = mapped.take(50).collect();
                Ok(rows?)
            })
            .await?;
        for (run_id, schedule_id, attempt) in retries {
            let Ok(sch) = blocking(self.db.clone(), move |db| models::get_schedule(&db, schedule_id)).await else {
                continue; // schedule vanished; leave the orphaned run alone
            };
            let Ok(pol) = blocking(self.db.clone(), move |db| policy(&db, schedule_id)).await else {
                continue;
            };
            // Disabled schedules never execute their queued retries; the run
            // stays pending until the schedule is re-enabled.
            if !should_run(
                sch.enabled,
                pol.only_when_online,
                self.is_online(sch.server_id).await,
            ) {
                continue;
            }
            // Atomically mark the retry running so no other attempt claims it,
            // and so a crash leaves a row recoverable as stale. At most one
            // running attempt may exist per schedule; a competing attempt wins
            // and this retry waits for the next tick.
            let claimed = self
                .db
                .call(move |conn| claim_pending_retry(conn, run_id))
                .await?;
            if !claimed {
                continue;
            }
            // The claim is now ours: register it as live so the periodic
            // stale-run recovery never reclaims it, then hand the attempt to
            // its own task (bounded by the concurrency semaphore) so one slow
            // retry cannot stall the retry queue or other schedules.
            mark_live(run_id);
            self.spawn_attempt(&sch, run_id, attempt, pol);
        }
        Ok(())
    }

    /// Run `f` on its own tokio task and await it, converting a panic into an
    /// `Err` carrying the payload. A panic inside the task unwinds only that
    /// task; the scheduler loop itself can never die from it.
    async fn guarded<F, T>(f: F) -> std::thread::Result<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let handle = tokio::task::spawn(f);
        match handle.await {
            Ok(v) => Ok(v),
            Err(e) => Err(Self::join_error_payload(e)),
        }
    }

    /// Panic payload for a failed task handle. A panicked task carries its own
    /// payload; a cancelled task (only possible when the runtime shuts down
    /// mid-await) gets a synthetic one so the attempt is marked failed, never
    /// left `running`.
    fn join_error_payload(e: tokio::task::JoinError) -> Box<dyn std::any::Any + Send + 'static> {
        if e.is_panic() {
            e.into_panic()
        } else {
            std::panic::panic_any("scheduler attempt task cancelled")
        }
    }

    /// Hand a claimed attempt to its own task. The caller must have already
    /// marked the run live (see `mark_live`); `run_attempt_guarded` unmarks
    /// it when the attempt finishes. Spawning instead of awaiting means one
    /// slow attempt never stalls the scheduler loop, the retry queue, or the
    /// run_now handler. The concurrency semaphore is not taken here: it is
    /// acquired per task inside `run_attempt`, so a flow-gate wait (up to
    /// 3600s) never occupies an execution slot.
    fn spawn_attempt(&self, sch: &Schedule, run_id: i64, attempt: i64, pol: RetryPolicy) {
        let this = Scheduler {
            db: self.db.clone(),
            procs: self.procs.clone(),
            hub: self.hub.clone(),
            notifier: self.notifier.clone(),
            node_client: self.node_client.clone(),
            running: self.running.clone(),
        };
        let sch = sch.clone();
        tokio::task::spawn(async move {
            this.run_attempt_guarded(&sch, run_id, attempt, &pol).await;
        });
    }


    /// Execute one attempt inside its own task so a panicking task
    /// (`procs.start`, `resolve_startup`, archive copy, ...) cannot kill the
    /// scheduler loop. On a panic the claimed row is marked exactly like a task
    /// error: requeued as pending while retries remain, failed otherwise.
    async fn run_attempt_guarded(
        &self,
        sch: &Schedule,
        run_id: i64,
        attempt: i64,
        pol: &RetryPolicy,
    ) {
        let this = Scheduler {
            db: self.db.clone(),
            procs: self.procs.clone(),
            hub: self.hub.clone(),
            notifier: self.notifier.clone(),
            node_client: self.node_client.clone(),
            running: self.running.clone(),
        };
        // Owned copies for the spawned task; the panic handler below needs the
        // schedule's identity too, so extract it before `sch` moves.
        let sch = sch.clone();
        let sch_id = sch.id;
        let sch_name = sch.name.clone();
        let sch_server_id = sch.server_id;
        let pol = *pol;
        let outcome =
            Self::guarded(async move { this.run_attempt(&sch, run_id, attempt, &pol).await })
                .await;
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("scheduler attempt {run_id}: {e:#}");
                // `run_attempt` only errors on its own DB bookkeeping (a
                // failed finish_run / retry insert), so the row may still be
                // 'running'. Mark it failed best-effort so the schedule is
                // not wedged; the periodic stale-run recovery is the backstop
                // if even this write fails.
                let log = format!("[db-error] {e:#}\n");
                if let Err(mark) = self
                    .db
                    .call(move |conn| finish_run_on(conn, run_id, "failed", &log, true))
                    .await
                {
                    tracing::error!(
                        "scheduler attempt {run_id}: could not mark run failed: {mark:#}"
                    );
                }
            }
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic payload".to_string()
                };
                tracing::error!("scheduler attempt {run_id} panicked: {msg}");
                let log = format!("[panic] {msg}\n");
                let notifier = self.notifier.clone();
                let sch_name_c = sch_name.clone();
                let log_c = log.clone();
                let msg_c = msg.clone();
                let pol_c = pol;
                let _ = blocking(self.db.clone(), move |db| {
                    handle_attempt_failure(
                        &db,
                        &notifier,
                        run_id,
                        attempt,
                        sch_id,
                        &sch_name_c,
                        sch_server_id,
                        &pol_c,
                        &log_c,
                        &msg_c,
                    );
                    Ok(())
                })
                .await;
            }
        }
        // Whatever happened to the attempt, it is no longer executing here;
        // drop it from the live set so a later recovery pass could reclaim
        // the row if the terminal bookkeeping above failed.
        unmark_live(run_id);
    }


    pub async fn execute(&self, sch: &mut Schedule) -> Result<ExecuteOutcome> {
        let expr = sch.cron_expr.clone();
        let now = Utc::now();
        // Copy the id fields up front: the blocking closures below are
        // `'static`, so they must capture owned values, never `sch` itself.
        let sch_id = sch.id;
        let sch_server_id = sch.server_id;
        let tz = schedule_tz_of(self.db.clone(), sch_id).await;
        let pol = blocking(self.db.clone(), move |db| policy(&db, sch_id)).await?;
        // Fresh fire with only_when_online while offline: skip without
        // consuming the schedule's active slot or queueing a run row. The
        // due time is left in place — bounded catch-up — so the next tick
        // re-tries the same fire once the workspace is back; only when the
        // fire ages past the catch-up window is it advanced past (with one
        // notification) so an offline fire is never silently lost nor
        // re-fired forever.
        if !should_run(
            true,
            pol.only_when_online,
            self.is_online(sch.server_id).await,
        ) {
            let due = sch.next_run_at.as_deref().and_then(|n| {
                DateTime::parse_from_rfc3339(n)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            });
            if let Some(due) = due {
                if now.signed_duration_since(due) > chrono::Duration::seconds(OFFLINE_CATCHUP_WINDOW_S) {
                    match next_run_tz(&expr, now, tz) {
                        Ok(n) => {
                            let next = n.to_rfc3339();
                            let _ = blocking(self.db.clone(), move |db| {
                                models::set_schedule_next(&db, sch_id, Some(&next))
                            })
                            .await;
                        }
                        Err(_) => {
                            let _ = blocking(self.db.clone(), move |db| {
                                models::set_schedule_next(&db, sch_id, None)
                            })
                            .await;
                        }
                    }
                    self.notifier.notify(
                        "info",
                        &format!("Schedule '{}' skipped", sch.name),
                        "workspace offline",
                        Some(sch_server_id),
                    );
                }
            }
            return Ok(ExecuteOutcome::Skipped);
        }
        // The fire is being dispatched: advance to the next occurrence so a
        // tick can never re-fire it, in the schedule's own timezone.
        match next_run_tz(&expr, now, tz) {
            Ok(n) => {
                let next = n.to_rfc3339();
                let _ = blocking(self.db.clone(), move |db| {
                    models::set_schedule_next(&db, sch_id, Some(&next))
                })
                .await;
            }
            Err(_) => {
                // Impossible cron: never due again, so the loop cannot churn.
                let _ = blocking(self.db.clone(), move |db| {
                    models::set_schedule_next(&db, sch_id, None)
                })
                .await;
            }
        }
        // At most one active attempt per schedule, robust to run_now racing
        // the loop: the claim is atomic on the locked connection, so a second
        // caller finds the existing 'running' row and drops out.
        let run_id = {
            let now_str = now.to_rfc3339();
            let Some(id) = self
                .db
                .call(move |conn| claim_fresh_run(conn, sch_id, &now_str))
                .await?
            else {
                return Ok(ExecuteOutcome::AlreadyActive);
            };
            id
        };
        // The run is now actually claimed and will start: record last_run_at.
        // Skipped and AlreadyActive attempts above must not stamp history.
        let last = now.to_rfc3339();
        let _ = blocking(self.db.clone(), move |db| {
            models::set_schedule_last(&db, sch_id, Some(&last))
        })
        .await;
        self.notifier.notify(
            "info",
            &format!("Schedule '{}' triggered", sch.name),
            &format!("Running {} task(s)", sch.tasks.len()),
            Some(sch_server_id),
        );
        mark_live(run_id);
        self.spawn_attempt(sch, run_id, 1, pol);
        Ok(ExecuteOutcome::Ran)
    }

    /// Execute the tasks for one run row; marks the row terminal on success or
    /// final failure, or requeues the run as pending when retries remain.
    ///
    /// Retries resume from the persisted task index instead of replaying the
    /// whole chain: earlier tasks may carry side effects (restart, backup,
    /// command) that must never run twice. The index is advanced after every
    /// completed task, so even a panic mid-chain resumes at the right task.
    ///
    /// Flow Gates (iteration 4): a task carrying a `condition` runs only once
    /// the gate passes. An `exit` gate reads the run's persisted `task_exits`
    /// map (earlier tasks always completed before the chain reaches the gate,
    /// so this is instant); a `signal` gate polls the webhook bus's
    /// recent-event registry for the event. A gate that never passes fails the task with its recorded
    /// reason (`gate.timeout` / `gate.exit` / `gate.badref`) and keeps the
    /// task's own index — the gated task never ran, so a retry re-attempts
    /// the gate itself.
    async fn run_attempt(
        &self,
        sch: &Schedule,
        run_id: i64,
        attempt: i64,
        pol: &RetryPolicy,
    ) -> Result<()> {
        // Offline/disabled gating happens before the attempt is claimed (see
        // tick_once's retry pass and execute); a claimed attempt always runs.
        let start_idx = blocking(self.db.clone(), move |db| {
            let conn = db.get()?;
            Ok::<i64, anyhow::Error>(row_task_index_on(&conn, run_id))
        })
        .await
        .unwrap_or(0)
        .min(sch.tasks.len() as i64) as usize;
        let sch_id = sch.id;
        let conds = self
            .db
            .call(move |conn| task_conditions_on(conn, sch_id))
            .await?;
        let mut exits = self
            .db
            .call(move |conn| Ok(read_task_exits_on(conn, run_id)))
            .await
            .unwrap_or_default();
        // Exit-map and index persistence are module-owned SQL: run on the
        // blocking pool via Db::call, snapshotting the map (the closure must
        // be 'static). Errors are logged by the callers below, as before.
        let persist_exits = |snapshot: HashMap<usize, i64>| async move {
            self.db
                .call(move |conn| set_task_exits_on(conn, run_id, &snapshot))
                .await
        };
        let persist_index = |idx: usize| async move {
            self.db
                .call(move |conn| set_task_index_on(conn, run_id, idx))
                .await
        };
        let mut log = String::new();
        for (i, task) in sch.tasks.iter().enumerate().skip(start_idx) {
            // Flow Gate: the task's condition must hold before the task runs.
            // Gate evaluation holds NO concurrency permit: a signal gate may
            // wait up to timeout_s (3600s), and letting a wait occupy one of
            // the MAX_CONCURRENT_ATTEMPTS slots would let 8 concurrent waits
            // exhaust the budget and stall every schedule panel-wide.
            if let Err(e) = self
                .eval_gate(
                    i,
                    sch.server_id,
                    conds.get(&task.id).and_then(|c| c.as_deref()),
                    &exits,
                )
                .await
            {
                exits.insert(i, 1);
                if let Err(err) = persist_exits(exits.clone()).await {
                    tracing::warn!("scheduler attempt {run_id}: could not persist task exits: {err}");
                }
                log.push_str(&format!("[gate] {} {}: {e}\n", task.action, task.payload));
                let notifier = self.notifier.clone();
                let sch_c = sch.clone();
                let err_msg = e.to_string();
                let exits_c = exits.clone();
                let log_c = log.clone();
                let pol_c = *pol;
                blocking(self.db.clone(), move |db| {
                    fail_task(
                        &db,
                        &notifier,
                        &sch_c,
                        run_id,
                        attempt,
                        pol_c,
                        i,
                        &log_c,
                        &err_msg,
                        &exits_c,
                    )
                })
                .await?;
                return Ok(());
            }
            // The permit bounds only the execution phase; it drops at the end
            // of this iteration, so the next gate wait is permit-free again.
            // A closed semaphore degrades to unbounded rather than wedging.
            let _permit = SCHED_STATE.sem.clone().acquire_owned().await.ok();
            match self.run_task(sch.server_id, task).await {
                Ok(TaskOutcome::Ran) => {
                    log.push_str(&format!("[ok] {} {}\n", task.action, task.payload));
                    exits.insert(i, 0);
                    if let Err(e) = persist_exits(exits.clone()).await {
                        tracing::warn!("scheduler attempt {run_id}: could not persist task exits: {e}");
                    }
                    if let Err(e) = persist_index(i + 1).await {
                        tracing::warn!("scheduler attempt {run_id}: could not persist task index: {e}");
                    }
                }
                Ok(TaskOutcome::Skipped) => {
                    log.push_str(&format!("[skip] {} {}\n", task.action, task.payload));
                    exits.insert(i, 0);
                    if let Err(e) = persist_exits(exits.clone()).await {
                        tracing::warn!("scheduler attempt {run_id}: could not persist task exits: {e}");
                    }
                    if let Err(e) = persist_index(i + 1).await {
                        tracing::warn!("scheduler attempt {run_id}: could not persist task index: {e}");
                    }
                }
                Err(e) => {
                    log.push_str(&format!("[err] {} {}: {e}\n", task.action, task.payload));
                    exits.insert(i, 1);
                    if let Err(err) = persist_exits(exits.clone()).await {
                        tracing::warn!("scheduler attempt {run_id}: could not persist task exits: {err}");
                    }
                    let notifier = self.notifier.clone();
                    let sch_c = sch.clone();
                    let err_msg = e.to_string();
                    let exits_c = exits.clone();
                    let log_c = log.clone();
                    let pol_c = *pol;
                    blocking(self.db.clone(), move |db| {
                        fail_task(
                            &db,
                            &notifier,
                            &sch_c,
                            run_id,
                            attempt,
                            pol_c,
                            i,
                            &log_c,
                            &err_msg,
                            &exits_c,
                        )
                    })
                    .await?;
                    return Ok(());
                }
            }
        }
        self.db
            .call(move |conn| finish_run_on(conn, run_id, "success", &log, true))
            .await?;
        let sch_c = sch.clone();
        let _ = blocking(self.db.clone(), move |db| {
            emit_schedule_run(&db, &sch_c, run_id, "success", attempt);
            Ok(())
        })
        .await;
        Ok(())
    }



    /// Evaluate the flow gate of the task at chain position `own_index`
    /// against the run's recorded exits. `Ok(())` = no gate, or the gate
    /// holds. `schedule_server_id` is the schedule's own server: a signal
    /// gate always waits on events for THAT server, never for a server id
    /// stored in the condition (a schedule on server A must not be able to
    /// turn server B's run logs into an event oracle).
    async fn eval_gate(
        &self,
        own_index: usize,
        schedule_server_id: i64,
        cond_raw: Option<&str>,
        exits: &HashMap<usize, i64>,
    ) -> Result<()> {
        let Some(raw) = cond_raw else {
            return Ok(());
        };
        let v: serde_json::Value =
            serde_json::from_str(raw).context("gate condition is corrupt JSON")?;
        let Some(gate) = gate_from_value(&v)? else {
            return Ok(());
        };
        match gate {
            Gate::Exit { task_index, code } => {
                if task_index >= own_index {
                    bail!(
                        "gate.badref: task_index {task_index} does not reference an earlier task (own index {own_index})"
                    );
                }
                match exits.get(&task_index) {
                    Some(&recorded) if recorded == code => Ok(()),
                    Some(&recorded) => bail!(
                        "gate.exit: task {task_index} exited with {recorded}, gate requires {code}"
                    ),
                    None => bail!(
                        "gate.badref: task {task_index} has no recorded exit in this run"
                    ),
                }
            }
            // Evaluate against the schedule's own server only: the stored
            // `server_id` must never redirect the gate (cross-server oracle).
            Gate::Signal {
                event,
                server_id: _,
                timeout_s,
            } => self.wait_signal(&event, schedule_server_id, timeout_s).await,
        }
    }

    /// Wait up to `timeout_s` for webhook event `event` to have been emitted
    /// for `server_id` — always the schedule's OWN server: callers pass the
    /// schedule's server id, never one stored in the condition, so a gate
    /// can never observe another server's events. Events are observed through
    /// the webhook bus's in-memory recent-event registry (services/webhooks),
    /// which records every emission whether or not any webhook subscribes, so
    /// a signal gate no longer needs an enabled subscribed webhook. The wait
    /// counts emissions from the moment it starts, so an event that predates
    /// the gate never satisfies it; timestamps come from the same process's
    /// `SystemTime` clock.
    async fn wait_signal(&self, event: &str, server_id: i64, timeout_s: u64) -> Result<()> {
        let seen_after = SystemTime::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
        loop {
            if webhooks::recent_event_since(event, Some(server_id), seen_after) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "gate.timeout: no '{event}' event for server {server_id} within {timeout_s}s"
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn is_online(&self, server_id: i64) -> bool {
        let Ok(srv) = blocking(self.db.clone(), move |db| models::get_server(&db, server_id)).await
        else {
            return false;
        };
        if srv.node != "local" {
            let node_name = srv.node.clone();
            let Ok(node) = blocking(self.db.clone(), move |db| {
                crate::nodes::get_by_name(&db, &node_name)
            })
            .await
            else {
                return false;
            };
            return matches!(
                self.node_client.stats(&node, &srv.uuid).await,
                Ok(s) if s.pid.is_some()
            );
        }
        self.procs.state(server_id).is_some()
    }

    async fn run_task(&self, server_id: i64, task: &ScheduleTask) -> Result<TaskOutcome> {
        let srv = blocking(self.db.clone(), move |db| models::get_server(&db, server_id)).await?;
        if srv.node != "local" {
            let node_name = srv.node.clone();
            let node = blocking(self.db.clone(), move |db| {
                crate::nodes::get_by_name(&db, &node_name)
            })
            .await?;
            match task.action.as_str() {
                "start" => {
                    if srv.suspended {
                        self.skip_notify(server_id, "start")?;
                        return Ok(TaskOutcome::Skipped);
                    }
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
                    // Never (re)start a suspended server.
                    if srv.suspended {
                        self.skip_notify(server_id, "restart")?;
                        return Ok(TaskOutcome::Skipped);
                    }
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
                    // A suspended workspace is stopped; a snapshot flow would
                    // restart it, so skip the whole backup instead.
                    if srv.suspended {
                        self.skip_notify(server_id, "backup")?;
                        return Ok(TaskOutcome::Skipped);
                    }
                    let was_running = srv.status == "running";
                    if was_running {
                        self.node_client
                            .stop_and_wait(&node, &srv.uuid, 50, Duration::from_millis(100))
                            .await?;
                    }
                    let snapshot_result = self.node_client.snapshot_stream(&node, &srv.uuid).await;
                    // Always try to restart a server we stopped, and never
                    // swallow the restart result: a backup that leaves the
                    // server off must fail the run, not report success.
                    let restart_result = if was_running {
                        Some(
                            self.node_client
                                .power(&node, &srv.uuid, crate::node_protocol::PowerAction::Start)
                                .await,
                        )
                    } else {
                        None
                    };
                    let snapshot = snapshot_result?;
                    if let Some(res) = restart_result {
                        res?;
                    }
                    let uuid = uuid::Uuid::new_v4().to_string();
                    std::fs::create_dir_all(&crate::SETTINGS.paths.backups_dir)?;
                    let path = crate::SETTINGS
                        .paths
                        .backups_dir
                        .join(format!("{uuid}.tar.gz"));
                    // The streaming client hands back the archive as a temp
                    // file on disk (raw streamed body, or the base64-envelope
                    // fallback for pre-streaming agents — identical shape),
                    // so the base64 decode is gone. The copy of a (potentially
                    // large) archive belongs on the blocking pool; on the
                    // tokio worker it would stall the whole scheduler. The
                    // temp file stays alive for the copy: `snapshot` is a
                    // live local until the copy's `.await` completes.
                    let size_bytes = snapshot.size_bytes;
                    let checksum = snapshot.checksum;
                    let temp_path = snapshot.archive.path().to_path_buf();
                    let (size, path_str) = tokio::task::spawn_blocking(move || {
                        std::fs::copy(&temp_path, &path)?;
                        Ok::<_, anyhow::Error>((size_bytes, path.to_string_lossy().to_string()))
                    })
                    .await
                    .context("backup archive write task")??;
                    let backup_name = if task.payload.is_empty() {
                        "scheduled".to_string()
                    } else {
                        task.payload.clone()
                    };
                    let size_db = size as i64;
                    blocking(self.db.clone(), move |db| {
                        crate::models::create_backup(
                            &db,
                            &uuid,
                            srv.id,
                            &backup_name,
                            &path_str,
                            size_db,
                            &checksum,
                            "tar.gz",
                            "",
                        )
                    })
                    .await?;
                }
                other => bail!("unknown schedule action: {other}"),
            }
            return Ok(TaskOutcome::Ran);
        }
        match task.action.as_str() {
            "start" => {
                if srv.suspended {
                    self.skip_notify(server_id, "start")?;
                    return Ok(TaskOutcome::Skipped);
                }
                let srv_ref = srv.clone();
                let (cmd, env) = blocking(self.db.clone(), move |db| {
                    let cmd = crate::services::blueprint::resolve_startup(&db, &srv_ref)?;
                    let env = crate::services::blueprint::env_for_server(&db, &srv_ref);
                    Ok((cmd, env))
                })
                .await?;
                self.procs.start(&srv, &cmd, &env, self.notifier.clone())?;
            }
            "stop" => self.procs.stop(server_id)?,
            "restart" => {
                // A suspended server may be stopped but never (re)started.
                if srv.suspended {
                    self.skip_notify(server_id, "restart")?;
                    return Ok(TaskOutcome::Skipped);
                }
                self.procs.stop(server_id)?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let srv = blocking(self.db.clone(), move |db| models::get_server(&db, server_id)).await?;
                let srv_ref = srv.clone();
                let (cmd, env) = blocking(self.db.clone(), move |db| {
                    let cmd = crate::services::blueprint::resolve_startup(&db, &srv_ref)?;
                    let env = crate::services::blueprint::env_for_server(&db, &srv_ref);
                    Ok((cmd, env))
                })
                .await?;
                self.procs.start(&srv, &cmd, &env, self.notifier.clone())?;
            }
            "kill" => {
                self.procs.kill(server_id)?;
            }
            "command" => {
                let payload = task.payload.trim_matches('\'');
                // Route through the hub's per-server stdin writer so a wedged
                // child can never block the scheduler thread on the stdin
                // mutex. The writer thread is spawned on demand, so there is
                // no unattached-server fallback to procs; a stopped child
                // surfaces as StdinError::WriteFailed.
                self.hub
                    .write_stdin(server_id, self.procs.clone(), format!("{payload}\n"))
                    .await
                    .map_err(|e| anyhow::anyhow!("scheduled command failed: {e:?}"))?;
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
                // Same gate as the remote path: never snapshot a suspended
                // workspace via schedule.
                if srv.suspended {
                    self.skip_notify(server_id, "backup")?;
                    return Ok(TaskOutcome::Skipped);
                }
                let name = if task.payload.is_empty() {
                    format!("scheduled-{}", Utc::now().format("%Y%m%d-%H%M%S"))
                } else {
                    task.payload.clone()
                };
                crate::services::backups::create(&self.db, &crate::SETTINGS, server_id, &name, "")
                    .await?;
            }
            other => bail!("unknown schedule action: {other}"),
        }
        Ok(TaskOutcome::Ran)
    }

    /// Notify once about a task skipped on a suspended workspace.
    fn skip_notify(&self, server_id: i64, action: &str) -> Result<()> {
        self.notifier.notify(
            "warning",
            &format!("Scheduled {action} skipped"),
            "server suspended",
            Some(server_id),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Attempt bookkeeping helpers. Module-owned SQL lives in the conn-based
// `*_on` functions so both execution contexts can use it without touching a
// tokio worker: async callers run them inside `Db::call` closures, and the
// compound helpers below call them on a single checked-out connection inside
// a `blocking` unit. The defaulting/error behavior of the old `Scheduler`
// methods is preserved exactly.

/// Mark a run row terminal (`terminal`) or requeued (`!terminal`).
fn finish_run_on(
    conn: &rusqlite::Connection,
    run_id: i64,
    status: &str,
    log: &str,
    terminal: bool,
) -> Result<()> {
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

/// Raw `task_exits` cell of a run row; defaults to an empty map.
fn row_task_exits_on(conn: &rusqlite::Connection, run_id: i64) -> String {
    conn.query_row(
        "SELECT task_exits FROM schedule_runs WHERE id=?1",
        [run_id],
        |r| r.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "{}".to_string())
}

/// Persisted resume point for `run_id` (next task index to execute).
fn row_task_index_on(conn: &rusqlite::Connection, run_id: i64) -> i64 {
    conn.query_row(
        "SELECT task_index FROM schedule_runs WHERE id=?1",
        [run_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// Persist the run's per-task exit-code map. Errors are logged by callers.
fn set_task_exits_on(
    conn: &rusqlite::Connection,
    run_id: i64,
    exits: &HashMap<usize, i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE schedule_runs SET task_exits=?1 WHERE id=?2",
        rusqlite::params![serde_json::to_string(exits)?, run_id],
    )?;
    Ok(())
}

/// Advance the run's resume point. Errors are logged by the caller.
fn set_task_index_on(conn: &rusqlite::Connection, run_id: i64, idx: usize) -> Result<()> {
    conn.execute(
        "UPDATE schedule_runs SET task_index=?1 WHERE id=?2",
        rusqlite::params![idx as i64, run_id],
    )?;
    Ok(())
}

/// Load every task's stored condition for a schedule, keyed by task id.
fn task_conditions_on(
    conn: &rusqlite::Connection,
    schedule_id: i64,
) -> Result<HashMap<i64, Option<String>>> {
    let mut stmt = conn.prepare("SELECT id, condition FROM schedule_tasks WHERE schedule_id=?1")?;
    let rows = stmt.query_map([schedule_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (tid, cond) = r?;
        out.insert(tid, cond);
    }
    Ok(out)
}


/// Persisted per-task exit codes of the current attempt chain, keyed by
/// task index (JSON map). Corrupt state degrades to an empty map rather
/// than wedging the run.
fn read_task_exits_on(conn: &rusqlite::Connection, run_id: i64) -> HashMap<usize, i64> {
    let raw = row_task_exits_on(conn, run_id);
    serde_json::from_str::<HashMap<usize, i64>>(&raw).unwrap_or_default()
}

/// Enqueue the `schedule.run` event when an attempt's task chain finishes
/// (best-effort, fire and forget): run identity, schedule identity, and
/// the run row's status. Emitted from `run_attempt` finalization only —
/// never per task — so subscribers get exactly one event per attempt
/// outcome (success / failed / retry). The payload is far under the
/// 64 KiB emit cap.
fn emit_schedule_run(db: &Db, sch: &Schedule, run_id: i64, status: &str, attempt: i64) {
    let srv = models::get_server(db, sch.server_id).ok();
    let payload = json!({
        "event": "schedule.run",
        "server_id": sch.server_id,
        "uuid": srv.as_ref().map(|s| s.uuid.clone()),
        "server_name": srv.as_ref().map(|s| s.name.clone()),
        "schedule_id": sch.id,
        "schedule_name": sch.name,
        "run_id": run_id,
        "status": status,
        "attempt": attempt,
        "timestamp": Utc::now().to_rfc3339(),
    });
    webhooks::emit(db, "schedule.run", Some(sch.server_id), payload);
}

/// Failure handling shared by a failed task and a flow gate that never
/// passed: requeue as a pending row (resuming from the task's own index)
/// while retries remain, else mark the run failed. The caller has already
/// recorded the task's outcome in `exits` and persisted it.
fn fail_task(
    db: &Db,
    notifier: &proc::Notifier,
    sch: &Schedule,
    run_id: i64,
    attempt: i64,
    pol: RetryPolicy,
    i: usize,
    log: &str,
    err: &str,
    exits: &HashMap<usize, i64>,
) -> Result<()> {
    if retries_remain(attempt, pol.max_retries) {
        // not terminal: requeue as its own pending row after backoff,
        // resuming from the failing task (never replay earlier tasks)
        let conn = db.get()?;
        let _ = set_task_index_on(&conn, run_id, i);
        finish_run_on(&conn, run_id, "retry", log, false)?;
        emit_schedule_run(db, sch, run_id, "retry", attempt);
        let due = Utc::now() + Duration::from_secs(backoff_delay_s(pol.backoff_s, attempt));
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt,task_index,task_exits) VALUES(?1,?2,'pending',?3,?4,?5)",
            rusqlite::params![
                sch.id,
                due.to_rfc3339(),
                attempt + 1,
                i as i64,
                serde_json::to_string(exits)?
            ],
        )?;
        notifier.notify(
            "warning",
            &format!(
                "Schedule '{}' retrying ({}/{})",
                sch.name,
                attempt + 1,
                pol.max_retries + 1
            ),
            err,
            Some(sch.server_id),
        );
    } else {
        let conn = db.get()?;
        finish_run_on(&conn, run_id, "failed", log, true)?;
        emit_schedule_run(db, sch, run_id, "failed", attempt);
        notifier.notify(
            "error",
            &format!("Schedule '{}' failed", sch.name),
            err,
            Some(sch.server_id),
        );
    }
    Ok(())
}

/// Bookkeeping after a failed or panicked attempt: requeue as a pending
/// row while retries remain, else mark the run failed. A run row must
/// never be left `running` — fresh fires and pending retries both refuse
/// to claim — so when the requeue cannot be persisted the run is marked
/// failed instead; the periodic stale-run recovery is the backstop if
/// even that write fails. Never returns an error: every failure here is
/// logged or surfaced through the notifier.
fn handle_attempt_failure(
    db: &Db,
    notifier: &proc::Notifier,
    run_id: i64,
    attempt: i64,
    sch_id: i64,
    sch_name: &str,
    sch_server_id: i64,
    pol: &RetryPolicy,
    log: &str,
    notify_msg: &str,
) {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("scheduler attempt {run_id}: DB unavailable: {e:#}");
            notifier.notify(
                "error",
                &format!("Schedule '{sch_name}' failed"),
                notify_msg,
                Some(sch_server_id),
            );
            return;
        }
    };
    if retries_remain(attempt, pol.max_retries) {
        if let Err(e) = finish_run_on(&conn, run_id, "retry", log, false) {
            tracing::error!("scheduler attempt {run_id}: could not mark retry: {e:#}");
            // The row is still 'running'; best-effort terminal mark so a
            // later recovery pass can still unblock the schedule.
            let _ = finish_run_on(&conn, run_id, "failed", log, true);
        }
        let due = Utc::now() + Duration::from_secs(backoff_delay_s(pol.backoff_s, attempt));
        // Resume-from-task-index: the retry re-runs from the failing
        // task, never the whole chain (earlier tasks may have side
        // effects that must not replay). The run's persisted task_exits
        // map rides along so a later flow gate can still read what
        // earlier tasks exited with.
        let resume = row_task_index_on(&conn, run_id);
        let exits = row_task_exits_on(&conn, run_id);
        let inserted: rusqlite::Result<usize> = conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt,task_index,task_exits) VALUES(?1,?2,'pending',?3,?4,?5)",
            rusqlite::params![sch_id, due.to_rfc3339(), attempt + 1, resume, exits],
        );
        match inserted {
            Ok(_) => notifier.notify(
                "warning",
                &format!(
                    "Schedule '{}' retrying ({}/{})",
                    sch_name,
                    attempt + 1,
                    pol.max_retries + 1
                ),
                notify_msg,
                Some(sch_server_id),
            ),
            Err(e) => {
                // No pending row exists for the retry. Leaving the row as
                // 'retry'/'running' would wedge the schedule until
                // restart, so mark the run failed and surface the error.
                tracing::error!(
                    "scheduler attempt {run_id}: requeue insert failed ({e:#}); marking run failed"
                );
                let failed_log = format!("{log}[requeue-failed: {e:#}]");
                if let Err(mark) = finish_run_on(&conn, run_id, "failed", &failed_log, true) {
                    tracing::error!(
                        "scheduler attempt {run_id}: could not mark run failed: {mark:#}"
                    );
                }
                notifier.notify(
                    "error",
                    &format!("Schedule '{sch_name}' failed"),
                    &format!("{notify_msg}\nrequeue insert failed: {e:#}"),
                    Some(sch_server_id),
                );
            }
        }
    } else {
        if let Err(e) = finish_run_on(&conn, run_id, "failed", log, true) {
            tracing::error!("scheduler attempt {run_id}: could not mark run failed: {e:#}");
        }
        notifier.notify(
            "error",
            &format!("Schedule '{sch_name}' failed"),
            notify_msg,
            Some(sch_server_id),
        );
    }
}
#[cfg(test)]
mod tests {
    use super::{
        backoff_delay_s, backoff_secs, claim_fresh_run, claim_pending_retry, handle_attempt_failure,
        mark_live, policy, recover_stale_running, retries_remain, schedule_json, should_run,
        unmark_live, validate_condition, ExecuteOutcome, Scheduler, TaskOutcome,
        MAX_CONCURRENT_ATTEMPTS, SCHED_STATE,
    };
    use crate::db::Db;
    use crate::services::webhooks;
    use rusqlite::params;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    static DB_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Fresh migrated DB with one user, blueprint, server and schedule.
    fn test_db() -> (Db, i64) {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "voltpanel-scheduler-test-{}-{}.db",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_file(&path);
        let db = crate::db::open(path.to_str().unwrap()).unwrap();
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO users(username,email,password_hash,created_at,updated_at)
             VALUES('u','u@t','x','now','now')",
            [],
        )
        .unwrap();
        let uid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO blueprints(uuid,name,created_at,updated_at)
             VALUES('b','b','now','now')",
            [],
        )
        .unwrap();
        let bid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
             VALUES('s','s',?1,?2,'now','now')",
            params![uid, bid],
        )
        .unwrap();
        let sid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at)
             VALUES(?1,'sched','*/5 * * * *',1,'now')",
            [sid],
        )
        .unwrap();
        let sch_id = conn.last_insert_rowid();
        drop(conn);
        (db, sch_id)
    }

    #[test]
    fn fresh_claim_allows_one_active_attempt_per_schedule() {
        let (db, sch) = test_db();
        let conn = db.get().unwrap();
        // First claim inserts a running row and returns its id.
        let r1 = claim_fresh_run(&conn, sch, "2026-01-01T00:00:00Z")
            .unwrap()
            .expect("first claim must succeed");
        // A second claim while one is running is refused (run_now vs loop).
        assert!(claim_fresh_run(&conn, sch, "2026-01-01T00:01:00Z")
            .unwrap()
            .is_none());
        // Another schedule is unaffected (guard is per-schedule).
        conn.execute(
            "INSERT INTO schedules(server_id,name,cron_expr,enabled,created_at)
             VALUES(?1,'other','*/5 * * * *',1,'now')",
            [1],
        )
        .unwrap();
        let sch2 = conn.last_insert_rowid();
        assert!(claim_fresh_run(&conn, sch2, "2026-01-01T00:00:00Z")
            .unwrap()
            .is_some());
        drop(conn);
        // Terminal rows release the slot for the next fire.
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE schedule_runs SET status='success', finished_at='x' WHERE id=?1",
                [r1],
            )
            .unwrap();
        }
        let conn = db.get().unwrap();
        assert!(claim_fresh_run(&conn, sch, "2026-01-02T00:00:00Z")
            .unwrap()
            .is_some());
    }

    #[test]
    fn pending_retry_is_marked_running_and_guarded() {
        let (db, sch) = test_db();
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
             VALUES(?1,'2026-01-01T00:00:00Z','pending',2)",
            [sch],
        )
        .unwrap();
        let retry = conn.last_insert_rowid();
        // Claim transitions pending -> running.
        assert!(claim_pending_retry(&conn, retry).unwrap());
        let status: String = conn
            .query_row(
                "SELECT status FROM schedule_runs WHERE id=?1",
                [retry],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");
        // A second pending retry for the same schedule waits while one runs.
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
             VALUES(?1,'2026-01-01T00:00:00Z','pending',3)",
            [sch],
        )
        .unwrap();
        let retry2 = conn.last_insert_rowid();
        assert!(!claim_pending_retry(&conn, retry2).unwrap());
        // A non-pending row is never claimable.
        assert!(!claim_pending_retry(&conn, retry).unwrap());
        // Terminal status frees the slot; the queued retry now claims.
        conn.execute(
            "UPDATE schedule_runs SET status='failed', finished_at='x' WHERE id=?1",
            [retry],
        )
        .unwrap();
        assert!(claim_pending_retry(&conn, retry2).unwrap());
    }

    #[test]
    fn recover_marks_orphaned_running_rows_failed_deterministically() {
        let (db, _sch) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
                 VALUES(1,'t','running',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
                 VALUES(1,'t','success',1)",
                [],
            )
            .unwrap();
        }
        let n = recover_stale_running(&db).unwrap();
        assert_eq!(n, 1);
        let conn = db.get().unwrap();
        let (status, finished): (String, Option<String>) = conn
            .query_row(
                "SELECT status, finished_at FROM schedule_runs WHERE status='failed'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert!(finished.is_some(), "stale row must get a finished_at");
        let success: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_runs WHERE status='success'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(success, 1);
        // Deterministic: a second pass reclaims nothing.
        drop(conn);
        assert_eq!(recover_stale_running(&db).unwrap(), 0);
    }

    #[test]
    fn task_order_is_sequence_then_id() {
        let (db, sch) = test_db();
        {
            let conn = db.get().unwrap();
            // Deliberately out-of-rowid order: same sequence, ids 3,1,2.
            conn.execute(
                "INSERT INTO schedule_tasks(id,schedule_id,action,payload,sequence)
                 VALUES(3,?1,'notify','a',0)",
                [sch],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(id,schedule_id,action,payload,sequence)
                 VALUES(1,?1,'notify','b',0)",
                [sch],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(id,schedule_id,action,payload,sequence)
                 VALUES(2,?1,'notify','c',1)",
                [sch],
            )
            .unwrap();
        }
        let s = crate::models::get_schedule(&db, sch).unwrap();
        let payloads: Vec<&str> = s.tasks.iter().map(|t| t.payload.as_str()).collect();
        // seq 0 wins over seq 1; among seq 0, id 1 before id 3.
        assert_eq!(payloads, vec!["b", "a", "c"]);
    }

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
    fn backoff_delay_clamps_to_chrono_range() {
        // Normal retries keep the raw exponential value.
        assert_eq!(backoff_delay_s(30, 1), 30);
        assert_eq!(backoff_delay_s(30, 4), 240);
        // No-backoff base maps to zero.
        assert_eq!(backoff_delay_s(0, 5), 0);
        // Saturated raw backoff must not overflow chrono's TimeDelta when the
        // scheduler computes `Utc::now() + delay` (i64 ns, ~292 years).
        let max_s = (i64::MAX / 1_000_000_000) as u64;
        assert_eq!(backoff_delay_s(30, 100), max_s);
        assert_eq!(backoff_delay_s(i64::MAX, 2), max_s);
    }

    #[test]
    fn should_run_predicate() {
        // enabled and no online gate: runs regardless of liveness
        assert!(should_run(true, false, false));
        assert!(should_run(true, false, true));
        // only_when_online requires a live process
        assert!(should_run(true, true, true));
        assert!(!should_run(true, true, false));
        // disabled never runs (incl. queued retries)
        assert!(!should_run(false, false, true));
        assert!(!should_run(false, true, false));
    }
    #[test]
    fn disabled_schedule_retry_stays_pending() {
        let (db, sch) = test_db();
        // Disable the schedule, then queue a retry that is already due.
        {
            let conn = db.get().unwrap();
            conn.execute("UPDATE schedules SET enabled=0 WHERE id=?1", [sch])
                .unwrap();
            conn.execute(
                "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
                 VALUES(?1,'2020-01-01T00:00:00Z','pending',2)",
                [sch],
            )
            .unwrap();
        }
        let conn = db.get().unwrap();
        let retry_id: i64 = conn
            .query_row(
                "SELECT id FROM schedule_runs WHERE schedule_id=?1 AND status='pending'",
                [sch],
                |r| r.get(0),
            )
            .unwrap();
        // The disabled gate never lets a queued retry claim a slot; the row
        // stays pending until the schedule is re-enabled.
        assert!(!should_run(false, false, true));
        assert!(!claim_pending_retry(&conn, retry_id).unwrap());
        let status: String = conn
            .query_row(
                "SELECT status FROM schedule_runs WHERE id=?1",
                [retry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        drop(conn);
    }

    #[test]
    fn retry_limit_boundary() {
        // max_retries is the number of retries after the initial attempt.
        assert!(!retries_remain(1, 0)); // first failure, max 0: terminal
        assert!(retries_remain(1, 1));
        assert!(!retries_remain(2, 1)); // second failure, max 1: terminal
        assert!(retries_remain(2, 5));
        assert!(retries_remain(5, 5));
        assert!(!retries_remain(6, 5));
        // Negative max_retries is rejected at the API, but never requeues.
        assert!(!retries_remain(1, -1));
    }

    #[test]
    fn fresh_run_resets_attempt_counter() {
        let (db, sch) = test_db();
        let conn = db.get().unwrap();
        // History: a terminal failure and an outstanding pending retry.
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
             VALUES(?1,'2026-01-01T00:00:00Z','failed',1)",
            [sch],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
             VALUES(?1,'2026-01-01T00:00:00Z','pending',2)",
            [sch],
        )
        .unwrap();
        let pending = conn.last_insert_rowid();
        // A queued retry blocks a fresh fire: the retry must execute before a
        // new attempt, or the same task sequence would run twice.
        assert!(
            claim_fresh_run(&conn, sch, "2026-01-02T00:00:00Z")
                .unwrap()
                .is_none(),
            "fresh claim must refuse while a pending retry exists"
        );
        // Once the retry is resolved, the next fresh fire starts the counter
        // over at 1, independent of terminal history.
        conn.execute(
            "UPDATE schedule_runs SET status='failed', finished_at='x' WHERE id=?1",
            [pending],
        )
        .unwrap();
        let id = claim_fresh_run(&conn, sch, "2026-01-02T00:00:00Z")
            .unwrap()
            .expect("fresh claim after pending retry resolved must succeed");
        let attempt: i64 = conn
            .query_row("SELECT attempt FROM schedule_runs WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(attempt, 1);
    }

    #[test]
    fn claiming_retry_preserves_attempt() {
        let (db, sch) = test_db();
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO schedule_runs(schedule_id,triggered_at,status,attempt)
             VALUES(?1,'2026-01-01T00:00:00Z','pending',7)",
            [sch],
        )
        .unwrap();
        let retry = conn.last_insert_rowid();
        assert!(claim_pending_retry(&conn, retry).unwrap());
        let attempt: i64 = conn
            .query_row(
                "SELECT attempt FROM schedule_runs WHERE id=?1",
                [retry],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempt, 7, "claiming a retry must not reset its counter");
    }

    #[test]
    fn skip_outcome_is_not_a_failure() {
        assert_ne!(TaskOutcome::Ran, TaskOutcome::Skipped);
        assert_eq!(TaskOutcome::Skipped, TaskOutcome::Skipped);
    }

    /// Minimal scheduler for bookkeeping tests: nothing runs, only DB state
    /// transitions matter.
    fn test_scheduler(db: Db) -> Scheduler {
        let hub = Arc::new(crate::services::console::ConsoleHub::new(
            crate::config::Config::default(),
        ));
        let procs = Arc::new(crate::services::proc::ProcManager::new(db.clone(), hub.clone()));
        Scheduler {
            db,
            procs,
            hub,
            notifier: Arc::new(crate::services::proc::Notifier::new()),
            node_client: Arc::new(crate::services::node::NodeClient::new().unwrap()),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    /// A finished attempt emits exactly one schedule.run delivery carrying
    /// the run id and the run row's terminal status.
    #[tokio::test]
    async fn successful_attempt_emits_schedule_run_webhook_delivery() {
        let (db, sch_id) = test_db();
        let sched = test_scheduler(db.clone());
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"schedule.run\"]',1,1,'now','now')",
                [],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        // The test schedule has no tasks, so run_attempt walks an empty chain
        // and finalizes as success — the exact emit site we are covering.
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        sched.run_attempt(&sch, run_id, 1, &pol).await.unwrap();

        let conn = db.get().unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "success");
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "schedule.run");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], 1);
        assert_eq!(v["schedule_id"], sch_id);
        assert_eq!(v["run_id"], run_id);
        assert_eq!(v["status"], "success");
        assert_eq!(v["attempt"], 1);
    }

    /// A panicked attempt with retries left requeues a pending row and marks
    /// the claimed row `retry` — the bounded-retry behavior the guard must
    /// keep intact.
    #[tokio::test]
    async fn panicked_attempt_requeues_pending_while_retries_remain() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE schedules SET max_retries=2 WHERE id=?1",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        let sched = test_scheduler(db.clone());
        handle_attempt_failure(
            &sched.db,
            &sched.notifier,
            run_id,
            1,
            sch_id,
            &sch.name,
            sch.server_id,
            &pol,
            "[panic] boom\n",
            "[panic] boom",
        );
        let conn = db.get().unwrap();
        let status: String = conn
            .query_row("SELECT status FROM schedule_runs WHERE id=?1", [run_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "retry");
        let (pending_id, pending_attempt): (i64, i64) = conn
            .query_row(
                "SELECT id, attempt FROM schedule_runs WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pending_attempt, 2, "the retry runs with attempt+1");
        assert_ne!(pending_id, run_id, "the retry is its own pending row");
    }

    /// When the requeue insert itself fails, the run must be marked failed —
    /// never left 'running'/'retry' — or the schedule wedges until restart.
    #[tokio::test]
    async fn requeue_insert_failure_marks_run_failed() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE schedules SET max_retries=2 WHERE id=?1",
                [sch_id],
            )
            .unwrap();
            // Fail only the pending-requeue insert; finish_run's UPDATE must
            // keep working so the failed fallback can persist.
            conn.execute(
                "CREATE TRIGGER block_pending_requeue BEFORE INSERT ON schedule_runs
                 WHEN NEW.status='pending' BEGIN SELECT RAISE(ABORT, 'requeue blocked'); END",
                [],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        let sched = test_scheduler(db.clone());
        handle_attempt_failure(
            &sched.db,
            &sched.notifier,
            run_id,
            1,
            sch_id,
            &sch.name,
            sch.server_id,
            &pol,
            "[panic] boom\n",
            "[panic] boom",
        );
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row(
                "SELECT status, log FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "failed",
            "a run whose requeue failed must be terminal, not stuck running"
        );
        assert!(log.contains("requeue-failed"), "the failure is recorded");
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_runs WHERE status='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "no orphaned pending row may exist");
        // The schedule slot is free again: a fresh fire can claim.
        assert!(
            claim_fresh_run(&conn, sch_id, "2026-01-02T00:00:00Z")
                .unwrap()
                .is_some()
        );
    }

    /// A panicked attempt with no retries left is terminal: marked failed,
    /// nothing queued.
    #[tokio::test]
    async fn panicked_attempt_without_retries_marks_failed() {
        let (db, sch_id) = test_db(); // max_retries defaults to 0
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        assert_eq!(pol.max_retries, 0);
        let sched = test_scheduler(db.clone());
        handle_attempt_failure(
            &sched.db,
            &sched.notifier,
            run_id,
            1,
            sch_id,
            &sch.name,
            sch.server_id,
            &pol,
            "[panic] boom\n",
            "[panic] boom",
        );
        let conn = db.get().unwrap();
        let status: String = conn
            .query_row("SELECT status FROM schedule_runs WHERE id=?1", [run_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "failed");
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_runs WHERE status='pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
    }

    /// A `running` row that this process is actively executing (tagged +
    /// live) must survive stale-run recovery no matter how old it is — a
    /// long backup must never be false-terminalized, or the freed slot would
    /// let a duplicate attempt start. Once the attempt is no longer live the
    /// same pass reclaims it.
    #[test]
    fn live_running_row_survives_stale_recovery() {
        let (db, sch) = test_db();
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch, "2020-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let status = |conn: &rusqlite::Connection, id: i64| -> String {
            conn.query_row("SELECT status FROM schedule_runs WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        {
            let conn = db.get().unwrap();
            assert_eq!(status(&conn, run_id), "running");
        }
        // Live attempt: an hour-old run is still not reclaimed.
        mark_live(run_id);
        assert_eq!(recover_stale_running(&db).unwrap(), 0);
        {
            let conn = db.get().unwrap();
            assert_eq!(status(&conn, run_id), "running");
        }
        // Attempt finished: same pass now reclaims the row as failed.
        unmark_live(run_id);
        assert_eq!(recover_stale_running(&db).unwrap(), 1);
        {
            let conn = db.get().unwrap();
            assert_eq!(status(&conn, run_id), "failed");
        }
    }

    /// A retry must resume from the persisted task index instead of
    /// replaying the whole chain: tasks before the failing one carry side
    /// effects that must never run twice. A run that already completed
    /// tasks 0-1 (task_index=2) and fails on task 2 requeues a pending row
    /// pointing at task 2, and its log shows no replay of 0-1.
    #[tokio::test]
    async fn retry_resumes_from_persisted_task_index() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute("UPDATE schedules SET max_retries=2 WHERE id=?1", [sch_id])
                .unwrap();
            // Two harmless notifications, then a command that fails (no live
            // child to write stdin to on this test server).
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence)
                 VALUES(?1,'notify','first',0)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence)
                 VALUES(?1,'notify','second',1)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence)
                 VALUES(?1,'command','no-such-stdin',2)",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        // Simulate a first attempt that completed tasks 0-1 before dying.
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE schedule_runs SET task_index=2 WHERE id=?1",
                [run_id],
            )
            .unwrap();
        }
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row("SELECT status, log FROM schedule_runs WHERE id=?1", [run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "retry");
        assert!(
            !log.contains("first") && !log.contains("second"),
            "completed tasks must not replay on retry; log was: {log}"
        );
        assert!(log.contains("no-such-stdin"), "failing task is logged");
        let (pending_attempt, pending_index): (i64, i64) = conn
            .query_row(
                "SELECT attempt, task_index FROM schedule_runs
                 WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pending_attempt, 2, "the retry runs with attempt+1");
        assert_eq!(
            pending_index, 2,
            "the retry must resume from the failing task, not from 0"
        );
    }

    /// Condition validation (the API gate): unknown kinds, non-integer
    /// exit codes, exit references to non-earlier tasks, and out-of-range
    /// signal timeouts are all rejected; `none`/absent normalize to no gate.
    #[test]
    fn gate_condition_validation() {
        use serde_json::json;
        // Absent and {"kind":"none"} both mean unconditional.
        assert_eq!(validate_condition(None, 0).unwrap(), None);
        assert_eq!(validate_condition(Some(&json!({"kind": "none"})), 0).unwrap(), None);
        // Canonical exit gate, referencing an earlier task.
        assert_eq!(
            validate_condition(Some(&json!({"kind": "exit", "task_index": 0, "code": 0})), 1)
                .unwrap(),
            Some(r#"{"kind":"exit","task_index":0,"code":0}"#.to_string())
        );
        // A gate on itself or a later task is rejected.
        assert!(
            validate_condition(
                Some(&json!({"kind": "exit", "task_index": 1, "code": 0})),
                1
            )
            .is_err()
        );
        // Non-integer exit code is rejected.
        assert!(
            validate_condition(
                Some(&json!({"kind": "exit", "task_index": 0, "code": "x"})),
                1
            )
            .is_err()
        );
        // Unknown kinds are rejected, never silently stored as "no gate".
        assert!(validate_condition(Some(&json!({"kind": "magic"})), 0).is_err());
        // Canonical signal gate.
        assert_eq!(
            validate_condition(
                Some(&json!({"kind": "signal", "event": "site.updated",
                            "server_id": 1, "timeout_s": 60})),
                0
            )
            .unwrap(),
            Some(
                r#"{"kind":"signal","event":"site.updated","server_id":1,"timeout_s":60}"#
                    .to_string()
            )
        );
        // Empty event and out-of-bounds timeouts are rejected.
        assert!(
            validate_condition(
                Some(&json!({"kind": "signal", "event": "", "server_id": 1, "timeout_s": 60})),
                0
            )
            .is_err()
        );
        assert!(
            validate_condition(
                Some(&json!({"kind": "signal", "event": "x", "server_id": 1, "timeout_s": 0})),
                0
            )
            .is_err()
        );
        assert!(
            validate_condition(
                Some(&json!({"kind": "signal", "event": "x", "server_id": 1, "timeout_s": 3601})),
                0
            )
            .is_err()
        );
        assert!(
            validate_condition(
                Some(&json!({"kind": "signal", "event": "x", "server_id": 1, "timeout_s": 3600})),
                0
            )
            .is_ok()
        );
    }

    /// The API surface exposes each task's gate: schedule_json / the list
    /// serializer attach `condition` (parsed back to JSON) per task, and an
    /// ungated task reports null.
    #[test]
    fn schedule_json_exposes_task_condition() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','a',0,NULL)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','b',1,'{\"kind\":\"signal\",\"event\":\"site.updated\",\"server_id\":1,\"timeout_s\":60}')",
                [sch_id],
            )
            .unwrap();
        }
        let v = schedule_json(&db, sch_id).unwrap();
        let tasks = v["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["condition"], serde_json::Value::Null);
        assert_eq!(tasks[1]["condition"]["kind"], "signal");
        assert_eq!(tasks[1]["condition"]["event"], "site.updated");
        assert_eq!(tasks[1]["condition"]["timeout_s"], 60);
    }

    /// An exit gate referencing an earlier task that already exited with the
    /// required code passes instantly and the gated task runs normally.
    #[tokio::test]
    async fn exit_gate_passes_when_earlier_task_exited_with_code() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','first',0,NULL)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','second',1,'{\"kind\":\"exit\",\"task_index\":0,\"code\":0}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row("SELECT status, log FROM schedule_runs WHERE id=?1", [run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "success");
        assert!(log.contains("[ok] notify first"), "log: {log}");
        assert!(log.contains("[ok] notify second"), "log: {log}");
        let exits_raw: String = conn
            .query_row(
                "SELECT task_exits FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        let exits: serde_json::Value = serde_json::from_str(&exits_raw).unwrap();
        assert_eq!(exits["0"], 0);
        assert_eq!(exits["1"], 0);
    }

    /// An exit gate whose referenced task exited with a different code fails
    /// immediately (the code will never change), keeps the gated task's
    /// index, and requeues a retry that resumes at the gated task itself.
    #[tokio::test]
    async fn exit_gate_mismatch_fails_and_keeps_task_index() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute("UPDATE schedules SET max_retries=1 WHERE id=?1", [sch_id])
                .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','first',0,NULL)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','second',1,'{\"kind\":\"exit\",\"task_index\":0,\"code\":7}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row("SELECT status, log FROM schedule_runs WHERE id=?1", [run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "retry");
        assert!(
            log.contains("gate.exit"),
            "the recorded reason must carry gate.exit; log: {log}"
        );
        assert!(
            !log.contains("[ok] notify second"),
            "the gated task must never run; log: {log}"
        );
        // The retry resumes at the gated task's own index (it never ran).
        let (pending_attempt, pending_index): (i64, i64) = conn
            .query_row(
                "SELECT attempt, task_index FROM schedule_runs
                 WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pending_attempt, 2);
        assert_eq!(pending_index, 1, "the gated task that never ran keeps its index");
        let pending_exits: String = conn
            .query_row(
                "SELECT task_exits FROM schedule_runs
                 WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&pending_exits).unwrap();
        assert_eq!(v["0"], 0, "task 0's exit rides along to the retry");
        assert_eq!(v["1"], 1, "the failed gate records the task's attempt exit");
    }

    /// A signal gate with no matching webhook event times out after
    /// `timeout_s` and fails the task with the recorded `gate.timeout`
    /// reason; the gated task never ran, so its index is kept.
    #[tokio::test]
    async fn signal_gate_times_out_without_event() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','after',0,'{\"kind\":\"signal\",\"event\":\"never.emitted\",\"server_id\":1,\"timeout_s\":1}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let conn = db.get().unwrap();
        let (status, log, task_index): (String, String, i64) = conn
            .query_row(
                "SELECT status, log, task_index FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert!(
            log.contains("gate.timeout"),
            "the recorded reason must carry gate.timeout; log: {log}"
        );
        assert_eq!(task_index, 0, "the gated task that never ran keeps its index");
    }

    /// A signal gate passes once the matching event is emitted for the server
    /// after the gate started waiting; a stale historical emission (before
    /// the gate) must not satisfy it. Events are observed through the bus's
    /// in-memory registry, not the deliveries table.
    #[tokio::test]
    async fn signal_gate_passes_on_matching_webhook_event() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            // One enabled webhook subscribed to the gate's event for server 1.
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"site.updated\"]',1,1,'now','now')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','after',0,'{\"kind\":\"signal\",\"event\":\"site.updated\",\"server_id\":1,\"timeout_s\":10}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        // Run the attempt inline while a spawned task emits the event
        // mid-gate (the scheduler itself is not Clone, so only the emitter
        // goes on its own task).
        let emit_task = tokio::task::spawn({
            let db = db.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                webhooks::emit(&db, "site.updated", Some(1), serde_json::json!({"n": 1}));
            }
        });
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        emit_task.await.unwrap();
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row("SELECT status, log FROM schedule_runs WHERE id=?1", [run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "success", "the gate must pass once the event arrives");
        assert!(log.contains("[ok] notify after"), "log: {log}");
    }

    /// A signal gate passes even when NO webhook is subscribed to the event:
    /// the bus's recent-event registry records every emission, so a gate no
    /// longer depends on an enabled subscribed webhook (the pre-registry
    /// limitation). The emit enqueues zero deliveries — the registry alone
    /// satisfies the gate.
    #[tokio::test]
    async fn signal_gate_passes_without_any_webhook_subscription() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','after',0,'{\"kind\":\"signal\",\"event\":\"site.updated\",\"server_id\":1,\"timeout_s\":10}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        let emit_task = tokio::task::spawn({
            let db = db.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                // No webhook exists for this event: the emit lands only in
                // the registry and enqueues zero deliveries.
                webhooks::emit(&db, "site.updated", Some(1), serde_json::json!({"n": 1}))
            }
        });
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let enqueued = emit_task.await.unwrap();
        assert_eq!(enqueued, 0, "no webhook is subscribed to the event");
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row("SELECT status, log FROM schedule_runs WHERE id=?1", [run_id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            status, "success",
            "the gate must pass on a subscription-free emit"
        );
        assert!(log.contains("[ok] notify after"), "log: {log}");
    }
    /// A signal gate wait must not occupy one of the MAX_CONCURRENT_ATTEMPTS
    /// execution slots: 8 concurrent waits used to exhaust the budget and
    /// stall every schedule panel-wide (cross-tenant DoS). The test holds
    /// every permit itself, so an attempt that needed a permit to wait could
    /// never complete — the gate timing out to a terminal failure proves the
    /// wait was permit-free.
    #[tokio::test]
    async fn signal_gate_wait_holds_no_attempt_slot() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','after',0,
                        '{\"kind\":\"signal\",\"event\":\"never.arrives\",\"server_id\":1,\"timeout_s\":1}')",
                [sch_id],
            )
            .unwrap();
        }
        // Hold the entire concurrency budget: a gate wait that needed a
        // permit would block on acquire and never finish.
        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_ATTEMPTS {
            permits.push(SCHED_STATE.sem.clone().acquire_owned().await.unwrap());
        }
        let sched = test_scheduler(db.clone());
        let mut sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let outcome = sched.execute(&mut sch).await.unwrap();
        assert!(matches!(outcome, ExecuteOutcome::Ran));
        // The gated attempt must still complete: the gate waits, times out,
        // and the run lands as failed — all without ever touching a permit.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let status: String = {
                let conn = db.get().unwrap();
                conn.query_row(
                    "SELECT status FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT 1",
                    [sch_id],
                    |r| r.get(0),
                )
                .unwrap()
            };
            if status != "running" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the signal gate wait must not need an attempt slot"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        drop(permits);
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row(
                "SELECT status, log FROM schedule_runs WHERE schedule_id=?1 ORDER BY id DESC LIMIT 1",
                [sch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert!(log.contains("gate.timeout"), "log: {log}");
    }

    /// A signal gate always evaluates against the schedule's own server: a
    /// gate stored with a foreign server_id must never let a schedule observe
    /// another server's webhook events (run logs would become a cross-server
    /// oracle). The registry records the foreign emission — fresh and
    /// subscription-free — yet the gate on this server still times out.
    #[tokio::test]
    async fn signal_gate_never_watches_another_servers_events() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            // The gate is stored with the FOREIGN server id, but the
            // schedule itself lives on server 1.
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','after',0,
                        '{\"kind\":\"signal\",\"event\":\"foreign.signal\",\"server_id\":2,\"timeout_s\":1}')",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        // A fresh event for the FOREIGN server lands mid-gate. No webhook is
        // subscribed anywhere; the registry records it regardless. The event
        // name is unique to this test so the parallel test runner cannot
        // satisfy the gate through another test's emission.
        let emit_task = tokio::task::spawn({
            let db = db.clone();
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                webhooks::emit(&db, "foreign.signal", Some(2), serde_json::json!({"n": 1}));
            }
        });
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        emit_task.await.unwrap();
        let conn = db.get().unwrap();
        let (status, log): (String, String) = conn
            .query_row(
                "SELECT status, log FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "failed",
            "a foreign server's event must never satisfy the gate; log: {log}"
        );
        assert!(
            log.contains("gate.timeout"),
            "the gate must time out rather than observe another server's events; log: {log}"
        );
    }

    /// Resume-from-task-index must account for gated tasks: a gate that
    /// passed counts as a completed task (its index advances), so a later
    /// failing task resumes past it — and a gate on an earlier task still
    /// reads that task's exit code from the persisted map.
    #[tokio::test]
    async fn resume_across_gated_task_does_not_replay() {
        let (db, sch_id) = test_db();
        {
            let conn = db.get().unwrap();
            conn.execute("UPDATE schedules SET max_retries=2 WHERE id=?1", [sch_id])
                .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','first',0,NULL)",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'notify','gated',1,'{\"kind\":\"exit\",\"task_index\":0,\"code\":0}')",
                [sch_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schedule_tasks(schedule_id,action,payload,sequence,condition)
                 VALUES(?1,'command','no-such-stdin',2,NULL)",
                [sch_id],
            )
            .unwrap();
        }
        let run_id = {
            let conn = db.get().unwrap();
            claim_fresh_run(&conn, sch_id, "2026-01-01T00:00:00Z")
                .unwrap()
                .expect("claim must succeed")
        };
        let sch = crate::models::get_schedule(&db, sch_id).unwrap();
        let pol = policy(&db, sch_id).unwrap();
        test_scheduler(db.clone())
            .run_attempt(&sch, run_id, 1, &pol)
            .await
            .unwrap();
        let conn = db.get().unwrap();
        let log: String = conn
            .query_row(
                "SELECT log FROM schedule_runs WHERE id=?1",
                [run_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(log.contains("[ok] notify first"), "log: {log}");
        assert!(log.contains("[ok] notify gated"), "gated task ran: {log}");
        let (pending_attempt, pending_index): (i64, i64) = conn
            .query_row(
                "SELECT attempt, task_index FROM schedule_runs
                 WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pending_attempt, 2);
        assert_eq!(
            pending_index, 2,
            "the retry resumes past the passed gate, at the failing task"
        );
        let pending_exits: String = conn
            .query_row(
                "SELECT task_exits FROM schedule_runs
                 WHERE schedule_id=?1 AND status='pending'",
                [sch_id],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&pending_exits).unwrap();
        assert_eq!(v["0"], 0);
        assert_eq!(v["1"], 0, "the passed gate records exit 0 for its task");
        assert_eq!(v["2"], 1, "the failing task records its attempt exit");
    }
}