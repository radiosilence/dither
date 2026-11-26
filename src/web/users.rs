//! `/users`: the admins' page of accounts.
//!
//! An admin makes an account with a username and a role, and is given an
//! invite link to send its owner, who chooses a password with it. The
//! account's password until then is generated and told to nobody. Inviting an
//! account again sends a new link and changes nothing; resetting its password
//! signs it out everywhere and withdraws every link sent before, which is how
//! a link that went to the wrong place is taken back. Deleting an account
//! deletes its albums and their files. The last admin can be neither demoted
//! nor deleted (koan-auth's rules), and an admin does not delete themselves
//! here.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{Path, State};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use koan_auth::Role;
use koan_auth::accounts::AccountError;
use koan_kit_web as kit;

use super::Helpers as _;
use super::app::{Shell, shell};
use super::{AppState, human};
use crate::auth::session::Session;
use crate::error::{AppError, AppResult};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(page).post(create))
        .route("/users/{id}/invite", post(invite))
        .route("/users/{id}/reset", post(reset))
        .route("/users/{id}/role", post(set_role))
        .route("/users/{id}/delete", post(delete))
        .layer(middleware::from_fn(crate::auth::extract::require_admin))
}

struct AccountView {
    id: i64,
    username: String,
    role: &'static str,
    albums: u64,
    size: String,
    /// The admin looking.
    me: bool,
}

impl AccountView {
    /// The address of `action` on this account.
    fn url(&self, action: &str) -> String {
        format!("/users/{}/{action}", self.id)
    }
}

/// The roles to choose from, each saying `user`, `readonly` and `admin`: the
/// words are the template's.
pub fn roles(
    user: impl Into<kit::Text<'static>>,
    readonly: impl Into<kit::Text<'static>>,
    admin: impl Into<kit::Text<'static>>,
) -> [kit::Choice<'static>; 3] {
    [
        kit::Choice::new("user", user),
        kit::Choice::new("readonly", readonly),
        kit::Choice::new("admin", admin),
    ]
}

#[derive(Template)]
#[template(path = "app/users.html")]
struct UsersPage {
    shell: Shell,
    result: Html,
    list: Html,
}

#[derive(Template)]
#[template(path = "app/accounts.html")]
struct AccountList {
    accounts: Vec<AccountView>,
}

/// Why an account was not changed. Each is said in the template's words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Problem {
    Taken,
    BadUsername,
    LastAdmin,
    Yourself,
    Gone,
}

enum Shown {
    Nothing,
    Invite {
        username: String,
        link: String,
        /// The account's password was reset to send it.
        reset: bool,
    },
    Problem(Problem),
}

#[derive(Template)]
#[template(path = "app/invite.html")]
struct Outcome {
    shown: Shown,
}

/// A [`Problem`] in words, as markup for `kit::Confirm::error`.
#[derive(Template)]
#[template(path = "app/refused.html")]
struct Refusal {
    problem: Problem,
}

crate::i18n::translated!(UsersPage, AccountList, Outcome, Refusal);

fn render(t: &impl Template) -> AppResult<Html> {
    Html::render(t).map_err(|e| AppError::Internal(e.into()))
}

/// The problem an account's refusal is, or the failure to report.
fn problem(e: AccountError) -> AppResult<Problem> {
    match e {
        AccountError::Taken(_) => Ok(Problem::Taken),
        AccountError::BadUsername | AccountError::Reserved(_) => Ok(Problem::BadUsername),
        AccountError::LastAdmin => Ok(Problem::LastAdmin),
        AccountError::NoSuchUser(_) => Ok(Problem::Gone),
        e => Err(AppError::Internal(e.into())),
    }
}

async fn list(state: &AppState, session: &Session) -> AppResult<Html> {
    let accounts = state
        .db
        .accounts()
        .await?
        .into_iter()
        .map(|a| AccountView {
            me: a.id == session.user_id,
            id: a.id,
            username: a.username,
            role: a.role.as_str(),
            albums: a.albums,
            size: human(a.bytes),
        })
        .collect();
    render(&AccountList { accounts })
}

/// The link an account's owner joins with. The invite and the username ride
/// in the fragment, which browsers never send, so no log holds them.
fn link(state: &AppState, username: &str, token: &str) -> String {
    let fragment: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("invite", token)
        .append_pair("username", username)
        .finish();
    format!("{}/join#{fragment}", state.config.app_url)
}

fn outcome(shown: Shown) -> AppResult<Html> {
    render(&Outcome { shown })
}

/// An answer redrawing the outcome and, with `list`, the accounts.
fn answer(shown: Shown, list: Option<Html>) -> AppResult<Response> {
    let mut events = vec![outcome(shown)?.patch().into()];
    if let Some(list) = list {
        events.push(list.patch().into());
    }
    Ok(askama_datastar::events(events))
}

/// The answer to a change the page's question asked for: `shown`, and the
/// accounts with `list`, once the question is closed; a refusal inside the
/// question, which stays open.
fn asked(shown: Shown, list: Option<Html>) -> AppResult<Response> {
    if let Shown::Problem(problem) = shown {
        let why = render(&Refusal { problem })?;
        return Ok(askama_datastar::events([kit::Confirm::error(why).into()]));
    }
    let mut events = vec![kit::Confirm::close().into(), outcome(shown)?.patch().into()];
    if let Some(list) = list {
        events.push(list.patch().into());
    }
    Ok(askama_datastar::events(events))
}

async fn invited(state: &AppState, id: i64, reset: bool) -> AppResult<Shown> {
    match state
        .db
        .invite(id, state.config.signing_key.private.clone())
        .await
    {
        Ok((user, token)) => Ok(Shown::Invite {
            link: link(state, &user.username, &token),
            username: user.username,
            reset,
        }),
        Err(e) => Ok(Shown::Problem(problem(e)?)),
    }
}

pub async fn page(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
) -> AppResult<Response> {
    Ok(render(&UsersPage {
        result: render(&Outcome {
            shown: Shown::Nothing,
        })?,
        list: list(&state, &session).await?,
        shell: shell(&state, &session).await,
    })?
    .into_response())
}

#[derive(serde::Deserialize)]
pub struct NewAccount {
    #[serde(default)]
    newuser: String,
    #[serde(default)]
    newrole: String,
}

pub async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Form(new): Form<NewAccount>,
) -> AppResult<Response> {
    let role: Role = new
        .newrole
        .parse()
        .map_err(|_| AppError::BadRequest("unknown role".into()))?;
    let shown = match state.db.create_account(&new.newuser, role).await {
        Ok(user) => {
            tracing::info!(username = %user.username, %role, by = %session.username, "account made");
            invited(&state, user.id, false).await?
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    let made = matches!(shown, Shown::Invite { .. });
    let mut events = vec![
        outcome(shown)?.patch().into(),
        list(&state, &session).await?.patch().into(),
    ];
    if made {
        events.push(askama_datastar::PatchSignals::new(r#"{"_newuser":""}"#).into());
    }
    Ok(askama_datastar::events(events))
}

pub async fn invite(State(state): State<AppState>, Path(id): Path<i64>) -> AppResult<Response> {
    answer(invited(&state, id, false).await?, None)
}

pub async fn reset(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    // It would sign them out, with the link shown once the only way back in.
    if id == session.user_id {
        return asked(Shown::Problem(Problem::Yourself), None);
    }
    let shown = match state.db.reset_password(id).await {
        Ok(user) => {
            tracing::info!(username = %user.username, by = %session.username, "password reset");
            invited(&state, id, true).await?
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    asked(shown, None)
}

#[derive(serde::Deserialize)]
pub struct NewRole {
    role: String,
}

pub async fn set_role(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
    Json(new): Json<NewRole>,
) -> AppResult<Response> {
    let role: Role = new
        .role
        .parse()
        .map_err(|_| AppError::BadRequest("unknown role".into()))?;
    let shown = match state.db.set_role(id, role).await {
        Ok(user) => {
            tracing::info!(username = %user.username, %role, by = %session.username, "role changed");
            Shown::Nothing
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    // Redrawn either way, so a refused change shows the role as it is.
    answer(shown, Some(list(&state, &session).await?))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    if id == session.user_id {
        return asked(Shown::Problem(Problem::Yourself), None);
    }
    let shown = match state.db.delete_account(id).await {
        Ok(images) => {
            let count = images.len();
            state.uploads.jobs.forget(&images).await;
            crate::store::discard(&state.store, &state.db, images).await;
            tracing::info!(account = id, images = count, by = %session.username, "account deleted");
            Shown::Nothing
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    asked(shown, Some(list(&state, &session).await?))
}
