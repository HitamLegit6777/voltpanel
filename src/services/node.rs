//! Panel-side remote node client and replay-protection cache.
use crate::node_protocol::{
    self, ConsoleCommand, ConsoleSnapshot, FileOperation, FileWriteRequest, NodeApiResponse,
    PowerAction, PowerRequest, ProvisionRequest, RemoteFileEntry, RemoteServerStats,
    RestoreSnapshotRequest, SnapshotResponse,
};
use crate::nodes::Node;
use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct NodeClientError {
    pub status: Option<u16>,
    pub message: String,
}
impl NodeClientError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }
    fn remote(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }
}
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct NodeClient {
    /// WebPKI-validating client: plaintext nodes and nodes fronted by a real
    /// certificate for a real domain.
    plain: reqwest::Client,
    /// One client per pinned fingerprint. Building a client is expensive and the
    /// TLS config is immutable, so they are cached for the process lifetime;
    /// re-enrolling a node mints a new fingerprint and therefore a new entry.
    pinned: Arc<DashMap<String, reqwest::Client>>,
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(60))
        .user_agent(format!("VoltPanel/{}", env!("CARGO_PKG_VERSION")))
}

impl NodeClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            plain: builder().build()?,
            pinned: Arc::new(DashMap::new()),
        })
    }

    /// Client that will talk to `node`, pinning its self-signed certificate when
    /// one was recorded at enrollment.
    fn client_for(&self, node: &Node) -> Result<reqwest::Client> {
        let fp = crate::tls::normalize_fingerprint(&node.tls_fingerprint);
        if fp.is_empty() {
            return Ok(self.plain.clone());
        }
        if let Some(c) = self.pinned.get(&fp) {
            return Ok(c.clone());
        }
        let cfg = crate::tls::pinned_client_config(&fp)
            .with_context(|| format!("node '{}' has an invalid TLS fingerprint", node.name))?;
        let client = builder().use_preconfigured_tls((*cfg).clone()).build()?;
        self.pinned.insert(fp, client.clone());
        Ok(client)
    }

    async fn request<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        node: &Node,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        if !node.enrolled {
            bail!("node is not enrolled")
        }
        if !node.enabled {
            bail!("node is disabled")
        }
        let body_bytes = match body {
            Some(v) => serde_json::to_vec(v)?,
            None => Vec::new(),
        };
        let signed =
            node_protocol::sign(&node.secret, method.as_str(), path, &body_bytes, &node.uuid)?;
        let url = format!("{}{}", node.public_url.trim_end_matches('/'), path);
        let mut req = self
            .client_for(node)?
            .request(method, &url)
            .header(node_protocol::NODE_ID_HEADER, &signed.node_id)
            .header(
                node_protocol::TIMESTAMP_HEADER,
                signed.timestamp.to_string(),
            )
            .header(node_protocol::NONCE_HEADER, &signed.nonce)
            .header(node_protocol::SIGNATURE_HEADER, &signed.signature);
        if !body_bytes.is_empty() {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes)
        }
        let resp = req.send().await.map_err(|e| {
            NodeClientError::transport(format!("request to node '{}' failed: {e}", node.name))
        })?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if status == StatusCode::NO_CONTENT {
            return serde_json::from_str("null").map_err(Into::into);
        }
        let envelope: NodeApiResponse<T> = serde_json::from_slice(&bytes)
            .with_context(|| format!("node returned invalid JSON ({status})"))?;
        if !status.is_success() || !envelope.ok {
            return Err(NodeClientError::remote(
                status.as_u16(),
                format!(
                    "node error {status}: {}",
                    envelope
                        .error
                        .unwrap_or_else(|| "unknown node error".into())
                ),
            )
            .into());
        }
        envelope
            .data
            .ok_or_else(|| NodeClientError::transport("node response omitted data").into())
    }

    pub async fn health(&self, node: &Node) -> Result<serde_json::Value> {
        self.request::<(), _>(node, Method::GET, "/v1/health", None)
            .await
    }

    pub async fn provision(
        &self,
        node: &Node,
        req: &ProvisionRequest,
    ) -> Result<RemoteServerStats> {
        self.request(node, Method::POST, "/v1/servers", Some(req))
            .await
    }

    pub async fn delete_server(&self, node: &Node, uuid: &str) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}");
        self.request::<(), _>(node, Method::DELETE, &path, None)
            .await
    }

    pub async fn power(
        &self,
        node: &Node,
        uuid: &str,
        action: PowerAction,
    ) -> Result<RemoteServerStats> {
        let path = format!("/v1/servers/{uuid}/power");
        self.request(node, Method::POST, &path, Some(&PowerRequest { action }))
            .await
    }

    pub async fn stats(&self, node: &Node, uuid: &str) -> Result<RemoteServerStats> {
        let path = format!("/v1/servers/{uuid}/stats");
        self.request::<(), _>(node, Method::GET, &path, None).await
    }

    pub async fn command(&self, node: &Node, uuid: &str, command: &str) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}/command");
        self.request(
            node,
            Method::POST,
            &path,
            Some(&ConsoleCommand {
                command: command.into(),
            }),
        )
        .await
    }

    pub async fn console(&self, node: &Node, uuid: &str, cursor: u64) -> Result<ConsoleSnapshot> {
        let path = format!("/v1/servers/{uuid}/console?cursor={cursor}");
        self.request::<(), _>(node, Method::GET, &path, None).await
    }

    pub async fn files(
        &self,
        node: &Node,
        uuid: &str,
        path_value: &str,
    ) -> Result<Vec<RemoteFileEntry>> {
        let encoded: String = url::form_urlencoded::byte_serialize(path_value.as_bytes()).collect();
        let path = format!("/v1/servers/{uuid}/files?path={encoded}");
        self.request::<(), _>(node, Method::GET, &path, None).await
    }

    pub async fn read_file(
        &self,
        node: &Node,
        uuid: &str,
        file_path: &str,
    ) -> Result<serde_json::Value> {
        let encoded: String = url::form_urlencoded::byte_serialize(file_path.as_bytes()).collect();
        let path = format!("/v1/servers/{uuid}/files/content?path={encoded}");
        self.request::<(), _>(node, Method::GET, &path, None).await
    }

    pub async fn clear_console(&self, node: &Node, uuid: &str) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}/console/clear");
        self.request::<(), _>(node, Method::POST, &path, None).await
    }
    pub async fn write_file(
        &self,
        node: &Node,
        uuid: &str,
        req: &FileWriteRequest,
    ) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}/files/content");
        self.request(node, Method::POST, &path, Some(req)).await
    }

    pub async fn file_operation(
        &self,
        node: &Node,
        uuid: &str,
        operation: &FileOperation,
    ) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}/files/operation");
        self.request(node, Method::POST, &path, Some(operation))
            .await
    }
    pub async fn snapshot(&self, node: &Node, uuid: &str) -> Result<SnapshotResponse> {
        let path = format!("/v1/servers/{uuid}/snapshot");
        self.request::<(), _>(node, Method::GET, &path, None).await
    }
    pub async fn restore_snapshot(
        &self,
        node: &Node,
        uuid: &str,
        req: &RestoreSnapshotRequest,
    ) -> Result<bool> {
        let path = format!("/v1/servers/{uuid}/snapshot");
        self.request(node, Method::POST, &path, Some(req)).await
    }
}

#[derive(Clone, Default)]
pub struct NonceCache {
    values: Arc<DashMap<(String, String), i64>>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn use_once(&self, node_id: &str, nonce: &str, timestamp: i64) -> bool {
        self.prune(timestamp - node_protocol::MAX_CLOCK_SKEW_SECS * 2);
        self.values
            .insert((node_id.to_string(), nonce.to_string()), timestamp)
            .is_none()
    }

    fn prune(&self, before: i64) {
        self.values.retain(|_, ts| *ts >= before);
    }
}
