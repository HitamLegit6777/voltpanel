//! Shared wire protocol between VoltPanel and the `voltd` node daemon.
//!
//! Requests are authenticated with HMAC-SHA256 over:
//! `METHOD\nPATH\nTIMESTAMP\nNONCE\nSHA256(BODY)`.
//! The daemon rejects timestamps outside a 90-second window and re-used nonces.
use anyhow::{anyhow, bail, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SIGNATURE_HEADER: &str = "x-volt-signature";
pub const TIMESTAMP_HEADER: &str = "x-volt-timestamp";
pub const NONCE_HEADER: &str = "x-volt-nonce";
pub const NODE_ID_HEADER: &str = "x-volt-node";
pub const MAX_CLOCK_SKEW_SECS: i64 = 90;

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

pub fn canonical(method: &str, path: &str, timestamp: i64, nonce: &str, body: &[u8]) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        timestamp,
        nonce,
        body_hash(body)
    )
}

pub fn sign(secret: &str, method: &str, path: &str, body: &[u8], node_id: &str) -> Result<SignedHeaders> {
    let timestamp = chrono::Utc::now().timestamp();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let payload = canonical(method, path, timestamp, &nonce, body);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    Ok(SignedHeaders {
        node_id: node_id.to_string(),
        timestamp,
        nonce,
        signature: hex::encode(mac.finalize().into_bytes()),
    })
}

pub fn verify(
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &SignedHeaders,
    now: i64,
) -> Result<()> {
    if (now - headers.timestamp).abs() > MAX_CLOCK_SKEW_SECS {
        bail!("signed request expired");
    }
    let payload = canonical(method, path, headers.timestamp, &headers.nonce, body);
    let signature = hex::decode(&headers.signature).map_err(|_| anyhow!("invalid signature encoding"))?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| anyhow!("invalid HMAC key"))?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).map_err(|_| anyhow!("signature mismatch"))
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Start,
    Stop,
    Restart,
    Kill,
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
}

impl<T> NodeApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }

    pub fn failure(error: impl Into<String>) -> Self {
        Self { ok: false, data: None, error: Some(error.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trip_and_tamper_detection() {
        let body = br#"{"action":"start"}"#;
        let signed = sign("secret", "POST", "/v1/servers/a/power", body, "node-a").unwrap();
        verify("secret", "POST", "/v1/servers/a/power", body, &signed, signed.timestamp).unwrap();
        assert!(verify("secret", "POST", "/v1/servers/a/power", b"tampered", &signed, signed.timestamp).is_err());
        assert!(verify("secret", "POST", "/v1/servers/a/power", body, &signed, signed.timestamp + 91).is_err());
    }
}
