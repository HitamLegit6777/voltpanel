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

/// Upper bound on a key's lifetime in days. TTL must be positive and within
/// this ceiling so a misbehaving caller can't mint keys that never expire.
pub const MAX_TTL_DAYS: i64 = 3650;

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

    /// A key may act with full authority (e.g. on admin endpoints, which are
    /// not server-scoped) only when it grants every capability and is not
    /// restricted to specific servers. Any explicit capability list or server
    /// restriction keeps the key scoped.
    pub fn is_full_authority(&self) -> bool {
        self.wildcard && self.server_ids.is_empty()
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
/// dropped (they grant nothing). Malformed JSON fails closed: it yields
/// `Err`, so a corrupted key cannot authenticate and can never broaden
/// access. `["*"]` — the legacy full-array encoding — becomes the wildcard.
fn parse_scope(capabilities_json: &str, server_ids_json: &str) -> Result<KeyScope> {
    let names: Vec<String> = serde_json::from_str(capabilities_json)?;
    let wildcard = names.iter().any(|n| n == "*");
    let capabilities = names
        .iter()
        .filter_map(|n| Capability::from_str(n).ok())
        .collect();
    let server_ids: Vec<i64> = serde_json::from_str(server_ids_json)?;
    Ok(KeyScope {
        capabilities,
        wildcard,
        server_ids,
    })
}

fn parse_names(capabilities_json: &str) -> Vec<String> {
    serde_json::from_str(capabilities_json).unwrap_or_default()
}

/// Create a key. Requires at least one explicit capability — an empty list is
/// rejected rather than defaulting to full access. `ttl_days` must be a
/// positive integer no larger than [`MAX_TTL_DAYS`]. Returns `(id, raw token)`
/// — the raw token is shown exactly once to the caller; only its hash is stored.
pub fn create(
    db: &Db,
    user_id: i64,
    name: &str,
    capabilities: &[String],
    server_ids: &[i64],
    ttl_days: Option<i64>,
) -> Result<(i64, String)> {
    if capabilities.is_empty() {
        bail!("at least one capability is required");
    }
    let wildcard = capabilities.iter().any(|c| c == "*");
    for c in capabilities {
        if c != "*" && Capability::from_str(c).is_err() {
            bail!("unknown capability: {c}");
        }
    }
    if let Some(days) = ttl_days {
        if days <= 0 {
            bail!("ttl_days must be positive");
        }
        if days > MAX_TTL_DAYS {
            bail!("ttl_days exceeds the {MAX_TTL_DAYS}-day maximum");
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
    let conn = db.get()?;
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
    let conn = db.get()?;
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
    let conn = db.get()?;
    conn.execute("UPDATE api_keys SET revoked=1 WHERE id=?1", [id])?;
    Ok(())
}

/// Hard-delete a key row.
pub fn delete(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    conn.execute("DELETE FROM api_keys WHERE id=?1", [id])?;
    Ok(())
}

/// Authenticate a raw bearer token. Returns the owning user plus the key's
/// scope, or `None` when the token is unknown, revoked, expired, or the owner
/// is gone/disabled. Touches `last_used` on success.
pub fn authenticate(db: &Db, raw: &str) -> Result<Option<(User, KeyScope)>> {
    let hash = auth::hash_token(raw);
    let row: Option<(i64, i64, Option<String>, i64, String, String)> = db
        .get()?
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
        let exp = match DateTime::parse_from_rfc3339(&exp) {
            Ok(exp) => exp,
            // An unparseable expiry is untrustworthy: accept nothing rather
            // than risk letting a corrupted key outlive its intended window.
            Err(_) => return Ok(None),
        };
        if exp.with_timezone(&Utc) <= Utc::now() {
            return Ok(None);
        }
    }
    let user = models::get_user(db, user_id)?;
    if !user.active {
        return Ok(None);
    }
    models::touch_api_key(db, key_id)?;
    let scope = match parse_scope(&caps_json, &servers_json) {
        Ok(scope) => scope,
        // Corrupted capability/server JSON must never yield a broadened grant;
        // deny the key outright.
        Err(_) => return Ok(None),
    };
    Ok(Some((user, scope)))
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
        let s = parse_scope(r#"["*"]"#, "[]").unwrap();
        assert!(s.wildcard);
        assert!(s.allows(5, Capability::StartupSecrets));
    }

    #[test]
    fn unknown_names_are_dropped_not_granted() {
        let s = parse_scope(r#"["files.read","nope.bogus"]"#, "[7]").unwrap();
        assert!(!s.wildcard);
        assert!(s.allows(7, Capability::FilesRead));
        assert!(!s.allows(7, Capability::ControlStart));
        assert!(!s.allows(8, Capability::FilesRead));
    }

    #[test]
    fn malformed_capabilities_json_fails_closed() {
        assert!(parse_scope("not-json", "[]").is_err());
        // A valid array that is not a string array is also malformed input.
        assert!(parse_scope("[1,2,3]", "[]").is_err());
    }

    #[test]
    fn malformed_server_ids_json_fails_closed() {
        // A corrupted server list must never degrade to "all servers".
        assert!(parse_scope(r#"["*"]"#, "{oops").is_err());
        assert!(parse_scope(r#"["files.read"]"#, r#"["1","2"]"#).is_err());
    }

    #[test]
    fn empty_capabilities_are_rejected() {
        let db = test_db();
        assert!(create(&db, 1, "k", &[], &[], None).is_err());
    }

    #[test]
    fn ttl_must_be_positive_and_bounded() {
        let db = test_db();
        let caps = vec!["files.read".to_string()];
        assert!(create(&db, 1, "k", &caps, &[], Some(0)).is_err());
        assert!(create(&db, 1, "k", &caps, &[], Some(-7)).is_err());
        assert!(create(&db, 1, "k", &caps, &[], Some(MAX_TTL_DAYS + 1)).is_err());
        assert!(create(&db, 1, "k", &caps, &[], Some(MAX_TTL_DAYS)).is_ok());
    }

    #[test]
    fn explicit_wildcard_still_grants_full_access() {
        let db = test_db();
        let (_, raw) = create(&db, 1, "k", &["*".to_string()], &[], None).unwrap();
        let (_, scope) = authenticate(&db, &raw).unwrap().unwrap();
        assert!(scope.wildcard);
        assert!(scope.allows(9, Capability::StartupSecrets));
    }

    #[test]
    fn malformed_expiry_rejects_the_key() {
        let db = test_db();
        let (id, raw) = create(&db, 1, "k", &["files.read".to_string()], &[], None).unwrap();
        let conn = db.get().unwrap();
        conn.execute(
            "UPDATE api_keys SET expires_at=?1 WHERE id=?2",
            rusqlite::params!["not-a-date", id],
        )
        .unwrap();
        drop(conn);
        assert!(authenticate(&db, &raw).unwrap().is_none());
    }

    #[test]
    fn corrupted_stored_scope_rejects_the_key() {
        let db = test_db();
        let (id, raw) = create(&db, 1, "k", &["files.read".to_string()], &[], None).unwrap();
        let conn = db.get().unwrap();
        conn.execute(
            "UPDATE api_keys SET server_ids=?1 WHERE id=?2",
            rusqlite::params!["{corrupt", id],
        )
        .unwrap();
        drop(conn);
        assert!(authenticate(&db, &raw).unwrap().is_none());
    }

    #[test]
    fn expired_key_is_rejected() {
        let db = test_db();
        let (id, raw) = create(&db, 1, "k", &["files.read".to_string()], &[], None).unwrap();
        let conn = db.get().unwrap();
        conn.execute(
            "UPDATE api_keys SET expires_at=?1 WHERE id=?2",
            rusqlite::params![(Utc::now() - Duration::seconds(60)).to_rfc3339(), id],
        )
        .unwrap();
        drop(conn);
        assert!(authenticate(&db, &raw).unwrap().is_none());
    }

    #[test]
    fn revoked_key_is_rejected() {
        let db = test_db();
        let (id, raw) = create(&db, 1, "k", &["files.read".to_string()], &[], None).unwrap();
        let (_, raw2) = create(&db, 1, "k2", &["files.read".to_string()], &[], None).unwrap();
        assert!(authenticate(&db, &raw).unwrap().is_some());
        revoke(&db, id).unwrap();
        assert!(authenticate(&db, &raw).unwrap().is_none());
        // Revoking one key must not affect a sibling key.
        assert!(authenticate(&db, &raw2).unwrap().is_some());
    }

    #[test]
    fn scoped_server_ids_are_honoured_end_to_end() {
        let db = test_db();
        let (_, raw) = create(&db, 1, "k", &["files.read".to_string()], &[1, 2], None).unwrap();
        let (_, scope) = authenticate(&db, &raw).unwrap().unwrap();
        assert!(scope.allows(1, Capability::FilesRead));
        assert!(scope.allows(2, Capability::FilesRead));
        assert!(!scope.allows(3, Capability::FilesRead));
        assert!(!scope.allows(3, Capability::ControlStart));
    }

    #[test]
    fn raw_token_is_stored_hashed_not_plaintext() {
        let db = test_db();
        let (id, raw) = create(&db, 1, "k", &["files.read".to_string()], &[], None).unwrap();
        let stored: String = db
            .get().unwrap()
            .query_row("SELECT token FROM api_keys WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, raw, "plaintext token must never be persisted");
        assert_eq!(stored, auth::hash_token(&raw));
    }

    #[test]
    fn wildcard_key_with_server_ids_is_still_server_restricted() {
        let db = test_db();
        let (_, raw) = create(&db, 1, "k", &["*".to_string()], &[7], None).unwrap();
        let (_, scope) = authenticate(&db, &raw).unwrap().unwrap();
        assert!(scope.wildcard);
        assert!(scope.allows(7, Capability::ControlKill));
        assert!(!scope.allows(8, Capability::ControlKill));
        assert!(!scope.is_full_authority());
    }

    #[test]
    fn full_authority_requires_wildcard_and_unrestricted_servers() {
        assert!(scope(vec![], true, vec![]).is_full_authority());
        assert!(!scope(vec![], true, vec![1]).is_full_authority());
        assert!(!scope(vec![Capability::FilesRead], false, vec![]).is_full_authority());
        assert!(!scope(vec![], false, vec![]).is_full_authority());
    }

    #[test]
    fn enforce_never_widens_the_owner_grant() {
        let db = test_db();
        // User 1 holds an owner-level grant on servers 42 and 43.
        let now = Utc::now().to_rfc3339();
        let conn = db.get().unwrap();
        conn.execute(
            "INSERT INTO blueprints(uuid,name,created_at,updated_at) VALUES('bp-42','bp',?1,?1)",
            [&now],
        )
        .unwrap();
        for (id, uuid) in [(42, "srv-42"), (43, "srv-43")] {
            conn.execute(
                "INSERT INTO servers(id,uuid,name,user_id,blueprint_id,created_at,updated_at) \
                 VALUES(?1,?2,?2,1,1,?3,?3)",
                rusqlite::params![id, uuid, &now],
            )
            .unwrap();
        }
        drop(conn);
        models::add_subuser(&db, 42, 1, &crate::capability::Grant::owner()).unwrap();
        models::add_subuser(&db, 43, 1, &crate::capability::Grant::owner()).unwrap();
        let user = models::get_user(&db, 1).unwrap();
        assert!(models::user_has_capability(&db, &user, 42, Capability::ControlKill).unwrap());

        let (_, raw) = create(&db, 1, "k", &["files.read".to_string()], &[42], None).unwrap();
        let (mut scoped, scope) = authenticate(&db, &raw).unwrap().unwrap();
        scoped.key_scope = Some(scope.clone());
        // Inside the key's scope the grant is preserved…
        assert!(enforce(&db, &scoped, &scope, 42, Capability::FilesRead));
        // …but capabilities outside the key's list stay denied…
        assert!(!enforce(&db, &scoped, &scope, 42, Capability::ControlKill));
        // …and servers outside the key's server_ids are denied outright,
        // even though the owner grant would otherwise cover them.
        assert!(!enforce(&db, &scoped, &scope, 43, Capability::FilesRead));
    }

    #[test]
    fn server_access_is_narrowed_by_key_scope() {
        let db = test_db();
        let (_, raw) = create(&db, 1, "k", &["files.read".to_string()], &[42], None).unwrap();
        let (mut user, scope) = authenticate(&db, &raw).unwrap().unwrap();
        user.key_scope = Some(scope);
        // Even a root admin behind a scoped key cannot reach out-of-scope servers.
        user.root_admin = true;
        assert!(models::user_has_server_access(&db, &user, 42).unwrap());
        assert!(!models::user_has_server_access(&db, &user, 43).unwrap());
        // Without a key scope the same admin is unrestricted again.
        user.key_scope = None;
        assert!(models::user_has_server_access(&db, &user, 43).unwrap());
    }

    fn test_db() -> Db {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "voltpanel-keys-test-{}-{}.db",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let db = crate::db::open(path.to_str().unwrap()).unwrap();
        let now = Utc::now().to_rfc3339();
        db.get().unwrap()
            .execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at) \
                 VALUES('u','u@example.com','x',?1,?1)",
                [now],
            )
            .unwrap();
        db
    }
}