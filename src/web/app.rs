//! The app: a person's albums, an album to edit, and the image viewer.
//!
//! Pages are drawn whole on the server. Datastar patches parts of them in
//! place: a live stream redraws the album grid and the library whenever a
//! write lands (an upload finishing, another device reordering), sending a
//! part only when it differs from what was sent. Fields save as they are
//! typed in. The upload queue, selection and drag-to-reorder are the
//! browser script's (browser/app.ts),
//! since they live in the browser.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use askama::Template;
use askama_datastar::{Html as Fragment, PatchElements};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Json};
use futures_util::StreamExt as _;
use koan_kit_web as kit;

use crate::auth::session::Session;
use crate::db::{AlbumEdit, Card, Image, Status};
use crate::error::{AppError, AppResult};
use crate::media::{DISPLAY_MAX, fit};
use crate::upload::CHUNK;
use crate::web::Helpers as _;
use crate::web::{AppState, human, on_public_host};

/// Longest title, description and caption kept.
const TITLE_MAX: usize = 300;
const TEXT_MAX: usize = 20_000;

fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Facts the layout needs on every page.
pub(crate) struct Shell {
    pub(crate) login: String,
    /// Whether to offer the accounts page.
    pub(crate) admin: bool,
    pub(crate) max_upload: u64,
    /// The limit for a video; `None` when the server takes none.
    pub(crate) max_video: Option<u64>,
    pub(crate) chunk: u64,
    /// The running version, shown to the signed-in only: the public pages do
    /// not say what they run.
    pub(crate) version: &'static str,
}

impl Shell {
    pub(crate) fn max_label(&self) -> String {
        human(self.max_upload)
    }

    pub(crate) fn max_video_label(&self) -> String {
        self.max_video.map(human).unwrap_or_default()
    }
}

pub(crate) async fn shell(state: &AppState, session: &Session) -> Shell {
    Shell {
        login: session.username.clone(),
        admin: session.admin(),
        max_upload: state.config.max_upload_bytes,
        max_video: state.uploads.max_video_bytes(),
        chunk: CHUNK,
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[derive(Template)]
#[template(path = "app/library.html")]
struct LibraryPage {
    shell: Shell,
    albums: Fragment,
    usage: Fragment,
}

impl LibraryPage {
    /// The live stream's attributes, which open it and open it again
    /// whenever it ends or goes quiet.
    fn live(&self) -> Fragment {
        askama_datastar::reopen("/stream")
    }
}

#[derive(Template)]
#[template(path = "app/albums.html")]
struct Albums {
    cards: Vec<CardView>,
}

struct CardView {
    id: String,
    title: String,
    count: u64,
    /// What its images take, when they take anything.
    size: Option<String>,
    cover: Option<String>,
    private: bool,
}

impl CardView {
    fn new(c: &Card) -> Self {
        Self {
            id: c.album.id.clone(),
            title: c.album.title.clone(),
            count: c.count,
            size: (c.bytes > 0).then(|| human(c.bytes)),
            cover: c.cover.as_ref().map(|i| format!("/t/{}.webp", i.id)),
            private: c.album.private,
        }
    }
}

#[derive(Template)]
#[template(path = "app/usage.html")]
struct Usage {
    albums: u64,
    images: u64,
    size: String,
}

#[derive(Template)]
#[template(path = "app/summary.html")]
struct Summary {
    count: u64,
    size: Option<String>,
}

#[derive(Template)]
#[template(path = "app/deleted.html")]
struct Deleted;

/// The album's foot: deleting it, asked with how many images go with it.
#[derive(Template)]
#[template(path = "app/foot.html")]
struct Foot {
    id: String,
    count: u64,
}

impl Foot {
    fn delete_url(&self) -> String {
        format!("/albums/{}/delete", self.id)
    }
}

/// Why a change asked for in the page's question failed, as markup for
/// `kit::Confirm::error`.
#[derive(Template)]
#[template(path = "app/failed.html")]
struct Failure {
    /// Deleting this many images; else the album.
    images: Option<u64>,
}

/// The answer to a change the page's question asked for that failed: why,
/// inside the question, which stays open.
fn refused(images: Option<u64>) -> AppResult<Response> {
    let why = render(&Failure { images })?;
    Ok(askama_datastar::events([kit::Confirm::error(why).into()]))
}

/// The five ways the viewer offers to share an image.
struct Links {
    page: String,
    file: String,
    markdown: String,
    bbcode: String,
    html: String,
}

#[derive(Template)]
#[template(path = "app/album.html")]
struct AlbumPage {
    shell: Shell,
    id: String,
    /// Where its title, description and privacy are saved.
    meta: String,
    title: String,
    description: String,
    private: bool,
    share: String,
    grid: Fragment,
    summary: Fragment,
    foot: Fragment,
    views: Fragment,
}

impl AlbumPage {
    /// The live stream's attributes, which open it and open it again
    /// whenever it ends or goes quiet.
    fn live(&self) -> Fragment {
        askama_datastar::reopen(&format!("/stream?album={}", self.id))
    }
}

/// An image's or album's view counts, as its owner sees them.
#[derive(Template)]
#[template(path = "app/views.html")]
struct ViewCounts {
    recent: Line,
    all: Line,
    shared: Vec<(String, u64)>,
    referrers: Vec<(String, u64)>,
}

impl ViewCounts {
    /// The sites it was shared on, each with its count: names, not words.
    fn shared_on(&self) -> String {
        self.shared
            .iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
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

/// The view counts of image or album `target`, drawn. Only its owner's
/// pages call this.
async fn view_counts(state: &AppState, target: &str) -> AppResult<Fragment> {
    let since = crate::views::today() - crate::views::RECENT_DAYS + 1;
    let stats = state.db.view_stats(target, since).await?;
    render(&ViewCounts {
        recent: Line::new(stats.recent),
        all: Line::new(stats.all),
        shared: stats.shared,
        referrers: stats.referrers,
    })
}

#[derive(Template)]
#[template(path = "app/grid.html")]
struct Grid {
    album: String,
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
fn progress(id: &str, done: u16) -> kit::Meter<'static> {
    kit::Meter::new(done, 1000).id(format!("vp-{id}"))
}

struct Tile {
    id: String,
    thumb: String,
    animated: bool,
    captioned: bool,
    label: String,
    /// A video's length.
    length: Option<String>,
    /// A video being made: thousandths done.
    progress: Option<u16>,
    /// Why a video could not be made.
    failure: Option<String>,
}

impl Tile {
    fn new(i: &Image, jobs: &crate::jobs::Hub) -> Self {
        Self {
            id: i.id.clone(),
            thumb: format!("/t/{}.webp", i.id),
            animated: i.animated,
            length: (i.is_video() && i.ready()).then(|| crate::video::clock(i.seconds())),
            progress: (i.status == Status::Processing).then(|| jobs.progress(&i.id).unwrap_or(0)),
            failure: (i.status == Status::Failed).then(|| i.failure.clone()),
            captioned: !i.caption.is_empty(),
            label: Some(crate::markdown::plain(&i.caption))
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| i.name.clone()),
        }
    }
}

#[derive(Template)]
#[template(path = "app/viewer.html")]
struct Viewer {
    id: String,
    album: String,
    src: String,
    width: u32,
    height: u32,
    caption: String,
    /// The caption's words without its Markdown.
    alt: String,
    /// Where it falls in the album, from 1.
    position: usize,
    total: usize,
    prev: Option<String>,
    next: Option<String>,
    is_cover: bool,
    links: Links,
    file: String,
    download: String,
    name: String,
    width_full: u32,
    height_full: u32,
    size: String,
    animated: bool,
    /// A video, ready to play.
    clip: Option<crate::web::clip::Clip>,
    video: bool,
    /// A video's length.
    length: Option<String>,
    /// A video being made: thousandths done.
    progress: Option<u16>,
    /// Why a video could not be made.
    failure: Option<String>,
    views: Fragment,
}

impl Viewer {
    fn view_url(&self, id: &str) -> String {
        format!("/images/{id}/view")
    }

    fn caption_id(&self) -> String {
        format!("caption-{}", self.id)
    }

    fn caption_url(&self) -> String {
        format!("/images/{}/caption", self.id)
    }

    fn cover_url(&self) -> String {
        format!("/albums/{}/cover?image={}", self.album, self.id)
    }

    fn delete_url(&self) -> String {
        format!("/images/delete?ids={}", self.id)
    }
}

crate::i18n::translated!(
    LibraryPage,
    Albums,
    AlbumPage,
    Grid,
    Viewer,
    Picker,
    Usage,
    Summary,
    Deleted,
    Foot,
    Failure,
    ViewCounts
);

#[derive(Template)]
#[template(path = "app/picker.html")]
struct Picker {
    albums: Vec<CardView>,
}

impl Picker {
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

fn render(t: &impl Template) -> AppResult<Fragment> {
    Fragment::render(t).map_err(|e| AppError::Internal(e.into()))
}

/// `/`: the library behind sign-in; on the public domain, nothing.
pub async fn home(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if on_public_host(&state.config, &headers) {
        return crate::web::public::landing().await;
    }
    let Some(session) = crate::auth::extract::current(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    match library(&state, &session).await {
        Ok(html) => html.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn library(state: &AppState, session: &Session) -> AppResult<Fragment> {
    let (albums, usage) = library_parts(state, session).await?;
    render(&LibraryPage {
        shell: shell(state, session).await,
        albums,
        usage,
    })
}

/// The library's live parts: the albums, and what they add up to.
async fn library_parts(state: &AppState, session: &Session) -> AppResult<(Fragment, Fragment)> {
    let cards = state.db.cards(&session.username).await?;
    let albums = render(&Albums {
        cards: cards.iter().map(CardView::new).collect(),
    })?;
    let (images, bytes) = state.db.usage(&session.username).await?;
    let usage = render(&Usage {
        albums: cards.len() as u64,
        images,
        size: human(bytes),
    })?;
    Ok((albums, usage))
}

async fn owned(state: &AppState, session: &Session, id: &str) -> AppResult<crate::db::Album> {
    if !crate::id::valid(id) {
        return Err(AppError::NotFound);
    }
    match state.db.album(id).await? {
        Some(album) if album.owner == session.username => Ok(album),
        _ => Err(AppError::NotFound),
    }
}

/// `/albums/<id>`: the album's editor for its owner. Its address is what a
/// browser's bar shows, so it is what people copy: anyone else is sent to the
/// page its link shares, unless the album is private. Someone not signed in
/// whose browser has held a session, likely its owner, goes to sign-in. A
/// private or missing album is not found for someone signed in, and sends
/// anyone else to sign-in, which says nothing of whether it exists. The
/// app's own requests are answered with a status, never redirected.
pub async fn album(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
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
    let sign_in = || {
        let path = uri.path_and_query().map_or("/", |p| p.as_str());
        Redirect::to(&crate::auth::routes::login_url(path)).into_response()
    };
    Ok(match (session, found) {
        (Some(session), Some(album)) if album.owner == session.username => {
            editor(&state, &session, album).await?.into_response()
        }
        (None, _) if app_request => StatusCode::UNAUTHORIZED.into_response(),
        (Some(_), _) if app_request => return Err(AppError::NotFound),
        (None, _) if lapsed && page_load => sign_in(),
        (_, Some(album)) if !album.private => {
            Redirect::to(&format!("{}/a/{}", state.config.public_url, album.id)).into_response()
        }
        (None, _) if page_load => sign_in(),
        (None, _) => StatusCode::UNAUTHORIZED.into_response(),
        (Some(_), _) => return Err(AppError::NotFound),
    })
}

async fn editor(
    state: &AppState,
    session: &Session,
    album: crate::db::Album,
) -> AppResult<Fragment> {
    let (grid, summary, foot, _) = album_parts(state, &album).await?;
    let views = view_counts(state, &album.id).await?;
    render(&AlbumPage {
        views,
        shell: shell(state, session).await,
        share: format!("{}/a/{}", state.config.public_url, album.id),
        meta: format!("/albums/{}/meta", album.id),
        foot,
        id: album.id,
        title: album.title,
        description: album.description,
        private: album.private,
        grid,
        summary,
    })
}

/// An album page's live parts: the grid of images, the line counting them and
/// the foot that deletes them, and the images' ids.
async fn album_parts(
    state: &AppState,
    album: &crate::db::Album,
) -> AppResult<(Fragment, Fragment, Fragment, Vec<String>)> {
    let images = state.db.images(&album.id).await?;
    let cover = album
        .cover
        .clone()
        .or_else(|| images.first().map(|i| i.id.clone()))
        .unwrap_or_default();
    let grid = render(&Grid {
        album: album.id.clone(),
        cover,
        tiles: images
            .iter()
            .map(|i| Tile::new(i, &state.uploads.jobs))
            .collect(),
    })?;
    let bytes: u64 = images.iter().map(|i| i.bytes).sum();
    let summary = render(&Summary {
        count: images.len() as u64,
        size: (bytes > 0).then(|| human(bytes)),
    })?;
    let foot = render(&Foot {
        id: album.id.clone(),
        count: images.len() as u64,
    })?;
    Ok((
        grid,
        summary,
        foot,
        images.into_iter().map(|i| i.id).collect(),
    ))
}

pub async fn album_meta(
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
pub struct Ids {
    ids: Vec<String>,
}

pub async fn reorder(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(body): Json<Ids>,
) -> AppResult<StatusCode> {
    if state.db.reorder(&session.username, &id, body.ids).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// The image to make an album's cover, named in the address, since the
/// viewer's button sends the page's signals rather than a body of its own.
/// None goes back to the album's first image.
#[derive(serde::Deserialize)]
pub struct Cover {
    image: Option<String>,
}

/// Answered with the viewer redrawn, since that is where it was asked from.
pub async fn cover(
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
        Some(image) => viewer_html(&state, &session, &image).await?.into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// Asked for in the page's question, so answered there: the question closed
/// and the browser sent to the albums, or why it failed, in the question.
pub async fn delete_album(
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
            return refused(None);
        }
    };
    let count = images.len();
    state.uploads.jobs.forget(&images).await;
    crate::store::discard(&state.store, &state.db, images).await;
    tracing::info!(album = %id, images = count, "album deleted");
    // The page it was asked on is gone with it.
    Ok(askama_datastar::events([kit::Confirm::go("/").into()]))
}

#[derive(serde::Deserialize)]
pub struct Caption {
    caption: String,
}

pub async fn caption(
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

#[derive(serde::Deserialize)]
pub struct Move {
    ids: Vec<String>,
    /// An album to move to, or none for a new one called `title`.
    to: Option<String>,
    #[serde(default)]
    title: String,
}

pub async fn move_images(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Json(body): Json<Move>,
) -> AppResult<Json<serde_json::Value>> {
    let to = match body.to {
        Some(to) => to,
        None => {
            state
                .db
                .create_album(&session.username, &cut(&body.title, TITLE_MAX))
                .await?
                .id
        }
    };
    let moved = state
        .db
        .move_images(&session.username, body.ids, &to)
        .await?;
    Ok(Json(serde_json::json!({ "moved": moved, "to": to })))
}

/// The images to delete, named in the address, since the page's question
/// sends the page's signals rather than a body of its own.
#[derive(serde::Deserialize)]
pub struct Chosen {
    /// Comma-separated.
    #[serde(default)]
    ids: String,
}

/// Asked for in the page's question, so answered there: the question closed,
/// or why it failed, in the question. The live stream redraws the album
/// without them, and the page's script follows: a choice whose images are
/// gone ends, and a viewer showing one moves on.
pub async fn delete_images(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Query(chosen): Query<Chosen>,
) -> AppResult<Response> {
    let ids: Vec<String> = chosen
        .ids
        .split(',')
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
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
    Ok(askama_datastar::events([kit::Confirm::close().into()]))
}

/// The move dialog's list: every other album of the person's.
pub async fn picker(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Fragment> {
    let cards = state.db.cards(&session.username).await?;
    render(&Picker {
        albums: cards
            .iter()
            .filter(|c| c.album.id != id)
            .map(CardView::new)
            .collect(),
    })
}

/// An image in the viewer: large, with its caption to edit, the links to
/// share it by, and its neighbours in the album.
pub async fn viewer(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Fragment> {
    viewer_html(&state, &session, &id).await
}

async fn viewer_html(state: &AppState, session: &Session, id: &str) -> AppResult<Fragment> {
    if !crate::id::valid(id) {
        return Err(AppError::NotFound);
    }
    let (image, album) = state.db.image(id).await?.ok_or(AppError::NotFound)?;
    if album.owner != session.username {
        return Err(AppError::NotFound);
    }
    let images = state.db.images(&album.id).await?;
    let at = images.iter().position(|i| i.id == id).unwrap_or_default();
    let public = &state.config.public_url;
    let file = format!("/{}.{}", image.id, image.kind.ext());
    let direct = format!("{public}{file}");
    let page = format!("{public}/{}", image.id);
    let alt = crate::markdown::plain(&image.caption)
        .lines()
        .next()
        .unwrap_or_default()
        .replace(['[', ']', '"', '<', '>'], "");
    let (src, width, height) = if image.display && !image.is_video() {
        let (w, h) = fit(image.width, image.height, DISPLAY_MAX);
        (format!("/d/{}.webp", image.id), w, h)
    } else {
        (file.clone(), image.width, image.height)
    };
    // An embedded video plays the copy every browser plays.
    let html = if image.is_video() {
        format!(
            "<video src=\"{public}{}\" poster=\"{public}/p/{}.jpg\" controls playsinline></video>",
            crate::web::public::clip(&image).src,
            image.id
        )
    } else {
        format!("<img src=\"{direct}\" alt=\"{alt}\">")
    };
    let cover = album
        .cover
        .as_deref()
        .or(images.first().map(|i| i.id.as_str()));
    let name = if image.name.is_empty() {
        format!("{}.{}", image.id, image.kind.ext())
    } else {
        image.name.clone()
    };
    render(&Viewer {
        album: album.id.clone(),
        src,
        width,
        height,
        caption: image.caption.clone(),
        alt: crate::markdown::plain(&image.caption),
        position: at + 1,
        total: images.len(),
        prev: at
            .checked_sub(1)
            .and_then(|p| images.get(p))
            .map(|i| i.id.clone()),
        next: images.get(at + 1).map(|i| i.id.clone()),
        is_cover: cover == Some(image.id.as_str()),
        links: Links {
            page,
            markdown: format!("![{alt}]({direct})"),
            bbcode: format!("[img]{direct}[/img]"),
            html,
            file: direct,
        },
        download: format!("{}.{}", image.id, image.kind.ext()),
        file,
        name,
        width_full: image.width,
        height_full: image.height,
        size: human(image.bytes),
        animated: image.animated,
        clip: (image.is_video() && image.ready()).then(|| crate::web::public::clip(&image)),
        video: image.is_video(),
        length: (image.is_video() && image.ready()).then(|| crate::video::clock(image.seconds())),
        progress: (image.status == Status::Processing)
            .then(|| state.uploads.jobs.progress(&image.id).unwrap_or(0)),
        failure: (image.status == Status::Failed).then(|| image.failure.clone()),
        views: view_counts(state, &image.id).await?,
        id: image.id,
    })
}

#[derive(serde::Deserialize)]
pub struct StreamQuery {
    album: Option<String>,
}

/// The page's live parts, redrawn after each change to the person's albums
/// and sent when they changed. Changes come in bursts (an upload of three
/// hundred images is three hundred), so a redraw waits a moment for the
/// burst to settle. A video's progress on an album page is sent as its bar
/// alone, rather than the grid redrawn for each step.
pub async fn stream(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Query(q): Query<StreamQuery>,
) -> Response {
    let live = Live {
        changes: state.db.changes(),
        ticks: state.uploads.jobs.ticks(),
        me: session.username.as_str().into(),
        stopping: state.stopping.clone(),
        // A change to the account (a new role or password, its deletion)
        // since its session was read ends the stream; the page's next request
        // is authenticated as things then stand.
        account: session.mark,
        state,
        session,
        album: q.album,
        sent: HashMap::new(),
        shown: HashSet::new(),
        next: Next::Draw,
    };
    let events = futures_util::stream::unfold(live, |mut live| async move {
        let batch = live.step().await?;
        Some((futures_util::stream::iter(batch), live))
    })
    .flatten();
    askama_datastar::stream(events)
}

/// What a live stream does next.
enum Next {
    /// Draws the parts and sends those that changed.
    Draw,
    /// Waits for a change of the person's, sending progress meanwhile.
    Wait,
    /// The album is gone: stays open, quiet, until the server or the account
    /// changes, since the page would open one that ended again.
    Gone,
    Done,
}

/// A live stream's state between the events it sends.
struct Live {
    state: AppState,
    session: Session,
    album: Option<String>,
    changes: tokio::sync::broadcast::Receiver<std::sync::Arc<str>>,
    ticks: tokio::sync::broadcast::Receiver<crate::jobs::Tick>,
    me: std::sync::Arc<str>,
    stopping: tokio::sync::watch::Receiver<bool>,
    account: u64,
    /// Each part as last sent.
    sent: HashMap<&'static str, Fragment>,
    /// The album's images as last drawn: progress is sent for these.
    shown: HashSet<String>,
    next: Next,
}

impl Live {
    /// The events of the stream's next step, or `None` once it ends.
    async fn step(&mut self) -> Option<Vec<PatchElements>> {
        match self.next {
            Next::Draw => {
                self.next = Next::Wait;
                Some(self.draw().await)
            }
            Next::Wait => Some(self.wait().await),
            Next::Gone => {
                tokio::select! {
                    _ = self.stopping.wait_for(|s| *s) => {}
                    () = koan_auth::changes::account_changed_since(self.session.user_id, self.account) => {}
                }
                None
            }
            Next::Done => None,
        }
    }

    /// The parts that differ from those last sent. A database that could not
    /// answer (a long write holding the lock) is logged and asked again on
    /// the next write.
    async fn draw(&mut self) -> Vec<PatchElements> {
        let parts = match &self.album {
            Some(id) => match owned(&self.state, &self.session, id).await {
                Ok(album) => album_parts(&self.state, &album)
                    .await
                    .map(|(g, s, f, ids)| {
                        self.shown = ids.into_iter().collect();
                        vec![("grid", g), ("summary", s), ("foot", f)]
                    }),
                Err(AppError::NotFound) => {
                    self.next = Next::Gone;
                    return match render(&Deleted) {
                        Ok(html) => vec![html.patch()],
                        Err(e) => {
                            tracing::error!(error = %e, "drawing a deleted album failed");
                            Vec::new()
                        }
                    };
                }
                Err(e) => Err(e),
            },
            None => library_parts(&self.state, &self.session)
                .await
                .map(|(a, u)| vec![("albums", a), ("usage", u)]),
        };
        match parts {
            Ok(parts) => {
                let mut changed = Vec::new();
                for (key, html) in parts {
                    if self.sent.get(key) != Some(&html) {
                        changed.push(html.patch());
                        self.sent.insert(key, html);
                    }
                }
                changed
            }
            Err(e) => {
                tracing::warn!(error = %e, "drawing a live part failed");
                Vec::new()
            }
        }
    }

    /// The next progress of one of the album's videos, or nothing once a
    /// change of the person's is due a redraw. Changes come in bursts (an
    /// upload of three hundred images is three hundred), so a redraw waits a
    /// moment for the burst to settle.
    async fn wait(&mut self) -> Vec<PatchElements> {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};
        let woken = loop {
            let tick = tokio::select! {
                change = self.changes.recv() => match change {
                    Ok(owner) if owner == self.me => break true,
                    Ok(_) => continue,
                    // Missed some, which may have been theirs.
                    Err(RecvError::Lagged(_)) => break true,
                    Err(RecvError::Closed) => break false,
                },
                tick = self.ticks.recv(), if self.album.is_some() => tick,
                _ = self.stopping.wait_for(|s| *s) => break false,
                () = koan_auth::changes::account_changed_since(self.session.user_id, self.account) => break false,
            };
            if let Ok(tick) = tick
                && tick.owner == self.me
                && self.shown.contains(&tick.id)
            {
                return vec![progress(&tick.id, tick.done).patch()];
            }
        };
        if !woken {
            self.next = Next::Done;
            return Vec::new();
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        // The redraw covers whatever arrived meanwhile.
        while !matches!(
            self.changes.try_recv(),
            Err(TryRecvError::Empty | TryRecvError::Closed)
        ) {}
        self.next = Next::Draw;
        Vec::new()
    }
}
