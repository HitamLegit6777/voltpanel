//! Databases service: SQLite-backed per-server databases.
use crate::models::Server;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use std::path::PathBuf;

/// Directory holding sqlite files for a server.
pub fn db_dir(server: &Server, servers_root: &std::path::Path) -> PathBuf {
    servers_root.join(&server.uuid).join(".voltdb")
}

/// Open (or create) a sqlite database for a server.
pub fn open_server_db(
    server: &Server,
    servers_root: &std::path::Path,
    name: &str,
) -> Result<Connection> {
    validate_name(name)?;
    let dir = db_dir(server, servers_root);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{name}.db"));
    let conn = Connection::open(&file).context("cannot open server database")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    install_authorizer(&conn, false);
    Ok(conn)
}

fn install_authorizer(conn: &Connection, read_only: bool) {
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        let denied = match ctx.action {
            AuthAction::Attach { .. }
            | AuthAction::Detach { .. }
            | AuthAction::CreateVtable { .. }
            | AuthAction::DropVtable { .. } => true,
            AuthAction::Function { function_name }
                if function_name.eq_ignore_ascii_case("load_extension") =>
            {
                true
            }
            AuthAction::Pragma {
                pragma_name,
                pragma_value,
            } => {
                pragma_value.is_some()
                    || matches!(
                        pragma_name.to_ascii_lowercase().as_str(),
                        "writable_schema" | "temp_store_directory" | "data_store_directory"
                    )
            }
            AuthAction::Insert { .. }
            | AuthAction::Update { .. }
            | AuthAction::Delete { .. }
            | AuthAction::CreateTable { .. }
            | AuthAction::DropTable { .. }
            | AuthAction::AlterTable { .. }
            | AuthAction::CreateIndex { .. }
            | AuthAction::DropIndex { .. }
            | AuthAction::CreateTrigger { .. }
            | AuthAction::DropTrigger { .. }
            | AuthAction::CreateView { .. }
            | AuthAction::DropView { .. }
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

/// Execute SQL statements against a server database.
pub fn exec(server: &Server, name: &str, sql: &str) -> Result<()> {
    let conn = open_server_db(server, &crate::SETTINGS.paths.servers_dir, name)?;
    conn.execute_batch(sql)?;
    Ok(())
}

/// Run a read query and return rows as JSON.
pub fn query(server: &Server, name: &str, sql: &str) -> Result<serde_json::Value> {
    let conn = open_server_db(server, &crate::SETTINGS.paths.servers_dir, name)?;
    install_authorizer(&conn, true);
    let mut stmt = conn.prepare(sql)?;
    let col_count = stmt.column_count();
    let cols: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let rows = stmt.query([])?;
    let mut out = Vec::new();
    let mut rows = rows;
    while let Some(row) = rows.next()? {
        let mut obj = serde_json::Map::new();
        for (i, col) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                rusqlite::types::ValueRef::Real(n) => serde_json::json!(n),
                rusqlite::types::ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(b) => serde_json::json!(STANDARD.encode(b)),
            };
            obj.insert(col.clone(), v);
        }
        out.push(serde_json::Value::Object(obj));
    }
    Ok(serde_json::Value::Array(out))
}

/// List sqlite databases for a server.
pub fn list(server: &Server) -> Result<Vec<String>> {
    let dir = db_dir(server, &crate::SETTINGS.paths.servers_dir);
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

pub fn drop(server: &Server, name: &str) -> Result<()> {
    let dir = db_dir(server, &crate::SETTINGS.paths.servers_dir);
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

pub fn size(server: &Server, name: &str) -> Result<u64> {
    let dir = db_dir(server, &crate::SETTINGS.paths.servers_dir);
    let file = dir.join(format!("{name}.db"));
    Ok(std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        bail!("invalid database name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authorizer_denies_attach_and_query_mutation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(v INTEGER); INSERT INTO t VALUES(1);")
            .unwrap();
        install_authorizer(&conn, false);
        assert!(conn
            .execute_batch("ATTACH DATABASE '/tmp/escape.db' AS x;")
            .is_err());
        install_authorizer(&conn, true);
        assert!(conn.execute_batch("UPDATE t SET v=2;").is_err());
        assert_eq!(
            conn.query_row("SELECT v FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
