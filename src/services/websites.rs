//! Website / static hosting service: serve static files for a server,
//! resolve the server's directory for a domain.
use crate::config::Config;
use crate::db::Db;
use crate::models::Website;
use crate::services::files;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use url::Url;

const COLUMNS: &str = "id,server_id,domain,root_dir,port,proxy_type,upstream,ssl,force_https,enabled,created_at,updated_at";

#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub id: i64,
    pub server_id: i64,
    pub domain: String,
    pub root_dir: String,
    pub port: Option<i64>,
    pub proxy_type: String,
    pub upstream: String,
    pub ssl: bool,
    pub force_https: bool,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SiteInput {
    pub domain: String,
    pub root_dir: Option<String>,
    pub port: Option<i64>,
    pub proxy_type: Option<String>,
    pub upstream: Option<String>,
    pub ssl: Option<bool>,
    pub force_https: Option<bool>,
    pub enabled: Option<bool>,
}

fn site_from_row(r: &rusqlite::Row) -> rusqlite::Result<Site> {
    Ok(Site {
        id: r.get(0)?,
        server_id: r.get(1)?,
        domain: r.get(2)?,
        root_dir: r.get(3)?,
        port: r.get(4)?,
        proxy_type: r.get(5)?,
        upstream: r.get(6)?,
        ssl: r.get::<_, i64>(7)? != 0,
        force_https: r.get::<_, i64>(8)? != 0,
        enabled: r.get::<_, i64>(9)? != 0,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

/// Normalize a Host header: trim, lowercase, strip any port.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_lowercase();
    h.split(':').next().unwrap_or(&h).to_string()
}

/// Validate a site domain and return the lowercased canonical form:
/// optional leading `*.` wildcard, then hostname labels of `[a-z0-9-]` with
/// no leading/trailing dash; whole name 1..=253 chars, labels <= 63.
pub fn validate_hostname(raw: &str) -> Result<String> {
    let domain = raw.trim().to_lowercase();
    let body = domain.strip_prefix("*.").unwrap_or(&domain);
    if body.is_empty() {
        bail!("domain must not be empty");
    }
    if domain.len() > 253 {
        bail!("domain must be at most 253 characters");
    }
    for label in body.split('.') {
        if label.is_empty() {
            bail!("domain labels may not be empty");
        }
        if label.len() > 63 {
            bail!("domain labels may be at most 63 characters");
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("domain labels may only contain letters, digits, and dashes");
        }
        if label.starts_with('-') || label.ends_with('-') {
            bail!("domain labels may not start or end with a dash");
        }
    }
    Ok(domain)
}

/// Validate a reverse-proxy target: parseable `http(s)://host:port`.
pub fn validate_upstream(raw: &str) -> Result<()> {
    let u =
        Url::parse(raw).map_err(|_| anyhow!("upstream must be a valid http(s)://host:port URL"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        bail!("upstream must use http or https");
    }
    if u.host_str().is_none() {
        bail!("upstream must include a host");
    }
    if u.port_or_known_default().is_none() {
        bail!("upstream must include a port");
    }
    Ok(())
}

/// Validate that root_dir stays inside the site directory. Reuses the
/// containment helper from files.rs; it is lexical, so the directory does
/// not need to exist yet.
pub fn validate_root_dir(raw: &str) -> Result<()> {
    let rel = raw.trim_matches('/');
    files::safe_join(Path::new("/voltpanel-site-root"), rel)
        .map_err(|_| anyhow!("root_dir must stay inside the workspace directory"))?;
    Ok(())
}

/// Cross-field rules on a complete site record.
fn validate_site(s: &Site) -> Result<()> {
    if s.proxy_type != "static" && s.proxy_type != "proxy" {
        bail!("proxy_type must be one of: static, proxy");
    }
    if s.proxy_type == "proxy" {
        if s.upstream.is_empty() {
            bail!("proxy sites require an upstream http(s)://host:port URL");
        }
    } else if s.root_dir.trim_matches('/').is_empty() {
        bail!("static sites require a root_dir inside the workspace");
    }
    if !s.upstream.is_empty() {
        validate_upstream(&s.upstream)?;
    }
    validate_root_dir(&s.root_dir)?;
    Ok(())
}

/// Translate a SQLite UNIQUE violation into a clean duplicate-domain error.
fn map_unique(err: rusqlite::Error) -> anyhow::Error {
    match &err {
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::ConstraintViolation
                && (e.extended_code == 2067 || e.extended_code == 0) =>
        {
            anyhow!("domain already in use")
        }
        _ => err.into(),
    }
}

pub fn list(db: &Db, server_id: i64) -> Result<Vec<Site>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM websites WHERE server_id=?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map([server_id], site_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn get(db: &Db, server_id: i64, id: i64) -> Result<Option<Site>> {
    let conn = db.lock();
    get_inner(&conn, server_id, id)
}

fn get_inner(conn: &rusqlite::Connection, server_id: i64, id: i64) -> Result<Option<Site>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM websites WHERE id=?1 AND server_id=?2"),
        params![id, server_id],
        site_from_row,
    )
    .optional()
    .map_err(Into::into)
}

pub fn create(db: &Db, server_id: i64, input: &SiteInput) -> Result<Site> {
    let site = Site {
        id: 0,
        server_id,
        domain: validate_hostname(&input.domain)?,
        root_dir: input.root_dir.clone().unwrap_or_else(|| "/".into()),
        port: input.port,
        proxy_type: input.proxy_type.clone().unwrap_or_else(|| "static".into()),
        upstream: input.upstream.clone().unwrap_or_default(),
        ssl: input.ssl.unwrap_or(false),
        force_https: input.force_https.unwrap_or(false),
        enabled: input.enabled.unwrap_or(true),
        created_at: String::new(),
        updated_at: String::new(),
    };
    validate_site(&site)?;
    let now = Utc::now().to_rfc3339();
    let conn = db.lock();
    conn.execute(
        "INSERT INTO websites(server_id,domain,root_dir,port,proxy_type,upstream,ssl,force_https,enabled,created_at,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)",
        params![
            site.server_id,
            site.domain,
            site.root_dir,
            site.port,
            site.proxy_type,
            site.upstream,
            site.ssl as i64,
            site.force_https as i64,
            site.enabled as i64,
            now
        ],
    )
    .map_err(map_unique)?;
    let id = conn.last_insert_rowid();
    get_inner(&conn, server_id, id)?.ok_or_else(|| anyhow!("site not found"))
}

/// Partial update: only provided fields change; None result = no such site
/// on this server.
pub fn update(db: &Db, server_id: i64, id: i64, input: &SiteInput) -> Result<Option<Site>> {
    let conn = db.lock();
    let mut site = match get_inner(&conn, server_id, id)? {
        Some(s) => s,
        None => return Ok(None),
    };
    site.domain = validate_hostname(&input.domain)?;
    if let Some(r) = &input.root_dir {
        site.root_dir = r.clone();
    }
    if let Some(p) = input.port {
        site.port = Some(p);
    }
    if let Some(t) = &input.proxy_type {
        site.proxy_type = t.clone();
    }
    if let Some(u) = &input.upstream {
        site.upstream = u.clone();
    }
    if let Some(v) = input.ssl {
        site.ssl = v;
    }
    if let Some(v) = input.force_https {
        site.force_https = v;
    }
    if let Some(v) = input.enabled {
        site.enabled = v;
    }
    validate_site(&site)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE websites SET domain=?1, root_dir=?2, port=?3, proxy_type=?4, upstream=?5, ssl=?6, force_https=?7, enabled=?8, updated_at=?9 WHERE id=?10 AND server_id=?11",
        params![
            site.domain,
            site.root_dir,
            site.port,
            site.proxy_type,
            site.upstream,
            site.ssl as i64,
            site.force_https as i64,
            site.enabled as i64,
            now,
            id,
            server_id
        ],
    )
    .map_err(map_unique)?;
    get_inner(&conn, server_id, id)
}

pub fn set_enabled(db: &Db, server_id: i64, id: i64, enabled: bool) -> Result<Option<Site>> {
    let conn = db.lock();
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE websites SET enabled=?1, updated_at=?2 WHERE id=?3 AND server_id=?4",
        params![enabled as i64, now, id, server_id],
    )?;
    if n == 0 {
        return Ok(None);
    }
    get_inner(&conn, server_id, id)
}

pub fn delete(db: &Db, server_id: i64, id: i64) -> Result<bool> {
    let conn = db.lock();
    let n = conn.execute(
        "DELETE FROM websites WHERE id=?1 AND server_id=?2",
        params![id, server_id],
    )?;
    Ok(n > 0)
}

/// Resolve a Host header to an enabled site: exact domain first, then the
/// longest matching `*.suffix` wildcard.
pub fn resolve(db: &Db, host: &str) -> Result<Option<Site>> {
    let host = normalize_host(host);
    let conn = db.lock();
    let sql = format!("SELECT {COLUMNS} FROM websites WHERE enabled=1 AND domain=?1");
    if let Some(s) = conn.query_row(&sql, [&host], site_from_row).optional()? {
        return Ok(Some(s));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM websites WHERE enabled=1 AND domain LIKE '*.%'"
    ))?;
    let rows = stmt.query_map([], site_from_row)?;
    let wildcards = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(match_host(&wildcards, &host).cloned())
}

/// Pure host matcher: exact domain beats wildcard; among wildcards the
/// longest matching suffix wins.
pub fn match_host<'a>(sites: &'a [Site], host: &str) -> Option<&'a Site> {
    let host = normalize_host(host);
    if let Some(s) = sites.iter().find(|s| s.domain == host) {
        return Some(s);
    }
    let mut suffix = host.as_str();
    while let Some(i) = suffix.find('.') {
        suffix = &suffix[i + 1..];
        let wild = format!("*.{suffix}");
        if let Some(s) = sites.iter().find(|s| s.domain == wild) {
            return Some(s);
        }
    }
    None
}

/// Find website record by host header.
pub fn find_by_host(db: &Db, host: &str) -> Result<Option<Website>> {
    let host = host.trim().to_lowercase();
    let host = host.split(':').next().unwrap_or(&host).to_string();
    let conn = db.lock();
    let mut stmt = conn.prepare("SELECT id,server_id,domain,root_dir,port,proxy_type,ssl,enabled,created_at FROM websites WHERE domain=?1 AND enabled=1")?;
    let mut rows = stmt.query_map([&host], |r| {
        Ok(Website {
            id: r.get(0)?,
            server_id: r.get(1)?,
            domain: r.get(2)?,
            root_dir: r.get(3)?,
            port: r.get(4)?,
            proxy_type: r.get(5)?,
            ssl: r.get::<_, i64>(6)? != 0,
            enabled: r.get::<_, i64>(7)? != 0,
            created_at: r.get(8)?,
        })
    })?;
    if let Some(w) = rows.next().transpose()? {
        return Ok(Some(w));
    }
    Ok(None)
}

/// Resolve the on-disk root for a website.
pub fn root_for(cfg: &Config, server_id: i64, w: &Website) -> PathBuf {
    cfg.paths
        .website_dir
        .join(format!("server_{server_id}"))
        .join(w.root_dir.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(domain: &str) -> Site {
        Site {
            id: 1,
            server_id: 1,
            domain: domain.to_string(),
            root_dir: "/".into(),
            port: None,
            proxy_type: "static".into(),
            upstream: String::new(),
            ssl: false,
            force_https: false,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn hostname_accepts_plausible_domains() {
        for ok in [
            "example.com",
            "sub.example.com",
            "*.example.com",
            "my-site-1.io",
            "a.b.c.d.e",
            "127.0.0.1",
        ] {
            assert!(validate_hostname(ok).is_ok(), "{ok} should be accepted");
        }
        // lowercased before storage
        assert_eq!(validate_hostname("ExAmPle.COM").unwrap(), "example.com");
        assert_eq!(validate_hostname("*.Example.com").unwrap(), "*.example.com");
    }

    #[test]
    fn hostname_rejects_invalid_domains() {
        for bad in [
            "",
            "*",
            "*.",
            "-example.com",
            "example-.com",
            "exa_mple.com",
            "exa mple.com",
            "example..com",
            ".example.com",
            "example.com.",
            "foo.*.com",
            "*.foo.*.com",
        ] {
            assert!(
                validate_hostname(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn hostname_rejects_too_long() {
        assert!(validate_hostname(&format!("{}.com", "a".repeat(250))).is_err());
        assert!(validate_hostname(&format!("*.{}", "a".repeat(252))).is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(63))).is_ok());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn upstream_accepts_parseable_targets() {
        for ok in [
            "http://localhost:3000",
            "https://example.com:443",
            "http://127.0.0.1:8080",
            "http://example.com",
        ] {
            assert!(validate_upstream(ok).is_ok(), "{ok} should be accepted");
        }
    }

    #[test]
    fn upstream_rejects_bad_targets() {
        for bad in [
            "example.com:3000",
            "ftp://example.com:21",
            "http://",
            "not a url",
            "http://example.com:99999",
        ] {
            assert!(
                validate_upstream(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn root_dir_must_stay_contained() {
        for ok in ["assets", "public/index.html", "a/b/c", ""] {
            assert!(validate_root_dir(ok).is_ok(), "{ok:?} should be accepted");
        }
        for bad in ["../etc", "/../etc", "a/../../b", ".."] {
            assert!(
                validate_root_dir(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn match_prefers_exact_then_longest_wildcard() {
        let sites = vec![
            site("*.example.com"),
            site("api.example.com"),
            site("*.com"),
        ];
        assert_eq!(
            match_host(&sites, "api.example.com").unwrap().domain,
            "api.example.com"
        );
        assert_eq!(
            match_host(&sites, "www.example.com").unwrap().domain,
            "*.example.com"
        );
        assert_eq!(match_host(&sites, "anything.com").unwrap().domain, "*.com");
        assert!(match_host(&sites, "other.net").is_none());
        assert_eq!(
            match_host(&sites, "API.EXAMPLE.COM").unwrap().domain,
            "api.example.com"
        );
    }
}
