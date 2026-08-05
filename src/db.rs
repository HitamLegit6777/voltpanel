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
    Ok(())
}

/// Run integrity check used by /system/health.
pub fn integrity_check(db: &Db) -> Result<String> {
    let conn = db.lock();
    let out: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    Ok(out)
}
