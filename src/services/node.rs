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
use base64::Engine;
use serde::de::DeserializeOwned;
use sha2::Digest;
use futures::TryStreamExt;
use std::io::{Read, Seek, Write};
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
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One client per pinned fingerprint, capped so a long-lived panel with many
/// rotated nodes cannot grow the cache without bound. Building a client is
/// expensive and the TLS config is immutable, so cached entries are reused;
/// the cap only bounds the number of concurrently pinned identities.
const MAX_PINNED_CLIENTS: usize = 128;

/// Correlation header sent to the node with every request, mirroring the
/// panel's per-request id so node logs can be joined to panel logs. Outside
/// the HMAC signature (the canonical string is pinned by tests); the node
/// treats it as informational and never trusts it.
pub const REQUEST_ID_HEADER: &str = "x-volt-request-id";

/// Bounded, coherent fingerprint → client cache.
///
/// A mutex-protected LRU is deliberately used instead of a lock-free map: the
/// cap must be a hard bound even under parallel lookups, and holding the mutex
/// across the check-and-insert makes the bound atomic — the cache can never
/// briefly exceed `MAX_PINNED_CLIENTS` the way a best-effort length check could.
struct PinnedClientCache {
    clients: HashMap<String, reqwest::Client>,
    /// LRU recency order; the back is the most recently used key.
    order: VecDeque<String>,
    limit: usize,
}

impl PinnedClientCache {
    fn new(limit: usize) -> Self {
        Self {
            clients: HashMap::new(),
            order: VecDeque::new(),
            limit,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.clients.len()
    }

    /// Return the cached client for `fp`, marking it most recently used.
    fn get(&mut self, fp: &str) -> Option<reqwest::Client> {
        let hit = self.clients.get(fp)?;
        self.order.retain(|k| *k != fp);
        self.order.push_back(fp.to_string());
        Some(hit.clone())
    }

    /// Insert `client` for `fp`, evicting the least recently used entry when
    /// the cache is at its hard cap. Re-inserting an existing fingerprint only
    /// refreshes its recency — it never grows the cache.
    fn insert(&mut self, fp: String, client: reqwest::Client) {
        if self.clients.contains_key(&fp) {
            self.order.retain(|k| *k != fp);
            self.order.push_back(fp);
            return;
        }
        if self.clients.len() >= self.limit {
            if let Some(stale) = self.order.pop_front() {
                self.clients.remove(&stale);
            }
        }
        self.order.push_back(fp.clone());
        self.clients.insert(fp, client);
    }
}

#[derive(Clone)]
pub struct NodeClient {
    /// WebPKI-validating client: plaintext nodes and nodes fronted by a real
    /// certificate for a real domain.
    plain: reqwest::Client,
    /// One client per pinned fingerprint, bounded by `MAX_PINNED_CLIENTS`.
    /// Re-enrolling a node mints a new fingerprint and therefore a new entry;
    /// overflow evicts the least recently used entry (the fingerprint set is
    /// small and eviction only forces a one-time rebuild for that node).
    pinned: Arc<Mutex<PinnedClientCache>>,
    /// Node uuids already warned about for sending unsigned responses. The
    /// reconcile loop polls every few seconds, so a blanket warn per response
    /// would flood the logs during a fleet upgrade; warn once per node and
    /// drop to debug afterwards.
    warned_unsigned: Arc<Mutex<HashSet<String>>>,
    /// When true, an unsigned node response is rejected outright instead of
    /// being accepted with a warning: `[security] require_signed_node_responses`.
    require_signed: bool,
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(60))
        .user_agent(format!("VoltPanel/{}", env!("CARGO_PKG_VERSION")))
}

/// Transport-error retry budget for interactive reads (the terminal console).
/// Only GETs are retried: a POST that times out may already have been applied
/// on the node, so re-issuing it could double-apply a side effect. HTTP
/// responses — even 5xx — are final and never retried.
const REQUEST_MAX_ATTEMPTS: u32 = 3;
/// Budget for non-interactive GET reads (health, stats, files, snapshot):
/// one retry. A dead node otherwise stalls the reconcile loop for ~60s behind
/// three 20s timeouts; a single retry halves the stall while still riding out
/// a transient connection reset.
const READ_MAX_ATTEMPTS: u32 = 2;
/// Exponential backoff base: 200ms, then 400ms between attempts.
const REQUEST_BACKOFF_BASE_MS: u64 = 200;

impl NodeClient {
    pub fn new() -> Result<Self> {
        Self::with_limit(
            MAX_PINNED_CLIENTS,
            crate::SETTINGS.security.require_signed_node_responses,
        )
    }

    /// Constructor with an injectable cap so tests can exercise eviction
    /// without minting `MAX_PINNED_CLIENTS` real clients.
    fn with_limit(limit: usize, require_signed: bool) -> Result<Self> {
        Ok(Self {
            plain: builder().build()?,
            pinned: Arc::new(Mutex::new(PinnedClientCache::new(limit))),
            warned_unsigned: Arc::new(Mutex::new(HashSet::new())),
            require_signed,
        })
    }

    /// Client that will talk to `node`, pinning its self-signed certificate when
    /// one was recorded at enrollment.
    fn client_for(&self, node: &Node) -> Result<reqwest::Client> {
        let fp = crate::tls::normalize_fingerprint(&node.tls_fingerprint);
        if fp.is_empty() {
            return Ok(self.plain.clone());
        }
        // The lock covers lookup, build, and insert, so the cap is a hard bound
        // even under parallel calls and a fingerprint is never built twice.
        let mut cache = self.pinned.lock();
        if let Some(c) = cache.get(&fp) {
            return Ok(c);
        }
        let cfg = crate::tls::pinned_client_config(&fp)
            .with_context(|| format!("node '{}' has an invalid TLS fingerprint", node.name))?;
        let client = builder().use_preconfigured_tls((*cfg).clone()).build()?;
        cache.insert(fp, client.clone());
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
        let url = format!("{}{}", node.public_url.trim_end_matches('/'), path);
        let client = self.client_for(node)?;
        let build = |body: Vec<u8>, signed: &node_protocol::SignedHeaders| {
            let mut req = client
                .request(method.clone(), &url)
                .header(node_protocol::NODE_ID_HEADER, &signed.node_id)
                .header(
                    node_protocol::TIMESTAMP_HEADER,
                    signed.timestamp.to_string(),
                )
                .header(node_protocol::NONCE_HEADER, &signed.nonce)
                .header(node_protocol::SIGNATURE_HEADER, &signed.signature);
            if !body.is_empty() {
                req = req
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body)
            }
            // Correlation id for node-side logs; not part of the HMAC.
            if let Ok(rid) = crate::REQUEST_ID.try_with(|id| id.clone()) {
                req = req.header(REQUEST_ID_HEADER, rid);
            }
            req
        };
        // Idempotent GETs survive transient transport failures with a bounded
        // retry; every other method is sent exactly once because a timed-out
        // write may have been applied server-side. Interactive console reads
        // keep the full budget; non-interactive polls get one retry so a dead
        // node cannot stall the reconcile loop behind three 20s timeouts.
        let attempts = match method {
            Method::GET if path.starts_with("/v1/servers/") && path.contains("/console?") => {
                REQUEST_MAX_ATTEMPTS
            }
            Method::GET => READ_MAX_ATTEMPTS,
            _ => 1,
        };
        let (resp, nonce) = if method == Method::GET {
            send_with_retry(attempts, || {
                // A retried GET must NOT reuse the first attempt's nonce: if
                // that attempt reached the node but the response was lost, the
                // retry would replay the identical (node, nonce) pair and the
                // node's replay cache would reject it as a duplicate even
                // though the read is idempotent. Mint a fresh nonce (re-sign)
                // per attempt; the path, method, and body stay the same.
                let signed = node_protocol::sign(
                    &node.secret,
                    method.as_str(),
                    path,
                    &body_bytes,
                    &node.uuid,
                )?;
                let nonce = signed.nonce.clone();
                Ok((build(body_bytes.clone(), &signed), nonce))
            })
            .await
            .map_err(|e| {
                NodeClientError::transport(format!("request to node '{}' failed: {e}", node.name))
            })?
        } else {
            // Writes are sent exactly once: a timed-out write may already have
            // been applied on the node, so re-issuing it could double-apply a
            // side effect. Sign once, send once.
            let signed = node_protocol::sign(
                &node.secret,
                method.as_str(),
                path,
                &body_bytes,
                &node.uuid,
            )?;
            let nonce = signed.nonce.clone();
            let resp = build(body_bytes, &signed)
                .send()
                .await
                .map_err(|e| {
                    NodeClientError::transport(format!(
                        "request to node '{}' failed: {e}",
                        node.name
                    ))
                })?;
            (resp, nonce)
        };
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if status == StatusCode::NO_CONTENT {
            return serde_json::from_str("null").map_err(Into::into);
        }
        self.authenticate_response(node, method.as_str(), path, &nonce, status, &bytes)
            .await?;
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

    /// Authenticate a node response BEFORE trusting the envelope: the node
    /// signs each response with the shared secret over a canonical string
    /// that echoes this request's nonce. An envelope that fails the MAC is
    /// forged, stale, or corrupt — parse it over the panel's dead body. A
    /// pre-upgrade agent sends no signature at all; accept it with a per-node
    /// warning until the fleet upgrades. Shared by `request()` and the
    /// streaming download/upload paths.
    async fn authenticate_response(
        &self,
        node: &Node,
        method: &str,
        path: &str,
        nonce: &str,
        status: StatusCode,
        bytes: &[u8],
    ) -> Result<()> {
        match node_protocol::verify_response(
            &node.secret,
            method,
            path,
            status.as_u16(),
            nonce,
            bytes,
            &node.uuid,
        ) {
            Ok(node_protocol::ResponseAuth::Verified) => {}
            Ok(node_protocol::ResponseAuth::Unsigned) => {
                if self.require_signed {
                    // Hard policy: an unsigned response is rejected, and the
                    // per-node warn becomes an error log (deduped so a node
                    // that keeps polling does not flood the logs).
                    let first = !self.warned_unsigned.lock().contains(&node.uuid);
                    if first {
                        self.warned_unsigned.lock().insert(node.uuid.clone());
                        tracing::error!(
                            node_id = %node.uuid,
                            %path,
                            "unsigned node response rejected: agent predates response signing and [security] require_signed_node_responses is enabled; upgrade voltd on this node"
                        );
                    } else {
                        tracing::debug!(node_id = %node.uuid, %path, "unsigned node response rejected");
                    }
                    return Err(NodeClientError::remote(
                        status.as_u16(),
                        "node response is unsigned but [security] \
                         require_signed_node_responses is enabled"
                            .to_string(),
                    )
                    .into());
                }
                let first = !self.warned_unsigned.lock().contains(&node.uuid);
                if first {
                    self.warned_unsigned.lock().insert(node.uuid.clone());
                    tracing::warn!(
                        node_id = %node.uuid,
                        %path,
                        "unsigned node response accepted: agent predates response signing; upgrade voltd to authenticate the node's responses"
                    );
                } else {
                    tracing::debug!(node_id = %node.uuid, %path, "unsigned node response");
                }
            }
            Err(e) => {
                return Err(NodeClientError::remote(
                    status.as_u16(),
                    format!("node response failed authentication: {e}"),
                )
                .into());
            }
        }
        Ok(())
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

    /// Download a server snapshot from `node` as a streamed tar.gz archive
    /// (temp file + byte size + SHA-256). Uses the streaming wire protocol
    /// (raw chunked body + tail signature footer) when the agent supports it,
    /// and transparently degrades to the legacy base64 JSON envelope for
    /// pre-streaming agents — the returned shape is identical either way.
    ///
    /// GET retry semantics are unchanged: transport failures before the
    /// response headers retry once (fresh nonce per attempt). Re-issuing a
    /// snapshot GET is safe because it is a read-only re-take — nothing is
    /// applied on the node — and each attempt produces its own archive whose
    /// signature is bound to that attempt's nonce, so a retry can never
    /// double-apply or replay a stale capture. A failure AFTER the response
    /// headers (truncated stream, bad signature) is not retried, matching the
    /// buffered path.
    pub async fn snapshot_stream(&self, node: &Node, uuid: &str) -> Result<StreamedSnapshot> {
        if !node.enrolled {
            bail!("node is not enrolled")
        }
        if !node.enabled {
            bail!("node is disabled")
        }
        let path = format!("/v1/servers/{uuid}/snapshot");
        let url = format!("{}{}", node.public_url.trim_end_matches('/'), path);
        let client = self.client_for(node)?;
        let (resp, nonce) = send_with_retry(READ_MAX_ATTEMPTS, || {
            let signed = node_protocol::sign(&node.secret, "GET", &path, &[], &node.uuid)?;
            let nonce = signed.nonce.clone();
            let mut req = client
                .request(Method::GET, &url)
                .header(node_protocol::NODE_ID_HEADER, &signed.node_id)
                .header(
                    node_protocol::TIMESTAMP_HEADER,
                    signed.timestamp.to_string(),
                )
                .header(node_protocol::NONCE_HEADER, &signed.nonce)
                .header(node_protocol::SIGNATURE_HEADER, &signed.signature)
                .header(node_protocol::STREAM_HEADER, "1");
            if let Ok(rid) = crate::REQUEST_ID.try_with(|id| id.clone()) {
                req = req.header(REQUEST_ID_HEADER, rid);
            }
            Ok((req, nonce))
        })
        .await
        .map_err(|e| {
            NodeClientError::transport(format!("request to node '{}' failed: {e}", node.name))
        })?;
        let status = resp.status();
        // Discriminate on content-type first, not the capability header
        // alone: a streaming agent echoes `x-volt-stream` on EVERY response,
        // including JSON error envelopes ("server must be stopped", replay
        // rejection). Only an octet-stream body with the header is a raw
        // streaming archive; anything else — JSON in particular — is an
        // envelope and must be parsed (and HMAC-verified) so the real error
        // surfaces instead of a bogus "malformed streaming footer".
        let streaming_body = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/octet-stream"))
            && resp.headers().contains_key(node_protocol::STREAM_HEADER);
        if streaming_body {
            Self::receive_streamed_snapshot(node, &path, &nonce, status, resp).await
        } else {
            // JSON envelope: a pre-streaming agent (no header) or an error
            // envelope from a streaming agent (header echoed). Authenticate
            // it exactly like `request()` and decode into the same temp-file
            // shape.
            let bytes = resp.bytes().await?;
            self.authenticate_response(node, "GET", &path, &nonce, status, &bytes)
                .await?;
            let envelope: NodeApiResponse<SnapshotResponse> = serde_json::from_slice(&bytes)
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
            let data = envelope
                .data
                .ok_or_else(|| NodeClientError::transport("node response omitted data"))?;
            if data.archive_b64.len() > crate::services::backups::MAX_REMOTE_ARCHIVE_B64_CHARS {
                bail!("remote snapshot archive too large; refusing");
            }
            let mut file = tempfile::NamedTempFile::new()?;
            let copied = std::io::copy(
                &mut base64::read::DecoderReader::new(
                    data.archive_b64.as_bytes(),
                    &base64::engine::general_purpose::STANDARD,
                ),
                file.as_file_mut(),
            )?;
            file.as_file_mut().flush()?;
            // Defense in depth: the envelope is HMAC-authenticated, but
            // re-check the agent's declared checksum against the actual bytes.
            // `io::copy` left the cursor at EOF, so rewind before hashing.
            file.as_file_mut().seek(std::io::SeekFrom::Start(0))?;
            let actual = file_sha256(file.as_file_mut())?;
            if actual != data.checksum {
                bail!("snapshot checksum mismatch");
            }
            Ok(StreamedSnapshot {
                archive: file,
                size_bytes: copied,
                checksum: actual,
            })
        }
    }

    /// Read a streaming snapshot response body into a temp file while hashing
    /// the archive bytes, parse the tail signature footer, and verify it
    /// against the running SHA-256. The temp file holds exactly the archive
    /// bytes (the footer is never written), so `checksum` covers the same
    /// bytes the agent signed. The raw-body cap mirrors the legacy path
    /// (`MAX_EXTRACT_TOTAL_BYTES`, plus the fixed footer).
    async fn receive_streamed_snapshot(
        node: &Node,
        path: &str,
        nonce: &str,
        status: StatusCode,
        mut resp: reqwest::Response,
    ) -> Result<StreamedSnapshot> {
        let cap = crate::services::backups::MAX_EXTRACT_TOTAL_BYTES;
        let mut file = tempfile::NamedTempFile::new()?;
        let mut hasher = sha2::Sha256::new();
        // Bytes received but not yet hashed; the last STREAM_FOOTER_LEN are
        // the signature footer and must never enter the hash or the file.
        let mut pending: Vec<u8> = Vec::new();
        let mut received: u64 = 0;
        loop {
            let Some(chunk) = resp.chunk().await? else {
                break;
            };
            received = received.saturating_add(chunk.len() as u64);
            if received > cap + node_protocol::STREAM_FOOTER_LEN as u64 {
                bail!(
                    "snapshot archive exceeds the {} MiB streaming cap",
                    cap / (1024 * 1024)
                );
            }
            pending.extend_from_slice(&chunk);
            let keep = pending
                .len()
                .saturating_sub(node_protocol::STREAM_FOOTER_LEN);
            if keep > 0 {
                hasher.update(&pending[..keep]);
                file.as_file_mut().write_all(&pending[..keep])?;
                pending.drain(..keep);
            }
        }
        let (archive, signature) = node_protocol::split_stream_footer(&pending)?;
        if !archive.is_empty() {
            hasher.update(archive);
            file.as_file_mut().write_all(archive)?;
        }
        file.as_file_mut().flush()?;
        let checksum = hex::encode(hasher.finalize());
        node_protocol::verify_stream_response(
            &node.secret,
            "GET",
            path,
            status.as_u16(),
            nonce,
            &checksum,
            signature,
            &node.uuid,
        )
        .map_err(|e| {
            NodeClientError::remote(
                status.as_u16(),
                format!("node response failed authentication: {e}"),
            )
        })?;
        let size_bytes = file.as_file().metadata()?.len();
        Ok(StreamedSnapshot {
            archive: file,
            size_bytes,
            checksum,
        })
    }

    /// Upload a snapshot archive to `node` as a raw streamed request body.
    /// The archive is read once to compute the SHA-256 the request signature
    /// binds, then streamed chunk-by-chunk — neither the archive nor its
    /// base64 ever materializes in RAM. A pre-streaming agent rejects the raw
    /// body with a 400 envelope BEFORE any side effect; the client then
    /// retries once with the legacy base64 JSON envelope and a fresh nonce.
    /// Otherwise POST semantics are unchanged: exactly one streaming attempt,
    /// no transport retry.
    pub async fn restore_snapshot_stream(
        &self,
        node: &Node,
        uuid: &str,
        archive: &std::path::Path,
    ) -> Result<bool> {
        if !node.enrolled {
            bail!("node is not enrolled")
        }
        if !node.enabled {
            bail!("node is disabled")
        }
        let sha = file_sha256_path(archive)?;
        let path = format!("/v1/servers/{uuid}/snapshot");
        let url = format!("{}{}", node.public_url.trim_end_matches('/'), path);
        let client = self.client_for(node)?;
        let signed = node_protocol::sign_body_hash(&node.secret, "POST", &path, &sha, &node.uuid)?;
        let nonce = signed.nonce.clone();
        let file = tokio::fs::File::open(archive).await?;
        let stream = tokio_util::io::ReaderStream::new(file)
            .map_ok(hyper::body::Frame::data)
            .map_err(|e: std::io::Error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) });
        let body = reqwest::Body::wrap(http_body_util::StreamBody::new(stream));
        let mut req = client
            .request(Method::POST, &url)
            .header(node_protocol::NODE_ID_HEADER, &signed.node_id)
            .header(
                node_protocol::TIMESTAMP_HEADER,
                signed.timestamp.to_string(),
            )
            .header(node_protocol::NONCE_HEADER, &signed.nonce)
            .header(node_protocol::SIGNATURE_HEADER, &signed.signature)
            .header(node_protocol::STREAM_HEADER, "1")
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body);
        if let Ok(rid) = crate::REQUEST_ID.try_with(|id| id.clone()) {
            req = req.header(REQUEST_ID_HEADER, rid);
        }
        let resp = req.send().await.map_err(|e| {
            NodeClientError::transport(format!("request to node '{}' failed: {e}", node.name))
        })?;
        let status = resp.status();
        if !resp.headers().contains_key(node_protocol::STREAM_HEADER) {
            // Pre-streaming agent: it parsed our raw body as JSON, rejected it
            // with a 400 before any side effect. Fall back to the legacy
            // base64 envelope (the archive is re-read from disk).
            tracing::info!(
                node_id = %node.uuid,
                %path,
                "node predates streaming restore; retrying with the legacy base64 envelope"
            );
            let bytes = std::fs::read(archive)?;
            let legacy = RestoreSnapshotRequest {
                archive_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                checksum: sha,
            };
            return self.restore_snapshot(node, uuid, &legacy).await;
        }
        // Streaming agent: authenticate + parse the envelope (success or error).
        let bytes = resp.bytes().await?;
        self.authenticate_response(node, "POST", &path, &nonce, status, &bytes)
            .await?;
        let envelope: NodeApiResponse<bool> = serde_json::from_slice(&bytes)
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
    /// Stop a remote server and wait until the daemon reports no PID, so the
    /// caller never snapshots a half-stopped server. A `stats` error counts
    /// as "still running" — only a clean `pid.is_none()` ends the wait — and
    /// the server still being alive after `attempts` polls is an error, not
    /// a silent success. Shared by the backup API and the scheduler so both
    /// stop paths cannot drift apart.
    pub async fn stop_and_wait(
        &self,
        node: &Node,
        uuid: &str,
        attempts: u32,
        poll: Duration,
    ) -> Result<()> {
        self.power(node, uuid, PowerAction::Stop).await?;
        wait_until_stopped(attempts, poll, || async {
            self.stats(node, uuid).await.map(|s| s.pid.is_none())
        })
        .await
    }
}

/// Result of a streamed snapshot download: the tar.gz archive as a temp
/// file, its byte size, and its SHA-256 — the same `checksum` the legacy
/// envelope carried, so callers can store either shape.
#[derive(Debug)]
pub struct StreamedSnapshot {
    pub archive: tempfile::NamedTempFile,
    pub size_bytes: u64,
    pub checksum: String,
}

/// SHA-256 hex of a file, read in bounded chunks.
fn file_sha256_path(path: &std::path::Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    file_sha256(&mut file)
}

fn file_sha256(file: &mut std::fs::File) -> Result<String> {
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Send `build()`'s request up to `attempts` times, backing off exponentially
/// between attempts. Only transport failures (connection refused/reset/timeout)
/// are retried; any HTTP response is returned as-is. The caller picks the
/// budget: interactive reads keep the full one, non-interactive reads get one
/// retry so a dead node cannot stall the reconcile loop, and writes are sent
/// exactly once.
///
/// `build` runs once per attempt and MUST mint a fresh (re-signed) request
/// each time: re-issuing the first attempt's identical (node, nonce) pair
/// after a lost response would be rejected by the node's replay cache even
/// though the GET is idempotent. The returned `String` is the nonce of the
/// attempt that produced the response, which the caller binds its verification
/// to. A `build` error (signing failure) is deterministic and propagates
/// immediately; only request transport errors are retried.
async fn send_with_retry<F>(attempts: u32, mut build: F) -> Result<(reqwest::Response, String)>
where
    F: FnMut() -> Result<(reqwest::RequestBuilder, String)>,
{
    for attempt in 0..attempts {
        let (req, nonce) = build()?;
        match req.send().await {
            Ok(resp) => return Ok((resp, nonce)),
            Err(_) if attempt + 1 < attempts => {
                tokio::time::sleep(Duration::from_millis(
                    REQUEST_BACKOFF_BASE_MS * 2u64.pow(attempt),
                ))
                .await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("loop returns on every attempt")
}

/// Poll `probe` up to `attempts` times; a probe error is treated as "not yet
/// stopped", so a transient failure can never be mistaken for a clean stop.
/// Returns an error once the attempt budget is exhausted.
async fn wait_until_stopped<F, Fut>(attempts: u32, poll: Duration, mut probe: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    for _ in 0..attempts {
        if probe().await.unwrap_or(false) {
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
    bail!("server did not report stopped within {attempts} polls")
}

/// Hard bound on live (node, nonce) pairs. The time-based prune alone is not
/// enough: within a single 180-second window a busy panel can accumulate
/// arbitrarily many pairs, so `use_once` compacts the cache once it exceeds
/// the cap. Compaction evicts the oldest entries (by timestamp), making the
/// eviction cost an occasional O(n log n) sweep instead of an O(n) scan on
/// every insert.
const NONCE_CACHE_CAP: usize = 100_000;
/// Entries retired per compaction: a quarter of the cap (never less than the
/// current overflow), so a compaction frees a large batch and the next one
/// only happens after that many new inserts.
const NONCE_CACHE_RETIRE_FRACTION: usize = 4;
#[derive(Clone)]
pub struct NonceCache {
    values: Arc<DashMap<(String, String), i64>>,
    cap: usize,
    /// Serializes compaction sweeps (voltd's NonceStore pattern): a sweep's
    /// snapshot + retain must never overlap another's, or a nonce accepted
    /// mid-sweep is evicted right after `use_once` returned true, reopening
    /// the replay window.
    compacting: Arc<AtomicBool>,
}

impl NonceCache {
    pub fn new() -> Self {
        Self {
            values: Arc::new(DashMap::new()),
            cap: NONCE_CACHE_CAP,
            compacting: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn with_cap(cap: usize) -> Self {
        Self {
            values: Arc::new(DashMap::new()),
            cap,
            compacting: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn use_once(&self, node_id: &str, nonce: &str, timestamp: i64) -> bool {
        self.prune(timestamp - node_protocol::MAX_CLOCK_SKEW_SECS * 2);
        let key = (node_id.to_string(), nonce.to_string());
        let fresh = self.values.insert(key.clone(), timestamp).is_none();
        // Compact only after the insert, and only when the cap is actually
        // exceeded: the new pair is then counted in the sweep, so the newest
        // entries stay while the oldest ones are retired.
        if self.values.len() > self.cap {
            self.compact();
        }
        // Re-assert the pair unconditionally once any sweep has finished. A
        // concurrent sweep may have snapshotted its keep-set before this
        // insert and then evicted the pair; it may also have shrunk the map
        // below the cap while this caller was between the insert and the
        // `len > cap` check, in which case `compact` above never ran and the
        // pair would otherwise stay evicted. The re-insert is idempotent when
        // no sweep raced (same key, same timestamp), so an accepted nonce
        // never loses its replay guard.
        self.values.insert(key, timestamp);
        fresh
    }

    fn prune(&self, before: i64) {
        self.values.retain(|_, ts| *ts >= before);
    }

    /// Evict the oldest entries (by timestamp) once the cache overflows the
    /// cap. A batch of at least the overflow, up to a quarter of the cap, is
    /// retired per sweep, so the bound stays hard while the cost amortizes.
    ///
    /// Only one sweep runs at a time. A contender waits for the in-flight
    /// sweep to finish before returning, so the caller's re-insert (see
    /// `use_once`) always lands after that sweep's retain pass.
    fn compact(&self) {
        if self
            .compacting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            while self.compacting.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            return;
        }
        let over = self.values.len().saturating_sub(self.cap);
        if over > 0 {
            let retire = (self.cap / NONCE_CACHE_RETIRE_FRACTION).max(over);
            let mut entries: Vec<((String, String), i64)> = self
                .values
                .iter()
                .map(|e| (e.key().clone(), *e.value()))
                .collect();
            entries.sort_unstable_by_key(|(_, ts)| *ts);
            let keep: std::collections::HashSet<_> = entries
                .into_iter()
                .skip(retire)
                .map(|(k, _)| k)
                .collect();
            self.values.retain(|k, _| keep.contains(k));
        }
        self.compacting.store(false, Ordering::Release);
    }
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_protocol::NodeCapacity;

    fn node(fp: &str) -> Node {
        Node {
            id: 0,
            uuid: format!("uuid-{fp}"),
            name: "test-node".into(),
            public_url: "http://127.0.0.1:1".into(),
            secret: "secret".into(),
            enrollment_token: None,
            enrolled: true,
            enabled: true,
            maintenance: false,
            schedulable: true,
            location: String::new(),
            tags: Vec::new(),
            memory_limit_mb: 0,
            disk_limit_mb: 0,
            cpu_limit_percent: 0,
            memory_overallocate: 0,
            disk_overallocate: 0,
            daemon_version: String::new(),
            hostname: String::new(),
            os: String::new(),
            arch: String::new(),
            capacity: NodeCapacity::default(),
            last_heartbeat: None,
            last_error: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            tls_fingerprint: fp.into(),
            expected_fingerprint: String::new(),
        }
    }

    fn hex_fp(seed: u64) -> String {
        format!("{:064x}", seed)
    }

    #[test]
    fn pinned_cache_is_bounded_under_parallel_fingerprints() {
        // A small cap exercises the eviction path without minting the real
        // `MAX_PINNED_CLIENTS` clients; the cap logic is identical.
        let client = NodeClient::with_limit(4, false).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let client = client.clone();
                std::thread::spawn(move || {
                    for seed in 0..16u64 {
                        let node = node(&hex_fp(seed));
                        assert!(client.client_for(&node).is_ok());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(
            client.pinned.lock().len() <= 4,
            "cache must never exceed its hard cap under parallel fingerprints"
        );
    }

    #[test]
    fn pinned_cache_reuses_existing_client() {
        let client = NodeClient::with_limit(4, false).unwrap();
        let n = node(&hex_fp(7));
        let c1 = client.client_for(&n).unwrap();
        let c2 = client.client_for(&n).unwrap();
        assert_eq!(
            client.pinned.lock().len(),
            1,
            "repeated lookups must reuse one cached entry"
        );
        let _ = (c1, c2);
    }

    #[test]
    fn pinned_cache_evicts_least_recently_used() {
        let client = NodeClient::with_limit(2, false).unwrap();
        client.client_for(&node(&hex_fp(1))).unwrap();
        client.client_for(&node(&hex_fp(2))).unwrap();
        client.client_for(&node(&hex_fp(3))).unwrap();
        {
            let cache = client.pinned.lock();
            assert_eq!(cache.len(), 2);
            assert!(
                !cache.clients.contains_key(&hex_fp(1)),
                "least recently used entry must be evicted"
            );
            assert!(cache.clients.contains_key(&hex_fp(2)));
            assert!(cache.clients.contains_key(&hex_fp(3)));
        }
        // Reusing the evicted fingerprint re-inserts it, evicting the next LRU.
        client.client_for(&node(&hex_fp(1))).unwrap();
        let cache = client.pinned.lock();
        assert_eq!(cache.len(), 2);
        assert!(cache.clients.contains_key(&hex_fp(1)));
        assert!(!cache.clients.contains_key(&hex_fp(2)));
    }

    #[tokio::test]
    async fn wait_stopped_times_out_when_stats_errors_never_count_as_stopped() {
        let err = wait_until_stopped(3, Duration::from_millis(1), || async {
            anyhow::bail!("stats transport error")
        })
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("did not report stopped"),
            "a probe error must not end the wait as a clean stop: {err}"
        );
    }

    #[tokio::test]
    async fn wait_stopped_returns_once_pid_clears() {
        let polls = std::sync::atomic::AtomicU32::new(0);
        wait_until_stopped(10, Duration::from_millis(1), || async {
            let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            Ok(n >= 3)
        })
        .await
        .unwrap();
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn wait_stopped_returns_immediately_when_already_stopped() {
        let polls = std::sync::atomic::AtomicU32::new(0);
        wait_until_stopped(10, Duration::from_millis(1), || async {
            polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        })
        .await
        .unwrap();
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn nonce_cache_caps_size_and_evicts_oldest() {
        let cache = NonceCache::with_cap(3);
        assert!(cache.use_once("node", "a", 1));
        assert!(cache.use_once("node", "b", 2));
        assert!(cache.use_once("node", "c", 3));
        // The fourth insert hits the cap and evicts the oldest pair ("a").
        assert!(cache.use_once("node", "d", 4));
        assert!(
            cache.use_once("node", "a", 1),
            "the oldest pair must be evicted once the cap is reached"
        );
        assert!(
            !cache.use_once("node", "b", 2),
            "younger pairs must survive the compaction"
        );
        assert_eq!(cache.values.len(), 3, "the cache must stay bounded");
    }

    /// Deterministic interleaving of the snapshot/retain race: a compaction
    /// snapshots its keep-set before a fresh pair lands, so its retain pass
    /// would evict the just-accepted nonce. `use_once` must wait out the
    /// sweep and re-assert the pair, or the replay window reopens.
    #[test]
    fn use_once_reasserts_pair_accepted_during_compaction() {
        let cache = NonceCache::with_cap(3);
        assert!(cache.use_once("n", "a", 1));
        assert!(cache.use_once("n", "b", 2));
        assert!(cache.use_once("n", "c", 3));

        // Hold the compaction gate, simulating a compactor whose snapshot
        // predates the pair we are about to accept.
        cache.compacting.store(true, Ordering::Release);

        // The fresh pair is accepted mid-sweep; use_once blocks on the gate.
        let inserter_cache = cache.clone();
        let inserter = std::thread::spawn(move || inserter_cache.use_once("n", "d", 4));
        while cache.values.len() != 4 {
            std::thread::yield_now();
        }
        // The in-flight sweep's stale retain runs now, evicting "d".
        cache
            .values
            .retain(|k, _| matches!(k.0.as_str(), "a" | "b" | "c"));
        cache.compacting.store(false, Ordering::Release);

        assert!(
            inserter.join().unwrap(),
            "the pair accepted mid-sweep is fresh"
        );
        assert!(
            cache.values.contains_key(&("n".to_string(), "d".to_string())),
            "a pair accepted mid-sweep must be re-asserted once the sweep ends"
        );
    }

    /// Hammer `use_once` + compaction from many threads. Every pair carries a
    /// strictly increasing timestamp, so the globally newest pair must never
    /// be evicted by any sweep it participates in — a mid-sweep insert that
    /// races a stale retain would drop it and fail this invariant.
    #[test]
    fn use_once_never_loses_fresh_pairs_under_parallel_compaction() {
        let cache = NonceCache::with_cap(64);
        let ts = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let ts = ts.clone();
                std::thread::spawn(move || {
                    for _ in 0..4000 {
                        let t = ts.fetch_add(1, Ordering::SeqCst);
                        cache.use_once(&format!("node-{t}"), &format!("nonce-{t}"), t);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // fetch_add returns the pre-increment value, so the newest inserted
        // pair carries `load() - 1`.
        let last = ts.load(Ordering::SeqCst) - 1;
        assert!(
            cache
                .values
                .contains_key(&(format!("node-{last}"), format!("nonce-{last}"))),
            "the most recently accepted pair must still be guarded"
        );
        assert!(
            cache.values.len() <= cache.cap + 8,
            "the cache must stay bounded under parallel compaction"
        );
    }

    /// A blocking TCP server that drops the first `drops` connections without
    /// responding (transport errors) and then serves `response` once. Returns
    /// the base URL and a connection counter.
    fn flaky_server(
        drops: usize,
        response: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = conns.clone();
        std::thread::spawn(move || {
            for i in 0..=drops {
                let (mut stream, _) = listener.accept().unwrap();
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                if i < drops {
                    drop(stream); // reset: a transport error for the client
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.len()
                    );
                    stream.write_all(head.as_bytes()).unwrap();
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        (format!("http://{addr}"), conns)
    }

    fn node_at(url: &str) -> Node {
        let mut n = node("");
        n.public_url = url.into();
        n
    }

    #[tokio::test]
    async fn get_retries_transport_failures_with_backoff() {
        // `health` is a non-interactive read: the split budget allows exactly
        // one retry, so a single dropped connection is survived and the
        // second attempt succeeds.
        let (url, conns) = flaky_server(1, r#"{"ok":true,"data":{"status":"ok"}}"#);
        let client = NodeClient::new().unwrap();
        let health = client.health(&node_at(&url)).await.unwrap();
        assert_eq!(health["status"], "ok");
        assert_eq!(
            conns.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a non-interactive read retries exactly once"
        );
    }

    #[tokio::test]
    async fn console_gets_the_full_retry_budget() {
        // The terminal console is interactive: it keeps the full three-attempt
        // budget, so two dropped connections are survived.
        let (url, conns) = flaky_server(
            2,
            r#"{"ok":true,"data":{"lines":["hi"],"cursor":3},"error":null}"#,
        );
        let client = NodeClient::new().unwrap();
        let snap = client
            .console(&node_at(&url), "u", 0)
            .await
            .unwrap();
        assert_eq!(snap.lines, vec!["hi"]);
        assert_eq!(
            conns.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "interactive console reads keep the full retry budget"
        );
    }

    #[tokio::test]
    async fn get_retry_mints_a_fresh_nonce_per_attempt() {
        // A retried GET must not replay the first attempt's (node, nonce)
        // pair: if attempt 1 reached the node but the response was lost, the
        // node's replay cache would reject the identical pair on the retry.
        // Record the nonce each connection sends and assert they differ.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let nonces = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen = nonces.clone();
        let body = r#"{"ok":true,"data":{"status":"ok"}}"#;
        std::thread::spawn(move || {
            for i in 0..=1 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap();
                let head = std::str::from_utf8(&buf[..n]).unwrap();
                let nonce = head
                    .lines()
                    .find_map(|l| l.strip_prefix("x-volt-nonce:"))
                    .map(|v| v.trim().to_string())
                    .expect("every request carries a nonce header");
                seen.lock().push(nonce);
                if i == 0 {
                    drop(stream); // transport error: triggers the retry
                } else {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(resp.as_bytes()).unwrap();
                }
            }
        });
        let url = format!("http://{addr}");
        let client = NodeClient::new().unwrap();
        let health = client.health(&node_at(&url)).await.unwrap();
        assert_eq!(health["status"], "ok");
        let got = nonces.lock();
        assert_eq!(got.len(), 2, "two attempts were made");
        assert_ne!(got[0], got[1], "each retry attempt must carry a fresh nonce");
    }

    #[tokio::test]
    async fn post_is_never_retried() {
        let (url, conns) = flaky_server(2, r#"{"ok":true,"data":{"action":"started"}}"#);
        let client = NodeClient::new().unwrap();
        // The first attempt hits a dropped connection; a POST must fail
        // immediately instead of re-issuing an ambiguous side effect.
        let err = client
            .power(
                &node_at(&url),
                "u",
                crate::node_protocol::PowerAction::Start,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("request to node"));
        assert_eq!(
            conns.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a POST must be sent exactly once"
        );
    }

    #[tokio::test]
    async fn unsigned_response_rejected_when_require_signed_enabled() {
        // `flaky_server` never signs its body, so the response is Unsigned.
        let (url, _conns) = flaky_server(0, r#"{"ok":true,"data":{"status":"ok"}}"#);
        let client = NodeClient::with_limit(MAX_PINNED_CLIENTS, true).unwrap();
        let err = client.health(&node_at(&url)).await.unwrap_err();
        assert!(
            err.to_string().contains("require_signed_node_responses"),
            "the rejection must name the config flag: {err}"
        );
    }

    #[tokio::test]
    async fn unsigned_response_accepted_by_default() {
        // Control: with the flag off the legacy accept-with-warning path is
        // unchanged and the response is trusted.
        let (url, _conns) = flaky_server(0, r#"{"ok":true,"data":{"status":"ok"}}"#);
        let client = NodeClient::with_limit(MAX_PINNED_CLIENTS, false).unwrap();
        let health = client.health(&node_at(&url)).await.unwrap();
        assert_eq!(health["status"], "ok");
    }

    /// Small tar.gz fixture: one file "hello.txt" containing "world".
    fn test_archive() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(&mut header, "hello.txt", &b"world"[..])
                .unwrap();
            let encoder = tar.into_inner().unwrap();
            encoder.finish().unwrap();
        }
        out
    }

    /// Serve one request: parse the signed request headers, reply with the
    /// streaming snapshot body (archive + footer signed with the request's
    /// nonce) or the legacy envelope, per `streaming`.
    fn snapshot_server(
        signed_archive: Vec<u8>,
        served_archive: Vec<u8>,
        streaming: bool,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = served.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let head = std::str::from_utf8(&buf[..n]).unwrap();
            let nonce = head
                .lines()
                .find_map(|l| l.strip_prefix("x-volt-nonce:"))
                .map(|v| v.trim().to_string())
                .expect("request nonce");
            assert!(
                head.to_ascii_lowercase().contains("x-volt-stream: 1"),
                "the client must advertise streaming"
            );
            let body = if streaming {
                let sha = crate::node_protocol::body_hash(&signed_archive);
                let sig = crate::node_protocol::sign_stream_response(
                    "secret",
                    "GET",
                    "/v1/servers/uuid-0/snapshot",
                    200,
                    &nonce,
                    &sha,
                    "uuid-",
                )
                .unwrap();
                let mut body = served_archive.clone();
                body.extend_from_slice(&crate::node_protocol::stream_footer(&sig));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\nx-volt-stream: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                body
            } else {
                let envelope = NodeApiResponse::success(crate::node_protocol::SnapshotResponse {
                    archive_b64: base64::engine::general_purpose::STANDARD.encode(&signed_archive),
                    size_bytes: signed_archive.len() as u64,
                    checksum: crate::node_protocol::body_hash(&signed_archive),
                });
                let body = serde_json::to_vec(&envelope).unwrap();
                let s = std::str::from_utf8(&body).unwrap();
                let sig = crate::node_protocol::sign_response(
                    "secret",
                    "GET",
                    "/v1/servers/uuid-0/snapshot",
                    200,
                    &nonce,
                    &body,
                    "uuid-",
                )
                .unwrap();
                let wire =
                    format!("{},\"signature\":\"{}\",\"nonce\":\"{}\"}}", &s[..s.len() - 1], sig, nonce);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    wire.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                wire.into_bytes()
            };
            stream.write_all(&body).unwrap();
        });
        (format!("http://{addr}"), served)
    }

    #[tokio::test]
    async fn snapshot_stream_downloads_and_verifies_streaming_body() {
        let archive = test_archive();
        let (url, served) = snapshot_server(archive.clone(), archive.clone(), true);
        let client = NodeClient::new().unwrap();
        let mut n = node("");
        n.public_url = url;
        let snap = client.snapshot_stream(&n, "uuid-0").await.unwrap();
        assert_eq!(snap.size_bytes, archive.len() as u64);
        assert_eq!(snap.checksum, crate::node_protocol::body_hash(&archive));
        assert_eq!(std::fs::read(snap.archive.path()).unwrap(), archive);
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn snapshot_stream_degrades_to_legacy_envelope() {
        // Old agent, no streaming header on the response: the client must
        // fall back to the base64 envelope and return the same shape.
        let archive = test_archive();
        let (url, _served) = snapshot_server(archive.clone(), archive.clone(), false);
        let client = NodeClient::new().unwrap();
        let mut n = node("");
        n.public_url = url;
        let snap = client.snapshot_stream(&n, "uuid-0").await.unwrap();
        assert_eq!(snap.size_bytes, archive.len() as u64);
        assert_eq!(snap.checksum, crate::node_protocol::body_hash(&archive));
        assert_eq!(std::fs::read(snap.archive.path()).unwrap(), archive);
    }

    #[tokio::test]
    async fn snapshot_stream_rejects_tampered_streaming_body() {
        // A flipped byte in the archive must fail the tail signature.
        let mut archive = test_archive();
        let mid = archive.len() / 2;
        archive[mid] ^= 0x01;
        let (url, _served) = snapshot_server(test_archive(), archive, true);
        let mut n = node("");
        n.public_url = url;
        let client = NodeClient::new().unwrap();
        let err = client.snapshot_stream(&n, "uuid-0").await.unwrap_err();
        assert!(
            err.to_string().contains("failed authentication"),
            "unexpected error: {err}"
        );
    }

    /// Serve one request like a streaming agent failing before the stream:
    /// a signed JSON error envelope (400) that still echoes `x-volt-stream`
    /// — the real daemon's `sign_responses` middleware echoes the capability
    /// header on every response to a streaming request, including error
    /// envelopes. The client must route on content-type and surface the
    /// envelope's error instead of misreading it as a truncated stream.
    fn error_envelope_with_stream_header_server() -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let head = std::str::from_utf8(&buf[..n]).unwrap();
            let nonce = head
                .lines()
                .find_map(|l| l.strip_prefix("x-volt-nonce:"))
                .map(|v| v.trim().to_string())
                .expect("request nonce");
            let envelope = NodeApiResponse::<crate::node_protocol::SnapshotResponse>::failure(
                "server must be stopped before snapshot",
            );
            let bytes = serde_json::to_vec(&envelope).unwrap();
            let s = std::str::from_utf8(&bytes).unwrap();
            let sig = crate::node_protocol::sign_response(
                "secret",
                "GET",
                "/v1/servers/uuid-0/snapshot",
                400,
                &nonce,
                &bytes,
                "uuid-",
            )
            .unwrap();
            let wire =
                format!("{},\"signature\":\"{}\",\"nonce\":\"{}\"}}", &s[..s.len() - 1], sig, nonce);
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nx-volt-stream: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                wire.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.write_all(wire.as_bytes()).unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn snapshot_stream_json_error_envelope_wins_over_echoed_header() {
        // Regression: the daemon echoes `x-volt-stream` on JSON error
        // envelopes too; routing on the header alone used to funnel the
        // envelope into the footer parser ("malformed streaming footer").
        // Content-type discrimination must surface the real error instead.
        let url = error_envelope_with_stream_header_server();
        let client = NodeClient::new().unwrap();
        let mut n = node("");
        n.public_url = url;
        let err = client.snapshot_stream(&n, "uuid-0").await.unwrap_err();
        assert!(
            err.to_string().contains("server must be stopped before snapshot"),
            "real error was masked: {err}"
        );
        assert!(
            !err.to_string().contains("streaming"),
            "error should not mention the stream: {err}"
        );
    }

    /// Serve restore requests like a node: `streaming: true` replies with the
    /// STREAM header + a signed success envelope (a streaming agent); `false`
    /// replies without it (a pre-streaming agent), serving two connections —
    /// the streaming attempt and the legacy fallback. The request body is
    /// drained before replying, exactly like the real agent's restore path.
    /// Returns the base URL and counters for connections seen and how many
    /// of them advertised streaming.
    fn restore_server(
        streaming: bool,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let total = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streamed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let total_seen = total.clone();
        let streamed_seen = streamed.clone();
        std::thread::spawn(move || {
            // A pre-streaming agent gets two requests (streaming attempt +
            // legacy fallback); a streaming agent gets one.
            let conns = if streaming { 1 } else { 2 };
            for i in 0..conns {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let mut head_end = None;
                let mut have = 0;
                // The request body is a raw binary archive, so only the bytes
                // up to the header terminator are guaranteed UTF-8; a single
                // read may also return body bytes after the headers.
                while head_end.is_none() {
                    have += stream.read(&mut buf[have..]).unwrap();
                    head_end = buf[..have]
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4);
                    assert!(have < buf.len(), "request headers too large");
                }
                let head_end = head_end.unwrap();
                let head = std::str::from_utf8(&buf[..head_end]).unwrap();
                let nonce = head
                    .lines()
                    .find_map(|l| l.strip_prefix("x-volt-nonce:"))
                    .map(|v| v.trim().to_string())
                    .expect("request nonce");
                total_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if head.to_ascii_lowercase().contains("x-volt-stream: 1") {
                    streamed_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                // Drain the request body before replying — the real agent
                // consumes the stream before responding, and a reply sent
                // mid-upload is a connection error for the client.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut rest = [0u8; 4096];
                loop {
                    if stream.read(&mut rest).unwrap_or(0) == 0 {
                        break;
                    }
                }
                // Signed success envelope echoing the request nonce; the
                // streaming agent additionally echoes the STREAM header.
                let envelope = NodeApiResponse::<bool>::success(true);
                let bytes = serde_json::to_vec(&envelope).unwrap();
                let s = std::str::from_utf8(&bytes).unwrap();
                let sig = crate::node_protocol::sign_response(
                    "secret",
                    "POST",
                    "/v1/servers/uuid-0/snapshot",
                    200,
                    &nonce,
                    &bytes,
                    "uuid-",
                )
                .unwrap();
                let wire = format!(
                    "{},\"signature\":\"{}\",\"nonce\":\"{}\"}}",
                    &s[..s.len() - 1],
                    sig,
                    nonce
                );
                let stream_header = if streaming && i == 0 {
                    "x-volt-stream: 1\r\n"
                } else {
                    ""
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{stream_header}content-length: {}\r\nconnection: close\r\n\r\n",
                    wire.len()
                );
                stream.write_all(resp.as_bytes()).unwrap();
                stream.write_all(wire.as_bytes()).unwrap();
            }
        });
        (format!("http://{addr}"), total, streamed)
    }

    #[tokio::test]
    async fn restore_stream_uploads_raw_body_and_accepts() {
        let archive = test_archive();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive).unwrap();
        let (url, total, streamed) = restore_server(true);
        let client = NodeClient::new().unwrap();
        let mut n = node("");
        n.public_url = url;
        let ok = client
            .restore_snapshot_stream(&n, "uuid-0", tmp.path())
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(total.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            streamed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the upload must advertise streaming"
        );
    }

    #[tokio::test]
    async fn restore_stream_falls_back_to_legacy_envelope() {
        // A pre-streaming agent ignores the raw body and answers with the
        // legacy envelope path: the client retries once with base64 JSON.
        let archive = test_archive();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &archive).unwrap();
        let (url, total, streamed) = restore_server(false);
        let client = NodeClient::new().unwrap();
        let mut n = node("");
        n.public_url = url;
        let ok = client
            .restore_snapshot_stream(&n, "uuid-0", tmp.path())
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(
            total.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the raw attempt plus the legacy fallback"
        );
        assert_eq!(
            streamed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly the first attempt advertised streaming"
        );
    }
}