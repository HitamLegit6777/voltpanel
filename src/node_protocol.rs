//! Shared wire protocol between VoltPanel and the `voltd` execution agent.
//!
//! Requests are authenticated with HMAC-SHA256 over:
//! `NODE_ID\nMETHOD\nPATH\nTIMESTAMP\nNONCE\nSHA256(BODY)`.
//! The agent rejects timestamps outside a 90-second window and re-used nonces.
//!
//! Responses are authenticated in the opposite direction with the same secret:
//! the agent signs each envelope over
//! `NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY)`, echoing the nonce the
//! panel sent with the request, and appends the signature to the envelope as
//! serde-defaulted `signature`/`nonce` fields. A response that carries no
//! signature came from an agent predating response signing; the panel accepts
//! it with a warning until the fleet upgrades (see `verify_response`).
//!
//! The node id is part of the signed payload so a signature can never be
//! replayed against a different node even if secrets were ever shared.
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SIGNATURE_HEADER: &str = "x-volt-signature";
pub const TIMESTAMP_HEADER: &str = "x-volt-timestamp";
pub const NONCE_HEADER: &str = "x-volt-nonce";
pub const NODE_ID_HEADER: &str = "x-volt-node";
pub const MAX_CLOCK_SKEW_SECS: i64 = 90;

/// Streaming transfer capability header. The panel sends it with snapshot GET
/// and restore POST requests to opt into the raw-body streaming protocol; the
/// agent streams those requests/responses iff the header is present, and the
/// `sign_responses` middleware echoes it on every response to a streaming
/// request so the panel can tell a streaming agent from a pre-streaming one.
/// Snapshot streaming responses also carry it alongside
/// `application/octet-stream` — the marker the middleware uses to recognize
/// an already-signed raw body and leave it untouched.
pub const STREAM_HEADER: &str = "x-volt-stream";

/// Magic opening the fixed-length footer appended to a streaming snapshot
/// response body: `\nVOLTSTREAM1\n<64-hex-hmac>\n`.
pub const STREAM_FOOTER_MAGIC: &str = "VOLTSTREAM1";

/// Exact byte length of the streaming footer (`\n` + magic + `\n` + 64 hex
/// signature digits + `\n`). Fixed length makes the archive/footer split
/// unambiguous: the reader knows the last `STREAM_FOOTER_LEN` bytes are the
/// signature, whatever the archive bytes look like.
pub const STREAM_FOOTER_LEN: usize = 1 + STREAM_FOOTER_MAGIC.len() + 1 + 64 + 1;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedHeaders {
    pub node_id: String,
    pub timestamp: i64,
    pub nonce: String,
    pub signature: String,
}

pub fn body_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}
pub fn canonical_hash(
    node_id: &str,
    method: &str,
    path: &str,
    timestamp: i64,
    nonce: &str,
    sha256_hex: &str,
) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        node_id,
        method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
        sha256_hex
    )
}

pub fn canonical(
    node_id: &str,
    method: &str,
    path: &str,
    timestamp: i64,
    nonce: &str,
    body: &[u8],
) -> String {
    canonical_hash(node_id, method, path, timestamp, nonce, &body_hash(body))
}

/// Mint a signed request whose body hash is supplied directly instead of
/// being computed over a buffered body: the streaming restore path streams
/// the archive from disk, so the panel computes SHA-256 of the file first and
/// signs against that hash. The canonical string is byte-identical to
/// [`sign`] for the same body.
pub fn sign_body_hash(
    secret: &str,
    method: &str,
    path: &str,
    body_sha256_hex: &str,
    node_id: &str,
) -> Result<SignedHeaders> {
    let timestamp = chrono::Utc::now().timestamp();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let payload = canonical_hash(node_id, method, path, timestamp, &nonce, body_sha256_hex);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    Ok(SignedHeaders {
        node_id: node_id.to_string(),
        timestamp,
        nonce,
        signature: hex::encode(mac.finalize().into_bytes()),
    })
}

pub fn sign(
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
    node_id: &str,
) -> Result<SignedHeaders> {
    sign_body_hash(secret, method, path, &body_hash(body), node_id)
}


/// A signed request whose body MAC is deferred: the header checks that need
/// no body (node id, timestamp window, signature shape) have passed, and the
/// body hash must be supplied once the body has actually been read, via
/// [`complete_verify`]. The streaming restore path uses this split: the
/// archive is consumed as it arrives (never buffered), and its SHA-256 is
/// only known at the end of the body.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub node_id: String,
    pub timestamp: i64,
    pub nonce: String,
    signature: Vec<u8>,
}

/// Run every signed-request check that does not need the body. Cheap
/// structural rejections run FIRST: an attacker can forge headers at line
/// speed, so a skewed timestamp, a non-UUID node id, or a non-hex signature
/// must never make the verifier pay for SHA-256 over the body (unauthenticated
/// DoS). Only a well-formed header set reaches [`complete_verify`], which is
/// where the body hash enters the MAC.
///
pub fn verify_pending(headers: &SignedHeaders, now: i64) -> Result<PendingRequest> {
    if now.saturating_sub(headers.timestamp).unsigned_abs() > MAX_CLOCK_SKEW_SECS as u64 {
        bail!("signed request expired");
    }
    if uuid::Uuid::parse_str(&headers.node_id).is_err() {
        bail!("invalid node id");
    }
    let signature =
        hex::decode(&headers.signature).map_err(|_| anyhow!("invalid signature encoding"))?;
    Ok(PendingRequest {
        node_id: headers.node_id.clone(),
        timestamp: headers.timestamp,
        nonce: headers.nonce.clone(),
        signature,
    })
}

/// Finish a deferred request verification with the SHA-256 of the body that
/// was actually received.
pub fn complete_verify(
    secret: &str,
    method: &str,
    path: &str,
    pending: &PendingRequest,
    body_sha256_hex: &str,
) -> Result<()> {
    let payload = canonical_hash(
        &pending.node_id,
        method,
        path,
        pending.timestamp,
        &pending.nonce,
        body_sha256_hex,
    );
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&pending.signature)
        .map_err(|_| anyhow!("signature mismatch"))
}

pub fn verify(
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &SignedHeaders,
    now: i64,
) -> Result<()> {
    let pending = verify_pending(headers, now)?;
    complete_verify(secret, method, path, &pending, &body_hash(body))
}

/// Canonical string the agent signs for every response envelope:
/// `NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY)`.
///
/// The `nonce` is the one the panel sent with the request, echoed back, so a
/// captured response can never be replayed against a different request. The
/// `path` is the request's raw path+query exactly as the panel signed it.
/// `body` is the envelope serialized WITHOUT the `signature`/`nonce` fields
/// (the agent appends them only after computing the MAC).
///
/// Hash-first variant: the caller supplies the SHA-256 of the body instead of
/// the body itself ([`response_canonical_hash`]). Byte-identical output for
/// the same body, so the envelope path and the streaming path sign the same
/// canonical string.
pub fn response_canonical_hash(
    node_id: &str,
    method: &str,
    path: &str,
    status: u16,
    nonce: &str,
    sha256_hex: &str,
) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        node_id,
        method.to_ascii_uppercase(),
        path,
        status,
        nonce,
        sha256_hex
    )
}

pub fn response_canonical(
    node_id: &str,
    method: &str,
    path: &str,
    status: u16,
    nonce: &str,
    body: &[u8],
) -> String {
    response_canonical_hash(node_id, method, path, status, nonce, &body_hash(body))
}

/// Sign a response envelope (serialized without `signature`/`nonce`) and
/// return the hex HMAC the agent appends to the envelope. The panel verifies
/// with [`verify_response`] before parsing the envelope.
pub fn sign_response(
    secret: &str,
    method: &str,
    path: &str,
    status: u16,
    nonce: &str,
    body: &[u8],
    node_id: &str,
) -> Result<String> {
    let payload = response_canonical(node_id, method, path, status, nonce, body);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Outcome of authenticating a node's response envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseAuth {
    /// The envelope carried no signature fields: the agent predates response
    /// signing. The caller accepts the response with a warning while the
    /// fleet upgrades (rejecting would brick every legacy node at once).
    Unsigned,
    /// The envelope's signature matched the canonical string.
    Verified,
}

/// Locate a signed response's trailing `,"signature":"<64-hex>","nonce":"<request-nonce>"}`
/// block and return (signed bytes, signature hex) — or `None` when the
/// envelope carries no such block at its tail.
///
/// serde emits struct fields in declaration order and both fields are declared
/// last, so on the wire they are exactly the trailing substrings this parses.
/// The signature value is 64 lowercase hex and the echoed nonce a bare uuid
/// string, so the text surgery is unambiguous. The agent replaced the
/// envelope's closing `}` with the appended fields, so the signed bytes are
/// everything before the marker plus the closing brace.
///
/// This function is also the signed-vs-unsigned discriminator for
/// [`verify_response`]: it reports a signature only when the marker opens the
/// envelope's FINAL field — the block must run unbroken to the closing `}` at
/// end of input. A `,"signature":"` marker anywhere else is payload content of
/// a legacy unsigned response, not a signature, and yields `None` so the
/// envelope takes the Unsigned path instead of a false-positive rejection.
///
/// `Err` is reserved for a marker that IS at the envelope tail but echoes the
/// wrong request nonce (a forged, stale, or corrupt capture); the caller
/// rejects those.
fn split_response_signature(
    raw: &[u8],
    request_nonce: &str,
) -> Result<Option<(Vec<u8>, String)>> {
    let s = match std::str::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    // The last occurrence is the agent's appended block: it follows every
    // payload byte, so `rfind` lands on the true tail marker even when the
    // payload itself contains the marker substring.
    let marker = ",\"signature\":\"";
    let Some(at) = s.rfind(marker) else {
        // No marker anywhere: a pre-upgrade agent's unsigned envelope.
        return Ok(None);
    };
    let val_at = at + marker.len();
    let Some(hex_end) = val_at.checked_add(64) else {
        return Ok(None);
    };
    if !s
        .get(val_at..hex_end)
        .is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        // Not 64 hex digits after the marker: the marker is payload content.
        return Ok(None);
    }
    let signature = s[val_at..hex_end].to_string();
    let Some(after_nonce) = s
        .get(hex_end..)
        .and_then(|t| t.strip_prefix("\",\"nonce\":\""))
    else {
        return Ok(None);
    };
    let Some(quote) = after_nonce.find('"') else {
        return Ok(None);
    };
    if &after_nonce[quote..] != "\"}" {
        // The block is followed by more JSON (e.g. a nested `}`): the marker
        // is NOT at the envelope tail — legacy unsigned payload content.
        return Ok(None);
    }
    if &after_nonce[..quote] != request_nonce {
        bail!("malformed or misaddressed response signature");
    }
    let mut signed = s.as_bytes()[..at].to_vec();
    signed.push(b'}');
    Ok(Some((signed, signature)))
}

/// Verify a node's response envelope BEFORE it is parsed: the canonical string
/// is recomputed over the envelope bytes with the agent's `signature`/`nonce`
/// fields stripped, using the nonce this client sent with the request.
///
/// Returns [`ResponseAuth::Unsigned`] when the envelope carries no signature
/// block at its tail (pre-upgrade agent — accept with a warning). A signature
/// that is present but malformed, echoes the wrong nonce, or fails the MAC is
/// an error: the response is forged, stale, or corrupt and must not be trusted.
pub fn verify_response(
    secret: &str,
    method: &str,
    path: &str,
    status: u16,
    request_nonce: &str,
    raw: &[u8],
    node_id: &str,
) -> Result<ResponseAuth> {
    // Signed-vs-unsigned is decided by POSITION, not presence: an envelope is
    // signed only when the `,"signature":"` marker opens its FINAL field
    // (`split_response_signature` returns `Some`). A marker anywhere else is
    // payload content of a legacy unsigned response and is accepted via the
    // Unsigned path — the old anywhere-substring search misclassified those
    // as signed and rejected them. A marker at the tail that echoes the wrong
    // nonce is an error: the response is forged, stale, or corrupt.
    let Some((signed_body, signature)) = split_response_signature(raw, request_nonce)? else {
        return Ok(ResponseAuth::Unsigned);
    };
    let payload = response_canonical(node_id, method, path, status, request_nonce, &signed_body);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    let signature =
        hex::decode(signature).map_err(|_| anyhow!("invalid response signature encoding"))?;
    mac.verify_slice(&signature)
        .map_err(|_| anyhow!("response signature mismatch"))?;
    Ok(ResponseAuth::Verified)
}

/// Build the fixed-length signature footer appended to a streaming snapshot
/// response body: `\nVOLTSTREAM1\n<64-hex-hmac>\n`. See [`split_stream_footer`].
pub fn stream_footer(signature: &str) -> Vec<u8> {
    format!("\n{STREAM_FOOTER_MAGIC}\n{signature}\n").into_bytes()
}

/// Split a streaming snapshot response body into (archive bytes, signature
/// hex). The footer is mandatory and length-fixed, so the split is
/// unambiguous: the last [`STREAM_FOOTER_LEN`] bytes are the signature
/// whatever the archive bytes look like. Anything else — a body shorter than
/// the footer, a wrong magic, or a non-hex signature — is an error: the
/// stream was truncated, corrupted, or was never a streaming response.
pub fn split_stream_footer(body: &[u8]) -> Result<(&[u8], &str)> {
    if body.len() < STREAM_FOOTER_LEN {
        bail!("streaming response body is shorter than its signature footer");
    }
    let at = body.len() - STREAM_FOOTER_LEN;
    let footer = std::str::from_utf8(&body[at..]).map_err(|_| anyhow!("malformed streaming footer"))?;
    let mut parts = footer.split('\n');
    if parts.next() != Some("") {
        bail!("malformed streaming footer");
    }
    if parts.next() != Some(STREAM_FOOTER_MAGIC) {
        bail!("malformed streaming footer");
    }
    let signature = parts.next().unwrap_or_default();
    if signature.len() != 64 || !signature.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("malformed streaming footer");
    }
    if parts.next() != Some("") || parts.next().is_some() {
        bail!("malformed streaming footer");
    }
    Ok((&body[..at], signature))
}

/// Sign a streaming response body with the agent's secret: the HMAC covers
/// `NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY)` — the exact canonical
/// string the envelope path signs — where `body_sha256_hex` is the SHA-256 of
/// the streamed bytes and `nonce` the request's echoed nonce. The nonce is
/// bound cryptographically rather than carried on the wire: a captured footer
/// cannot verify against a different request, exactly like the envelope path.
pub fn sign_stream_response(
    secret: &str,
    method: &str,
    path: &str,
    status: u16,
    nonce: &str,
    body_sha256_hex: &str,
    node_id: &str,
) -> Result<String> {
    let payload = response_canonical_hash(node_id, method, path, status, nonce, body_sha256_hex);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verify a streaming response's tail signature against the SHA-256 of the
/// bytes the panel actually received (computed while streaming). Same
/// guarantees as [`verify_response`] for the envelope path: the signature is
/// bound to the request's method, path, status, and echoed nonce, so a
/// forged, stale, or corrupt stream is rejected.
#[allow(clippy::too_many_arguments)] // mirrors the pinned canonical string's fields
pub fn verify_stream_response(
    secret: &str,
    method: &str,
    path: &str,
    status: u16,
    request_nonce: &str,
    body_sha256_hex: &str,
    signature: &str,
    node_id: &str,
) -> Result<()> {
    let payload =
        response_canonical_hash(node_id, method, path, status, request_nonce, body_sha256_hex);
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    let signature =
        hex::decode(signature).map_err(|_| anyhow!("invalid response signature encoding"))?;
    mac.verify_slice(&signature)
        .map_err(|_| anyhow!("response signature mismatch"))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeCapacity {
    pub memory_total: u64,
    pub memory_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub cpu_percent: f64,
    pub cpu_threads: usize,
    pub load_1: f64,
    pub load_5: f64,
    pub load_15: f64,
    pub servers_running: usize,
    pub servers_total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHeartbeat {
    pub daemon_version: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub started_at: String,
    pub capacity: NodeCapacity,
    /// SHA-256 fingerprint of the agent's self-signed certificate. Empty for
    /// plaintext agents. `serde(default)` keeps old agents — and the panel
    /// round-tripping the struct — on the wire.
    #[serde(default)]
    pub tls_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpec {
    pub uuid: String,
    pub name: String,
    pub startup: String,
    pub stop_command: String,
    pub memory_mb: u64,
    pub disk_mb: u64,
    pub cpu_percent: u64,
    pub port: Option<u16>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub env: Vec<(String, String)>,
    pub auto_restart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionRequest {
    pub spec: ServerSpec,
    #[serde(default)]
    pub files: Vec<ProvisionFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionFile {
    pub path: String,
    pub content_b64: String,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerRequest {
    pub action: PowerAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Start,
    Stop,
    Restart,
    Kill,
}

impl PowerAction {
    /// Stable lowercase name; used for audit-log actions and API echoes.
    pub const fn as_str(self) -> &'static str {
        match self {
            PowerAction::Start => "start",
            PowerAction::Stop => "stop",
            PowerAction::Restart => "restart",
            PowerAction::Kill => "kill",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteServerStats {
    pub uuid: String,
    pub state: String,
    pub pid: Option<u32>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub uptime_secs: u64,
    pub restart_count: u64,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleCommand {
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleSnapshot {
    pub lines: Vec<String>,
    pub cursor: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileListRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
    pub mode: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FileOperation {
    Mkdir { path: String },
    Touch { path: String },
    Rename { from: String, to: String },
    Copy { from: String, to: String },
    Move { from: String, destination: String },
    Delete { path: String },
    Chmod { path: String, mode: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWriteRequest {
    pub path: String,
    pub content_b64: String,
    pub append: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotResponse {
    pub archive_b64: String,
    pub size_bytes: u64,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSnapshotRequest {
    pub archive_b64: String,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeApiResponse<T> {
    pub ok: bool,
    pub data: Option<T>,
    pub error: Option<String>,
    /// HMAC-SHA256 over the envelope serialized WITHOUT this field and `nonce`
    /// (see [`sign_response`]). Absent on responses from agents predating
    /// response signing; `#[serde(default)]` keeps old agents and old panels
    /// interoperating with new ones in both directions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// The request's nonce, echoed back so the panel can bind its
    /// verification to the exact request it sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

impl<T> NodeApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            signature: None,
            nonce: None,
        }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
            signature: None,
            nonce: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trip_and_tamper_detection() {
        let body = br#"{"action":"start"}"#;
        let signed = sign(
            "secret",
            "POST",
            "/v1/servers/a/power",
            body,
            "11111111-1111-4111-8111-111111111111",
        )
        .unwrap();
        verify(
            "secret",
            "POST",
            "/v1/servers/a/power",
            body,
            &signed,
            signed.timestamp,
        )
        .unwrap();
        assert!(verify(
            "secret",
            "POST",
            "/v1/servers/a/power",
            b"tampered",
            &signed,
            signed.timestamp
        )
        .is_err());
        assert!(verify(
            "secret",
            "POST",
            "/v1/servers/a/power",
            body,
            &signed,
            signed.timestamp + 91
        )
        .is_err());
    }

    #[test]
    fn canonical_string_is_stable_wire_format() {
        // The canonical string is the HMAC input; changing it invalidates every
        // in-flight agent. Pin the exact shape:
        // NODE_ID\nMETHOD\nPATH\nTIMESTAMP\nNONCE\nSHA256(BODY).
        let hash = body_hash(b"hi");
        assert_eq!(
            canonical("node-a", "post", "/v1/x", 1234, "n1", b"hi"),
            format!("node-a\nPOST\n/v1/x\n1234\nn1\n{hash}")
        );
        assert_eq!(
            canonical("node-b", "GET", "/v1/y", 0, "", b""),
            format!("node-b\nGET\n/v1/y\n0\n\n{}", body_hash(b""))
        );
    }

    #[test]
    fn signature_binds_the_node_identity() {
        let body = b"{}";
        let signed = sign(
            "secret",
            "POST",
            "/v1/x",
            body,
            "11111111-1111-4111-8111-111111111111",
        )
        .unwrap();
        verify("secret", "POST", "/v1/x", body, &signed, signed.timestamp).unwrap();
        // The node id is inside the signed payload: re-targeting the same
        // signature at another node must fail even with the same secret.
        let mut spoofed = signed.clone();
        spoofed.node_id = "22222222-2222-4222-8222-222222222222".into();
        assert!(
            verify("secret", "POST", "/v1/x", body, &spoofed, signed.timestamp).is_err(),
            "a signature minted for one node must not verify for another"
        );
    }

    #[test]
    fn timestamp_skew_check_is_overflow_free() {
        let body = b"x";
        let mut headers = SignedHeaders {
            node_id: "33333333-3333-4333-8333-333333333333".into(),
            timestamp: 0,
            nonce: "n".into(),
            signature: "00".repeat(32),
        };
        // Attacker-controlled extreme timestamps must fail cleanly, never
        // panic (`(now - ts).abs()` overflowed on i64::MIN in debug) or wrap.
        headers.timestamp = i64::MIN;
        assert!(verify("s", "GET", "/", body, &headers, 0).is_err());
        headers.timestamp = i64::MAX;
        assert!(verify("s", "GET", "/", body, &headers, 0).is_err());
        headers.timestamp = 0;
        assert!(verify("s", "GET", "/", body, &headers, i64::MAX).is_err());
        assert!(verify("s", "GET", "/", body, &headers, i64::MIN).is_err());
        // The ±90s boundary is preserved in both directions.
        let signed = sign(
            "s",
            "GET",
            "/",
            body,
            "33333333-3333-4333-8333-333333333333",
        )
        .unwrap();
        assert!(verify("s", "GET", "/", body, &signed, signed.timestamp - 90).is_ok());
        assert!(verify("s", "GET", "/", body, &signed, signed.timestamp + 90).is_ok());
        assert!(verify("s", "GET", "/", body, &signed, signed.timestamp - 91).is_err());
        assert!(verify("s", "GET", "/", body, &signed, signed.timestamp + 91).is_err());
    }

    #[test]
    fn rotated_secret_invalidates_old_hmac() {
        let body = br#"{"a":1}"#;
        let signed = sign(
            "old-secret",
            "POST",
            "/v1/servers/u/power",
            body,
            "44444444-4444-4444-8444-444444444444",
        )
        .unwrap();
        // A rotated shared secret must reject requests signed with the old one,
        // and the new secret must accept fresh requests.
        assert!(verify(
            "new-secret",
            "POST",
            "/v1/servers/u/power",
            body,
            &signed,
            signed.timestamp,
        )
        .is_err());
        let resigned = sign(
            "new-secret",
            "POST",
            "/v1/servers/u/power",
            body,
            "44444444-4444-4444-8444-444444444444",
        )
        .unwrap();
        verify(
            "new-secret",
            "POST",
            "/v1/servers/u/power",
            body,
            &resigned,
            resigned.timestamp,
        )
        .unwrap();
    }

    #[test]
    fn heartbeat_tls_fingerprint_defaults_and_round_trips() {
        // A pre-fingerprint agent omits `tls_fingerprint` entirely; the field
        // must default to empty so enrollment/heartbeat accept the payload.
        // NodeCapacity stays strict: the legacy fixture still sends every
        // capacity field, exactly as `voltd` does today.
        let legacy = r#"{"daemon_version":"1.0","hostname":"h","os":"linux","arch":"x64","started_at":"2026-01-01T00:00:00Z","capacity":{"memory_total":0,"memory_used":0,"disk_total":0,"disk_used":0,"cpu_percent":0.0,"cpu_threads":0,"load_1":0.0,"load_5":0.0,"load_15":0.0,"servers_running":0,"servers_total":0}}"#;
        let hb: NodeHeartbeat = serde_json::from_str(legacy).unwrap();
        assert_eq!(
            hb.tls_fingerprint, "",
            "missing field must default to empty"
        );

        let fp = "ab".repeat(32);
        let hb = NodeHeartbeat {
            daemon_version: "1.0".into(),
            hostname: "h".into(),
            os: "linux".into(),
            arch: "x64".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            capacity: NodeCapacity::default(),
            tls_fingerprint: fp.clone(),
        };
        let round: NodeHeartbeat =
            serde_json::from_str(&serde_json::to_string(&hb).unwrap()).unwrap();
        assert_eq!(round.tls_fingerprint, fp);
    }

    #[test]
    fn heartbeat_without_capacity_fields_is_rejected() {
        // NodeCapacity validation is not weakened for the legacy case: a body
        // that drops capacity fields still fails loudly instead of zeroing.
        let truncated = r#"{"daemon_version":"1.0","hostname":"h","os":"linux","arch":"x64","started_at":"2026-01-01T00:00:00Z","capacity":{}}"#;
        assert!(serde_json::from_str::<NodeHeartbeat>(truncated).is_err());
    }

    const NODE_A: &str = "11111111-1111-4111-8111-111111111111";

    /// Mimic the agent: serialize the envelope without signature/nonce, sign
    /// those exact bytes, then append the fields to the raw JSON.
    fn signed_wire<T: Serialize>(
        secret: &str,
        method: &str,
        path: &str,
        status: u16,
        nonce: &str,
        envelope: &NodeApiResponse<T>,
    ) -> String {
        let body = serde_json::to_vec(envelope).unwrap();
        let s = std::str::from_utf8(&body).unwrap();
        let sig = sign_response(secret, method, path, status, nonce, &body, NODE_A).unwrap();
        format!("{},\"signature\":\"{}\",\"nonce\":\"{}\"}}", &s[..s.len() - 1], sig, nonce)
    }

    #[test]
    fn response_signing_round_trip_and_tamper_detection() {
        let envelope = NodeApiResponse::success(serde_json::json!({ "status": "ok" }));
        let wire = signed_wire("secret", "GET", "/v1/health", 200, "req-nonce-1", &envelope);
        assert_eq!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                wire.as_bytes(),
                NODE_A,
            )
            .unwrap(),
            ResponseAuth::Verified
        );
        // A tampered body must fail the MAC.
        let forged = wire.replace(r#"{"status":"ok"}"#, r#"{"status":"owned"}"#);
        assert!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                forged.as_bytes(),
                NODE_A,
            )
            .is_err(),
            "a modified response body must be rejected"
        );
        // A signature echoed against a different request nonce is a stale or
        // forged capture and must be rejected, not treated as unsigned.
        assert!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "other-nonce",
                wire.as_bytes(),
                NODE_A,
            )
            .is_err(),
            "the echoed nonce must match the request's nonce"
        );
        // The response binds the request path/method/status: re-verifying the
        // same envelope against a different path fails.
        assert!(
            verify_response(
                "secret",
                "GET",
                "/v1/other",
                200,
                "req-nonce-1",
                wire.as_bytes(),
                NODE_A,
            )
            .is_err()
        );
    }

    #[test]
    fn response_canonical_string_is_pinned_wire_format() {
        // The response canonical string is the HMAC input for the
        // panel<->node channel; changing it invalidates in-flight agents and
        // panels. Pin the exact shape:
        // NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY).
        let hash = body_hash(b"hi");
        assert_eq!(
            response_canonical(NODE_A, "post", "/v1/x", 200, "n1", b"hi"),
            format!("{NODE_A}\nPOST\n/v1/x\n200\nn1\n{hash}")
        );
        assert_eq!(
            response_canonical(NODE_A, "GET", "/v1/y", 500, "", b""),
            format!("{NODE_A}\nGET\n/v1/y\n500\n\n{}", body_hash(b""))
        );
    }

    #[test]
    fn unsigned_response_is_reported_unsigned() {
        // A pre-upgrade agent's envelope has no signature fields at all: the
        // panel accepts it with a warning, it must not hard-fail.
        let wire = r#"{"ok":true,"data":{"status":"ok"},"error":null}"#;
        assert_eq!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                wire.as_bytes(),
                NODE_A,
            )
            .unwrap(),
            ResponseAuth::Unsigned
        );
    }

    #[test]
    fn marker_inside_unsigned_payload_is_not_treated_as_signed() {
        // A legacy unsigned response whose payload merely CONTAINS the
        // `,"signature":"` marker substring must be accepted via the Unsigned
        // path: the signed/unsigned discriminator is the marker at the
        // ENVELOPE TAIL, not its presence anywhere. The old anywhere-substring
        // search misclassified this as signed and rejected it.
        let marker = ",\"signature\":\"";
        let plain = r#"{"ok":true,"data":{"status":"ok","signature":"noise"},"error":null}"#;
        assert!(plain.contains(marker), "fixture must contain the marker substring");
        assert_eq!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                plain.as_bytes(),
                NODE_A,
            )
            .unwrap(),
            ResponseAuth::Unsigned
        );
        // Even a marker followed by 64 hex digits and a nonce-shaped tail
        // inside the payload (still not at the envelope tail — more JSON
        // follows the block) stays unsigned.
        let hex = "ab".repeat(32);
        let nested = format!(
            r#"{{"ok":true,"data":{{"severity":"high","signature":"{hex}","nonce":"req-nonce-1"}},"error":null}}"#
        );
        assert!(nested.contains(marker));
        assert_eq!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                nested.as_bytes(),
                NODE_A,
            )
            .unwrap(),
            ResponseAuth::Unsigned
        );
    }

    #[test]
    fn signed_response_whose_payload_contains_the_marker_still_verifies() {
        // The agent's signature block is appended at the very end, so a
        // payload that itself contains the marker substring must not confuse
        // the tail discriminator: `rfind` lands on the true appended marker.
        // `serde_json::Map` sorts keys, so `a` must sort before `signature`
        // for the payload marker to be preceded by a comma in the wire.
        let envelope = NodeApiResponse::success(serde_json::json!({
            "a": 1,
            "signature": "noise",
        }));
        let wire = signed_wire("secret", "GET", "/v1/health", 200, "req-nonce-1", &envelope);
        assert!(
            wire.contains(",\"signature\":\"noise\""),
            "the payload marker must be present in the wire for this test to be meaningful"
        );
        assert_eq!(
            verify_response(
                "secret",
                "GET",
                "/v1/health",
                200,
                "req-nonce-1",
                wire.as_bytes(),
                NODE_A,
            )
            .unwrap(),
            ResponseAuth::Verified
        );
    }

    #[test]
    fn envelope_signing_fields_are_optional_and_defaulted() {
        // Old agent -> new panel: missing fields default to None.
        let legacy: NodeApiResponse<serde_json::Value> =
            serde_json::from_str(r#"{"ok":true,"data":{"a":1},"error":null}"#).unwrap();
        assert!(legacy.signature.is_none() && legacy.nonce.is_none());
        // New agent -> old panel: the fields serialize only when present, so
        // an unsigned envelope's bytes are unchanged from the pre-signing era.
        let unsigned = serde_json::to_string(&NodeApiResponse::<serde_json::Value>::success(
            serde_json::json!({"a": 1}),
        ))
        .unwrap();
        assert_eq!(
            unsigned,
            r#"{"ok":true,"data":{"a":1},"error":null}"#,
            "constructors must not emit signature/nonce keys"
        );
        // New agent -> old panel: unknown fields are ignored by serde.
        let signed = serde_json::from_str::<NodeApiResponse<serde_json::Value>>(
            r#"{"ok":true,"data":{"a":1},"error":null,"signature":"aa","nonce":"n1"}"#,
        )
        .unwrap();
        assert_eq!(signed.signature.as_deref(), Some("aa"));
        assert_eq!(signed.nonce.as_deref(), Some("n1"));
    }

    #[test]
    fn verify_rejects_non_uuid_node_id_before_accepting() {
        // Node ids are UUIDs minted by the panel; a malformed id is a cheap
        // structural rejection that never reaches the body hash.
        let signed = sign("s", "GET", "/v1/x", b"x", "not-a-uuid").unwrap();
        assert!(verify("s", "GET", "/v1/x", b"x", &signed, signed.timestamp).is_err());
    }

    const SNAP_PATH: &str = "/v1/servers/u/snapshot";

    #[test]
    fn stream_footer_round_trip_and_tamper_detection() {
        // The streaming signature binds the same canonical string the
        // envelope path signs, with the body hash supplied instead of the
        // body: NODE_ID\nMETHOD\nPATH\nSTATUS\nNONCE\nSHA256(BODY).
        let archive = b"gzip-archive-bytes";
        let sha = body_hash(archive);
        let sig = sign_stream_response("secret", "GET", SNAP_PATH, 200, "req-nonce", &sha, NODE_A)
            .unwrap();
        let mut wire = archive.to_vec();
        wire.extend_from_slice(&stream_footer(&sig));
        let (got_archive, got_sig) = split_stream_footer(&wire).unwrap();
        assert_eq!(got_archive, archive);
        assert_eq!(got_sig, sig);
        verify_stream_response(
            "secret", "GET", SNAP_PATH, 200, "req-nonce", &sha, got_sig, NODE_A,
        )
        .unwrap();

        // A single flipped byte in the archive changes the hash: reject.
        let mut tampered = archive.to_vec();
        tampered[5] ^= 0x01;
        assert!(
            verify_stream_response(
                "secret",
                "GET",
                SNAP_PATH,
                200,
                "req-nonce",
                &body_hash(&tampered),
                got_sig,
                NODE_A,
            )
            .is_err(),
            "a modified stream must fail the MAC"
        );
        // A signature echoed against a different request nonce is a stale or
        // forged capture and must be rejected.
        assert!(
            verify_stream_response(
                "secret", "GET", SNAP_PATH, 200, "other-nonce", &sha, got_sig, NODE_A,
            )
            .is_err(),
            "the streaming signature must bind the request nonce"
        );
        // The signature binds path/status/method like the envelope path.
        assert!(
            verify_stream_response(
                "secret", "GET", "/v1/other", 200, "req-nonce", &sha, got_sig, NODE_A,
            )
            .is_err()
        );
        assert!(
            verify_stream_response(
                "secret", "GET", SNAP_PATH, 500, "req-nonce", &sha, got_sig, NODE_A,
            )
            .is_err()
        );
    }

    #[test]
    fn stream_footer_is_fixed_length_and_rejects_malformed() {
        let sig = "ab".repeat(32);
        let footer = stream_footer(&sig);
        assert_eq!(footer.len(), STREAM_FOOTER_LEN);
        // Truncated stream: any cut leaves a short or partial footer.
        for cut in [0usize, 1, 10, footer.len() - 1] {
            assert!(
                split_stream_footer(&footer[..cut]).is_err(),
                "a truncated body must be rejected (cut={cut})"
            );
        }
        // Wrong magic / non-hex signature / wrong length.
        assert!(split_stream_footer(&stream_footer("zz")).is_err());
        let bad_magic = format!("\nNOTSTREAM\n{sig}\n");
        assert!(split_stream_footer(bad_magic.as_bytes()).is_err());
        let no_trailing = format!("\n{STREAM_FOOTER_MAGIC}\n{sig}");
        assert!(split_stream_footer(no_trailing.as_bytes()).is_err());
    }

    #[test]
    fn response_canonical_hash_matches_envelope_canonical() {
        // The streaming path signs SHA-256 instead of the body; the canonical
        // string must be byte-identical to the envelope path's.
        assert_eq!(
            response_canonical(NODE_A, "GET", SNAP_PATH, 200, "n1", b"hi"),
            response_canonical_hash(NODE_A, "GET", SNAP_PATH, 200, "n1", &body_hash(b"hi"))
        );
        assert_eq!(
            canonical(NODE_A, "post", "/v1/x", 1234, "n1", b"hi"),
            canonical_hash(NODE_A, "post", "/v1/x", 1234, "n1", &body_hash(b"hi"))
        );
    }

    #[test]
    fn sign_body_hash_matches_sign_for_the_same_body() {
        // The streaming restore signs against a precomputed SHA-256; the
        // canonical string must be identical to signing the buffered body,
        // and the minted header must verify the real body.
        let body = b"the-archive";
        let sha = body_hash(body);
        assert_eq!(
            canonical(NODE_A, "POST", SNAP_PATH, 1234, "fixed-nonce", body),
            canonical_hash(NODE_A, "POST", SNAP_PATH, 1234, "fixed-nonce", &sha),
            "body-hash signing must produce the same canonical string"
        );
        let signed = sign_body_hash("secret", "POST", SNAP_PATH, &sha, NODE_A).unwrap();
        verify("secret", "POST", SNAP_PATH, body, &signed, signed.timestamp).unwrap();
    }

    #[test]
    fn pending_verify_defers_only_the_body_mac() {
        let body = b"the-archive";
        let signed = sign("s", "POST", SNAP_PATH, body, NODE_A).unwrap();
        // Structural checks run at the pending stage...
        let pending =
            verify_pending(&signed, signed.timestamp).expect("structural checks must pass");
        assert!(
            verify_pending(&signed, signed.timestamp + 91).is_err(),
            "timestamp skew must be rejected before the body is read"
        );
        // ...and the MAC only at completion, against the hash actually seen.
        complete_verify("s", "POST", SNAP_PATH, &pending, &body_hash(body)).unwrap();
        assert!(
            complete_verify("s", "POST", SNAP_PATH, &pending, &body_hash(b"other")).is_err(),
            "a body different from the one signed must fail the deferred MAC"
        );
        assert!(
            complete_verify("s", "POST", "/v1/other", &pending, &body_hash(body)).is_err(),
            "the deferred MAC must still bind the path"
        );
    }

    #[test]
    fn verify_is_unchanged_when_split_into_pending_and_complete() {
        // `verify` now composes verify_pending + complete_verify; the pinned
        // request-signing behavior must not drift.
        let body = br#"{"action":"start"}"#;
        let signed = sign("secret", "POST", SNAP_PATH, body, NODE_A).unwrap();
        verify("secret", "POST", SNAP_PATH, body, &signed, signed.timestamp).unwrap();
        assert!(verify("secret", "POST", SNAP_PATH, b"tampered", &signed, signed.timestamp).is_err());
    }
}