//! Databases service: SQLite-backed per-server databases.
use crate::models::Server;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Hard limits for the SQL console, applied per request so a hostile or
/// mistaken statement cannot exhaust panel memory or pin the worker thread:
/// statement text size, output row count, per-cell size, serialized output
/// bytes, and total execution time (the deadline is enforced by the SQLite
/// progress handler, which fires every `EXEC_PROGRESS_OPS` VM opcodes and
/// interrupts the running statement).
const MAX_STMT_BYTES: usize = 1024 * 1024; // 1 MiB of SQL text
const MAX_QUERY_ROWS: usize = 10_000;
const MAX_CELL_BYTES: usize = 4 * 1024 * 1024; // 4 MiB per text/blob cell
const MAX_QUERY_BYTES: usize = 32 * 1024 * 1024; // 32 MiB of serialized rows
const EXEC_DEADLINE_MS: u64 = 30_000; // 30 s
const EXEC_PROGRESS_OPS: i32 = 1000;

/// Directory holding sqlite files for a server.
///
/// This lives under the panel-owned Data Lab root, never under the server
/// root: `servers_dir/<uuid>` is bind-mounted into the workload sandbox and
/// chowned to the workload UID, so a workload could replace any path
/// component there with a symlink to the panel's own database. Keeping the
/// files outside that tree removes the attack surface rather than racing it.
pub fn db_dir(server: &Server, datalab_root: &std::path::Path) -> PathBuf {
    datalab_root.join(&server.uuid)
}

/// Open (or create) a sqlite database for a server.
pub fn open_server_db(
    server: &Server,
    datalab_root: &std::path::Path,
    name: &str,
) -> Result<Connection> {
    validate_name(name)?;
    let dir = db_dir(server, datalab_root);
    std::fs::create_dir_all(&dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let file = dir.join(format!("{name}.db"));
    let conn = Connection::open(&file).context("cannot open server database")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    install_authorizer(&conn, false);
    Ok(conn)
}

/// Open a server database read-write without creating the file.
///
/// `Connection::open` implies `SQLITE_OPEN_CREATE`, so a query or exec
/// racing a drop would silently recreate the deleted database file. The
/// exec/query paths must fail on a missing database instead of
/// resurrecting it, so they open with the default flags minus `CREATE`.
/// `read_only` only tunes the authorizer; the connection stays read-write
/// so WAL reads behave exactly as before.
fn open_server_db_no_create(
    server: &Server,
    datalab_root: &std::path::Path,
    name: &str,
    read_only: bool,
) -> Result<Connection> {
    validate_name(name)?;
    let file = db_dir(server, datalab_root).join(format!("{name}.db"));
    let conn = Connection::open_with_flags(
        &file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("cannot open server database {}", file.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    install_authorizer(&conn, read_only);
    Ok(conn)
}

fn install_authorizer(conn: &Connection, read_only: bool) {
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        use AuthAction::*;
        let denied = match ctx.action {
            // Attaching/detaching databases and the file-backed functions can
            // read or write files outside the server's .voltdb directory.
            Attach { .. } | Detach { .. } => true,
            Function { function_name }
                if function_name.eq_ignore_ascii_case("load_extension")
                    || function_name.eq_ignore_ascii_case("readfile")
                    || function_name.eq_ignore_ascii_case("writefile")
                    || function_name.eq_ignore_ascii_case("edit") =>
            {
                true
            }
            // Virtual tables (fts, fileio, ...) can load external modules.
            CreateVtable { .. } | DropVtable { .. } => true,
            // Schema/stats rewrites are denied outright.
            Reindex { .. } | Analyze { .. } => true,
            // Any pragma carrying a value mutates configuration; the three
            // filesystem-pointing pragmas are denied even read-only.
            Pragma {
                pragma_name,
                pragma_value,
            } => {
                pragma_value.is_some()
                    || matches!(
                        pragma_name.to_ascii_lowercase().as_str(),
                        "writable_schema" | "temp_store_directory" | "data_store_directory"
                    )
            }
            // On read-only connections every data/schema mutation is denied,
            // including the temp-schema variants.
            Insert { .. }
            | Update { .. }
            | Delete { .. }
            | CreateTable { .. }
            | DropTable { .. }
            | AlterTable { .. }
            | CreateIndex { .. }
            | DropIndex { .. }
            | CreateView { .. }
            | DropView { .. }
            | CreateTrigger { .. }
            | DropTrigger { .. }
            | CreateTempTable { .. }
            | DropTempTable { .. }
            | CreateTempIndex { .. }
            | DropTempIndex { .. }
            | CreateTempView { .. }
            | DropTempView { .. }
            | CreateTempTrigger { .. }
            | DropTempTrigger { .. }
                if read_only =>
            {
                true
            }
            _ => false,
        };
        if denied {
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }));
}

/// True when `sql` contains `kw` as a bare keyword outside strings, quoted
/// identifiers and comments. SQLite never invokes the authorizer for
/// `VACUUM` (or `VACUUM INTO`, which writes an external file), so it must be
/// rejected here.
fn contains_keyword(sql: &str, kw: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'[' => {
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                i += 1;
            }
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == q {
                        i += 1;
                        if bytes.get(i) == Some(&q) {
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if sql[start..i].eq_ignore_ascii_case(kw) {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// Execute SQL statements against a server database.
pub fn exec(
    server: &Server,
    datalab_root: &std::path::Path,
    name: &str,
    sql: &str,
) -> Result<()> {
    if sql.len() > MAX_STMT_BYTES {
        bail!(
            "SQL statement exceeds {} MiB limit",
            MAX_STMT_BYTES / (1024 * 1024)
        );
    }
    if contains_keyword(sql, "VACUUM") {
        bail!("VACUUM is not allowed");
    }
    let conn = open_server_db_no_create(server, datalab_root, name, false)?;
    install_deadline(&conn);
    conn.execute_batch(sql)?;
    Ok(())
}

/// Run a read query and return rows as JSON.
pub fn query(
    server: &Server,
    datalab_root: &std::path::Path,
    name: &str,
    sql: &str,
) -> Result<serde_json::Value> {
    if sql.len() > MAX_STMT_BYTES {
        bail!(
            "SQL statement exceeds {} MiB limit",
            MAX_STMT_BYTES / (1024 * 1024)
        );
    }
    if contains_keyword(sql, "VACUUM") {
        bail!("VACUUM is not allowed");
    }
    let conn = open_server_db_no_create(server, datalab_root, name, true)?;
    install_deadline(&conn);
    let mut stmt = conn.prepare(sql)?;
    let col_count = stmt.column_count();
    let cols: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    // Approximate serialized size, accumulated before each row is pushed so
    // the output is bounded in memory, not just on the wire.
    let mut total_bytes: usize = 0;
    while let Some(row) = rows.next()? {
        if out.len() >= MAX_QUERY_ROWS {
            bail!("query returned too many rows (limit {MAX_QUERY_ROWS})");
        }
        let mut obj = serde_json::Map::new();
        for (i, col) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                rusqlite::types::ValueRef::Real(n) => serde_json::json!(n),
                rusqlite::types::ValueRef::Text(t) => {
                    if t.len() > MAX_CELL_BYTES {
                        bail!("query cell exceeds {} MiB limit", MAX_CELL_BYTES / (1024 * 1024));
                    }
                    serde_json::json!(String::from_utf8_lossy(t))
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    if b.len() > MAX_CELL_BYTES {
                        bail!("query cell exceeds {} MiB limit", MAX_CELL_BYTES / (1024 * 1024));
                    }
                    serde_json::json!(STANDARD.encode(b))
                }
            };
            obj.insert(col.clone(), v);
        }
        let row_bytes = serde_json::to_string(&obj)
            .map(|s| s.len())
            .unwrap_or_else(|_| obj.len() * 16);
        total_bytes = total_bytes.saturating_add(row_bytes);
        if total_bytes > MAX_QUERY_BYTES {
            bail!(
                "query result exceeds {} MiB limit",
                MAX_QUERY_BYTES / (1024 * 1024)
            );
        }
        out.push(serde_json::Value::Object(obj));
    }
    Ok(serde_json::Value::Array(out))
}
/// Arm a wall-clock deadline on `conn` for the next statement(s): the SQLite
/// progress handler is invoked every `EXEC_PROGRESS_OPS` VM opcodes, and
/// returning `true` interrupts the running statement with SQLITE_INTERRUPT.
/// The handler fires only while SQLite is stepping, so a statement that
/// yields to the OS (e.g. an fsync storm) is not cut mid-step — but no single
/// statement can run past the deadline.
fn install_deadline(conn: &Connection) {
    let start = Instant::now();
    let deadline = Duration::from_millis(EXEC_DEADLINE_MS);
    conn.progress_handler(
        EXEC_PROGRESS_OPS,
        Some(move || start.elapsed() > deadline),
    );
}


/// List sqlite databases for a server.
pub fn list(server: &Server, datalab_root: &std::path::Path) -> Result<Vec<String>> {
    let dir = db_dir(server, datalab_root);
    let mut out = Vec::new();
    if dir.exists() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".db") {
                out.push(name.trim_end_matches(".db").to_string());
            }
        }
    }
    Ok(out)
}

pub fn drop(server: &Server, datalab_root: &std::path::Path, name: &str) -> Result<()> {
    let dir = db_dir(server, datalab_root);
    let file = dir.join(format!("{name}.db"));
    if file.exists() {
        std::fs::remove_file(&file)?;
    }
    for ext in ["-wal", "-shm"] {
        let f = dir.join(format!("{name}.db{ext}"));
        if f.exists() {
            let _ = std::fs::remove_file(f);
        }
    }
    Ok(())
}

pub fn size(server: &Server, datalab_root: &std::path::Path, name: &str) -> Result<u64> {
    let dir = db_dir(server, datalab_root);
    let file = dir.join(format!("{name}.db"));
    Ok(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        bail!("invalid database name");
    }
    Ok(())
}

/// Move Data Lab files created before the storage relocation out of the
/// workload-owned server tree and into the panel-owned Data Lab root.
///
/// Called once at startup, before any workload process is spawned, so no
/// workload can be racing these paths. The legacy directory is still treated
/// as hostile: every component is opened with `O_NOFOLLOW` and anything that
/// is not a regular file is left behind rather than followed.
pub fn migrate_legacy_storage(servers_root: &std::path::Path, datalab_root: &std::path::Path) {
    let entries = match std::fs::read_dir(servers_root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let legacy = entry.path().join(".voltdb");
        match std::fs::symlink_metadata(&legacy) {
            Ok(meta) if meta.is_dir() => {}
            _ => continue,
        }
        let uuid = entry.file_name();
        let dest = datalab_root.join(&uuid);
        let files = match std::fs::read_dir(&legacy) {
            Ok(files) => files,
            Err(_) => continue,
        };
        let mut moved = 0usize;
        for file in files.flatten() {
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.ends_with(".db") && !name.ends_with(".db-wal") && !name.ends_with(".db-shm") {
                continue;
            }
            let src = legacy.join(name);
            // Regular files only: a symlink here is exactly the escape this
            // relocation exists to close, and copying through it would carry
            // the attack across the migration.
            match std::fs::symlink_metadata(&src) {
                Ok(meta) if meta.is_file() => {}
                _ => {
                    tracing::warn!(path = %src.display(), "skipping non-regular Data Lab entry");
                    continue;
                }
            }
            if std::fs::create_dir_all(&dest).is_err() {
                break;
            }
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&src);
            let Ok(mut reader) = opened else { continue };
            let created = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dest.join(name));
            let Ok(mut writer) = created else { continue };
            if std::io::copy(&mut reader, &mut writer).is_ok() {
                let _ = std::fs::remove_file(&src);
                moved += 1;
            }
        }
        if moved > 0 {
            tracing::info!(
                server = %uuid.to_string_lossy(),
                files = moved,
                "migrated Data Lab storage out of the workload tree"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorizer_denies_attach_and_query_mutation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t(v INTEGER); INSERT INTO t VALUES(1);
             CREATE INDEX ix ON t(v); CREATE VIEW w AS SELECT v FROM t;",
        )
        .unwrap();
        install_authorizer(&conn, false);
        assert!(conn
            .execute_batch("ATTACH DATABASE '/tmp/escape.db' AS x;")
            .is_err());
        assert!(conn.execute_batch("DETACH DATABASE main;").is_err());
        assert!(conn
            .execute_batch("SELECT readfile('/etc/passwd');")
            .is_err());
        assert!(conn
            .execute_batch("SELECT writefile('/tmp/x', 'y');")
            .is_err());
        assert!(conn.execute_batch("SELECT load_extension('lib');").is_err());
        install_authorizer(&conn, true);
        assert!(conn.execute_batch("UPDATE t SET v=2;").is_err());
        assert!(conn.execute_batch("CREATE TABLE x(v);").is_err());
        assert!(conn.execute_batch("CREATE TEMP TABLE tmp(v);").is_err());
        // `w` exists, so these drops are real mutations and must be denied —
        // with or without `IF EXISTS` (see the regression test below for the
        // missing-object no-op case).
        assert!(conn.execute_batch("DROP VIEW w;").is_err());
        assert!(conn.execute_batch("DROP VIEW IF EXISTS w;").is_err());
        assert!(conn.execute_batch("ALTER TABLE t RENAME TO t2;").is_err());
        assert!(conn.execute_batch("ANALYZE;").is_err());
        // `ix` exists, so REINDEX is a real mutation (index rebuild) and must
        // be denied; on an index-less schema SQLite no-ops it instead.
        assert!(conn.execute_batch("REINDEX;").is_err());
        assert!(conn
            .execute_batch("CREATE VIRTUAL TABLE vt USING fts5(x);")
            .is_err());
        assert!(conn.execute_batch("PRAGMA writable_schema = ON;").is_err());
        assert!(conn
            .execute_batch("PRAGMA temp_store_directory = '/tmp';")
            .is_err());
        assert!(conn.execute_batch("PRAGMA journal_mode = WAL;").is_err());
        assert_eq!(
            conn.query_row("SELECT v FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn authorizer_denies_drops_of_existing_objects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t(v INTEGER);
             INSERT INTO t VALUES(1);
             CREATE VIEW w AS SELECT v FROM t;
             CREATE INDEX ix ON t(v);
             CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET v = new.v; END;",
        )
        .unwrap();
        install_authorizer(&conn, true);
        // Existing objects: the drop is a real mutation, so the authorizer is
        // invoked and denies it — with or without `IF EXISTS`.
        for sql in [
            "DROP VIEW w;",
            "DROP VIEW IF EXISTS w;",
            "DROP TABLE t;",
            "DROP TABLE IF EXISTS t;",
            "DROP INDEX IF EXISTS ix;",
            "DROP TRIGGER IF EXISTS tr;",
            // `ix` exists, so this rebuilds a real index — a mutation.
            "REINDEX;",
        ] {
            assert!(conn.execute_batch(sql).is_err(), "should deny {sql}");
        }
        // Missing objects: `DROP ... IF EXISTS` is a SQLite no-op — the
        // authorizer is never invoked because nothing mutates. It is safe to
        // succeed; asserting denial here would be a false security claim.
        assert!(conn.execute_batch("DROP VIEW IF EXISTS nope;").is_ok());
        assert!(conn.execute_batch("DROP TABLE IF EXISTS nope;").is_ok());
        // Same for REINDEX on an index-less schema: SQLite no-ops it without
        // consulting the authorizer.
        let bare = Connection::open_in_memory().unwrap();
        install_authorizer(&bare, true);
        assert!(bare.execute_batch("REINDEX;").is_ok());
        // Nothing above was dropped.
        assert_eq!(
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }

    #[test]
    fn contains_keyword_catches_vacuum_only_outside_strings_and_comments() {
        assert!(contains_keyword("VACUUM", "VACUUM"));
        assert!(contains_keyword("  vacuum into '/tmp/out.db'", "VACUUM"));
        assert!(!contains_keyword("-- VACUUM\nSELECT 1;", "VACUUM"));
        assert!(contains_keyword("/* VACUUM */ VACUUM", "VACUUM"));
        assert!(!contains_keyword("SELECT 'VACUUM';", "VACUUM"));
        assert!(!contains_keyword("SELECT \"VACUUM\";", "VACUUM"));
        assert!(!contains_keyword("SELECT [VACUUM];", "VACUUM"));
        assert!(!contains_keyword("SELECT `VACUUM`;", "VACUUM"));
        assert!(!contains_keyword("SELECT vacuum_col FROM t;", "VACUUM"));
        assert!(!contains_keyword("SELECT 1;", "VACUUM"));
    }

    #[test]
    fn validate_name_rejects_paths_and_url_metacharacters() {
        for bad in [
            "",
            "..",
            ".",
            ".hidden",
            "a/b",
            "a\\b",
            "a..b",
            "a:b",
            "a?b",
            "a#b",
            "a&b",
            "a=b",
            "a%2eb",
            "a+b",
            "a@b",
            "a b",
            "a;b",
            "a\u{0}b",
            "héllo",
            &"a".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "should reject {bad:?}");
        }
        for good in ["db", "my_db-1", "a.b", "ABC123._-"] {
            assert!(validate_name(good).is_ok(), "should accept {good:?}");
        }
    }

    fn test_server(uuid: &str) -> Server {
        Server {
            id: 1,
            uuid: uuid.into(),
            name: "test".into(),
            user_id: 1,
            blueprint_id: 0,
            description: String::new(),
            status: "running".into(),
            runtime_hint: String::new(),
            startup: String::new(),
            node: "local".into(),
            port: None,
            memory_mb: 1024,
            disk_mb: 1024,
            cpu_percent: 100,
            suspended: false,
            auto_restart: false,
            restart_count: 0,
            crash_detect_clean_exit: false,
            crash_restart_budget: 5,
            crash_restarts: 0,
            crash_window_start: String::new(),
            crash_reason: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// The escape being closed: Data Lab files must never resolve inside the
    /// server root, which is bind-mounted into the sandbox and owned by the
    /// workload UID.
    #[test]
    fn storage_lives_outside_the_workload_owned_server_root() {
        let temp = tempfile::tempdir().unwrap();
        let servers_root = temp.path().join("servers");
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");

        let dir = db_dir(&server, &datalab_root);
        assert!(dir.starts_with(&datalab_root));
        assert!(!dir.starts_with(&servers_root));

        open_server_db(&server, &datalab_root, "lab").unwrap();
        assert!(datalab_root.join("srv-uuid").join("lab.db").exists());
        assert!(!servers_root.join("srv-uuid").join(".voltdb").exists());
    }

    /// create → drop → query/exec must not resurrect the database: the
    /// exec/query open path carries no `CREATE` flag, so a missing file is
    /// an error, never a silent re-creation of the deleted database.
    #[test]
    fn query_and_exec_after_drop_do_not_recreate_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        let file = db_dir(&server, &datalab_root).join("gone.db");

        open_server_db(&server, &datalab_root, "gone").unwrap();
        assert!(file.exists());

        // A live database still reads through the no-create open path.
        let rows = query(&server, &datalab_root, "gone", "SELECT 42 AS n").unwrap();
        assert_eq!(rows[0]["n"], serde_json::json!(42));

        drop(&server, &datalab_root, "gone").unwrap();
        assert!(!file.exists());

        // Reading a dropped database fails and leaves no file behind.
        assert!(query(&server, &datalab_root, "gone", "SELECT 1").is_err());
        assert!(!file.exists());
        assert!(list(&server, &datalab_root).unwrap().is_empty());

        // Writing to a dropped database fails the same way.
        assert!(exec(&server, &datalab_root, "gone", "CREATE TABLE t(v)").is_err());
        assert!(!file.exists());
        assert!(list(&server, &datalab_root).unwrap().is_empty());
    }

    /// A workload that plants a symlink where its legacy database used to be
    /// must not get that link followed during the upgrade migration.
    #[test]
    fn migration_moves_real_files_and_refuses_planted_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let servers_root = temp.path().join("servers");
        let datalab_root = temp.path().join("datalab");
        let legacy = servers_root.join("srv-uuid").join(".voltdb");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&datalab_root).unwrap();

        let panel_db = temp.path().join("voltpanel.db");
        std::fs::write(&panel_db, b"panel-secrets").unwrap();
        std::fs::write(legacy.join("real.db"), b"workload-data").unwrap();
        symlink(&panel_db, legacy.join("stolen.db")).unwrap();

        migrate_legacy_storage(&servers_root, &datalab_root);

        let moved = datalab_root.join("srv-uuid");
        assert_eq!(
            std::fs::read(moved.join("real.db")).unwrap(),
            b"workload-data"
        );
        // The symlinked entry was skipped, so the panel database was never
        // copied out and is still intact where it belongs.
        assert!(!moved.join("stolen.db").exists());
        assert_eq!(std::fs::read(&panel_db).unwrap(), b"panel-secrets");
        // The real file was consumed; the hostile one was left in place.
        assert!(!legacy.join("real.db").exists());
        assert!(legacy.join("stolen.db").symlink_metadata().is_ok());
    }
}