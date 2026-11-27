//! The layers every response passes through: counting what the public
//! routes served, logging what link previews' fetchers asked, refusing an
//! unmarked write, the headers that harden every answer, and keeping the app
//! off the public domain.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::client::{fetcher, forwarded, on_public_host};

/// Count what a public route served, as its handler marked the response
/// ([`crate::views::Counted`]), unless the album's owner asked for it. GET
/// only: a HEAD is answered by the same handlers and carries nothing. The
/// outermost layer, so the bytes counted are the ones sent, compressed or
/// not.
///
/// The client's address ([`forwarded`], else the peer's) goes into the
/// visitor's hash and nowhere else.
pub async fn count(State(state): State<AppState>, request: Request, next: Next) -> Response {
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

/// What a link preview's fetcher asked for and was answered, logged: a
/// preview that does not appear is found out from what its fetcher saw.
pub async fn preview_log(request: Request, next: Next) -> Response {
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
pub async fn marked_writes(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    let marked = askama_datastar::is_datastar(request.headers())
        || request.headers().contains_key("x-dither");
    if !safe && !marked {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// What every response gets beyond [`SecurityHeaders`]' policy, `nosniff`
/// and referrer policy: no framing, even where a route set a policy of its
/// own (the public pages, the files), no powerful features, and HTTPS from
/// here on.
pub async fn hardening(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
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

/// The app's routes on the public domain: not found, as if they were not
/// there.
pub async fn app_host_only(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if on_public_host(&state.config, request.headers()) {
        return super::public::files::not_found().await.into_response();
    }
    next.run(request).await
}
