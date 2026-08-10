//! Workload blueprint endpoints: CRUD + import/export.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState, AuthUser};
use crate::capability::Capability;
use crate::db::blocking;
use crate::models::{self, Blueprint, BlueprintInput, User};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

// ---- DB execution off the async worker ----
//
// Handlers must not run SQLite work on a Tokio worker thread (see servers.rs
// for the full contract). `Db::call` runs the module-owned SQL in
// `categories`; `blocking` runs the pool-based `models`/`services::blueprint`
// functions — including the ed25519 sign/verify in the registry endpoints,
// whose CPU work belongs off the worker too. Never hold a pooled connection
// across an `.await`; split into separate blocking units instead.
/// Cap on blueprint import documents; a giant JSON body must not OOM the
/// parser at the API boundary.
const MAX_IMPORT_BYTES: usize = crate::services::blueprint::MAX_BLUEPRINT_IMPORT_BYTES;
/// Convert a service-level "not found" into a 404; everything else stays a 500.
fn not_found_or(e: anyhow::Error, needle: &str) -> ApiError {
    if e.to_string().contains(needle) {
        ApiError::not_found(e.to_string())
    } else {
        e.into()
    }
}

/// True when the caller may see the `default_value` of `user_viewable=false`
/// inputs: root admins and API keys whose scope covers every server. Mirrors
/// the servers.rs gate. `server_id` 0 can never be a real server, so `allows`
/// selects exactly the keys with an unrestricted server scope (global
/// StartupSecrets).
fn can_view_hidden_defaults(u: &User) -> bool {
    u.root_admin
        || u
            .key_scope
            .as_ref()
            .is_some_and(|s| s.allows(0, Capability::StartupSecrets))
}

/// Blank the `default_value` of non-viewable inputs unless the caller holds
/// StartupSecrets authority. The declaration itself stays visible — only the
/// secret default is blanked, so the redacted document round-trips cleanly.
fn redact_hidden_defaults(mut b: Blueprint, u: &User) -> Blueprint {
    if !can_view_hidden_defaults(u) {
        for v in &mut b.variables {
            if !v.user_viewable {
                v.default_value.clear();
            }
        }
    }
    b
}

/// Apply the same gate to a served registry package document: blank the
/// `default_value` of hidden inputs on the SERVED copy. Runs after signature
/// verification, so the digest/signature still describe the on-disk artifact
/// and the redacted copy is never re-signed.
fn redact_package_doc(mut doc: serde_json::Value, u: &User) -> serde_json::Value {
    if can_view_hidden_defaults(u) {
        return doc;
    }
    let Some(vars) = doc
        .get_mut("blueprint")
        .and_then(|b| b.get_mut("variables"))
        .and_then(|v| v.as_array_mut())
    else {
        return doc;
    };
    for v in vars {
        // Redact unless the variable is explicitly viewable — a package that
        // omits `user_viewable` is treated as hidden.
        if v.get("user_viewable") == Some(&serde_json::Value::Bool(true)) {
            continue;
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("default_value".into(), serde_json::Value::String(String::new()));
        }
    }
    doc
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let blueprints = blocking(state.db.clone(), move |db| models::list_blueprints(&db))
        .await?
        .into_iter()
        .map(|b| redact_hidden_defaults(b, &u))
        .collect::<Vec<_>>();
    Ok(data(serde_json::to_value(blueprints)?))
}

pub async fn get(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Blueprint>> {
    let b = blocking(state.db.clone(), move |db| models::get_blueprint(&db, id))
        .await
        .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(Json(redact_hidden_defaults(b, &u)))
}


#[derive(Deserialize)]
pub struct CreateBlueprintReq {
    pub name: String,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub runtime_hint: Option<String>,
    pub startup: String,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<BlueprintInput>>,
    pub default_config: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateBlueprintReq>,
) -> ApiResult<Json<Blueprint>> {
    crate::services::blueprint::validate_inputs(req.variables.as_deref().unwrap_or(&[]))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let b = blocking(state.db.clone(), move |db| {
        let id = models::create_blueprint(
            &db,
            &uuid,
            &req.name,
            req.description.as_deref().unwrap_or(""),
            req.author.as_deref().unwrap_or(""),
            req.category.as_deref().unwrap_or("generic"),
            req.runtime_hint.as_deref().unwrap_or("native"),
            &req.startup,
            req.default_config.as_deref(),
            req.install_script.as_deref(),
            req.variables.as_deref().unwrap_or(&[]),
            req.stop_command.as_deref().unwrap_or("stop"),
        )?;
        models::get_blueprint(&db, id)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(Json(b))
}

#[derive(Deserialize)]
pub struct UpdateBlueprintReq {
    pub name: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub category: Option<String>,
    pub runtime_hint: Option<String>,
    pub startup: Option<String>,
    pub install_script: Option<String>,
    pub stop_command: Option<String>,
    pub variables: Option<Vec<BlueprintInput>>,
    pub default_config: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateBlueprintReq>,
) -> ApiResult<Json<Blueprint>> {
    let mut e = blocking(state.db.clone(), move |db| models::get_blueprint(&db, id))
        .await
        .map_err(|e| not_found_or(e, "blueprint not found"))?;
    let before = e.clone();
    if let Some(vars) = &req.variables {
        crate::services::blueprint::validate_inputs(vars)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(v) = req.name {
        e.name = v;
    }
    if let Some(v) = req.description {
        e.description = v;
    }
    if let Some(v) = req.author {
        e.author = v;
    }
    if let Some(v) = req.category {
        e.category = v;
    }
    if let Some(v) = req.runtime_hint {
        e.runtime_hint = v;
    }
    if let Some(v) = req.startup {
        e.startup = v;
    }
    if let Some(v) = req.install_script {
        e.install_script = Some(v);
    }
    if let Some(v) = req.stop_command {
        e.stop_command = v;
    }
    if let Some(v) = req.variables {
        e.variables = v;
    }
    if let Some(v) = req.default_config {
        e.default_config = Some(v);
    }
    // No-op PATCH: content is identical, so no revision is written and the
    // version counter does not move.
    if crate::services::blueprint::content_equals(&before, &e) {
        return Ok(Json(e));
    }
    // Snapshot and apply in one transaction so the recorded revision always
    // matches the content being replaced and history stays coherent.
    let username = a.0.username;
    let e = blocking(state.db.clone(), move |db| {
        crate::services::blueprint::snapshot_and_update(&db, id, &e, &username, "")?;
        Ok(e)
    })
    .await?;
    Ok(Json(e))
}

pub async fn delete(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    // References from soft-deleted servers still own the blueprint row, so the
    // guard counts every server and surfaces a clean 409 instead of a 500.
    let used = blocking(state.db.clone(), move |db| models::blueprint_references(&db, id)).await?;
    if used > 0 {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            format!("blueprint is used by {used} workspace(s)"),
        ));
    }
    blocking(state.db.clone(), move |db| models::delete_blueprint(&db, id)).await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ImportReq {
    pub json: String,
}

pub async fn import(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<ImportReq>,
) -> ApiResult<Json<Blueprint>> {
    if req.json.len() > MAX_IMPORT_BYTES {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "blueprint import exceeds 1 MiB limit",
        ));
    }
    let parsed =
        crate::services::blueprint::parse_blueprint_json(&req.json)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let b = blocking(state.db.clone(), move |db| {
        let id = models::create_blueprint(
            &db,
            &uuid,
            &parsed.name,
            &parsed.description,
            &parsed.author,
            &parsed.category,
            &parsed.runtime_hint,
            &parsed.startup,
            parsed
                .config
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap_or_default())
                .as_deref(),
            parsed.install.as_ref().map(|i| i.script.clone()).as_deref(),
            &parsed.variables,
            &parsed.stop,
        )?;
        models::get_blueprint(&db, id)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(Json(b))
}

#[derive(Serialize)]
pub struct ExportResp {
    pub json: String,
}

pub async fn export(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<ExportResp>> {
    let e = redact_hidden_defaults(
        blocking(state.db.clone(), move |db| models::get_blueprint(&db, id))
            .await
            .map_err(|e| not_found_or(e, "blueprint not found"))?,
        &u,
    );
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "name": e.name,
        "description": e.description,
        "author": e.author,
        "category": e.category,
        "runtime_hint": e.runtime_hint,
        "startup": e.startup,
        "config": e
            .default_config
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()?,
        "install": e.install_script.map(|s| serde_json::json!({ "script": s })),
        "variables": e.variables,
        "stop": e.stop_command,
    }))?;
    Ok(Json(ExportResp { json: out }))
}

pub async fn categories(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let out = state
        .db
        .call(|conn| {
            let mut stmt =
                conn.prepare("SELECT DISTINCT category FROM blueprints ORDER BY category")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await?;
    Ok(data(serde_json::json!(out)))
}

// ---------------- Versioning & drift ----------------

pub async fn revisions(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = blocking(state.db.clone(), move |db| {
        crate::services::blueprint::list_revisions(&db, id)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(data(serde_json::to_value(rows)?))
}

pub async fn revision_detail(
    State(state): State<AppState>,
    _a: AdminUser,
    Path((id, version)): Path<(i64, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = blocking(state.db.clone(), move |db| {
        crate::services::blueprint::revision_snapshot(&db, id, version)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(data(snap))
}

#[derive(Deserialize)]
pub struct RollbackReq {
    pub version: i64,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn rollback(
    State(state): State<AppState>,
    a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<RollbackReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let username = a.0.username;
    let version = match req.note {
        Some(note) => {
            let username = username.clone();
            blocking(state.db.clone(), move |db| {
                crate::services::blueprint::rollback_with_note(&db, id, req.version, &username, &note)
            })
            .await
            .map_err(|e| not_found_or(e, "blueprint not found"))?
        }
        None => blocking(state.db.clone(), move |db| {
            crate::services::blueprint::rollback(&db, id, req.version, &username)
        })
        .await
        .map_err(|e| not_found_or(e, "blueprint not found"))?,
    };
    Ok(data(serde_json::json!({ "version": version })))
}

pub async fn drift(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = blocking(state.db.clone(), move |db| {
        crate::services::blueprint::drift_for_blueprint(&db, id)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(data(serde_json::to_value(rows)?))
}

#[derive(Deserialize)]
pub struct PinReq {
    pub version: i64,
}

pub async fn pin(
    State(state): State<AppState>,
    user: AuthUser,
    Path(server_id): Path<i64>,
    Json(req): Json<PinReq>,
) -> ApiResult<Json<serde_json::Value>> {
    super::require_capability(&state, &user, server_id, Capability::StartupUpdate).await?;
    blocking(state.db.clone(), move |db| {
        crate::services::blueprint::pin_server(&db, server_id, req.version)
    })
    .await
    .map_err(|e| not_found_or(e, "server not found"))?;
    Ok(data(serde_json::json!({ "ok": true })))
}

use crate::services::blueprint as bp;

// ---------------- VoltSpec Registry ----------------

/// Registry root on disk: `<blueprints_dir>/registry`.
fn registry_root(state: &AppState) -> std::path::PathBuf {
    crate::services::blueprint::registry_root(&state.cfg.paths.blueprints_dir)
}

/// Signing posture of this panel, derived from the `registry.signing_key`
/// setting (hex-encoded ed25519 seed; empty or absent = signing disabled).
async fn registry_signing_status(state: &AppState) -> ApiResult<serde_json::Value> {
    let seed = blocking(state.db.clone(), move |db| {
        models::get_setting(&db, "registry.signing_key")
    })
    .await?
    .filter(|s| !s.trim().is_empty());
    let Some(seed) = seed else {
        return Ok(serde_json::json!({
            "enabled": false,
            "public_key": serde_json::Value::Null,
            "fingerprint": serde_json::Value::Null,
        }));
    };
    let key = bp::signing_key_from_hex(&seed).map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("registry signing key is misconfigured: {e}"),
        )
    })?;
    let pk = bp::public_key_hex(&key);
    Ok(serde_json::json!({
        "enabled": true,
        "public_key": pk,
        "fingerprint": bp::public_key_fingerprint(&pk),
    }))
}

/// GET /api/blueprints/registry — catalog of published packages plus which of
/// them are already installed locally and the panel's signing posture.
pub async fn registry_list(
    State(state): State<AppState>,
    _u: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let root = registry_root(&state);
    let (packages, local) = blocking(state.db.clone(), move |_db| {
        let packages = bp::list_registry_packages(&root)?;
        let local = bp::local_registry_installs(&root)?;
        Ok((packages, local))
    })
    .await?;
    let signing = registry_signing_status(&state).await?;
    Ok(data(serde_json::json!({
        "packages": packages,
        "local": local,
        "signing": signing,
    })))
}

#[derive(Deserialize)]
pub struct RegistryPublishReq {
    /// Local blueprint id whose latest revision becomes a package.
    pub id: i64,
}

/// POST /api/blueprints/registry/publish — publish the latest revision of a
/// blueprint, signed when a signing key is configured.
pub async fn registry_publish(
    State(state): State<AppState>,
    a: AdminUser,
    Json(req): Json<RegistryPublishReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = registry_root(&state);
    let id = req.id;
    let b = blocking(state.db.clone(), move |db| models::get_blueprint(&db, id))
        .await
        .map_err(|e| not_found_or(e, "blueprint not found"))?;
    let actor_id = a.0.id;
    let actor_name = a.0.username;
    let (version, signed, package_id) = blocking(state.db.clone(), move |db| {
        // The current revision number lives on the blueprints row; the
        // revision machinery owns that counter, so read it where the rest of
        // the versioning code does.
        let conn = db.get()?;
        let version: i64 = conn.query_row(
            "SELECT version FROM blueprints WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        let seed = models::get_setting(&db, "registry.signing_key")?
            .filter(|s| !s.trim().is_empty());
        let mut doc = bp::build_package_doc(&b, version, &actor_name);
        bp::publish_package(&root, &mut doc, seed.as_deref())?;
        let signed = bp::package_is_signed(&doc);
        models::audit(
            &db,
            Some(actor_id),
            "blueprint.publish",
            &format!("blueprint #{} ({})", id, b.name),
            "",
            if signed { "signed" } else { "unsigned" },
        )?;
        let package_id = doc
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((version, signed, package_id))
    })
    .await?;
    Ok(Json(serde_json::json!({
        "id": package_id,
        "version": version,
        "signed": signed,
        "warning": if signed {
            serde_json::Value::Null
        } else {
            serde_json::json!("no registry.signing_key configured — package published unsigned")
        },
    })))
}

#[derive(Deserialize)]
pub struct RegistryImportReq {
    /// Install a package already published on this panel's registry.
    pub id: Option<String>,
    pub version: Option<i64>,
    /// Install a package fetched from a remote registry URL (SSRF-guarded).
    pub url: Option<String>,
}

/// POST /api/blueprints/registry/import — install a package into the local
/// blueprint store, recording provenance. A bad signature on a signed package
/// is a hard error; an unsigned package installs with a visible warning.
pub async fn registry_import(
    State(state): State<AppState>,
    a: AdminUser,
    Json(req): Json<RegistryImportReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = registry_root(&state);
    let (doc, source_url) = match (req.id.as_deref(), req.url.as_deref()) {
        (Some(id), None) => {
            let version = req.version.ok_or_else(|| {
                ApiError::bad_request("version is required when importing by package id")
            })?;
            let id = id.to_string();
            let load_root = root.clone();
            let doc = blocking(state.db.clone(), move |_db| {
                bp::load_registry_package(&load_root, &id, version)
            })
            .await
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
            (doc, None)
        }
        (None, Some(url)) => (
            bp::fetch_registry_package(url, bp::MAX_BLUEPRINT_IMPORT_BYTES)
                .await
                .map_err(|e| ApiError::bad_request(e.to_string()))?,
            Some(url.to_string()),
        ),
        _ => {
            return Err(ApiError::bad_request(
                "provide either { id, version } or { url }",
            ))
        }
    };
    let signed = bp::package_is_signed(&doc);
    let blueprint_doc = doc
        .get("blueprint")
        .ok_or_else(|| ApiError::bad_request("registry package carries no blueprint document"))?;
    let blueprint_json = serde_json::to_string(blueprint_doc)?;
    let parsed = bp::parse_blueprint_json(&blueprint_json)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let uuid = uuid::Uuid::new_v4().to_string();
    let package_id = doc
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let version = doc.get("version").and_then(|v| v.as_i64()).unwrap_or(1);
    let prov = bp::PackageProvenance {
        package_id: package_id.clone(),
        version,
        source_url,
        public_key: doc
            .get("public_key")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        signature: doc
            .get("signature")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        verified: signed,
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    let actor_id = a.0.id;
    let created = blocking(state.db.clone(), move |db| {
        let id = models::create_blueprint(
            &db,
            &uuid,
            &parsed.name,
            &parsed.description,
            &parsed.author,
            &parsed.category,
            &parsed.runtime_hint,
            &parsed.startup,
            parsed
                .config
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap_or_default())
                .as_deref(),
            parsed.install.as_ref().map(|i| i.script.clone()).as_deref(),
            &parsed.variables,
            &parsed.stop,
        )?;
        bp::record_import_provenance(&root, &uuid, &prov)?;
        models::audit(
            &db,
            Some(actor_id),
            "blueprint.import",
            &format!("registry {package_id}@v{version}"),
            "",
            if signed { "signed" } else { "unsigned" },
        )?;
        models::get_blueprint(&db, id)
    })
    .await
    .map_err(|e| not_found_or(e, "blueprint not found"))?;
    Ok(Json(serde_json::json!({
        "blueprint": created,
        "warning": if signed {
            serde_json::Value::Null
        } else {
            serde_json::json!("package is unsigned — installed without signature verification")
        },
    })))
}

/// GET /api/blueprints/registry/package/:id/:version — the raw package
/// document, signature-verified before serving, so remote panels can fetch it
/// by URL and install it through their own SSRF-guarded import. Hidden-input
/// default values are redacted on the served copy for callers without
/// StartupSecrets authority, like the list/get/export endpoints.
pub async fn registry_package_get(
    State(state): State<AppState>,
    AuthUser(u): AuthUser,
    Path((id, version)): Path<(String, i64)>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = registry_root(&state);
    let doc = blocking(state.db.clone(), move |_db| {
        bp::load_registry_package(&root, &id, version)
    })
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(redact_package_doc(doc, &u)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn not_found_or_maps_blueprint_missing_to_404() {
        let e = anyhow::anyhow!("blueprint not found");
        let err = not_found_or(e, "blueprint not found");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.message, "blueprint not found");
    }

    #[test]
    fn not_found_or_passes_other_errors_through_as_500() {
        let e = anyhow::anyhow!("boom");
        let err = not_found_or(e, "blueprint not found");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn test_user(root_admin: bool, key_scope: Option<crate::services::keys::KeyScope>) -> User {
        User {
            id: 1,
            username: "u".into(),
            email: "u@u".into(),
            avatar: String::new(),
            language: "en".into(),
            theme: "dark".into(),
            root_admin,
            active: true,
            twofa_secret: None,
            twofa_enabled: false,
            about: String::new(),
            created_at: "t".into(),
            updated_at: "t".into(),
            key_scope,
        }
    }

    fn bp_with_secret() -> Blueprint {
        let var = |name: &str, env: &str, default: &str, viewable: bool| BlueprintInput {
            name: name.into(),
            description: String::new(),
            env_var: env.into(),
            default_value: default.into(),
            user_viewable: viewable,
            user_editable: viewable,
            required: false,
            kind: crate::models::InputKind::Text {
                max_len: None,
                pattern: None,
            },
        };
        Blueprint {
            id: 1,
            uuid: "u1".into(),
            name: "n".into(),
            description: String::new(),
            author: String::new(),
            category: "generic".into(),
            runtime_hint: "native".into(),
            startup: "echo hi".into(),
            default_config: None,
            install_script: None,
            variables: vec![
                var("Visible", "VISIBLE", "visible", true),
                var("Secret", "SECRET", "hunter2", false),
            ],
            stop_command: "stop".into(),
            created_at: "t".into(),
            updated_at: "t".into(),
        }
    }

    #[test]
    fn redact_hidden_defaults_blanks_secrets_for_plain_user() {
        let out = redact_hidden_defaults(bp_with_secret(), &test_user(false, None));
        assert_eq!(out.variables[0].default_value, "visible");
        assert_eq!(out.variables[1].default_value, "");
    }

    #[test]
    fn redact_hidden_defaults_keeps_secrets_for_admin() {
        let out = redact_hidden_defaults(bp_with_secret(), &test_user(true, None));
        assert_eq!(out.variables[1].default_value, "hunter2");
    }

    #[test]
    fn redact_hidden_defaults_keeps_secrets_for_global_startup_secrets_key() {
        let scope = crate::services::keys::KeyScope {
            capabilities: vec![Capability::StartupSecrets],
            wildcard: false,
            server_ids: vec![],
        };
        let out = redact_hidden_defaults(bp_with_secret(), &test_user(false, Some(scope)));
        assert_eq!(out.variables[1].default_value, "hunter2");
    }

    #[test]
    fn redact_hidden_defaults_blanks_secrets_for_server_scoped_key() {
        let scope = crate::services::keys::KeyScope {
            capabilities: vec![Capability::StartupSecrets],
            wildcard: false,
            server_ids: vec![7],
        };
        let out = redact_hidden_defaults(bp_with_secret(), &test_user(false, Some(scope)));
        assert_eq!(out.variables[1].default_value, "");
    }

    #[test]
    fn redact_package_doc_blanks_secrets_for_plain_user() {
        let doc = bp::build_package_doc(&bp_with_secret(), 1, "alice");
        let out = redact_package_doc(doc.clone(), &test_user(false, None));
        let vars = out["blueprint"]["variables"].as_array().unwrap();
        assert_eq!(vars[0]["default_value"], "visible");
        assert_eq!(vars[1]["default_value"], "");
        // Redaction never touches the digest/signature of the artifact.
        assert_eq!(out["digest"], doc["digest"]);
    }

    #[test]
    fn redact_package_doc_keeps_secrets_for_admin() {
        let doc = bp::build_package_doc(&bp_with_secret(), 1, "alice");
        let out = redact_package_doc(doc, &test_user(true, None));
        assert_eq!(out["blueprint"]["variables"][1]["default_value"], "hunter2");
    }

    #[test]
    fn redact_package_doc_keeps_secrets_for_global_startup_secrets_key() {
        let scope = crate::services::keys::KeyScope {
            capabilities: vec![Capability::StartupSecrets],
            wildcard: false,
            server_ids: vec![],
        };
        let doc = bp::build_package_doc(&bp_with_secret(), 1, "alice");
        let out = redact_package_doc(doc, &test_user(false, Some(scope)));
        assert_eq!(out["blueprint"]["variables"][1]["default_value"], "hunter2");
    }

    #[tokio::test]
    async fn package_get_redacts_secrets_for_plain_user_and_keeps_for_admin() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        cfg.paths.blueprints_dir = tmp.path().join("blueprints");
        let hub = Arc::new(crate::services::console::ConsoleHub::new(cfg.clone()));
        let procs = Arc::new(crate::services::proc::ProcManager::new(db.clone(), hub.clone()));
        let state = AppState {
            db,
            cfg: cfg.clone(),
            procs,
            hub,
            notifier: Arc::new(crate::services::proc::Notifier::new()),
            monitor: Arc::new(crate::services::Monitor::new()),
            node_client: Arc::new(crate::services::node::NodeClient::new().unwrap()),
            node_nonces: Arc::new(crate::services::node::NonceCache::default()),
            running: Arc::new(AtomicBool::new(true)),
        };
        let root = bp::registry_root(&cfg.paths.blueprints_dir);
        let mut doc = bp::build_package_doc(&bp_with_secret(), 1, "alice");
        bp::publish_package(&root, &mut doc, None).unwrap();
        let id = bp::package_id_from_name(&bp_with_secret().name);

        let served = registry_package_get(
            State(state.clone()),
            AuthUser(test_user(false, None)),
            Path((id.clone(), 1)),
        )
        .await
        .unwrap()
        .0;
        let vars = served["blueprint"]["variables"].as_array().unwrap();
        assert_eq!(vars[0]["default_value"], "visible");
        assert_eq!(vars[1]["default_value"], "");
        // Served copy still carries the original artifact digest.
        assert_eq!(served["digest"], doc["digest"]);

        let served = registry_package_get(
            State(state),
            AuthUser(test_user(true, None)),
            Path((id, 1)),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(served["blueprint"]["variables"][1]["default_value"], "hunter2");
    }
}