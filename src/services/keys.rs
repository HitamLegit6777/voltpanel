//! API key persistence and capability scope enforcement.
use crate::auth;
use crate::capability::Capability;
use crate::db::Db;
use crate::models::{self, User};
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::str::FromStr;

/// Effective permission set of an API key. `wildcard` (stored as `"*"`) grants
/// every capability; empty `server_ids` grants every server the owner can see.
#[derive(Debug, Clone)]
pub struct KeyScope {
    pub capabilities: Vec<Capability>,
    pub wildcard: bool,
    pub server_ids: Vec<i64>,
}

impl KeyScope {
    pub fn allows(&self, server_id: i64, cap: Capability) -> bool {
        let cap_ok = self.wildcard || self.capabilities.contains(&cap);
        let server_ok = self.server_ids.is_empty() || self.server_ids.contains(&server_id);
        cap_ok && server_ok
    }
}

/// API-surface view of a key. Never contains the token or its hash.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyInfo {
    pub id: i64,
    pub name: String,
    pub capabilities: Vec<String>,
    pub server_ids: Vec<i64>,
    pub expires_at: Option<String>,
    pub revoked: bool,
    pub created_at: String,
    pub last_used: Option<String>,
}

/// Parse the stored JSON columns into a scope. Unknown capability names are
/// dropped (they grant nothing); malformed JSON degrades to an empty scope,
/// i.e. deny-by-default. `["*"]` — the legacy full-access encoding — becomes
/// the wildcard.
fn parse_scope(capabilities_json: &str, server_ids_json: &str) -> KeyScope {
    let names: Vec<String> = serde_json::from_str(capabilities_json).unwrap_or_default();
    let wildcard = names.iter().any(|n| n == "*");
    let capabilities = names
        .iter()
        .filter_map(|n| Capability::from_str(n).ok())
        .collect();
    let server_ids: Vec<i64> = serde_json::from_str(server_ids_json).unwrap_or_default();
    KeyScope {
        capabilities,
        wildcard,
        server_ids,
    }
}

fn parse_names(capabilities_json: &str) -> Vec<String> {
    serde_json::from_str(capabilities_json).unwrap_or_default()
}

/// Create a key. Empty `capabilities`/`server_ids` default to today's full
/// access (wildcard, all servers). Returns `(id, raw token)` — the raw token
/// is shown exactly once to the caller; only its hash is stored.
pub fn create(
    db: &Db,
    user_id: i64,
    name: &str,
    capabilities: &[String],
    server_ids: &[i64],
    ttl_days: Option<i64>,
) -> Result<(i64, String)> {
    let wildcard = capabilities.is_empty() || capabilities.iter().any(|c| c == "*");
    for c in capabilities {
        if c != "*" && Capability::from_str(c).is_err() {
            bail!("unknown capability: {c}");
        }
    }
    let raw = format!("vp_{}", auth::random_token(32));
    let caps_json = if wildcard {
        "[\"*\"]".to_string()
    } else {
        serde_json::to_string(capabilities)?
    };
    let servers_json = serde_json::to_string(server_ids)?;
    let expires_at = ttl_days.map(|d| (Utc::now() + Duration::days(d)).to_rfc3339());
    let conn = db.lock();
    conn.execute(
        "INSERT INTO api_keys(user_id,token,name,capabilities,server_ids,expires_at,created_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![
            user_id,
            auth::hash_token(&raw),
            name,
            caps_json,
            servers_json,
            expires_at,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok((conn.last_insert_rowid(), raw))
}

/// List a user's keys, newest first. Never exposes the token or its hash.
pub fn list(db: &Db, user_id: i64) -> Result<Vec<ApiKeyInfo>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT id,name,capabilities,server_ids,expires_at,revoked,created_at,last_used \
         FROM api_keys WHERE user_id=?1 ORDER BY id DESC",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(ApiKeyInfo {
            id: r.get(0)?,
            name: r.get(1)?,
            capabilities: parse_names(&r.get::<_, String>(2)?),
            server_ids: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            expires_at: r.get(4)?,
            revoked: r.get::<_, i64>(5)? != 0,
            created_at: r.get(6)?,
            last_used: r.get(7)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Soft-revoke: keep the row for audit, stop authenticating it.
pub fn revoke(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("UPDATE api_keys SET revoked=1 WHERE id=?1", [id])?;
    Ok(())
}

/// Hard-delete a key row.
pub fn delete(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM api_keys WHERE id=?1", [id])?;
    Ok(())
}

/// Authenticate a raw bearer token. Returns the owning user plus the key's
/// scope, or `None` when the token is unknown, revoked, expired, or the owner
/// is gone/disabled. Touches `last_used` on success.
pub fn authenticate(db: &Db, raw: &str) -> Result<Option<(User, KeyScope)>> {
    let hash = auth::hash_token(raw);
    let row: Option<(i64, i64, Option<String>, i64, String, String)> = db
        .lock()
        .query_row(
            "SELECT id,user_id,expires_at,revoked,capabilities,server_ids \
             FROM api_keys WHERE token=?1",
            [hash],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((key_id, user_id, expires_at, revoked, caps_json, servers_json)) = row else {
        return Ok(None);
    };
    if revoked != 0 {
        return Ok(None);
    }
    if let Some(exp) = expires_at {
        if let Ok(exp) = DateTime::parse_from_rfc3339(&exp) {
            if exp.with_timezone(&Utc) <= Utc::now() {
                return Ok(None);
            }
        }
    }
    let user = models::get_user(db, user_id)?;
    if !user.active {
        return Ok(None);
    }
    models::touch_api_key(db, key_id)?;
    Ok(Some((user, parse_scope(&caps_json, &servers_json))))
}

/// A key can only narrow its owner's rights: both the key's scope and the
/// owner's effective grant must allow the action.
pub fn enforce(db: &Db, user: &User, scope: &KeyScope, server_id: i64, cap: Capability) -> bool {
    scope.allows(server_id, cap)
        && models::user_has_capability(db, user, server_id, cap).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(caps: Vec<Capability>, wildcard: bool, servers: Vec<i64>) -> KeyScope {
        KeyScope {
            capabilities: caps,
            wildcard,
            server_ids: servers,
        }
    }

    #[test]
    fn wildcard_grants_any_capability() {
        let s = scope(vec![], true, vec![]);
        assert!(s.allows(7, Capability::ControlKill));
        assert!(s.allows(7, Capability::StartupSecrets));
    }

    #[test]
    fn explicit_list_rejects_unlisted() {
        let s = scope(
            vec![Capability::FilesRead, Capability::ConsoleRead],
            false,
            vec![],
        );
        assert!(s.allows(1, Capability::FilesRead));
        assert!(!s.allows(1, Capability::FilesWrite));
        assert!(!s.allows(1, Capability::ControlStart));
    }

    #[test]
    fn empty_server_ids_allow_any_server() {
        let s = scope(vec![Capability::FilesRead], false, vec![]);
        assert!(s.allows(42, Capability::FilesRead));
        assert!(s.allows(999, Capability::FilesRead));
    }

    #[test]
    fn populated_server_ids_reject_outsiders() {
        let s = scope(vec![Capability::FilesRead], false, vec![1, 2, 3]);
        assert!(s.allows(2, Capability::FilesRead));
        assert!(!s.allows(4, Capability::FilesRead));
    }

    #[test]
    fn legacy_wildcard_row_stays_full_access() {
        let s = parse_scope(r#"["*"]"#, "[]");
        assert!(s.wildcard);
        assert!(s.allows(5, Capability::StartupSecrets));
    }

    #[test]
    fn unknown_names_are_dropped_not_granted() {
        let s = parse_scope(r#"["files.read","nope.bogus"]"#, "[7]");
        assert!(!s.wildcard);
        assert!(s.allows(7, Capability::FilesRead));
        assert!(!s.allows(7, Capability::ControlStart));
        assert!(!s.allows(8, Capability::FilesRead));
    }
}
