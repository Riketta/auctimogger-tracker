//! Embedded UI assets. The stylesheet and script are compiled into the
//! binary (`include_str!`): no filesystem layout, no extra routes beyond the
//! two below, and the single-binary property (Docker, release tarballs)
//! survives.

use axum::{
    http::{
        header,
        StatusCode,
    },
    response::{
        IntoResponse,
        Response,
    },
};

/// The compiled design system (tokens + components).
const APP_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/app.css"));
/// The enhancement layer (sorting, completions, chart, mutation buttons).
const APP_JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/app.js"));

/// Browser-cache window for the assets: 5 minutes of freshness, then
/// revalidate — new releases change asset content while the URL stays stable,
/// so the window stays deliberately short.
const ASSET_CACHE: &str = "public, max-age=300";

/// `GET /assets/app.css`
pub async fn asset_css() -> Response {
    static_response(APP_CSS, "text/css; charset=utf-8").await
}

/// `GET /assets/app.js`
pub async fn asset_js() -> Response {
    static_response(APP_JS, "text/javascript; charset=utf-8").await
}

async fn static_response(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, ASSET_CACHE)], body)
        .into_response()
}

/// `GET /favicon.svg` — the brand dot, inlined so no favicon request 404s.
pub async fn favicon() -> Response {
    const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 32 32\">\
<rect width=\"32\" height=\"32\" rx=\"7\" fill=\"#12151d\"/>\
<circle cx=\"16\" cy=\"16\" r=\"7\" fill=\"#5aa2ff\"/></svg>";
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, ASSET_CACHE)],
        SVG,
    )
        .into_response()
}
