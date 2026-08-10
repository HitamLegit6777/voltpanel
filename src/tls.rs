//! Self-signed TLS material and certificate pinning shared by the panel and `voltd`.
//!
//! VoltPanel deployments frequently have no domain name: nodes are reached by raw
//! IP on a private network, so public CA issuance is impossible. Instead every
//! endpoint generates a long-lived self-signed certificate on first boot and the
//! peer pins its SHA-256 fingerprint, exchanged over the already authenticated
//! enrollment channel. That yields real transport encryption plus an identity
//! check that does not depend on the WebPKI.
use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

/// Process-wide crypto provider. `rustls` is built without default features, so the
/// provider must be handed to every builder explicitly.
static PROVIDER: LazyLock<Arc<CryptoProvider>> =
    LazyLock::new(|| Arc::new(rustls::crypto::ring::default_provider()));

pub fn provider() -> Arc<CryptoProvider> {
    PROVIDER.clone()
}

/// PEM-encoded certificate/key pair plus the fingerprint peers pin.
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
}

/// Lowercase hex SHA-256 of a DER certificate — the pinning identity.
pub fn fingerprint_der(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Fingerprint of the leaf certificate in a PEM bundle.
pub fn fingerprint_pem(cert_pem: &str) -> Result<String> {
    let leaf = parse_certs(cert_pem)?
        .into_iter()
        .next()
        .context("certificate PEM contains no certificate")?;
    Ok(fingerprint_der(leaf.as_ref()))
}

/// Normalize user-supplied fingerprints: drop colons/whitespace, lowercase.
pub fn normalize_fingerprint(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn parse_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        bail!("no PEM certificate found");
    }
    Ok(certs)
}

fn parse_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("no PEM private key found")
}

/// True when `key_pem` pairs with the leaf of `cert_pem` — i.e. the stored
/// `ServerConfig` can actually be built. A key that does not match its
/// certificate makes every handshake fail, so `ensure_material` must replace
/// the pair instead of serving a broken config.
fn key_matches_cert(cert_pem: &str, key_pem: &str) -> bool {
    let Ok(certs) = parse_certs(cert_pem) else {
        return false;
    };
    let Ok(key) = parse_key(key_pem) else {
        return false;
    };
    rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .ok()
        .and_then(|b| b.with_no_client_auth().with_single_cert(certs, key).ok())
        .is_some()
}

/// Local IPv4/IPv6 addresses read from the kernel via `getifaddrs`.
fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with an owned list freed below via `freeifaddrs`.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` walks a kernel-allocated list terminated by null.
        let entry = unsafe { &*cur };
        if !entry.ifa_addr.is_null() {
            // SAFETY: `ifa_addr` points at a sockaddr whose family tag we read first.
            let family = unsafe { (*entry.ifa_addr).sa_family } as i32;
            if family == libc::AF_INET {
                // SAFETY: the family tag guarantees the sockaddr_in layout.
                let sa = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                let ip = std::net::Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr));
                if !ip.is_loopback() {
                    out.push(ip.to_string());
                }
            } else if family == libc::AF_INET6 {
                // SAFETY: the family tag guarantees the sockaddr_in6 layout.
                let sa = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                let ip = std::net::Ipv6Addr::from(sa.sin6_addr.s6_addr);
                if !ip.is_loopback() && !ip.to_string().starts_with("fe80") {
                    out.push(ip.to_string());
                }
            }
        }
        cur = entry.ifa_next;
    }
    // SAFETY: `head` came from a successful `getifaddrs` and is freed exactly once.
    unsafe { libc::freeifaddrs(head) };
    out
}

/// SANs to embed for a listener: hostname, loopback, and every non-loopback local
/// address, so one certificate works however the peer dials in.
pub fn default_sans(extra: &[String]) -> Vec<String> {
    let mut sans: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Ok(h) = hostname::get() {
        let h = h.to_string_lossy().to_string();
        if !h.is_empty() {
            sans.push(h);
        }
    }
    sans.extend(local_addresses());
    for e in extra {
        let e = e.trim();
        if !e.is_empty() {
            sans.push(e.to_string());
        }
    }
    sans.sort();
    sans.dedup();
    sans
}

/// True when the stored leaf already carries every SAN we would embed today.
///
/// The SAN extension is parsed structurally (via `rcgen`'s x509-parser-backed
/// `CertificateParams::from_ca_cert_der`), so only names actually present in the
/// extension count — raw byte matches anywhere in the DER could alias an IP or
/// DNS string that is really a key blob or signature. DNS names match exactly;
/// IPs match their raw address encoding. A certificate we cannot parse counts
/// as not covering, prompting regeneration.
fn covers_sans(cert_pem: &str, sans: &[String]) -> bool {
    let Ok(certs) = parse_certs(cert_pem) else {
        return false;
    };
    let Some(leaf) = certs.first() else {
        return false;
    };
    let Ok(params) = rcgen::CertificateParams::from_ca_cert_der(leaf) else {
        return false;
    };
    sans.iter().all(|san| {
        if let Ok(ip) = san.parse::<std::net::IpAddr>() {
            params
                .subject_alt_names
                .iter()
                .any(|s| matches!(s, rcgen::SanType::IpAddress(a) if *a == ip))
        } else {
            params
                .subject_alt_names
                .iter()
                .any(|s| matches!(s, rcgen::SanType::DnsName(d) if d.as_str() == san))
        }
    })
}

/// Load `dir/cert.pem` + `dir/key.pem`, generating a self-signed pair when absent.
///
/// Regenerates when the stored certificate lacks a SAN we now need (e.g. the
/// machine gained a public IP after first boot) or when the key file no longer
/// pairs with the certificate — a mismatched pair would fail every handshake.
///
/// Regeneration is crash-safe: the new pair is written to fsynced temp files in
/// the same directory, validated, and then renamed into place. A crash at any
/// point leaves either the old pair, a new-but-unvalidated pair that the next
/// call detects and replaces, or (after a crash between the two renames) a
/// mismatched pair that the validation gate refuses to serve and repairs on the
/// next call — never a half-written file served to a caller. Concurrent callers
/// in this process are serialized by [`MATERIAL_LOCK`].
pub fn ensure_material(dir: &Path, sans: &[String]) -> Result<TlsMaterial> {
    use std::os::unix::fs::PermissionsExt;
    // Serialize readers and writers of the pair. The critical section is short
    // and synchronous, so a parking_lot mutex is sufficient and keeps the lock
    // process-local (the panel and `voltd` write disjoint TLS directories).
    let _guard = MATERIAL_LOCK.lock();
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        match fingerprint_pem(&cert_pem) {
            Ok(fingerprint)
                if covers_sans(&cert_pem, sans) && key_matches_cert(&cert_pem, &key_pem) =>
            {
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
                return Ok(TlsMaterial {
                    cert_pem,
                    key_pem,
                    fingerprint,
                });
            }
            Ok(_) => tracing::info!(
                "regenerating TLS certificate: stored pair stale or cert/key mismatch"
            ),
            Err(e) => tracing::warn!("stored certificate unreadable ({e}), regenerating"),
        }
    }

    let generated = rcgen::generate_simple_self_signed(sans.to_vec())
        .context("failed to generate self-signed certificate")?;
    let cert_pem = generated.cert.pem();
    let key_pem = generated.key_pair.serialize_pem();
    let fingerprint = fingerprint_der(generated.cert.der());

    // Recover from a process that died between staging and rename, then stage
    // both files fresh. Nothing is renamed until the staged pair validates.
    cleanup_stale_stage(dir);
    let seq = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let stage_cert = stage_file(
        dir,
        &format!("cert.pem.tmp.{pid}.{seq}"),
        cert_pem.as_bytes(),
        0o644,
    )?;
    let stage_key = stage_file(
        dir,
        &format!("key.pem.tmp.{pid}.{seq}"),
        key_pem.as_bytes(),
        0o600,
    )?;

    let staged_cert = std::fs::read_to_string(&stage_cert)?;
    let staged_key = std::fs::read_to_string(&stage_key)?;
    if fingerprint_pem(&staged_cert).is_err()
        || !key_matches_cert(&staged_cert, &staged_key)
        || !covers_sans(&staged_cert, sans)
    {
        let _ = std::fs::remove_file(&stage_cert);
        let _ = std::fs::remove_file(&stage_key);
        bail!("staged TLS material failed validation; nothing was published");
    }

    // Publish cert first, then key. Both renames are atomic on the same
    // filesystem; a crash between them leaves a mismatched pair that the next
    // `ensure_material` call refuses to serve and atomically replaces.
    std::fs::rename(&stage_cert, &cert_path)?;
    std::fs::rename(&stage_key, &key_path)?;

    // Persist the renames so a crash cannot resurrect an older pair. Directory
    // fsync is best-effort: it is unsupported on a few filesystems.
    if let Ok(d) = std::fs::File::open(dir) {
        if let Err(e) = d.sync_all() {
            tracing::warn!("could not fsync TLS directory: {e}");
        }
    }

    tracing::info!("generated self-signed certificate, fingerprint {fingerprint}");
    Ok(TlsMaterial {
        cert_pem,
        key_pem,
        fingerprint,
    })
}

/// Process-local lock serializing regeneration and reads of a TLS pair so
/// concurrent callers never interleave staged writes.
static MATERIAL_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// Unique suffix for staged files so retries inside one process never collide.
static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Remove leftover `*.pem.tmp.*` stage files, e.g. from a process killed
/// between staging and rename. Best-effort: a leftover temp is never served,
/// and a failed removal only leaves garbage for the next regeneration to try.
fn cleanup_stale_stage(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("cert.pem.tmp.") || name.starts_with("key.pem.tmp.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `contents` to a brand-new `dir/name` with `mode`, fsync the file, and
/// return its path. `create_new` refuses to clobber an existing stage file, so
/// a stale file with the same name is never silently reused.
fn stage_file(dir: &Path, name: &str, contents: &[u8], mode: u32) -> Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&path)
        .with_context(|| format!("create stage file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write stage file {}", path.display()))?;
    // Pin the mode explicitly: `mode` above is subject to the process umask.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    file.sync_all()
        .with_context(|| format!("fsync stage file {}", path.display()))?;
    Ok(path)
}

/// TLS server configuration for a listener, ALPN-ready for HTTP/2 and HTTP/1.1.
pub fn server_config(material: &TlsMaterial) -> Result<Arc<rustls::ServerConfig>> {
    let certs = parse_certs(&material.cert_pem)?;
    let key = parse_key(&material.key_pem)?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and private key do not match")?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// Verifier that accepts exactly one leaf certificate, identified by fingerprint.
///
/// Chain, name and expiry checks are deliberately bypassed: a pinned self-signed
/// certificate has no issuer to validate against and is usually addressed by raw IP.
/// Handshake signature verification still runs through the crypto provider, so a
/// man-in-the-middle cannot replay the pinned certificate without its private key.
#[derive(Debug)]
struct PinnedVerifier {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint_der(end_entity.as_ref());
        if seen == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint mismatch: expected {}, got {seen}",
                self.fingerprint
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Client configuration that trusts exactly the certificate with `fingerprint`.
pub fn pinned_client_config(fingerprint: &str) -> Result<Arc<rustls::ClientConfig>> {
    let fingerprint = normalize_fingerprint(fingerprint);
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected a 64-character hex SHA-256 fingerprint (32 bytes), got {fingerprint:?}");
    }
    let provider = provider();
    let cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            fingerprint,
            provider,
        }))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Upper bound for one TLS handshake. A client that connects but never speaks
/// (half-open) must not pin a connection task forever; each such task also
/// holds a connection permit, so an unbounded handshake would exhaust the pool.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Maximum number of TLS connection tasks accepted concurrently. Connections
/// beyond this are rejected (the socket is closed) instead of spawning an
/// unbounded number of detached tasks.
const MAX_CONNECTIONS: usize = 1024;

/// Bounded period after shutdown is signalled during which in-flight
/// connections may wind down gracefully. Anything still running afterwards is
/// aborted and joined, so shutdown never waits forever.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Serve an axum router over TLS until `shutdown` resolves.
///
/// `axum::serve` has no TLS entry point, so the accept loop is spelled out here:
/// each connection is handshaked and driven on its own task, and `ConnectInfo` is
/// injected manually because the router never sees the raw `TcpStream`.
///
/// Concurrency is bounded by [`MAX_CONNECTIONS`] semaphore permits; excess
/// connections are rejected rather than queued, so the accept loop never
/// accumulates detached tasks. On shutdown the accept loop stops, every
/// connection task is signalled to close its socket (so idle keep-alive and
/// upgraded connections cannot stall shutdown), and after [`SHUTDOWN_GRACE`]
/// any stragglers are aborted and joined.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    config: Arc<rustls::ServerConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    serve_tls_inner(
        listener,
        router,
        config,
        HANDSHAKE_TIMEOUT,
        MAX_CONNECTIONS,
        SHUTDOWN_GRACE,
        shutdown,
    )
    .await
}

/// Implementation of [`serve_tls`] with injectable limits so tests can exercise
/// the half-open, idle keep-alive, and task-limit paths without waiting out the
/// production-scale values.
async fn serve_tls_inner(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    config: Arc<rustls::ServerConfig>,
    handshake_timeout: std::time::Duration,
    max_connections: usize,
    shutdown_grace: std::time::Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    use tower::Service as _;

    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let make = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    // One permit per in-flight connection: bounded tasks with explicit
    // rejection at the limit. `try_acquire_owned` never blocks the accept loop,
    // so shutdown stays responsive even while the listener is saturated.
    let permits = Arc::new(tokio::sync::Semaphore::new(max_connections));
    // Broadcast to every connection task so shutdown closes their sockets
    // immediately instead of waiting out idle keep-alive or upgraded conns.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let mut tasks = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                // A single failed accept (fd exhaustion, peer RST) must not take the
                // listener down; yield so the loop cannot spin hot on a sticky error.
                Err(e) => {
                    tracing::warn!("tls accept failed: {e}");
                    tokio::task::yield_now().await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };

        let Ok(permit) = permits.clone().try_acquire_owned() else {
            // At the connection limit: close the socket. The peer sees an
            // immediate EOF/reset instead of a half-open connection, and no
            // task is spawned past the bound.
            tracing::debug!("tls connection limit ({max_connections}) reached, rejecting {peer}");
            continue;
        };

        let acceptor = acceptor.clone();
        let mut make = make.clone();
        let mut shutdown_rx = shutdown_rx.clone();
        tasks.spawn(async move {
            // Held for the task's whole lifetime: the connection is counted
            // from accept until its socket closes.
            let _permit = permit;
            let tls = tokio::select! {
                r = tokio::time::timeout(handshake_timeout, acceptor.accept(stream)) => match r {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        tracing::debug!("tls handshake with {peer} failed: {e}");
                        return;
                    }
                    Err(_) => {
                        tracing::debug!("tls handshake with {peer} timed out after {handshake_timeout:?}");
                        return;
                    }
                },
                // Shutdown while still handshaking: leave immediately.
                _ = shutdown_rx.changed() => return,
            };
            // Infallible: the make-service only clones the router and records `peer`.
            let Ok(svc) = make.call(peer).await;
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let conn = builder.serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), svc);
            tokio::pin!(conn);
            tokio::select! {
                r = &mut conn => {
                    if let Err(e) = r {
                        tracing::debug!("connection from {peer} ended: {e}");
                    }
                }
                // Shutdown while idle (keep-alive) or upgraded: drop the
                // connection future, which closes the socket.
                _ = shutdown_rx.changed() => {}
            }
        });
    }

    // Stop accepting and tell every connection task to close its socket.
    let _ = shutdown_tx.send(true);
    drop(shutdown_tx);

    // Graceful drain: let tasks wind down for up to `shutdown_grace`, then
    // abort the stragglers and join everything so no task is leaked.
    let grace = tokio::time::sleep(shutdown_grace);
    tokio::pin!(grace);
    loop {
        tokio::select! {
            done = tasks.join_next() => {
                if done.is_none() {
                    break;
                }
            }
            _ = &mut grace => break,
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(dir: &Path) -> TlsMaterial {
        ensure_material(dir, &default_sans(&[])).unwrap()
    }

    #[test]
    fn material_is_stable_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let a = material(dir.path());
        let b = material(dir.path());
        assert_eq!(a.fingerprint, b.fingerprint, "certificate must be reused");
        assert_eq!(a.fingerprint.len(), 64);
        assert_eq!(a.fingerprint, fingerprint_pem(&a.cert_pem).unwrap());
    }

    #[test]
    fn material_regenerates_when_san_missing() {
        let dir = tempfile::tempdir().unwrap();
        let a = material(dir.path());
        let b = ensure_material(dir.path(), &["panel.internal".to_string()]).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert!(covers_sans(&b.cert_pem, &["panel.internal".to_string()]));
    }

    #[test]
    fn covers_sans_rejects_substring_byte_matches() {
        // The old DER scan matched any byte substring, so a DNS name like
        // "ocalhos" found inside the ASCII "localhost" SAN counted as covered.
        // SAN parsing must only honor names that are actually SAN entries.
        let dir = tempfile::tempdir().unwrap();
        let m = material(dir.path());
        assert!(covers_sans(&m.cert_pem, &["localhost".to_string()]));
        assert!(!covers_sans(&m.cert_pem, &["ocalhos".to_string()]));
        assert!(!covers_sans(&m.cert_pem, &["ocal".to_string()]));
    }

    #[test]
    fn covers_sans_distinguishes_ip_and_dns_names() {
        // An IP SAN must match only the exact address; a same-spelled DNS name
        // or a neighbor address must not count, and vice versa.
        let generated = rcgen::generate_simple_self_signed(vec![
            "192.0.2.7".to_string(),
            "node-a.internal".to_string(),
        ])
        .unwrap();
        let cert_pem = generated.cert.pem();
        assert!(covers_sans(&cert_pem, &["192.0.2.7".to_string()]));
        assert!(covers_sans(&cert_pem, &["node-a.internal".to_string()]));
        assert!(!covers_sans(&cert_pem, &["192.0.2.8".to_string()]));
        assert!(!covers_sans(&cert_pem, &["192.0.2.7.internal".to_string()]));
        assert!(!covers_sans(&cert_pem, &["node-a".to_string()]));
    }

    #[test]
    fn material_regenerates_when_key_does_not_match_cert() {
        let dir = tempfile::tempdir().unwrap();
        let a = material(dir.path());
        // Overwrite the key with an unrelated, freshly generated one.
        let other = rcgen::generate_simple_self_signed(vec!["other.internal".to_string()]).unwrap();
        std::fs::write(dir.path().join("key.pem"), other.key_pair.serialize_pem()).unwrap();
        let b = ensure_material(dir.path(), &default_sans(&[])).unwrap();
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "a mismatched key must trigger regeneration"
        );
        assert!(key_matches_cert(&b.cert_pem, &b.key_pem));
    }

    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        material(dir.path());
        let mode = std::fs::metadata(dir.path().join("key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn fingerprints_normalize() {
        assert_eq!(normalize_fingerprint("AB:cd ef"), "abcdef");
    }

    #[test]
    fn pinned_config_rejects_malformed_fingerprint() {
        assert!(pinned_client_config("deadbeef").is_err());
        // 64 characters of the right length but not hex — must still be rejected.
        assert!(pinned_client_config(&"g".repeat(64)).is_err());
        assert!(pinned_client_config(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn server_config_builds_from_material() {
        let dir = tempfile::tempdir().unwrap();
        assert!(server_config(&material(dir.path())).is_ok());
    }

    #[test]
    fn material_concurrent_calls_share_one_pair() {
        use std::collections::HashSet;
        let dir = tempfile::tempdir().unwrap();
        let sans = default_sans(&[]);
        // Race several threads through `ensure_material`; the process-local
        // lock must serialize them so they all serve the exact same pair.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dir = dir.path().to_path_buf();
                let sans = sans.clone();
                std::thread::spawn(move || ensure_material(&dir, &sans).unwrap().fingerprint)
            })
            .collect();
        let fingerprints: HashSet<String> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            fingerprints.len(),
            1,
            "all concurrent callers must agree on one certificate pair"
        );

        // The on-disk pair must be exactly the served material, and valid.
        let m = ensure_material(dir.path(), &sans).unwrap();
        let cert = std::fs::read_to_string(dir.path().join("cert.pem")).unwrap();
        let key = std::fs::read_to_string(dir.path().join("key.pem")).unwrap();
        assert_eq!(cert, m.cert_pem);
        assert_eq!(key, m.key_pem);
        assert!(key_matches_cert(&cert, &key));
    }

    #[test]
    fn material_regenerates_when_files_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let a = material(dir.path());
        // Corrupt the certificate while the key stays untouched.
        std::fs::write(dir.path().join("cert.pem"), "definitely not PEM").unwrap();
        let b = ensure_material(dir.path(), &default_sans(&[])).unwrap();
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "a malformed certificate must trigger regeneration"
        );
        assert!(key_matches_cert(&b.cert_pem, &b.key_pem));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cert.pem")).unwrap(),
            b.cert_pem,
            "the repaired pair must be what was published"
        );
    }

    #[test]
    fn material_cleans_up_stale_stage_files() {
        let dir = tempfile::tempdir().unwrap();
        // Leftovers from a process that died between staging and rename must
        // never break (or be served by) the next regeneration.
        std::fs::write(dir.path().join("cert.pem.tmp.4242.7"), "garbage").unwrap();
        std::fs::write(dir.path().join("key.pem.tmp.4242.7"), "garbage").unwrap();
        let m = material(dir.path());
        assert!(key_matches_cert(&m.cert_pem, &m.key_pem));
        assert!(
            !dir.path().join("cert.pem.tmp.4242.7").exists(),
            "stale cert stage file must be removed"
        );
        assert!(
            !dir.path().join("key.pem.tmp.4242.7").exists(),
            "stale key stage file must be removed"
        );
    }

    /// Complete a real TLS handshake against the test server using the pinned
    /// client config, then keep the connection open (idle keep-alive).
    async fn tls_connect(
        addr: std::net::SocketAddr,
        fingerprint: &str,
    ) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
        let cfg = pinned_client_config(fingerprint).unwrap();
        let connector = tokio_rustls::TlsConnector::from(cfg);
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        connector.connect(name, tcp).await.unwrap()
    }

    #[tokio::test]
    async fn half_open_handshake_does_not_block_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = server_config(&material(dir.path())).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_tls_inner(
            listener,
            axum::Router::new(),
            cfg,
            std::time::Duration::from_millis(500),
            32,
            std::time::Duration::from_secs(30),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Connect but never send a TLS ClientHello — a half-open handshake.
        let _half_open = tokio::net::TcpStream::connect(addr).await.unwrap();
        // Let the accept loop pick the connection up before signalling shutdown.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let deadline = std::time::Duration::from_secs(5);
        let _ = shutdown_tx.send(());
        let start = std::time::Instant::now();
        tokio::time::timeout(deadline, server)
            .await
            .expect("server must shut down despite the half-open connection")
            .unwrap()
            .unwrap();
        assert!(start.elapsed() < deadline, "shutdown must be finite");
    }

    #[tokio::test]
    async fn idle_keepalive_connection_does_not_block_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let m = material(dir.path());
        let cfg = server_config(&m).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        // The shutdown grace is 30s but the deadline is 5s: this test only
        // passes if shutdown closes idle keep-alive connections promptly
        // instead of waiting out the grace period.
        let server = tokio::spawn(serve_tls_inner(
            listener,
            axum::Router::new(),
            cfg,
            std::time::Duration::from_secs(5),
            32,
            std::time::Duration::from_secs(30),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // A completed handshake that then sits idle on keep-alive.
        let _idle = tls_connect(addr, &m.fingerprint).await;
        // Give the server a moment to finish the handshake and enter keep-alive.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let deadline = std::time::Duration::from_secs(5);
        let _ = shutdown_tx.send(());
        let start = std::time::Instant::now();
        tokio::time::timeout(deadline, server)
            .await
            .expect("server must shut down despite an idle keep-alive connection")
            .unwrap()
            .unwrap();
        assert!(start.elapsed() < deadline, "shutdown must be finite");
    }

    #[tokio::test]
    async fn connection_limit_rejects_excess_connections() {
        const LIMIT: usize = 2;
        let dir = tempfile::tempdir().unwrap();
        let m = material(dir.path());
        let cfg = server_config(&m).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_tls_inner(
            listener,
            axum::Router::new(),
            cfg,
            std::time::Duration::from_secs(5),
            LIMIT,
            std::time::Duration::from_secs(5),
            async move {
                let _ = shutdown_rx.await;
            },
        ));

        // Two connections complete handshakes and stay open (idle keep-alive),
        // holding both connection permits for the rest of the test.
        let _hold1 = tls_connect(addr, &m.fingerprint).await;
        let _hold2 = tls_connect(addr, &m.fingerprint).await;
        // Let the server spawn both tasks before probing for rejection.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Connections past the limit must be closed by the server without a
        // handshake: the socket reports EOF or a reset, never stays open.
        for i in 0..3 {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let read = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let mut buf = [0u8; 1];
                // A readiness error (e.g. reset) is itself proof of rejection;
                // EOF arrives as `Ok(0)` from `try_read` afterwards.
                let _ = tcp.readable().await;
                tcp.try_read(&mut buf)
            })
            .await
            .unwrap_or_else(|_| {
                panic!("excess connection {i} was not closed by the server");
            });
            assert!(
                matches!(read, Ok(0)) || read.is_err(),
                "excess connection {i} must be rejected, got {read:?}"
            );
        }

        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("server must shut down after the rejection test")
            .unwrap()
            .unwrap();
    }
}