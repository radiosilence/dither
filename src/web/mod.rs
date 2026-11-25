//! HTTP: the app behind sign-in, the public pages and image files, and the
//! JSON the uploader speaks.
//!
//! One router serves both hosts. While the public pages share the app's
//! hostname that is the whole story; once they have a domain of their own,
//! requests for that domain reach the public routes only, and it never sets
//! or receives a cookie. Every route behind sign-in sits under one session
//! layer, so a route added there later is protected by where it is added.

pub mod account;
pub mod api;
pub mod app;
pub mod assets;
pub mod clip;
pub mod public;
pub mod users;
pub mod work;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};

use crate::config::Config;
use crate::db::Db;
use crate::store::Store;
use crate::upload::Uploads;

pub struct Inner {
    pub config: Config,
    pub db: Db,
    pub store: Store,
    /// Every password checked goes through this, with its ceiling on argon2
    /// and its budget of failures per account.
    pub verifier: koan_auth::password::Verifier,
    /// Posts to the sign-in pages, per address.
    pub sign_ins: koan_auth::limit::RateLimiter,
    /// Tries at changing a password, per account, whatever their outcome:
    /// each is two argon2 runs, one under the write lock.
    pub password_changes: koan_auth::limit::FailureLimiter<i64>,
    /// The code `/setup` asks for, logged at start while there is no
    /// account; `None` once there was one then.
    pub setup: Option<koan_auth::setup::SetupCode>,
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
        .merge(users::router())
        .layer(middleware::from_fn(crate::auth::extract::require_writer))
        // Its own password is a readonly account's one write.
        .merge(account::router())
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
        .route("/assets/datastar.js", get(askama_datastar::script::serve))
        .route("/assets/{name}", get(assets::serve))
        .fallback(public::not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
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
        .layer(middleware::from_fn_with_state(state.clone(), count))
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
/// ([`crate::views::Counted`]), unless the album's owner asked for it. GET
/// only: a HEAD is answered by the same handlers and carries nothing. The
/// outermost layer, so the bytes counted are the ones sent, compressed or
/// not.
///
/// The client's address ([`forwarded`], else the peer's) goes into the
/// visitor's hash and nowhere else.
async fn count(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request.method() != Method::GET {
        return next.run(request).await;
    }
    let h = request.headers();
    let text = |name| {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    let session = crate::auth::cookie::session_id(h).map(str::to_string);
    let agent = text(header::USER_AGENT).to_string();
    let forwarded = forwarded(&state.config, h);
    let own = [
        crate::views::host_only(state.config.app_host()),
        crate::views::host_only(state.config.public_host()),
    ];
    let from = crate::views::source(text(header::REFERER), &own);
    let dest = h
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let peer = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_string());
    let response = next.run(request).await;
    let Some(counted) = response
        .extensions()
        .get::<crate::views::Counted>()
        .cloned()
    else {
        return response;
    };
    if let Some(id) = session
        && state
            .db
            .holder(&id)
            .await
            .ok()
            .flatten()
            .is_some_and(|u| u.username == counted.owner)
    {
        return response;
    }
    let visit = crate::views::Visit {
        address: forwarded.or(peer).unwrap_or_default(),
        fetcher: fetcher(&agent),
        agent,
        from,
        dest,
    };
    let (mut parts, body) = response.into_parts();
    // Counting streams the body, which would otherwise lose its length. A
    // 304 or 204 has none to keep, and says nothing of the stored copy's.
    if !matches!(
        parts.status,
        StatusCode::NOT_MODIFIED | StatusCode::NO_CONTENT
    ) && !parts.headers.contains_key(header::CONTENT_LENGTH)
        && let Some(n) = axum::body::HttpBody::size_hint(&body).exact()
    {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(n));
    }
    Response::from_parts(parts, state.views.counting(body, counted, visit))
}

/// The client's address as `X-Forwarded-For` gives it: the last entry outside
/// the trusted proxies' ranges. Traefik appends the address it saw, and a CDN
/// in front (trusted) appends the client's before that, so whatever a client
/// sends sits to the left of both and is never chosen. Where the last entry is
/// a trusted proxy's, the address header it set comes first. `None` for a
/// request that came with no proxy.
fn forwarded(config: &Config, h: &HeaderMap) -> Option<String> {
    let trusted = |a: &str| {
        a.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| config.trusted_proxies.iter().any(|net| net.contains(&ip)))
    };
    let hops: Vec<&str> = h
        .get_all("x-forwarded-for")
        .iter()
        .rev()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.rsplit(','))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .collect();
    let given = match (&config.address_header, hops.first()) {
        (Some(name), Some(last)) if trusted(last) => h
            .get_all(name)
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|a| a.parse::<std::net::IpAddr>().is_ok()),
        _ => None,
    };
    given
        .or_else(|| hops.iter().copied().find(|a| !trusted(a)))
        .map(str::to_string)
}

/// The address a request came from, for limits on what one address may try.
/// What the proxies say ([`forwarded`]) is believed only from a peer that can
/// be a proxy in front of dither: a private or loopback address (an ingress
/// in the cluster, a tunnel on the host) or a trusted proxy's. Anyone else
/// could name any address in the header, and take a fresh allowance, or an
/// account's own network, with every try. Unspecified with no peer, as in a
/// test, which then shares one allowance.
pub fn client_ip(
    config: &Config,
    h: &HeaderMap,
    extensions: &axum::http::Extensions,
) -> std::net::IpAddr {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip().to_canonical());
    let proxy = peer.is_some_and(|ip| {
        let internal = match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
            std::net::IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        };
        internal || config.trusted_proxies.iter().any(|net| net.contains(&ip))
    });
    proxy
        .then(|| forwarded(config, h))
        .flatten()
        .and_then(|a| a.parse::<std::net::IpAddr>().ok())
        .or(peer)
        .map_or(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            |ip| ip.to_canonical(),
        )
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
/// request forgery twice over. A variation of
/// `askama_datastar::require_datastar_on_post`: it also admits the uploader's
/// header and covers every method that writes, not only POST.
async fn marked_writes(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    let marked = askama_datastar::is_datastar(request.headers())
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

    fn join_js_url(&self) -> &'static str {
        assets::join_js_url()
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
    fn a_proxys_address_is_believed_only_from_a_peer_that_can_be_one() {
        let mut config = Config::test(std::path::PathBuf::new());
        config.trusted_proxies = vec!["198.51.100.0/24".parse().unwrap()];
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "192.0.2.7".parse().unwrap());
        let from = |peer: Option<&str>| {
            let mut ext = axum::http::Extensions::new();
            if let Some(peer) = peer {
                ext.insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
                    peer.parse().unwrap(),
                    443,
                )));
            }
            client_ip(&config, &h, &ext).to_string()
        };
        // A client reaching dither directly names whatever it likes.
        assert_eq!(from(Some("203.0.113.9")), "203.0.113.9");
        // An ingress in the cluster, a tunnel on the host, a trusted proxy.
        assert_eq!(from(Some("10.42.0.7")), "192.0.2.7");
        assert_eq!(from(Some("127.0.0.1")), "192.0.2.7");
        assert_eq!(from(Some("198.51.100.5")), "192.0.2.7");
        assert_eq!(from(None), "0.0.0.0");
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1500), "1.5 KB");
        assert_eq!(human(3_400_000), "3.4 MB");
        assert_eq!(human(250_000_000), "250 MB");
    }
}
