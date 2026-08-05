//! UI layer: SPA shell, static assets, catch-all route.
use crate::api::AppState;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use tower_http::services::ServeDir;

pub fn static_dir() -> ServeDir {
    ServeDir::new("static").append_index_html_on_directories(true)
}

pub async fn index(State(_state): State<AppState>) -> impl IntoResponse {
    let html = include_str!("../../templates/index.html");
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Html(html),
    )
}


/// SPA fallback: serve index.html for any non-API path.
pub async fn spa_fallback(State(state): State<AppState>) -> impl IntoResponse {
    index(State(state)).await
}

