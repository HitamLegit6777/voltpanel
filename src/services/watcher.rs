//! Console watcher engine: evaluates operator-defined patterns against
//! completed console lines and dispatches an action (notify / restart / stop /
//! command) when one matches, subject to a per-watcher cooldown debounce.
//!
//! Design notes (original to VoltPanel, not a cron/reactor port):
//!
//! * **Evaluation is off the hot path's blocking budget.** `ConsoleHub::append`
//!   collects the just-completed line texts and hands them to
//!   [`WatcherEngine::evaluate`], which is *sync and allocation-free on the
//!   no-match path*: it walks a per-server compiled cache and does a literal
//!   `str::contains` or a precompiled `Regex::is_match` per watcher. No DB
//!   round-trip, no lock beyond the `DashMap` shard, per line.
//! * **Cooldown is an in-memory monotonic clock, not a DB read.** Each compiled
//!   watcher carries an `AtomicI64` of the last fire time (unix secs), seeded
//!   from the persisted `last_fired_at` when the cache compiles. A match past
//!   cooldown flips the clock with a single CAS before the action is spawned,
//!   so a burst of matching lines fires exactly once per window even under
//!   concurrent appends.
//! * **Cache invalidation is version-stamped and lazy.** CRUD bumps a per-server
//!   version via [`WatcherEngine::invalidate`]; the next `evaluate` for that
//!   server observes the mismatch and recompiles. No eager rebuilds, no
//!   background sweeper.
//! * **Actions never block the caller.** On a match past cooldown the action is
//!   `spawn`ed onto a captured runtime [`Handle`], so the append thread (which
//!   may be a blocking reaper) returns immediately.
use crate::db::Db;
use crate::models;
use crate::node_protocol::PowerAction;
use crate::services::node::NodeClient;
use crate::services::proc::{Notifier, ProcManager};
use dashmap::DashMap;
use regex::Regex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::runtime::Handle;

/// A pattern matcher compiled once per cache generation.
enum Matcher {
    Literal(String),
    Regex(Regex),
}

impl Matcher {
    #[inline]
    fn is_match(&self, line: &str) -> bool {
        match self {
            Matcher::Literal(s) => line.contains(s.as_str()),
            Matcher::Regex(re) => re.is_match(line),
        }
    }
}

/// One watcher, compiled for the evaluation hot path.
struct CompiledWatcher {
    id: i64,
    matcher: Matcher,
    action: String,
    payload: String,
    cooldown_secs: i64,
    /// Unix seconds of the last fire, seeded from `last_fired_at`. `0` = never.
    last_fired: AtomicI64,
}

impl CompiledWatcher {
    /// Try to claim the cooldown window for a match observed at `now_secs`.
    /// Returns `true` at most once per window: the winning caller CASes the
    /// clock forward, losers see the fresh value and back off.
    #[inline]
    fn try_claim(&self, now_secs: i64) -> bool {
        loop {
            let last = self.last_fired.load(Ordering::Acquire);
            if last != 0 && now_secs.saturating_sub(last) < self.cooldown_secs {
                return false;
            }
            if self
                .last_fired
                .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

/// The compiled watcher set for one server, tagged with the version it was
/// built from so a stale generation triggers a lazy recompile.
struct ServerWatchers {
    version: u64,
    watchers: Vec<CompiledWatcher>,
}

/// Evaluates console lines against per-server watchers and dispatches actions.
pub struct WatcherEngine {
    db: Db,
    notifier: Arc<Notifier>,
    /// The hub owns the engine (`Arc<ConsoleHub>` holds it), so the back edge is
    /// weak to avoid a reference cycle. `command` dispatch upgrades it.
    hub: Weak<super::ConsoleHub>,
    procs: Arc<ProcManager>,
    node_client: Arc<NodeClient>,
    handle: Handle,
    /// server_id -> compiled set. Absent until first evaluate for that server.
    cache: DashMap<i64, Arc<ServerWatchers>>,
    /// server_id -> current desired version. Bumped by CRUD; drives lazy reload.
    versions: DashMap<i64, u64>,
}

impl WatcherEngine {
    pub fn new(
        db: Db,
        notifier: Arc<Notifier>,
        hub: Weak<super::ConsoleHub>,
        procs: Arc<ProcManager>,
        node_client: Arc<NodeClient>,
        handle: Handle,
    ) -> Self {
        Self {
            db,
            notifier,
            hub,
            procs,
            node_client,
            handle,
            cache: DashMap::new(),
            versions: DashMap::new(),
        }
    }

    /// Mark a server's compiled set stale after a watcher CRUD. The next
    /// `evaluate` recompiles from the database.
    pub fn invalidate(&self, server_id: i64) {
        self.versions
            .entry(server_id)
            .and_modify(|v| *v = v.wrapping_add(1))
            .or_insert(1);
    }

    /// Compile (or reuse) the watcher set for `server_id`, honoring the current
    /// version. Returns `None` when the server has no enabled watchers.
    fn resolve(&self, server_id: i64) -> Option<Arc<ServerWatchers>> {
        let want = self.versions.get(&server_id).map(|v| *v).unwrap_or(0);
        if let Some(existing) = self.cache.get(&server_id) {
            if existing.version == want {
                return if existing.watchers.is_empty() {
                    None
                } else {
                    Some(existing.clone())
                };
            }
        }
        // Miss or stale: compile from the database.
        let rows = match models::list_enabled_watchers(&self.db, server_id) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("watcher load failed for server {server_id}: {e}");
                return None;
            }
        };
        let mut watchers = Vec::with_capacity(rows.len());
        for w in rows {
            let matcher = if w.is_regex {
                match Regex::new(&w.pattern) {
                    Ok(re) => Matcher::Regex(re),
                    Err(e) => {
                        tracing::warn!("watcher {} has invalid regex, skipping: {e}", w.id);
                        continue;
                    }
                }
            } else {
                Matcher::Literal(w.pattern.clone())
            };
            // Seed the debounce clock from the persisted fire time so a reload
            // (or a process restart) does not let a watcher immediately re-fire.
            let seed = w
                .last_fired_at
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|dt| dt.timestamp())
                .unwrap_or(0);
            watchers.push(CompiledWatcher {
                id: w.id,
                matcher,
                action: w.action,
                payload: w.action_payload,
                cooldown_secs: w.cooldown_secs.max(0),
                last_fired: AtomicI64::new(seed),
            });
        }
        let compiled = Arc::new(ServerWatchers {
            version: want,
            watchers,
        });
        self.cache.insert(server_id, compiled.clone());
        if compiled.watchers.is_empty() {
            None
        } else {
            Some(compiled)
        }
    }

    /// Evaluate the just-completed `lines` for `server_id`. Sync, cheap, and
    /// allocation-free when nothing matches. Called from `ConsoleHub::append`
    /// after the completed lines are pushed to the ring.
    pub fn evaluate(&self, server_id: i64, lines: &[&str]) {
        if lines.is_empty() {
            return;
        }
        let Some(set) = self.resolve(server_id) else {
            return;
        };
        let now_secs = chrono::Utc::now().timestamp();
        for w in &set.watchers {
            // A watcher fires at most once per evaluate call: the first matching
            // line in the chunk claims the window; later lines see the fresh
            // clock and skip.
            if !lines.iter().any(|line| w.matcher.is_match(line)) {
                continue;
            }
            if !w.try_claim(now_secs) {
                continue;
            }
            self.dispatch(server_id, w.id, &w.action, &w.payload);
        }
    }

    /// Spawn the action for a fired watcher off the caller's thread and stamp
    /// the persisted fire clock. Never blocks the append path.
    fn dispatch(&self, server_id: i64, watcher_id: i64, action: &str, payload: &str) {
        let db = self.db.clone();
        let notifier = self.notifier.clone();
        let procs = self.procs.clone();
        let node_client = self.node_client.clone();
        let hub = self.hub.clone();
        let action = action.to_string();
        let payload = payload.to_string();
        self.handle.spawn(async move {
            // Resolve the server once: routing (local vs remote) and identity
            // for every action below.
            let srv = match crate::db::blocking(db.clone(), move |db| {
                models::get_server(&db, server_id)
            })
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("watcher {watcher_id}: server {server_id} gone: {e}");
                    return;
                }
            };
            let remote = srv.node != "local";
            let node = if remote {
                match crate::db::blocking(db.clone(), {
                    let n = srv.node.clone();
                    move |db| crate::nodes::get_by_name(&db, &n)
                })
                .await
                {
                    Ok(n) => Some(n),
                    Err(e) => {
                        tracing::warn!("watcher {watcher_id}: node {} gone: {e}", srv.node);
                        return;
                    }
                }
            } else {
                None
            };

            match action.as_str() {
                "notify" => {
                    let level = match payload.as_str() {
                        "warn" | "error" => payload.as_str(),
                        _ => "info",
                    };
                    notifier.notify_link(
                        level,
                        "Console watcher",
                        &format!("A watcher matched on server \"{}\"", srv.name),
                        Some(server_id),
                        Some(format!("#/server/{server_id}")),
                    );
                }
                "restart" => {
                    if let Some(node) = &node {
                        if let Err(e) = node_client
                            .power(node, &srv.uuid, PowerAction::Restart)
                            .await
                        {
                            tracing::warn!("watcher {watcher_id}: remote restart failed: {e}");
                        }
                    } else if let Err(e) = Self::local_restart(&db, &procs, &notifier, &srv).await {
                        tracing::warn!("watcher {watcher_id}: restart failed: {e}");
                    }
                }
                "stop" => {
                    if let Some(node) = &node {
                        if let Err(e) =
                            node_client.power(node, &srv.uuid, PowerAction::Stop).await
                        {
                            tracing::warn!("watcher {watcher_id}: remote stop failed: {e}");
                        }
                    } else if let Err(e) = procs.stop(server_id) {
                        tracing::warn!("watcher {watcher_id}: stop failed: {e}");
                    }
                }
                "command" => {
                    if let Some(node) = &node {
                        if let Err(e) =
                            node_client.command(node, &srv.uuid, &payload).await
                        {
                            tracing::warn!("watcher {watcher_id}: remote command failed: {e}");
                        }
                    } else if let Some(hub) = hub.upgrade() {
                        if let Err(e) =
                            hub.write_stdin(server_id, procs.clone(), payload.clone()).await
                        {
                            tracing::warn!("watcher {watcher_id}: command failed: {e:?}");
                        }
                    }
                }
                other => {
                    tracing::warn!("watcher {watcher_id}: unknown action {other:?}");
                    return;
                }
            }

            // Persist the fire for audit/UI (trigger_count, last_fired_at). The
            // in-memory clock already debounced; this is bookkeeping.
            if let Err(e) =
                crate::db::blocking(db, move |db| models::record_watcher_fire(&db, watcher_id))
                    .await
            {
                tracing::warn!("watcher {watcher_id}: record fire failed: {e}");
            }
        });
    }

    /// Local restart: stop, let the drain settle, re-resolve startup, start.
    /// Mirrors the API power path's Restart arm over the same primitives.
    async fn local_restart(
        db: &Db,
        procs: &Arc<ProcManager>,
        notifier: &Arc<Notifier>,
        srv: &models::Server,
    ) -> anyhow::Result<()> {
        procs.stop(srv.id)?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let sid = srv.id;
        let fresh = crate::db::blocking(db.clone(), move |db| models::get_server(&db, sid)).await?;
        let fresh2 = fresh.clone();
        let (cmd, env) = crate::db::blocking(db.clone(), move |db| {
            Ok((
                super::blueprint::resolve_startup(&db, &fresh2)?,
                super::blueprint::env_for_server(&db, &fresh2),
            ))
        })
        .await?;
        procs.start(&fresh, &cmd, &env, notifier.clone())?;
        Ok(())
    }
}

/// Version counter so tests and callers can observe monotonic generations if
/// needed later; kept internal for now.
#[allow(dead_code)]
static GENERATION: AtomicU64 = AtomicU64::new(0);
