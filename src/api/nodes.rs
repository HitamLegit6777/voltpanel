//! Multi-node administration, enrollment, heartbeat, placement and health APIs.
use super::{data, ok, AdminUser, ApiError, ApiResult, AppState};
use crate::node_protocol::{self, NodeHeartbeat, SignedHeaders};
use crate::nodes;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct CreateNodeRequest {
    pub name: String,
    pub public_url: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub async fn list(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let values: Vec<_> = nodes::list(&state.db)?
        .into_iter()
        .map(|n| {
            let mut v = serde_json::to_value(&n).unwrap_or_default();
            v["online"] = serde_json::json!(n.online());
            v["available_memory_mb"] = serde_json::json!(n.available_memory_mb());
            v["available_disk_mb"] = serde_json::json!(n.available_disk_mb());
            v
        })
        .collect();
    Ok(data(serde_json::json!(values)))
}

pub async fn get(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = nodes::get(&state.db, id)?;
    let events = nodes::events(&state.db, id, 100)?;
    Ok(Json(serde_json::json!({
        "node": n,
        "online": n.online(),
        "available_memory_mb": n.available_memory_mb(),
        "available_disk_mb": n.available_disk_mb(),
        "events": events,
    })))
}

pub async fn create(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateNodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.name.trim().len() < 2 {
        return Err(ApiError::bad_request("node name too short"));
    }
    let parsed =
        url::Url::parse(&req.public_url).map_err(|_| ApiError::bad_request("invalid node URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("node URL must use http or https"));
    }
    let n = nodes::create(
        &state.db,
        req.name.trim(),
        &req.public_url,
        &req.location,
        &req.tags,
    )?;
    nodes::record_event(
        &state.db,
        n.id,
        "info",
        "created",
        "node record created",
        &serde_json::json!({}),
    )?;
    Ok(Json(serde_json::json!({
        "node": n,
        "enrollment_token": n.enrollment_token,
        "secret": n.secret,
    })))
}

#[derive(Debug, Deserialize)]
pub struct UpdateNodeRequest {
    pub name: String,
    pub public_url: String,
    pub enabled: bool,
    pub maintenance: bool,
    pub schedulable: bool,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub memory_limit_mb: i64,
    #[serde(default)]
    pub disk_limit_mb: i64,
    #[serde(default)]
    pub memory_overallocate: i64,
    #[serde(default)]
    pub disk_overallocate: i64,
}

pub async fn update(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateNodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = nodes::update(
        &state.db,
        id,
        &req.name,
        &req.public_url,
        req.enabled,
        req.maintenance,
        req.schedulable,
        &req.location,
        &req.tags,
        req.memory_limit_mb,
        req.disk_limit_mb,
        req.memory_overallocate,
        req.disk_overallocate,
    )?;
    nodes::record_event(
        &state.db,
        id,
        "info",
        "updated",
        "node configuration updated",
        &serde_json::json!({}),
    )?;
    Ok(Json(serde_json::to_value(n)?))
}

pub async fn delete(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    nodes::delete(&state.db, id)?;
    Ok(ok(serde_json::json!({ "ok": true })))
}

pub async fn rotate_secret(
    State(_state): State<AppState>,
    _a: AdminUser,
    Path(_id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    Err(ApiError::new(
        axum::http::StatusCode::CONFLICT,
        "safe secret rotation requires re-enrollment; generate a new enrollment token instead",
    ))
}

pub async fn regenerate_enrollment(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = nodes::regenerate_enrollment(&state.db, id)?;
    nodes::record_event(
        &state.db,
        id,
        "warn",
        "reenroll",
        "new enrollment token generated",
        &serde_json::json!({}),
    )?;
    Ok(Json(serde_json::json!({ "enrollment_token": token })))
}

#[derive(Debug, Deserialize)]
pub struct EnrollmentRequest {
    pub token: String,
    pub heartbeat: NodeHeartbeat,
}

pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollmentRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = nodes::enroll(&state.db, &req.token, &req.heartbeat)
        .map_err(|e| ApiError::unauthorized(e.to_string()))?;
    nodes::record_event(
        &state.db,
        n.id,
        "info",
        "enrolled",
        "node daemon enrolled",
        &serde_json::json!({
            "hostname": req.heartbeat.hostname,
            "version": req.heartbeat.daemon_version,
        }),
    )?;
    Ok(Json(serde_json::json!({
        "node_id": n.uuid,
        "secret": n.secret,
        "heartbeat_interval_secs": 15,
    })))
}

pub async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let node_id = header(&headers, node_protocol::NODE_ID_HEADER)?;
    let timestamp = header(&headers, node_protocol::TIMESTAMP_HEADER)?
        .parse::<i64>()
        .map_err(|_| ApiError::unauthorized("invalid timestamp"))?;
    let signed = SignedHeaders {
        node_id: node_id.clone(),
        timestamp,
        nonce: header(&headers, node_protocol::NONCE_HEADER)?,
        signature: header(&headers, node_protocol::SIGNATURE_HEADER)?,
    };
    let node = nodes::get_by_uuid(&state.db, &node_id)
        .map_err(|_| ApiError::unauthorized("unknown node"))?;
    node_protocol::verify(
        &node.secret,
        "POST",
        "/api/node/heartbeat",
        &body,
        &signed,
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| ApiError::unauthorized(e.to_string()))?;
    if !state
        .node_nonces
        .use_once(&node_id, &signed.nonce, signed.timestamp)
    {
        return Err(ApiError::unauthorized("replayed node request"));
    }
    let heartbeat: NodeHeartbeat = serde_json::from_slice(&body)?;
    nodes::heartbeat(&state.db, &node_id, &heartbeat)?;
    Ok(ok(
        serde_json::json!({ "accepted": true, "panel_time": chrono::Utc::now().timestamp() }),
    ))
}

pub async fn test_connection(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = nodes::get(&state.db, id)?;
    let started = std::time::Instant::now();
    match state.node_client.health(&n).await {
        Ok(health) => {
            let latency = started.elapsed().as_millis();
            nodes::record_event(
                &state.db,
                id,
                "info",
                "connection_test",
                "node connection successful",
                &serde_json::json!({ "latency_ms": latency }),
            )?;
            Ok(Json(
                serde_json::json!({ "ok": true, "latency_ms": latency, "health": health }),
            ))
        }
        Err(e) => {
            nodes::set_error(&state.db, &n.uuid, &e.to_string())?;
            nodes::record_event(
                &state.db,
                id,
                "error",
                "connection_test",
                &e.to_string(),
                &serde_json::json!({}),
            )?;
            Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                e.to_string(),
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PlacementRequest {
    pub memory_mb: i64,
    pub disk_mb: i64,
    #[serde(default)]
    pub tags: Vec<String>,
    pub location: Option<String>,
}

pub async fn placement(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<PlacementRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = nodes::select_for_server(
        &state.db,
        req.memory_mb,
        req.disk_mb,
        &req.tags,
        req.location.as_deref(),
    )?;
    Ok(Json(
        serde_json::json!({ "node": n, "reason": "lowest weighted cpu/memory/server load" }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct TransferRequest {
    pub server_id: i64,
    pub target_node: String,
}

pub async fn transfer_server(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<TransferRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let server = crate::models::get_server(&state.db, req.server_id)?;
    if server.node == "local" {
        return Err(ApiError::bad_request(
            "local-to-node transfer is not supported by this endpoint",
        ));
    }
    if server.node == req.target_node {
        return Err(ApiError::bad_request(
            "source and target node are identical",
        ));
    }
    let source = crate::nodes::get_by_name(&state.db, &server.node)?;
    let target = crate::nodes::get_by_name(&state.db, &req.target_node)?;
    if !target.online() || !target.enabled || !target.schedulable || target.maintenance {
        return Err(ApiError::bad_request(
            "target node is not online and schedulable",
        ));
    }
    if target.available_memory_mb() < server.memory_mb
        || target.available_disk_mb() < server.disk_mb
    {
        return Err(ApiError::bad_request("target node lacks capacity"));
    }
    let ports = crate::models::ports_for_server(&state.db, server.id)?;
    for port in &ports {
        if !crate::nodes::port_available_on_node(&state.db, &target.name, *port)? {
            return Err(ApiError::bad_request(format!(
                "port {port} conflicts on target node"
            )));
        }
    }
    let was_running = server.status == "running";
    if was_running {
        state
            .node_client
            .power(
                &source,
                &server.uuid,
                crate::node_protocol::PowerAction::Stop,
            )
            .await?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let snapshot = match state.node_client.snapshot(&source, &server.uuid).await {
        Ok(v) => v,
        Err(e) => {
            if was_running {
                let _ = state
                    .node_client
                    .power(
                        &source,
                        &server.uuid,
                        crate::node_protocol::PowerAction::Start,
                    )
                    .await;
            }
            return Err(e.into());
        }
    };
    let egg = crate::models::get_egg(&state.db, server.egg_id)?;
    let spec = crate::node_protocol::ServerSpec {
        uuid: server.uuid.clone(),
        name: server.name.clone(),
        startup: crate::services::egg::resolve_startup(&state.db, &server)?,
        stop_command: egg.stop_command,
        memory_mb: server.memory_mb as u64,
        disk_mb: server.disk_mb as u64,
        cpu_percent: server.cpu_percent as u64,
        port: server.port.and_then(|p| u16::try_from(p).ok()),
        ports: ports
            .into_iter()
            .filter_map(|p| u16::try_from(p).ok())
            .collect(),
        env: crate::services::egg::env_for_server(&state.db, &server),
        auto_restart: server.auto_restart,
    };
    if let Err(e) = state
        .node_client
        .provision(
            &target,
            &crate::node_protocol::ProvisionRequest {
                spec,
                files: vec![],
            },
        )
        .await
    {
        if was_running {
            let _ = state
                .node_client
                .power(
                    &source,
                    &server.uuid,
                    crate::node_protocol::PowerAction::Start,
                )
                .await;
        }
        return Err(e.into());
    }
    if let Err(e) = state
        .node_client
        .restore_snapshot(
            &target,
            &server.uuid,
            &crate::node_protocol::RestoreSnapshotRequest {
                archive_b64: snapshot.archive_b64,
                checksum: snapshot.checksum,
            },
        )
        .await
    {
        let _ = state.node_client.delete_server(&target, &server.uuid).await;
        if was_running {
            let _ = state
                .node_client
                .power(
                    &source,
                    &server.uuid,
                    crate::node_protocol::PowerAction::Start,
                )
                .await;
        }
        return Err(e.into());
    }
    if let Err(e) = crate::models::set_server_node(&state.db, server.id, &target.name) {
        let _ = state.node_client.delete_server(&target, &server.uuid).await;
        if was_running {
            let _ = state
                .node_client
                .power(
                    &source,
                    &server.uuid,
                    crate::node_protocol::PowerAction::Start,
                )
                .await;
        }
        return Err(e.into());
    }
    let _ = state.node_client.delete_server(&source, &server.uuid).await;
    if was_running {
        let _ = state
            .node_client
            .power(
                &target,
                &server.uuid,
                crate::node_protocol::PowerAction::Start,
            )
            .await;
    }
    crate::nodes::record_event(
        &state.db,
        target.id,
        "info",
        "server_transfer",
        "server transferred in",
        &serde_json::json!({"server_id":server.id,"source":source.name}),
    )?;
    Ok(Json(
        serde_json::json!({"ok":true,"server_id":server.id,"source_node":source.name,"target_node":target.name,"bytes":snapshot.size_bytes}),
    ))
}

fn header(headers: &HeaderMap, name: &str) -> ApiResult<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::unauthorized(format!("missing {name}")))
}
