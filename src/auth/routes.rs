//! `/login`, `/setup`, `/join` and `/auth/logout`: the pages that sign
//! someone in, and the link that signs them out.
//!
//! Each page's form posts back to it. A form cannot carry the header the
//! app's writes need, so these posts prove they came from this origin the way
//! a browser does (`koan_auth::web::same_origin`). Every password goes through
//! the one verifier (`koan_auth::password::Verifier`), with its ceiling on
//! argon2 and its budget of failures per account, and every post counts
//! against its address's allowance.

use axum::Form;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use askama::Template;
use koan_auth::accounts::AccountError;
use koan_auth::password::Refused;
use koan_kit_web as kit;

use crate::auth::cookie::{SESSION_COOKIE, clear_cookie, session_id, set_cookie};
use crate::auth::session::SESSION_TTL;
use crate::error::{AppError, AppResult};
use crate::web::AppState;
use crate::web::Helpers as _;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_form).post(login))
        .route("/setup", get(setup_form).post(setup))
        .route("/join", get(join_form).post(join))
        .route("/auth/logout", get(logout))
}

/// Where to sign in so as to come back to `path`.
pub fn login_url(path: &str) -> String {
    if path == "/" {
        return "/login".into();
    }
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("next", path)
        .finish();
    format!("/login?{query}")
}

/// A path on this site to return to: anything else would make the sign-in
/// page an open redirect (`koan_auth::web::local_path`), and these pages
/// themselves would loop.
fn local(next: Option<&str>) -> String {
    next.and_then(koan_auth::web::local_path)
        .filter(|p| {
            !["/login", "/setup", "/join", "/auth/"]
                .iter()
                .any(|own| p.starts_with(own))
        })
        .unwrap_or("/")
        .to_string()
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response();
    for cookie in cookies {
        if let Ok(value) = cookie.parse() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

/// Signed in with the session `secret` opens: its cookie, and the end of any
/// session the browser held before.
async fn signed_in(state: &AppState, headers: &HeaderMap, secret: &str, to: &str) -> Response {
    if let Some(old) = session_id(headers)
        && let Err(e) = state.db.end_session(old).await
    {
        tracing::warn!(error = %e, "ending the session a sign-in replaced failed");
    }
    redirect(
        to,
        &[set_cookie(
            SESSION_COOKIE,
            secret,
            SESSION_TTL.as_secs() as i64,
        )],
    )
}

/// A post from another site: a status alone, since words here would be
/// outside the templates, for a request nobody meant to make.
fn cross_site() -> Response {
    StatusCode::FORBIDDEN.into_response()
}

/// Why a sign-in page is shown again. Each is said in the template's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    Wrong,
    TooMany,
    Busy,
    WrongCode,
    Mismatch,
    Short,
    BadUsername,
    BadInvite,
}

impl Problem {
    fn status(self) -> StatusCode {
        match self {
            Problem::Wrong => StatusCode::UNAUTHORIZED,
            Problem::TooMany => StatusCode::TOO_MANY_REQUESTS,
            Problem::Busy => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    /// The problem an account's refusal is, or the failure to report.
    fn of(e: AccountError) -> Result<Self, AppError> {
        match e {
            AccountError::ShortPassword => Ok(Problem::Short),
            AccountError::BadUsername | AccountError::Reserved(_) => Ok(Problem::BadUsername),
            AccountError::BadInvite | AccountError::NoSuchUser(_) => Ok(Problem::BadInvite),
            e => Err(AppError::Internal(e.into())),
        }
    }
}

#[derive(Template)]
#[template(path = "sign/login.html")]
struct LoginPage {
    next: String,
    username: String,
    problem: Option<Problem>,
}

#[derive(Template)]
#[template(path = "sign/setup.html")]
struct SetupPage {
    username: String,
    problem: Option<Problem>,
}

#[derive(Template)]
#[template(path = "sign/join.html")]
struct JoinPage {
    /// Put back after a refused post; the page's script fills it, and the
    /// username, from the link the first time.
    invite: String,
    username: String,
    problem: Option<Problem>,
}

crate::i18n::translated!(LoginPage, SetupPage, JoinPage);

fn page(template: &impl Template, problem: Option<Problem>) -> AppResult<Response> {
    let html = template
        .render()
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok((
        problem.map_or(StatusCode::OK, Problem::status),
        [(header::CACHE_CONTROL, "no-store")],
        axum::response::Html(html),
    )
        .into_response())
}

#[derive(serde::Deserialize)]
pub struct Next {
    next: Option<String>,
}

async fn login_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Next>,
) -> AppResult<Response> {
    let next = local(q.next.as_deref());
    if crate::auth::extract::current(&state, &headers)
        .await
        .is_some()
    {
        return Ok(redirect(&next, &[]));
    }
    // Nobody to sign in as yet: the first admin comes first.
    if !state.db.has_accounts().await? {
        return Ok(redirect("/setup", &[]));
    }
    page(
        &LoginPage {
            next,
            username: String::new(),
            problem: None,
        },
        None,
    )
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    next: String,
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(f): Form<LoginForm>,
) -> AppResult<Response> {
    if !koan_auth::web::same_origin(&headers) {
        return Ok(cross_site());
    }
    let next = local(Some(&f.next));
    let username = f.username.trim().to_string();
    let refused = |problem| {
        page(
            &LoginPage {
                next: next.clone(),
                username: username.clone(),
                problem: Some(problem),
            },
            Some(problem),
        )
    };
    let from = crate::web::client_ip(&state.config, &headers, &extensions);
    if !state.sign_ins.allow(from) || state.verifier.spent(&username, from) {
        return refused(Problem::TooMany);
    }
    let user = state.db.account_named(&username).await?;
    let checking = state.clone();
    let (name, password) = (username.clone(), f.password);
    let checked =
        tokio::task::spawn_blocking(move || checking.verifier.verify(&name, &password, user))
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    match checked {
        Ok(user) => {
            state.verifier.signed_in(&username, from);
            let secret = state.db.open_session(user.id, SESSION_TTL).await?;
            tracing::info!(username = %user.username, "signed in");
            Ok(signed_in(&state, &headers, &secret, &next).await)
        }
        Err(Refused::Wrong) => {
            state.verifier.failed(&username);
            refused(Problem::Wrong)
        }
        Err(Refused::Busy) => refused(Problem::Busy),
    }
}

async fn setup_form(State(state): State<AppState>) -> AppResult<Response> {
    if state.db.has_accounts().await? {
        return Ok(redirect("/login", &[]));
    }
    page(
        &SetupPage {
            username: String::new(),
            problem: None,
        },
        None,
    )
}

#[derive(serde::Deserialize)]
pub struct SetupForm {
    code: String,
    username: String,
    password: String,
    confirm: String,
}

/// The first admin, made with the code the server logged at start. Whoever
/// reached an empty server first would otherwise become its admin, and its
/// database can be empty for worse reasons than a new install, such as a
/// bucket with no copy to restore.
async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(f): Form<SetupForm>,
) -> AppResult<Response> {
    if !koan_auth::web::same_origin(&headers) {
        return Ok(cross_site());
    }
    if state.db.has_accounts().await? {
        return Ok(redirect("/login", &[]));
    }
    let refused = |problem| {
        page(
            &SetupPage {
                username: f.username.clone(),
                problem: Some(problem),
            },
            Some(problem),
        )
    };
    let from = crate::web::client_ip(&state.config, &headers, &extensions);
    if !state.sign_ins.allow(from) {
        return refused(Problem::TooMany);
    }
    if !state
        .setup
        .as_ref()
        .is_some_and(|code| code.matches(&f.code))
    {
        return refused(Problem::WrongCode);
    }
    if f.password != f.confirm {
        return refused(Problem::Mismatch);
    }
    match state
        .db
        .create_first_admin(&f.username, &f.password, SESSION_TTL)
        .await
    {
        Ok((user, secret)) => {
            tracing::warn!(username = %user.username, %from, "the first admin was made through /setup");
            Ok(signed_in(&state, &headers, &secret, "/").await)
        }
        // Someone else finished first: theirs is the server now.
        Err(AccountError::AlreadySetUp) => Ok(redirect("/login", &[])),
        Err(e) => refused(Problem::of(e)?),
    }
}

async fn join_form() -> AppResult<Response> {
    page(
        &JoinPage {
            invite: String::new(),
            username: String::new(),
            problem: None,
        },
        None,
    )
}

#[derive(serde::Deserialize)]
pub struct JoinForm {
    #[serde(default)]
    invite: String,
    password: String,
    confirm: String,
}

/// An invite's account gets the password its owner chose, which spends the
/// invite, and is signed in.
async fn join(
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    Form(f): Form<JoinForm>,
) -> AppResult<Response> {
    if !koan_auth::web::same_origin(&headers) {
        return Ok(cross_site());
    }
    let public = state.config.signing_key.public.clone();
    // Whose account it is, from the invite rather than the form, for the
    // page shown again.
    let username = state
        .db
        .invited(f.invite.trim(), public.clone())
        .await
        .map(|user| user.username)
        .unwrap_or_default();
    let refused = |problem| {
        page(
            &JoinPage {
                invite: f.invite.clone(),
                username: username.clone(),
                problem: Some(problem),
            },
            Some(problem),
        )
    };
    let from = crate::web::client_ip(&state.config, &headers, &extensions);
    if !state.sign_ins.allow(from) {
        return refused(Problem::TooMany);
    }
    if f.password != f.confirm {
        return refused(Problem::Mismatch);
    }
    match state
        .db
        .join(f.invite.trim(), public, &f.password, SESSION_TTL)
        .await
    {
        Ok((user, secret)) => {
            tracing::info!(username = %user.username, "joined from an invite");
            Ok(signed_in(&state, &headers, &secret, "/").await)
        }
        Err(e) => refused(Problem::of(e)?),
    }
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
    if let Some(id) = session_id(&headers)
        && let Err(e) = state.db.end_session(id).await
    {
        tracing::warn!(error = %e, "ending a session failed");
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
        // The sign-in pages themselves would loop.
        assert_eq!(local(Some("/login?next=/x")), "/");
        assert_eq!(local(Some("/auth/logout")), "/");
    }

    #[test]
    fn the_login_url_carries_where_to_return() {
        assert_eq!(login_url("/"), "/login");
        assert_eq!(
            login_url("/albums/a?b=c"),
            "/login?next=%2Falbums%2Fa%3Fb%3Dc"
        );
    }
}
