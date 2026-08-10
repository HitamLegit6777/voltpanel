//! Database connection & schema (SQLite via rusqlite).
use anyhow::{anyhow, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// A pool of SQLite connections, replacing the old single global
/// `Arc<Mutex<Connection>>`. Concurrent requests now get their own
/// connection (WAL mode keeps readers off the writer's back), so the panel
/// no longer serializes every query on one mutex. Transactions that need
/// serialized check-then-act semantics use `BEGIN IMMEDIATE` and let SQLite
/// arbitrate writers.
///
/// Wraps the `r2d2` pool so the DB-off-worker contract ([`Db::call`],
/// [`Db::run`]) can be inherent methods; [`std::ops::Deref`] keeps every
/// existing `db.get()` call site working unchanged.
#[derive(Clone, Debug)]
pub struct Db(r2d2::Pool<SqliteConnectionManager>);

impl std::ops::Deref for Db {
    type Target = r2d2::Pool<SqliteConnectionManager>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Db {
    /// Execute blocking SQLite work off the async worker thread.
    ///
    /// The connection is checked out from the pool on a `spawn_blocking`
    /// thread, so the closure's queries never stall a Tokio worker. The
    /// closure runs on that connection exactly as a direct `pool.get()`
    /// would: it may start transactions on it, and it must not outlive the
    /// call. A join failure (closure panic, runtime shutdown) is surfaced as
    /// an `anyhow` error, never unwrapped.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let pool = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            f(&mut conn)
        })
        .await
        .map_err(|e| anyhow!("database worker task failed: {e}"))?
    }

    /// Blocking variant for non-async contexts (background threads, tests).
    ///
    /// Runs `f` on a freshly checked-out connection on the calling thread —
    /// the direct `get()` behavior the async handlers used to have, for
    /// places that cannot await (e.g. `Drop` impls).
    pub fn run<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T>,
    {
        let mut conn = self.get()?;
        f(&mut conn)
    }
}

/// Run a pool-based DB closure off the async worker (shared helper; per-module
/// copies are being removed).
///
/// The closure runs on a `spawn_blocking` thread, so its `db.get()` plus
/// queries never stall a Tokio worker. A join failure (closure panic, runtime
/// shutdown) is surfaced as a `db worker failed` error, never unwrapped.
pub async fn blocking<T, F>(db: Db, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Db) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(db))
        .await
        .map_err(|e| anyhow!("db worker failed: {e}"))?
}

/// Cap on concurrent connections. WAL allows many readers; a handful is
/// plenty for the panel's workload and keeps per-connection overhead (and
/// write-lock contention) low. `min_idle` stays 0 so connections are only
/// opened on demand.
const POOL_MAX_SIZE: u32 = 8;
/// How many times a boot that loses the migration write-lock race retries
/// before giving up. Two instances starting against the same file both race
/// the ladder's `BEGIN IMMEDIATE`; the loser blocks up to `busy_timeout`
/// (5s) per attempt, and a slow ladder on the winner (large DB, data
/// backfills) can exceed that. Retrying lets the concurrent booter wait for
/// the winner to commit instead of dying with SQLITE_BUSY.
const MIGRATION_RETRY_ATTEMPTS: u32 = 5;
/// Per-retry sleep, growing with the attempt (0.5s, 1s, 1.5s, ...), on top of
/// the per-attempt `busy_timeout` already spent waiting on the lock.
const MIGRATION_RETRY_BACKOFF_MS: u64 = 500;

pub fn open(path: &str) -> Result<Db> {
    use std::os::unix::fs::PermissionsExt;
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    // Pre-create the database file at 0600 before SQLite ever opens it:
    // SQLite creates a missing file with the process umask applied (typically
    // 0644), and chmodding after the first pooled open still left a window in
    // which the file existed world-readable. The mode is umask-independent
    // (only 0600's own bits are set); the chmod inside the migration block
    // below remains as a repair pass for files created by older versions.
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(p)?;
    }
    // Built fresh per retry attempt: `SqliteConnectionManager` is not
    // `Clone`, and a fresh pool + connection guarantees no stale busy state
    // carries over between attempts.
    let manager = || {
        SqliteConnectionManager::file(path).with_init(|conn| {
            // Same PRAGMAs the single connection used to get. `journal_mode`
            // is sticky in the database file, but re-asserting it keeps every
            // pooled connection explicit; the rest are per-connection and
            // must be set on each new connection. `busy_timeout` comes first
            // so even the `journal_mode` call below — which may need the
            // write lock while a concurrent booter initializes the file —
            // waits politely instead of failing with an instant SQLITE_BUSY.
            conn.pragma_update(None, "busy_timeout", 5000)?;
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
            Ok(())
        })
    };
    // The migration ladder runs BEGIN IMMEDIATE and re-reads user_version
    // inside its transaction; it must see one consistent schema, so it runs
    // on ONE dedicated connection held for the whole ladder. Later
    // connections simply inherit the committed schema.
    //
    // Two instances booting against the same file race the ladder: the
    // loser's BEGIN IMMEDIATE blocks up to busy_timeout (5s) on the winner's
    // write lock, and a slow ladder (large DB, data backfills) can exceed
    // that, so the loser would otherwise die with SQLITE_BUSY instead of
    // waiting. Retry the whole open-and-migrate step on a busy database with
    // a growing backoff. Anything other than "database is locked" propagates
    // immediately — a real schema or I/O error must not be masked by
    // retrying.
    let mut attempt = 0u32;
    loop {
        let pool = match r2d2::Pool::builder()
            .max_size(POOL_MAX_SIZE)
            .build(manager())
        {
            Ok(pool) => pool,
            Err(err) => {
                let err = anyhow::Error::new(err);
                if is_busy(&err) && attempt < MIGRATION_RETRY_ATTEMPTS {
                    attempt += 1;
                    migration_retry_sleep(attempt);
                    continue;
                }
                return Err(err);
            }
        };
        let conn = match pool.get() {
            Ok(conn) => conn,
            Err(err) => {
                let err = anyhow::Error::new(err);
                if is_busy(&err) && attempt < MIGRATION_RETRY_ATTEMPTS {
                    attempt += 1;
                    migration_retry_sleep(attempt);
                    continue;
                }
                return Err(err);
            }
        };
        // Lock down the main file (also repairs a pre-existing looser mode,
        // e.g. from a version predating the pre-create above), then re-apply
        // to the -wal/-shm sidecars that WAL mode creates during migration.
        // SQLite creates the sidecars with the main file's permissions, so
        // the explicit pass is belt-and-suspenders.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        match migrate(&conn) {
            Ok(()) => {
                for sidecar in [format!("{path}-wal"), format!("{path}-shm")] {
                    if std::path::Path::new(&sidecar).exists() {
                        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600))?;
                    }
                }
                return Ok(Db(pool));
            }
            Err(err) if is_busy(&err) && attempt < MIGRATION_RETRY_ATTEMPTS => {
                attempt += 1;
                migration_retry_sleep(attempt);
            }
            Err(err) => return Err(err),
        }
    }
}
/// Sleep the growing backoff for retry `attempt` (1-based), on top of the
/// per-attempt `busy_timeout` already spent waiting on the lock.
fn migration_retry_sleep(attempt: u32) {
    std::thread::sleep(std::time::Duration::from_millis(
        MIGRATION_RETRY_BACKOFF_MS * u64::from(attempt),
    ));
}

/// True when `err` means "database is locked" — the only condition a boot
/// race retries on. A real schema or I/O error must propagate immediately,
/// not be masked by sleeping. `rusqlite` errors carry the typed code; r2d2
/// 0.8 stringifies backend failures (`Error(Option<String>)`), so pool
/// construction errors are recognized by their message text.
fn is_busy(err: &anyhow::Error) -> bool {
    if err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
        )
    }) {
        return true;
    }
    let text = err.to_string();
    text.contains("database is locked") || text.contains("database is busy")
}
/// True when `table` already has a column named `column`.
///
/// Names come from the migration ladder's own compile-time constants, so the
/// identifiers are inlined directly rather than bound as parameters (the
/// table-valued `pragma_table_info` accepts a literal pragma argument).
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let sql =
        format!("SELECT EXISTS(SELECT 1 FROM pragma_table_info('{table}') WHERE name='{column}')");
    let exists: bool = conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(exists)
}

/// `ALTER TABLE ... ADD COLUMN` that is safe to re-run: when the column is
/// already present the statement is skipped, so a ladder resumed after a
/// partial apply (possible only from pre-transactional code, or after a
/// version-stamp downgrade) never fails with "duplicate column name".
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    if !column_exists(conn, table, column)? {
        conn.execute_batch(ddl)?;
    }
    Ok(())
}

/// True when a table named `table` exists in the schema.
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let sql =
        format!("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='{table}')");
    let exists: bool = conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(exists)
}

/// `ALTER TABLE ... RENAME TO` that is safe to re-run: the rename only runs
/// when the source table still exists and the destination does not.
fn rename_table_if_needed(conn: &Connection, from: &str, to: &str) -> Result<()> {
    if table_exists(conn, from)? && !table_exists(conn, to)? {
        conn.execute_batch(&format!("ALTER TABLE {from} RENAME TO {to};"))?;
    }
    Ok(())
}

/// `ALTER TABLE ... RENAME COLUMN` that is safe to re-run: only runs when the
/// old name is still present and the new name is not.
fn rename_column_if_needed(conn: &Connection, table: &str, from: &str, to: &str) -> Result<()> {
    if column_exists(conn, table, from)? && !column_exists(conn, table, to)? {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} RENAME COLUMN {from} TO {to};"
        ))?;
    }
    Ok(())
}

/// `ALTER TABLE ... DROP COLUMN` that is safe to re-run: no-ops when the
/// column is already gone.
fn drop_column_if_exists(conn: &Connection, table: &str, column: &str) -> Result<()> {
    if column_exists(conn, table, column)? {
        conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column};"))?;
    }
    Ok(())
}

/// Apply schema. Uses user_version pragma for incremental migrations.
///
/// The whole ladder runs inside ONE transaction: a failure at any step rolls
/// back every earlier schema change, data backfill, and the `user_version`
/// bump, so the database always lands on a consistent, resumable step.
/// Connection pragmas (`journal_mode`, `foreign_keys`, `busy_timeout`,
/// `wal_autocheckpoint`) are applied before the transaction because SQLite
/// ignores them mid-transaction.
fn migrate(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "wal_autocheckpoint", 1000)?;

    // BEGIN IMMEDIATE takes the write lock up front, so two instances booting
    // against the same file serialize instead of racing the ladder; the
    // version stamp is re-read inside the transaction so the loser observes
    // the winner's commit and skips straight to the current version.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let conn: &Connection = &tx;

    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                username TEXT NOT NULL UNIQUE,
                email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                avatar TEXT NOT NULL DEFAULT '',
                language TEXT NOT NULL DEFAULT 'en',
                theme TEXT NOT NULL DEFAULT 'dark',
                root_admin INTEGER NOT NULL DEFAULT 0,
                active INTEGER NOT NULL DEFAULT 1,
                twofa_secret TEXT,
                about TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                token TEXT NOT NULL UNIQUE,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                user_agent TEXT NOT NULL DEFAULT '',
                ip TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0,
                remember INTEGER NOT NULL DEFAULT 0,
                last_seen TEXT
            );

            CREATE TABLE IF NOT EXISTS api_keys (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                token TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL DEFAULT 'default',
                created_at TEXT NOT NULL,
                last_used TEXT,
                scopes TEXT NOT NULL DEFAULT 'full'
            );

            CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                blueprint_id INTEGER NOT NULL REFERENCES blueprints(id) ON DELETE RESTRICT,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'offline',
                runtime_hint TEXT NOT NULL DEFAULT '',
                startup TEXT NOT NULL DEFAULT '',
                node TEXT NOT NULL DEFAULT 'local',
                port INTEGER,
                memory_mb INTEGER NOT NULL DEFAULT 1024,
                disk_mb INTEGER NOT NULL DEFAULT 8192,
                cpu_percent INTEGER NOT NULL DEFAULT 100,
                threads TEXT NOT NULL DEFAULT '',
                suspended INTEGER NOT NULL DEFAULT 0,
                auto_restart INTEGER NOT NULL DEFAULT 0,
                restart_count INTEGER NOT NULL DEFAULT 0,
                ignore_oom INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS server_variables (
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                blueprint_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (server_id, key)
            );

            CREATE TABLE IF NOT EXISTS subusers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                permissions TEXT NOT NULL DEFAULT '[]',
                UNIQUE(server_id, user_id)
            );

            CREATE TABLE IF NOT EXISTS backups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                checksum TEXT NOT NULL DEFAULT '',
                format TEXT NOT NULL DEFAULT 'zip',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS databases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                db_type TEXT NOT NULL DEFAULT 'mysql',
                host TEXT NOT NULL DEFAULT '127.0.0.1',
                port INTEGER NOT NULL DEFAULT 3306,
                db_name TEXT NOT NULL,
                username TEXT NOT NULL,
                password TEXT NOT NULL,
                max_conns INTEGER NOT NULL DEFAULT 10,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schedules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                cron_expr TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_run_at TEXT,
                next_run_at TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS schedule_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id INTEGER NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
                action TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '',
                sequence INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS schedule_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id INTEGER NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
                triggered_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                log TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS allocations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                port INTEGER NOT NULL,
                assigned_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS audit_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER,
                action TEXT NOT NULL,
                target TEXT NOT NULL DEFAULT '',
                ip TEXT NOT NULL DEFAULT '',
                details TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS websites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                domain TEXT NOT NULL,
                root_dir TEXT NOT NULL DEFAULT '/',
                port INTEGER,
                proxy_type TEXT NOT NULL DEFAULT 'static',
                ssl INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS rate_limits (
                key TEXT PRIMARY KEY,
                window_start INTEGER NOT NULL,
                count INTEGER NOT NULL DEFAULT 0
            );

            PRAGMA user_version = 1;
            "#,
        )?;

        // Fresh installs create the workload-definition table directly under
        // its final name. The guard keeps a full re-run from user_version 0
        // against an already-migrated database (which already has
        // `blueprints`) from recreating it alongside the migrated table;
        // pre-v5 databases keep their `egss` rows and reach `blueprints`
        // via the existence-guarded v5 rename below.
        if !table_exists(conn, "blueprints")? {
            conn.execute_batch(
                r#"
            CREATE TABLE IF NOT EXISTS blueprints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                author TEXT NOT NULL DEFAULT '',
                category TEXT NOT NULL DEFAULT 'generic',
                runtime_hint TEXT NOT NULL DEFAULT 'alpine',
                startup TEXT NOT NULL DEFAULT '',
                default_config TEXT,
                config_files TEXT,
                install_script TEXT,
                variables TEXT NOT NULL DEFAULT '[]',
                stop_command TEXT NOT NULL DEFAULT 'stop',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
            )?;
        }
    }
    if version < 2 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL UNIQUE,
                public_url TEXT NOT NULL,
                secret TEXT NOT NULL,
                enrollment_token TEXT UNIQUE,
                enrolled INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                maintenance INTEGER NOT NULL DEFAULT 0,
                schedulable INTEGER NOT NULL DEFAULT 1,
                location TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                memory_limit_mb INTEGER NOT NULL DEFAULT 0,
                disk_limit_mb INTEGER NOT NULL DEFAULT 0,
                cpu_limit_percent INTEGER NOT NULL DEFAULT 0,
                memory_overallocate INTEGER NOT NULL DEFAULT 0,
                disk_overallocate INTEGER NOT NULL DEFAULT 0,
                daemon_version TEXT NOT NULL DEFAULT '',
                hostname TEXT NOT NULL DEFAULT '',
                os TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL DEFAULT '',
                capacity_json TEXT NOT NULL DEFAULT '{}',
                last_heartbeat TEXT,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS node_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                level TEXT NOT NULL DEFAULT 'info',
                kind TEXT NOT NULL,
                message TEXT NOT NULL DEFAULT '',
                payload TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS server_transfers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                source_node TEXT NOT NULL,
                target_node TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                bytes_total INTEGER NOT NULL DEFAULT 0,
                bytes_done INTEGER NOT NULL DEFAULT 0,
                error TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_nodes_enabled ON nodes(enabled, schedulable, maintenance);
            CREATE INDEX IF NOT EXISTS idx_node_events_node ON node_events(node_id, id DESC);
            CREATE INDEX IF NOT EXISTS idx_server_node ON servers(node);
            PRAGMA user_version = 2;
            "#,
        )?;
    }
    if version < 3 {
        conn.execute_batch(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_servers_node_port_unique
                ON servers(node, port) WHERE port IS NOT NULL AND deleted=0;
            PRAGMA user_version = 3;
            "#,
        )?;
    }
    if version < 4 {
        add_column_if_missing(
            conn,
            "allocations",
            "node",
            "ALTER TABLE allocations ADD COLUMN node TEXT NOT NULL DEFAULT 'local';",
        )?;
        conn.execute_batch(
            r#"
            UPDATE allocations SET node=COALESCE((SELECT node FROM servers WHERE servers.id=allocations.server_id),'local');
            DELETE FROM allocations WHERE id NOT IN (SELECT MIN(id) FROM allocations GROUP BY node,port);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_allocations_node_port ON allocations(node,port);
            UPDATE servers SET port=(SELECT MIN(port) FROM allocations WHERE allocations.server_id=servers.id) WHERE port IS NULL;
            PRAGMA user_version = 4;
            "#,
        )?;
    }
    if version < 5 {
        // Native VoltPanel domain naming: the blueprint table replaces the legacy eggs table,
        // and columns that were never read by the isolation layer are removed.
        // Renames and drops are guarded so a ladder resumed from a lower
        // version stamp never trips "already exists" / "no such column".
        rename_table_if_needed(conn, "egss", "blueprints")?;
        rename_column_if_needed(conn, "blueprints", "docker_image", "runtime_hint")?;
        drop_column_if_exists(conn, "blueprints", "config_files")?;
        rename_column_if_needed(conn, "servers", "egg_id", "blueprint_id")?;
        rename_column_if_needed(conn, "servers", "docker_image", "runtime_hint")?;
        drop_column_if_exists(conn, "servers", "threads")?;
        drop_column_if_exists(conn, "servers", "ignore_oom")?;
        rename_column_if_needed(conn, "server_variables", "egg_id", "blueprint_id")?;
        conn.execute_batch("PRAGMA user_version = 5;")?;
    }
    if version < 6 {
        // Typed capability model: subusers gain a role preset, and legacy
        // free-form permission tokens are normalized to canonical capabilities.
        add_column_if_missing(
            conn,
            "subusers",
            "role",
            "ALTER TABLE subusers ADD COLUMN role TEXT NOT NULL DEFAULT 'custom';",
        )?;
        conn.execute_batch("PRAGMA user_version = 6;")?;
        normalize_subuser_permissions(conn)?;
    }
    if version < 7 {
        // Telemetry, blueprint revisions, capability-scoped keys, webhook bus,
        // schedule retry policy, and first-class sites. Every `ADD COLUMN` is
        // guarded so a ladder resumed after a partial apply never trips
        // "duplicate column name".
        add_column_if_missing(
            conn,
            "blueprints",
            "version",
            "ALTER TABLE blueprints ADD COLUMN version INTEGER NOT NULL DEFAULT 1;",
        )?;
        add_column_if_missing(
            conn,
            "servers",
            "blueprint_version",
            "ALTER TABLE servers ADD COLUMN blueprint_version INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "schedules",
            "max_retries",
            "ALTER TABLE schedules ADD COLUMN max_retries INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "schedules",
            "retry_backoff_s",
            "ALTER TABLE schedules ADD COLUMN retry_backoff_s INTEGER NOT NULL DEFAULT 30;",
        )?;
        add_column_if_missing(
            conn,
            "schedules",
            "only_when_online",
            "ALTER TABLE schedules ADD COLUMN only_when_online INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "schedule_runs",
            "attempt",
            "ALTER TABLE schedule_runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;",
        )?;
        add_column_if_missing(
            conn,
            "schedule_runs",
            "finished_at",
            "ALTER TABLE schedule_runs ADD COLUMN finished_at TEXT;",
        )?;
        add_column_if_missing(
            conn,
            "websites",
            "updated_at",
            "ALTER TABLE websites ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';",
        )?;
        add_column_if_missing(
            conn,
            "websites",
            "upstream",
            "ALTER TABLE websites ADD COLUMN upstream TEXT NOT NULL DEFAULT '';",
        )?;
        add_column_if_missing(
            conn,
            "websites",
            "force_https",
            "ALTER TABLE websites ADD COLUMN force_https INTEGER NOT NULL DEFAULT 0;",
        )?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS server_metrics (
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                ts INTEGER NOT NULL,
                cpu_percent REAL NOT NULL DEFAULT 0,
                memory_bytes INTEGER NOT NULL DEFAULT 0,
                disk_bytes INTEGER NOT NULL DEFAULT 0,
                rx_bytes INTEGER NOT NULL DEFAULT 0,
                tx_bytes INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (server_id, ts)
            );
            CREATE INDEX IF NOT EXISTS idx_server_metrics_ts ON server_metrics(ts);

            CREATE TABLE IF NOT EXISTS blueprint_revisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                blueprint_id INTEGER NOT NULL REFERENCES blueprints(id) ON DELETE CASCADE,
                version INTEGER NOT NULL,
                snapshot TEXT NOT NULL,
                digest TEXT NOT NULL,
                author TEXT NOT NULL DEFAULT '',
                note TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                UNIQUE(blueprint_id, version)
            );

            CREATE TABLE IF NOT EXISTS webhooks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                url TEXT NOT NULL,
                secret TEXT NOT NULL DEFAULT '',
                events TEXT NOT NULL DEFAULT '["*"]',
                server_id INTEGER REFERENCES servers(id) ON DELETE CASCADE,
                enabled INTEGER NOT NULL DEFAULT 1,
                failure_count INTEGER NOT NULL DEFAULT 0,
                last_status TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS webhook_deliveries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                webhook_id INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
                event TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '{}',
                attempt INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'pending',
                response_code INTEGER,
                error TEXT NOT NULL DEFAULT '',
                next_attempt_at INTEGER,
                created_at TEXT NOT NULL,
                delivered_at TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_pending
                ON webhook_deliveries(status, next_attempt_at);

            DELETE FROM websites WHERE id NOT IN (SELECT MIN(id) FROM websites GROUP BY domain);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_websites_domain ON websites(domain);

            PRAGMA user_version = 7;
            "#,
        )?;
        apply_api_key_capabilities(conn)?;
    }
    if version < 8 {
        // Node agents serve HTTPS with a self-signed certificate; the panel pins
        // the fingerprint captured at enrollment instead of trusting the WebPKI.
        add_column_if_missing(
            conn,
            "nodes",
            "tls_fingerprint",
            "ALTER TABLE nodes ADD COLUMN tls_fingerprint TEXT NOT NULL DEFAULT '';",
        )?;
        conn.execute_batch("PRAGMA user_version = 8;")?;
    }
    if version < 9 {
        // Backup retention controls (lock + ignore list) and per-allocation
        // metadata, plus the indexed columns the per-actor activity feed
        // filters on. `audit_logs` already stores the actor; the added
        // `server_id` lets a server-scoped feed avoid scanning `target`.
        add_column_if_missing(
            conn,
            "backups",
            "is_locked",
            "ALTER TABLE backups ADD COLUMN is_locked INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "backups",
            "ignored_files",
            "ALTER TABLE backups ADD COLUMN ignored_files TEXT NOT NULL DEFAULT '';",
        )?;
        add_column_if_missing(
            conn,
            "allocations",
            "notes",
            "ALTER TABLE allocations ADD COLUMN notes TEXT NOT NULL DEFAULT '';",
        )?;
        add_column_if_missing(
            conn,
            "allocations",
            "is_primary",
            "ALTER TABLE allocations ADD COLUMN is_primary INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "audit_logs",
            "server_id",
            "ALTER TABLE audit_logs ADD COLUMN server_id INTEGER;",
        )?;
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_audit_user ON audit_logs(user_id, id DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_server ON audit_logs(server_id, id DESC);
            PRAGMA user_version = 9;
            "#,
        )?;
        backfill_primary_allocations(conn)?;
    }
    if version < 10 {
        // Crash policy (G8): per-server crash-vs-clean-exit classification and
        // a bounded restart budget. `crash_detect_clean_exit` mirrors the
        // detect-clean-exit-as-crash toggle; `crash_restart_budget` caps the
        // auto-restarts one crash burst may consume; `crash_restarts` /
        // `crash_window_start` track the live burst; `crash_reason` records
        // the last classification so the console UI can surface it.
        add_column_if_missing(
            conn,
            "servers",
            "crash_detect_clean_exit",
            "ALTER TABLE servers ADD COLUMN crash_detect_clean_exit INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "servers",
            "crash_restart_budget",
            "ALTER TABLE servers ADD COLUMN crash_restart_budget INTEGER NOT NULL DEFAULT 5;",
        )?;
        add_column_if_missing(
            conn,
            "servers",
            "crash_restarts",
            "ALTER TABLE servers ADD COLUMN crash_restarts INTEGER NOT NULL DEFAULT 0;",
        )?;
        add_column_if_missing(
            conn,
            "servers",
            "crash_window_start",
            "ALTER TABLE servers ADD COLUMN crash_window_start TEXT NOT NULL DEFAULT '';",
        )?;
        add_column_if_missing(
            conn,
            "servers",
            "crash_reason",
            "ALTER TABLE servers ADD COLUMN crash_reason TEXT NOT NULL DEFAULT '';",
        )?;
        conn.execute_batch("PRAGMA user_version = 10;")?;
    }
    if version < 11 {
        // Per-user server listing/counting (`list_servers(Some(user_id))`,
        // quota checks, access lookups) filters on `user_id` and live rows
        // only; the partial index covers exactly that predicate. Admin
        // listings with `include_deleted=true` skip the `deleted=0` clause
        // and fall back to a scan, which is the cold path.
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_servers_user
                ON servers(user_id) WHERE deleted=0;
            PRAGMA user_version = 11;
            "#,
        )?;
    }
    if version < 12 {
        // FK child-table lookups (`backups`, `databases`, `schedules`,
        // `schedule_tasks`, `schedule_runs`, `allocations`, `websites`,
        // `webhook_deliveries`) plus the session owner/expiry and api-key
        // owner filters ran on unindexed columns; every delete-cascade and
        // list query now has a covering index. Guarded so a resumed ladder
        // never trips "index already exists".
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_backups_server ON backups(server_id);
            CREATE INDEX IF NOT EXISTS idx_databases_server ON databases(server_id);
            CREATE INDEX IF NOT EXISTS idx_schedules_server ON schedules(server_id);
            CREATE INDEX IF NOT EXISTS idx_schedule_tasks_schedule ON schedule_tasks(schedule_id);
            CREATE INDEX IF NOT EXISTS idx_schedule_runs_schedule ON schedule_runs(schedule_id);
            CREATE INDEX IF NOT EXISTS idx_allocations_server ON allocations(server_id);
            CREATE INDEX IF NOT EXISTS idx_websites_server ON websites(server_id);
            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_webhook ON webhook_deliveries(webhook_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);
            CREATE INDEX IF NOT EXISTS idx_api_keys_user ON api_keys(user_id);
            PRAGMA user_version = 12;
            "#,
        )?;
    }
    if version < 13 {
        // Webhook hardening: https-only by default with a per-webhook opt-in
        // for plain http. Existing http URLs are grandfathered (allow_http=1)
        // so they keep delivering; new http targets require the flag. The
        // (status, delivered_at) index serves both the bounded retention
        // prune of terminal deliveries and the pending-count cap in `emit`.
        add_column_if_missing(
            conn,
            "webhooks",
            "allow_http",
            "ALTER TABLE webhooks ADD COLUMN allow_http INTEGER NOT NULL DEFAULT 0;",
        )?;
        conn.execute_batch(
            r#"
            UPDATE webhooks SET allow_http = 1 WHERE substr(lower(url), 1, 5) = 'http:';
            CREATE INDEX IF NOT EXISTS idx_webhook_deliveries_status_delivered
                ON webhook_deliveries(status, delivered_at);
            PRAGMA user_version = 13;
            "#,
        )?;
    }
    if version < 14 {
        // The v12 FK-index pass covered most child-table lookups but missed
        // three columns the service layer filters on: webhook scoping by
        // server, transfer source lookups, and blueprint-versioned server
        // queries. Guarded so a resumed ladder never trips "index already
        // exists".
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_webhooks_server ON webhooks(server_id);
            CREATE INDEX IF NOT EXISTS idx_server_transfers_server ON server_transfers(server_id);
            CREATE INDEX IF NOT EXISTS idx_servers_blueprint ON servers(blueprint_id);
            PRAGMA user_version = 14;
            "#,
        )?;
    }
    if version < 15 {
        // Scheduler correctness: per-schedule timezone, process-tagged
        // in-flight runs (stale recovery must not kill live attempts),
        // a retry resume point (re-run only from the failing task), and an
        // index for pruning terminal run history. Every step is guarded so
        // a ladder resumed after a partial apply never trips.
        add_column_if_missing(
            conn,
            "schedules",
            "schedule_tz",
            "ALTER TABLE schedules ADD COLUMN schedule_tz TEXT NOT NULL DEFAULT 'UTC';",
        )?;
        add_column_if_missing(
            conn,
            "schedule_runs",
            "in_flight_tag",
            "ALTER TABLE schedule_runs ADD COLUMN in_flight_tag TEXT;",
        )?;
        add_column_if_missing(
            conn,
            "schedule_runs",
            "task_index",
            "ALTER TABLE schedule_runs ADD COLUMN task_index INTEGER NOT NULL DEFAULT 0;",
        )?;
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_schedule_runs_terminal
                ON schedule_runs(status, finished_at);
            PRAGMA user_version = 15;
            "#,
        )?;
    }
    if version < 16 {
        // Operator-seeded expected certificate fingerprint (iteration 3):
        // the enrollment dead-end for plaintext/proxy-fronted agents is the
        // 400 on a missing pin, and such agents cannot self-present the
        // endpoint certificate the panel dials. This column lets an operator
        // declare the fingerprint the first enrollment must present (and
        // later re-enrollments must match), so the panel pins the endpoint
        // certificate instead of the agent's own material. NULL = not seeded
        // (plain TOFU at first enrollment, as before); a value is validated
        // strict 64-hex by the API layer before it is written. Kept (not
        // cleared) at enrollment so a rotated node can re-enroll against the
        // same operator-declared identity. Guarded ADD COLUMN so a resumed
        // ladder never trips "duplicate column name".
        add_column_if_missing(
            conn,
            "nodes",
            "expected_fingerprint",
            "ALTER TABLE nodes ADD COLUMN expected_fingerprint TEXT;",
        )?;
        conn.execute_batch("PRAGMA user_version = 16;")?;
    }
    if version < 17 {
        // Flow Gates (iteration 4, GapFeatures): a schedule task may carry a
        // condition that must hold before the task runs — `exit` gates on an
        // earlier task's recorded exit code, `signal` waits for a matching
        // webhook event for the server. `schedule_tasks.condition` holds the
        // canonical gate JSON (NULL = unconditional, i.e. the pre-gate
        // behavior). `schedule_runs.task_exits` persists the per-task exit
        // codes of the current attempt chain as JSON (`{"0":0,...}`) so a
        // later gate in the same chain — and a retry resumed from a task
        // index — can read what earlier tasks actually exited with. Guarded
        // ADD COLUMN so a resumed ladder never trips "duplicate column name".
        add_column_if_missing(
            conn,
            "schedule_tasks",
            "condition",
            "ALTER TABLE schedule_tasks ADD COLUMN condition TEXT;",
        )?;
        add_column_if_missing(
            conn,
            "schedule_runs",
            "task_exits",
            "ALTER TABLE schedule_runs ADD COLUMN task_exits TEXT NOT NULL DEFAULT '{}';",
        )?;
        conn.execute_batch("PRAGMA user_version = 17;")?;
    }
    if version < 18 {
        // Iteration-5 fold-in: the squads/squad_members/squad_servers tables
        // (models::ensure_squads_tables) and node_reservations
        // (nodes::ensure_reservations_table) previously lived OUTSIDE the
        // ladder as lazy ensure-on-use sites, so fresh vs upgraded databases
        // diverged in sqlite_master. The ladder now owns the DDL; the lazy
        // sites stay as cheap no-op fallbacks for upgraded databases.
        // Guarded CREATE TABLE IF NOT EXISTS: DDL is byte-identical to the
        // lazy sites, so a v17 DB whose tables were already created lazily
        // migrates as a no-op.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS squads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS squad_members (
                squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                PRIMARY KEY (squad_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS squad_servers (
                squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                PRIMARY KEY (squad_id, server_id)
            );
            CREATE TABLE IF NOT EXISTS node_reservations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                memory_mb INTEGER NOT NULL,
                disk_mb INTEGER NOT NULL,
                reserved_until TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            PRAGMA user_version = 18;
            "#,
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Elect one primary allocation per server for pre-v9 rows.
///
/// Before v9 a server's "main" port was implicitly `servers.port`. The lowest
/// allocation matching it becomes primary; servers whose `port` no longer has a
/// matching allocation fall back to their lowest-numbered one, so every server
/// with at least one allocation ends up with exactly one primary.
fn backfill_primary_allocations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        UPDATE allocations SET is_primary = 0;
        UPDATE allocations SET is_primary = 1 WHERE id IN (
            SELECT MIN(a.id) FROM allocations a
            JOIN servers s ON s.id = a.server_id
            WHERE s.port IS NOT NULL AND a.port = s.port
            GROUP BY a.server_id
        );
        UPDATE allocations SET is_primary = 1 WHERE id IN (
            SELECT MIN(a.id) FROM allocations a
            WHERE NOT EXISTS (
                SELECT 1 FROM allocations p
                WHERE p.server_id = a.server_id AND p.is_primary = 1
            )
            GROUP BY a.server_id
        );
        "#,
    )?;
    Ok(())
}

/// Backfill the v7 `capabilities` column for legacy API-key rows.
///
/// The column is added with an empty-set default so a restricted legacy key
/// can never be treated as full access. Only rows whose legacy `scopes`
/// explicitly grant everything (`full`/`*`) are widened to the wildcard.
fn apply_api_key_capabilities(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "api_keys",
        "capabilities",
        "ALTER TABLE api_keys ADD COLUMN capabilities TEXT NOT NULL DEFAULT '[]';",
    )?;
    add_column_if_missing(
        conn,
        "api_keys",
        "server_ids",
        "ALTER TABLE api_keys ADD COLUMN server_ids TEXT NOT NULL DEFAULT '[]';",
    )?;
    add_column_if_missing(
        conn,
        "api_keys",
        "expires_at",
        "ALTER TABLE api_keys ADD COLUMN expires_at TEXT;",
    )?;
    add_column_if_missing(
        conn,
        "api_keys",
        "revoked",
        "ALTER TABLE api_keys ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0;",
    )?;
    conn.execute_batch(
        r#"
        UPDATE api_keys SET capabilities='["*"]' WHERE scopes IN ('full','*');
        "#,
    )?;
    Ok(())
}

/// Rewrite pre-v6 permission rows into canonical capability names and infer a
/// role preset where the stored set matches one exactly.
///
/// A row whose `permissions` cell is corrupt JSON is skipped and logged
/// instead of being treated as an empty set: `unwrap_or_default` would
/// otherwise rewrite the corrupt bytes to `[]` and destroy the row for good.
/// The UPDATE is prepared once and re-executed per row; the token→capability
/// expansion is Rust-side (`expand_legacy`), so a pure set-based UPDATE would
/// have to re-implement it in SQL.
fn normalize_subuser_permissions(conn: &Connection) -> Result<()> {
    use crate::capability::{expand_legacy, Role};
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id,permissions FROM subusers")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<std::result::Result<_, _>>()?
    };
    let mut update = conn.prepare("UPDATE subusers SET permissions=?1, role=?2 WHERE id=?3")?;
    for (id, raw) in rows {
        let tokens: Vec<String> = match serde_json::from_str(&raw) {
            Ok(tokens) => tokens,
            Err(e) => {
                tracing::warn!(
                    "subuser {id}: skipping corrupt permissions row ({e}), leaving bytes untouched"
                );
                continue;
            }
        };
        let caps = expand_legacy(&tokens);
        let role = Role::ALL
            .into_iter()
            .find(|r| r.capabilities() == caps)
            .unwrap_or(Role::Custom);
        let names: Vec<&str> = caps.iter().map(|c| c.as_str()).collect();
        update.execute(rusqlite::params![
            serde_json::to_string(&names)?,
            role.as_str(),
            id
        ])?;
    }
    Ok(())
}

/// Run integrity check used by /system/health.
///
/// `quick_check` instead of `integrity_check`: this runs on the shared live
/// connection and is polled by /system/health, and the full check is O(DB) —
/// quick_check skips the expensive cross-page consistency passes while still
/// catching the corruption classes that matter for a health probe.
pub fn integrity_check(db: &Db) -> Result<String> {
    let conn = db.get()?;
    let out: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pre-v7 api_keys rows: the legacy `scopes` column, no `capabilities`.
    fn legacy_key_fixture(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                username TEXT NOT NULL UNIQUE,
                email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                avatar TEXT NOT NULL DEFAULT '',
                language TEXT NOT NULL DEFAULT 'en',
                theme TEXT NOT NULL DEFAULT 'dark',
                root_admin INTEGER NOT NULL DEFAULT 0,
                active INTEGER NOT NULL DEFAULT 1,
                twofa_secret TEXT,
                about TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE api_keys (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                token TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL DEFAULT 'default',
                created_at TEXT NOT NULL,
                last_used TEXT,
                scopes TEXT NOT NULL DEFAULT 'full'
            );
            INSERT INTO users (username,email,password_hash,created_at,updated_at)
                VALUES ('u','u@x','h','2026-01-01','2026-01-01');
            INSERT INTO api_keys (user_id,token,name,created_at,scopes) VALUES
                (1,'full-key','full key','2026-01-01','full'),
                (1,'star-key','star key','2026-01-01','*'),
                (1,'restricted-key','restricted key','2026-01-01','restricted');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn legacy_restricted_key_is_not_wildcard() {
        let conn = Connection::open_in_memory().unwrap();
        legacy_key_fixture(&conn);

        apply_api_key_capabilities(&conn).unwrap();

        let caps = |token: &str| -> String {
            conn.query_row(
                "SELECT capabilities FROM api_keys WHERE token=?1",
                [token],
                |r| r.get(0),
            )
            .unwrap()
        };
        // Explicit full / wildcard scopes keep full access.
        assert_eq!(caps("full-key"), "[\"*\"]");
        assert_eq!(caps("star-key"), "[\"*\"]");
        // Any other legacy scope degrades to deny-by-default, never wildcard.
        assert_eq!(caps("restricted-key"), "[]");
        assert_ne!(caps("restricted-key"), "[\"*\"]");
    }

    // A v6 fixture with a minimal `subusers` table: migrate runs v6 (adding the
    // `role` column) then fails in v7 (missing `blueprints`). The whole ladder
    // is one transaction, so nothing may leak past the failure.
    #[test]
    fn failed_migration_rolls_back_the_whole_ladder() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE subusers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                server_id INTEGER NOT NULL,
                permissions TEXT NOT NULL DEFAULT '[]'
            );
            INSERT INTO subusers (user_id, server_id, permissions) VALUES (1, 1, '["files"]');
            PRAGMA user_version = 5;
            "#,
        )
        .unwrap();

        // v7 ALTERs `blueprints`; the fixture deliberately omits that table.
        let err = migrate(&conn).unwrap_err();
        assert!(
            err.to_string().contains("blueprints"),
            "unexpected error: {err}"
        );

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            version, 5,
            "user_version must roll back with the failed migration"
        );

        let role_column_gone: bool = conn
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('subusers') WHERE name='role')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(role_column_gone, "v6 `role` column must be rolled back");

        // v6's data backfill must roll back with the schema: the fixture row's
        // `permissions` were rewritten by normalize_subuser_permissions inside
        // the transaction and must return to their pre-migration bytes.
        let perms: String = conn
            .query_row("SELECT permissions FROM subusers WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(perms, "[\"files\"]", "v6 data backfill must roll back");
    }

    // A fresh install must never contain the Pterodactyl-era identifiers:
    // the bootstrap emits `blueprints`/`blueprint_id`/`runtime_hint` under
    // their final names, so no `egss` table and no legacy columns exist —
    // not even transiently before the v5 rename runs.
    #[test]
    fn fresh_bootstrap_never_creates_egss() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "fresh DB must land on the latest version");

        let table_count = |name: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            table_count("egss"),
            0,
            "fresh bootstrap must never create egss"
        );
        assert_eq!(
            table_count("blueprints"),
            1,
            "fresh bootstrap must create blueprints"
        );

        let columns = |table: &str| -> Vec<String> {
            let mut stmt = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            let mapped = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            mapped
        };
        let has = |cols: &[String], name: &str| cols.iter().any(|c| c == name);

        let servers = columns("servers");
        assert!(
            has(&servers, "blueprint_id"),
            "servers must use blueprint_id"
        );
        assert!(
            has(&servers, "runtime_hint"),
            "servers must use runtime_hint"
        );
        assert!(!has(&servers, "egg_id"), "servers must not keep egg_id");
        assert!(
            !has(&servers, "docker_image"),
            "servers must not keep docker_image"
        );
        assert!(!has(&servers, "threads"), "servers must not keep threads");
        assert!(
            !has(&servers, "ignore_oom"),
            "servers must not keep ignore_oom"
        );

        let variables = columns("server_variables");
        assert!(
            has(&variables, "blueprint_id"),
            "server_variables must use blueprint_id"
        );
        assert!(
            !has(&variables, "egg_id"),
            "server_variables must not keep egg_id"
        );

        let blueprints = columns("blueprints");
        assert!(
            has(&blueprints, "runtime_hint"),
            "blueprints must use runtime_hint"
        );
        assert!(
            !has(&blueprints, "docker_image"),
            "blueprints must not keep docker_image"
        );

        // The servers.blueprint_id FK must resolve: a server row referencing
        // a blueprint inserts cleanly with foreign keys enforced.
        conn.execute_batch(
            r#"
            INSERT INTO users (username,email,password_hash,created_at,updated_at)
                VALUES ('u','u@x','h','t','t');
            INSERT INTO blueprints (uuid,name,created_at,updated_at) VALUES ('b','b','t','t');
            INSERT INTO servers (uuid,name,user_id,blueprint_id,created_at,updated_at)
                VALUES ('s','s',1,1,'t','t');
            "#,
        )
        .unwrap();
        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "blueprint FK must resolve on a fresh DB");
    }

    // A pre-v5 database (v4-era fixture with `egss`/`egg_id`/`docker_image`
    // and data) must upgrade to v14: the v5 existence-guarded renames move
    // the rows and columns to their final names, and every later step runs
    // on the renamed schema.
    #[test]
    fn pre_v5_db_migrates_to_blueprint_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"

            CREATE TABLE users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                username TEXT NOT NULL UNIQUE,
                email TEXT NOT NULL UNIQUE,
                password_hash TEXT NOT NULL,
                avatar TEXT NOT NULL DEFAULT '',
                language TEXT NOT NULL DEFAULT 'en',
                theme TEXT NOT NULL DEFAULT 'dark',
                root_admin INTEGER NOT NULL DEFAULT 0,
                active INTEGER NOT NULL DEFAULT 1,
                twofa_secret TEXT,
                about TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE egss (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                author TEXT NOT NULL DEFAULT '',
                category TEXT NOT NULL DEFAULT 'generic',
                docker_image TEXT NOT NULL DEFAULT 'alpine',
                startup TEXT NOT NULL DEFAULT '',
                default_config TEXT,
                config_files TEXT,
                install_script TEXT,
                variables TEXT NOT NULL DEFAULT '[]',
                stop_command TEXT NOT NULL DEFAULT 'stop',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                egg_id INTEGER NOT NULL REFERENCES egss(id) ON DELETE RESTRICT,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'offline',
                docker_image TEXT NOT NULL DEFAULT '',
                startup TEXT NOT NULL DEFAULT '',
                node TEXT NOT NULL DEFAULT 'local',
                port INTEGER,
                memory_mb INTEGER NOT NULL DEFAULT 1024,
                disk_mb INTEGER NOT NULL DEFAULT 8192,
                cpu_percent INTEGER NOT NULL DEFAULT 100,
                threads TEXT NOT NULL DEFAULT '',
                suspended INTEGER NOT NULL DEFAULT 0,
                auto_restart INTEGER NOT NULL DEFAULT 0,
                restart_count INTEGER NOT NULL DEFAULT 0,
                ignore_oom INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE server_variables (
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                egg_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (server_id, key)
            );
            CREATE TABLE subusers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                user_id INTEGER NOT NULL,
                permissions TEXT NOT NULL DEFAULT '[]',
                UNIQUE(server_id, user_id)
            );
            CREATE TABLE api_keys (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                token TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL DEFAULT 'default',
                created_at TEXT NOT NULL,
                last_used TEXT,
                scopes TEXT NOT NULL DEFAULT 'full'
            );
            CREATE TABLE allocations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                port INTEGER NOT NULL,
                assigned_at TEXT NOT NULL
            );
            CREATE TABLE audit_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER,
                action TEXT NOT NULL,
                target TEXT NOT NULL DEFAULT '',
                ip TEXT NOT NULL DEFAULT '',
                details TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE TABLE schedules (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                cron_expr TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_run_at TEXT,
                next_run_at TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE schedule_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id INTEGER NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
                action TEXT NOT NULL,
                payload TEXT NOT NULL DEFAULT '',
                sequence INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE schedule_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id INTEGER NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
                triggered_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'running',
                log TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE websites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                domain TEXT NOT NULL,
                root_dir TEXT NOT NULL DEFAULT '/',
                port INTEGER,
                proxy_type TEXT NOT NULL DEFAULT 'static',
                ssl INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL
            );
            CREATE TABLE databases (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                db_type TEXT NOT NULL DEFAULT 'mysql',
                host TEXT NOT NULL DEFAULT '127.0.0.1',
                port INTEGER NOT NULL DEFAULT 3306,
                db_name TEXT NOT NULL,
                username TEXT NOT NULL,
                password TEXT NOT NULL,
                max_conns INTEGER NOT NULL DEFAULT 10,
                created_at TEXT NOT NULL
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                token TEXT NOT NULL UNIQUE,
                user_id INTEGER NOT NULL,
                user_agent TEXT NOT NULL DEFAULT '',
                ip TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0,
                remember INTEGER NOT NULL DEFAULT 0,
                last_seen TEXT
            );
            CREATE TABLE nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL UNIQUE,
                public_url TEXT NOT NULL,
                secret TEXT NOT NULL,
                enrollment_token TEXT UNIQUE,
                enrolled INTEGER NOT NULL DEFAULT 0,
                enabled INTEGER NOT NULL DEFAULT 1,
                maintenance INTEGER NOT NULL DEFAULT 0,
                schedulable INTEGER NOT NULL DEFAULT 1,
                location TEXT NOT NULL DEFAULT '',
                tags TEXT NOT NULL DEFAULT '[]',
                memory_limit_mb INTEGER NOT NULL DEFAULT 0,
                disk_limit_mb INTEGER NOT NULL DEFAULT 0,
                cpu_limit_percent INTEGER NOT NULL DEFAULT 0,
                memory_overallocate INTEGER NOT NULL DEFAULT 0,
                disk_overallocate INTEGER NOT NULL DEFAULT 0,
                daemon_version TEXT NOT NULL DEFAULT '',
                hostname TEXT NOT NULL DEFAULT '',
                os TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL DEFAULT '',
                capacity_json TEXT NOT NULL DEFAULT '{}',
                last_heartbeat TEXT,
                last_error TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE server_transfers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                source_node TEXT NOT NULL,
                target_node TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                bytes_total INTEGER NOT NULL DEFAULT 0,
                bytes_done INTEGER NOT NULL DEFAULT 0,
                error TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE backups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                uuid TEXT NOT NULL UNIQUE,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                checksum TEXT NOT NULL DEFAULT '',
                format TEXT NOT NULL DEFAULT 'zip',
                created_at TEXT NOT NULL
            );

            INSERT INTO users (id,username,email,password_hash,created_at,updated_at)
                VALUES (1,'u','u@x','h','t','t');
            INSERT INTO egss (id,uuid,name,docker_image,created_at,updated_at)
                VALUES (1,'egg-1','nginx','alpine:3.20','t','t');
            INSERT INTO servers (id,uuid,name,user_id,egg_id,docker_image,threads,ignore_oom,created_at,updated_at)
                VALUES (1,'srv-1','web',1,1,'alpine:3.20','',0,'t','t');
            INSERT INTO server_variables (server_id,egg_id,key,value) VALUES (1,1,'PORT','8080');
            PRAGMA user_version = 4;
            "#,
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "pre-v5 DB must land on the latest version");

        let egss_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='egss'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(egss_left, 0, "egss must be renamed away during v5");

        let (bp_name, bp_hint): (String, String) = conn
            .query_row(
                "SELECT name, runtime_hint FROM blueprints WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(bp_name, "nginx", "egss row must carry into blueprints");
        assert_eq!(
            bp_hint, "alpine:3.20",
            "docker_image must rename to runtime_hint"
        );

        let (bp_id, hint): (i64, String) = conn
            .query_row(
                "SELECT blueprint_id, runtime_hint FROM servers WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(bp_id, 1, "servers.egg_id must rename to blueprint_id");
        assert_eq!(
            hint, "alpine:3.20",
            "servers.docker_image must rename to runtime_hint"
        );

        let (var_bp, var_value): (i64, String) = conn
            .query_row(
                "SELECT blueprint_id, value FROM server_variables WHERE server_id=1 AND key='PORT'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            var_bp, 1,
            "server_variables.egg_id must rename to blueprint_id"
        );
        assert_eq!(var_value, "8080", "variable value must survive the rename");

        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let fk_violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(fk_violations, 0, "renamed FKs must be consistent");
    }

    // Full fresh upgrade v0 -> v8, plus re-runs: a second `migrate` at the
    // current version is a no-op, and even a forced downgrade back to v6
    // re-applies the ladder without duplicate-column errors while re-running
    // the deny-by-default API-key backfill against legacy rows.
    #[test]
    fn rerun_and_downgrade_rerun_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "fresh DB must land on the latest version");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");

        // No-op rerun at the current version: same version, single column.
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
        let capabilities_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('api_keys') WHERE name='capabilities'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            capabilities_columns, 1,
            "capabilities column must exist exactly once"
        );

        // Simulate a legacy deployment stuck at v6: downgrade the version
        // stamp and insert pre-v7 api_keys rows (the v7 columns already
        // exist on the table).
        conn.execute_batch("PRAGMA user_version = 6;").unwrap();
        conn.execute_batch(
            r#"
            INSERT INTO users (username,email,password_hash,created_at,updated_at)
                VALUES ('legacy','legacy@x','h','2026-01-01','2026-01-01');
            INSERT INTO api_keys (user_id,token,name,created_at,scopes) VALUES
                (1,'full-key','full key','2026-01-01','full'),
                (1,'star-key','star key','2026-01-01','*'),
                (1,'restricted-key','restricted key','2026-01-01','restricted');
            "#,
        )
        .unwrap();

        // Re-running from v6 must skip the existing v7 columns (no duplicate
        // column errors), backfill the legacy rows deny-by-default, and land
        // back at v11 with an intact database.
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);

        let caps = |token: &str| -> String {
            conn.query_row(
                "SELECT capabilities FROM api_keys WHERE token=?1",
                [token],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(caps("full-key"), "[\"*\"]");
        assert_eq!(caps("star-key"), "[\"*\"]");
        assert_eq!(caps("restricted-key"), "[]");

        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    // Fresh upgrade scenario on a real temporary file: the full ladder must
    // land at v12 with WAL journaling, foreign keys enforced, a clean
    // integrity check, and 0600 permissions on the database file AND its
    // -wal/-shm sidecars.
    #[test]
    fn open_runs_full_upgrade_and_passes_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("panel.db");
        let db = open(db_path.to_str().unwrap()).unwrap();

        assert_eq!(integrity_check(&db).unwrap(), "ok");

        let conn = db.get().unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "fresh file must migrate to the latest version");
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign keys must stay enabled after migration");
        drop(conn);

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "database file must be private");
        for sidecar in ["panel.db-wal", "panel.db-shm"] {
            let mode = std::fs::metadata(dir.path().join(sidecar))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{sidecar} sidecar must be private");
        }
    }

    // The pool must hand out DIFFERENT connections concurrently: the whole
    // point of replacing the global mutex. Deterministic — not a timing
    // check — via SQLite's visibility rules: an uncommitted write on one
    // checked-out connection is invisible to a second one, while committed
    // data is visible to both. A single shared connection would fail the
    // first assertion (and a one-connection pool would deadlock the second
    // `get`).
    #[test]
    fn pooled_connections_are_distinct_and_share_commits() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("panel.db");
        let db = open(db_path.to_str().unwrap()).unwrap();

        let a = db.get().unwrap();
        a.execute_batch(
            "BEGIN; \
             INSERT INTO users(username,email,password_hash,created_at,updated_at) \
             VALUES('uncommitted','u@x','h','t','t');",
        )
        .unwrap();

        // A second checkout must be a different connection: it cannot see
        // connection A's uncommitted row.
        let b = db.get().unwrap();
        let hidden: i64 = b
            .query_row(
                "SELECT COUNT(*) FROM users WHERE username='uncommitted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hidden, 0, "concurrent pooled connections must be distinct");

        // A sees its own uncommitted write on the same connection.
        let own: i64 = a
            .query_row(
                "SELECT COUNT(*) FROM users WHERE username='uncommitted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(own, 1);
        a.execute_batch("COMMIT;").unwrap();

        // Once committed, the row is visible on every pooled connection.
        let visible: i64 = b
            .query_row(
                "SELECT COUNT(*) FROM users WHERE username='uncommitted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            visible, 1,
            "committed writes must be visible across the pool"
        );
    }

    // Rerunning the ladder from the very start (user_version = 0) against a
    // fully migrated database is a no-op that preserves every table.
    #[test]
    fn forced_full_rerun_is_a_noop() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        conn.execute_batch("PRAGMA user_version = 0;").unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // v1 tables (17) + v2 (3) + v7 (4) = 24, plus the four lazy tables
        // folded in at v18 (squads, squad_members, squad_servers,
        // node_reservations) = 28; a duplicate-column failure or a partial
        // apply would change this count or fail outright.
        assert_eq!(tables, 28);
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    // v11 adds a partial index on `servers(user_id)` for live rows only: it
    // must exist on a fresh DB, carry the `deleted=0` predicate, and serve
    // the per-user listing query the dashboard actually runs.
    #[test]
    fn v11_adds_partial_index_on_servers_user() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "fresh DB must land on the latest version");

        // `partial` lives on `pragma_index_list`, not `sqlite_master`.
        let (name, partial): (String, i64) = conn
            .query_row(
                "SELECT name, partial FROM pragma_index_list('servers') \
                 WHERE name='idx_servers_user'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type='index' AND name='idx_servers_user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "idx_servers_user");
        assert_eq!(partial, 1, "index must be partial (live rows only)");
        assert!(
            sql.contains("WHERE deleted=0"),
            "partial predicate must be deleted=0: {sql}"
        );

        // The hot per-user listing predicate must be served by the index.
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN \
                 SELECT * FROM servers WHERE user_id=1 AND deleted=0",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_servers_user"),
            "per-user query must use the index, got plan: {plan}"
        );
    }
    // v12 adds covering indexes for every FK child-table lookup plus the
    // session/api-key owner filters: they must all exist on a fresh DB and
    // the guarded ladder must re-apply cleanly from a lower version stamp.
    #[test]
    fn v12_adds_fk_and_owner_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'")
                .unwrap();
            let mapped = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            mapped
        };
        for want in [
            "idx_backups_server",
            "idx_databases_server",
            "idx_schedules_server",
            "idx_schedule_tasks_schedule",
            "idx_schedule_runs_schedule",
            "idx_allocations_server",
            "idx_websites_server",
            "idx_webhook_deliveries_webhook",
            "idx_sessions_user",
            "idx_sessions_expires",
            "idx_api_keys_user",
        ] {
            assert!(indexes.contains(&want.to_string()), "missing index {want}");
        }

        // Downgrade to v11 and re-run: guarded CREATE INDEX IF NOT EXISTS
        // must not trip "index already exists" or alter the count.
        conn.execute_batch("PRAGMA user_version = 11;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
    // v14 adds indexes for the three FK columns the v12 pass missed
    // (webhook server scoping, transfer source, blueprint-versioned server
    // queries): they must exist on a fresh DB and the guarded ladder must
    // re-apply cleanly from a lower version stamp.
    #[test]
    fn v14_adds_fk_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'")
                .unwrap();
            let mapped = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            mapped
        };
        for want in [
            "idx_webhooks_server",
            "idx_server_transfers_server",
            "idx_servers_blueprint",
        ] {
            assert!(indexes.contains(&want.to_string()), "missing index {want}");
        }

        // Downgrade to v13 and re-run: guarded CREATE INDEX IF NOT EXISTS
        // must not trip "index already exists".
        conn.execute_batch("PRAGMA user_version = 13;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
    // v15 adds the scheduler-correctness columns (per-schedule timezone,
    // in-flight run tag, retry resume point) plus the terminal-run prune
    // index: all must exist on a fresh DB and the guarded ladder must
    // re-apply cleanly from a lower version stamp.
    #[test]
    fn v15_adds_scheduler_columns_and_prune_index() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let columns = |table: &str| -> Vec<String> {
            let mut stmt = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            let mapped = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            mapped
        };
        let has = |cols: &[String], name: &str| cols.iter().any(|c| c == name);

        let sched = columns("schedules");
        assert!(
            has(&sched, "schedule_tz"),
            "schedules must carry schedule_tz"
        );
        let runs = columns("schedule_runs");
        assert!(
            has(&runs, "in_flight_tag"),
            "schedule_runs must carry in_flight_tag"
        );
        assert!(
            has(&runs, "task_index"),
            "schedule_runs must carry task_index"
        );

        let indexes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'")
                .unwrap();
            let mapped = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            mapped
        };
        assert!(
            indexes.contains(&"idx_schedule_runs_terminal".to_string()),
            "missing terminal-run prune index"
        );

        // Downgrade to v14 and re-run: guarded ADD COLUMN / CREATE INDEX
        // must not trip "duplicate column name" / "index already exists".
        conn.execute_batch("PRAGMA user_version = 14;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
    // v16 adds the operator-seeded expected_fingerprint column to `nodes`
    // (the plaintext/proxy-fronted enrollment path): it must exist on a
    // fresh DB, stay nullable, and the guarded ladder must re-apply cleanly
    // from a v15 stamp.
    #[test]
    fn v16_adds_expected_fingerprint_column() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let (has, notnull): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(\"notnull\"),0) \
                 FROM pragma_table_info('nodes') WHERE name='expected_fingerprint'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(has, 1, "nodes must carry expected_fingerprint");
        assert_eq!(notnull, 0, "expected_fingerprint must stay nullable");

        // Downgrade to v15 and re-run: guarded ADD COLUMN must not trip
        // "duplicate column name".
        conn.execute_batch("PRAGMA user_version = 15;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
    // v17 adds the Flow Gate columns: `schedule_tasks.condition` (nullable
    // gate JSON) and `schedule_runs.task_exits` (per-task exit-code map,
    // defaulting to an empty chain). Both must exist on a fresh DB and the
    // guarded ladder must re-apply cleanly from a v16 stamp.
    #[test]
    fn v17_adds_flow_gate_columns() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let (has_cond, cond_notnull, has_exits): (i64, i64, i64) = conn
            .query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM pragma_table_info('schedule_tasks') WHERE name='condition'), \
                    (SELECT COALESCE(SUM(\"notnull\"),0) FROM pragma_table_info('schedule_tasks') WHERE name='condition'), \
                    (SELECT COUNT(*) FROM pragma_table_info('schedule_runs') WHERE name='task_exits')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(has_cond, 1, "schedule_tasks must carry condition");
        assert_eq!(
            cond_notnull, 0,
            "condition must stay nullable (NULL = no gate)"
        );
        assert_eq!(has_exits, 1, "schedule_runs must carry task_exits");
        let exits_default: String = conn
            .query_row(
                "SELECT dflt_value FROM pragma_table_info('schedule_runs') \
                 WHERE name='task_exits'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            exits_default, "'{}'",
            "task_exits must default to an empty map"
        );

        // Downgrade to v16 and re-run: guarded ADD COLUMN must not trip
        // "duplicate column name".
        conn.execute_batch("PRAGMA user_version = 16;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18);
    }
    // v18 folds the lazily-created tables into the ladder: squads,
    // squad_members, squad_servers (models::ensure_squads_tables) and
    // node_reservations (nodes::ensure_reservations_table) previously
    // appeared in sqlite_master only on first use, so fresh vs upgraded
    // databases diverged. A fresh install must land at v18 with all four
    // tables present, a downgrade-to-17 re-run must re-apply cleanly, and
    // an upgrade from a v17 DB whose tables were already created lazily
    // must no-op (guarded CREATE TABLE IF NOT EXISTS).
    #[test]
    fn v18_folds_lazy_tables_into_the_ladder() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "fresh DB must land on the latest version");

        let table_count = |name: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
        };
        for want in [
            "squads",
            "squad_members",
            "squad_servers",
            "node_reservations",
        ] {
            assert_eq!(
                table_count(want),
                1,
                "fresh DB must contain {want} from the v18 ladder step"
            );
        }

        // Downgrade to v17 and re-run: guarded CREATE TABLE IF NOT EXISTS
        // must not trip "table already exists" and must land back at v18.
        conn.execute_batch("PRAGMA user_version = 17;").unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "downgrade-to-17 re-run must land at v18");

        // Upgrade from a real iteration-4/5 v17 DB, where the tables were
        // created lazily outside the ladder. Drop the ladder-created copies
        // and re-create them the lazy way: the v18 step must no-op on them
        // (IF NOT EXISTS), not fail or duplicate.
        conn.execute_batch("PRAGMA user_version = 17;").unwrap();
        conn.execute_batch(
            "DROP TABLE node_reservations; DROP TABLE squad_servers; \
             DROP TABLE squad_members; DROP TABLE squads;",
        )
        .unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS squads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS squad_members (
                squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                PRIMARY KEY (squad_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS squad_servers (
                squad_id INTEGER NOT NULL REFERENCES squads(id) ON DELETE CASCADE,
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                PRIMARY KEY (squad_id, server_id)
            );
            CREATE TABLE IF NOT EXISTS node_reservations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
                memory_mb INTEGER NOT NULL,
                disk_mb INTEGER NOT NULL,
                reserved_until TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 18, "v17 DB with lazy tables must land at v18");
        for want in [
            "squads",
            "squad_members",
            "squad_servers",
            "node_reservations",
        ] {
            assert_eq!(
                table_count(want),
                1,
                "lazy-created {want} must survive the v18 no-op"
            );
        }
    }
    fn test_pool() -> Db {
        Db(r2d2::Pool::builder()
            .max_size(2)
            .build(SqliteConnectionManager::memory())
            .unwrap())
    }

    #[tokio::test]
    async fn call_runs_on_the_blocking_pool_and_persists_work() {
        let pool = test_pool();
        pool.call(|conn| {
            conn.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (42);")?;
            Ok(())
        })
        .await
        .unwrap();
        // The closure ran on a blocking-pool thread, never a Tokio worker.
        let thread_name = pool
            .call(|_conn| Ok(std::thread::current().name().map(str::to_owned)))
            .await
            .unwrap();
        assert_ne!(thread_name.as_deref(), Some("tokio-runtime-worker"));
        // Work written inside `call` is visible to a later `run` (same pooled
        // connection is reused once the first is returned).
        let x: i64 = pool
            .run(|conn| Ok(conn.query_row("SELECT x FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(x, 42);
    }

    #[tokio::test]
    async fn call_propagates_closure_errors() {
        let pool = test_pool();
        let err = pool
            .call(|conn| {
                conn.execute("INSERT INTO nope(x) VALUES (1)", [])?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such table"));
    }

    #[tokio::test]
    async fn call_maps_a_panicking_closure_to_a_join_error() {
        let pool = test_pool();
        let err = pool
            .call(|_conn| -> Result<()> { panic!("closure exploded") })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("database worker task failed"), "{msg}");
        assert!(msg.contains("panicked"), "{msg}");
    }

    #[tokio::test]
    async fn blocking_runs_on_the_blocking_pool_and_passes_the_pool_in() {
        let pool = test_pool();
        // The closure receives the `Db` itself (pool-based functions call
        // `db.get()`), runs on a blocking-pool thread, and its result comes
        // back across the join.
        let (thread_name, x) = blocking(pool.clone(), |db| {
            let conn = db.get()?;
            let x: i64 = conn.query_row("SELECT 1", [], |r| r.get(0))?;
            Ok((std::thread::current().name().map(str::to_owned), x))
        })
        .await
        .unwrap();
        assert_ne!(thread_name.as_deref(), Some("tokio-runtime-worker"));
        assert_eq!(x, 1);
        // A closure error propagates as the returned error, not a panic.
        let err = blocking(pool, |db| -> Result<()> {
            let _ = db.get()?;
            Err(anyhow!("query exploded"))
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("query exploded"));
    }

    #[tokio::test]
    async fn blocking_maps_a_panicking_closure_to_a_join_error() {
        let pool = test_pool();
        let err = blocking(pool, |_db| -> Result<()> { panic!("closure exploded") })
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("db worker failed"), "{msg}");
        assert!(msg.contains("panicked"), "{msg}");
    }

    #[test]
    fn run_behaves_like_a_direct_get() {
        let pool = test_pool();
        pool.run(|conn| {
            conn.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (7);")?;
            Ok(())
        })
        .unwrap();
        let x: i64 = pool
            .run(|conn| Ok(conn.query_row("SELECT x FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(x, 7);
        // A closure error propagates as the returned error, not a panic.
        let err = pool
            .run(|_conn| -> Result<()> { Err(anyhow!("query exploded")) })
            .unwrap_err();
        assert!(err.to_string().contains("query exploded"));
    }
}
