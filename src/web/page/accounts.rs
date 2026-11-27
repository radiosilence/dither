//! Accounts, at `/accounts`: the admins' page of accounts.
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
//!
//! Nothing streams this page, so every action answers with the regions it
//! changed: the outcome, and the list when an account changed.

use askama::Template;
use askama_datastar::{Html, PatchSignals};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::{Extension, Form, Json};
use koan_auth::Role;
use koan_auth::accounts::AccountError;
use koan_kit_web as kit;

use super::{Body, Page};
use crate::auth::session::Session;
use crate::error::{AppError, AppResult};
use crate::web::AppState;
use crate::web::layout::{Draw, Name, human};
use crate::web::paths::{self, AccountAction};

/// The signal the new account's name is bound to: the page's own, never sent,
/// so a made account's answer can empty it.
pub(in crate::web) const NEW_USER: &str = "_newuser";

pub(in crate::web) struct Accounts;

#[derive(Template)]
#[template(path = "accounts/page.html")]
struct AccountsPage {
    outcome: Html,
    list: Html,
}

impl AccountsPage {
    fn create_url(&self) -> &'static str {
        paths::ACCOUNTS
    }
}

/// The accounts, each with what can be done to it.
#[derive(Template)]
#[template(path = "accounts/list.html")]
struct List {
    accounts: Vec<Row>,
}

struct Row {
    id: i64,
    username: String,
    role: Role,
    albums: u64,
    size: String,
    /// The admin looking.
    me: bool,
}

impl Row {
    fn url(&self, action: AccountAction) -> String {
        paths::account_action(self.id, action)
    }
}

/// The roles to choose from, worded by the template that asks: the words
/// are the template's, so a translator reaches them.
pub(in crate::web) fn roles(
    user: impl Into<kit::Text<'static>>,
    readonly: impl Into<kit::Text<'static>>,
    admin: impl Into<kit::Text<'static>>,
) -> [kit::Choice<'static>; 3] {
    [
        kit::Choice::new(Role::User.as_str(), user),
        kit::Choice::new(Role::Readonly.as_str(), readonly),
        kit::Choice::new(Role::Admin.as_str(), admin),
    ]
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

/// What the last action on the page came to.
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
#[template(path = "accounts/outcome.html")]
struct Outcome {
    shown: Shown,
}

/// A [`Problem`] in words, as markup for `kit::Confirm::error`.
#[derive(Template)]
#[template(path = "accounts/refused.html")]
struct Refused {
    problem: Problem,
}

crate::i18n::translated!(AccountsPage, List, Outcome, Refused);

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
        .map(|a| Row {
            me: a.id == session.user_id,
            id: a.id,
            username: a.username,
            role: a.role,
            albums: a.albums,
            size: human(a.bytes),
        })
        .collect();
    Ok(List { accounts }.draw())
}

impl Page for Accounts {
    const NAME: Name = Name::Accounts;
    const STREAM: &'static str = paths::STREAM_QUIET;
    type Args = ();

    async fn body(s: &AppState, session: &Session, (): ()) -> AppResult<Body> {
        Ok(Body::of(&AccountsPage {
            outcome: Outcome {
                shown: Shown::Nothing,
            }
            .draw(),
            list: list(s, session).await?,
        }))
    }
}

/// An answer redrawing the outcome and, with `list`, the accounts.
fn answer(shown: Shown, list: Option<Html>) -> Response {
    let mut events = vec![Outcome { shown }.draw().patch().into()];
    events.extend(list.map(|l| l.patch().into()));
    askama_datastar::events(events)
}

/// The answer to a change the page's question asked for: `shown`, and the
/// accounts with `list`, once the question is closed; a refusal inside the
/// question, which stays open.
fn asked(shown: Shown, list: Option<Html>) -> Response {
    if let Shown::Problem(problem) = shown {
        return askama_datastar::events([kit::Confirm::error(Refused { problem }.draw()).into()]);
    }
    let mut events = vec![
        kit::Confirm::close().into(),
        Outcome { shown }.draw().patch().into(),
    ];
    events.extend(list.map(|l| l.patch().into()));
    askama_datastar::events(events)
}

async fn invited(state: &AppState, id: i64, reset: bool) -> AppResult<Shown> {
    match state
        .db
        .invite(id, state.config.signing_key.private.clone())
        .await
    {
        Ok((user, token)) => Ok(Shown::Invite {
            link: paths::invite(&state.config.app_url, &user.username, &token),
            username: user.username,
            reset,
        }),
        Err(e) => Ok(Shown::Problem(problem(e)?)),
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct NewAccount {
    #[serde(default)]
    newuser: String,
    newrole: Role,
}

pub(in crate::web) async fn create(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Form(new): Form<NewAccount>,
) -> AppResult<Response> {
    let role = new.newrole;
    let shown = match state.db.create_account(&new.newuser, role).await {
        Ok(user) => {
            tracing::info!(username = %user.username, %role, by = %session.username, "account made");
            invited(&state, user.id, false).await?
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    let made = matches!(shown, Shown::Invite { .. });
    let mut events = vec![
        Outcome { shown }.draw().patch().into(),
        list(&state, &session).await?.patch().into(),
    ];
    if made {
        let emptied = serde_json::Map::from_iter([(NEW_USER.to_string(), "".into())]);
        events.push(PatchSignals::new(serde_json::Value::from(emptied).to_string()).into());
    }
    Ok(askama_datastar::events(events))
}

/// `/accounts/<id>/<action>`, each action an admin takes on an account.
pub(in crate::web) async fn act(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path((id, action)): Path<(i64, AccountAction)>,
    body: Option<Json<NewRole>>,
) -> AppResult<Response> {
    match action {
        AccountAction::Invite => Ok(answer(invited(&state, id, false).await?, None)),
        AccountAction::Reset => reset(&state, &session, id).await,
        AccountAction::Role => {
            let Some(Json(NewRole { role })) = body else {
                return Err(AppError::BadRequest("no role".into()));
            };
            set_role(&state, &session, id, role).await
        }
        AccountAction::Delete => delete(&state, &session, id).await,
    }
}

async fn reset(state: &AppState, session: &Session, id: i64) -> AppResult<Response> {
    // It would sign them out, with the link shown once the only way back in.
    if id == session.user_id {
        return Ok(asked(Shown::Problem(Problem::Yourself), None));
    }
    let shown = match state.db.reset_password(id).await {
        Ok(user) => {
            tracing::info!(username = %user.username, by = %session.username, "password reset");
            invited(state, id, true).await?
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    Ok(asked(shown, None))
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct NewRole {
    role: Role,
}

async fn set_role(state: &AppState, session: &Session, id: i64, role: Role) -> AppResult<Response> {
    let shown = match state.db.set_role(id, role).await {
        Ok(user) => {
            tracing::info!(username = %user.username, %role, by = %session.username, "role changed");
            Shown::Nothing
        }
        Err(e) => Shown::Problem(problem(e)?),
    };
    // Redrawn either way, so a refused change shows the role as it is.
    Ok(answer(shown, Some(list(state, session).await?)))
}

async fn delete(state: &AppState, session: &Session, id: i64) -> AppResult<Response> {
    if id == session.user_id {
        return Ok(asked(Shown::Problem(Problem::Yourself), None));
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
    Ok(asked(shown, Some(list(state, session).await?)))
}
