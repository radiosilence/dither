//! `/auth/login`, `/auth/callback`, `/auth/logout`.

use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::auth::cookie::{
    FLOW_COOKIE, SESSION_COOKIE, clear_cookie, read_cookie, session_id, set_cookie,
};
use crate::auth::oidc::{authorize_url, exchange_code, pkce};
use crate::auth::session::Sessions;
use crate::error::{AppError, AppResult};
use crate::web::AppState;

/// How long a sign-in lasts: a month, since signing in again from a phone in
/// the middle of sharing something is the worst moment for it.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// Long enough to finish a sign-in round trip and no longer.
pub const FLOW_TTL: Duration = Duration::from_secs(600);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", get(logout))
}

/// Where to sign in so as to come back to `path`.
pub fn login_url(path: &str) -> String {
    if path == "/" {
        return "/auth/login".into();
    }
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("next", path)
        .finish();
    format!("/auth/login?{query}")
}

/// A path on this site to return to: anything else would make the sign-in
/// page an open redirect. Browsers read a backslash as a slash and drop tabs
/// and newlines from a URL before parsing it, so `/\t/evil.example` is
/// `//evil.example`; any such character, or any whitespace at all, refuses
/// the path, rather than trying to predict what a browser makes of it.
fn local(next: Option<&str>) -> String {
    match next {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p
                    .chars()
                    .any(|c| c == '\\' || c.is_whitespace() || c.is_control()) =>
        {
            p.to_string()
        }
        _ => "/".into(),
    }
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

#[derive(serde::Deserialize)]
pub struct Login {
    next: Option<String>,
}

async fn login(State(state): State<AppState>, Query(q): Query<Login>) -> AppResult<Response> {
    let next = local(q.next.as_deref());
    let Some(oidc) = state.config.oidc.as_ref() else {
        // Nothing to sign in to: everyone is already "dev".
        return Ok(redirect(&next, &[]));
    };
    let (verifier, challenge) = pkce();
    let csrf = Sessions::token();
    let flow = state.sessions.begin_flow(&verifier, &csrf, &next, FLOW_TTL);
    let url = authorize_url(oidc, &state.config.redirect_uri(), &csrf, &challenge);
    Ok(redirect(
        &url,
        &[set_cookie(FLOW_COOKIE, &flow, FLOW_TTL.as_secs() as i64)],
    ))
}

#[derive(serde::Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> AppResult<Response> {
    let oidc = state.config.oidc.as_ref().ok_or(AppError::Unauthorized)?;
    let flow_id = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| read_cookie(h, FLOW_COOKIE))
        .ok_or(AppError::Unauthorized)?;
    let flow = state
        .sessions
        .take_flow(flow_id)
        .ok_or(AppError::Unauthorized)?;
    if flow.csrf != q.state {
        return Err(AppError::BadRequest("state mismatch".into()));
    }
    let who = exchange_code(
        &state.http,
        oidc,
        &state.config.redirect_uri(),
        &q.code,
        &flow.verifier,
    )
    .await?;
    state.db.upsert_user(&who.sub, &who.login).await?;
    let session = state.sessions.create(&who.sub, SESSION_TTL).await?;
    tracing::info!(login = %who.login, "signed in");
    Ok(redirect(
        &flow.next,
        &[
            set_cookie(SESSION_COOKIE, &session, SESSION_TTL.as_secs() as i64),
            clear_cookie(FLOW_COOKIE),
        ],
    ))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Another site's link or script cannot sign anyone out: browsers say
    // where a navigation came from. One that says nothing, an older browser,
    // is let through.
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return redirect("/", &[]);
    }
    if let Some(id) = session_id(&headers) {
        state.sessions.delete(id).await;
    }
    redirect("/", &[clear_cookie(SESSION_COOKIE)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_local_path_is_returned_to() {
        assert_eq!(local(Some("/albums/abc?x=1")), "/albums/abc?x=1");
        assert_eq!(local(Some("//evil.example/x")), "/");
        assert_eq!(local(Some("https://evil.example")), "/");
        assert_eq!(local(Some("/\\evil.example")), "/");
        // Browsers strip these before parsing, leaving `//evil.example`.
        for sneaky in [
            "/\t/evil.example",
            "/\n/evil.example",
            "/\r/evil.example",
            "/ /evil.example",
            "/\u{0}/x",
        ] {
            assert_eq!(local(Some(sneaky)), "/", "{sneaky:?}");
        }
        assert_eq!(local(None), "/");
    }

    #[test]
    fn the_login_url_carries_where_to_return() {
        assert_eq!(login_url("/"), "/auth/login");
        assert_eq!(
            login_url("/albums/a?b=c"),
            "/auth/login?next=%2Falbums%2Fa%3Fb%3Dc"
        );
    }
}
