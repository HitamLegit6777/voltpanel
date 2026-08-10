//! Execution fabric administration, enrollment, heartbeat, placement, and health APIs.
use super::{client_ip, data, ok, AdminUser, ApiError, ApiResult, AppState};
use crate::db::blocking;
use crate::models;
use crate::node_protocol::{self, NodeHeartbeat, SignedHeaders};
use crate::nodes;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
pub struct CreateNodeRequest {
    pub name: String,
    pub public_url: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Operator-seeded certificate fingerprint the enrollment must present
    /// (v16): strict 64-hex SHA-256 when present, validated in the handler.
    #[serde(default)]
    pub expected_fingerprint: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    _a: AdminUser,
) -> ApiResult<Json<serde_json::Value>> {
    let values: Vec<_> = blocking(state.db.clone(), move |db| nodes::list(&db))
        .await?
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
    let n = blocking(state.db.clone(), move |db| nodes::get(&db, id)).await?;
    let events = blocking(state.db.clone(), move |db| nodes::events(&db, id, 100)).await?;
    Ok(Json(serde_json::json!({
        "node": n,
        "online": n.online(),
        "available_memory_mb": n.available_memory_mb(),
        "available_disk_mb": n.available_disk_mb(),
        "events": events,
    })))
}

/// Normalize and strictly validate the operator-seeded expected fingerprint
/// from create/update: absent or empty clears (or leaves unset); anything
/// else must be exactly 64 hex characters (a SHA-256 digest).
fn validate_expected_fingerprint(raw: Option<&str>) -> ApiResult<Option<String>> {
    match raw {
        None | Some("") => Ok(None),
        Some(value) => {
            let fp = crate::tls::normalize_fingerprint(value);
            if !is_fingerprint(&fp) {
                return Err(ApiError::bad_request(
                    "invalid expected_fingerprint: expected 64 hex characters (a SHA-256 certificate fingerprint)",
                ));
            }
            Ok(Some(fp))
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    _a: AdminUser,
    Json(req): Json<CreateNodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.name.trim().len() < 2 {
        return Err(ApiError::bad_request("node name too short"));
    }
    let public_url = crate::nodes::validate_public_url(&req.public_url)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let expected_fingerprint =
        validate_expected_fingerprint(req.expected_fingerprint.as_deref())?;
    let name = req.name.trim().to_string();
    let location = req.location.clone();
    let tags = req.tags.clone();
    let n = blocking(state.db.clone(), move |db| {
        nodes::create(&db, &name, &public_url, &location, &tags)
    })
    .await?;
    let node_id = n.id;
    if let Some(fp) = &expected_fingerprint {
        let fp = fp.clone();
        blocking(state.db.clone(), move |db| {
            nodes::set_expected_fingerprint(&db, node_id, Some(&fp))
        })
        .await?;
    }
    blocking(state.db.clone(), move |db| {
        nodes::record_event(
            &db,
            node_id,
            "info",
            "created",
            "node record created",
            &serde_json::json!({}),
        )
    })
    .await?;
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
    /// Operator-seeded certificate fingerprint the enrollment must present
    /// (v16): strict 64-hex SHA-256 when present, validated in the handler.
    /// `null` clears a previously seeded value.
    #[serde(default)]
    pub expected_fingerprint: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
    Json(req): Json<UpdateNodeRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let public_url = crate::nodes::validate_public_url(&req.public_url)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if req.memory_limit_mb < 0
        || req.disk_limit_mb < 0
        || req.memory_overallocate < 0
        || req.disk_overallocate < 0
    {
        return Err(ApiError::bad_request(
            "resource limits must not be negative",
        ));
    }
    // Full-replace update: absent/empty expected_fingerprint clears the
    // seed (same semantics as the defaulted fields above), a valid 64-hex
    // value re-seeds it. The setter runs before the re-read so the response
    // reflects the stored column.
    let expected_fingerprint =
        validate_expected_fingerprint(req.expected_fingerprint.as_deref())?;
    let name = req.name.clone();
    let location = req.location.clone();
    let tags = req.tags.clone();
    blocking(state.db.clone(), move |db| {
        nodes::update(
            &db,
            id,
            &name,
            &public_url,
            req.enabled,
            req.maintenance,
            req.schedulable,
            &location,
            &tags,
            req.memory_limit_mb,
            req.disk_limit_mb,
            req.memory_overallocate,
            req.disk_overallocate,
        )
    })
    .await?;
    let fp = expected_fingerprint.clone();
    blocking(state.db.clone(), move |db| {
        nodes::set_expected_fingerprint(&db, id, fp.as_deref())
    })
    .await?;
    let n = blocking(state.db.clone(), move |db| nodes::get(&db, id)).await?;
    blocking(state.db.clone(), move |db| {
        nodes::record_event(
            &db,
            id,
            "info",
            "updated",
            "execution agent configuration updated",
            &serde_json::json!({}),
        )
    })
    .await?;
    Ok(Json(serde_json::to_value(n)?))
}

pub async fn delete(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    blocking(state.db.clone(), move |db| nodes::delete(&db, id)).await?;
    Ok(ok(serde_json::json!({ "ok": true })))
}


pub async fn rotate_secret(
    State(state): State<AppState>,
    a: AdminUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let (secret, token) =
        blocking(state.db.clone(), move |db| nodes::rotate_secret(&db, id)).await?;
    // Secret rotation kills the node's current credentials: attribute it to
    // the admin who requested it, not just a bare node event.
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let admin_id = a.0.id;
    let ip2 = ip.clone();
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(admin_id),
            "node_secret_rotate",
            &format!("node #{id}"),
            &ip2,
            "shared secret rotated; node must re-enroll with the new token",
        )
    })
    .await?;
    blocking(state.db.clone(), move |db| {
        nodes::record_event(
            &db,
            id,
            "warn",
            "rotate",
            "shared secret rotated; node must re-enroll with the new token",
            &serde_json::json!({}),
        )
    })
    .await?;
    Ok(Json(serde_json::json!({
        "secret": secret,
        "enrollment_token": token,
        "enrolled": false,
        "message": "node must re-enroll with the new token; the old secret is revoked",
    })))
}

pub async fn regenerate_enrollment(
    State(state): State<AppState>,
    a: AdminUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let token =
        blocking(state.db.clone(), move |db| nodes::regenerate_enrollment(&db, id)).await?;
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let admin_id = a.0.id;
    let ip2 = ip.clone();
    blocking(state.db.clone(), move |db| {
        models::audit(
            &db,
            Some(admin_id),
            "node_enrollment_regenerate",
            &format!("node #{id}"),
            &ip2,
            "new enrollment token generated",
        )
    })
    .await?;
    blocking(state.db.clone(), move |db| {
        nodes::record_event(
            &db,
            id,
            "warn",
            "reenroll",
            "new enrollment token generated",
            &serde_json::json!({}),
        )
    })
    .await?;
    Ok(Json(serde_json::json!({ "enrollment_token": token })))
}
#[derive(Debug, Deserialize)]
pub struct EnrollmentRequest {
    pub token: String,
    pub heartbeat: NodeHeartbeat,
}

pub async fn enroll(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<EnrollmentRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    // Enrollment is unauthenticated (the bearer token arrives in the body):
    // throttle per source IP so a leaked token cannot be brute-forced at line
    // speed, using the same primitive as the login route.
    let ip = client_ip(peer, &headers, &state.cfg.web.trusted_proxies);
    let cfg = state.cfg.clone();
    let ip2 = ip.clone();
    if !blocking(state.db.clone(), move |db| {
        crate::auth::rate_limit(&db, &cfg, &format!("enroll-ip:{ip2}"))
    })
    .await?
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many enrollment attempts; try again later",
        ));
    }
    // Enrollment is one-shot TOFU: the token and the self-claimed fingerprint
    // travel together, so an unauthenticated plaintext transport would let a
    // MITM steal the token and pin its own fingerprint in the same round
    // trip. The enrollment is therefore REFUSED unless the transport is
    // positively TLS: `request_is_tls` covers native panel TLS
    // (`web.tls_self_signed`) and TLS terminated by a trusted proxy, which
    // reports `X-Forwarded-Proto: https` (the shipped Caddy setup). Any
    // transport we cannot positively identify as TLS fails closed — the
    // operator fixes the proxy/`trusted_proxies` configuration rather than
    // silently enrolling over plaintext. Iteration 3 (v16) adds the
    // operator-seeded expected_fingerprint path for plaintext deployments.
    if !super::request_is_tls(&state, peer, &headers) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "enrollment refused: transport is not positively TLS (configure web.tls_self_signed for native TLS, or web.trusted_proxies so a TLS-terminating proxy's X-Forwarded-Proto is honored)",
        ));
    }
    // The fingerprint travels inside the heartbeat body (the one shape `voltd`
    // sends for both enrollment and heartbeats). It must be present AND
    // well-formed: an enrollment with no pin is TOFU with nothing to pin, so
    // it is rejected outright rather than storing an empty column. A
    // plaintext agent (no certificate material) therefore still cannot
    // enroll even with a seeded `expected_fingerprint` — the seed only
    // gates WHICH fingerprint may be presented, it never substitutes for
    // one; proxy-fronted agents present the endpoint fingerprint once the
    // agent learns to report it (see scripts/install-node.sh --help).
    let fp = crate::tls::normalize_fingerprint(&req.heartbeat.tls_fingerprint);
    if fp.is_empty() {
        return Err(ApiError::bad_request(
            "enrollment refused: tls_fingerprint is required; run the agent with TLS material (a plaintext agent cannot enroll)",
        ));
    }
    if !is_fingerprint(&fp) {
        return Err(ApiError::bad_request(
            "invalid tls_fingerprint in enrollment",
        ));
    }
    // A node whose fingerprint is already pinned must not be re-enrolled
    // with a DIFFERENT self-claimed fingerprint over a one-shot token: over
    // plaintext the stolen token would re-pin the attacker's identity. This
    // pre-check reads the row for a precise error; it is NOT the gate. The
    // authoritative check is the fingerprint predicate inside
    // `nodes::enroll`'s atomic UPDATE ... RETURNING, re-evaluated with the
    // enrolled flip — so a seed committed between this read and the UPDATE
    // (or a pin changing under the read) still refuses an unseeded pin
    // instead of TOFU-pinning it (check-then-act closed, FASE-4). An
    // `enrolled=1` row is refused outright. Otherwise the presented
    // fingerprint is accepted when it matches what is pinned (the pin
    // survives `rotate_secret`, so re-enrollment with the same fingerprint,
    // identity unchanged, succeeds) OR — the v16 operator-seeded path — when
    // it matches `expected_fingerprint`: an operator declares the identity in
    // advance (plaintext / proxy-fronted agents whose endpoint certificate
    let token = req.token.clone();
    let lookup_token = token.clone();
    let existing = state
        .db
        .call(move |conn| {
            conn.query_row(
                "SELECT enrolled, tls_fingerprint, expected_fingerprint FROM nodes WHERE enrollment_token=?1",
                [&lookup_token],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? != 0,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .map_err(anyhow::Error::from)
        })
        .await;
    match existing {
        Ok((true, _, _)) => {
            return Err(ApiError::unauthorized(
                "enrollment refused: node is already enrolled",
            ));
        }
        Ok((false, pinned, expected)) => {
            let expected = expected.unwrap_or_default();
            let pin_matches = !pinned.is_empty() && pinned == fp;
            let expected_matches = !expected.is_empty() && expected == fp;
            // Plain TOFU (nothing pinned, nothing seeded) stays open; a
            // non-empty pin or a seeded expectation gates enrollment on a
            // matching presented fingerprint.
            let accepted =
                (pinned.is_empty() && expected.is_empty()) || pin_matches || expected_matches;
            if !accepted {
                return Err(ApiError::unauthorized(
                    "enrollment refused: presented certificate fingerprint matches neither this node's pinned nor its operator-seeded expected fingerprint; re-enroll with the matching certificate, update expected_fingerprint, or delete and recreate the node",
                ));
            }
        }
        Err(e)
            if e.downcast_ref::<rusqlite::Error>()
                == Some(&rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e.into()),
    }
    let heartbeat = req.heartbeat.clone();
    let hostname = heartbeat.hostname.clone();
    let daemon_version = heartbeat.daemon_version.clone();
    let token2 = token.clone();
    let n = blocking(state.db.clone(), move |db| {
        nodes::enroll(&db, &token2, &heartbeat)
    })
    .await
    .map_err(|e| ApiError::unauthorized(e.to_string()))?;
    let node_id = n.id;
    blocking(state.db.clone(), move |db| {
        nodes::record_event(
            &db,
            node_id,
            "info",
            "enrolled",
            "execution agent enrolled",
            &serde_json::json!({
                "hostname": hostname,
                "version": daemon_version,
            }),
        )
    })
    .await?;
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
    let node_id2 = node_id.clone();
    let node = blocking(state.db.clone(), move |db| nodes::get_by_uuid(&db, &node_id2))
        .await
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
    // The agent explicitly reports its current certificate fingerprint on the
    // authenticated heartbeat channel; accept it only when it is well-formed.
    // This is the sole pin-refresh path — a mismatched pin keeps failing
    // handshakes until an enrolled agent proves its identity here.
    let reported = crate::tls::normalize_fingerprint(&heartbeat.tls_fingerprint);
    if !reported.is_empty() {
        if !is_fingerprint(&reported) {
            return Err(ApiError::bad_request(
                "invalid tls_fingerprint in heartbeat",
            ));
        }
        if reported != node.tls_fingerprint {
            let reported2 = reported.clone();
            let nid = node_id.clone();
            blocking(state.db.clone(), move |db| {
                nodes::set_tls_fingerprint(&db, &nid, &reported2)
            })
            .await?;
        }
    }
    let node_id3 = node_id.clone();
    blocking(state.db.clone(), move |db| {
        nodes::heartbeat(&db, &node_id3, &heartbeat)
    })
    .await?;
    Ok(ok(
        serde_json::json!({ "accepted": true, "panel_time": chrono::Utc::now().timestamp() }),
    ))
}

pub async fn test_connection(
    State(state): State<AppState>,
    _a: AdminUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = blocking(state.db.clone(), move |db| nodes::get(&db, id)).await?;
    let started = std::time::Instant::now();
    match state.node_client.health(&n).await {
        Ok(health) => {
            let latency = started.elapsed().as_millis();
            blocking(state.db.clone(), move |db| {
                nodes::record_event(
                    &db,
                    id,
                    "info",
                    "connection_test",
                    "node connection successful",
                    &serde_json::json!({ "latency_ms": latency }),
                )
            })
            .await?;
            Ok(Json(
                serde_json::json!({ "ok": true, "latency_ms": latency, "health": health }),
            ))
        }
        Err(e) => {
            let uuid = n.uuid.clone();
            let err = e.to_string();
            let err2 = err.clone();
            let err3 = err2.clone();
            blocking(state.db.clone(), move |db| {
                nodes::set_error(&db, &uuid, &err)
            })
            .await?;
            blocking(state.db.clone(), move |db| {
                nodes::record_event(
                    &db,
                    id,
                    "error",
                    "connection_test",
                    &err2,
                    &serde_json::json!({}),
                )
            })
            .await?;
            Err(ApiError::new(
                axum::http::StatusCode::BAD_GATEWAY,
                err3,
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
    let n = blocking(state.db.clone(), move |db| {
        nodes::select_for_server(
            &db,
            req.memory_mb,
            req.disk_mb,
            &req.tags,
            req.location.as_deref(),
        )
    })
    .await?;
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
    let server_node = req.target_node.clone();
    let server = blocking(state.db.clone(), move |db| {
        crate::models::get_server(&db, req.server_id)
    })
    .await?;
    if server.node == "local" {
        return Err(ApiError::bad_request(
            "local-to-node transfer is not supported by this endpoint",
        ));
    }
    if server.node == server_node {
        return Err(ApiError::bad_request(
            "source and target node are identical",
        ));
    }
    let source_name = server.node.clone();
    let source = blocking(state.db.clone(), move |db| {
        crate::nodes::get_by_name(&db, &source_name)
    })
    .await?;
    let target_name = server_node.clone();
    let target = blocking(state.db.clone(), move |db| {
        crate::nodes::get_by_name(&db, &target_name)
    })
    .await?;
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
    let server_id = server.id;
    let ports = blocking(state.db.clone(), move |db| {
        crate::models::ports_for_server(&db, server_id)
    })
    .await?;
    let target_name = target.name.clone();
    for port in &ports {
        let port = *port;
        let tn = target_name.clone();
        if !blocking(state.db.clone(), move |db| {
            crate::nodes::port_available_on_node(&db, &tn, port)
        })
        .await?
        {
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
    // Checked wire conversions: negative or oversized resource values abort the
    // transfer instead of wrapping to huge unsigned numbers or being dropped.
    let (memory_mb, disk_mb, cpu_percent, port, ports) = match checked_spec_numerics(
        server.memory_mb,
        server.disk_mb,
        server.cpu_percent,
        server.port,
        &ports,
    ) {
        Ok(v) => v,
        Err(e) => {
            rollback_transfer(&state, &source, &target, &server.uuid, was_running, false).await;
            return Err(ApiError::bad_request(e.to_string()));
        }
    };
    let blueprint_id = server.blueprint_id;
    let blueprint = match blocking(state.db.clone(), move |db| {
        crate::models::get_blueprint(&db, blueprint_id)
    })
    .await
    {
        Ok(b) => b,
        Err(e) => {
            rollback_transfer(&state, &source, &target, &server.uuid, was_running, false).await;
            return Err(e.into());
        }
    };
    let server_clone = server.clone();
    let startup = match blocking(state.db.clone(), move |db| {
        crate::services::blueprint::resolve_startup(&db, &server_clone)
    })
    .await
    {
        Ok(s) => s,
        Err(e) => {
            rollback_transfer(&state, &source, &target, &server.uuid, was_running, false).await;
            return Err(e.into());
        }
    };
    let snapshot = match state.node_client.snapshot(&source, &server.uuid).await {
        Ok(v) => v,
        Err(e) => {
            rollback_transfer(&state, &source, &target, &server.uuid, was_running, false).await;
            return Err(e.into());
        }
    };
    let server_env = server.clone();
    let env = blocking(state.db.clone(), move |db| {
        Ok::<_, anyhow::Error>(crate::services::blueprint::env_for_server(&db, &server_env))
    })
    .await?;
    let spec = crate::node_protocol::ServerSpec {
        uuid: server.uuid.clone(),
        name: server.name.clone(),
        startup,
        stop_command: blueprint.stop_command,
        memory_mb,
        disk_mb,
        cpu_percent,
        port,
        ports,
        env,
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
        rollback_transfer(&state, &source, &target, &server.uuid, was_running, true).await;
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
        rollback_transfer(&state, &source, &target, &server.uuid, was_running, true).await;
        return Err(e.into());
    }
    let server_id = server.id;
    let target_name = target.name.clone();
    if let Err(e) = blocking(state.db.clone(), move |db| {
        crate::models::set_server_node(&db, server_id, &target_name)
    })
    .await
    {
        rollback_transfer(&state, &source, &target, &server.uuid, was_running, true).await;
        return Err(e.into());
    }
    // Commit point: the DB now points at the target and the target owns the
    // server row. From here nothing is rolled back — the target's completed
    // restore is the source of truth. Source cleanup and target startup are
    // best-effort, but every failure is recorded as a node event AND surfaced
    // truthfully in the response, so an operator never mistakes a cleanup
    // hiccup for a clean transfer.
    let mut issues: Vec<String> = Vec::new();
    if let Err(e) = state.node_client.delete_server(&source, &server.uuid).await {
        let source_id = source.id;
        let server_id = server.id;
        let target_node = target.name.clone();
        let e2 = e.to_string();
        let _ = blocking(state.db.clone(), move |db| {
            crate::nodes::record_event(
                &db,
                source_id,
                "error",
                "server_transfer_cleanup",
                &format!("source cleanup after transfer failed: {e2}"),
                &serde_json::json!({"server_id": server_id, "target_node": target_node}),
            )
        })
        .await;
        issues.push(format!(
            "source node {} still holds a stale copy: {e}",
            source.name
        ));
    }
    if was_running {
        if let Err(e) = state
            .node_client
            .power(
                &target,
                &server.uuid,
                crate::node_protocol::PowerAction::Start,
            )
            .await
        {
            let target_id = target.id;
            let server_id = server.id;
            let e2 = e.to_string();
            let _ = blocking(state.db.clone(), move |db| {
                crate::nodes::record_event(
                    &db,
                    target_id,
                    "error",
                    "server_transfer_cleanup",
                    &format!("target start after transfer failed: {e2}"),
                    &serde_json::json!({"server_id": server_id}),
                )
            })
            .await;
            issues.push(format!(
                "target node {} failed to start the server: {e}",
                target.name
            ));
        }
    }
    let target_id = target.id;
    let server_id = server.id;
    let source_name = source.name.clone();
    blocking(state.db.clone(), move |db| {
        crate::nodes::record_event(
            &db,
            target_id,
            "info",
            "server_transfer",
            "server transferred in",
            &serde_json::json!({"server_id":server_id,"source":source_name}),
        )
    })
    .await?;
    let mut body = serde_json::json!({
        "ok": true,
        "server_id": server.id,
        "source_node": source.name,
        "target_node": target.name,
        "bytes": snapshot.size_bytes,
    });
    if !issues.is_empty() {
        body["warning"] = serde_json::json!(format!(
            "server {} transferred to {}, but: {}",
            server.uuid,
            target.name,
            issues.join("; ")
        ));
    }
    Ok(Json(body))
}

/// Checked conversion of panel-side `i64` resource values to the wire's
/// unsigned types. Negative or oversized values fail loudly instead of
/// wrapping to huge unsigned numbers or being silently dropped.
type CheckedSpecNumerics = (u64, u64, u64, Option<u16>, Vec<u16>);

fn checked_spec_numerics(
    memory_mb: i64,
    disk_mb: i64,
    cpu_percent: i64,
    port: Option<i64>,
    ports: &[i64],
) -> anyhow::Result<CheckedSpecNumerics> {
    let memory_mb = u64::try_from(memory_mb)
        .map_err(|_| anyhow::anyhow!("memory_mb {memory_mb} is negative or out of range"))?;
    let disk_mb = u64::try_from(disk_mb)
        .map_err(|_| anyhow::anyhow!("disk_mb {disk_mb} is negative or out of range"))?;
    let cpu_percent = u64::try_from(cpu_percent)
        .map_err(|_| anyhow::anyhow!("cpu_percent {cpu_percent} is negative or out of range"))?;
    let port = port
        .map(|p| {
            u16::try_from(p).map_err(|_| anyhow::anyhow!("port {p} is negative or out of range"))
        })
        .transpose()?;
    let ports = ports
        .iter()
        .map(|p| {
            u16::try_from(*p).map_err(|_| anyhow::anyhow!("port {p} is negative or out of range"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((memory_mb, disk_mb, cpu_percent, port, ports))
}

/// Best-effort rollback after a failed transfer: clean the target when it may
/// have received the server and restart the source when it was stopped. The DB
/// still names the source as authority on every path that calls this. Cleanup
/// failures are recorded as node events rather than masking the original error.
async fn rollback_transfer(
    state: &AppState,
    source: &crate::nodes::Node,
    target: &crate::nodes::Node,
    uuid: &str,
    was_running: bool,
    target_has_server: bool,
) {
    if target_has_server {
        if let Err(e) = state.node_client.delete_server(target, uuid).await {
            let target_id = target.id;
            let uuid2 = uuid.to_string();
            let e2 = e.to_string();
            let _ = blocking(state.db.clone(), move |db| {
                crate::nodes::record_event(
                    &db,
                    target_id,
                    "error",
                    "server_transfer_rollback",
                    &format!("target cleanup after failed transfer: {e2}"),
                    &serde_json::json!({"server_id": uuid2}),
                )
            })
            .await;
        }
    }
    if was_running {
        if let Err(e) = state
            .node_client
            .power(source, uuid, crate::node_protocol::PowerAction::Start)
            .await
        {
            let source_id = source.id;
            let uuid2 = uuid.to_string();
            let e2 = e.to_string();
            let _ = blocking(state.db.clone(), move |db| {
                crate::nodes::record_event(
                    &db,
                    source_id,
                    "error",
                    "server_transfer_rollback",
                    &format!("source restart after failed transfer: {e2}"),
                    &serde_json::json!({"server_id": uuid2}),
                )
            })
            .await;
        }
    }
}

/// True when `value` is a normalized SHA-256 fingerprint: 64 lowercase hex.
fn is_fingerprint(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn header(headers: &HeaderMap, name: &str) -> ApiResult<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::unauthorized(format!("missing {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn fingerprint_validation_is_strict_64_hex() {
        assert!(is_fingerprint(&"ab".repeat(32)));
        assert!(is_fingerprint(&"0123456789abcdef".repeat(4)));
        // Wrong length: too short, too long.
        assert!(!is_fingerprint(&"ab".repeat(31)));
        assert!(!is_fingerprint(&"ab".repeat(33)));
        // Non-hex characters. Uppercase A-F are valid hex digits; the check
        // only sees lowercase because callers normalize first.
        assert!(!is_fingerprint(&"zz".repeat(32)));
        assert!(!is_fingerprint(&"gh".repeat(32)));
        assert!(!is_fingerprint(""));
    }

    fn test_state() -> (tempfile::TempDir, AppState) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::open(tmp.path().join("t.db").to_str().unwrap()).unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.paths.logs_dir = tmp.path().join("logs");
        cfg.paths.servers_dir = tmp.path().join("servers");
        // Enrollment demands a positively-TLS transport; the test drives the
        // handler directly, so native panel TLS is declared.
        cfg.web.tls_self_signed = true;
        // Enrollment is IP-rate-limited; keep the shared in-process bucket
        // from tripping under parallel test load.
        cfg.security.rate_limit_per_min = 10_000;
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

    fn router(state: AppState) -> axum::Router {
        axum::Router::new()
            .route("/api/node/enroll", axum::routing::post(enroll))
            .with_state(state)
    }

    /// The one heartbeat shape `voltd` sends for enrollment, pinned to `fp`.
    fn enroll_body(token: &str, fp: &str) -> serde_json::Value {
        let hb = crate::node_protocol::NodeHeartbeat {
            daemon_version: "0.1.1-test".into(),
            hostname: "host".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            capacity: Default::default(),
            tls_fingerprint: fp.into(),
        };
        serde_json::json!({ "token": token, "heartbeat": hb })
    }

    async fn enroll_request(
        state: AppState,
        token: &str,
        fp: &str,
    ) -> (StatusCode, serde_json::Value) {
        // `ConnectInfo` is injected by the serving layer in production; in the
        // test it is stuffed into the request extensions the same way
        // `into_make_service_with_connect_info` does.
        let mut req = Request::builder()
            .method("POST")
            .uri("/api/node/enroll")
            .header("content-type", "application/json")
            .body(Body::from(enroll_body(token, fp).to_string()))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:55000".parse::<std::net::SocketAddr>().unwrap()));
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
    async fn enroll_guard_allows_matching_fingerprint_reenroll_after_rotation() {
        let (_tmp, state) = test_state();
        let n = crate::nodes::create(
            &state.db,
            "node-reenroll",
            "https://node-reenroll.example.com",
            "",
            &[],
        )
        .unwrap();
        let fp = "ab12".repeat(16);

        // Initial TOFU enrollment pins the fingerprint.
        let (status, body) =
            enroll_request(state.clone(), n.enrollment_token.as_deref().unwrap(), &fp).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["secret"].is_string());

        // rotate_secret keeps the pin while detaching (enrolled=0 + fresh
        // token): re-enrolling with the SAME fingerprint must succeed — this
        // is the documented rotation flow, and the old guard dead-ended it.
        let (_, new_token) = crate::nodes::rotate_secret(&state.db, n.id).unwrap();
        let (status, body) = enroll_request(state.clone(), &new_token, &fp).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "same-fingerprint re-enroll after rotation must succeed: {body}"
        );
        let after = crate::nodes::get(&state.db, n.id).unwrap();
        assert!(after.enrolled);
        assert_eq!(after.tls_fingerprint, fp);

        // A DIFFERENT fingerprint is a re-pin attempt: refused without
        // consuming the fresh token, so the legit agent can retry.
        let (_, new_token) = crate::nodes::rotate_secret(&state.db, n.id).unwrap();
        let (status, body) = enroll_request(state.clone(), &new_token, &"cd34".repeat(16)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("fingerprint"));
        let after = crate::nodes::get(&state.db, n.id).unwrap();
        assert!(!after.enrolled, "refused re-enroll must not flip the row");
        assert_eq!(
            after.enrollment_token.as_deref(),
            Some(new_token.as_str()),
            "refused re-enroll must not consume the token"
        );
        // The refused token still works for the pinned identity.
        let (status, _) = enroll_request(state.clone(), &new_token, &fp).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The v16 operator-seeded path: when `expected_fingerprint` is set, the
    /// first enrollment must present exactly it (TOFU is disabled), the
    /// presented value becomes the pin, the seed is kept, and a later
    /// operator re-seed lets a NEW fingerprint re-pin after rotation.
    #[tokio::test]
    async fn enroll_with_expected_fingerprint_pins_and_reenrolls() {
        let (_tmp, state) = test_state();
        let n = crate::nodes::create(
            &state.db,
            "node-expected",
            "https://node-expected.example.com",
            "",
            &[],
        )
        .unwrap();
        let seeded = "ab12".repeat(16);
        crate::nodes::set_expected_fingerprint(&state.db, n.id, Some(&seeded)).unwrap();
        assert_eq!(
            crate::nodes::get(&state.db, n.id).unwrap().expected_fingerprint,
            seeded
        );

        // A first enrollment presenting anything but the seed is refused and
        // must not consume the token.
        let (status, body) =
            enroll_request(state.clone(), n.enrollment_token.as_deref().unwrap(), &"cd34".repeat(16)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("expected"));
        let after = crate::nodes::get(&state.db, n.id).unwrap();
        assert!(!after.enrolled);

        // The matching first enrollment succeeds: pins the presented value
        // into tls_fingerprint and KEEPS the seed for repeatability.
        let (status, body) =
            enroll_request(state.clone(), n.enrollment_token.as_deref().unwrap(), &seeded).await;
        assert_eq!(status, StatusCode::OK, "seeded first enroll must succeed: {body}");
        let after = crate::nodes::get(&state.db, n.id).unwrap();
        assert!(after.enrolled);
        assert_eq!(after.tls_fingerprint, seeded);
        assert_eq!(after.expected_fingerprint, seeded, "seed must be kept");

        // Rotation + same-fingerprint re-enroll: accepted via the pinned
        // clause (identity unchanged), seed intact.
        let (_, new_token) = crate::nodes::rotate_secret(&state.db, n.id).unwrap();
        let (status, _) = enroll_request(state.clone(), &new_token, &seeded).await;
        assert_eq!(status, StatusCode::OK, "same-fingerprint re-enroll must succeed");

        // Operator re-seeds a NEW fingerprint; after rotation the fresh
        // identity re-pins through the expected_fingerprint clause even
        // though the old pin differs.
        let reseeded = "ef34".repeat(16);
        crate::nodes::set_expected_fingerprint(&state.db, n.id, Some(&reseeded)).unwrap();
        let (_, new_token) = crate::nodes::rotate_secret(&state.db, n.id).unwrap();
        let (status, body) = enroll_request(state.clone(), &new_token, &reseeded).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "operator-reseeded fingerprint must re-pin after rotation: {body}"
        );
        let after = crate::nodes::get(&state.db, n.id).unwrap();
        assert_eq!(after.tls_fingerprint, reseeded);
        assert_eq!(after.expected_fingerprint, reseeded);

        // A fingerprint matching neither pin nor seed stays refused.
        let (_, new_token) = crate::nodes::rotate_secret(&state.db, n.id).unwrap();
        let (status, _) = enroll_request(state.clone(), &new_token, &"9911".repeat(16)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Transfer commit point migrates the server's allocations to the target
    /// node in the same transaction (`set_server_node`, the exact call
    /// `transfer_server` makes): without it the panel would keep treating the
    /// old node as the owner of every port.
    #[test]
    fn transfer_migrates_allocations_node() {
        let (_tmp, state) = test_state();
        crate::nodes::create(&state.db, "source", "https://source.example.com", "", &[]).unwrap();
        crate::nodes::create(&state.db, "target", "https://target.example.com", "", &[]).unwrap();
        let conn = state.db.get().unwrap();
        conn.execute(
            "INSERT INTO users (username,email,password_hash,created_at,updated_at) VALUES ('u','u@example.com','x','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blueprints (uuid,name,created_at,updated_at) VALUES ('bp-t','e','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO servers (uuid,name,user_id,blueprint_id,node,created_at,updated_at) VALUES ('srv-t','s1',1,1,'source','t','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO allocations (server_id,port,node,assigned_at) VALUES (1,8080,'source','t'),(1,8081,'source','t')",
            [],
        )
        .unwrap();
        drop(conn);

        crate::models::set_server_node(&state.db, 1, "target").unwrap();

        let conn = state.db.get().unwrap();
        let servers_node: String = conn
            .query_row("SELECT node FROM servers WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(servers_node, "target");
        let alloc_nodes: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT node FROM allocations WHERE server_id=1 ORDER BY port")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(alloc_nodes, vec!["target".to_string(), "target".to_string()]);
    }
}