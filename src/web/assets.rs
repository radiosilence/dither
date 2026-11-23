//! The stylesheet, scripts, typeface and icons, embedded in the binary.
//!
//! Each is linked by a URL carrying a hash of its contents, so a page from a
//! new build fetches its own assets rather than a cached copy of the last
//! build's, and a hashed URL can be cached for good.

use std::sync::LazyLock;

use axum::extract::{Path, RawQuery};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

const APP_CSS: &str = include_str!("../../assets/app.css");
const DATASTAR_JS: &str = include_str!("../../assets/datastar.js");
/// browser/app.ts with its types stripped by build.rs.
const APP_JS: &str = include_str!(concat!(env!("OUT_DIR"), "/app.js"));
/// The typeface @radiosilence/theme sets; app.css names it by a URL relative
/// to itself.
const GEIST_MONO: &[u8] = include_bytes!("../../assets/geist-mono.woff2");
const ICON_SVG: &str = include_str!("../../assets/icon.svg");
const ICON_PNG: &[u8] = include_bytes!("../../assets/icon-180.png");

/// A short hash of `bytes` (FNV-1a): enough to notice a change.
fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

static CSS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/assets/app.css?v={}", fingerprint(APP_CSS.as_bytes())));
static DATASTAR_URL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "/assets/datastar.js?v={}",
        fingerprint(DATASTAR_JS.as_bytes())
    )
});
static APP_JS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/assets/app.js?v={}", fingerprint(APP_JS.as_bytes())));

pub fn css_url() -> &'static str {
    &CSS_URL
}

pub fn datastar_url() -> &'static str {
    &DATASTAR_URL
}

pub fn app_js_url() -> &'static str {
    &APP_JS_URL
}

fn respond(body: impl IntoResponse, kind: &'static str, query: Option<&str>) -> Response {
    // A hashed URL never changes; a bare one is rechecked hourly.
    let cache = if query.is_some_and(|q| q.starts_with("v=")) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)],
        body,
    )
        .into_response()
}

pub async fn serve(Path(name): Path<String>, RawQuery(query): RawQuery) -> Response {
    let q = query.as_deref();
    match name.as_str() {
        "app.css" => respond(APP_CSS, "text/css; charset=utf-8", q),
        "datastar.js" => respond(DATASTAR_JS, "text/javascript; charset=utf-8", q),
        "app.js" => respond(APP_JS, "text/javascript; charset=utf-8", q),
        "geist-mono.woff2" => respond(GEIST_MONO, "font/woff2", Some("v=")),
        "icon.svg" => respond(ICON_SVG, "image/svg+xml", q),
        "icon-180.png" => respond(ICON_PNG, "image/png", q),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub async fn favicon() -> Response {
    respond(ICON_PNG, "image/png", None)
}
