//! Self-signed TLS material and certificate pinning shared by the panel and `voltd`.
//!
//! VoltPanel deployments frequently have no domain name: nodes are reached by raw
//! IP on a private network, so public CA issuance is impossible. Instead every
//! endpoint generates a long-lived self-signed certificate on first boot and the
//! peer pins its SHA-256 fingerprint, exchanged over the already authenticated
//! enrollment channel. That yields real transport encryption plus an identity
//! check that does not depend on the WebPKI.
use anyhow::{bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;
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
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        bail!("no PEM certificate found");
    }
    Ok(certs)
}

fn parse_key(pem: &str) -> Result<PrivateKeyDer<'static>> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    rustls_pemfile::private_key(&mut reader)?.context("no PEM private key found")
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
/// The SAN extension stores DNS names as ASCII and IPs as raw octets; scanning the
/// DER for both forms avoids pulling in a full X.509 parser for a staleness check.
fn covers_sans(cert_pem: &str, sans: &[String]) -> bool {
    let Ok(certs) = parse_certs(cert_pem) else {
        return false;
    };
    let Some(leaf) = certs.first() else {
        return false;
    };
    let der = leaf.as_ref();
    sans.iter().all(|san| {
        if let Ok(ip) = san.parse::<std::net::IpAddr>() {
            let octets: Vec<u8> = match ip {
                std::net::IpAddr::V4(v) => v.octets().to_vec(),
                std::net::IpAddr::V6(v) => v.octets().to_vec(),
            };
            der.windows(octets.len()).any(|w| w == octets)
        } else {
            der.windows(san.len()).any(|w| w == san.as_bytes())
        }
    })
}

/// Load `dir/cert.pem` + `dir/key.pem`, generating a self-signed pair when absent.
///
/// Regenerates when the stored certificate lacks a SAN we now need, e.g. the machine
/// gained a public IP after first boot.
pub fn ensure_material(dir: &Path, sans: &[String]) -> Result<TlsMaterial> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(&cert_path)?;
        let key_pem = std::fs::read_to_string(&key_path)?;
        match fingerprint_pem(&cert_pem) {
            Ok(fingerprint) if covers_sans(&cert_pem, sans) => {
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
                return Ok(TlsMaterial {
                    cert_pem,
                    key_pem,
                    fingerprint,
                });
            }
            Ok(_) => tracing::info!("regenerating TLS certificate: local addresses changed"),
            Err(e) => tracing::warn!("stored certificate unreadable ({e}), regenerating"),
        }
    }

    let generated = rcgen::generate_simple_self_signed(sans.to_vec())
        .context("failed to generate self-signed certificate")?;
    let cert_pem = generated.cert.pem();
    let key_pem = generated.key_pair.serialize_pem();
    let fingerprint = fingerprint_der(generated.cert.der());
    std::fs::write(&cert_path, &cert_pem)?;
    std::fs::write(&key_path, &key_pem)?;
    std::fs::set_permissions(&cert_path, std::fs::Permissions::from_mode(0o644))?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("generated self-signed certificate, fingerprint {fingerprint}");
    Ok(TlsMaterial {
        cert_pem,
        key_pem,
        fingerprint,
    })
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
    if fingerprint.len() != 64 {
        bail!("expected a 64-character hex SHA-256 fingerprint, got {} characters", fingerprint.len());
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

/// Serve an axum router over TLS until `shutdown` resolves.
///
/// `axum::serve` has no TLS entry point, so the accept loop is spelled out here:
/// each connection is handshaked and driven on its own task, and `ConnectInfo` is
/// injected manually because the router never sees the raw `TcpStream`.
///
/// Shutdown drains in-flight connections by dropping the accept loop's task-count
/// sender and waiting for every connection task to release its clone.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    router: axum::Router,
    config: Arc<rustls::ServerConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    use tower::Service as _;

    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let make = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
    // Cloned into every connection task; the receiver resolves once all clones drop.
    let (alive, mut all_done) = tokio::sync::mpsc::channel::<()>(1);
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

        let acceptor = acceptor.clone();
        let alive = alive.clone();
        let mut make = make.clone();
        tokio::spawn(async move {
            let _alive = alive;
            let tls = match acceptor.accept(stream).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!("tls handshake with {peer} failed: {e}");
                    return;
                }
            };
            // Infallible: the make-service only clones the router and records `peer`.
            let Ok(svc) = make.call(peer).await;
            let svc = hyper_util::service::TowerToHyperService::new(svc);
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            if let Err(e) = builder
                .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), svc)
                .await
            {
                tracing::debug!("connection from {peer} ended: {e}");
            }
        });
    }

    drop(alive);
    let _ = all_done.recv().await;
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
        assert!(pinned_client_config(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn server_config_builds_from_material() {
        let dir = tempfile::tempdir().unwrap();
        assert!(server_config(&material(dir.path())).is_ok());
    }
}
