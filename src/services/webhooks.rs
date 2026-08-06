//! Webhook event bus: signed deliveries with retry and backoff.

use crate::auth;
use crate::db::Db;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// Event names the bus understands; subscriptions may also use `"*"` or a
/// group wildcard such as `"server.*"`.
pub const EVENTS: &[&str] = &[
    "server.start",
    "server.stop",
    "server.crash",
    "server.install",
    "backup.complete",
    "backup.failed",
    "schedule.run",
    "site.updated",
];

/// Synthetic event used by the admin "test webhook" action.
pub const TEST_EVENT: &str = "test.ping";

/// Delivery attempts before a webhook is marked failed.
const MAX_ATTEMPTS: i64 = 5;
/// Base backoff for rescheduling failed deliveries; doubles per attempt.
const BACKOFF_BASE_S: i64 = 30;
/// Per-request timeout when POSTing a delivery.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

const WH_COLS: &str = "id, uuid, name, url, secret, events, server_id, enabled, \
                       failure_count, last_status, created_at, updated_at";

#[derive(Serialize, Debug, Clone)]
pub struct Webhook {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub url: String,
    #[serde(skip_serializing)]
    pub secret: String,
    pub events: Vec<String>,
    pub server_id: Option<i64>,
    pub enabled: bool,
    pub failure_count: i64,
    pub last_status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Serialize, Debug)]
pub struct Delivery {
    pub id: i64,
    pub webhook_id: i64,
    pub event: String,
    pub payload: Value,
    pub attempt: i64,
    pub status: String,
    pub response_code: Option<i64>,
    pub error: String,
    pub next_attempt_at: Option<i64>,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

/// Partial update for a webhook; `None` leaves a field untouched.
#[derive(Default)]
pub struct WebhookPatch<'a> {
    pub name: Option<&'a str>,
    pub url: Option<&'a str>,
    pub events: Option<&'a [String]>,
    pub secret: Option<&'a str>,
    pub server_id: Option<Option<i64>>,
    pub enabled: Option<bool>,
}

fn row_to_webhook(r: &rusqlite::Row) -> rusqlite::Result<Webhook> {
    Ok(Webhook {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        url: r.get(3)?,
        secret: r.get(4)?,
        events: serde_json::from_str(&r.get::<_, String>(5)?)
            .unwrap_or_else(|_| vec!["*".to_string()]),
        server_id: r.get(6)?,
        enabled: r.get::<_, i64>(7)? != 0,
        failure_count: r.get(8)?,
        last_status: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

/// Does a subscription `pattern` fire for `event`? Exact, `"*"`, or `"group.*"`.
pub fn event_matches(pattern: &str, event: &str) -> bool {
    pattern == "*"
        || pattern == event
        || (pattern.ends_with(".*") && event.starts_with(&pattern[..pattern.len() - 1]))
}

/// Reject subscription lists that reference unknown event names.
pub fn validate_events(events: &[String]) -> Result<()> {
    for e in events {
        let known = EVENTS.contains(&e.as_str())
            || e == "*"
            || EVENTS
                .iter()
                .any(|name| e.ends_with(".*") && name.starts_with(&e[..e.len() - 1]));
        if !known {
            bail!("unknown webhook event: {e}");
        }
    }
    Ok(())
}

/// HMAC-SHA256 over `"{ts}.{body}"`, hex-encoded.
pub fn sign(secret: &str, body: &str, ts: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{ts}.{body}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn create(
    db: &Db,
    name: &str,
    url: &str,
    events: &[String],
    server_id: Option<i64>,
) -> Result<Webhook> {
    validate_events(events)?;
    let conn = db.lock();
    let now = Utc::now().to_rfc3339();
    let uuid = uuid::Uuid::new_v4().to_string();
    let secret = auth::random_token(32);
    let events_json = serde_json::to_string(events)?;
    conn.execute(
        "INSERT INTO webhooks (uuid, name, url, secret, events, server_id, enabled, \
         failure_count, last_status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 0, '', ?7, ?7)",
        params![uuid, name, url, secret, events_json, server_id, now],
    )?;
    let id = conn.last_insert_rowid();
    drop(conn);
    get(db, id)
}

pub fn list(db: &Db) -> Result<Vec<Webhook>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(&format!("SELECT {WH_COLS} FROM webhooks ORDER BY name"))?;
    let rows = stmt.query_map([], row_to_webhook)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn get(db: &Db, id: i64) -> Result<Webhook> {
    let conn = db.lock();
    conn.query_row(&format!("SELECT {WH_COLS} FROM webhooks WHERE id=?1"), [id], row_to_webhook)
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("webhook not found"),
            other => other.into(),
        })
}

pub fn update(db: &Db, id: i64, patch: WebhookPatch) -> Result<Webhook> {
    let conn = db.lock();
    let wh = conn
        .query_row(&format!("SELECT {WH_COLS} FROM webhooks WHERE id=?1"), [id], row_to_webhook)
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("webhook not found"),
            other => other.into(),
        })?;
    if let Some(events) = patch.events {
        validate_events(events)?;
    }
    let name = patch.name.unwrap_or(&wh.name);
    let url = patch.url.unwrap_or(&wh.url);
    let events: Vec<String> = patch.events.map(|e| e.to_vec()).unwrap_or_else(|| wh.events.clone());
    let secret = patch.secret.map(str::to_string).unwrap_or_else(|| wh.secret.clone());
    let server_id = patch.server_id.unwrap_or(wh.server_id);
    let enabled = patch.enabled.unwrap_or(wh.enabled);
    let events_json = serde_json::to_string(&events)?;
    conn.execute(
        "UPDATE webhooks SET name=?1, url=?2, events=?3, secret=?4, server_id=?5, \
         enabled=?6, updated_at=?7 WHERE id=?8",
        params![name, url, events_json, secret, server_id, enabled as i64, Utc::now().to_rfc3339(), id],
    )?;
    drop(conn);
    get(db, id)
}

pub fn delete(db: &Db, id: i64) -> Result<()> {
    let conn = db.lock();
    let n = conn.execute("DELETE FROM webhooks WHERE id=?1", [id])?;
    if n == 0 {
        bail!("webhook not found");
    }
    Ok(())
}

pub fn set_enabled(db: &Db, id: i64, enabled: bool) -> Result<Webhook> {
    {
        let conn = db.lock();
        conn.execute(
            "UPDATE webhooks SET enabled=?1, updated_at=?2 WHERE id=?3",
            params![enabled as i64, Utc::now().to_rfc3339(), id],
        )?;
    }
    get(db, id)
}

/// Enqueue one `pending` delivery per enabled subscription matching `event`.
/// A webhook with `server_id = NULL` is global; a scoped one only fires for
/// its server. Synchronous and write-only — never performs HTTP.
pub fn emit(db: &Db, event: &str, server_id: Option<i64>, payload: Value) -> usize {
    let payload = payload.to_string();
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let next_at = now.timestamp();
    let conn = db.lock();
    let mut insert = match conn.prepare(
        "INSERT INTO webhook_deliveries (webhook_id, event, payload, attempt, status, \
         next_attempt_at, created_at) VALUES (?1, ?2, ?3, 0, 'pending', ?4, ?5)",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("webhook emit: {e}");
            return 0;
        }
    };
    let mut sel = match conn.prepare("SELECT id, events, server_id FROM webhooks WHERE enabled=1") {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("webhook emit: {e}");
            return 0;
        }
    };
    let rows = match sel.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
    }) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("webhook emit: {e}");
            return 0;
        }
    };
    let mut enqueued = 0;
    for row in rows.flatten() {
        let (wid, events_json, scope) = row;
        if scope.is_some() && scope != server_id {
            continue;
        }
        let patterns: Vec<String> = serde_json::from_str(&events_json).unwrap_or_default();
        if !patterns.iter().any(|p| event_matches(p, event)) {
            continue;
        }
        if insert.execute(params![wid, event, payload, next_at, created_at]).is_ok() {
            enqueued += 1;
        }
    }
    enqueued
}

/// Queue a delivery for one specific webhook, bypassing subscription matching.
/// Used by the admin "test webhook" action, where the operator has already
/// chosen the target and the synthetic event is subscribed to by nobody.
pub fn enqueue_one(db: &Db, webhook_id: i64, event: &str, payload: Value) -> Result<()> {
    let now = Utc::now();
    let conn = db.lock();
    conn.execute(
        "INSERT INTO webhook_deliveries (webhook_id, event, payload, attempt, status, \
         next_attempt_at, created_at) VALUES (?1, ?2, ?3, 0, 'pending', ?4, ?5)",
        params![webhook_id, event, payload.to_string(), now.timestamp(), now.to_rfc3339()],
    )?;
    Ok(())
}

enum DeliveryOutcome {
    Delivered { code: i64 },
    Failed { code: Option<i64>, error: String },
}

/// Claim up to `limit` due `pending` deliveries, POST each with the signature
/// headers and a short timeout, then record the result: `delivered` on 2xx,
/// rescheduled with exponential backoff otherwise, `failed` past the attempt
/// cap. The DB lock is only held around the claim and the result write,
/// never across the HTTP call.
pub async fn dispatch_due(db: &Db, client: &reqwest::Client, limit: usize) -> usize {
    let now = Utc::now().timestamp();
    let claimed: Vec<(i64, i64, String, String, i64)> = {
        let conn = db.lock();
        let mut stmt = match conn.prepare(
            "SELECT id, webhook_id, event, payload, attempt FROM webhook_deliveries \
             WHERE status='pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1) \
             ORDER BY id LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("webhook dispatch: {e}");
                return 0;
            }
        };
        let rows = match stmt.query_map(params![now, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        }) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("webhook dispatch: {e}");
                return 0;
            }
        };
        rows.flatten().collect()
    };
    if claimed.is_empty() {
        return 0;
    }

    // Snapshot endpoints so the result write needs no webhook lookup.
    let endpoints: HashMap<i64, (String, String)> = {
        let conn = db.lock();
        let mut stmt = match conn.prepare("SELECT id, url, secret FROM webhooks WHERE enabled=1") {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("webhook dispatch: {e}");
                return 0;
            }
        };
        let rows = match stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        }) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("webhook dispatch: {e}");
                return 0;
            }
        };
        rows.flatten().map(|(id, url, secret)| (id, (url, secret))).collect()
    };

    let mut processed = 0usize;
    for (delivery_id, webhook_id, event, payload, attempt) in claimed {
        let outcome = match endpoints.get(&webhook_id) {
            Some((url, secret)) => {
                let ts = Utc::now().timestamp();
                let sig = sign(secret, &payload, ts);
                let resp = client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .header("X-VoltPanel-Event", &event)
                    .header("X-VoltPanel-Delivery", delivery_id.to_string())
                    .header("X-VoltPanel-Timestamp", ts.to_string())
                    .header("X-VoltPanel-Signature", format!("sha256={sig}"))
                    .timeout(HTTP_TIMEOUT)
                    .body(payload)
                    .send()
                    .await;
                match resp {
                    Ok(r) => {
                        let code = r.status().as_u16() as i64;
                        if (200..300).contains(&code) {
                            DeliveryOutcome::Delivered { code }
                        } else {
                            DeliveryOutcome::Failed {
                                code: Some(code),
                                error: format!("HTTP {code}"),
                            }
                        }
                    }
                    Err(e) => DeliveryOutcome::Failed {
                        code: None,
                        error: e.to_string(),
                    },
                }
            }
            None => DeliveryOutcome::Failed {
                code: None,
                error: "webhook deleted or disabled".to_string(),
            },
        };

        let new_attempt = attempt + 1;
        let backoff_s = BACKOFF_BASE_S << attempt.min(MAX_ATTEMPTS - 1) as u32;
        let gave_up = new_attempt >= MAX_ATTEMPTS;
        let conn = db.lock();
        match outcome {
            DeliveryOutcome::Delivered { code } => {
                let _ = conn.execute(
                    "UPDATE webhook_deliveries SET status='delivered', response_code=?1, \
                     error='', delivered_at=?2 WHERE id=?3",
                    params![code, Utc::now().to_rfc3339(), delivery_id],
                );
                let _ = conn.execute(
                    "UPDATE webhooks SET last_status=?1 WHERE id=?2",
                    params![code.to_string(), webhook_id],
                );
            }
            DeliveryOutcome::Failed { code, error } => {
                if gave_up {
                    let _ = conn.execute(
                        "UPDATE webhook_deliveries SET status='failed', attempt=?1, \
                         response_code=?2, error=?3, delivered_at=?4 WHERE id=?5",
                        params![new_attempt, code, error, Utc::now().to_rfc3339(), delivery_id],
                    );
                } else {
                    let next = Utc::now().timestamp() + backoff_s;
                    let _ = conn.execute(
                        "UPDATE webhook_deliveries SET status='pending', attempt=?1, \
                         response_code=?2, error=?3, next_attempt_at=?4 WHERE id=?5",
                        params![new_attempt, code, error, next, delivery_id],
                    );
                }
                let _ = conn.execute(
                    "UPDATE webhooks SET failure_count=failure_count+1, last_status=?1 WHERE id=?2",
                    params![error, webhook_id],
                );
            }
        }
        processed += 1;
    }
    processed
}

pub fn deliveries(db: &Db, webhook_id: i64, limit: i64) -> Result<Vec<Delivery>> {
    let conn = db.lock();
    let mut stmt = conn.prepare(
        "SELECT id, webhook_id, event, payload, attempt, status, response_code, error, \
         next_attempt_at, created_at, delivered_at \
         FROM webhook_deliveries WHERE webhook_id=?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![webhook_id, limit], |r| {
        Ok(Delivery {
            id: r.get(0)?,
            webhook_id: r.get(1)?,
            event: r.get(2)?,
            payload: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
            attempt: r.get(4)?,
            status: r.get(5)?,
            response_code: r.get(6)?,
            error: r.get(7)?,
            next_attempt_at: r.get(8)?,
            created_at: r.get(9)?,
            delivered_at: r.get(10)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::{event_matches, sign, validate_events};

    #[test]
    fn event_matches_exact_star_and_group() {
        assert!(event_matches("server.start", "server.start"));
        assert!(event_matches("*", "backup.complete"));
        assert!(event_matches("server.*", "server.crash"));
        assert!(event_matches("server.*", "server.install"));
        assert!(event_matches("backup.*", "backup.failed"));
        assert!(!event_matches("server.*", "backup.complete"));
        assert!(!event_matches("server.*", "server"));
        assert!(!event_matches("site.updated", "server.start"));
        assert!(!event_matches("server.start", "server.stop"));
        assert!(!event_matches("server.*", "site.updated"));
    }

    #[test]
    fn sign_stable_and_sensitive() {
        let body = r#"{"event":"server.start","server_id":3}"#;
        let ts = 1_700_000_000;
        let a = sign("s3cret", body, ts);
        let b = sign("s3cret", body, ts);
        assert_eq!(a, b, "same inputs must produce the same signature");
        assert_eq!(a.len(), 64, "sha256 hex is 64 chars");
        let c = sign("s3cret", r#"{"event":"server.stop","server_id":3}"#, ts);
        assert_ne!(a, c, "changed body must change the signature");
        let d = sign("s3cret", body, ts + 1);
        assert_ne!(a, d, "changed timestamp must change the signature");
    }

    #[test]
    fn validate_events_accepts_known_and_rejects_unknown() {
        assert!(validate_events(&["server.*".to_string()]).is_ok());
        assert!(validate_events(&["*".to_string(), "backup.failed".to_string()]).is_ok());
        assert!(validate_events(&["bogus.event".to_string()]).is_err());
        let mixed = ["server.*", "nope.*"].iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(validate_events(&mixed).is_err());
    }
}
