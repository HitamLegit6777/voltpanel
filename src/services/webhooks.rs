//! Webhook event bus: signed deliveries with retry and backoff, plus an
//! in-memory recent-event registry the scheduler's signal gates observe.

use crate::auth;
use crate::db::Db;
use crate::services::proc::Notifier;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use aes_gcm::KeyInit as _;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use url::{Host, Url};
use futures::{stream, StreamExt};

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
    // Panel self-health: global-scope, edge-triggered (emitted only when an
    // alert kind transitions from inactive to active). `panel.*` matches.
    "panel.alert",
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
/// Slack added to the claim lease: covers scheduler sweeps so a live batch is
/// never re-claimed by a concurrent dispatcher, while a batch orphaned by a
/// crashed dispatcher becomes due again after the lease expires.
const CLAIM_LEASE_MARGIN_S: i64 = 60;
/// Extra seconds budgeted per delivery (beyond `HTTP_TIMEOUT`) for connect and
/// DNS while sizing the claim lease.
const CONNECT_MARGIN_S: i64 = 5;
/// Consecutive connection failures (DNS/connect/timeout — never 4xx, which is
/// the target's business logic) before the circuit breaker disables a webhook.
const BREAKER_THRESHOLD: i64 = 20;
/// Pending deliveries per webhook beyond which `emit` drops new events for it.
const MAX_PENDING_PER_WEBHOOK: i64 = 500;
/// Serialized payload size cap: oversized events are dropped at `emit` rather
/// than filling the deliveries table (and later the sender's memory) with
/// multi-megabyte rows.
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Minimum length for an operator-supplied webhook secret. A degenerate key
/// (empty, or a few characters) makes the HMAC trivially forgeable; the create
/// path auto-generates a 32-char token and is unaffected.
const MIN_SECRET_CHARS: usize = 16;

const WH_COLS: &str = "id, uuid, name, url, secret, events, server_id, enabled, \
                       allow_http, failure_count, last_status, created_at, updated_at";

// ---------------------------------------------------------------------------
// Webhook secrets at rest: `[security] webhook_master_key` enables AES-256-GCM
// encryption of the `webhooks.secret` column. With no key configured the
// legacy plaintext behavior is preserved (plus a one-time warning when any
// secret exists). With a key, every stored secret is `v1:<base64>` — version
// prefix, 96-bit per-row random nonce, GCM ciphertext+tag — so a changed or
// lost master key fails authentication on load instead of decrypting to
// garbage, and legacy plaintext rows are encrypted in place on first read.
// ---------------------------------------------------------------------------

/// Version prefix of the encrypted secret format.
const SECRET_PREFIX: &str = "v1:";
/// AES-GCM nonce length (96 bits).
const SECRET_NONCE_LEN: usize = 12;

/// Master key from `[security] webhook_master_key`; empty string disables
/// encryption entirely (the legacy plaintext behavior).
fn master_key() -> Option<&'static str> {
    let key = crate::SETTINGS.security.webhook_master_key.as_str();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// Fold an arbitrary-length master key into the fixed 256-bit AES key.
fn derive_secret_key(master: &str) -> [u8; 32] {
    Sha256::digest(master.as_bytes()).into()
}

/// Encrypt `plaintext` under `master`. The nonce is minted fresh per row, so
/// two encryptions of the same secret never collide at rest.
fn encrypt_secret(master: &str, plaintext: &str) -> Result<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derive_secret_key(master)));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow!("webhook secret encryption failed"))?;
    let mut blob = Vec::with_capacity(SECRET_NONCE_LEN + ct.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ct);
    Ok(format!("{SECRET_PREFIX}{}", STANDARD.encode(&blob)))
}

/// Decrypt a `v1:` secret. A wrong master key fails the authenticated GCM
/// tag check and surfaces loudly — never a silently empty value.
fn decrypt_secret(master: &str, stored: &str) -> Result<String> {
    let body = stored
        .strip_prefix(SECRET_PREFIX)
        .ok_or_else(|| anyhow!("webhook secret is missing the {SECRET_PREFIX} prefix"))?;
    let blob = STANDARD
        .decode(body)
        .map_err(|_| anyhow!("webhook secret blob is not valid base64"))?;
    if blob.len() < SECRET_NONCE_LEN + 16 {
        anyhow::bail!("webhook secret blob is truncated ({} bytes)", blob.len());
    }
    let (nonce, ct) = blob.split_at(SECRET_NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&derive_secret_key(master)));
    let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| {
        anyhow!(
            "webhook secret cannot be decrypted with the configured \
             webhook_master_key — the key changed or the row is corrupt"
        )
    })?;
    String::from_utf8(pt).map_err(|_| anyhow!("decrypted webhook secret is not valid UTF-8"))
}

/// One loud, once-per-process warning when secrets sit in plaintext because
/// no master key is configured. Fires on the first load that observes a
/// non-empty stored secret.
fn warn_plaintext_at_rest() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "webhook secrets are stored in PLAINTEXT: set [security] \
             webhook_master_key to encrypt webhook secrets at rest"
        );
    }
}

/// Resolve the stored secret of webhook `id` to the plaintext callers need.
/// - no key: pass through unchanged (legacy), warning once when a secret
///   exists;
/// - key + `v1:` row: decrypt; a mismatch fails loudly (key change);
/// - key + plaintext row: encrypt in place (lazy migration) and return the
///   plaintext.
fn secret_for_row(conn: &rusqlite::Connection, id: i64, raw: &str, key: Option<&str>) -> Result<String> {
    match key {
        None => {
            if !raw.is_empty() {
                warn_plaintext_at_rest();
            }
            Ok(raw.to_string())
        }
        Some(k) if raw.starts_with(SECRET_PREFIX) => decrypt_secret(k, raw),
        Some(k) => {
            let enc = encrypt_secret(k, raw)?;
            conn.execute(
                "UPDATE webhooks SET secret=?1, updated_at=?2 WHERE id=?3",
                params![enc, Utc::now().to_rfc3339(), id],
            )?;
            Ok(raw.to_string())
        }
    }
}

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
    pub allow_http: bool,
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
    pub allow_http: Option<bool>,
}

/// A webhooks row exactly as stored, before secret decryption. Kept separate
/// from [`Webhook`] so the at-rest secret can be decrypted (or lazily
/// upgraded) after the SELECT statement is done — the mapper never writes.
struct RawWebhook {
    id: i64,
    uuid: String,
    name: String,
    url: String,
    raw_secret: String,
    events: Vec<String>,
    server_id: Option<i64>,
    enabled: bool,
    allow_http: bool,
    failure_count: i64,
    last_status: String,
    created_at: String,
    updated_at: String,
}

fn row_to_raw(r: &rusqlite::Row) -> rusqlite::Result<RawWebhook> {
    Ok(RawWebhook {
        id: r.get(0)?,
        uuid: r.get(1)?,
        name: r.get(2)?,
        url: r.get(3)?,
        raw_secret: r.get(4)?,
        events: serde_json::from_str(&r.get::<_, String>(5)?)
            .unwrap_or_else(|_| vec!["*".to_string()]),
        server_id: r.get(6)?,
        enabled: r.get::<_, i64>(7)? != 0,
        allow_http: r.get::<_, i64>(8)? != 0,
        failure_count: r.get(9)?,
        last_status: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

/// Decrypt the raw secret (lazily migrating legacy plaintext rows) and build
/// a [`Webhook`]. An undecryptable secret — key changed or row corrupt —
/// fails loudly instead of silently loading an empty value.
fn raw_to_webhook(
    raw: RawWebhook,
    conn: &rusqlite::Connection,
    key: Option<&str>,
) -> Result<Webhook> {
    let secret = secret_for_row(conn, raw.id, &raw.raw_secret, key)?;
    Ok(Webhook {
        id: raw.id,
        uuid: raw.uuid,
        name: raw.name,
        url: raw.url,
        secret,
        events: raw.events,
        server_id: raw.server_id,
        enabled: raw.enabled,
        allow_http: raw.allow_http,
        failure_count: raw.failure_count,
        last_status: raw.last_status,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
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

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast()
                || a == 0
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19)))
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            if ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x2001 && segments[1] == 0)
                || (segments[0] == 0x2001 && segments[1] == 2)
                || (segments[0] == 0x0064 && segments[1] == 0xff9b)
            {
                return false;
            }
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            (segments[0] & 0xe000) == 0x2000
        }
    }
}

/// Strict validator: https only. Plain http requires the per-webhook
/// `allow_http` opt-in (see `validate_target_url_opts`).
pub fn validate_target_url(raw: &str) -> Result<Url> {
    validate_target_url_opts(raw, false)
}

fn validate_target_url_opts(raw: &str, allow_http: bool) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| anyhow!("webhook URL must be a valid URL"))?;
    let https = matches!(url.scheme(), "https");
    if !(https || (allow_http && url.scheme() == "http")) {
        bail!("webhook URL must use https (http requires the allow_http flag)");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("webhook URL must not contain credentials");
    }
    if url.fragment().is_some() {
        bail!("webhook URL must not contain a fragment");
    }
    let host = url
        .host()
        .ok_or_else(|| anyhow!("webhook URL must include a host"))?;
    if matches!(host, Host::Ipv4(ip) if !is_public_ip(IpAddr::V4(ip)))
        || matches!(host, Host::Ipv6(ip) if !is_public_ip(IpAddr::V6(ip)))
    {
        bail!("webhook URL must use a public destination");
    }
    if url.port_or_known_default().is_none_or(|port| port == 0) {
        bail!("webhook URL has no valid port");
    }
    Ok(url)
}

/// Build a client for the API pre-flight checks (https only, mirroring
/// `validate_target_url`).
pub async fn client_for_target(raw: &str) -> Result<(reqwest::Client, Url)> {
    client_for_target_opts(raw, false).await
}

/// Build a client for the API pre-flight checks, honoring the per-webhook
/// `allow_http` opt-in. The dispatcher uses the same path so a grandfathered
/// http webhook still delivers.
pub async fn client_for_target_opts(raw: &str, allow_http: bool) -> Result<(reqwest::Client, Url)> {
    let url = validate_target_url_opts(raw, allow_http)?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("webhook URL must include a host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("webhook URL has no valid port"))?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| anyhow!("webhook DNS lookup failed: {e}"))?
        .collect();
    if addresses.is_empty() || addresses.iter().any(|addr| !is_public_ip(addr.ip())) {
        bail!("webhook DNS must resolve only to public addresses");
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(host, &addresses)
        .build()?;
    Ok((client, url))
}


/// HMAC-SHA256 over `"{ts}.{body}"`, hex-encoded.
pub fn sign(secret: &str, body: &str, ts: i64) -> String {
    let mut mac = <HmacSha256 as hmac::Mac>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(format!("{ts}.{body}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn create(
    db: &Db,
    name: &str,
    url: &str,
    events: &[String],
    server_id: Option<i64>,
    allow_http: bool,
) -> Result<Webhook> {
    create_impl(db, master_key(), name, url, events, server_id, allow_http)
}

fn create_impl(
    db: &Db,
    key: Option<&str>,
    name: &str,
    url: &str,
    events: &[String],
    server_id: Option<i64>,
    allow_http: bool,
) -> Result<Webhook> {
    validate_events(events)?;
    validate_target_url_opts(url, allow_http)?;
    let conn = db.get()?;
    let now = Utc::now().to_rfc3339();
    let uuid = uuid::Uuid::new_v4().to_string();
    let secret = auth::random_token(32);
    let stored_secret = match key {
        Some(k) => encrypt_secret(k, &secret)?,
        None => secret.clone(),
    };
    let events_json = serde_json::to_string(events)?;
    conn.execute(
        "INSERT INTO webhooks (uuid, name, url, secret, events, server_id, enabled, \
         allow_http, failure_count, last_status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, 0, '', ?8, ?8)",
        params![uuid, name, url, stored_secret, events_json, server_id, allow_http as i64, now],
    )?;
    let id = conn.last_insert_rowid();
    drop(conn);
    get_impl(db, key, id)
}

pub fn list(db: &Db) -> Result<Vec<Webhook>> {
    list_impl(db, master_key())
}

fn list_impl(db: &Db, key: Option<&str>) -> Result<Vec<Webhook>> {
    let conn = db.get()?;
    let mut stmt = conn.prepare(&format!("SELECT {WH_COLS} FROM webhooks ORDER BY name"))?;
    let raws = stmt
        .query_map([], row_to_raw)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        out.push(raw_to_webhook(raw, &conn, key)?);
    }
    Ok(out)
}

pub fn get(db: &Db, id: i64) -> Result<Webhook> {
    get_impl(db, master_key(), id)
}

fn get_impl(db: &Db, key: Option<&str>, id: i64) -> Result<Webhook> {
    let conn = db.get()?;
    let raw = conn
        .query_row(
            &format!("SELECT {WH_COLS} FROM webhooks WHERE id=?1"),
            [id],
            row_to_raw,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("webhook not found"),
            other => other.into(),
        })?;
    raw_to_webhook(raw, &conn, key)
}

pub fn update(db: &Db, id: i64, patch: WebhookPatch) -> Result<Webhook> {
    update_impl(db, master_key(), id, patch)
}

fn update_impl(db: &Db, key: Option<&str>, id: i64, patch: WebhookPatch) -> Result<Webhook> {
    let conn = db.get()?;
    let raw = conn
        .query_row(
            &format!("SELECT {WH_COLS} FROM webhooks WHERE id=?1"),
            [id],
            row_to_raw,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => anyhow!("webhook not found"),
            other => other.into(),
        })?;
    // Decrypt before writing so a wrong key fails loudly here, and a legacy
    // plaintext row is migrated before it is re-encrypted on the write.
    let wh = raw_to_webhook(raw, &conn, key)?;
    if let Some(events) = patch.events {
        validate_events(events)?;
    }
    // Operator-supplied secrets must clear the strength bar: an empty or
    // tiny key makes the delivery HMAC forgeable. The create path
    // auto-generates its own 32-char token and never passes through here.
    if let Some(s) = patch.secret {
        if s.len() < MIN_SECRET_CHARS {
            bail!("webhook secret must be at least {MIN_SECRET_CHARS} bytes");
        }
    }
    let name = patch.name.unwrap_or(&wh.name);
    let url = patch.url.unwrap_or(&wh.url);
    let allow_http = patch.allow_http.unwrap_or(wh.allow_http);
    // Validate the effective (url, allow_http) pair whenever either side
    // changes: toggling the flag off against a plain-http URL is rejected in
    // the same round trip, and so is pointing an https-only webhook at http.
    if patch.url.is_some() || patch.allow_http.is_some() {
        validate_target_url_opts(url, allow_http)?;
    }
    let events: Vec<String> = patch
        .events
        .map(|e| e.to_vec())
        .unwrap_or_else(|| wh.events.clone());
    let secret = patch
        .secret
        .map(str::to_string)
        .unwrap_or_else(|| wh.secret.clone());
    let stored_secret = match key {
        Some(k) => encrypt_secret(k, &secret)?,
        None => secret,
    };
    let server_id = patch.server_id.unwrap_or(wh.server_id);
    let enabled = patch.enabled.unwrap_or(wh.enabled);
    let events_json = serde_json::to_string(&events)?;
    conn.execute(
        "UPDATE webhooks SET name=?1, url=?2, events=?3, secret=?4, server_id=?5, \
         enabled=?6, allow_http=?7, updated_at=?8 WHERE id=?9",
        params![
            name,
            url,
            events_json,
            stored_secret,
            server_id,
            enabled as i64,
            allow_http as i64,
            Utc::now().to_rfc3339(),
            id
        ],
    )?;
    drop(conn);
    get_impl(db, key, id)
}

pub fn delete(db: &Db, id: i64) -> Result<()> {
    let conn = db.get()?;
    let n = conn.execute("DELETE FROM webhooks WHERE id=?1", [id])?;
    if n == 0 {
        bail!("webhook not found");
    }
    Ok(())
}

pub fn set_enabled(db: &Db, id: i64, enabled: bool) -> Result<Webhook> {
    set_enabled_impl(db, master_key(), id, enabled)
}

fn set_enabled_impl(db: &Db, key: Option<&str>, id: i64, enabled: bool) -> Result<Webhook> {
    {
        let conn = db.get()?;
        conn.execute(
            "UPDATE webhooks SET enabled=?1, updated_at=?2 WHERE id=?3",
            params![enabled as i64, Utc::now().to_rfc3339(), id],
        )?;
    }
    get_impl(db, key, id)
}


// ---------------------------------------------------------------------------
// Recent-event registry: a process-wide, bounded ring of every emission the
// bus has observed, decoupled from webhook subscriptions. The scheduler's
// signal gates poll this registry instead of `webhook_deliveries`, so a gate
// sees an event whether or not any enabled webhook subscribes to it. `emit`
// records here on every call (no DB, one short lock hold) before any
// subscription matching; the deliveries enqueue below is untouched.
// ---------------------------------------------------------------------------

/// Retain at most this many emissions; the oldest is dropped past the cap.
const EVENT_REGISTRY_CAP: usize = 10_000;
/// Drop emissions older than this on insert, so even a quiet registry sheds
/// stale events instead of holding them until the ring fills.
const EVENT_REGISTRY_TTL: Duration = Duration::from_secs(10 * 60);

/// One observed emission: the event name, the server it fired for (`None` =
/// global / server-agnostic), and when it happened.
struct RegistryEntry {
    event: String,
    server_id: Option<i64>,
    at: SystemTime,
}

/// Process-wide ring of recent emissions, oldest at the front (entries are
/// pushed back in non-decreasing `at` order). TTL pruning walks forward from
/// the front and the cap is a single pop, so `record_event` is amortized
/// O(1); the lock is held only for that insert, never across the DB work in
/// `emit`. Insert-triggered pruning keeps memory bounded at `EVENT_REGISTRY_CAP`
/// entries even at a sustained high emit rate.
static EVENT_REGISTRY: LazyLock<parking_lot::Mutex<VecDeque<RegistryEntry>>> =
    LazyLock::new(|| parking_lot::Mutex::new(VecDeque::with_capacity(EVENT_REGISTRY_CAP)));

/// Record `event` for `server_id` in the recent-event registry, pruning
/// TTL-expired entries from the front and dropping the oldest when the ring
/// is at cap. Cheap and synchronous — safe on the per-domain emit paths
/// (proc/backups/scheduler/websites).
fn record_event(event: &str, server_id: Option<i64>) {
    let mut ring = EVENT_REGISTRY.lock();
    let now = SystemTime::now();
    while let Some(age) = ring.front().and_then(|e| now.duration_since(e.at).ok()) {
        if age < EVENT_REGISTRY_TTL {
            break;
        }
        ring.pop_front();
    }
    if ring.len() == EVENT_REGISTRY_CAP {
        ring.pop_front();
    }
    ring.push_back(RegistryEntry {
        event: event.to_string(),
        server_id,
        at: now,
    });
}

/// Has `event` been emitted within the retention window? A `Some(s)` query
/// matches server-`s` emissions plus global (`None`) ones — the visibility a
/// NULL-scoped webhook used to give; a `None` query matches any emission of
/// the event.
pub fn recent_event(event: &str, server_id: Option<i64>) -> bool {
    recent_event_since(event, server_id, SystemTime::UNIX_EPOCH)
}

/// [`recent_event`] limited to emissions at or after `since`, so a caller
/// (the scheduler's signal gates) can start counting from the moment a wait
/// began and a pre-existing event can never satisfy a fresh wait. `since` is
/// compared on the same `SystemTime` clock `emit` stamps, so in-process
/// comparisons are exact.
pub fn recent_event_since(event: &str, server_id: Option<i64>, since: SystemTime) -> bool {
    let ring = EVENT_REGISTRY.lock();
    let now = SystemTime::now();
    for e in ring.iter().rev() {
        let Ok(age) = now.duration_since(e.at) else {
            continue; // stamped in the future (clock skew): never counts
        };
        if age >= EVENT_REGISTRY_TTL {
            break; // newest-first scan: every earlier entry is stale too
        }
        if e.at < since || e.event != event {
            continue;
        }
        if server_id.is_none() || e.server_id == server_id || e.server_id.is_none() {
            return true;
        }
    }
    false
}
/// Enqueue one `pending` delivery per enabled subscription matching `event`.
/// A webhook with `server_id = NULL` is global; a scoped one only fires for
/// its server. Synchronous and write-only — never performs HTTP.
pub fn emit(db: &Db, event: &str, server_id: Option<i64>, payload: Value) -> usize {
    // The recent-event registry records every emission, subscribed or not
    // (cheap, no DB), so signal gates observe events no webhook is
    // subscribed to. The deliveries enqueue below still requires a match.
    record_event(event, server_id);
    let payload = payload.to_string();
    // Size cap: serialize once, and refuse to enqueue a payload too large to
    // deliver sanely. Dropping the whole event here (rather than per webhook)
    // keeps the semantics simple — an oversized event is a caller bug, not
    // something to fan out partially.
    if payload.len() > MAX_PAYLOAD_BYTES {
        tracing::warn!(
            "webhook emit: dropping {event}: payload is {} bytes (cap {MAX_PAYLOAD_BYTES})",
            payload.len()
        );
        return 0;
    }
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let next_at = now.timestamp();
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("webhook emit: {e}");
            return 0;
        }
    };
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
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
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
        // Pending cap: a webhook whose queue has piled up stops ingesting
        // rather than growing the backlog without bound. Admin "test" sends
        // bypass this via enqueue_one.
        let pending: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM webhook_deliveries WHERE webhook_id=?1 AND status='pending'",
            [wid],
            |r| r.get(0),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("webhook emit: {e}");
                continue;
            }
        };
        if pending >= MAX_PENDING_PER_WEBHOOK {
            continue; // backlogged — drop this event for this webhook
        }
        if insert
            .execute(params![wid, event, payload, next_at, created_at])
            .is_ok()
        {
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
    let conn = db.get()?;
    // Same pending cap as `emit`: a backlogged webhook rejects the admin test
    // delivery too, so the test action cannot grow an unbounded queue that
    // the dispatcher then has to chew through.
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE webhook_id=?1 AND status='pending'",
        [webhook_id],
        |r| r.get(0),
    )?;
    if pending >= MAX_PENDING_PER_WEBHOOK {
        bail!("webhook queue is full ({pending} pending deliveries)");
    }
    conn.execute(
        "INSERT INTO webhook_deliveries (webhook_id, event, payload, attempt, status, \
         next_attempt_at, created_at) VALUES (?1, ?2, ?3, 0, 'pending', ?4, ?5)",
        params![
            webhook_id,
            event,
            payload.to_string(),
            now.timestamp(),
            now.to_rfc3339()
        ],
    )?;
    Ok(())
}

enum DeliveryOutcome {
    Delivered { code: i64 },
    Failed { code: Option<i64>, error: String },
}

/// Per-delivery endpoint snapshot for the dispatch stream: a cached client
/// and parsed target for a live webhook, the validation/DNS error when the
/// endpoint failed to build, or `Deleted` when the webhook vanished mid-sweep.
enum EndpointSlot {
    Ready {
        client: reqwest::Client,
        target: Url,
        secret: String,
    },
    Unavailable(String),
    Deleted,
}

/// Claim up to `limit` due `pending` deliveries, POST each with the signature
/// headers and a short timeout, then record the result: `delivered` on 2xx,
/// rescheduled with exponential backoff otherwise, `failed` past the attempt
/// cap. The claim is a single conditional UPDATE that leases the rows to this
/// dispatcher, so a concurrent dispatcher's identical claim matches nothing
/// and never POSTs; the result write replaces the lease with the delivery's
/// real status and backoff. The DB lock is only held around the claim and the
/// result write, never across the HTTP call.
pub async fn dispatch_due(db: &Db, notifier: &Notifier, limit: usize) -> usize {
    dispatch_due_impl(db, master_key(), notifier, limit).await
}

async fn dispatch_due_impl(
    db: &Db,
    key: Option<&str>,
    notifier: &Notifier,
    limit: usize,
) -> usize {
    let now = Utc::now().timestamp();
    // Own the master key for the `'static` endpoints closure below: it is
    // consulted (via `as_deref`) while decrypting at-rest secrets.
    let key = key.map(str::to_string);
    // Lease ceiling: the worst legitimate batch is `limit` deliveries, each
    // bounded by HTTP_TIMEOUT plus a connect margin, plus sweep slack. A live
    // batch is therefore never re-claimed, while a batch orphaned by a
    // crashed dispatcher becomes due again after the lease expires and is
    // delivered by a later sweep (at-least-once; a duplicate can only follow
    // a crash, never a live claim race).
    let lease_until = now
        + CLAIM_LEASE_MARGIN_S
        + (limit as i64) * (HTTP_TIMEOUT.as_secs() as i64 + CONNECT_MARGIN_S);
    let claimed: Vec<(i64, i64, String, String, i64)> = match db.call(move |conn| {
        let mut stmt = conn.prepare(
            "UPDATE webhook_deliveries SET next_attempt_at = ?3 \
             WHERE id IN ( \
                 SELECT id FROM webhook_deliveries \
                 WHERE status='pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?1) \
                 ORDER BY id LIMIT ?2 \
             ) \
             RETURNING id, webhook_id, event, payload, attempt",
        )?;
        let rows = stmt.query_map(params![now, limit as i64, lease_until], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        Ok(rows.flatten().collect())
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("webhook dispatch: {e:#}");
            return 0;
        }
    };
    if claimed.is_empty() {
        return 0;
    }

    // Snapshot endpoints so the result write needs no webhook lookup. The
    // at-rest secret is decrypted here; a row that fails authentication
    // (master key changed or corrupt) becomes `None` so its deliveries fail
    // with a clear error — loud, never a silently empty signature.
    let endpoints: HashMap<i64, (String, Option<String>, bool)> = match db.call(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, url, secret, allow_http FROM webhooks WHERE enabled=1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)? != 0,
            ))
        })?;
        let mut out = HashMap::new();
        for (id, url, raw, allow_http) in rows.flatten() {
            match secret_for_row(conn, id, &raw, key.as_deref()) {
                Ok(secret) => {
                    out.insert(id, (url, Some(secret), allow_http));
                }
                Err(e) => {
                    tracing::error!(webhook_id = id, "webhook dispatch: {e:#}");
                    out.insert(id, (url, None, allow_http));
                }
            }
        }
        Ok(out)
    })
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("webhook dispatch: {e:#}");
            return 0;
        }
    };

    // One cached client per (url, allow_http): reqwest pools connections and
    // the builder pins the DNS answer (`resolve_to_addrs`), so repeated
    // deliveries to the same webhook reuse the connection instead of
    // re-resolving and rebuilding a client per POST. Only webhooks with
    // claimed deliveries in this batch are resolved.
    let claimed_ids: HashSet<i64> = claimed.iter().map(|c| c.1).collect();
    let mut client_cache: HashMap<(String, bool), Result<(reqwest::Client, Url), String>> =
        HashMap::new();
    for (id, (url, _, allow_http)) in endpoints.iter() {
        if !claimed_ids.contains(id) {
            continue;
        }
        let key = (url.clone(), *allow_http);
        if client_cache.contains_key(&key) {
            continue;
        }
        let built = match client_for_target_opts(url, *allow_http).await {
            Ok(pair) => Ok(pair),
            Err(e) => Err(e.to_string()),
        };
        client_cache.insert(key, built);
    }

    // Snapshot each delivery's endpoint before the stream so the POST futures
    // own all their state; the result writes below stay serialized — one DB
    // write at a time, never across an HTTP call.
    let deliveries: Vec<(i64, i64, String, String, i64, EndpointSlot)> = claimed
        .into_iter()
        .map(|(delivery_id, webhook_id, event, payload, attempt)| {
            let slot = match endpoints.get(&webhook_id) {
                Some((url, Some(secret), allow_http)) => {
                    let key = (url.clone(), *allow_http);
                    match client_cache.get(&key) {
                        Some(Ok((client, target))) => EndpointSlot::Ready {
                            client: client.clone(),
                            target: target.clone(),
                            secret: secret.clone(),
                        },
                        Some(Err(e)) => EndpointSlot::Unavailable(e.clone()),
                        None => EndpointSlot::Unavailable("endpoint not resolved".to_string()),
                    }
                }
                Some((_url, None, _allow_http)) => EndpointSlot::Unavailable(
                    "webhook secret cannot be decrypted with the configured \
                     webhook_master_key — the key changed or the row is corrupt"
                        .to_string(),
                ),
                None => EndpointSlot::Deleted,
            };
            (delivery_id, webhook_id, event, payload, attempt, slot)
        })
        .collect();

    // POST the batch with bounded concurrency (8 in flight), recording each
    // result as its POST completes. Claim-lease, backoff and circuit-breaker
    // semantics are unchanged: the lease above sizes the worst serial batch,
    // which still bounds the faster concurrent one.
    const CONCURRENCY: usize = 8;
    let mut batch = stream::iter(deliveries)
        .map(|(delivery_id, webhook_id, event, payload, attempt, slot)| async move {
            let outcome = match slot {
                EndpointSlot::Ready {
                    client,
                    target,
                    secret,
                } => {
                    let ts = Utc::now().timestamp();
                    let sig = sign(&secret, &payload, ts);
                    let resp = client
                        .post(target)
                        .header("Content-Type", "application/json")
                        .header("X-VoltPanel-Event", &event)
                        .header("X-VoltPanel-Delivery", delivery_id.to_string())
                        .header("X-VoltPanel-Timestamp", ts.to_string())
                        .header("X-VoltPanel-Signature", format!("sha256={sig}"))
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
                EndpointSlot::Unavailable(e) => DeliveryOutcome::Failed {
                    code: None,
                    error: e,
                },
                EndpointSlot::Deleted => DeliveryOutcome::Failed {
                    code: None,
                    error: "webhook deleted or disabled".to_string(),
                },
            };
            (delivery_id, webhook_id, attempt, outcome)
        })
        .buffer_unordered(CONCURRENCY);

    let mut processed = 0usize;
    while let Some((delivery_id, webhook_id, attempt, outcome)) = batch.next().await {
        let new_attempt = attempt + 1;
        let backoff_s = BACKOFF_BASE_S << attempt.min(MAX_ATTEMPTS - 1) as u32;
        let gave_up = new_attempt >= MAX_ATTEMPTS;
        let disabled: bool = match db.call(move |conn| {
            match outcome {
                DeliveryOutcome::Delivered { code } => {
                    let _ = conn.execute(
                        "UPDATE webhook_deliveries SET status='delivered', response_code=?1, \
                         error='', delivered_at=?2 WHERE id=?3",
                        params![code, Utc::now().to_rfc3339(), delivery_id],
                    );
                    let _ = conn.execute(
                        "UPDATE webhooks SET last_status=?1, failure_count=0 WHERE id=?2",
                        params![code.to_string(), webhook_id],
                    );
                }
                DeliveryOutcome::Failed { code, error } => {
                    if gave_up {
                        let _ = conn.execute(
                            "UPDATE webhook_deliveries SET status='failed', attempt=?1, \
                             response_code=?2, error=?3, delivered_at=?4 WHERE id=?5",
                            params![
                                new_attempt,
                                code,
                                error,
                                Utc::now().to_rfc3339(),
                                delivery_id
                            ],
                        );
                    } else {
                        let next = Utc::now().timestamp() + backoff_s;
                        let _ = conn.execute(
                            "UPDATE webhook_deliveries SET status='pending', attempt=?1, \
                             response_code=?2, error=?3, next_attempt_at=?4 WHERE id=?5",
                            params![new_attempt, code, error, next, delivery_id],
                        );
                    }
                    // The breaker counts connection errors only (code None —
                    // DNS, connect, timeout). A 4xx is the target rejecting
                    // the payload, not a delivery infrastructure problem, and
                    // must not trip the breaker. Any delivered delivery
                    // resets the count.
                    if code.is_none() {
                        let _ = conn.execute(
                            "UPDATE webhooks SET failure_count=failure_count+1, last_status=?1 \
                             WHERE id=?2",
                            params![error, webhook_id],
                        );
                        let disabled = conn
                            .execute(
                                "UPDATE webhooks SET enabled=0, last_status=?1, updated_at=?2 \
                                 WHERE id=?3 AND enabled=1 AND failure_count>=?4",
                                params![
                                    error,
                                    Utc::now().to_rfc3339(),
                                    webhook_id,
                                    BREAKER_THRESHOLD
                                ],
                            )
                            .unwrap_or(0);
                        return Ok(disabled > 0);
                    } else {
                        let _ = conn.execute(
                            "UPDATE webhooks SET last_status=?1 WHERE id=?2",
                            params![error, webhook_id],
                        );
                    }
                }
            }
            Ok(false)
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                // Lease already claimed; the result write is best-effort, so a
                // pool failure just logs and leaves the row to be retried once
                // the lease expires.
                tracing::warn!("webhook dispatch: {e}");
                continue;
            }
        };
        if disabled {
            notifier.notify(
                "warn",
                "Webhook disabled",
                &format!(
                    "webhook #{webhook_id} disabled after {BREAKER_THRESHOLD} \
                     consecutive connection failures"
                ),
                None,
            );
        }
        processed += 1;
    }
    processed
}

/// Delete terminal deliveries older than `older_than_ts` (rfc3339) in bounded
/// batches: each DELETE (and its write lock) touches at most 5000 rows, served
/// by the (status, delivered_at) index.
pub fn prune_deliveries(db: &Db, older_than_ts: &str) -> Result<usize> {
    let conn = db.get()?;
    let mut total = 0usize;
    loop {
        let n = conn.execute(
            "DELETE FROM webhook_deliveries \
             WHERE status IN ('delivered','failed') AND delivered_at < ?1 \
               AND id IN ( \
                   SELECT id FROM webhook_deliveries \
                   WHERE status IN ('delivered','failed') AND delivered_at < ?1 \
                   LIMIT 5000 \
               )",
            [older_than_ts],
        )?;
        total += n;
        if n == 0 {
            return Ok(total);
        }
    }
}

pub fn deliveries(db: &Db, webhook_id: i64, limit: i64) -> Result<Vec<Delivery>> {
    let limit = limit.clamp(1, 200);
    let conn = db.get()?;
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
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::{
        create, create_impl, decrypt_secret, dispatch_due, emit, enqueue_one,
        encrypt_secret, event_matches, get, get_impl, is_public_ip, prune_deliveries,
        record_event, recent_event, recent_event_since, sign, update, validate_events,
        validate_target_url, validate_target_url_opts, RegistryEntry, WebhookPatch,
        EVENT_REGISTRY, EVENT_REGISTRY_CAP, EVENT_REGISTRY_TTL, TEST_EVENT,
    };
    use crate::db::Db;
    use crate::services::proc::Notifier;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use chrono::Utc;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    /// Fresh migrated DB with no seed data (the webhook tables come from the
    /// schema ladder in `db::open`).
    fn test_db() -> Db {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "voltpanel-webhooks-test-{}-{}.db",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_file(&path);
        crate::db::open(path.to_str().unwrap()).unwrap()
    }

    /// Public-IP URL for dispatch tests: passes `validate_target_url`, and the
    /// connect to a dead port fails fast (or times out) without touching a
    /// real service — so an attempted POST is observable through `attempt`.
    const DEAD_URL: &str = "https://8.8.8.8:1/hook";

    #[test]
    fn secret_encrypt_decrypt_round_trip() {
        let key = "master-key-for-tests";
        let secret = "tok_0123456789abcdef";
        let enc = encrypt_secret(key, secret).unwrap();
        assert!(enc.starts_with("v1:"), "stored form carries the version prefix: {enc}");
        // Nonce (12) + ciphertext with GCM tag (>= 16) survive the base64 body.
        let body = STANDARD.decode(enc.strip_prefix("v1:").unwrap()).unwrap();
        assert!(body.len() >= 12 + 16, "blob must hold nonce + ciphertext + tag");
        assert_eq!(decrypt_secret(key, &enc).unwrap(), secret);
        // A fresh nonce per row: the same secret never encrypts to the same blob.
        assert_ne!(enc, encrypt_secret(key, secret).unwrap());
        // Any-length master keys work (the key is folded via SHA-256).
        assert_eq!(decrypt_secret("k", &encrypt_secret("k", "s").unwrap()).unwrap(), "s");
    }

    #[test]
    fn secret_decrypt_with_wrong_key_fails_loudly() {
        let enc = encrypt_secret("master-A", "s3cret-value").unwrap();
        let err = decrypt_secret("master-B", &enc).unwrap_err();
        assert!(
            err.to_string().contains("webhook_master_key"),
            "the error must point the operator at the master key: {err}"
        );
        assert!(
            decrypt_secret("master-A", "v1:%%%not-base64%%%").is_err(),
            "corrupt blobs must not decrypt silently"
        );
        assert!(
            decrypt_secret("master-A", "plaintext-legacy-value").is_err(),
            "a plaintext value under a key is an error, never a silent empty"
        );
    }

    #[test]
    fn secrets_are_encrypted_at_rest_and_load_back() {
        let db = test_db();
        let key = "k-0123456789abcdef";
        let wh = create_impl(
            &db,
            Some(key),
            "wh",
            "https://example.com/hook",
            &["*".to_string()],
            None,
            false,
        )
        .unwrap();
        let conn = db.get().unwrap();
        let stored: String = conn
            .query_row("SELECT secret FROM webhooks WHERE id=?1", [wh.id], |r| r.get(0))
            .unwrap();
        assert!(stored.starts_with("v1:"), "at rest the secret must be encrypted: {stored}");
        assert_ne!(stored, wh.secret);
        // Same key round-trips; a different key fails loudly at load.
        assert_eq!(get_impl(&db, Some(key), wh.id).unwrap().secret, wh.secret);
        let err = get_impl(&db, Some("wrong-key"), wh.id).unwrap_err();
        assert!(err.to_string().contains("webhook_master_key"), "{err}");
        // The no-key path still reads the row (plaintext passthrough of the
        // encrypted blob is NOT valid — a row with a key removed is broken
        // config, and the encrypted blob must not masquerade as a secret).
        assert_ne!(get_impl(&db, None, wh.id).unwrap().secret, wh.secret);
    }

    #[test]
    fn legacy_plaintext_secret_upgrades_lazily_on_first_read() {
        let db = test_db();
        // Public `create` runs under the real config's master key (empty in
        // tests) so the row lands in plaintext, simulating a pre-key install.
        let wh = create(&db, "wh", "https://example.com/hook", &["*".to_string()], None, false).unwrap();
        {
            let conn = db.get().unwrap();
            let stored: String = conn
                .query_row("SELECT secret FROM webhooks WHERE id=?1", [wh.id], |r| r.get(0))
                .unwrap();
            assert!(!stored.starts_with("v1:"), "no key configured → legacy plaintext");
        }
        // First read under a key migrates the row in place and returns the secret.
        let key = "k-0123456789abcdef";
        assert_eq!(get_impl(&db, Some(key), wh.id).unwrap().secret, wh.secret);
        let conn = db.get().unwrap();
        let stored: String = conn
            .query_row("SELECT secret FROM webhooks WHERE id=?1", [wh.id], |r| r.get(0))
            .unwrap();
        assert!(stored.starts_with("v1:"), "legacy row must be encrypted at rest: {stored}");
        assert_eq!(get_impl(&db, Some(key), wh.id).unwrap().secret, wh.secret);
    }

    #[tokio::test]
    async fn concurrent_dispatchers_claim_each_delivery_once() {
        let db = test_db();
        let wh = create(&db, "wh", DEAD_URL, &["*".to_string()], None, false).unwrap();
        enqueue_one(&db, wh.id, TEST_EVENT, json!({"ok": true})).unwrap();
        // Two dispatchers race for one due delivery: the atomic claim lets
        // exactly one win; the loser's claim matches nothing and it never POSTs.
        let (n1, n2) = (Notifier::default(), Notifier::default());
        let (a, b) = tokio::join!(
            dispatch_due(&db, &n1, 10),
            dispatch_due(&db, &n2, 10)
        );
        assert_eq!(a + b, 1, "exactly one dispatcher must win the claim");
        let conn = db.get().unwrap();
        let (status, attempt): (String, i64) = conn
            .query_row(
                "SELECT status, attempt FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "pending",
            "a single failed POST reschedules; only the attempt cap gives up"
        );
        assert_eq!(attempt, 1, "exactly one POST must have been attempted");
    }

    #[tokio::test]
    async fn leased_delivery_is_invisible_to_other_dispatchers() {
        let db = test_db();
        let wh = create(&db, "wh", DEAD_URL, &["*".to_string()], None, false).unwrap();
        enqueue_one(&db, wh.id, TEST_EVENT, json!({"ok": true})).unwrap();
        let now = Utc::now().timestamp();
        // Emulate dispatcher A's in-flight claim: lease far in the future.
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE webhook_deliveries SET next_attempt_at=?1",
                [now + 600],
            )
            .unwrap();
        }
        let processed = dispatch_due(&db, &Notifier::default(), 10).await;
        assert_eq!(processed, 0, "a leased delivery is never claimed twice");
        let conn = db.get().unwrap();
        let (status, attempt, next): (String, i64, Option<i64>) = conn
            .query_row(
                "SELECT status, attempt, next_attempt_at FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(attempt, 0, "no POST may be attempted for a leased row");
        assert_eq!(next, Some(now + 600), "the lease must be left untouched");
    }

    #[tokio::test]
    async fn failed_delivery_reschedules_with_backoff_after_claim() {
        let db = test_db();
        let wh = create(&db, "wh", DEAD_URL, &["*".to_string()], None, false).unwrap();
        enqueue_one(&db, wh.id, TEST_EVENT, json!({"ok": true})).unwrap();
        let processed = dispatch_due(&db, &Notifier::default(), 10).await;
        assert_eq!(processed, 1);
        let conn = db.get().unwrap();
        let (status, attempt, next): (String, i64, i64) = conn
            .query_row(
                "SELECT status, attempt, next_attempt_at FROM webhook_deliveries",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "pending", "retry semantics unchanged");
        assert_eq!(attempt, 1, "claim lease is replaced by the real attempt count");
        let now = Utc::now().timestamp();
        assert!(
            next > now && next <= now + 40,
            "backoff (30s base) must be scheduled, not the multi-minute lease"
        );
    }

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
        let mixed = ["server.*", "nope.*"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert!(validate_events(&mixed).is_err());
    }

    #[test]
    fn target_url_rejects_non_public_and_credentialed_destinations() {
        for blocked in [
            "http://127.0.0.1:8080/hook",
            "http://10.0.0.1/hook",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/hook",
            "https://user:pass@example.com/hook",
            "file:///etc/passwd",
        ] {
            assert!(validate_target_url(blocked).is_err(), "accepted {blocked}");
        }
        assert!(validate_target_url("https://example.com/hook").is_ok());
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn https_is_required_unless_allow_http_opt_in() {
        // Plain http on a public host: rejected by the strict validator,
        // accepted only with the per-webhook opt-in.
        assert!(
            validate_target_url("http://example.com/hook").is_err(),
            "http must be rejected without the allow_http flag"
        );
        assert!(
            validate_target_url_opts("http://example.com/hook", true).is_ok(),
            "http must be accepted when the webhook opts in"
        );
        assert!(validate_target_url("https://example.com/hook").is_ok());
    }

    #[test]
    fn create_and_update_respect_allow_http_flag() {
        let db = test_db();
        // http without the opt-in is rejected at create; with it, it lands.
        assert!(
            create(&db, "wh", "http://example.com/hook", &["*".to_string()], None, false).is_err(),
            "http must be rejected at create without allow_http"
        );
        let wh = create(&db, "wh", "http://example.com/hook", &["*".to_string()], None, true)
            .expect("http must be accepted at create with allow_http");
        assert!(wh.allow_http, "the created webhook must carry the opt-in");
        // Toggling the flag off while the URL stays http is rejected in the
        // same round trip.
        let patch = WebhookPatch {
            name: None,
            url: None,
            events: None,
            secret: None,
            server_id: None,
            enabled: None,
            allow_http: Some(false),
        };
        assert!(
            update(&db, wh.id, patch).is_err(),
            "dropping allow_http under a plain-http URL must be rejected"
        );
        // Moving to https lets the flag come off cleanly.
        let patch = WebhookPatch {
            name: None,
            url: Some("https://example.com/hook"),
            events: None,
            secret: None,
            server_id: None,
            enabled: None,
            allow_http: Some(false),
        };
        let wh = update(&db, wh.id, patch).unwrap();
        assert!(!wh.allow_http);
    }

    #[tokio::test]
    async fn circuit_breaker_disables_webhook_at_connection_failure_threshold() {
        let db = test_db();
        let wh = create(&db, "wh", DEAD_URL, &["*".to_string()], None, false).unwrap();
        // Seed one below the threshold so a single failed POST trips it.
        {
            let conn = db.get().unwrap();
            conn.execute(
                "UPDATE webhooks SET failure_count=?1 WHERE id=?2",
                rusqlite::params![19, wh.id],
            )
            .unwrap();
        }
        let notifier = Notifier::default();
        enqueue_one(&db, wh.id, TEST_EVENT, json!({"ok": true})).unwrap();
        let processed = dispatch_due(&db, &notifier, 10).await;
        assert_eq!(processed, 1);
        let after = get(&db, wh.id).unwrap();
        assert_eq!(after.failure_count, 20, "connection failure must be counted");
        assert!(!after.enabled, "breaker must disable the webhook at the threshold");
        assert!(
            notifier
                .history()
                .iter()
                .any(|n| n.title.contains("Webhook disabled")),
            "the disable must surface as a notification"
        );
    }

    #[test]
    fn emit_drops_events_for_backlogged_webhooks() {
        let db = test_db();
        let wh = create(&db, "wh", "https://example.com/hook", &["*".to_string()], None, false).unwrap();
        // Fill the queue with 500 pending deliveries (the cap).
        {
            let conn = db.get().unwrap();
            conn.execute_batch(&format!(
                "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM cnt WHERE x<500) \
                 INSERT INTO webhook_deliveries (webhook_id, event, payload, attempt, status, \
                 next_attempt_at, created_at) \
                 SELECT {}, 'server.start', '{{}}', 0, 'pending', 0, 'now' FROM cnt;",
                wh.id
            ))
            .unwrap();
        }
        assert_eq!(
            emit(&db, "server.start", None, json!({"n": 1})),
            0,
            "at the cap the event must be dropped"
        );
        // Control: a second webhook with an empty queue still ingests.
        let other = create(&db, "other", "https://example.com/hook", &["*".to_string()], None, false)
            .unwrap();
        let _ = other;
        assert_eq!(
            emit(&db, "server.start", None, json!({"n": 2})),
            1,
            "an uncapped webhook must still receive the event"
        );
    }

    #[test]
    fn prune_deliveries_removes_only_old_terminal_rows() {
        let db = test_db();
        let wh = create(&db, "wh", "https://example.com/hook", &["*".to_string()], None, false).unwrap();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO webhook_deliveries \
                 (webhook_id, event, payload, attempt, status, delivered_at, created_at) \
                 VALUES (?1, 'e', '{}', 1, 'delivered', ?2, 'now'), \
                        (?1, 'e', '{}', 1, 'failed', ?2, 'now'), \
                        (?1, 'e', '{}', 1, 'delivered', ?3, 'now'), \
                        (?1, 'e', '{}', 0, 'pending', NULL, 'now')",
                rusqlite::params![wh.id, "2020-01-01T00:00:00Z", "2022-01-01T00:00:00Z"],
            )
            .unwrap();
        }
        let n = prune_deliveries(&db, "2021-01-01T00:00:00Z").unwrap();
        assert_eq!(n, 2, "exactly the two pre-cutoff terminal rows go");
        let conn = db.get().unwrap();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT status FROM webhook_deliveries ORDER BY id")
                .unwrap();
            let rows = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            rows
        };
        assert_eq!(
            remaining,
            vec!["delivered".to_string(), "pending".to_string()],
            "fresh delivered rows and pending rows must survive"
        );
    }
    // The registry is process-wide and the test binary runs tests on
    // parallel threads, so these use event names no other test emits and
    // assert only facts that hold regardless of concurrent emissions.

    #[test]
    fn event_registry_bounds_ring_at_cap() {
        record_event("cap.probe", Some(1));
        // Filling past the cap must evict the oldest emission: bounded memory.
        for i in 0..EVENT_REGISTRY_CAP + 100 {
            record_event(&format!("cap.filler.{i}"), Some(1));
        }
        assert!(
            !recent_event("cap.probe", Some(1)),
            "the oldest emission must be evicted once the ring passes cap"
        );
        let len = EVENT_REGISTRY.lock().len();
        assert!(
            len <= EVENT_REGISTRY_CAP,
            "the ring must stay at or under cap, was {len}"
        );
    }

    #[test]
    fn event_registry_prunes_stale_entries_on_insert() {
        // A stale entry at the front (the oldest position) is dropped by the
        // next insert's TTL prune.
        let aged = RegistryEntry {
            event: "aged.probe".to_string(),
            server_id: Some(1),
            at: SystemTime::now() - (EVENT_REGISTRY_TTL + Duration::from_secs(1)),
        };
        EVENT_REGISTRY.lock().push_front(aged);
        record_event("aged.fresh", None);
        let ring = EVENT_REGISTRY.lock();
        assert!(
            ring.iter().all(|e| e.event != "aged.probe"),
            "the stale entry must be pruned by the insert"
        );
    }

    #[test]
    fn event_registry_scopes_emissions_by_server() {
        record_event("scoped.only", Some(7));
        record_event("global.only", None);
        assert!(recent_event("scoped.only", Some(7)), "the server sees its own event");
        assert!(!recent_event("scoped.only", Some(8)), "other servers do not");
        assert!(recent_event("scoped.only", None), "a global query sees every emission");
        assert!(
            recent_event("global.only", Some(7)),
            "a global emission counts for every server"
        );
        assert!(!recent_event("never.seen", Some(7)), "unknown events are never seen");
    }

    #[test]
    fn event_registry_since_bound_excludes_older_emissions() {
        record_event("since.probe", Some(1));
        let after = SystemTime::now();
        assert!(
            !recent_event_since("since.probe", Some(1), after),
            "the pre-bound emission must not count"
        );
        assert!(recent_event_since("since.probe", Some(1), SystemTime::UNIX_EPOCH));
        record_event("since.probe", Some(1));
        assert!(recent_event_since("since.probe", Some(1), after), "the new emission counts");
    }
}