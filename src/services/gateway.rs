//! Embedded host-routing gateway.
//!
//! A second, independent HTTP listener (configured under `[sites].listen`)
//! that maps the `Host` header of each request to a website record and
//! either serves its static root or reverse-proxies its upstream. It is
//! deliberately separate from the panel's `web.listen` so public vhost
//! traffic never touches the admin UI surface.
//!
//! Security model:
//! - **Static roots** are constrained under `paths.website_dir/server_<id>`
//!   by `websites::root_for_dir` (lexical + canonicalize containment
//!   checks) and the URL path is joined with `files::safe_join`, which
//!   rejects `..` escapes and symlink components. The gateway never follows
//!   symlinks inside a site root.
//! - **Reverse proxying** is SSRF-gated: the upstream must resolve to
//!   loopback or a private-network address (RFC 1918 / IPv6 ULA), and
//!   redirects are never followed. Local workloads listen on such addresses;
//!   a public-IP upstream is refused because the gateway would otherwise be
//!   an open proxy to the internet and to cloud metadata endpoints. This is
//!   the deliberate policy — `validate_upstream` already restricts the
//!   *format* at write time, and the gateway re-checks the *address* at
//!   request time, and the connection is pinned to the checked address (see
//!   `proxy`), closing the DNS-rebinding window: the upstream is connected by
//!   IP while the Host header and TLS SNI keep the configured name.
//! - **`force_https`** redirects every plain-HTTP request with a 308. The
//!   gateway terminates plain HTTP, so a request counts as HTTPS only when
//!   the socket peer is inside `[sites].trusted_proxies` AND the request
//!   carries `X-Forwarded-Proto: https`. With no trusted proxies configured
//!   the forwarded header is never trusted.
//! - `/__volt/health` is answered by the router before host dispatch, so it
//!   works for any `Host` header.
//!
//! Startup contract (see `main.rs`): a bind error on a *configured* listen
//! address fails fast — the panel refuses to boot with a misconfigured
//! gateway. A runtime serve error is logged and the panel keeps running.
use crate::config::Config;
use crate::db::Db;
use crate::services::{files, websites};
use anyhow::{anyhow, bail, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{HeaderName, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use percent_encoding::percent_decode_str;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::io::ReaderStream;
use url::Url;

#[derive(Clone)]
struct GwState {
    db: Db,
    cfg: Config,
    client: reqwest::Client,
}

/// The host-routing gateway. `start` returns `Ok(None)` when the gateway is
/// disabled (`[sites].listen` unset).
pub struct Gateway {
    db: Db,
    cfg: Config,
}

impl Gateway {
    pub fn new(db: Db, cfg: Config) -> Self {
        Self { db, cfg }
    }

    /// Bind and serve the gateway on the configured `[sites].listen` address.
    /// Fails fast (Err) on a bind error; once bound, runtime serve failures
    /// are logged by the task itself. The task shuts down gracefully when
    /// `running` flips false.
    pub async fn start(
        self,
        running: Arc<AtomicBool>,
    ) -> Result<Option<tokio::task::JoinHandle<()>>> {
        let Some(listen) = self.cfg.sites.listen else {
            return Ok(None);
        };
        let client = build_client()?;
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| anyhow!("cannot bind sites.listen {listen}: {e}"))?;
        tracing::info!("site gateway listening on http://{listen} (host-routed vhosts)");
        let app = router(self.db, self.cfg, client);
        let stop = wait_for_stop(running);
        let handle = tokio::spawn(async move {
            let serve = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop)
            .await;
            if let Err(e) = serve {
                tracing::error!("site gateway server error: {e}");
            }
        });
        Ok(Some(handle))
    }
}

/// Shared client settings for upstream requests: 5s connect timeout, 30s
/// total timeout (from first connect through the end of the response body,
/// so a stalled upstream can never pin a gateway task forever — which also
/// keeps graceful shutdown bounded: in-flight proxies drain within 30s), and
/// redirects disabled so a `Location` can never point at an address the SSRF
/// check never saw.
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
}

fn build_client() -> Result<reqwest::Client> {
    Ok(client_builder().build()?)
}

/// A client whose connections to `host` are pinned to `addr` instead of a
/// fresh DNS lookup: the URL hostname is still used for the Host header and
/// TLS SNI, but the socket can only ever reach the SSRF-checked address.
fn resolve_pinned_client(host: &str, addr: SocketAddr) -> Result<reqwest::Client> {
    Ok(client_builder().resolve(host, addr).build()?)
}

fn router(db: Db, cfg: Config, client: reqwest::Client) -> Router {
    // The gateway's own body cap (`sites.max_body_mb`, default 16 MiB) —
    // independent of the panel's `web.max_body_mb`. The layer bounds any
    // extractor route; `dispatch` consumes the raw `Request`, so `proxy`
    // re-applies the same cap manually via `axum::body::to_bytes`, which is
    // the bound that actually bites there.
    let max_body = (cfg.sites.max_body_mb as usize).saturating_mul(1024 * 1024);
    Router::new()
        .route("/__volt/health", get(health))
        .fallback(dispatch)
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(GwState { db, cfg, client })
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn wait_for_stop(running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn dispatch(
    State(st): State<GwState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let host_for_resolve = host.clone();
    let site = match crate::db::blocking(st.db.clone(), move |db| {
        websites::resolve_host(&db, &host_for_resolve)
    })
    .await
    {
        Ok(Some(site)) => site,
        Ok(None) => {
            tracing::debug!("gateway: no enabled site for host {host:?}");
            return not_found();
        }
        Err(e) => {
            tracing::error!("gateway: host resolution failed for {host:?}: {e}");
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap();
        }
    };
    if site.force_https && !request_is_https(&st, peer, &req) {
        return redirect_https(&host, req.uri());
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    match site.proxy_type.as_str() {
        "proxy" => match proxy(&st, &site, req).await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::warn!("gateway: proxy error for {}: {e}", site.domain);
                Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .body(Body::empty())
                    .unwrap()
            }
        },
        _ => match serve_static(&st.cfg, &site, method, path).await {
            Ok(resp) => resp,
            Err(_) => not_found(),
        },
    }
}

/// A request counts as HTTPS only when a trusted proxy (socket peer inside
/// `[sites].trusted_proxies`) says so via `X-Forwarded-Proto`.
fn request_is_https(st: &GwState, peer: SocketAddr, req: &Request) -> bool {
    let proto = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    if proto != Some("https") {
        return false;
    }
    st.cfg
        .sites
        .trusted_proxies
        .iter()
        .any(|net| net.contains(peer.ip()))
}

fn redirect_https(host: &str, uri: &Uri) -> Response {
    let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let location = format!("https://{host}{target}");
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header("location", location)
        .body(Body::empty())
        .unwrap()
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .unwrap()
}

/// Serve a static site. The URL path is percent-decoded and joined onto the
/// site root with `files::safe_join`, which rejects `..` escapes and any
/// symlink component, so a site can never reach outside its own root. A
/// directory resolves to its `index.html`. Directory-listing and non-GET
/// methods are refused.
async fn serve_static(
    cfg: &Config,
    site: &websites::Site,
    method: Method,
    raw_path: String,
) -> Result<Response> {
    if method != Method::GET && method != Method::HEAD {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())?);
    }
    let root = websites::root_for_dir(cfg, site.server_id, &site.root_dir)?;
    let decoded = percent_decode_str(&raw_path)
        .decode_utf8()
        .map_err(|_| anyhow!("path is not valid UTF-8"))?;
    if decoded.contains('\0') {
        bail!("path contains a NUL byte");
    }
    let rel = decoded.trim_start_matches('/');
    let joined = files::safe_join(&root, rel)?;
    let meta = tokio::fs::metadata(&joined).await?;
    let target = if meta.is_dir() {
        let idx = if rel.is_empty() {
            "index.html".to_string()
        } else {
            format!("{}/index.html", rel.trim_end_matches('/'))
        };
        files::safe_join(&root, &idx)?
    } else {
        joined
    };
    let file = tokio::fs::File::open(&target).await?;
    let len = file.metadata().await?.len();
    if !file.metadata().await?.is_file() {
        bail!("not a regular file");
    }
    let mime = mime_guess::from_path(&target).first_or_octet_stream();
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, mime.as_ref())
        .header(CONTENT_LENGTH, len)
        .header("x-content-type-options", "nosniff");
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(file))
    };
    Ok(builder.body(body)?)
}

/// Reverse-proxy to the site's upstream. Hop-by-hop headers are stripped on
/// both legs; `Host` and `Content-Length` are left to reqwest/hyper to set
/// from the target URL and body. Response headers and status pass through.
async fn proxy(st: &GwState, site: &websites::Site, req: Request) -> Result<Response> {
    let upstream = Url::parse(&site.upstream)?;
    let (host, addrs) = ssrf_check(&upstream).await?;
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", site.upstream.trim_end_matches('/'), path_and_query);
    // DNS-rebinding pin: connect only to the SSRF-checked address. A
    // literal-IP upstream connects by construction, so the shared client
    // suffices; a hostname upstream gets a per-request client whose DNS for
    // `host` is replaced with the first validated address. Every A record
    // passed the check, so the first is a safe pick — the Host header and
    // TLS SNI still carry the configured name.
    let client = if host.parse::<IpAddr>().is_ok() {
        st.client.clone()
    } else {
        resolve_pinned_client(&host, addrs[0])?
    };
    let mut fwd = client.request(req.method().clone(), url);
    // The gateway terminates plain HTTP and buffers the request body up to
    // the gateway's own cap (`sites.max_body_mb`, independent of the panel's
    // `web.max_body_mb`) — axum's request body type is not `Sync`, so it
    // cannot be streamed into reqwest. The router's DefaultBodyLimit does not
    // apply to this raw-`Request` route; this manual cap is what bounds it.
    let max_body = (st.cfg.sites.max_body_mb as usize).saturating_mul(1024 * 1024);
    let bytes = axum::body::to_bytes(req.into_body(), max_body)
        .await
        .map_err(|e| anyhow!("reading request body: {e}"))?;
    fwd = fwd.body(bytes);
    let resp = client.execute(fwd.build()?).await?;
    let mut builder = Response::builder().status(resp.status());
    for (name, value) in resp.headers() {
        if is_hop_by_hop(name) || name == CONTENT_LENGTH {
            continue;
        }
        builder = builder.header(name, value);
    }
    Ok(builder.body(Body::from_stream(resp.bytes_stream()))?)
}

/// SSRF policy for proxy upstreams: every address the target resolves to
/// must be loopback or private (RFC 1918 / IPv6 ULA). Public addresses are
/// refused — local workloads listen on local/private addresses, and allowing
/// anything else would make the gateway an open proxy into the internet and
/// cloud metadata endpoints. Link-local and unspecified addresses are also
/// refused.
///
/// Returns the trimmed hostname (brackets stripped for IPv6 literals) and
/// every validated address. [`proxy`] pins the connection to the returned
/// addresses, so a DNS rebinding between this check and the connect cannot
/// redirect the proxy to an address that was never vetted.
async fn ssrf_check(upstream: &Url) -> Result<(String, Vec<SocketAddr>)> {
    let host = upstream
        .host_str()
        .ok_or_else(|| anyhow!("upstream has no host"))?;
    let port = upstream
        .port_or_known_default()
        .ok_or_else(|| anyhow!("upstream has no port"))?;
    // `host_str` keeps the brackets on IPv6 literals (`[::1]`); strip them
    // for address parsing and DNS lookup.
    let host_trimmed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host_trimmed.parse::<IpAddr>() {
        return if allowed_upstream_addr(ip) {
            Ok((host_trimmed.to_string(), vec![SocketAddr::new(ip, port)]))
        } else {
            bail!("refusing upstream {host}:{port}: not a loopback or private address")
        };
    }
    let mut addrs = Vec::new();
    for addr in tokio::net::lookup_host((host_trimmed, port)).await? {
        if !allowed_upstream_addr(addr.ip()) {
            bail!("refusing upstream {host}:{port}: {addr} is not a loopback or private address");
        }
        addrs.push(addr);
    }
    if addrs.is_empty() {
        bail!("upstream {host} did not resolve");
    }
    Ok((host_trimmed.to_string(), addrs))
}


fn allowed_upstream_addr(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => v6.is_unique_local(),
    }
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;
    use tower::ServiceExt;

    fn test_client() -> reqwest::Client {
        build_client().unwrap()
    }


    /// Drive the gateway router directly (no listener) with a synthetic peer.
    async fn send(
        app: Router,
        host: &str,
        method: Method,
        path: &str,
        extra_headers: &'static [(&'static str, &'static str)],
    ) -> Response {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, host)
            .body(Body::empty())
            .unwrap();
        for (k, v) in extra_headers {
            req.headers_mut().insert(*k, HeaderValue::from_str(v).unwrap());
        }
        req.extensions_mut()
            .insert(ConnectInfo("127.0.0.1:9999".parse::<SocketAddr>().unwrap()));
        app.oneshot(req).await.unwrap()
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    struct TestDb {
        db: Db,
        path: std::path::PathBuf,
        sid: i64,
    }

    impl TestDb {
        fn new() -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "voltpanel-gateway-test-{}-{}.db",
                std::process::id(),
                seq
            ));
            let _ = std::fs::remove_file(&path);
            let db = crate::db::open(path.to_str().unwrap()).unwrap();
            let conn = db.get().unwrap();
            conn.execute(
                "INSERT INTO users(username,email,password_hash,created_at,updated_at)
                 VALUES('t','t@t','x','now','now')",
                [],
            )
            .unwrap();
            let uid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO blueprints(uuid,name,created_at,updated_at)
                 VALUES('b','b','now','now')",
                [],
            )
            .unwrap();
            let bid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO servers(uuid,name,user_id,blueprint_id,created_at,updated_at)
                 VALUES('s','s',?1,?2,'now','now')",
                rusqlite::params![uid, bid],
            )
            .unwrap();
            let sid = conn.last_insert_rowid();
            drop(conn);
            TestDb { db, path, sid }
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_file(format!("{}-wal", self.path.display()));
            let _ = std::fs::remove_file(format!("{}-shm", self.path.display()));
        }
    }

    fn insert_site(
        conn: &rusqlite::Connection,
        server_id: i64,
        domain: &str,
        proxy_type: &str,
        upstream: &str,
        root_dir: &str,
        force_https: bool,
        enabled: bool,
    ) {
        conn.execute(
            "INSERT INTO websites(server_id,domain,root_dir,proxy_type,upstream,ssl,force_https,enabled,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'now','now')",
            rusqlite::params![
                server_id,
                domain,
                root_dir,
                proxy_type,
                upstream,
                1i64,
                force_https as i64,
                enabled as i64
            ],
        )
        .unwrap();
    }

    fn temp_website_dir() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let website_dir = tmp.path().join("websites");
        let server_dir = website_dir.join("server_1");
        std::fs::create_dir_all(server_dir.join("assets")).unwrap();
        (tmp, website_dir)
    }

    fn static_config(website_dir: &std::path::Path) -> Config {
        let mut cfg = Config::default();
        cfg.paths.website_dir = website_dir.to_path_buf();
        cfg
    }

    #[tokio::test]
    async fn static_host_dispatch_and_index() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        std::fs::write(
            website_dir.join("server_1/assets/index.html"),
            "<h1>hello</h1>",
        )
        .unwrap();
        std::fs::write(
            website_dir.join("server_1/assets/robots.txt"),
            "User-agent: *\n",
        )
        .unwrap();
        {
            let conn = t.db.get().unwrap();
            insert_site(&conn, t.sid, "one.example.com", "static", "", "assets", false, true);
        }
        let cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        // index.html served at /
        let resp = send(app.clone(), "one.example.com", Method::GET, "/", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[CONTENT_TYPE], "text/html");
        assert_eq!(body_text(resp).await, "<h1>hello</h1>");

        // named file served with correct length
        let resp = send(app.clone(), "one.example.com", Method::GET, "/robots.txt", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[CONTENT_LENGTH].to_str().unwrap(),
            "User-agent: *\n".len().to_string()
        );
        assert_eq!(body_text(resp).await, "User-agent: *\n");

        // HEAD gets headers but no body
        let resp = send(app, "one.example.com", Method::HEAD, "/robots.txt", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[CONTENT_LENGTH].to_str().unwrap(), "14");
        assert_eq!(body_text(resp).await, "");

        let _ = tmp;
    }

    #[tokio::test]
    async fn traversal_and_symlink_are_never_served() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        let server_dir = website_dir.join("server_1");
        std::fs::write(server_dir.join("assets/index.html"), "inside").unwrap();
        // A secret OUTSIDE the site root must stay unreachable.
        std::fs::write(tmp.path().join("secret.txt"), "top-secret").unwrap();
        // A symlink inside the root must stay unreachable too.
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), server_dir.join("assets/leak"))
            .unwrap();
        {
            let conn = t.db.get().unwrap();
            insert_site(&conn, t.sid, "one.example.com", "static", "", "assets", false, true);
        }
        let cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        for path in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/%2e%2e%2fsecret.txt",
            "/assets/../../secret.txt",
            "/leak",
            "/assets/leak",
            "/assets/../assets/../secret.txt",
        ] {
            let resp = send(app.clone(), "one.example.com", Method::GET, path, &[]).await;
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} must not be served"
            );
            assert_ne!(body_text(resp).await, "top-secret", "{path} leaked the secret");
        }

        let _ = tmp;
    }

    #[tokio::test]
    async fn unknown_host_and_disabled_site_are_404() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        std::fs::write(
            website_dir.join("server_1/assets/index.html"),
            "hello",
        )
        .unwrap();
        {
            let conn = t.db.get().unwrap();
            insert_site(&conn, t.sid, "on.example.com", "static", "", "assets", false, true);
            insert_site(&conn, t.sid, "off.example.com", "static", "", "assets", false, false);
        }
        let cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        assert_eq!(
            send(app.clone(), "unknown.example.net", Method::GET, "/", &[]).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            send(app.clone(), "off.example.com", Method::GET, "/", &[]).await.status(),
            StatusCode::NOT_FOUND
        );
        // the enabled site still works
        assert_eq!(
            send(app, "on.example.com", Method::GET, "/", &[]).await.status(),
            StatusCode::OK
        );
        let _ = tmp;
    }

    #[tokio::test]
    async fn reverse_proxies_to_local_upstream() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        let upstream_app = axum::Router::new()
            .route(
                "/echo",
                get(|| async { (StatusCode::OK, "upstream-ok") }),
            )
            .route(
                "/status",
                get(|| async { (StatusCode::IM_A_TEAPOT, "teapot") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _upstream = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        {
            let conn = t.db.get().unwrap();
            insert_site(
                &conn,
                t.sid,
                "proxy.example.com",
                "proxy",
                &format!("http://{addr}"),
                "",
                false,
                true,
            );
        }
        let cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        let resp = send(app.clone(), "proxy.example.com", Method::GET, "/echo", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_text(resp).await, "upstream-ok");

        // path + query pass through; upstream status is preserved
        let resp = send(app, "proxy.example.com", Method::GET, "/status?x=1", &[]).await;
        assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(body_text(resp).await, "teapot");

        let _ = tmp;
    }

    #[tokio::test]
    async fn force_https_redirects_unless_a_trusted_proxy_says_https() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        std::fs::write(
            website_dir.join("server_1/assets/index.html"),
            "hello",
        )
        .unwrap();
        {
            let conn = t.db.get().unwrap();
            insert_site(&conn, t.sid, "secure.example.com", "static", "", "assets", true, true);
        }
        let mut cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        // no trusted proxies -> XFP never trusted -> always redirect
        let resp = send(
            app.clone(),
            "secure.example.com",
            Method::GET,
            "/x?y=1",
            &[("x-forwarded-proto", "https")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers()["location"],
            "https://secure.example.com/x?y=1"
        );

        // plain request redirects too
        let resp = send(app.clone(), "secure.example.com", Method::GET, "/", &[]).await;
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);

        // trusted proxy + XFP https -> served
        cfg.sites.trusted_proxies = vec!["127.0.0.1".parse().unwrap()];
        let app = router(t.db.clone(), cfg.clone(), test_client());
        let resp = send(
            app.clone(),
            "secure.example.com",
            Method::GET,
            "/",
            &[("x-forwarded-proto", "https")],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // trusted proxy but no XFP -> still redirect
        let resp = send(app, "secure.example.com", Method::GET, "/", &[]).await;
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);

        let _ = tmp;
    }

    #[tokio::test]
    async fn health_route_ignores_host_routing() {
        let t = TestDb::new();
        let (tmp, website_dir) = temp_website_dir();
        let cfg = static_config(&website_dir);
        let app = router(t.db.clone(), cfg.clone(), test_client());

        // unknown Host, but the health route is outside host dispatch
        let resp = send(app.clone(), "no-such-host.example", Method::GET, "/__volt/health", &[]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_text(resp).await, "ok");

        let _ = tmp;
    }

    #[tokio::test]
    async fn disabled_gateway_start_returns_none() {
        let t = TestDb::new();
        let cfg = Config::default(); // sites.listen = None
        assert!(cfg.sites.listen.is_none());
        let gw = Gateway::new(t.db.clone(), cfg);
        let running = Arc::new(AtomicBool::new(true));
        let handle = gw.start(running).await.unwrap();
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn configured_bind_error_fails_fast() {
        let t = TestDb::new();
        // Occupy a port, then ask the gateway to bind it: startup must fail
        // fast (Err), not silently fall back to serving without a gateway.
        let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = blocker.local_addr().unwrap();
        let mut cfg = Config::default();
        cfg.sites.listen = Some(addr);
        let gw = Gateway::new(t.db.clone(), cfg);
        let running = Arc::new(AtomicBool::new(true));
        let err = gw.start(running).await.unwrap_err();
        assert!(err.to_string().contains("sites.listen"), "{err}");
        drop(blocker);
    }

    #[tokio::test]
    async fn ssrf_policy_blocks_public_and_allows_local() {
        let ok = [
            "http://127.0.0.1:8080",
            "http://10.1.2.3:80",
            "http://172.16.0.1:8080",
            "http://192.168.1.10:3000",
            "http://[::1]:80",
            "http://[fc00::1]:80",
            "http://localhost:8080", // resolves to loopback
        ];
        for u in ok {
            let url = Url::parse(u).unwrap();
            let (_host, addrs) = ssrf_check(&url).await.expect(u);
            assert!(!addrs.is_empty(), "{u} must return validated addrs");
            assert!(
                addrs.iter().all(|a| allowed_upstream_addr(a.ip())),
                "{u} addrs must all be allowed"
            );
        }
        // A literal IP returns exactly itself, bracketed IPv6 stripped.
        let (host, addrs) = ssrf_check(&Url::parse("http://127.0.0.1:8080").unwrap())
            .await
            .unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(addrs, vec!["127.0.0.1:8080".parse::<SocketAddr>().unwrap()]);
        let (host, addrs) = ssrf_check(&Url::parse("http://[::1]:80").unwrap())
            .await
            .unwrap();
        assert_eq!(host, "::1");
        assert_eq!(addrs, vec!["[::1]:80".parse::<SocketAddr>().unwrap()]);
        let bad = [
            "http://8.8.8.8:53",
            "http://169.254.169.254:80", // cloud metadata
            "http://203.0.113.7:80",
            "http://0.0.0.0:80", // unspecified
        ];
        for u in bad {
            let url = Url::parse(u).unwrap();
            assert!(
                ssrf_check(&url).await.is_err(),
                "{u} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn resolver_pin_forces_connection_to_checked_addr() {
        // The pin helper must connect to the pinned address even though the
        // URL host is a name that real DNS would never resolve to it. The
        // gateway relies on this to close the DNS-rebinding window.
        let upstream_app = axum::Router::new()
            .route("/", get(|| async { "pinned" }))
            .route("/echo-host", get(|headers: axum::http::HeaderMap| async move {
                headers
                    .get(HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _upstream = tokio::spawn(async move {
            axum::serve(listener, upstream_app).await.unwrap();
        });
        let client = resolve_pinned_client("pin.test", addr).unwrap();
        let resp = client.get("http://pin.test/").send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "pinned");
        // The URL hostname is still what the upstream sees.
        let resp = client.get("http://pin.test/echo-host").send().await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "pin.test");
    }
}
