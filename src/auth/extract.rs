//! The gate in front of the app.
//!
//! [`require_session`] is a layer over the whole protected router rather than a
//! check in each handler: a route added later is protected by being added
//! there, and an unprotected one has to be mounted somewhere else on purpose.
//! The layer puts the [`Session`] in the request's extensions for handlers to
//! read.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::auth::cookie::session_id;
use crate::auth::session::Session;
use crate::web::AppState;

/// Whether an unauthenticated request should be sent to sign in or refused.
///
/// A page load wants the sign-in round trip. Anything the page issues itself
/// wants a status: Datastar follows redirects like any fetch, so a 302 on a
/// fragment request would swap the sign-in page into the middle of the app,
/// and an event stream cannot be redirected at all.
pub fn is_page_load(headers: &HeaderMap) -> bool {
    !headers.contains_key("datastar-request")
        && !headers.contains_key("x-dither")
        && headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|a| a.contains("text/html"))
}

/// Who is asking, if anyone. Without an identity provider (local development
/// only; the config refuses it on anything but loopback) everyone is "dev".
pub async fn current(state: &AppState, headers: &HeaderMap) -> Option<Session> {
    match session_id(headers) {
        Some(id) => state.sessions.get(id).await,
        None if state.config.oidc.is_none() => Some(Session {
            sub: "dev".into(),
            since: crate::db::now(),
        }),
        None => None,
    }
}

/// Whether a page load for `session` goes back through the identity provider
/// first: a day after it vouched for them, so that whoever it no longer lets
/// in stops getting in here within a day. The provider remembers the person
/// and asks nothing, so a page load is all it costs. What a page already
/// open is doing (an upload, a live stream) is not interrupted: the session
/// lasts a day beyond this ([`crate::auth::routes::SESSION_TTL`]).
pub fn stale(state: &AppState, session: &Session) -> bool {
    state.config.oidc.is_some() && session.age() >= crate::auth::routes::REVERIFY
}

pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let page = is_page_load(request.headers());
    match current(&state, request.headers()).await {
        Some(session) if !(page && stale(&state, &session)) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        _ if page => {
            let path = request.uri().path_and_query().map_or("/", |p| p.as_str());
            Redirect::to(&crate::auth::routes::login_url(path)).into_response()
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn only_a_browser_asking_for_a_page_is_a_page_load() {
        assert!(is_page_load(&headers(&[(
            "accept",
            "text/html,application/xhtml+xml"
        )])));
        assert!(is_page_load(&headers(&[])));
        assert!(!is_page_load(&headers(&[("datastar-request", "true")])));
        assert!(!is_page_load(&headers(&[("x-dither", "1")])));
        assert!(!is_page_load(&headers(&[("accept", "text/event-stream")])));
        assert!(!is_page_load(&headers(&[("accept", "application/json")])));
    }
}
