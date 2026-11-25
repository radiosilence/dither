//! `/account`: who is signed in, and changing their own password.
//!
//! A cut-down kōan account page. Changing the password needs the current one,
//! and a wrong one counts against the account's budget of failures, as a
//! sign-in does. The browser it is changed from stays signed in with a new
//! session; every other session of the account ends, since a new password is
//! how someone shuts out whoever else had the old one. A read-only account
//! changes its own password too: it is the one write it may make.

use askama::Template;
use askama_gettext::Translate as _;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use koan_auth::accounts::AccountError;
use koan_auth::password::Refused;

use super::AppState;
use super::Helpers as _;
use super::app::{Shell, shell};
use crate::auth::cookie::{SESSION_COOKIE, set_cookie};
use crate::auth::session::{SESSION_TTL, Session};
use crate::error::{AppError, AppResult};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/account", get(page))
        .route("/account/password", post(change_password))
}

#[derive(Template)]
#[template(path = "app/account.html")]
struct AccountPage {
    shell: Shell,
    role: &'static str,
    result: String,
}

/// How a change of password went. Each is said in the template's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Nothing,
    Changed,
    Wrong,
    Mismatch,
    Short,
    TooMany,
    Busy,
}

#[derive(Template)]
#[template(path = "app/password.html")]
struct PasswordResult {
    outcome: Outcome,
}

crate::i18n::translated!(AccountPage, PasswordResult);

fn result(outcome: Outcome) -> AppResult<String> {
    PasswordResult { outcome }
        .render()
        .map_err(|e| AppError::Internal(e.into()))
}

pub async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> AppResult<Response> {
    let page = AccountPage {
        role: session.role.as_str(),
        result: result(Outcome::Nothing)?,
        shell: shell(&state, &session).await,
    };
    Ok(Html(page.render().map_err(|e| AppError::Internal(e.into()))?).into_response())
}

#[derive(serde::Deserialize)]
pub struct NewPassword {
    #[serde(default)]
    current: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

/// The answer: the outcome, the form emptied once it worked, and the new
/// session's cookie when there is one.
fn answer(outcome: Outcome, secret: Option<&str>) -> AppResult<Response> {
    let mut events = vec![
        askama_datastar::Html::trusted(result(outcome)?)
            .patch()
            .into(),
    ];
    if outcome == Outcome::Changed {
        events.push(
            askama_datastar::PatchSignals::new(r#"{"current":"","password":"","confirm":""}"#)
                .into(),
        );
    }
    let mut response = askama_datastar::events(events);
    if let Some(secret) = secret
        && let Ok(cookie) = HeaderValue::from_str(&set_cookie(
            SESSION_COOKIE,
            secret,
            SESSION_TTL.as_secs() as i64,
        ))
    {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

pub async fn change_password(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Json(new): Json<NewPassword>,
) -> AppResult<Response> {
    if new.password != new.confirm {
        return answer(Outcome::Mismatch, None);
    }
    // Not a sign-in, so the networks the account signed in from are not
    // spared: a guesser on one of them would otherwise try without limit.
    if state.verifier.exhausted(&session.username) {
        return answer(Outcome::TooMany, None);
    }
    let user = state.db.account_named(&session.username).await?;
    let checking = state.clone();
    let (name, current) = (session.username.clone(), new.current);
    let checked =
        tokio::task::spawn_blocking(move || checking.verifier.verify(&name, &current, user))
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    match checked {
        Ok(_) => {}
        Err(Refused::Wrong) => {
            state.verifier.failed(&session.username);
            return answer(Outcome::Wrong, None);
        }
        Err(Refused::Busy) => return answer(Outcome::Busy, None),
    }
    match state
        .db
        .change_password(session.user_id, &new.password, SESSION_TTL)
        .await
    {
        Ok(secret) => {
            tracing::info!(username = %session.username, "password changed");
            answer(Outcome::Changed, Some(&secret))
        }
        Err(AccountError::ShortPassword) => answer(Outcome::Short, None),
        Err(e) => Err(AppError::Internal(e.into())),
    }
}
