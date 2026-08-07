//! UI layer: SPA shell, static assets, catch-all route.
use crate::api::AppState;
use axum::extract::State;
use axum::response::{Html, IntoResponse};

const APP_CSS: &str = include_str!("../../static/css/app.css");
const ICONS_JS: &str = include_str!("../../static/js/icons.js");
const APP_JS: &str = include_str!("../../static/js/app.js");

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

pub async fn index(State(_state): State<AppState>) -> impl IntoResponse {
    let html = include_str!("../../templates/index.html");
    (
        [
            (
                axum::http::header::CACHE_CONTROL,
                "no-cache, no-store, must-revalidate",
            ),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Html(html),
    )
}

/// SPA fallback: serve index.html for any non-API path.
pub async fn spa_fallback(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> axum::response::Response {
    if uri.path().starts_with("/api/") {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error":"API route not found","status":404})),
        )
            .into_response();
    }
    index(State(state)).await.into_response()
}
