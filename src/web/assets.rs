//! The stylesheet, scripts and icons, embedded in the binary. Datastar's
//! script is `askama_datastar`'s, served from `/assets/datastar.js`, and the
//! typeface is koan-kit-web's, served beside the stylesheet that names it.
//!
//! Each is linked by a URL carrying a hash of its contents, so a page from a
//! new build fetches its own assets rather than a cached copy of the last
//! build's, and a hashed URL can be cached for good.

use askama_datastar::Asset;
use axum::extract::{Path, RawQuery};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

static APP_CSS: Asset = Asset::new(
    include_bytes!("../../assets/app.css"),
    "text/css; charset=utf-8",
);
/// browser/app.ts with its types stripped by build.rs.
static APP_JS: Asset = Asset::new(
    include_bytes!(concat!(env!("OUT_DIR"), "/app.js")),
    "text/javascript; charset=utf-8",
);
/// browser/player.ts, likewise: the video player, on public pages too.
static PLAYER_JS: Asset = Asset::new(
    include_bytes!(concat!(env!("OUT_DIR"), "/player.js")),
    "text/javascript; charset=utf-8",
);
/// browser/join.ts: moves an invite from the address into the join form.
static JOIN_JS: Asset = Asset::new(
    include_bytes!(concat!(env!("OUT_DIR"), "/join.js")),
    "text/javascript; charset=utf-8",
);
static ICON_SVG: Asset = Asset::new(include_bytes!("../../assets/icon.svg"), "image/svg+xml");
static ICON_PNG: Asset = Asset::new(include_bytes!("../../assets/icon-180.png"), "image/png");

pub fn css_url() -> String {
    APP_CSS.url("/assets/app.css")
}

pub fn app_js_url() -> String {
    APP_JS.url("/assets/app.js")
}

pub fn player_js_url() -> String {
    PLAYER_JS.url("/assets/player.js")
}

pub fn join_js_url() -> String {
    JOIN_JS.url("/assets/join.js")
}

pub fn icon_svg_url() -> String {
    ICON_SVG.url("/assets/icon.svg")
}

pub fn icon_png_url() -> String {
    ICON_PNG.url("/assets/icon-180.png")
}

pub async fn serve(Path(name): Path<String>, RawQuery(query): RawQuery) -> Response {
    let asset = match name.as_str() {
        "app.css" => &APP_CSS,
        "app.js" => &APP_JS,
        "player.js" => &PLAYER_JS,
        "join.js" => &JOIN_JS,
        "icon.svg" => &ICON_SVG,
        "icon-180.png" => &ICON_PNG,
        _ => return (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response(),
    };
    asset.response(query.as_deref())
}

pub async fn favicon() -> Response {
    ICON_PNG.response(None)
}
