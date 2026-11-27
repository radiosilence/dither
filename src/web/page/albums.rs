//! Albums, at `/`: a person's albums, the place to drop images into a new
//! one, and what they add up to. The page's stream (`stream::albums`) redraws
//! the list and the usage line as uploads land.

use askama::Template;
use askama_datastar::Html;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use koan_kit_web as kit;

use super::{Body, Page};
use crate::auth::session::Session;
use crate::db::Card;
use crate::error::AppResult;
use crate::web::layout::{Draw, Name, Shell, human};
use crate::web::{AppState, paths};

pub(in crate::web) struct Albums;

#[derive(Template)]
#[template(path = "albums/page.html")]
struct AlbumsPage {
    shell: Shell,
    list: Html,
    usage: Html,
}

impl AlbumsPage {
    fn max_label(&self) -> String {
        human(self.shell.max_upload)
    }

    fn max_video_label(&self) -> String {
        self.shell.max_video.map(human).unwrap_or_default()
    }
}

/// The albums, each a card leading to its page.
#[derive(Template)]
#[template(path = "albums/list.html")]
pub(in crate::web) struct List {
    cards: Vec<CardView>,
}

/// What the albums add up to.
#[derive(Template)]
#[template(path = "albums/usage.html")]
pub(in crate::web) struct Usage {
    albums: u64,
    images: u64,
    size: String,
}

crate::i18n::translated!(AlbumsPage, List, Usage);

/// An album as a card, or as a choice in the move dialog.
pub(in crate::web) struct CardView {
    pub(in crate::web) id: String,
    pub(in crate::web) href: String,
    pub(in crate::web) title: String,
    pub(in crate::web) count: u64,
    /// What its images take, when they take anything.
    pub(in crate::web) size: Option<String>,
    pub(in crate::web) cover: Option<String>,
    pub(in crate::web) private: bool,
}

impl CardView {
    pub(in crate::web) fn new(c: &Card) -> Self {
        Self {
            id: c.album.id.clone(),
            href: paths::album(&c.album.id),
            title: c.album.title.clone(),
            count: c.count,
            size: (c.bytes > 0).then(|| human(c.bytes)),
            cover: c.cover.as_ref().map(|i| paths::thumb(&i.id)),
            private: c.album.private,
        }
    }
}

/// The page's live regions: the list, and what the albums add up to.
pub(in crate::web) async fn regions(state: &AppState, owner: &str) -> AppResult<(List, Usage)> {
    let cards = state.db.cards(owner).await?;
    let (images, bytes) = state.db.usage(owner).await?;
    let usage = Usage {
        albums: cards.len() as u64,
        images,
        size: human(bytes),
    };
    let list = List {
        cards: cards.iter().map(CardView::new).collect(),
    };
    Ok((list, usage))
}

impl Page for Albums {
    const NAME: Name = Name::Albums;
    const STREAM: &'static str = paths::STREAM_ALBUMS;
    type Args = ();

    async fn body(s: &AppState, session: &Session, (): ()) -> AppResult<Body> {
        let (list, usage) = regions(s, &session.username).await?;
        Ok(Body::of(&AlbumsPage {
            shell: Shell::of(s),
            list: list.draw(),
            usage: usage.draw(),
        }))
    }
}

/// `/`: the albums behind sign-in; on the public domain, a page that says
/// nothing.
pub(in crate::web) async fn home(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response {
    if crate::web::client::on_public_host(&state.config, &headers) {
        return crate::web::public::pages::landing().await;
    }
    let Some(session) = crate::auth::extract::current(&state, &headers).await else {
        if crate::auth::extract::is_page_load(&headers) {
            return Redirect::to(paths::LOGIN).into_response();
        }
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let url = askama_datastar::address(&uri);
    super::respond::<Albums>(&state, &session, &headers, &url, ())
        .await
        .unwrap_or_else(IntoResponse::into_response)
}
