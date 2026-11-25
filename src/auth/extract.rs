//! The gates in front of the app.
//!
//! [`require_session`] is a layer over the whole protected router rather than a
//! check in each handler: a route added later is protected by being added
//! there, and an unprotected one has to be mounted somewhere else on purpose.
//! The layer puts the [`Session`] in the request's extensions for handlers to
//! read. Inside it, [`require_writer`] refuses a readonly account's writes
//! everywhere but its own account's page, and [`require_admin`] goes over the
//! admins' routes.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::auth::cookie::session_id;
use crate::auth::session::Session;
use crate::web::AppState;

/// Whether an unauthenticated request should be sent to sign in or refused.
///
/// A page load wants the sign-in page. Anything the page issues itself wants
/// a status: Datastar follows redirects like any fetch, so a 302 on a fragment
/// request would swap the sign-in page into the middle of the app, and an
/// event stream cannot be redirected at all.
pub fn is_page_load(headers: &HeaderMap) -> bool {
    !askama_datastar::is_datastar(headers)
        && !headers.contains_key("x-dither")
        && headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_none_or(|a| a.contains("text/html"))
}

/// Who is asking, if anyone. `None` too when the database cannot be asked: a
/// sign-in page is the safe answer to not knowing.
pub async fn current(state: &AppState, headers: &HeaderMap) -> Option<Session> {
    let secret = session_id(headers)?;
    let mark = koan_auth::changes::account_mark();
    state
        .db
        .holder(secret)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "session lookup failed"))
        .ok()
        .flatten()
        .map(|user| Session::new(user, mark))
}

pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    match current(&state, request.headers()).await {
        Some(session) => {
            request.extensions_mut().insert(session);
            next.run(request).await
        }
        None if is_page_load(request.headers()) => {
            let path = request.uri().path_and_query().map_or("/", |p| p.as_str());
            Redirect::to(&crate::auth::routes::login_url(path)).into_response()
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// No writes from a readonly account, behind [`require_session`].
pub async fn require_writer(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    match request.extensions().get::<Session>() {
        Some(session) if safe || session.writes() => next.run(request).await,
        _ => StatusCode::FORBIDDEN.into_response(),
    }
}

/// Admins only, behind [`require_session`].
pub async fn require_admin(request: Request, next: Next) -> Response {
    match request.extensions().get::<Session>() {
        Some(session) if session.admin() => next.run(request).await,
        _ => StatusCode::FORBIDDEN.into_response(),
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
