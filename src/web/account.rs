//! `/account`: who is signed in, and changing their own password.
//!
//! A cut-down kōan account page. Changing the password needs the current one,
//! and every try counts against a budget of the page's own, per account. The browser it is changed from stays signed in with a new
//! session; every other session of the account ends, since a new password is
//! how someone shuts out whoever else had the old one. A read-only account
//! changes its own password too: it is the one write it may make.

use askama::Template;
use askama_datastar::Html;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use koan_auth::accounts::AccountError;
use koan_auth::password::Refused;
use koan_kit_web as kit;

use super::AppState;
use super::Helpers as _;
use super::app::{Shell, shell};
use crate::auth::cookie::{SESSION_COOKIE, session_id, set_cookie};
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
    result: Html,
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
    /// The session ended, or the password changed elsewhere, meanwhile.
    Lapsed,
}

#[derive(Template)]
#[template(path = "app/password.html")]
struct PasswordResult {
    outcome: Outcome,
}

crate::i18n::translated!(AccountPage, PasswordResult);

fn result(outcome: Outcome) -> AppResult<Html> {
    Html::render(&PasswordResult { outcome }).map_err(|e| AppError::Internal(e.into()))
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
    Ok(Html::render(&page)
        .map_err(|e| AppError::Internal(e.into()))?
        .into_response())
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

/// The answer: the outcome, the form's passwords emptied once it worked, and
/// the new session's cookie when there is one.
fn answer(outcome: Outcome, secret: Option<&str>) -> AppResult<Response> {
    let mut events = vec![result(outcome)?.patch().into()];
    if outcome == Outcome::Changed {
        events.push(
            askama_datastar::PatchSignals::new(r#"{"_current":"","_password":"","_confirm":""}"#)
                .into(),
        );
    }
    let mut response = askama_datastar::events(events);
    if let Some(secret) = secret {
        // Every other session has ended: without this one, nor would this
        // browser stay signed in.
        let cookie = HeaderValue::from_str(&set_cookie(
            SESSION_COOKIE,
            secret,
            SESSION_TTL.as_secs() as i64,
        ))
        .map_err(|e| AppError::Internal(e.into()))?;
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(response)
}

pub async fn change_password(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Form(new): Form<NewPassword>,
) -> AppResult<Response> {
    if new.password != new.confirm {
        return answer(Outcome::Mismatch, None);
    }
    if koan_auth::accounts::valid_password(&new.password).is_err() {
        return answer(Outcome::Short, None);
    }
    // Every try counts, on a budget of this page's own: each costs two argon2
    // runs, and a budget shared with sign-in would let someone failing to
    // sign in as this account stop its owner changing the password.
    if state.password_changes.exhausted(&session.user_id) {
        return answer(Outcome::TooMany, None);
    }
    state.password_changes.record(session.user_id);
    let Some(secret) = session_id(&headers).map(str::to_string) else {
        return answer(Outcome::Lapsed, None);
    };
    let user = state.db.account_named(&session.username).await?;
    let checked_hash = user.as_ref().map(|u| u.password_hash.clone());
    let checking = state.clone();
    let (name, current) = (session.username.clone(), new.current);
    let checked =
        tokio::task::spawn_blocking(move || checking.verifier.verify(&name, &current, user))
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    match checked {
        Ok(_) => {}
        Err(Refused::Wrong) => return answer(Outcome::Wrong, None),
        Err(Refused::Busy) => return answer(Outcome::Busy, None),
    }
    let Some(checked_hash) = checked_hash else {
        return answer(Outcome::Lapsed, None);
    };
    match state
        .db
        .change_password(
            session.user_id,
            &checked_hash,
            &secret,
            &new.password,
            SESSION_TTL,
        )
        .await
    {
        Ok(secret) => {
            tracing::info!(username = %session.username, "password changed");
            answer(Outcome::Changed, Some(&secret))
        }
        Err(AccountError::ShortPassword) => answer(Outcome::Short, None),
        Err(AccountError::NoSuchUser(_)) => answer(Outcome::Lapsed, None),
        Err(e) => Err(AppError::Internal(e.into())),
    }
}
