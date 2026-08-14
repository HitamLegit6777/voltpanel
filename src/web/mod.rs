//! UI layer: SPA shell, static assets, catch-all route.
use crate::api::AppState;
use axum::extract::State;
use axum::response::{Html, IntoResponse};

const APP_CSS: &str = include_str!("../../static/css/app.css");
const ICONS_JS: &str = include_str!("../../static/js/icons.js");
const APP_JS: &str = include_str!("../../static/js/app.js");

/// Version stamp for immutable assets, compiled in from the package version:
/// an upgraded panel serves new JS/CSS under fresh URLs, so browsers can never
/// keep a stale bundle across releases. `index.html` references
/// `?v=__ASSET_VERSION__`; the placeholder is stamped at serve time.
pub const ASSET_VERSION: &str = include!(concat!(env!("OUT_DIR"), "/asset_version.rs"));

/// The SPA shell with the version stamp baked in. Compiled once.
static INDEX_HTML: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    include_str!("../../templates/index.html").replace("__ASSET_VERSION__", ASSET_VERSION)
});

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        body,
    )
}

pub async fn app_css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", APP_CSS)
}

pub async fn icons_js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", ICONS_JS)
}

pub async fn app_js() -> impl IntoResponse {
    asset("text/javascript; charset=utf-8", APP_JS)
}

const FAVICON_SVG: &str = include_str!("../../static/img/favicon.svg");

pub async fn favicon_svg() -> impl IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/svg+xml"),
            (
                axum::http::header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
        ],
        FAVICON_SVG,
    )
}

pub async fn index(State(_state): State<AppState>) -> impl IntoResponse {
    (
        [
            (
                axum::http::header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Html(INDEX_HTML.clone()),
    )
}

/// SPA fallback: serve index.html for extensionless non-API paths only.
/// Missing `/static/*` assets and any path that looks like a file (its last
/// segment carries an extension) get a plain 404 instead of an HTML 200, so a
/// stale or typo'd asset reference never masks a broken deployment.
pub async fn spa_fallback(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error":"API route not found","status":404})),
        )
            .into_response();
    }
    if path.starts_with("/static/") || looks_like_file(path) {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    index(State(state)).await.into_response()
}

/// True when the final path segment carries a file extension (`favicon.ico`,
/// `robots.txt`). SPA routes are hash-based and never reach the server.
fn looks_like_file(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
}