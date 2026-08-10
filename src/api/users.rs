//! Auth + user management endpoints.
use super::{
    client_ip, data, ok, request_is_tls, AdminUser, ApiError, ApiResult, AppState, AuthUser,
};
#[cfg(test)]
use super::request_is_https;
use crate::auth;
use crate::db::blocking;
use crate::models::{self, User};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread. Module-owned
// SQL rides `Db::call(|conn| ...)`; pool-based `models`/`auth` functions
// (they do their own `pool.get()` and cannot run inside a `db.call` closure
// without a nested checkout) ride `blocking(...)` on Tokio's blocking pool.
// One rule: never hold a pooled connection across an `.await`.

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default)]
    pub totp_code: Option<String>,
}

#[derive(Serialize)]
pub struct LoginResp {
    pub user: Option<User>,
    pub needs_2fa: bool,
    pub expires_at: Option<String>,
}

/// First-tier login rate limit, applied *before* the request body is read so
/// an unauthenticated flood cannot make axum buffer a full body (up to
/// `max_body_mb` per connection) for every attempt. IP-keyed only; the
/// per-account + per-IP buckets inside [`login`] remain as the second tier.
pub async fn pre_login_rate_limit(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let cfg = state.cfg.clone();
    let key = format!("login-ip-pre:{ip}");
    let allowed = blocking(state.db.clone(), move |db| {
        auth::rate_limit(&db, &cfg, &key)
    })
    .await;
    match allowed {
        Ok(true) => next.run(request).await,
        Ok(false) => ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many login attempts; try again later",
        )
        .into_response(),
        Err(e) => ApiError::from(e).into_response(),
    }
}

pub async fn login(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<axum::response::Response> {
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let account_key = auth::hash_token(&req.username.trim().to_ascii_lowercase());
    let cfg = state.cfg.clone();
    let ip_key = format!("login-ip:{ip}");
    let acct_key = format!("login-account:{account_key}");
    let (ip_allowed, account_allowed) = blocking(state.db.clone(), move |db| {
        Ok((
            auth::rate_limit(&db, &cfg, &ip_key)?,
            auth::rate_limit(&db, &cfg, &acct_key)?,
        ))
    })
    .await?;
    if !ip_allowed || !account_allowed {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many login attempts; try again later",
        ));
    }
    let username = req.username.trim().to_string();
    let user = blocking(state.db.clone(), move |db| {
        models::get_user_by_name(&db, &username)
    })
    .await
    .ok();
    let user_id = user.as_ref().map(|u| u.id);
    let stored = state
        .db
        .call(move |conn| {
            let by_id = user_id.and_then(|id| {
                conn.query_row(
                    "SELECT password_hash FROM users WHERE id=?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .ok()
            });
            Ok(by_id
                .or_else(|| {
                    conn.query_row(
                        "SELECT password_hash FROM users ORDER BY id LIMIT 1",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                })
                .unwrap_or_else(|| "$argon2id$v=19$m=65536,t=3,p=1$c29tZXNhbHQ$ti7JgaHWDvWq5h3rP9nG9YoXNfBPf7buKh7VTHO3M2o".to_string()))
        })
        .await?;
    let cfg = state.cfg.clone();
    let stored2 = stored.clone();
    let password = req.password.clone();
    // argon2 work for BOTH branches rides the blocking pool: an
    // unknown-username flood must not stall Tokio workers with dummy
    // hashes (the exact scenario pre_login_rate_limit throttles).
    let (password_ok, user) = blocking(state.db.clone(), move |db| {
        let password_ok = match &user {
            // Real user: harden a weak stored hash in place on successful login.
            Some(u) => {
                auth::verify_password_with_upgrade(&db, &cfg, u.id, &stored2, &password)
            }
            // No such user: verify against the dummy hash so the argon2 work is
            // constant whether or not the account exists.
            None => auth::verify_password(&stored, &password),
        };
        Ok((password_ok, user))
    })
    .await?;
    let user = user.ok_or_else(|| ApiError::unauthorized("invalid credentials"))?;
    if !password_ok || !user.active {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if state.cfg.features.enable_2fa {
        if let Some(secret) = &user.twofa_secret {
            let code = req.totp_code.as_deref().unwrap_or("");
            if !auth::verify_totp_replay(user.id, secret, code) {
                return Ok(Json(LoginResp {
                    user: None,
                    needs_2fa: true,
                    expires_at: None,
                })
                .into_response());
            }
        }
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let uid = user.id;
    let remember = req.remember;
    let cfg = state.cfg.clone();
    let (raw, expires) = blocking(state.db.clone(), move |db| {
        let (raw, expires) = auth::create_session(&db, &cfg, uid, &ua, &ip, remember)?;
        models::audit(
            &db,
            Some(uid),
            "login",
            "session",
            &ip,
            "user logged in",
        )?;
        Ok((raw, expires))
    })
    .await?;
    let cookie = {
        let mut c = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax",
            crate::auth::SESSION_COOKIE,
            raw
        );
        if request_is_tls(&state, peer, &headers) {
            c.push_str("; Secure");
        }
        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&expires) {
            let max_age = (exp.with_timezone(&chrono::Utc) - chrono::Utc::now())
                .num_seconds()
                .max(0);
            if req.remember {
                c.push_str(&format!("; Max-Age={max_age}"));
            }
        }
        c
    };
    let mut resp = (Json(LoginResp {
        user: Some(user),
        needs_2fa: false,
        expires_at: Some(expires),
    }))
    .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    AuthUser(u): AuthUser,
) -> ApiResult<axum::response::Response> {
    let raw = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';').find(|c| {
                c.trim_start()
                    .starts_with(&format!("{}=", crate::auth::SESSION_COOKIE))
            })
        })
        .and_then(|c| c.split_once('='))
        .map(|(_, v)| v.trim().to_string());
    if let Some(raw) = raw {
        let _ = blocking(state.db.clone(), move |db| {
            auth::revoke_session(&db, &raw)
        })
        .await;
    }
    let uid = u.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(uid),
            "logout",
            "session",
            "",
            "user logged out",
        )
    })
    .await?;
    let mut resp = ok(serde_json::json!({ "ok": true })).into_response();
    let clear = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        crate::auth::SESSION_COOKIE
    );
    if let Ok(v) = axum::http::HeaderValue::from_str(&clear) {
        resp.headers_mut().insert(axum::http::header::SET_COOKIE, v);
    }
    Ok(resp)
}

pub async fn me(State(state): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let uid = u.id;
    let json = blocking(state.db.clone(), move |db| {
        let user = models::get_user(&db, uid)?;
        user_json_with_squads(&db, &user)
    })
    .await?;
    Ok(Json(json))
}

#[derive(Deserialize)]
pub struct UpdateProfileReq {
    pub email: Option<String>,
    pub avatar: Option<String>,
    pub language: Option<String>,
    pub theme: Option<String>,
    pub about: Option<String>,
}

pub async fn update_profile(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<UpdateProfileReq>,
) -> ApiResult<Json<User>> {
    let uid = u.id;
    let mut u = blocking(state.db.clone(), move |db| models::get_user(&db, uid)).await?;
    if let Some(email) = req.email {
        let email = email.trim();
        if email.len() > 254
            || !email.contains('@')
            || email.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(ApiError::bad_request("invalid email"));
        }
        u.email = email.to_string();
    }
    if let Some(avatar) = req.avatar {
        let avatar = avatar.trim();
        if avatar.len() > 2048
            || (!avatar.is_empty()
                && !avatar.starts_with("https://")
                && !avatar.starts_with("http://")
                && !avatar.starts_with("data:image/"))
        {
            return Err(ApiError::bad_request("invalid avatar URL"));
        }
        u.avatar = avatar.to_string();
    }
    if let Some(language) = req.language {
        if !matches!(language.as_str(), "en" | "id") {
            return Err(ApiError::bad_request("unsupported language"));
        }
        u.language = language;
    }
    if let Some(theme) = req.theme {
        if !matches!(theme.as_str(), "dark" | "light") {
            return Err(ApiError::bad_request("unsupported theme"));
        }
        u.theme = theme;
    }
    if let Some(about) = req.about {
        if about.len() > 1000 {
            return Err(ApiError::bad_request("about is too long"));
        }
        u.about = about;
    }
    let u = blocking(state.db.clone(), move |db| {
        models::update_user(&db, &u)?;
        Ok(u)
    })
    .await?;
    Ok(Json(u))
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub current: String,
    pub new: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = state.cfg.clone();
    let key = format!("password:{}", u.id);
    if !blocking(state.db.clone(), move |db| auth::rate_limit(&db, &cfg, &key)).await? {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many verification attempts; try again later",
        ));
    }
    if req.new.len() < state.cfg.security.password_min_len {
        return Err(ApiError::bad_request(format!(
            "password must be at least {} chars",
            state.cfg.security.password_min_len
        )));
    }
    let stored = state
        .db
        .call(move |conn| {
            Ok(conn.query_row(
                "SELECT password_hash FROM users WHERE id=?1",
                [u.id],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await?;
    if !auth::verify_password(&stored, &req.current) {
        return Err(ApiError::bad_request("current password incorrect"));
    }
    let hash = auth::hash_password(&state.cfg, &req.new)?;
    blocking(state.db.clone(), move |db| {
        models::set_password(&db, u.id, &hash)?;
        auth::revoke_all_user_sessions(&db, u.id, None)?;
        models::audit(
            &db,
            Some(u.id),
            "password_change",
            "user",
            "",
            "password changed",
        )?;
        Ok(())
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- 2FA ----------------

#[derive(Serialize)]
pub struct TotpSetupResp {
    pub secret: String,
    pub otpauth_uri: String,
    pub qr_b64: String,
}

pub async fn setup_2fa(
    State(_state): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<TotpSetupResp>> {
    if !_state.cfg.features.enable_2fa {
        return Err(ApiError::not_found("2FA is disabled"));
    }
    let secret = auth::generate_totp_secret()?;
    auth::store_pending_totp(u.id, &secret);
    let uri = auth::totp_uri(&secret, &u.username)?;
    // render QR as PNG (frontend shows it as data:image/png)
    let qr = qrcode::QrCode::new(uri.clone())?;
    let img = qr
        .render::<image::Luma<u8>>()
        .min_dimensions(256, 256)
        .build();
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img).write_to(&mut cursor, image::ImageFormat::Png)?;
    let b64 = STANDARD.encode(cursor.into_inner());
    Ok(Json(TotpSetupResp {
        secret: secret.clone(),
        otpauth_uri: uri,
        qr_b64: b64,
    }))
}

#[derive(Deserialize)]
pub struct TotpConfirmReq {
    pub secret: String,
    pub code: String,
}

#[derive(Deserialize)]
pub struct TotpDisableReq {
    pub code: String,
}

pub async fn confirm_2fa(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<TotpConfirmReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = state.cfg.clone();
    let key = format!("2fa-confirm:{}", u.id);
    if !blocking(state.db.clone(), move |db| auth::rate_limit(&db, &cfg, &key)).await? {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many verification attempts; try again later",
        ));
    }
    if !state.cfg.features.enable_2fa {
        return Err(ApiError::not_found("2FA is disabled"));
    }
    let Some(pending) = auth::get_pending_totp(u.id) else {
        return Err(ApiError::bad_request(
            "2FA setup expired; start enrollment again",
        ));
    };
    if !auth::verify_totp_replay(u.id, &pending, &req.code) {
        return Err(ApiError::bad_request("invalid code"));
    }
    let secret = pending.clone();
    blocking(state.db.clone(), move |db| {
        models::set_twofa_secret(&db, u.id, Some(&secret))
    })
    .await?;
    auth::clear_pending_totp(u.id);
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(u.id),
            "2fa_enable",
            "user",
            "",
            "2FA enabled",
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn disable_2fa(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<TotpDisableReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = state.cfg.clone();
    let key = format!("2fa-disable:{}", u.id);
    if !blocking(state.db.clone(), move |db| auth::rate_limit(&db, &cfg, &key)).await? {
        return Err(ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "too many verification attempts; try again later",
        ));
    }
    let user = blocking(state.db.clone(), move |db| models::get_user(&db, u.id)).await?;
    let secret = user
        .twofa_secret
        .ok_or_else(|| ApiError::bad_request("2FA is not enabled"))?;
    if !auth::verify_totp_replay(u.id, &secret, &req.code) {
        return Err(ApiError::bad_request("invalid code"));
    }
    blocking(state.db.clone(), move |db| {
        models::set_twofa_secret(&db, u.id, None)
    })
    .await?;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(u.id),
            "2fa_disable",
            "user",
            "",
            "2FA disabled",
        )
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

// ---------------- Admin: user management ----------------

#[derive(Deserialize)]
pub struct CreateUserReq {
    pub username: String,
    pub email: String,
    pub password: String,
    pub root_admin: bool,
}

/// True when `err` is a SQLite UNIQUE/constraint violation, which is how a
/// concurrent `admin_create_user` (or an email collision) surfaces: the
/// sequential `get_user_by_name` pre-check cannot see the other insert.
fn create_constraint_violation(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

pub async fn admin_create_user(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateUserReq>,
) -> ApiResult<Json<User>> {
    if req.username.len() < 3 {
        return Err(ApiError::bad_request("username too short"));
    }
    if !req.email.contains('@') || req.email.trim().is_empty() {
        return Err(ApiError::bad_request("invalid email"));
    }
    if req.password.len() < state.cfg.security.password_min_len {
        return Err(ApiError::bad_request("password too short"));
    }
    let username = req.username.clone();
    let username2 = username.clone();
    if blocking(state.db.clone(), move |db| {
        models::get_user_by_name(&db, &username)
    })
    .await
    .is_ok()
    {
        return Err(ApiError::bad_request("username taken"));
    }
    let hash = auth::hash_password(&state.cfg, &req.password)?;
    let email = req.email.trim().to_string();
    let root_admin = req.root_admin;
    let locale = state.cfg.general.locale.clone();
    let id = match blocking(state.db.clone(), move |db| {
        models::create_user(&db, &username2, &email, &hash, root_admin, &locale, "dark")
    })
    .await
    {
        Ok(id) => id,
        Err(e) if create_constraint_violation(&e) => {
            // Lost the check-then-insert race, or the email already exists:
            // users.username and users.email are both UNIQUE.
            return Err(ApiError::conflict("username or email already in use"));
        }
        Err(e) => return Err(e.into()),
    };
    let user = blocking(state.db.clone(), move |db| models::get_user(&db, id)).await?;
    Ok(Json(user))
}

/// User JSON enriched with read-only squad memberships (`squads`), for the
/// detail surfaces (me, admin user detail/list). The field is always present,
/// empty for users with no memberships.
fn user_json_with_squads(db: &crate::db::Db, user: &User) -> anyhow::Result<serde_json::Value> {
    let mut v = serde_json::to_value(user)
        .map_err(|e| anyhow::anyhow!("user serialization failed: {e}"))?;
    v["squads"] = serde_json::to_value(models::squad_memberships_for(db, user.id)?)?;
    Ok(v)
}

pub async fn admin_list_users(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let (users, memberships) = blocking(state.db.clone(), move |db| {
        let users = models::list_users(&db)?;
        let ids: Vec<i64> = users.iter().map(|u| u.id).collect();
        let memberships = models::squad_memberships_for_users(&db, &ids)?;
        Ok((users, memberships))
    })
    .await?;
    let out = users
        .iter()
        .map(|u| {
            let mut v = serde_json::to_value(u).map_err(|e| {
                tracing::warn!("user serialization failed: {e}");
                ApiError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error",
                )
            })?;
            v["squads"] =
                serde_json::to_value(memberships.get(&u.id).cloned().unwrap_or_default())
                    .map_err(|e| {
                        tracing::warn!("squad memberships serialization failed: {e}");
                        ApiError::new(
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "internal server error",
                        )
                    })?;
            Ok(v)
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(serde_json::json!(out)))
}

#[derive(Deserialize)]
pub struct AdminUpdateUserReq {
    pub email: Option<String>,
    pub root_admin: Option<bool>,
    pub active: Option<bool>,
    pub language: Option<String>,
    pub theme: Option<String>,
    pub about: Option<String>,
    pub reset_password: Option<String>,
}

pub async fn admin_update_user(
    State(state): State<AppState>,
    _a: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(req): Json<AdminUpdateUserReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut u = blocking(state.db.clone(), move |db| models::get_user(&db, id)).await?;
    if u.id == 1 && (req.root_admin == Some(false) || req.active == Some(false)) {
        return Err(ApiError::bad_request(
            "cannot demote or disable the primary admin",
        ));
    }
    if let Some(email) = req.email {
        let email = email.trim();
        if email.len() > 254
            || !email.contains('@')
            || email.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(ApiError::bad_request("invalid email"));
        }
        u.email = email.to_string();
    }
    if let Some(ra) = req.root_admin {
        u.root_admin = ra;
    }
    if let Some(active) = req.active {
        u.active = active;
    }
    if let Some(lang) = req.language {
        u.language = lang;
    }
    if let Some(theme) = req.theme {
        u.theme = theme;
    }
    if let Some(about) = req.about {
        u.about = about;
    }
    if let Some(pw) = req.reset_password {
        if pw.len() < state.cfg.security.password_min_len {
            return Err(ApiError::bad_request("password too short"));
        }
        let hash = auth::hash_password(&state.cfg, &pw)?;
        blocking(state.db.clone(), move |db| {
            models::set_password(&db, u.id, &hash)
        })
        .await?;
        let _ = blocking(state.db.clone(), move |db| {
            auth::revoke_all_user_sessions(&db, u.id, None)
        })
        .await;
    }
    let json = blocking(state.db.clone(), move |db| {
        models::update_user(&db, &u)?;
        user_json_with_squads(&db, &u)
    })
    .await?;
    Ok(Json(json))
}

/// Admin user detail: the full user record plus read-only squad memberships.
pub async fn admin_get_user(
    State(state): State<AppState>,
    _a: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let json = blocking(state.db.clone(), move |db| {
        let u = models::get_user(&db, id)?;
        user_json_with_squads(&db, &u)
    })
    .await?;
    Ok(data(json))
}

pub async fn admin_delete_user(
    State(state): State<AppState>,
    a: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    if id == 1 {
        return Err(ApiError::bad_request("cannot delete the first admin"));
    }
    let (u, count) = blocking(state.db.clone(), move |db| {
        let u = models::get_user(&db, id)?;
        let count = models::count_all_servers_by_user(&db, id)?;
        Ok((u, count))
    })
    .await?;
    if count != 0 {
        return Err(ApiError::bad_request(
            "transfer or purge this user's workspaces before deleting the account",
        ));
    }
    let admin_id = a.0.id;
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(admin_id),
            "user_delete",
            &format!("user #{id}"),
            "",
            &format!("{} id={}", u.username, u.id),
        )?;
        models::delete_user(&db, id)?;
        Ok(())
    })
    .await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn admin_sessions(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let out = state
        .db
        .call(move |conn| {
            let mut stmt = conn.prepare("SELECT token,user_id,user_agent,ip,created_at,expires_at,revoked,remember,last_seen FROM sessions ORDER BY id DESC LIMIT 100")?;
            let rows = stmt.query_map([], |r| {
                Ok(serde_json::json!({
                    "token": r.get::<_, String>(0)?.get(0..12).unwrap_or(""),
                    "user_id": r.get::<_, i64>(1)?,
                    "user_agent": r.get::<_, String>(2)?,
                    "ip": r.get::<_, String>(3)?,
                    "created_at": r.get::<_, String>(4)?,
                    "expires_at": r.get::<_, String>(5)?,
                    "revoked": r.get::<_, i64>(6)?,
                    "remember": r.get::<_, i64>(7)?,
                    "last_seen": r.get::<_, Option<String>>(8)?,
                }))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?;
    Ok(Json(serde_json::json!({ "data": out })))
}

pub async fn admin_revoke_session(
    State(state): State<AppState>,
    _a: AdminUser,
    axum::extract::Path(token_prefix): axum::extract::Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    if token_prefix.len() != 12 || !token_prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("invalid session token prefix"));
    }
    let token = token_prefix.to_ascii_lowercase();
    let n = state
        .db
        .call(move |conn| {
            Ok(conn.execute(
                "UPDATE sessions SET revoked=1 WHERE substr(token,1,12)=?1",
                [token],
            )?)
        })
        .await?;
    Ok(ok(serde_json::json!({ "revoked": n })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::delete;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn forwarded_login_metadata_is_trusted_only_from_trusted_proxy() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.7, 127.0.0.1".parse().unwrap());
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        let proxy: std::net::SocketAddr = "127.0.0.1:45000".parse().unwrap();
        let remote: std::net::SocketAddr = "198.51.100.9:45000".parse().unwrap();

        // Loopback peers stay implicitly trusted, exactly as before.
        assert_eq!(client_ip(proxy, &headers, &[]), "203.0.113.7");
        assert!(request_is_https(proxy, &headers, &[]));
        // Untrusted remote peers can never influence forwarded metadata.
        assert_eq!(client_ip(remote, &headers, &[]), "198.51.100.9");
        assert!(!request_is_https(remote, &headers, &[]));
        // A trusted non-loopback proxy is honored via configuration.
        let trusted = vec!["10.0.0.0/8".parse().unwrap()];
        let nat_proxy: std::net::SocketAddr = "10.0.0.5:45000".parse().unwrap();
        assert_eq!(client_ip(nat_proxy, &headers, &trusted), "203.0.113.7");
        assert!(request_is_https(nat_proxy, &headers, &trusted));
    }

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        let hub = Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(crate::services::proc::ProcManager::new(db.clone(), hub.clone()));
        let state = AppState {
            db,
            cfg,
            procs,
            hub,
            notifier: Arc::new(crate::services::proc::Notifier::new()),
            monitor: Arc::new(crate::services::Monitor::new()),
            node_client: Arc::new(crate::services::node::NodeClient::new().unwrap()),
            node_nonces: Arc::new(crate::services::node::NonceCache::default()),
            running: Arc::new(AtomicBool::new(true)),
        };
        (tmp, state)
    }

    /// A root admin with a live session cookie.
    fn seed_admin(state: &AppState, uuid: &str) -> (i64, String) {
        let user_id = models::create_user(
            &state.db,
            &format!("admin-{uuid}"),
            &format!("admin-{uuid}@x.io"),
            "h",
            true,
            "en",
            "dark",
        )
        .unwrap();
        let (raw, _) = crate::auth::create_session(
            &state.db, &state.cfg, user_id, "test-agent", "127.0.0.1", false,
        )
        .unwrap();
        (user_id, format!("vp_session={raw}"))
    }

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/users", axum::routing::post(admin_create_user))
            .route("/api/users/:id", delete(admin_delete_user))
            .with_state(state)
    }

    async fn request(
        state: AppState,
        method: &str,
        uri: &str,
        cookie: &str,
    ) -> (StatusCode, serde_json::Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        let req = builder.body(Body::empty()).unwrap();
        let response = router(state).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value = if bytes.is_empty() {
            serde_json::json!(null)
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null))
        };
        (status, value)
    }

    #[tokio::test]
    async fn admin_delete_user_audits_with_admin_actor_and_target_id() {
        let (_tmp, state) = test_state();
        let (admin_id, cookie) = seed_admin(&state, "del-audit");
        let target_id = models::create_user(
            &state.db,
            "victim-del",
            "victim-del@x.io",
            "h",
            false,
            "en",
            "dark",
        )
        .unwrap();

        let (status, _) = request(
            state.clone(),
            "DELETE",
            &format!("/api/users/{target_id}"),
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let conn = state.db.get().unwrap();
        let (user_id, action, target, details): (Option<i64>, String, String, String) = conn
            .query_row(
                "SELECT user_id,action,target,details FROM audit_logs ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            user_id,
            Some(admin_id),
            "actor must be the admin, not the deleted user"
        );
        assert_eq!(action, "user_delete");
        assert_eq!(target, format!("user #{target_id}"));
        assert!(
            details.contains("victim-del"),
            "details must name the target user: {details}"
        );
        assert!(
            details.contains(&target_id.to_string()),
            "details must carry the target id: {details}"
        );
    }

    #[tokio::test]
    async fn admin_create_user_email_collision_returns_409() {
        let (_tmp, state) = test_state();
        let (_admin_id, cookie) = seed_admin(&state, "collide-admin");
        let create = |username: String, email: String| {
            let state = state.clone();
            let cookie = cookie.clone();
            async move {
                let body = serde_json::json!({
                    "username": username,
                    "email": email,
                    "password": "correct-horse-battery",
                    "root_admin": false,
                });
                let req = Request::builder()
                    .method("POST")
                    .uri("/api/users")
                    .header("content-type", "application/json")
                    .header("cookie", cookie)
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let resp = router(state).oneshot(req).await.unwrap();
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::json!(null)))
            }
        };
        let (first, _) = create("first-collide".to_string(), "same-email@x.io".to_string()).await;
        assert_eq!(first, StatusCode::OK);
        // A different username with the same email slips past the username
        // pre-check and hits the UNIQUE(email) constraint: the race that used
        // to surface as a 500 must now be a 409 conflict.
        let (second, _) = create("second-collide".to_string(), "same-email@x.io".to_string()).await;
        assert_eq!(second, StatusCode::CONFLICT);
    }
    #[tokio::test]
    async fn admin_user_detail_and_me_include_squad_memberships() {
        let (_tmp, state) = test_state();
        let (admin_id, cookie) = seed_admin(&state, "sq-detail");
        let member_id = models::create_user(
            &state.db, "squad-guy", "sqg@x.io", "h", false, "en", "dark",
        )
        .unwrap();
        let squad_id = models::create_squad(&state.db, "Platform", admin_id).unwrap();
        let admin = models::get_user(&state.db, admin_id).unwrap();
        models::add_squad_member(
            &state.db,
            squad_id,
            member_id,
            crate::capability::Role::Manager,
            &admin,
        )
        .unwrap();

        let router = axum::Router::new()
            .route(
                "/api/admin/users/:id",
                axum::routing::get(admin_get_user),
            )
            .route("/api/me", axum::routing::get(me))
            .with_state(state.clone());

        // Admin user detail: squads as [{id,name,role}].
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/admin/users/{member_id}"))
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["id"], member_id);
        assert_eq!(body["data"]["squads"].as_array().unwrap().len(), 1);
        assert_eq!(body["data"]["squads"][0]["id"], squad_id);
        assert_eq!(body["data"]["squads"][0]["name"], "Platform");
        assert_eq!(body["data"]["squads"][0]["role"], "manager");

        // A member's own /api/me carries the same read-only shape.
        let (raw, _) = crate::auth::create_session(
            &state.db, &state.cfg, member_id, "test-agent", "127.0.0.1", false,
        )
        .unwrap();
        let member_cookie = format!("vp_session={raw}");
        let req = Request::builder()
            .method("GET")
            .uri("/api/me")
            .header("cookie", &member_cookie)
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["squads"].as_array().unwrap().len(), 1);
        assert_eq!(body["squads"][0]["id"], squad_id);
        assert_eq!(body["squads"][0]["role"], "manager");
    }
}