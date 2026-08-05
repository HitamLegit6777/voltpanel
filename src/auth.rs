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
use std::sync::atomic::{AtomicU64, Ordering};

pub const SESSION_COOKIE: &str = "vp_session";
pub const TOTP_ISSUER: &str = "VoltPanel";

pub fn hash_password(cfg: &Config, plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let params = argon2::Params::new(
        cfg.security.argon2_mem_kib,
        2,
        cfg.security.argon2_cost,
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

pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill(&mut buf[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
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
        cfg.web.session_ttl_hours * 7 // remember me = 7 days from base TTL (hours)
    } else {
        cfg.web.session_ttl_hours
    };
    let created = Utc::now();
    let expires = created + chrono::Duration::hours(ttl as i64);
    let conn = db.lock();
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
    Ok((raw, expires.to_rfc3339()))
}

/// Resolve a raw session cookie into a user. Also bumps last_seen.
pub fn user_from_session(db: &Db, raw: &str) -> Result<User> {
    let token = hash_token(raw);
    let conn = db.lock();
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
    let _ = conn.execute(
        "UPDATE sessions SET last_seen=?1 WHERE token=?2",
        params![Utc::now().to_rfc3339(), token],
    );
    drop(conn);
    models::get_user(db, user_id)
}

pub fn revoke_session(db: &Db, raw: &str) -> Result<()> {
    let token = hash_token(raw);
    let conn = db.lock();
    conn.execute("UPDATE sessions SET revoked=1 WHERE token=?1", [token])?;
    Ok(())
}

pub fn revoke_all_user_sessions(db: &Db, user_id: i64, except: Option<&str>) -> Result<()> {
    let except_token = except.map(hash_token);
    let conn = db.lock();
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

pub fn verify_totp(secret_b64: &str, code: &str) -> bool {
    let Ok(totp) = totp_from_secret(secret_b64) else {
        return false;
    };
    totp.check_current(code).unwrap_or(false)
}

pub fn totp_uri(secret_b64: &str, username: &str) -> Result<String> {
    let totp = totp_from_secret(secret_b64)?;
    let url = totp.get_url();
    // inject issuer + account into the generated URL
    let sep = if url.contains('?') { '&' } else { '?' };
    Ok(format!(
        "{url}{sep}issuer={}&label={}",
        percent_encode(TOTP_ISSUER),
        percent_encode(username)
    ))
}

fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

// ---------------- Rate limiting ----------------

static WINDOW: AtomicU64 = AtomicU64::new(0);

pub fn rate_limit(db: &Db, cfg: &Config, key: &str) -> Result<()> {
    let now = Utc::now().timestamp();
    let window = now / 60;
    // prune stale windows once per minute boundary
    if WINDOW.swap(window as u64, Ordering::Relaxed) != window as u64 {
        let conn = db.lock();
        let _ = conn.execute(
            "DELETE FROM rate_limits WHERE window_start < ?1",
            [window - 2],
        );
    }
    let count = models::bump_rate_limit(db, key, window)?;
    if count > cfg.security.rate_limit_per_min as i64 {
        bail!("rate limit exceeded, try again later");
    }
    Ok(())
}

pub fn window_now() -> i64 {
    Utc::now().timestamp() / 60
}
