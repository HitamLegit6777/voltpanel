//! Website / static hosting service: serve static files for a server,
//! resolve the server's directory for a domain.
use crate::config::Config;
use crate::db::Db;
use crate::models::Website;
use anyhow::Result;
use std::path::PathBuf;

/// Find website record by host header.
pub fn find_by_host(db: &Db, host: &str) -> Result<Option<Website>> {
    let host = host.trim().to_lowercase();
    let host = host
        .split(':')
        .next()
        .unwrap_or(&host)
        .to_string();
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
    cfg.paths.website_dir
        .join(format!("server_{server_id}"))
        .join(w.root_dir.trim_start_matches('/'))
}
