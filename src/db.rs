//! Database connection & schema (SQLite via rusqlite).
use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;

pub type Db = Arc<Mutex<Connection>>;

pub fn open(path: &str) -> Result<Db> {
    let conn = Connection::open(path)?;
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    migrate(&conn)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Arc::new(Mutex::new(conn)))
}

/// Apply schema. Uses user_version pragma for incremental migrations.
fn migrate(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;

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

            CREATE TABLE IF NOT EXISTS egss (
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

            CREATE TABLE IF NOT EXISTS servers (
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

            CREATE TABLE IF NOT EXISTS server_variables (
                server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
                egg_id INTEGER NOT NULL,
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
        conn.execute_batch(
            r#"
            ALTER TABLE allocations ADD COLUMN node TEXT NOT NULL DEFAULT 'local';
            UPDATE allocations SET node=COALESCE((SELECT node FROM servers WHERE servers.id=allocations.server_id),'local');
            DELETE FROM allocations WHERE id NOT IN (SELECT MIN(id) FROM allocations GROUP BY node,port);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_allocations_node_port ON allocations(node,port);
            UPDATE servers SET port=(SELECT MIN(port) FROM allocations WHERE allocations.server_id=servers.id) WHERE port IS NULL;
            PRAGMA user_version = 4;
            "#,
        )?;
    }
    if version < 5 {
        // Native VoltPanel domain naming: blueprints replace the borrowed egg table,
        // and columns that were never read by the isolation layer are removed.
        conn.execute_batch(
            r#"
            ALTER TABLE egss RENAME TO blueprints;
            ALTER TABLE blueprints RENAME COLUMN docker_image TO runtime_hint;
            ALTER TABLE blueprints DROP COLUMN config_files;
            ALTER TABLE servers RENAME COLUMN egg_id TO blueprint_id;
            ALTER TABLE servers RENAME COLUMN docker_image TO runtime_hint;
            ALTER TABLE servers DROP COLUMN threads;
            ALTER TABLE servers DROP COLUMN ignore_oom;
            ALTER TABLE server_variables RENAME COLUMN egg_id TO blueprint_id;
            PRAGMA user_version = 5;
            "#,
        )?;
    }
    if version < 6 {
        // Typed capability model: subusers gain a role preset, and legacy
        // free-form permission tokens are normalized to canonical capabilities.
        conn.execute_batch(
            r#"
            ALTER TABLE subusers ADD COLUMN role TEXT NOT NULL DEFAULT 'custom';
            PRAGMA user_version = 6;
            "#,
        )?;
        normalize_subuser_permissions(conn)?;
    }
    if version < 7 {
        // Telemetry, blueprint revisions, capability-scoped keys, webhook bus,
        // schedule retry policy, and first-class sites.
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

            ALTER TABLE blueprints ADD COLUMN version INTEGER NOT NULL DEFAULT 1;
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
            ALTER TABLE servers ADD COLUMN blueprint_version INTEGER NOT NULL DEFAULT 0;

            ALTER TABLE api_keys ADD COLUMN capabilities TEXT NOT NULL DEFAULT '["*"]';
            ALTER TABLE api_keys ADD COLUMN server_ids TEXT NOT NULL DEFAULT '[]';
            ALTER TABLE api_keys ADD COLUMN expires_at TEXT;
            ALTER TABLE api_keys ADD COLUMN revoked INTEGER NOT NULL DEFAULT 0;
            UPDATE api_keys SET capabilities='["*"]' WHERE scopes IN ('full','*');

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

            ALTER TABLE schedules ADD COLUMN max_retries INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE schedules ADD COLUMN retry_backoff_s INTEGER NOT NULL DEFAULT 30;
            ALTER TABLE schedules ADD COLUMN only_when_online INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE schedule_runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1;
            ALTER TABLE schedule_runs ADD COLUMN finished_at TEXT;

            ALTER TABLE websites ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';
            ALTER TABLE websites ADD COLUMN upstream TEXT NOT NULL DEFAULT '';
            ALTER TABLE websites ADD COLUMN force_https INTEGER NOT NULL DEFAULT 0;
            DELETE FROM websites WHERE id NOT IN (SELECT MIN(id) FROM websites GROUP BY domain);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_websites_domain ON websites(domain);

            PRAGMA user_version = 7;
            "#,
        )?;
    }
    if version < 8 {
        // Node agents serve HTTPS with a self-signed certificate; the panel pins
        // the fingerprint captured at enrollment instead of trusting the WebPKI.
        conn.execute_batch(
            r#"
            ALTER TABLE nodes ADD COLUMN tls_fingerprint TEXT NOT NULL DEFAULT '';
            PRAGMA user_version = 8;
            "#,
        )?;
    }
    Ok(())
}

/// Rewrite pre-v6 permission rows into canonical capability names and infer a
/// role preset where the stored set matches one exactly.
fn normalize_subuser_permissions(conn: &Connection) -> Result<()> {
    use crate::capability::{expand_legacy, Role};
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id,permissions FROM subusers")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<std::result::Result<_, _>>()?
    };
    for (id, raw) in rows {
        let tokens: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
        let caps = expand_legacy(&tokens);
        let role = Role::ALL
            .into_iter()
            .find(|r| r.capabilities() == caps)
            .unwrap_or(Role::Custom);
        let names: Vec<&str> = caps.iter().map(|c| c.as_str()).collect();
        conn.execute(
            "UPDATE subusers SET permissions=?1, role=?2 WHERE id=?3",
            rusqlite::params![serde_json::to_string(&names)?, role.as_str(), id],
        )?;
    }
    Ok(())
}

/// Run integrity check used by /system/health.
pub fn integrity_check(db: &Db) -> Result<String> {
    let conn = db.lock();
    let out: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    Ok(out)
}
