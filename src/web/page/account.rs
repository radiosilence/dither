//! Account, at `/account`: who is signed in, and changing their own
//! password.
//!
//! A cut-down kōan account page. Changing the password needs the current one,
//! and every try counts against a budget of the page's own, per account. The
//! browser it is changed from stays signed in with a new session; every other
//! session of the account ends, since a new password is how someone shuts out
//! whoever else had the old one. A read-only account changes its own password
//! too: it is the one write it may make.

use askama::Template;
use askama_datastar::{Html, PatchSignals};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use axum::{Extension, Form};
use koan_auth::Role;
use koan_auth::accounts::AccountError;
use koan_auth::password::Refused;
use koan_kit_web as kit;

use super::{Body, Page};
use crate::auth::cookie::{SESSION_COOKIE, session_id, set_cookie};
use crate::auth::session::{SESSION_TTL, Session};
use crate::error::{AppError, AppResult};
use crate::web::layout::{Draw, Name};
use crate::web::{AppState, paths};

/// The signals the password fields are bound to: the page's own, never sent,
/// so a changed password's answer can empty them.
pub(in crate::web) const CURRENT: &str = "_current";
pub(in crate::web) const PASSWORD: &str = "_password";
pub(in crate::web) const CONFIRM: &str = "_confirm";

pub(in crate::web) struct Account;

#[derive(Template)]
#[template(path = "account/page.html")]
struct AccountPage<'a> {
    login: &'a str,
    role: Role,
    outcome: Html,
}

impl AccountPage<'_> {
    fn logout(&self) -> &'static str {
        paths::LOGOUT
    }
    fn password_url(&self) -> &'static str {
        paths::ACCOUNT_PASSWORD
    }
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
#[template(path = "account/outcome.html")]
struct Result {
    outcome: Outcome,
}

crate::i18n::translated!(AccountPage<'_>, Result);

impl Page for Account {
    const NAME: Name = Name::Account;
    const STREAM: &'static str = paths::STREAM_QUIET;
    type Args = ();

    async fn body(_: &AppState, session: &Session, (): ()) -> AppResult<Body> {
        Ok(Body::of(&AccountPage {
            login: &session.username,
            role: session.role,
            outcome: Result {
                outcome: Outcome::Nothing,
            }
            .draw(),
        }))
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct NewPassword {
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
    let mut events = vec![Result { outcome }.draw().patch().into()];
    if outcome == Outcome::Changed {
        let emptied: serde_json::Map<_, _> = [CURRENT, PASSWORD, CONFIRM]
            .into_iter()
            .map(|name| (name.to_string(), serde_json::Value::from("")))
            .collect();
        events.push(PatchSignals::new(serde_json::Value::from(emptied).to_string()).into());
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

pub(in crate::web) async fn change_password(
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
