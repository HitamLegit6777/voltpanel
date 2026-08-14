//! Website / static hosting service: serve static files for a server,
//! resolve the server's directory for a domain.
use crate::config::Config;
use crate::db::Db;
use crate::models::{self, Website};
use crate::services::files;
use crate::services::webhooks;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
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

/// Normalize a Host header: trim, lowercase, strip a trailing root dot and
/// any port; bracketed IPv6 literals are kept intact.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_lowercase();
    let host = match h.find(']') {
        // Bracketed IPv6 (`[::1]:8080`): drop the port after `]`.
        Some(end) => h[..=end].to_string(),
        None => {
            // `host:port` has at most one colon; a bare IPv6 literal has more.
            if h.matches(':').count() <= 1 {
                h.split(':').next().unwrap_or(&h).to_string()
            } else {
                h
            }
        }
    };
    host.strip_suffix('.').unwrap_or(&host).to_string()
}

/// Validate a site domain and return the lowercased canonical form:
/// optional leading `*.` wildcard, then hostname labels of `[a-z0-9-]` with
/// no leading/trailing dash; whole name 1..=253 chars, labels <= 63.
///
/// A wildcard's body must span at least two labels, so `*.example.com` is
/// accepted but a bare-TLD wildcard like `*.com` is rejected: `*.com` would
/// match every `.com` host the gateway sees, hijacking hosts from any tenant
/// (the cross-tenant wildcard finding). Without a public-suffix list this
/// label count is the conservative proxy for "covers a registrable domain" —
/// a two-label body that is itself a public suffix (`*.co.uk`) still passes;
/// rejecting that class needs a PSL dependency, deliberately not added.
pub fn validate_hostname(raw: &str) -> Result<String> {
    let domain = raw.trim().to_lowercase();
    let body = domain.strip_prefix("*.").unwrap_or(&domain);
    if body.is_empty() {
        bail!("domain must not be empty");
    }
    if domain.len() > 253 {
        bail!("domain must be at most 253 characters");
    }
    if domain.starts_with("*.") && body.split('.').count() < 2 {
        bail!(
            "wildcard domains must cover at least two labels after '*.' \
             (e.g. *.example.com, not *.com)"
        );
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

/// Validate a reverse-proxy target and return the normalized
/// `http(s)://host:port` form. An explicit port is required; userinfo, a
/// path, a query, and a fragment are rejected so the persisted target is
/// always exactly scheme, host, and port.
pub fn validate_upstream(raw: &str) -> Result<String> {
    let u =
        Url::parse(raw).map_err(|_| anyhow!("upstream must be a valid http(s)://host:port URL"))?;
    if u.scheme() != "http" && u.scheme() != "https" {
        bail!("upstream must use http or https");
    }
    let host = match u.host_str() {
        Some(h) => h,
        None => bail!("upstream must include a host"),
    };
    if !u.username().is_empty() || u.password().is_some() {
        bail!("upstream must not include userinfo");
    }
    if !u.path().is_empty() && u.path() != "/" {
        bail!("upstream must not include a path");
    }
    if u.query().is_some() || u.fragment().is_some() {
        bail!("upstream must not include a query or fragment");
    }
    // An explicit port is required. The url crate strips default ports during
    // parse (`https://example.com:443` yields port() == None), so the raw
    // authority is inspected instead: it must end in `:<digits>` right after
    // the host.
    let authority = raw
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(raw)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, hp)| hp)
        .unwrap_or(&authority);
    let port = host_port
        .strip_prefix(host)
        .and_then(|rest| {
            let digits = rest.strip_prefix(':')?;
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            digits.parse::<u16>().ok().filter(|&p| p != 0)
        })
        .ok_or_else(|| anyhow!("upstream must include an explicit port"))?;
    Ok(format!("{}://{host}:{port}", u.scheme()))
}

/// Canonicalize `p`, resolving the deepest existing ancestor so the check
/// works before the directory exists. Both sides of a containment compare go
/// through this, so a relative `website_dir` never mixes with an absolute
/// canonical path.
fn canonicalize_deepest(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut prefix = p;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while let Some(parent) = prefix.parent() {
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        if let Some(name) = prefix.file_name() {
            tail.push(name.to_os_string());
        }
        prefix = parent;
    }
    p.to_path_buf()
}

/// Validate that root_dir stays inside the site's own directory
/// (`website_dir/server_<id>`). The lexical pass (safe_join) works before the
/// directory exists; the canonicalize pass rejects symlink escapes in
/// existing trees.
pub fn validate_root_dir(cfg: &Config, server_id: i64, raw: &str) -> Result<()> {
    let rel = raw.trim_matches('/');
    files::safe_join(Path::new("/voltpanel-site-root"), rel)
        .map_err(|_| anyhow!("root_dir must stay inside the website directory"))?;
    let server_dir = cfg.paths.website_dir.join(format!("server_{server_id}"));
    let canon = canonicalize_deepest(&server_dir.join(rel));
    let server_canon = canonicalize_deepest(&server_dir);
    if !canon.starts_with(&server_canon) {
        bail!("root_dir escapes the server's website directory");
    }
    Ok(())
}

/// Cross-field rules on a complete site record.
fn validate_site(cfg: &Config, s: &Site) -> Result<()> {
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
    validate_root_dir(cfg, s.server_id, &s.root_dir)?;
    if let Some(p) = s.port {
        if !(1..=65535).contains(&p) {
            bail!("port must be between 1 and 65535");
        }
    }
    if s.force_https && !s.ssl {
        bail!("force_https requires ssl to be enabled");
    }
    if s.ssl {
        if s.domain.starts_with("*.") {
            bail!("ssl cannot be enabled on a wildcard domain (no HTTP-01 challenge)");
        }
        if s.domain.parse::<std::net::IpAddr>().is_ok() {
            bail!("ssl cannot be enabled on an IP-address domain (no HTTP-01 challenge)");
        }
    }
    Ok(())
}

/// Translate a SQLite UNIQUE violation into a clean duplicate-domain error.
fn map_unique(err: rusqlite::Error) -> anyhow::Error {
    match &err {
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::ConstraintViolation
                && e.extended_code == 2067 =>
        {
            anyhow!("domain already in use")
        }
        _ => err.into(),
    }
}

pub fn list(db: &Db, server_id: i64) -> Result<Vec<Site>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM websites WHERE server_id=?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map([server_id], site_from_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn get(db: &Db, server_id: i64, id: i64) -> Result<Option<Site>> {
    let conn = db.get()?;
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


/// Enqueue a `site.updated` event after a vhost config change (best-effort,
/// fire and forget): the server identity, the site identity, and what
/// changed. Payloads stay far under the 64 KiB emit cap.
fn emit_site_event(db: &Db, server_id: i64, operation: &str, site: &Site, timestamp: &str) {
    let srv = models::get_server(db, server_id).ok();
    let payload = json!({
        "event": "site.updated",
        "server_id": server_id,
        "uuid": srv.as_ref().map(|s| s.uuid.clone()),
        "server_name": srv.as_ref().map(|s| s.name.clone()),
        "operation": operation,
        "site_id": site.id,
        "domain": site.domain,
        "enabled": site.enabled,
        "timestamp": timestamp,
    });
    webhooks::emit(db, "site.updated", Some(server_id), payload);
}

pub fn create(db: &Db, cfg: &Config, server_id: i64, input: &SiteInput) -> Result<Site> {
    let mut upstream = input.upstream.clone().unwrap_or_default();
    if !upstream.is_empty() {
        upstream = validate_upstream(&upstream)?;
    }
    let site = Site {
        id: 0,
        server_id,
        domain: validate_hostname(&input.domain)?,
        root_dir: input.root_dir.clone().unwrap_or_else(|| "/".into()),
        port: input.port,
        proxy_type: input.proxy_type.clone().unwrap_or_else(|| "static".into()),
        upstream,
        ssl: input.ssl.unwrap_or(false),
        force_https: input.force_https.unwrap_or(false),
        enabled: input.enabled.unwrap_or(true),
        created_at: String::new(),
        updated_at: String::new(),
    };
    validate_site(cfg, &site)?;
    let now = Utc::now().to_rfc3339();
    let conn = db.get()?;
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
    let site = get_inner(&conn, server_id, id)?.ok_or_else(|| anyhow!("site not found"))?;
    emit_site_event(db, server_id, "create", &site, &now);
    Ok(site)
}
/// on this server.
pub fn update(
    db: &Db,
    cfg: &Config,
    server_id: i64,
    id: i64,
    input: &SiteInput,
) -> Result<Option<Site>> {
    // BEGIN IMMEDIATE serializes the read-modify-write so a concurrent PATCH
    // cannot overwrite this update with a stale snapshot.
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut site = match get_inner(&tx, server_id, id)? {
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
    if !site.upstream.is_empty() {
        site.upstream = validate_upstream(&site.upstream)?;
    }
    validate_site(cfg, &site)?;
    let now = Utc::now().to_rfc3339();
    tx.execute(
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
    let updated = get_inner(&tx, server_id, id)?;
    tx.commit()?;
    if let Some(site) = &updated {
        emit_site_event(db, server_id, "update", site, &now);
    }
    Ok(updated)
}

pub fn set_enabled(db: &Db, server_id: i64, id: i64, enabled: bool) -> Result<Option<Site>> {
    let conn = db.get()?;
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE websites SET enabled=?1, updated_at=?2 WHERE id=?3 AND server_id=?4",
        params![enabled as i64, now, id, server_id],
    )?;
    if n == 0 {
        return Ok(None);
    }
    let site = get_inner(&conn, server_id, id)?;
    if let Some(s) = &site {
        emit_site_event(
            db,
            server_id,
            if s.enabled { "enable" } else { "disable" },
            s,
            &now,
        );
    }
    Ok(site)
}

pub fn delete(db: &Db, cfg: &Config, server_id: i64, id: i64) -> Result<bool> {
    let conn = db.get()?;
    let site = get_inner(&conn, server_id, id)?;
    let n = conn.execute(
        "DELETE FROM websites WHERE id=?1 AND server_id=?2",
        params![id, server_id],
    )?;
    if n > 0 {
        if let Some(s) = site {
            emit_site_event(db, server_id, "delete", &s, &Utc::now().to_rfc3339());
            remove_site_files(cfg, &s);
        }
    }
    Ok(n > 0)
}

/// Best-effort removal of a deleted site's root directory, scoped under
/// `website_dir`. The server's own directory (a shared `/` root) is never
/// removed: it may hold other sites' files.
fn remove_site_files(cfg: &Config, s: &Site) {
    let rel = s.root_dir.trim_start_matches('/');
    if rel.is_empty() {
        tracing::warn!("site {} has a shared root; files left in place", s.id);
        return;
    }
    let server_dir = cfg.paths.website_dir.join(format!("server_{}", s.server_id));
    let dir = server_dir.join(rel);
    let canon = canonicalize_deepest(&dir);
    let server_canon = canonicalize_deepest(&server_dir);
    if !canon.starts_with(&server_canon) {
        tracing::warn!(
            "site {} root {} escapes the server directory; not removed",
            s.id,
            dir.display()
        );
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove site {} root {}: {e}",
                s.id,
                dir.display()
            );
        }
    }
}

/// Resolve a Host header to an enabled site of `server_id`: exact domain
/// first, then the longest matching `*.suffix` wildcard. Wildcards are
/// scoped to the owning server, so one tenant's `*.com` can never hijack
/// another tenant's unmatched hosts.
pub fn resolve(db: &Db, server_id: i64, host: &str) -> Result<Option<Site>> {
    let host = normalize_host(host);
    let conn = db.get()?;
    let sql = format!(
        "SELECT {COLUMNS} FROM websites WHERE enabled=1 AND server_id=?1 AND domain=?2"
    );
    if let Some(s) = conn
        .query_row(&sql, params![server_id, host], site_from_row)
        .optional()?
    {
        return Ok(Some(s));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM websites WHERE enabled=1 AND server_id=?1 AND domain LIKE '*.%'"
    ))?;
    let rows = stmt.query_map([server_id], site_from_row)?;
    let wildcards = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(match_host(&wildcards, &host).cloned())
}

/// Pure host matcher: exact domain beats wildcard; among wildcards the
/// longest matching suffix wins. Wildcards only match at a real dot
/// boundary, so `badexample.com` never matches `*.example.com`.
pub fn match_host<'a>(sites: &'a [Site], host: &str) -> Option<&'a Site> {
    let host = normalize_host(host);
    if let Some(s) = sites.iter().find(|s| s.domain == host) {
        return Some(s);
    }
    let mut suffix = host.as_str();
    while let Some(i) = suffix.find('.') {
        let label = &suffix[i + 1..];
        // Dot boundary: the label must be a real dotted suffix of the host
        // (`host.ends_with(".<label>")`), never a bare string suffix.
        if host.ends_with(&format!(".{label}")) {
            let wild = format!("*.{label}");
            if let Some(s) = sites.iter().find(|s| s.domain == wild) {
                return Some(s);
            }
        }
        suffix = label;
    }
    None
}

/// Resolve the on-disk root for a website.
pub fn root_for(cfg: &Config, server_id: i64, w: &Website) -> Result<PathBuf> {
    root_for_dir(cfg, server_id, &w.root_dir)
}


/// Resolve a Host header to an enabled site across ALL servers — the
/// host-routing gateway's scope. Domains (including wildcards) are globally
/// unique (`idx_websites_domain`), so an exact match is unambiguous; the
/// same dot-boundary longest-suffix matcher applies to wildcards.
pub fn resolve_host(db: &Db, host: &str) -> Result<Option<Site>> {
    let host = normalize_host(host);
    let conn = db.get()?;
    if let Some(s) = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM websites WHERE enabled=1 AND domain=?1"),
            params![host],
            site_from_row,
        )
        .optional()?
    {
        return Ok(Some(s));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM websites WHERE enabled=1 AND domain LIKE '*.%'"
    ))?;
    let rows = stmt.query_map([], site_from_row)?;
    let wildcards = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(match_host(&wildcards, &host).cloned())
}

/// Resolve the on-disk root for a site's `root_dir`, re-validating that it
/// stays inside `website_dir/server_<id>` (lexical pass + symlink-escape
/// pass). Used by both the site API and the host-routing gateway.
pub fn root_for_dir(cfg: &Config, server_id: i64, root_dir: &str) -> Result<PathBuf> {
    validate_root_dir(cfg, server_id, root_dir)?;
    Ok(cfg
        .paths
        .website_dir
        .join(format!("server_{server_id}"))
        .join(root_dir.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(domain: &str) -> Site {
        Site {
            id: 1,
            server_id: 1,
            domain: domain.to_string(),
            root_dir: "assets".into(),
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
    fn wildcard_requires_two_label_body() {
        // A wildcard must cover at least two labels: *.example.com is fine,
        // but *.com would match every .com host the gateway sees.
        for ok in [
            "*.example.com",
            "*.sub.example.com",
            // Two-label body ending in a public suffix passes by design —
            // distinguishing it from *.example.com needs a PSL (documented
            // in validate_hostname).
            "*.co.uk",
            "*.co.jp",
        ] {
            assert!(validate_hostname(ok).is_ok(), "{ok} should be accepted");
        }
        for bad in ["*.com", "*.net", "*.org", "*.io", "*.uk", "*.co"] {
            assert!(
                validate_hostname(bad).is_err(),
                "{bad:?} must be rejected (would hijack the whole TLD)"
            );
        }
        // Non-wildcard single-label hosts stay valid (e.g. an intranet name).
        assert!(validate_hostname("intranet").is_ok());
    }

    #[test]
    fn upstream_accepts_parseable_targets() {
        for ok in [
            "http://localhost:3000",
            "https://example.com:443",
            "http://127.0.0.1:8080",
        ] {
            assert!(validate_upstream(ok).is_ok(), "{ok} should be accepted");
        }
        // normalized to canonical scheme://host:port
        assert_eq!(
            validate_upstream("HTTP://EXAMPLE.com:8080").unwrap(),
            "http://example.com:8080"
        );
    }

    #[test]
    fn upstream_rejects_bad_targets() {
        for bad in [
            "example.com:3000",
            "ftp://example.com:21",
            "http://",
            "not a url",
            "http://example.com:99999",
            "http://example.com",
            "https://example.com",
            "http://u:p@h:8080",
            "http://h:8080/x",
            "http://h:8080/?q=1",
            "http://h:8080/#f",
        ] {
            assert!(
                validate_upstream(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn root_dir_must_stay_contained() {
        let cfg = Config::default();
        for ok in ["assets", "public/index.html", "a/b/c", ""] {
            assert!(
                validate_root_dir(&cfg, 1, ok).is_ok(),
                "{ok:?} should be accepted"
            );
        }
        for bad in ["../etc", "/../etc", "a/../../b", ".."] {
            assert!(
                validate_root_dir(&cfg, 1, bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn root_dir_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let website_dir = tmp.path().join("websites");
        let server_dir = website_dir.join("server_1");
        std::fs::create_dir_all(&server_dir).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, server_dir.join("link")).unwrap();

        let mut cfg = Config::default();
        cfg.paths.website_dir = website_dir;
        // a symlink pointing outside the server directory is rejected
        assert!(validate_root_dir(&cfg, 1, "link").is_err());
        // a real subdirectory is fine
        std::fs::create_dir_all(server_dir.join("assets")).unwrap();
        assert!(validate_root_dir(&cfg, 1, "assets").is_ok());
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

    #[test]
    fn match_host_requires_a_dot_boundary() {
        let sites = vec![site("*.example.com")];
        // shares the suffix but not a label boundary
        assert!(match_host(&sites, "badexample.com").is_none());
        // a wildcard needs at least one subdomain label
        assert!(match_host(&sites, "example.com").is_none());
        assert_eq!(
            match_host(&sites, "sub.example.com").unwrap().domain,
            "*.example.com"
        );
        // longest matching label still wins
        let nested = vec![site("*.com"), site("*.badexample.com")];
        assert_eq!(
            match_host(&nested, "a.badexample.com").unwrap().domain,
            "*.badexample.com"
        );
    }

    #[test]
    fn normalize_host_handles_ports_ipv6_and_root_dot() {
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
        assert_eq!(normalize_host(" example.com "), "example.com");
        assert_eq!(normalize_host("example.com."), "example.com");
        assert_eq!(normalize_host("[::1]:8443"), "[::1]");
        assert_eq!(normalize_host("[2001:db8::1]"), "[2001:db8::1]");
        assert_eq!(normalize_host("2001:db8::1"), "2001:db8::1");
        assert_eq!(normalize_host("127.0.0.1:8080"), "127.0.0.1");
    }

    #[test]
    fn validate_site_cross_field_rules() {
        let cfg = Config::default();

        // force_https requires ssl
        let mut s = site("example.com");
        s.force_https = true;
        assert!(validate_site(&cfg, &s).is_err());

        // ssl refuses wildcard and IP-address domains (no HTTP-01 challenge)
        let mut s = site("example.com");
        s.ssl = true;
        s.domain = "*.example.com".into();
        assert!(validate_site(&cfg, &s).is_err());
        let mut s = site("example.com");
        s.ssl = true;
        s.domain = "127.0.0.1".into();
        assert!(validate_site(&cfg, &s).is_err());

        // ssl + force_https on a normal domain is fine
        let mut s = site("example.com");
        s.ssl = true;
        s.force_https = true;
        assert!(validate_site(&cfg, &s).is_ok());

        // port must be in 1..=65535
        for bad in [0, -1, 65536] {
            let mut s = site("example.com");
            s.port = Some(bad);
            assert!(validate_site(&cfg, &s).is_err(), "port {bad} rejected");
        }
        let mut s = site("example.com");
        s.port = Some(8080);
        assert!(validate_site(&cfg, &s).is_ok());

        // proxy sites require a valid upstream
        let mut s = site("example.com");
        s.proxy_type = "proxy".into();
        assert!(validate_site(&cfg, &s).is_err());
        s.upstream = "http://127.0.0.1:8080".into();
        assert!(validate_site(&cfg, &s).is_ok());
    }

    struct TestDb {
        db: Db,
        path: std::path::PathBuf,
        sid: i64,
        uid: i64,
        bid: i64,
    }

    impl TestDb {
        fn new() -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voltpanel-websites-test-{}-{}.db",
                std::process::id(),
                seq
            ));
            let _ = std::fs::remove_file(&path);
            let db = crate::db::open(path.to_str().unwrap()).unwrap();
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at)
                 VALUES('t','t@t','x','now','now')",
                [],
            )
            .unwrap();
            let uid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO blueprints(uuid,name,created_at,updated_at)
                 VALUES('b','b','now','now')",
                [],
            )
            .unwrap();
            let bid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s','s',?1,?2,'now','now')",
                rusqlite::params![uid, bid],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            drop(conn);
            TestDb { db, path, sid, uid, bid }
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    fn insert_website(conn: &rusqlite::Connection, server_id: i64, domain: &str) -> i64 {
        conn.execute(
            "INSERT INTO websites(server_id,domain,enabled,created_at,updated_at)
             VALUES(?1,?2,1,'now','now')",
            rusqlite::params![server_id, domain],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn resolve_scopes_wildcards_to_the_owning_server() {
        let t = TestDb::new();
        let srv2: i64 = {
            let conn = t.db.get().unwrap();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s2','srv2',?1,?2,'now','now')",
                rusqlite::params![t.uid, t.bid],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            insert_website(&conn, t.sid, "*.example.com");
            insert_website(&conn, id, "*.com");
            id
        };

        // wildcards resolve within the owning server
        assert_eq!(
            resolve(&t.db, t.sid, "www.example.com").unwrap().unwrap().domain,
            "*.example.com"
        );
        assert_eq!(
            resolve(&t.db, srv2, "www.example.com").unwrap().unwrap().domain,
            "*.com"
        );
        // tenant isolation: server 1 must not see server 2's `*.com`
        assert!(resolve(&t.db, t.sid, "unrelated.com").unwrap().is_none());
        assert_eq!(
            resolve(&t.db, srv2, "unrelated.com").unwrap().unwrap().domain,
            "*.com"
        );
        // exact match beats wildcard
        {
            let conn = t.db.get().unwrap();
            insert_website(&conn, t.sid, "www.example.com");
        }
        assert_eq!(
            resolve(&t.db, t.sid, "www.example.com").unwrap().unwrap().domain,
            "www.example.com"
        );
    }

    #[test]
    fn resolve_host_matches_across_all_servers() {
        let t = TestDb::new();
        let srv2: i64 = {
            let conn = t.db.get().unwrap();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s2','srv2',?1,?2,'now','now')",
                rusqlite::params![t.uid, t.bid],
            )
            .unwrap();
            let id = conn.last_insert_rowid();
            insert_website(&conn, id, "*.example.com");
            id
        };

        // the gateway scope crosses server boundaries
        assert_eq!(
            resolve_host(&t.db, "www.example.com").unwrap().unwrap().domain,
            "*.example.com"
        );
        assert_eq!(
            resolve_host(&t.db, "www.example.com").unwrap().unwrap().server_id,
            srv2
        );
        // exact beats wildcard regardless of owning server
        {
            let conn = t.db.get().unwrap();
            insert_website(&conn, t.sid, "www.example.com");
        }
        assert_eq!(
            resolve_host(&t.db, "www.example.com").unwrap().unwrap().server_id,
            t.sid
        );
        // disabled sites and unknown hosts resolve to None
        {
            let conn = t.db.get().unwrap();
            conn.execute(
                "UPDATE websites SET enabled=0 WHERE domain='*.example.com'",
                [],
            )
            .unwrap();
        }
        assert!(resolve_host(&t.db, "other.example.com").unwrap().is_none());
        assert!(resolve_host(&t.db, "nope.example.net").unwrap().is_none());
    }

    #[test]
    fn delete_removes_orphaned_site_files() {
        let t = TestDb::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.website_dir = tmp.path().to_path_buf();
        let server_dir = cfg.paths.website_dir.join(format!("server_{}", t.sid));
        std::fs::create_dir_all(server_dir.join("assets")).unwrap();

        let site_id = {
            let conn = t.db.get().unwrap();
            conn.execute(
                "INSERT INTO websites(server_id,domain,root_dir,enabled,created_at,updated_at)
                 VALUES(?1,'site.example.com','assets',1,'now','now')",
                [t.sid],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        assert!(delete(&t.db, &cfg, t.sid, site_id).unwrap());
        assert!(!server_dir.join("assets").exists());
        // a second delete reports no row
        assert!(!delete(&t.db, &cfg, t.sid, site_id).unwrap());
    }

    #[test]
    fn delete_leaves_shared_root_untouched() {
        let t = TestDb::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.paths.website_dir = tmp.path().to_path_buf();
        let server_dir = cfg.paths.website_dir.join(format!("server_{}", t.sid));
        std::fs::create_dir_all(server_dir.join("other-site")).unwrap();

        let site_id = {
            let conn = t.db.get().unwrap();
            conn.execute(
                "INSERT INTO websites(server_id,domain,root_dir,enabled,created_at,updated_at)
                 VALUES(?1,'root.example.com','/',1,'now','now')",
                [t.sid],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        assert!(delete(&t.db, &cfg, t.sid, site_id).unwrap());
        // the server directory itself (shared root) is never removed
        assert!(server_dir.join("other-site").exists());
    }

    #[test]
    fn create_enqueues_site_updated_webhook_delivery() {
        let t = TestDb::new();
        {
            let conn = t.db.get().unwrap();
            conn.execute(
                "INSERT INTO webhooks(uuid,name,url,secret,events,server_id,enabled,created_at,updated_at)
                 VALUES('wh-uuid','wh','https://hooks.example/x','0123456789abcdef','[\"site.updated\"]',?1,1,'now','now')",
                [t.sid],
            )
            .unwrap();
        }
        let input = SiteInput {
            domain: "example.com".into(),
            root_dir: Some("assets".into()),
            port: None,
            proxy_type: None,
            upstream: None,
            ssl: None,
            force_https: None,
            enabled: None,
        };
        let site = create(&t.db, &Config::default(), t.sid, &input).unwrap();
        assert_eq!(site.domain, "example.com");

        let conn = t.db.get().unwrap();
        let (event, payload): (String, String) = conn
            .query_row(
                "SELECT event, payload FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event, "site.updated");
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["server_id"], t.sid);
        assert_eq!(v["uuid"], "s");
        assert_eq!(v["operation"], "create");
        assert_eq!(v["site_id"], site.id);
        assert_eq!(v["domain"], "example.com");
        assert_eq!(v["enabled"], true);
    }
}