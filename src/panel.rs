//! Embedded management panel — Vite build output (Vue 3 SPA) served as
//! static assets. Dev mode: `npm run dev` in web/panel (hot reload, proxies
//! /manage to the gateway); release: `npm run build && cargo build` embeds
//! dist/ into the binary.

use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/panel/dist"]
pub struct PanelDist;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// GET /manage/panel/ — index without path args.
pub async fn serve_root() -> axum::response::Response {
    serve_inner("index.html")
}

/// GET /manage/panel/{*rest} — static assets; missing paths return 404.
pub async fn serve(
    axum::extract::Path(rest): axum::extract::Path<String>,
) -> axum::response::Response {
    serve_inner(&rest)
}

fn serve_inner(path: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    PanelDist::get(path)
        .map(|f| {
            (
                [(axum::http::header::CONTENT_TYPE, content_type(path))],
                f.data.to_vec(),
            )
                .into_response()
        })
        .unwrap_or_else(|| axum::http::StatusCode::NOT_FOUND.into_response())
}
