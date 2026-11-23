//! HTTP: the app behind sign-in, the public pages and image files, and the
//! JSON the uploader speaks.
//!
//! One router serves both hosts. While the public pages share the app's
//! hostname that is the whole story; once they have a domain of their own,
//! requests for that domain reach the public routes only, and it never sets
//! or receives a cookie. Every route behind sign-in sits under one session
//! layer, so a route added there later is protected by where it is added.

pub mod api;
pub mod app;
pub mod assets;
pub mod clip;
pub mod public;
pub mod work;

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures_util::Stream;

use crate::auth::session::Sessions;
use crate::config::Config;
use crate::db::Db;
use crate::store::Store;
use crate::upload::Uploads;

pub struct Inner {
    pub config: Config,
    pub db: Db,
    pub store: Store,
    pub sessions: Sessions,
    pub http: reqwest::Client,
    pub uploads: Arc<Uploads>,
    /// What the public routes served, counted anonymously.
    pub views: crate::views::Views,
    /// Becomes true when the server begins to stop: live streams end on it.
    pub stopping: tokio::sync::watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// Pages and fragments of the app: scripts from here only (Datastar needs
/// `unsafe-eval` for its expressions), no inline script or style, images from
/// here and from previews of files about to be uploaded.
const CSP_APP: &str = "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self'; \
     img-src 'self' blob: data:; font-src 'self'; connect-src 'self'; form-action 'self'; \
     frame-ancestors 'none'; base-uri 'none'; object-src 'none'";

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/albums/{id}/meta", post(app::album_meta))
        .route("/albums/{id}/order", post(app::reorder))
        .route("/albums/{id}/cover", post(app::cover))
        .route("/albums/{id}/delete", post(app::delete_album))
        .route("/albums/{id}/picker", get(app::picker))
        .route("/images/{id}/view", get(app::viewer))
        .route("/images/{id}/caption", post(app::caption))
        .route("/images/move", post(app::move_images))
        .route("/images/delete", post(app::delete_images))
        .route("/stream", get(app::stream))
        .route("/api/albums", post(api::create_album))
        .route("/api/albums/{id}/uploads", post(api::begin))
        .route(
            "/api/uploads/{id}",
            put(api::chunk).get(api::status).delete(api::cancel),
        )
        .route("/api/uploads/{id}/finish", post(api::finish))
        .layer(middleware::from_fn(marked_writes))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::extract::require_session,
        ));
    let app_only = Router::new()
        .route("/albums/{id}", get(app::album))
        .merge(protected)
        .merge(crate::auth::routes::router())
        .layer(middleware::from_fn_with_state(state.clone(), app_host_only));
    Router::new()
        .route("/", get(app::home))
        .merge(app_only)
        .route("/a/{id}", get(public::album))
        .route("/t/{file}", get(public::thumb))
        .route("/d/{file}", get(public::display))
        .route("/p/{file}", get(public::preview))
        .route("/{slug}", get(public::image))
        .route("/healthz", get(|| async { "ok" }))
        .route("/robots.txt", get(public::robots))
        .route("/favicon.ico", get(assets::favicon))
        .route("/assets/{name}", get(assets::serve))
        .fallback(public::not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), count))
        .layer(middleware::from_fn(preview_log))
        // Video is compressed already, and a compressed range would not be
        // the range asked for.
        .layer(
            tower_http::compression::CompressionLayer::new().compress_when(
                tower_http::compression::Predicate::and(
                    tower_http::compression::DefaultPredicate::new(),
                    tower_http::compression::predicate::NotForContentType::const_new("video/"),
                ),
            ),
        )
        .with_state(state)
}

/// The host a request is for, with its port when it names one, as the
/// configured URLs do.
pub fn host(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Whether a request reached the public pages' own domain.
pub fn on_public_host(config: &Config, headers: &HeaderMap) -> bool {
    config.split_hosts() && host(headers).eq_ignore_ascii_case(config.public_host())
}

async fn app_host_only(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if on_public_host(&state.config, request.headers()) {
        return public::not_found().await.into_response();
    }
    next.run(request).await
}

/// Link previews' fetchers: what their user agents carry, and their names.
const FETCHERS: [(&str, &str); 10] = [
    ("WhatsApp", "WhatsApp"),
    ("facebookexternalhit", "Facebook"),
    ("Twitterbot", "X"),
    ("Slackbot", "Slack"),
    ("Discordbot", "Discord"),
    ("TelegramBot", "Telegram"),
    ("LinkedInBot", "LinkedIn"),
    ("redditbot", "Reddit"),
    ("Iframely", "Iframely"),
    ("Embedly", "Embedly"),
];

/// The name of the link preview's fetcher a user agent belongs to.
fn fetcher(agent: &str) -> Option<&'static str> {
    FETCHERS
        .iter()
        .find(|(token, _)| agent.contains(token))
        .map(|(_, name)| *name)
}

/// Count what a public route served, as its handler marked the response
/// ([`crate::views::Counted`]), unless the album's owner asked for it. The
/// client's address is the last `X-Forwarded-For` entry (the one the proxy
/// in front appends), else the peer's; it goes into the visitor's hash and
/// nowhere else, so a forged header can do no more than inflate a count.
async fn count(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers().clone();
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let response = next.run(request).await;
    let Some(counted) = response.extensions().get::<crate::views::Counted>() else {
        return response;
    };
    if let Some(id) = crate::auth::cookie::session_id(&headers)
        && state
            .sessions
            .get(id)
            .await
            .is_some_and(|s| s.sub == counted.owner)
    {
        return response;
    }
    let text = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    let agent = text(header::USER_AGENT);
    let address = text(header::HeaderName::from_static("x-forwarded-for"))
        .rsplit(',')
        .next()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .or(peer)
        .unwrap_or_default();
    let own = [
        crate::views::host_only(state.config.app_host()),
        crate::views::host_only(state.config.public_host()),
    ];
    state.views.record(
        counted,
        &crate::views::Visit {
            address: &address,
            agent,
            referrer: crate::views::referrer(text(header::REFERER), &own),
            fetcher: fetcher(agent),
        },
    );
    response
}

/// What a link preview's fetcher asked for and was answered, logged: a
/// preview that does not appear is found out from what its fetcher saw.
async fn preview_log(request: Request, next: Next) -> Response {
    let agent = request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let Some(fetcher) = fetcher(agent) else {
        return next.run(request).await;
    };
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let response = next.run(request).await;
    tracing::info!(
        fetcher,
        %method,
        path,
        range,
        status = response.status().as_u16(),
        "a link preview's fetcher asked"
    );
    response
}

/// Every write carries a header a cross-site form cannot set: Datastar's own,
/// or the uploader's. With `SameSite=Lax` cookies this closes cross-site
/// request forgery twice over.
async fn marked_writes(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    let marked = request.headers().contains_key("datastar-request")
        || request.headers().contains_key("x-dither");
    if !safe && !marked {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// Headers every response gets unless its handler chose its own: the app's
/// policy for anything that did not say, and the hardening that applies to
/// everything.
async fn security_headers(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(CSP_APP));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.entry(header::REFERRER_POLICY)
        .or_insert(HeaderValue::from_static("no-referrer"));
    h.entry(header::X_FRAME_OPTIONS)
        .or_insert(HeaderValue::from_static("DENY"));
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), browsing-topics=()"),
    );
    if state.config.secure() {
        h.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// What the templates call: askama makes a call in a template a method on the
/// template, so every template has these.
pub trait Helpers {
    fn css_url(&self) -> &'static str {
        assets::css_url()
    }

    fn datastar_url(&self) -> &'static str {
        assets::datastar_url()
    }

    fn app_js_url(&self) -> &'static str {
        assets::app_js_url()
    }

    fn player_js_url(&self) -> &'static str {
        assets::player_js_url()
    }

    fn icon(&self, name: &str, px: f32) -> String {
        crate::icon::named(name, px)
    }

    /// The page's language, for `<html lang>`.
    fn lang(&self) -> &'static str {
        crate::i18n::LOCALE
    }

    /// A count from each plural form of the page's language, for the copy
    /// the browser's script reads.
    fn plural_samples(&self) -> impl Iterator<Item = u64> {
        crate::i18n::plural_samples().iter().copied()
    }
}

impl<T: askama::Template> Helpers for T {}

/// A Datastar patch of whole elements, matched by their ids.
pub fn patch(html: &str) -> Event {
    // A bare `\r` would start a line of its own, outside the `elements`
    // prefix.
    Event::default().event("datastar-patch-elements").data(
        html.replace('\r', "")
            .lines()
            .map(|l| format!("elements {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

pub fn sse(events: impl Stream<Item = Result<Event, Infallible>> + Send + 'static) -> Response {
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// `n` bytes for a person to read.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{bytes} B"),
        _ if v >= 100.0 => format!("{v:.0} {}", UNITS[u]),
        _ => format!("{v:.1} {}", UNITS[u]),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1500), "1.5 KB");
        assert_eq!(human(3_400_000), "3.4 MB");
        assert_eq!(human(250_000_000), "250 MB");
    }
}
