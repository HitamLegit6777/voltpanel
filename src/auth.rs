//! Auth: password hashing (argon2id), session tokens, TOTP 2FA, rate limiting.
use crate::config::Config;
use crate::db::Db;
use crate::models::{self, User};
use anyhow::{anyhow, bail, Result};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use base64::Engine;
use chrono::Utc;
use rand::Rng;
use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

pub const SESSION_COOKIE: &str = "vp_session";
pub const TOTP_ISSUER: &str = "VoltPanel";

/// Minimum age of `sessions.last_seen` before `user_from_session` rewrites
/// it, so an active panel does not pay a write on every authenticated request.
const LAST_SEEN_THROTTLE_MINS: i64 = 5;
/// Maximum concurrent sessions kept per user. When a new session pushes a
/// user past the cap, expired/revoked sessions are evicted first (oldest
/// first), then the oldest still-active ones — never the newest.
const MAX_SESSIONS_PER_USER: i64 = 10;
/// Opportunistic sweep cadence: `create_session` runs `prune_sessions` at
/// most once per interval, so an idle deployment still self-cleans without a
/// scheduler hook. A background sweep (e.g. from the scheduler) can call
/// `prune_sessions` directly at its own cadence.
const SESSION_PRUNE_INTERVAL_SECS: i64 = 3600;
/// `prune_sessions` only deletes expired/revoked rows older than this, so a
/// freshly revoked session survives long enough to be audited.
const SESSION_PRUNE_MAX_AGE_HOURS: i64 = 24 * 7;

pub fn hash_password(cfg: &Config, plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params = argon2::Params::new(
        cfg.security.argon2_mem_kib,
        cfg.security.argon2_cost,
        1,
        None,
    )
    .map_err(|e| anyhow!("argon2 params: {e}"))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    Ok(argon
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow!("hash: {e}"))?
        .to_string())
}

pub fn verify_password(hash: &str, plain: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

/// True when `hash` (a PHC string) encodes weaker Argon2 parameters than the
/// current `cfg` floor: memory below `argon2_mem_kib` or iterations below
/// `argon2_cost`. Unparseable hashes count as "needs upgrade": a successful
/// verification is the only path that acts on this, so a bogus hash can never
/// trigger a write.
pub fn hash_needs_upgrade(cfg: &Config, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    let (mut m_cost, mut t_cost) = (None, None);
    for (name, value) in parsed.params.iter() {
        match name.as_str() {
            "m" => m_cost = value.decimal().ok(),
            "t" => t_cost = value.decimal().ok(),
            _ => {}
        }
    }
    match (m_cost, t_cost) {
        (Some(m), Some(t)) => m < cfg.security.argon2_mem_kib || t < cfg.security.argon2_cost,
        _ => true,
    }
}

/// Verify `plain` against `stored`; on success, transparently rehash and
/// persist an upgraded hash when the stored one is weaker than `cfg`'s floor.
/// The upgrade is best-effort: login success never depends on the write.
/// Login handlers should call this instead of [`verify_password`] so old
/// hashes harden in place the first time each user signs in.
pub fn verify_password_with_upgrade(
    db: &Db,
    cfg: &Config,
    user_id: i64,
    stored: &str,
    plain: &str,
) -> bool {
    if !verify_password(stored, plain) {
        return false;
    }
    if hash_needs_upgrade(cfg, stored) {
        // Best-effort: a failed rehash or write must never fail the login.
        let _ = hash_password(cfg, plain).and_then(|h| models::set_password(db, user_id, &h));
    }
    true
}

pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill(&mut buf[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

// ---------------- 2FA recovery codes ----------------

/// How many one-time recovery codes every 2FA-enabled user holds.
pub const RECOVERY_CODE_COUNT: usize = 10;

/// Alphabet for recovery codes: uppercase letters and digits with the
/// confusable pairs removed (0/O, 1/I/l). 32 symbols = 5 bits each, so a
/// 10-character code carries 50 bits of entropy — infeasible to guess
/// online while staying short enough to type.
const RECOVERY_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// A fresh single-use 2FA recovery code. Codes are shown to the user exactly
/// once at generation time; only the SHA-256 digest is ever stored.
pub fn generate_recovery_code() -> String {
    let mut buf = [0u8; 10];
    rand::thread_rng().fill(&mut buf);
    // 256 % 32 == 0, so the modulo draw is unbiased.
    buf.iter()
        .map(|b| RECOVERY_ALPHABET[*b as usize % RECOVERY_ALPHABET.len()] as char)
        .collect()
}

/// Generate a fresh set of single-use recovery codes (plaintext). Callers
/// persist only [`hash_recovery_code`] results and return these strings in
/// the response exactly once.
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT)
        .map(|_| generate_recovery_code())
        .collect()
}

/// Canonicalize user input before hashing/consuming: strip separators (the
/// UI groups codes as `ABCDE-FGHJK`) and force uppercase, so `abcde-fghjk`
/// and `ABCDEFGHJK` are the same code.
pub fn normalize_recovery_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Hash a recovery code at rest (SHA-256 hex, same primitive as session
/// tokens). The codes are 50-bit random values, so a plain digest is safe:
/// there is no low-entropy dictionary to brute-force.
pub fn hash_recovery_code(code: &str) -> String {
    hash_token(&normalize_recovery_code(code))
}

// ---------------- Sessions ----------------

pub fn create_session(
    db: &Db,
    cfg: &Config,
    user_id: i64,
    user_agent: &str,
    ip: &str,
    remember: bool,
) -> Result<(String, String)> {
    let raw = random_token(32);
    let token = hash_token(&raw);
    let ttl = if remember {
        cfg.web.session_ttl_hours.checked_mul(7)
    } else {
        Some(cfg.web.session_ttl_hours)
    }
    .and_then(|hours| i64::try_from(hours).ok())
    .ok_or_else(|| anyhow!("session TTL is too large"))?;
    let created = Utc::now();
    let expires = created + chrono::Duration::hours(ttl);
    let conn = db.get()?;
    conn.execute(
        "INSERT INTO sessions(token,user_id,user_agent,ip,created_at,expires_at,remember) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            token,
            user_id,
            user_agent,
            ip,
            created.to_rfc3339(),
            expires.to_rfc3339(),
            remember as i64
        ],
    )?;
    drop(conn);
    // Opportunistic housekeeping: sweep expired/revoked rows (throttled) and
    // cap concurrent sessions so the table cannot grow without bound.
    maybe_prune_sessions(db);
    enforce_session_cap(db, user_id)?;
    Ok((raw, expires.to_rfc3339()))
}

/// Run `prune_sessions` at most once per `SESSION_PRUNE_INTERVAL_SECS`.
/// Inlined from `create_session`; a background sweep (e.g. the scheduler)
/// should call [`prune_sessions`] directly at its own cadence instead.
fn maybe_prune_sessions(db: &Db) {
    static LAST_PRUNE: AtomicI64 = AtomicI64::new(0);
    let now = Utc::now().timestamp();
    let last = LAST_PRUNE.load(Ordering::Relaxed);
    if now - last >= SESSION_PRUNE_INTERVAL_SECS
        && LAST_PRUNE
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let _ = prune_sessions(db, SESSION_PRUNE_MAX_AGE_HOURS);
    }
}

/// Delete expired or revoked sessions older than `max_age_hours`. Returns the
/// number of rows removed. Expiry/revocation timestamps are RFC3339 UTC
/// strings written by this module in one fixed format, so lexicographic
/// comparison is valid.
pub fn prune_sessions(db: &Db, max_age_hours: i64) -> Result<usize> {
    let now = Utc::now();
    let cutoff = now - chrono::Duration::hours(max_age_hours);
    let conn = db.get()?;
    Ok(conn.execute(
        "DELETE FROM sessions WHERE (revoked=1 OR expires_at < ?1) AND created_at < ?2",
        params![now.to_rfc3339(), cutoff.to_rfc3339()],
    )?)
}

/// Keep at most [`MAX_SESSIONS_PER_USER`] rows per user. When over the cap,
/// evict the oldest expired/revoked sessions first, then the oldest
/// still-active ones (`bad ASC` puts expired/revoked first; `id DESC` breaks
/// created_at ties so the just-inserted, newest session is never a victim).
fn enforce_session_cap(db: &Db, user_id: i64) -> Result<()> {
    // Count-then-evict stays atomic across pooled connections: two concurrent
    // creates could otherwise both count, both evict, and overshoot the cap
    // by one. BEGIN IMMEDIATE serializes writers like the old global mutex.
    let mut conn = db.get()?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sessions WHERE user_id=?1",
        [user_id],
        |r| r.get(0),
    )?;
    if total <= MAX_SESSIONS_PER_USER {
        return Ok(());
    }
    let excess = total - MAX_SESSIONS_PER_USER;
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "DELETE FROM sessions WHERE id IN (
            SELECT id FROM (
                SELECT id,
                       CASE WHEN revoked=1 OR expires_at < ?1 THEN 0 ELSE 1 END AS bad,
                       created_at
                FROM sessions WHERE user_id=?2
                ORDER BY bad ASC, created_at ASC, id DESC
                LIMIT ?3
            )
        )",
        params![now, user_id, excess],
    )?;
    tx.commit()?;
    Ok(())
}

/// Resolve a raw session cookie into a user. Also bumps last_seen.
pub fn user_from_session(db: &Db, raw: &str) -> Result<User> {
    let token = hash_token(raw);
    let conn = db.get()?;
    let row = conn
        .query_row(
            "SELECT user_id,expires_at,revoked FROM sessions WHERE token=?1",
            [&token],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("session not found"))?;
    let (user_id, expires_at, revoked) = row;
    if revoked != 0 {
        bail!("session revoked");
    }
    let exp = chrono::DateTime::parse_from_rfc3339(&expires_at)?;
    if exp < Utc::now() {
        bail!("session expired");
    }
    let now = Utc::now();
    // Throttle the last_seen write to at most once per 5 minutes per
    // session: the staleness condition lives in the UPDATE itself, so there
    // is no extra read and check-and-write stays atomic under the connection
    // mutex. Both sides of the comparison are RFC3339 UTC strings produced
    // by this code in one fixed format, so lexicographic order is valid.
    let _ = conn.execute(
        "UPDATE sessions SET last_seen=?1 WHERE token=?2 AND (last_seen IS NULL OR last_seen < ?3)",
        params![
            now.to_rfc3339(),
            token,
            (now - chrono::Duration::minutes(LAST_SEEN_THROTTLE_MINS)).to_rfc3339()
        ],
    );
    drop(conn);
    models::get_user(db, user_id)
}

pub fn revoke_session(db: &Db, raw: &str) -> Result<()> {
    let token = hash_token(raw);
    let conn = db.get()?;
    conn.execute("UPDATE sessions SET revoked=1 WHERE token=?1", [token])?;
    Ok(())
}

pub fn revoke_all_user_sessions(db: &Db, user_id: i64, except: Option<&str>) -> Result<()> {
    let except_token = except.map(hash_token);
    let conn = db.get()?;
    match except_token {
        Some(tok) => conn.execute(
            "UPDATE sessions SET revoked=1 WHERE user_id=?1 AND token<>?2",
            params![user_id, tok],
        )?,
        None => conn.execute("UPDATE sessions SET revoked=1 WHERE user_id=?1", [user_id])?,
    };
    Ok(())
}

// ---------------- TOTP ----------------

pub fn generate_totp_secret() -> Result<String> {
    use totp_rs::TOTP;
    // generate 20 random bytes (160-bit) then base32-encode
    let mut buf = [0u8; 20];
    rand::Rng::fill(&mut rand::thread_rng(), &mut buf);
    let b32 = base32_encode(&buf);
    let _totp = TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        buf.to_vec(),
        Some(TOTP_ISSUER.into()),
        "voltpanel".into(),
    )?;
    Ok(b32)
}

fn base32_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Result<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits: u32 = 0;
    let mut buffer: u32 = 0;
    let mut out = Vec::new();
    for c in s.chars() {
        let v = ALPHABET
            .iter()
            .position(|&a| a == c.to_ascii_uppercase() as u8)
            .ok_or_else(|| anyhow!("invalid base32 char"))? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Ok(out)
}

pub fn totp_from_secret(b64: &str) -> Result<totp_rs::TOTP> {
    use totp_rs::TOTP;
    let raw = base32_decode(b64.trim())?;
    Ok(TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        raw,
        Some(TOTP_ISSUER.into()),
        "voltpanel".into(),
    )?)
}

/// Constant-time byte-string comparison: the loop always touches every byte
/// and the exit condition never depends on where the strings diverge.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Which 30s window `code` verifies for, if any: the current one plus the two
/// adjacent ones (±1 step, matching the 30s period) so a skewed client clock
/// cannot lock the user out. Every comparison is constant-time. `None` means
/// the code did not verify.
fn totp_matched_window(secret_b64: &str, code: &str) -> Option<i64> {
    let totp = totp_from_secret(secret_b64).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    [now.saturating_sub(30), now, now.saturating_add(30)]
        .into_iter()
        .find(|&t| constant_time_eq(totp.generate(t as u64).as_bytes(), code.as_bytes()))
        .map(|t| t / 30)
}

pub fn verify_totp(secret_b64: &str, code: &str) -> bool {
    totp_matched_window(secret_b64, code).is_some()
}

/// Single-use tracking for TOTP codes: `(user_id, hashed secret)` → last
/// successfully verified 30s window. A code whose window equals the stored
/// one is a replay and is rejected, so a captured code cannot be reused
/// within its validity window. Keying by the hashed secret keeps the login
/// path (active secret) and enrollment (pending secret) apart. Failed
/// attempts record nothing, so a typo can be retried.
static TOTP_REPLAY: LazyLock<Mutex<HashMap<(i64, String), i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Above this many tracked entries, windows too old to ever verify again are
/// dropped so the map stays bounded.
const TOTP_REPLAY_TRACK_LIMIT: usize = 4096;

pub fn verify_totp_replay(user_id: i64, secret_b64: &str, code: &str) -> bool {
    let Some(window) = totp_matched_window(secret_b64, code) else {
        return false;
    };
    let key = (user_id, hash_token(secret_b64));
    let mut cache = TOTP_REPLAY.lock().unwrap_or_else(|p| p.into_inner());
    if cache.len() >= TOTP_REPLAY_TRACK_LIMIT {
        // `totp_matched_window` accepts at most now±1, so a window older than
        // now−2 can never match again; keep a small margin for slow clocks.
        let now_window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_secs() as i64) / 30)
            .unwrap_or(0);
        cache.retain(|_, last| *last >= now_window - 2);
    }
    match cache.get(&key) {
        Some(&last) if last == window => false,
        _ => {
            cache.insert(key, window);
            true
        }
    }
}

/// Server-side pending 2FA enrollment: `user_id` → (secret, expires_at). The
/// setup endpoint mints and stores the secret; confirm verifies the code
/// against it before persisting, so a client can never enroll a secret it
/// merely claims to have scanned. TTL ~10 minutes.
static PENDING_TOTP: LazyLock<Mutex<HashMap<i64, (String, i64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const PENDING_TOTP_TTL_SECS: i64 = 600;
const PENDING_TOTP_TRACK_LIMIT: usize = 4096;

pub fn store_pending_totp(user_id: i64, secret: &str) {
    let mut m = PENDING_TOTP.lock().unwrap_or_else(|p| p.into_inner());
    if m.len() >= PENDING_TOTP_TRACK_LIMIT {
        let now = Utc::now().timestamp();
        m.retain(|_, (_, exp)| *exp > now);
    }
    m.insert(
        user_id,
        (secret.to_string(), Utc::now().timestamp() + PENDING_TOTP_TTL_SECS),
    );
}

/// The unexpired pending secret for `user_id`, if any. Expired entries are
/// dropped as a side effect.
pub fn get_pending_totp(user_id: i64) -> Option<String> {
    let mut m = PENDING_TOTP.lock().unwrap_or_else(|p| p.into_inner());
    let now = Utc::now().timestamp();
    match m.get(&user_id) {
        Some((secret, exp)) if *exp > now => Some(secret.clone()),
        _ => {
            m.remove(&user_id);
            None
        }
    }
}

pub fn clear_pending_totp(user_id: i64) {
    PENDING_TOTP
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&user_id);
}

pub fn totp_uri(secret_b64: &str, username: &str) -> Result<String> {
    let totp = totp_from_secret(secret_b64)?;
    // Build the otpauth URI by hand: the account name belongs in the path
    // label (`issuer:user`), never as a `label=` query parameter, and the
    // query only carries secret/issuer/algorithm/digits/period.
    Ok(format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        percent_encode(TOTP_ISSUER),
        percent_encode(username),
        totp.get_secret_base32(),
        percent_encode(TOTP_ISSUER),
    ))
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------- Rate limiting ----------------

static WINDOW: AtomicU64 = AtomicU64::new(0);

/// Per-key token bucket: capacity is the per-minute limit and tokens refill
/// continuously at `limit/60` per second. A fixed 60s window resets at every
/// minute boundary, so a burst straddling two windows could spend the limit
/// twice; a bucket has no reset instant, so it caps any burst at the limit
/// no matter when it lands.
struct Bucket {
    tokens: f64,
    last_refill: i64,
}

static BUCKETS: LazyLock<Mutex<HashMap<String, Bucket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Above this many tracked keys, buckets idle for over an hour are dropped so
/// the map stays bounded.
const BUCKET_TRACK_LIMIT: usize = 4096;

pub fn rate_limit(db: &Db, cfg: &Config, key: &str) -> Result<bool> {
    let now = Utc::now().timestamp();
    let window = now / 60;
    // Prune stale windows once per minute boundary.
    if WINDOW.swap(window as u64, Ordering::Relaxed) != window as u64 {
        let conn = db.get()?;
        let _ = conn.execute(
            "DELETE FROM rate_limits WHERE window_start < ?1",
            [window - 2],
        );
    }
    let rate = cfg.security.rate_limit_per_min as f64 / 60.0;
    let capacity = cfg.security.rate_limit_per_min as f64;
    let mut buckets = BUCKETS.lock().unwrap_or_else(|p| p.into_inner());
    if buckets.len() >= BUCKET_TRACK_LIMIT {
        buckets.retain(|_, b| now - b.last_refill < 3600);
    }
    // Keep the DB counter current for /api/system/rate-limits visibility;
    // enforcement lives in the token bucket below. Once the in-memory tracker
    // is at its cap a brand-new key is not tracked, so skip its DB row too —
    // otherwise a rotating-IP flood inserts one row per unique key per minute
    // and rate_limits grows without bound. Tracked keys still bump normally.
    if buckets.contains_key(key) || buckets.len() < BUCKET_TRACK_LIMIT {
        let _ = models::bump_rate_limit(db, key, window)?;
    }
    let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
        tokens: capacity,
        last_refill: now,
    });
    let elapsed = (now - bucket.last_refill).max(0) as f64;
    bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
    bucket.last_refill = now;
    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        Ok(true)
    } else {
        Ok(false)
    }
}
pub fn window_now() -> i64 {
    Utc::now().timestamp() / 60
}

/// Drop all in-memory rate-limit buckets (admin reset). The DB view is
/// cleared separately via [`models::reset_rate_limits`].
pub fn reset_rate_limits() {
    BUCKETS.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn test_db() -> (Db, crate::config::Config, tempfile::TempDir) {
        let mut cfg = crate::config::Config::default();
        // Use the validate() floors so a test config stays loadable; hashing
        // a password with these costs is still fast enough for unit tests.
        cfg.security.argon2_cost = crate::config::MIN_ARGON2_COST;
        cfg.security.argon2_mem_kib = crate::config::MIN_ARGON2_MEM_KIB;
        let temp = tempfile::tempdir().unwrap();
        let db = crate::db::open(&temp.path().join("voltpanel.db").to_string_lossy()).unwrap();
        (db, cfg, temp)
    }

    #[test]
    fn last_seen_is_throttled_to_five_minutes() {
        let (db, cfg, _temp) = test_db();
        let hash = hash_password(&cfg, "password").unwrap();
        let user_id =
            models::create_user(&db, "admin", "admin@example.com", &hash, true, "en", "dark")
                .unwrap();
        let (raw, _) = create_session(&db, &cfg, user_id, "test", "127.0.0.1", false).unwrap();

        let read_last_seen = |conn: &rusqlite::Connection| -> Option<String> {
            conn.query_row(
                "SELECT last_seen FROM sessions WHERE user_id=?1",
                [user_id],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Fresh session: last_seen is NULL until the first resolve.
        assert_eq!(read_last_seen(&db.get().unwrap()), None);

        let user = user_from_session(&db, &raw).unwrap();
        assert_eq!(user.id, user_id);
        let first = read_last_seen(&db.get().unwrap()).expect("first resolve writes last_seen");

        // Second resolve inside the throttle window must not rewrite it.
        user_from_session(&db, &raw).unwrap();
        assert_eq!(read_last_seen(&db.get().unwrap()), Some(first));

        // A stale last_seen older than the throttle window is refreshed.
        let stale = (Utc::now() - Duration::minutes(LAST_SEEN_THROTTLE_MINS + 5)).to_rfc3339();
        db.get().unwrap()
            .execute(
                "UPDATE sessions SET last_seen=?1 WHERE user_id=?2",
                params![stale, user_id],
            )
            .unwrap();
        user_from_session(&db, &raw).unwrap();
        let refreshed = read_last_seen(&db.get().unwrap()).expect("stale last_seen is refreshed");
        assert_ne!(refreshed, stale);
        let refreshed_ts = chrono::DateTime::parse_from_rfc3339(&refreshed).unwrap();
        assert!(
            refreshed_ts > chrono::DateTime::parse_from_rfc3339(&stale).unwrap(),
            "refreshed last_seen must be newer than the stale value"
        );
    }

    #[test]
    fn sessions_are_capped_per_user() {
        let (db, cfg, _temp) = test_db();
        let user_id =
            models::create_user(&db, "cap", "cap@example.com", "x", true, "en", "dark").unwrap();
        for _ in 0..(MAX_SESSIONS_PER_USER + 5) {
            create_session(&db, &cfg, user_id, "t", "127.0.0.1", false).unwrap();
        }
        let total: i64 = db
            .get().unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1",
                [user_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, MAX_SESSIONS_PER_USER);
    }

    #[test]
    fn cap_prefers_evicting_expired_and_revoked_sessions() {
        let (db, cfg, _temp) = test_db();
        let user_id =
            models::create_user(&db, "evict", "evict@example.com", "x", true, "en", "dark")
                .unwrap();
        let now = Utc::now();
        let old = (now - Duration::days(2)).to_rfc3339();
        let expired = (now - Duration::hours(1)).to_rfc3339();
        let future = (now + Duration::days(1)).to_rfc3339();
        {
            let conn = db.get().unwrap();
            // Four expired/revoked rows plus eight still-active ones.
            for i in 0..4 {
                conn.execute(
                    "INSERT INTO sessions(token,user_id,user_agent,ip,created_at,expires_at,revoked) VALUES(?1,?2,'t','ip',?3,?4,1)",
                    params![format!("bad-{i}"), user_id, old, expired],
                )
                .unwrap();
            }
            for i in 0..8 {
                conn.execute(
                    "INSERT INTO sessions(token,user_id,user_agent,ip,created_at,expires_at,revoked) VALUES(?1,?2,'t','ip',?3,?4,0)",
                    params![format!("ok-{i}"), user_id, old, future],
                )
                .unwrap();
            }
        }
        // 13 rows for one user: the new session pushes 3 over the cap, and
        // all three evictions must come from the expired/revoked bucket.
        create_session(&db, &cfg, user_id, "t", "127.0.0.1", false).unwrap();
        let conn = db.get().unwrap();
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1",
                [user_id],
                |r| r.get(0),
            )
            .unwrap();
        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1 AND (revoked=1 OR expires_at < ?2)",
                params![user_id, Utc::now().to_rfc3339()],
                |r| r.get(0),
            )
            .unwrap();
        let active: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE user_id=?1 AND revoked=0",
                [user_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, MAX_SESSIONS_PER_USER);
        assert_eq!(
            bad, 1,
            "expired/revoked rows are evicted before active ones"
        );
        assert_eq!(active, 9);
    }

    #[test]
    fn prune_removes_old_expired_and_revoked_sessions() {
        let (db, _cfg, _temp) = test_db();
        let user_id =
            models::create_user(&db, "prune", "prune@example.com", "x", true, "en", "dark")
                .unwrap();
        let now = Utc::now();
        let old = (now - Duration::days(30)).to_rfc3339();
        {
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO sessions(token,user_id,created_at,expires_at,revoked) VALUES('old-expired',?1,?2,?3,0)",
                params![user_id, old, (now - Duration::hours(1)).to_rfc3339()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions(token,user_id,created_at,expires_at,revoked) VALUES('old-revoked',?1,?2,?3,1)",
                params![user_id, old, (now + Duration::days(1)).to_rfc3339()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions(token,user_id,created_at,expires_at,revoked) VALUES('fresh-expired',?1,?2,?3,0)",
                params![user_id, now.to_rfc3339(), (now - Duration::minutes(1)).to_rfc3339()],
            )
            .unwrap();
        }
        assert_eq!(prune_sessions(&db, 24 * 7).unwrap(), 2);
        let left: Vec<String> = {
            let conn = db.get().unwrap();
            let mut stmt = conn
                .prepare("SELECT token FROM sessions WHERE user_id=?1")
                .unwrap();
            let rows = stmt
                .query_map([user_id], |r| r.get::<_, String>(0))
                .unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(left, vec!["fresh-expired".to_string()]);
    }

    #[test]
    fn login_rehashes_weak_password_hash() {
        let (db, cfg, _temp) = test_db();
        // A pre-hardening hash: parameters below the current cfg floor.
        let mut weak_cfg = crate::config::Config::default();
        weak_cfg.security.argon2_cost = 1;
        weak_cfg.security.argon2_mem_kib = 8 * 1024;
        let weak_hash = hash_password(&weak_cfg, "hunter2").unwrap();
        assert!(hash_needs_upgrade(&cfg, &weak_hash));
        let user_id = models::create_user(
            &db,
            "upgrade",
            "upgrade@example.com",
            &weak_hash,
            true,
            "en",
            "dark",
        )
        .unwrap();
        let read_hash = |db: &Db, user_id: i64| -> String {
            db.get().unwrap()
                .query_row(
                    "SELECT password_hash FROM users WHERE id=?1",
                    [user_id],
                    |r| r.get::<_, String>(0),
                )
                .unwrap()
        };
        assert!(verify_password_with_upgrade(
            &db,
            &cfg,
            user_id,
            &read_hash(&db, user_id),
            "hunter2"
        ));
        let upgraded = read_hash(&db, user_id);
        assert_ne!(upgraded, weak_hash, "weak hash must be rehashed in place");
        assert!(!hash_needs_upgrade(&cfg, &upgraded));
        assert!(verify_password(&upgraded, "hunter2"));
        // A failed login must not rewrite the stored hash.
        assert!(!verify_password_with_upgrade(
            &db, &cfg, user_id, &upgraded, "wrong"
        ));
        assert_eq!(read_hash(&db, user_id), upgraded);
        // A hash already at the floor is left untouched.
        assert!(verify_password_with_upgrade(
            &db, &cfg, user_id, &upgraded, "hunter2"
        ));
        assert_eq!(read_hash(&db, user_id), upgraded);
    }

    #[test]
    fn rate_limit_allows_up_to_limit_per_minute() {
        let (db, mut cfg, _temp) = test_db();
        cfg.security.rate_limit_per_min = 10;
        for _ in 0..10 {
            assert!(rate_limit(&db, &cfg, "burst").unwrap());
        }
        assert!(!rate_limit(&db, &cfg, "burst").unwrap());
    }

    #[test]
    fn rate_limit_caps_bursts_across_window_boundaries() {
        let (db, mut cfg, _temp) = test_db();
        cfg.security.rate_limit_per_min = 10;
        // 2x the limit in rapid succession. A fixed 60s window allows exactly
        // 2x when the burst straddles a minute boundary; the token bucket has
        // no reset instant, so it must cap total allowed at the capacity and
        // reject the very next request with no rollover to reset it.
        let mut allowed = 0;
        for _ in 0..20 {
            if rate_limit(&db, &cfg, "boundary-burst").unwrap() {
                allowed += 1;
            }
        }
        assert!(
            allowed <= 10,
            "burst must never exceed the per-minute limit"
        );
        assert!(!rate_limit(&db, &cfg, "boundary-burst").unwrap());
    }

    #[test]
    fn totp_codes_are_single_use_per_user_and_secret() {
        let secret = generate_totp_secret().unwrap();
        // A code valid for the current 30s window (the code generator and the
        // verifier share the clock, so `generate(now)` is the current code).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let code = totp_from_secret(&secret).unwrap().generate(now);
        let code = code.as_str();

        // First use succeeds and records the window.
        assert!(verify_totp_replay(7, &secret, code));
        // Replaying the same code in the same window is rejected.
        assert!(!verify_totp_replay(7, &secret, code));
        // A failed attempt records nothing: the same code still works for a
        // user who has not used it yet.
        assert!(!verify_totp_replay(8, &secret, "000000"));
        assert!(verify_totp_replay(8, &secret, code));
        // Replay tracking is per secret too: a fresh enrollment secret is not
        // blocked by the active secret's used window.
        let fresh = generate_totp_secret().unwrap();
        let fresh_code = totp_from_secret(&fresh).unwrap().generate(now);
        assert!(verify_totp_replay(7, &fresh, fresh_code.as_str()));
    }

    #[test]
    fn pending_totp_secret_round_trips_until_cleared() {
        let secret = generate_totp_secret().unwrap();
        store_pending_totp(7, &secret);
        assert_eq!(get_pending_totp(7).as_deref(), Some(secret.as_str()));
        // A user without a pending secret sees none.
        assert_eq!(get_pending_totp(8), None);
        clear_pending_totp(7);
        assert_eq!(get_pending_totp(7), None);
        // Storing again overwrites the previous pending secret.
        let second = generate_totp_secret().unwrap();
        store_pending_totp(7, &second);
        assert_eq!(get_pending_totp(7).as_deref(), Some(second.as_str()));
    }

    #[test]
    fn recovery_codes_are_unique_high_entropy_and_normalizable() {
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), RECOVERY_CODE_COUNT);
        // All distinct: a collision would let one code open two accounts.
        let unique: std::collections::HashSet<&String> = codes.iter().collect();
        assert_eq!(unique.len(), codes.len(), "recovery codes must be unique");
        // Each code is 10 chars drawn from the confusable-free alphabet.
        let valid: std::collections::HashSet<char> =
            RECOVERY_ALPHABET.iter().map(|b| *b as char).collect();
        for code in &codes {
            assert_eq!(code.len(), 10, "code must be 10 chars: {code}");
            assert!(
                code.chars().all(|c| valid.contains(&c)),
                "code must draw from the recovery alphabet: {code}"
            );
            // Codes are generated already-normalized: hashing must be stable.
            assert_eq!(
                hash_recovery_code(code),
                hash_recovery_code(&normalize_recovery_code(code)),
                "generated codes must already be in canonical form"
            );
        }
        // Normalization strips separators and uppercases, so a UI grouping
        // like ABCDE-FGHJK and a lowercased entry are the same code.
        assert_eq!(normalize_recovery_code("abcde-fghjk"), "ABCDEFGHJK");
        assert_eq!(normalize_recovery_code(" ab cd "), "ABCD");
        assert_eq!(
            hash_recovery_code("abcde-fghjk"),
            hash_recovery_code("ABCDEFGHJK")
        );
        assert_eq!(
            hash_recovery_code("abcde-fghjk"),
            hash_token("ABCDEFGHJK"),
            "hash is the plain SHA-256 of the canonical code"
        );
    }

    #[test]
    fn recovery_code_hashes_never_leak_plaintext_or_collide() {
        let codes = generate_recovery_codes();
        let hashes: std::collections::HashSet<String> =
            codes.iter().map(|c| hash_recovery_code(c)).collect();
        assert_eq!(hashes.len(), codes.len(), "hashes must not collide");
        for code in &codes {
            let h = hash_recovery_code(code);
            assert_eq!(h.len(), 64, "hash must be SHA-256 hex");
            assert_ne!(h, *code, "stored hash must never equal the plaintext");
            assert!(
                !hashes.iter().any(|other| other == code),
                "no stored hash may reveal a plaintext code"
            );
        }
    }
}