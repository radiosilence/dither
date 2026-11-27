//! An album, at `/albums/<id>`, as its owner edits it: its title,
//! description and privacy saved as they are typed, its images to add,
//! order, choose, open in the viewer ([`viewer`]) and move ([`mover`]), and
//! deleting it. The page's stream (`stream::album`) redraws the grid, the
//! count and the foot as uploads land, and moves a video's bar as it is made.
//! Selection and drag to reorder are the browser script's (`browser/app.ts`),
//! since they live in the browser.

pub(in crate::web) mod mover;
pub(in crate::web) mod viewer;

use askama::Template;
use askama_datastar::Html;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Json};
use koan_kit_web as kit;

use super::{Body, Page};
use crate::auth::session::Session;
use crate::db::{AlbumEdit, Image, Status, TEXT_MAX, TITLE_MAX, cut};
use crate::error::{AppError, AppResult};
use crate::jobs::Failure;
use crate::web::layout::{Draw, Helpers as _, Name, Shell, human};
use crate::web::{AppState, paths};

pub(in crate::web) struct Album;

#[derive(Template)]
#[template(path = "album/page.html")]
struct AlbumPage {
    shell: Shell,
    id: String,
    title: String,
    description: String,
    private: bool,
    /// The address its link shares.
    share: String,
    grid: Html,
    summary: Html,
    foot: Html,
    views: Html,
}

impl AlbumPage {
    fn meta_url(&self) -> String {
        paths::album_meta(&self.id)
    }
    fn order_url(&self) -> String {
        paths::album_order(&self.id)
    }
    fn cover_url(&self) -> String {
        paths::album_cover(&self.id)
    }
    fn mover_url(&self) -> String {
        paths::album_mover_base(&self.id)
    }
    fn delete_url(&self) -> &'static str {
        paths::DELETE_IMAGES
    }
    fn move_url(&self) -> &'static str {
        paths::MOVE
    }
    fn uploads_url(&self) -> String {
        paths::api_uploads(&self.id)
    }
    fn albums_url(&self) -> &'static str {
        paths::ALBUMS
    }
}

/// The album's images as tiles to open, choose and drag.
#[derive(Template)]
#[template(path = "album/grid.html")]
pub(in crate::web) struct Grid {
    cover: String,
    tiles: Vec<Tile>,
}

impl Grid {
    fn meter(&self, id: &str, done: &u16) -> kit::Meter<'static> {
        progress(id, *done)
    }
}

/// A video's progress on its tile, in thousandths: drawn with the grid, and
/// sent alone as it moves.
pub(in crate::web) fn progress(id: &str, done: u16) -> kit::Meter<'static> {
    kit::Meter::new(done, 1000).id(format!("vp-{id}"))
}

struct Tile {
    id: String,
    thumb: String,
    view: String,
    animated: bool,
    captioned: bool,
    label: String,
    /// A video's length.
    length: Option<String>,
    /// A video being made: thousandths done.
    progress: Option<u16>,
    /// Why a video could not be made.
    failure: Option<Failure>,
}

impl Tile {
    fn new(i: &Image, jobs: &crate::jobs::Hub) -> Self {
        Self {
            id: i.id.clone(),
            thumb: paths::thumb(&i.id),
            view: paths::viewer(&i.id),
            animated: i.animated,
            length: (i.is_video() && i.ready()).then(|| crate::video::clock(i.seconds())),
            progress: (i.status == Status::Processing).then(|| jobs.progress(&i.id).unwrap_or(0)),
            failure: (i.status == Status::Failed).then(|| Failure::of(&i.failure)),
            captioned: !i.caption.is_empty(),
            label: Some(crate::markdown::plain(&i.caption))
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| i.name.clone()),
        }
    }
}

/// How many images the album holds, and what they take.
#[derive(Template)]
#[template(path = "album/summary.html")]
pub(in crate::web) struct Summary {
    count: u64,
    size: Option<String>,
}

/// The album's foot: deleting it, asked with how many images go with it.
#[derive(Template)]
#[template(path = "album/foot.html")]
pub(in crate::web) struct Foot {
    id: String,
    count: u64,
}

impl Foot {
    fn delete_url(&self) -> String {
        paths::album_delete(&self.id)
    }
}

/// The grid of an album deleted elsewhere.
#[derive(Template)]
#[template(path = "album/deleted.html")]
pub(in crate::web) struct Deleted;

/// Why a change asked for in the page's question failed, as markup for
/// `kit::Confirm::error`.
#[derive(Template)]
#[template(path = "album/failed.html")]
struct Refused {
    /// Deleting this many images; else the album.
    images: Option<u64>,
}

/// An image's or album's view counts, as its owner sees them.
#[derive(Template)]
#[template(path = "album/views.html")]
pub(in crate::web) struct ViewCounts {
    recent: Line,
    all: Line,
    shared: Vec<(String, u64)>,
    referrers: Vec<(String, u64)>,
}

impl ViewCounts {
    /// Each site it was shared on, with its count.
    fn sites(&self) -> Vec<String> {
        self.shared
            .iter()
            .map(|(s, n)| format!("{s} {n}"))
            .collect()
    }

    /// Each host linking to it, with its count; `other` for the rest.
    fn hosts(&self, other: impl std::fmt::Display) -> Vec<String> {
        self.referrers
            .iter()
            .map(|(h, n)| match h.as_str() {
                "" => format!("{other} {n}"),
                h => format!("{h} {n}"),
            })
            .collect()
    }
}

struct Line {
    views: u64,
    visitors: u64,
    size: String,
}

impl Line {
    fn new(t: crate::views::Totals) -> Self {
        Self {
            views: t.views,
            visitors: t.visitors,
            size: human(t.bytes),
        }
    }
}

crate::i18n::translated!(AlbumPage, Grid, Summary, Foot, Deleted, Refused, ViewCounts);

/// The view counts of image or album `target`. Only its owner's pages draw
/// them.
pub(in crate::web) async fn view_counts(state: &AppState, target: &str) -> AppResult<ViewCounts> {
    let since = crate::views::today() - crate::views::RECENT_DAYS + 1;
    let stats = state.db.view_stats(target, since).await?;
    Ok(ViewCounts {
        recent: Line::new(stats.recent),
        all: Line::new(stats.all),
        shared: stats.shared,
        referrers: stats.referrers,
    })
}

/// The page's live regions, as they stand: the grid, the count and the foot,
/// with the ids of the images the grid shows.
pub(in crate::web) async fn regions(
    state: &AppState,
    album: &crate::db::Album,
) -> AppResult<(Grid, Summary, Foot, Vec<String>)> {
    let images = state.db.images(&album.id).await?;
    let cover = album
        .cover
        .clone()
        .or_else(|| images.first().map(|i| i.id.clone()))
        .unwrap_or_default();
    let bytes: u64 = images.iter().map(|i| i.bytes).sum();
    let count = images.len() as u64;
    let grid = Grid {
        cover,
        tiles: images
            .iter()
            .map(|i| Tile::new(i, &state.uploads.jobs))
            .collect(),
    };
    let summary = Summary {
        count,
        size: (bytes > 0).then(|| human(bytes)),
    };
    let foot = Foot {
        id: album.id.clone(),
        count,
    };
    Ok((
        grid,
        summary,
        foot,
        images.into_iter().map(|i| i.id).collect(),
    ))
}

/// Album `id` if `owner`'s; not found otherwise, as if there were none.
pub(in crate::web) async fn owned(
    state: &AppState,
    owner: &str,
    id: &str,
) -> AppResult<crate::db::Album> {
    if !crate::id::valid(id) {
        return Err(AppError::NotFound);
    }
    match state.db.album(id).await? {
        Some(album) if album.owner == owner => Ok(album),
        _ => Err(AppError::NotFound),
    }
}

impl Page for Album {
    const NAME: Name = Name::Album;
    const STREAM: &'static str = paths::STREAM_ALBUM;
    type Args = crate::db::Album;

    async fn body(s: &AppState, _: &Session, album: crate::db::Album) -> AppResult<Body> {
        let (grid, summary, foot, _) = regions(s, &album).await?;
        let views = view_counts(s, &album.id).await?;
        let page = AlbumPage {
            shell: Shell::of(s),
            share: paths::absolute(&s.config.public_url, &paths::public_album(&album.id)),
            id: album.id,
            title: album.title.clone(),
            description: album.description,
            private: album.private,
            grid: grid.draw(),
            summary: summary.draw(),
            foot: foot.draw(),
            views: views.draw(),
        };
        Ok(Body::of(&page).heading(album.title))
    }
}

/// `/albums/<id>`: the album's editor for its owner. Its address is what a
/// browser's bar shows, so it is what people copy: anyone else is sent to the
/// page its link shares, unless the album is private. Someone not signed in
/// whose browser has held a session, likely its owner, goes to sign-in. A
/// private or missing album is not found for someone signed in, and sends
/// anyone else to sign-in, which says nothing of whether it exists. The
/// app's own requests are answered with a status, never redirected.
pub(in crate::web) async fn page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> AppResult<Response> {
    let session = crate::auth::extract::current(&state, &headers).await;
    let found = if crate::id::valid(&id) {
        state.db.album(&id).await?
    } else {
        None
    };
    let page_load = crate::auth::extract::is_page_load(&headers);
    // The app's own requests, which a redirect would swap a page into. A
    // link preview's fetcher may not ask for HTML, so this is not
    // `!page_load`.
    let app_request = askama_datastar::is_datastar(&headers) || headers.contains_key("x-dither");
    let lapsed = crate::auth::cookie::session_id(&headers).is_some();
    let url = askama_datastar::address(&uri);
    let sign_in = || Redirect::to(&paths::login_next(&url)).into_response();
    Ok(match (session, found) {
        (Some(session), Some(album)) if album.owner == session.username => {
            super::respond::<Album>(&state, &session, &headers, &url, album).await?
        }
        (None, _) if app_request => StatusCode::UNAUTHORIZED.into_response(),
        (Some(_), _) if app_request => return Err(AppError::NotFound),
        (None, _) if lapsed && page_load => sign_in(),
        (_, Some(album)) if !album.private => Redirect::to(&paths::absolute(
            &state.config.public_url,
            &paths::public_album(&album.id),
        ))
        .into_response(),
        (None, _) if page_load => sign_in(),
        (None, _) => StatusCode::UNAUTHORIZED.into_response(),
        (Some(_), _) => return Err(AppError::NotFound),
    })
}

/// The answer to a change the page's question asked for that failed: why,
/// inside the question, which stays open.
fn refused(images: Option<u64>) -> Response {
    askama_datastar::events([kit::Confirm::error(Refused { images }.markup()).into()])
}

/// Saved as it is typed: nothing is drawn again, since the fields already
/// show it.
pub(in crate::web) async fn meta(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(mut edit): Json<AlbumEdit>,
) -> AppResult<StatusCode> {
    edit.title = edit.title.map(|t| cut(&t, TITLE_MAX));
    edit.description = edit.description.map(|d| cut(&d, TEXT_MAX));
    if state.db.edit_album(&session.username, &id, edit).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct Order {
    ids: Vec<String>,
}

/// The order the script's drag left the grid in.
pub(in crate::web) async fn order(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(order): Json<Order>,
) -> AppResult<StatusCode> {
    if state.db.reorder(&session.username, &id, order.ids).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// The image to make an album's cover, named in the address, since a button
/// posts the page's signals rather than a body of its own. None goes back to
/// the album's first image.
#[derive(serde::Deserialize)]
pub(in crate::web) struct Cover {
    image: Option<String>,
}

/// Answered with the viewer redrawn, since its button says whether the image
/// is the cover; the stream redraws the grid's mark.
pub(in crate::web) async fn cover(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Query(cover): Query<Cover>,
) -> AppResult<Response> {
    if !state
        .db
        .set_cover(&session.username, &id, cover.image.as_deref())
        .await?
    {
        return Err(AppError::NotFound);
    }
    Ok(match cover.image {
        Some(image) => viewer::region(&state, &session, &image)
            .await?
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// Asked for in the page's question, so answered there: the browser sent to
/// the albums, or why it failed, in the question.
pub(in crate::web) async fn delete(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let images = match state.db.delete_album(&session.username, &id).await {
        Ok(Some(images)) => images,
        // Not this person's: as not found as everywhere else, though their
        // page would never ask.
        Ok(None) => return Err(AppError::NotFound),
        Err(e) => {
            tracing::warn!(album = %id, error = %e, "deleting an album failed");
            return Ok(refused(None));
        }
    };
    let count = images.len();
    state.uploads.jobs.forget(&images).await;
    crate::store::discard(&state.store, &state.db, images).await;
    tracing::info!(album = %id, images = count, "album deleted");
    // The page it was asked on is gone with it.
    Ok(askama_datastar::events([askama_datastar::go(
        paths::ALBUMS,
    )
    .into()]))
}

#[derive(serde::Deserialize)]
pub(in crate::web) struct Caption {
    caption: String,
}

/// Saved as it is typed, and as the viewer moves on.
pub(in crate::web) async fn caption(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(body): Json<Caption>,
) -> AppResult<StatusCode> {
    if state
        .db
        .set_caption(&session.username, &id, &cut(&body.caption, TEXT_MAX))
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// The images to delete, named in the address, since the page's question
/// sends the page's signals rather than a body of its own.
#[derive(serde::Deserialize)]
pub(in crate::web) struct Chosen {
    /// Comma-separated.
    #[serde(default)]
    ids: String,
}

impl Chosen {
    pub(in crate::web) fn ids(&self) -> Vec<String> {
        self.ids
            .split(',')
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Asked for in the page's question, so answered there: the question closed,
/// or why it failed, in the question. The stream redraws the album without
/// them, and the page's script follows: a choice whose images are gone ends,
/// and a viewer showing one moves on.
pub(in crate::web) async fn delete_images(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Query(chosen): Query<Chosen>,
) -> Response {
    let ids = chosen.ids();
    let asked = ids.len() as u64;
    let deleted = match state.db.delete_images(&session.username, ids).await {
        Ok(deleted) => deleted,
        Err(e) => {
            tracing::warn!(error = %e, "deleting images failed");
            return refused(Some(asked));
        }
    };
    state.uploads.jobs.forget(&deleted).await;
    crate::store::discard(&state.store, &state.db, deleted).await;
    askama_datastar::events([kit::Confirm::close().into()])
}
