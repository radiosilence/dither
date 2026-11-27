//! The album page's move dialog: the person's other albums, or a new one, to
//! move the chosen images to. Its body is read into the dialog as it opens,
//! with the images it moves in its form; the move answers with a flash
//! leading to the album they went to, and the page's stream redraws the grid
//! without them.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::{Extension, Form};
use koan_kit_web as kit;

use super::Chosen;
use crate::auth::session::Session;
use crate::db::{TITLE_MAX, cut};
use crate::error::{AppError, AppResult};
use crate::web::layout::{Draw, Helpers as _};
use crate::web::page::albums::CardView;
use crate::web::{AppState, paths};

/// The dialog's body: the form of albums to choose from.
#[derive(Template)]
#[template(path = "album/mover.html")]
struct Mover {
    /// The images to move, comma-separated, as the form sends them.
    ids: String,
    albums: Vec<CardView>,
}

impl Mover {
    /// An album to move images to: its cover, its title or `untitled`, and
    /// `detail` under it.
    fn choice<'a>(
        &self,
        a: &'a CardView,
        untitled: impl Into<kit::Text<'a>>,
        detail: impl Into<kit::Text<'a>>,
    ) -> kit::Check<'a> {
        let title = if a.title.is_empty() {
            untitled.into()
        } else {
            a.title.as_str().into()
        };
        kit::Check::radio("to", a.id.as_str(), title)
            .picture(a.cover.as_deref().unwrap_or_default())
            .detail(detail)
    }
}

/// The flash answering a move, leading to where the images went.
#[derive(Template)]
#[template(path = "album/moved.html")]
struct Moved {
    count: u64,
    to: String,
}

crate::i18n::translated!(Mover, Moved);

/// What tells the page that a move was made, so its script closes the viewer
/// and ends the choice: a cancel or a failed read does neither.
const MOVED: &str =
    r#"<i hidden data-init="el.remove(), dispatchEvent(new CustomEvent('dither-moved'))"></i>"#;

/// `/albums/<id>/mover?ids=…`: every other album of the person's, to move
/// the images named to.
pub(in crate::web) async fn mover(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Query(chosen): Query<Chosen>,
) -> AppResult<Response> {
    let ids = chosen.ids();
    if ids.is_empty() || !ids.iter().all(|i| crate::id::valid(i)) {
        return Err(AppError::NotFound);
    }
    let cards = state.db.cards(&session.username).await?;
    let mover = Mover {
        ids: ids.join(","),
        albums: cards
            .iter()
            .filter(|c| c.album.id != id)
            .map(CardView::new)
            .collect(),
    };
    Ok(askama_datastar::events([kit::Sheet::fill(
        "mover",
        mover.markup(),
    )
    .into()]))
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct Move {
    /// Comma-separated.
    #[serde(default)]
    ids: String,
    /// An album to move to, or empty for a new one called `title`.
    #[serde(default)]
    to: String,
    #[serde(default)]
    title: String,
}

pub(in crate::web) async fn move_images(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Form(form): Form<Move>,
) -> AppResult<Response> {
    let ids = Chosen { ids: form.ids }.ids();
    let to = if form.to.is_empty() {
        state
            .db
            .create_album(&session.username, &cut(&form.title, TITLE_MAX))
            .await?
            .id
    } else {
        form.to
    };
    // The count says if some did not move: gone meanwhile, or not theirs.
    let count = state.db.move_images(&session.username, ids, &to).await?;
    let moved = Moved {
        count: count as u64,
        to: paths::album(&to),
    };
    Ok(askama_datastar::events([
        kit::Sheet::dismiss("mover").into(),
        moved.draw().patch().into(),
        askama_datastar::Html::trusted(MOVED)
            .patch()
            .selector("body")
            .mode(askama_datastar::ElementPatchMode::Append)
            .into(),
    ]))
}
