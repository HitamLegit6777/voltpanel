//! Auth + user management endpoints.
use super::{ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::auth;
use crate::models::{self, User};
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};

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
    pub user: User,
    pub needs_2fa: bool,
    pub session: Option<String>,
    pub expires_at: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<axum::response::Response> {
    let ip = peer.ip().to_string();
    auth::rate_limit(&state.db, &state.cfg, &format!("login:{ip}"))?;
    let user = models::get_user_by_name(&state.db, &req.username.trim())
        .map_err(|_| ApiError::unauthorized("invalid credentials"))?;
    if !user.active {
        return Err(ApiError::forbidden("account disabled"));
    }
    let stored = {
        let conn = state.db.lock();
        conn.query_row(
            "SELECT password_hash FROM users WHERE id=?1",
            [user.id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|_| ApiError::unauthorized("invalid credentials"))?
    };
    if !auth::verify_password(&stored, &req.password) {
        return Err(ApiError::unauthorized("invalid credentials"));
    }
    if state.cfg.features.enable_2fa {
        if let Some(secret) = &user.twofa_secret {
            let code = req.totp_code.as_deref().unwrap_or("");
            if !auth::verify_totp(secret, code) {
                return Ok(Json(LoginResp {
                    user,
                    needs_2fa: true,
                    session: None,
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
    let (raw, expires) =
        auth::create_session(&state.db, &state.cfg, user.id, &ua, &ip, req.remember)?;
    models::audit(
        &state.db,
        Some(user.id),
        "login",
        "session",
        &ip,
        "user logged in",
    )?;
    let cookie = {
        let mut c = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax",
            crate::auth::SESSION_COOKIE,
            raw
        );
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
        user,
        needs_2fa: false,
        session: Some(raw),
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
        let _ = auth::revoke_session(&state.db, &raw);
    }
    models::audit(
        &state.db,
        Some(u.id),
        "logout",
        "session",
        "",
        "user logged out",
    )?;
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

pub async fn me(State(state): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<User>> {
    Ok(Json(models::get_user(&state.db, u.id)?))
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
    let mut u = models::get_user(&state.db, u.id)?;
    if let Some(email) = req.email {
        if !email.contains('@') {
            return Err(ApiError::bad_request("invalid email"));
        }
        u.email = email;
    }
    if let Some(avatar) = req.avatar {
        u.avatar = avatar;
    }
    if let Some(language) = req.language {
        u.language = language;
    }
    if let Some(theme) = req.theme {
        u.theme = theme;
    }
    if let Some(about) = req.about {
        u.about = about;
    }
    models::update_user(&state.db, &u)?;
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
    if req.new.len() < state.cfg.security.password_min_len {
        return Err(ApiError::bad_request(format!(
            "password must be at least {} chars",
            state.cfg.security.password_min_len
        )));
    }
    let stored = {
        let conn = state.db.lock();
        conn.query_row("SELECT password_hash FROM users WHERE id=?1", [u.id], |r| {
            r.get::<_, String>(0)
        })?
    };
    if !auth::verify_password(&stored, &req.current) {
        return Err(ApiError::bad_request("current password incorrect"));
    }
    let hash = auth::hash_password(&state.cfg, &req.new)?;
    models::set_password(&state.db, u.id, &hash)?;
    auth::revoke_all_user_sessions(&state.db, u.id, None)?;
    models::audit(
        &state.db,
        Some(u.id),
        "password_change",
        "user",
        "",
        "password changed",
    )?;
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
    let secret = auth::generate_totp_secret()?;
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

pub async fn confirm_2fa(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<TotpConfirmReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if !auth::verify_totp(&req.secret, &req.code) {
        return Err(ApiError::bad_request("invalid code"));
    }
    models::set_twofa_secret(&state.db, u.id, Some(&req.secret))?;
    models::audit(
        &state.db,
        Some(u.id),
        "2fa_enable",
        "user",
        "",
        "2FA enabled",
    )?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn disable_2fa(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Json(req): Json<TotpConfirmReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if !auth::verify_totp(&req.secret, &req.code) {
        return Err(ApiError::bad_request("invalid code"));
    }
    models::set_twofa_secret(&state.db, u.id, None)?;
    models::audit(
        &state.db,
        Some(u.id),
        "2fa_disable",
        "user",
        "",
        "2FA disabled",
    )?;
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
    if models::get_user_by_name(&state.db, &req.username).is_ok() {
        return Err(ApiError::bad_request("username taken"));
    }
    let hash = auth::hash_password(&state.cfg, &req.password)?;
    let id = models::create_user(
        &state.db,
        &req.username,
        &req.email.trim(),
        &hash,
        req.root_admin,
        &state.cfg.general.locale,
        "dark",
    )?;
    Ok(Json(models::get_user(&state.db, id)?))
}

pub async fn admin_list_users(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<Vec<User>>> {
    Ok(Json(models::list_users(&state.db)?))
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
) -> ApiResult<Json<User>> {
    let mut u = models::get_user(&state.db, id)?;
    if u.id == 1 && (req.root_admin == Some(false) || req.active == Some(false)) {
        return Err(ApiError::bad_request(
            "cannot demote or disable the primary admin",
        ));
    }
    if let Some(email) = req.email {
        if !email.contains('@') || email.trim().is_empty() {
            return Err(ApiError::bad_request("invalid email"));
        }
        u.email = email;
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
        models::set_password(&state.db, u.id, &hash)?;
        let _ = auth::revoke_all_user_sessions(&state.db, u.id, None);
    }
    models::update_user(&state.db, &u)?;
    Ok(Json(u))
}

pub async fn admin_delete_user(
    State(state): State<AppState>,
    _a: AdminUser,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    if id == 1 {
        return Err(ApiError::bad_request("cannot delete the first admin"));
    }
    let u = models::get_user(&state.db, id)?;
    models::audit(
        &state.db,
        Some(u.id),
        "user_delete",
        "user",
        "",
        "user deleted by admin",
    )?;
    models::delete_user(&state.db, id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn admin_sessions(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
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
    Ok(Json(serde_json::json!({ "data": out })))
}

pub async fn admin_revoke_session(
    State(state): State<AppState>,
    _a: AdminUser,
    axum::extract::Path(token_prefix): axum::extract::Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let n = conn.execute(
        "UPDATE sessions SET revoked=1 WHERE token LIKE ?1",
        [format!("{token_prefix}%")],
    )?;
    Ok(ok(serde_json::json!({ "revoked": n })))
}
