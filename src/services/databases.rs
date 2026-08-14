//! Databases service: SQLite-backed per-server databases.
//!
//! Storage lives under the panel-owned Data Lab root (`datalab_dir/<uuid>`),
//! never under the server root: the server root is bind-mounted into the
//! workload sandbox and owned by the workload UID, so a workload could replace
//! any path component there with a symlink to the panel's own database.
//!
//! Workload access (blueprint install / proc launch) binds `datalab_dir/<uuid>`
//! read-write into the sandbox at [`crate::isolation::DATALAB_MOUNT_DIR`] and
//! chowns it to the workload UID — which makes the directory workload-owned.
//! Every panel open therefore refuses symlinks end to end: the directory
//! components are verified real (no symlinked `datalab_dir` or `<uuid>`), the
//! main file is opened with `SQLITE_OPEN_NOFOLLOW`, and the SQLite unix VFS
//! opens `-wal`/`-shm` with `O_NOFOLLOW` unconditionally. A planted symlink is
//! refused, never followed, so a workload cannot redirect the panel's own
//! SQLite through it.
use crate::models::Server;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
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

/// Per-server Data Lab budget: at most this many SQLite databases, and a
/// total (WAL-inclusive) byte allowance of 25% of the server's disk limit,
/// floored at [`DATALAB_BYTE_FLOOR`] so small servers keep a usable Data Lab.
pub const MAX_DATABASES_PER_SERVER: usize = 16;
const DATALAB_BYTE_FLOOR: u64 = 256 * 1024 * 1024; // 256 MiB

/// The server's Data Lab byte budget. `disk_mb` is the server's disk limit
/// in MiB; the quota is a quarter of it, never below the floor.
pub fn datalab_byte_cap(server: &Server) -> u64 {
    (server.disk_mb as u64)
        .saturating_mul(1024 * 1024)
        .saturating_div(4)
        .max(DATALAB_BYTE_FLOOR)
}

/// Resolve the panel-owned Data Lab directory for a server, creating it when
/// missing. Every component is verified to be a real directory: the workload
/// owns the `<uuid>` directory at launch, so a planted symlink component must
/// never be followed into panel storage.
fn checked_db_dir(server: &Server, datalab_root: &Path) -> Result<PathBuf> {
    // The root itself may not exist yet (a fresh install); create it rather
    // than demanding it, but refuse to traverse a symlinked root.
    match std::fs::symlink_metadata(datalab_root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "datalab root is not a directory: {}",
            datalab_root.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(datalab_root)?;
            let meta = std::fs::symlink_metadata(datalab_root)?;
            if !meta.is_dir() {
                bail!(
                    "datalab root is not a directory: {}",
                    datalab_root.display()
                );
            }
        }
        Err(e) => {
            return Err(e).with_context(|| format!("cannot access {}", datalab_root.display()));
        }
    }
    let dir = datalab_root.join(&server.uuid);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "server datalab path is not a real directory: {}",
            dir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&dir)?;
            // `create_dir_all` follows a symlink that appears under us; the
            // re-check keeps the create path as symlink-averse as the rest.
            let meta = std::fs::symlink_metadata(&dir)?;
            if !meta.is_dir() {
                bail!(
                    "server datalab path is not a real directory: {}",
                    dir.display()
                );
            }
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(e) => {
            return Err(e).with_context(|| format!("cannot access {}", dir.display()));
        }
    }
    Ok(dir)
}

/// Like [`checked_db_dir`], but never creates: `Ok(None)` when the server's
/// Data Lab directory does not exist yet (e.g. listing a fresh server).
fn existing_db_dir(server: &Server, datalab_root: &Path) -> Result<Option<PathBuf>> {
    match std::fs::symlink_metadata(datalab_root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!(
            "datalab root is not a directory: {}",
            datalab_root.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("cannot access {}", datalab_root.display()));
        }
    }
    let dir = datalab_root.join(&server.uuid);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => Ok(Some(dir)),
        Ok(_) => bail!(
            "server datalab path is not a real directory: {}",
            dir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot access {}", dir.display())),
    }
}

/// Directory holding sqlite files for a server.
///
/// This lives under the panel-owned Data Lab root, never under the server
/// root: `servers_dir/<uuid>` is bind-mounted into the workload sandbox and
/// chowned to the workload UID, so a workload could replace any path
/// component there with a symlink to the panel's own database. Keeping the
/// files outside that tree removes the attack surface rather than racing it.
pub fn db_dir(server: &Server, datalab_root: &Path) -> PathBuf {
    datalab_root.join(&server.uuid)
}
/// Open (or create) a sqlite database for a server.
///
/// The create path is the enforcement point for the per-server Data Lab
/// budget: a new database is refused once the database count or the total
/// (WAL-inclusive) byte allowance is exhausted.
pub fn open_server_db(
    server: &Server,
    datalab_root: &Path,
    name: &str,
) -> Result<Connection> {
    validate_name(name)?;
    if list(server, datalab_root)?.len() >= MAX_DATABASES_PER_SERVER {
        bail!(
            "Data Lab database limit reached ({MAX_DATABASES_PER_SERVER} per server)"
        );
    }
    if total_size(server, datalab_root)? >= datalab_byte_cap(server) {
        bail!("Data Lab storage quota exceeded");
    }
    let dir = checked_db_dir(server, datalab_root)?;
    let file = dir.join(format!("{name}.db"));
    let conn = open_flags(&file, true).context("cannot open server database")?;
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
    datalab_root: &Path,
    name: &str,
    read_only: bool,
) -> Result<Connection> {
    validate_name(name)?;
    let file = db_dir(server, datalab_root).join(format!("{name}.db"));
    let conn = open_flags(&file, false)
        .with_context(|| format!("cannot open server database {}", file.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    install_authorizer(&conn, read_only);
    Ok(conn)
}

/// Open a SQLite file with the panel's Data Lab hardening: `NOFOLLOW` refuses
/// a symlinked path (a workload owns the Data Lab directory at launch), and
/// `NO_MUTEX`/`URI` match the historical console flags.
fn open_flags(file: &Path, create: bool) -> Result<Connection> {
    let mut flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    if create {
        flags |= rusqlite::OpenFlags::SQLITE_OPEN_CREATE;
    }
    Connection::open_with_flags(file, flags).context("cannot open sqlite file")
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
    // The byte budget is enforced before every write so a runaway statement
    // cannot keep growing storage past the quota.
    if total_size(server, datalab_root)? > datalab_byte_cap(server) {
        bail!("Data Lab storage quota exceeded");
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


/// List sqlite databases for a server. Symlinked entries (a workload planting
/// a link where a database used to be) are excluded: they are not databases.
pub fn list(server: &Server, datalab_root: &Path) -> Result<Vec<String>> {
    let Some(dir) = existing_db_dir(server, datalab_root)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".db") {
            // `symlink_metadata` (not `metadata`): a symlink is skipped, never
            // resolved into whatever it points at.
            match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) if meta.is_file() => {
                    out.push(name.trim_end_matches(".db").to_string());
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Drop a database and its WAL sidecar files. `remove_file` unlinks a symlink
/// without following it, so a planted link at the database path is removed,
/// never traversed.
pub fn drop(server: &Server, datalab_root: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let Some(dir) = existing_db_dir(server, datalab_root)? else {
        return Ok(());
    };
    let file = dir.join(format!("{name}.db"));
    match std::fs::symlink_metadata(&file) {
        Ok(_) => std::fs::remove_file(&file)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    drop_sidecar_files(&dir, name);
    Ok(())
}

/// On-disk size of one database, including its `-wal`/`-shm` sidecars so the
/// reported size matches the storage the Data Lab actually consumes. Symlinks
/// count as zero and are never followed.
pub fn size(server: &Server, datalab_root: &Path, name: &str) -> Result<u64> {
    let Some(dir) = existing_db_dir(server, datalab_root)? else {
        return Ok(0);
    };
    Ok(size_in_dir(&dir, name))
}

fn size_in_dir(dir: &Path, name: &str) -> u64 {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let f = dir.join(format!("{name}.db{suffix}"));
        if let Ok(meta) = std::fs::symlink_metadata(&f) {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Total (WAL-inclusive) Data Lab bytes for a server.
pub fn total_size(server: &Server, datalab_root: &Path) -> Result<u64> {
    let mut total = 0u64;
    for name in list(server, datalab_root)? {
        total = total.saturating_add(size(server, datalab_root, &name)?);
    }
    Ok(total)
}

fn total_size_excluding(server: &Server, datalab_root: &Path, name: &str) -> Result<u64> {
    let mut total = 0u64;
    for n in list(server, datalab_root)? {
        if n != name {
            total = total.saturating_add(size(server, datalab_root, &n)?);
        }
    }
    Ok(total)
}

fn drop_sidecar_files(dir: &Path, name: &str) {
    for ext in ["-wal", "-shm"] {
        let f = dir.join(format!("{name}.db{ext}"));
        if std::fs::symlink_metadata(&f).is_ok() {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Verify that `path` is an intact SQLite database: openable read-only and
/// reporting `ok` from `PRAGMA integrity_check`. Anything else — a corrupt
/// file, a truncated file, a non-database — is an error. The open is
/// `NOFOLLOW` because callers may hand over workload-adjacent paths.
pub fn integrity_check(path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("cannot open {}", path.display()))?;
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .with_context(|| format!("integrity check failed on {}", path.display()))?;
    if result.trim() == "ok" {
        Ok(())
    } else {
        bail!("database integrity check failed: {result}");
    }
}

/// Snapshot one database to `dest` through the SQLite online backup API. The
/// copy is consistent and integrity-safe even while the workload is writing;
/// it never touches the source's `-wal`/`-shm` layout.
pub fn export_to(server: &Server, datalab_root: &Path, name: &str, dest: &Path) -> Result<()> {
    validate_name(name)?;
    let conn = open_server_db_no_create(server, datalab_root, name, true)?;
    let mut dst = Connection::open(dest).context("cannot create export file")?;
    let backup =
        rusqlite::backup::Backup::new(&conn, &mut dst).context("cannot start database backup")?;
    backup
        .run_to_completion(256, Duration::from_millis(50), None)
        .context("database backup failed")?;
    Ok(())
}

/// Snapshot every Data Lab database of a server into `dest_dir/<name>.db`.
/// Returns the number of databases snapshotted; a server with no Data Lab
/// directory yet is `Ok(0)`, not an error.
pub fn snapshot_server(server: &Server, datalab_root: &Path, dest_dir: &Path) -> Result<usize> {
    let names = list(server, datalab_root)?;
    if names.is_empty() {
        return Ok(0);
    }
    std::fs::create_dir_all(dest_dir)?;
    let mut count = 0usize;
    for name in &names {
        export_to(server, datalab_root, name, &dest_dir.join(format!("{name}.db")))?;
        count += 1;
    }
    Ok(count)
}

/// Replace a server database with the contents of `src` (an uploaded export).
/// The upload is integrity-checked, staged inside the Data Lab directory
/// (same filesystem, so the final rename is atomic), re-checked after the
/// copy, and only then swapped into place under the byte quota. A corrupt
/// upload leaves the live database untouched.
pub fn import(server: &Server, datalab_root: &Path, name: &str, src: &Path) -> Result<()> {
    validate_name(name)?;
    // Refuse an invalid upload before touching the live tree.
    integrity_check(src)?;
    let dir = checked_db_dir(server, datalab_root)?;
    let staging = dir.join(format!(".import-{name}-{}.db.tmp", std::process::id()));
    // The staging path is inside a workload-owned directory; open with
    // `create_new` + `O_NOFOLLOW` so a symlink planted there can never
    // redirect the copy.
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staging)
        .with_context(|| format!("cannot create staging file {}", staging.display()))?;
    let result = (|| -> Result<()> {
        let mut writer = writer;
        let mut reader = std::fs::File::open(src)
            .with_context(|| format!("cannot open upload {}", src.display()))?;
        std::io::copy(&mut reader, &mut writer)?;
        std::mem::drop(writer);
        // Belt and braces: the copied file must be an intact database too.
        integrity_check(&staging)?;
        // The byte budget is shared: the replacement must fit alongside the
        // server's other databases (the previous version of `name` no longer
        // counts, so it is excluded).
        let staging_len = std::fs::metadata(&staging)?.len();
        if total_size_excluding(server, datalab_root, name)?.saturating_add(staging_len)
            > datalab_byte_cap(server)
        {
            bail!("import would exceed the Data Lab storage quota");
        }
        // `rename` replaces the destination entry itself and never follows a
        // symlink at the target, so even a raced link is swapped over.
        std::fs::rename(&staging, dir.join(format!("{name}.db")))?;
        // The sidecar files belong to the old database; the replacement
        // starts with a clean WAL.
        drop_sidecar_files(&dir, name);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
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
            network_mbps: 0,
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

    #[test]
    fn datalab_byte_cap_is_quarter_of_disk_with_a_floor() {
        let mut small = test_server("srv-uuid");
        small.disk_mb = 64; // 16 MiB quarter < 256 MiB floor
        assert_eq!(datalab_byte_cap(&small), 256 * 1024 * 1024);
        let mut big = test_server("srv-uuid");
        big.disk_mb = 8192; // 2 GiB quarter > floor
        assert_eq!(datalab_byte_cap(&big), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn create_enforces_database_count_cap() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        for i in 0..MAX_DATABASES_PER_SERVER {
            let name = format!("db{i}");
            open_server_db(&server, &datalab_root, &name).unwrap();
        }
        let err = open_server_db(&server, &datalab_root, "overflow").unwrap_err();
        assert!(
            err.to_string().contains("limit reached"),
            "got: {err}"
        );
        assert_eq!(
            list(&server, &datalab_root).unwrap().len(),
            MAX_DATABASES_PER_SERVER
        );
    }

    /// A sparse file standing in for a runaway database must block further
    /// creates and writes once the total (WAL-inclusive) bytes pass the cap.
    #[test]
    fn byte_quota_blocks_create_and_exec() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let mut server = test_server("srv-uuid");
        server.disk_mb = 1024; // cap = 256 MiB floor
        open_server_db(&server, &datalab_root, "a").unwrap();
        let cap = datalab_byte_cap(&server);
        // Grow `a` past the cap without touching disk: a sparse extension.
        std::fs::OpenOptions::new()
            .write(true)
            .open(db_dir(&server, &datalab_root).join("a.db"))
            .unwrap()
            .set_len(cap + 1)
            .unwrap();
        let err = open_server_db(&server, &datalab_root, "b").unwrap_err();
        assert!(err.to_string().contains("quota"), "got: {err}");
        let err = exec(&server, &datalab_root, "a", "SELECT 1").unwrap_err();
        assert!(err.to_string().contains("quota"), "got: {err}");
    }

    /// `size` reports the WAL/SHM-inclusive footprint, so the reported number
    /// matches the storage the Data Lab actually consumes.
    #[test]
    fn size_includes_wal_and_shm_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        // Keep the connection open: on last-close SQLite checkpoints and may
        // delete the WAL, which would make the sidecar assertion vacuous.
        let conn = open_server_db(&server, &datalab_root, "lab").unwrap();
        conn.execute_batch(
            "CREATE TABLE t(v TEXT);
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 128)
             INSERT INTO t SELECT printf('%04096d', x) FROM c;",
        )
        .unwrap();
        let dir = db_dir(&server, &datalab_root);
        let main_len = std::fs::metadata(dir.join("lab.db")).unwrap().len();
        let wal_len = std::fs::metadata(dir.join("lab.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        let shm_len = std::fs::metadata(dir.join("lab.db-shm"))
            .map(|m| m.len())
            .unwrap_or(0);
        assert!(wal_len > 0, "WAL should hold the uncheckpointed rows");
        let reported = size(&server, &datalab_root, "lab").unwrap();
        assert_eq!(reported, main_len + wal_len + shm_len);
        assert!(reported > main_len, "size must include the WAL");
    }
    #[test]
    fn export_import_round_trip_preserves_data() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        open_server_db(&server, &datalab_root, "lab").unwrap();
        exec(
            &server,
            &datalab_root,
            "lab",
            "CREATE TABLE t(v INTEGER, s TEXT);
             INSERT INTO t VALUES (1, 'alpha'), (2, 'beta');",
        )
        .unwrap();
        let snap = temp.path().join("lab.db");
        export_to(&server, &datalab_root, "lab", &snap).unwrap();
        integrity_check(&snap).unwrap();

        drop(&server, &datalab_root, "lab").unwrap();
        assert!(list(&server, &datalab_root).unwrap().is_empty());

        import(&server, &datalab_root, "lab", &snap).unwrap();
        let rows = query(&server, &datalab_root, "lab", "SELECT v, s FROM t ORDER BY v")
            .unwrap();
        assert_eq!(rows, serde_json::json!([
            {"v": 1, "s": "alpha"},
            {"v": 2, "s": "beta"}
        ]));
    }

    #[test]
    fn import_refuses_corrupt_and_non_database_uploads() {
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        open_server_db(&server, &datalab_root, "lab").unwrap();
        exec(&server, &datalab_root, "lab", "CREATE TABLE t(v); INSERT INTO t VALUES (7);")
            .unwrap();

        let garbage = temp.path().join("garbage.db");
        std::fs::write(&garbage, b"this is not a sqlite database at all").unwrap();
        assert!(integrity_check(&garbage).is_err());
        assert!(import(&server, &datalab_root, "lab", &garbage).is_err());
        // The live database is untouched.
        let rows = query(&server, &datalab_root, "lab", "SELECT v FROM t").unwrap();
        assert_eq!(rows, serde_json::json!([{"v": 7}]));

        // A truncated (corrupt) sqlite file is refused the same way.
        let snap = temp.path().join("truncated.db");
        export_to(&server, &datalab_root, "lab", &snap).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&snap)
            .unwrap()
            .set_len(512)
            .unwrap();
        assert!(integrity_check(&snap).is_err());
        assert!(import(&server, &datalab_root, "lab", &snap).is_err());
        let rows = query(&server, &datalab_root, "lab", "SELECT v FROM t").unwrap();
        assert_eq!(rows, serde_json::json!([{"v": 7}]));
    }

    /// The escape a workload-owned Data Lab directory would otherwise open:
    /// a symlink planted where the panel expects a database must never be
    /// followed — by open, list, size, or drop.
    #[test]
    fn panel_opens_never_follow_planted_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let datalab_root = temp.path().join("datalab");
        let server = test_server("srv-uuid");
        let dir = db_dir(&server, &datalab_root);
        open_server_db(&server, &datalab_root, "lab").unwrap();
        // A decoy standing in for the panel's own database.
        let decoy = temp.path().join("decoy.db");
        std::fs::write(&decoy, b"panel-secrets").unwrap();

        // Plant: the workload replaces the database file with a symlink.
        std::fs::remove_file(dir.join("lab.db")).unwrap();
        symlink(&decoy, dir.join("lab.db")).unwrap();

        assert!(exec(&server, &datalab_root, "lab", "SELECT 1").is_err());
        assert!(query(&server, &datalab_root, "lab", "SELECT 1").is_err());
        // Not a regular database any more: list excludes it, size counts 0.
        assert!(!list(&server, &datalab_root).unwrap().contains(&"lab".to_string()));
        assert_eq!(size(&server, &datalab_root, "lab").unwrap(), 0);
        // The decoy was never read or written.
        assert_eq!(std::fs::read(&decoy).unwrap(), b"panel-secrets");

        // A planted symlink at the directory component is refused outright.
        std::fs::remove_dir_all(&dir).unwrap();
        symlink(temp.path().join("elsewhere"), &dir).unwrap();
        std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
        assert!(open_server_db(&server, &datalab_root, "fresh").is_err());
        assert!(exec(&server, &datalab_root, "lab", "SELECT 1").is_err());
        assert!(drop(&server, &datalab_root, "lab").is_err());
    }
}