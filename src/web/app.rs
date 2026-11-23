//! The app: a person's albums, an album to edit, and the image viewer.
//!
//! Pages are drawn whole on the server. Datastar patches parts of them in
//! place: a live stream redraws the album grid and the library whenever a
//! write lands (an upload finishing, another device reordering), sending a
//! part only when it differs from what was sent. Fields save as they are
//! typed in. The upload queue, selection and drag-to-reorder are the
//! browser script's (browser/app.ts),
//! since they live in the browser.

use std::collections::HashMap;
use std::time::Duration;

use askama::Template;
use askama_gettext::Translate as _;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Extension, Json};

use crate::auth::session::Session;
use crate::db::{AlbumEdit, Card, Image, Status};
use crate::error::{AppError, AppResult};
use crate::media::{DISPLAY_MAX, fit};
use crate::upload::CHUNK;
use crate::web::Helpers as _;
use crate::web::{AppState, human, on_public_host, patch, sse};

/// Longest title, description and caption kept.
const TITLE_MAX: usize = 300;
const TEXT_MAX: usize = 20_000;

fn cut(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Facts the layout needs on every page.
struct Shell {
    login: String,
    max_upload: u64,
    /// The limit for a video; `None` when the server takes none.
    max_video: Option<u64>,
    chunk: u64,
}

impl Shell {
    fn max_label(&self) -> String {
        human(self.max_upload)
    }

    fn max_video_label(&self) -> String {
        self.max_video.map(human).unwrap_or_default()
    }
}

async fn shell(state: &AppState, session: &Session) -> Shell {
    Shell {
        login: state
            .db
            .login_of(&session.sub)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| session.sub.clone()),
        max_upload: state.config.max_upload_bytes,
        max_video: state.uploads.max_video_bytes(),
        chunk: CHUNK,
    }
}

#[derive(Template)]
#[template(path = "app/library.html")]
struct LibraryPage {
    shell: Shell,
    albums: String,
    usage: String,
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
    title: String,
    description: String,
    private: bool,
    share: String,
    grid: String,
    summary: String,
}

#[derive(Template)]
#[template(path = "app/grid.html")]
struct Grid {
    album: String,
    cover: String,
    tiles: Vec<Tile>,
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
            label: if i.caption.is_empty() {
                i.name.clone()
            } else {
                i.caption.clone()
            },
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
    Deleted
);

#[derive(Template)]
#[template(path = "app/picker.html")]
struct Picker {
    albums: Vec<CardView>,
}

fn render(t: &impl Template) -> AppResult<String> {
    t.render().map_err(|e| AppError::Internal(e.into()))
}

/// `/`: the library behind sign-in; on the public domain, nothing.
pub async fn home(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if on_public_host(&state.config, &headers) {
        return crate::web::public::landing().await;
    }
    let Some(session) = crate::auth::extract::current(&state, &headers).await else {
        return Redirect::to("/auth/login").into_response();
    };
    match library(&state, &session).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn library(state: &AppState, session: &Session) -> AppResult<String> {
    let (albums, usage) = library_parts(state, session).await?;
    render(&LibraryPage {
        shell: shell(state, session).await,
        albums,
        usage,
    })
}

/// The library's live parts: the albums, and what they add up to.
async fn library_parts(state: &AppState, session: &Session) -> AppResult<(String, String)> {
    let cards = state.db.cards(&session.sub).await?;
    let albums = render(&Albums {
        cards: cards.iter().map(CardView::new).collect(),
    })?;
    let (images, bytes) = state.db.usage(&session.sub).await?;
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
        Some(album) if album.owner == session.sub => Ok(album),
        _ => Err(AppError::NotFound),
    }
}

/// `/albums/<id>`: the album's editor for its owner. Its address is what a
/// browser's bar shows, so it is what people copy: anyone else is sent to the
/// page its link shares, unless the album is private, and someone not signed
/// in to sign-in, which says nothing of whether the album exists.
pub async fn album(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let session = crate::auth::extract::current(&state, &headers).await;
    let found = if crate::id::valid(&id) {
        state.db.album(&id).await.ok().flatten()
    } else {
        None
    };
    match (session, found) {
        (Some(session), Some(album)) if album.owner == session.sub => {
            match editor(&state, &session, album).await {
                Ok(html) => Html(html).into_response(),
                Err(e) => e.into_response(),
            }
        }
        (_, Some(album)) if !album.private => {
            Redirect::to(&format!("{}/a/{}", state.config.public_url, album.id)).into_response()
        }
        (None, _) => {
            Redirect::to(&crate::auth::routes::login_url(&format!("/albums/{id}"))).into_response()
        }
        (Some(_), _) => AppError::NotFound.into_response(),
    }
}

async fn editor(state: &AppState, session: &Session, album: crate::db::Album) -> AppResult<String> {
    let (grid, summary, _) = album_parts(state, &album).await?;
    render(&AlbumPage {
        shell: shell(state, session).await,
        share: format!("{}/a/{}", state.config.public_url, album.id),
        id: album.id,
        title: album.title,
        description: album.description,
        private: album.private,
        grid,
        summary,
    })
}

/// An album page's live parts: the grid of images and the line counting them,
/// and the images' ids.
async fn album_parts(
    state: &AppState,
    album: &crate::db::Album,
) -> AppResult<(String, String, Vec<String>)> {
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
    Ok((grid, summary, images.into_iter().map(|i| i.id).collect()))
}

pub async fn album_meta(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(mut edit): Json<AlbumEdit>,
) -> AppResult<StatusCode> {
    edit.title = edit.title.map(|t| cut(&t, TITLE_MAX));
    edit.description = edit.description.map(|d| cut(&d, TEXT_MAX));
    if state.db.edit_album(&session.sub, &id, edit).await? {
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
    if state.db.reorder(&session.sub, &id, body.ids).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[derive(serde::Deserialize)]
pub struct Cover {
    image: Option<String>,
}

/// Answered with the viewer redrawn, since that is where it was asked from.
pub async fn cover(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(body): Json<Cover>,
) -> AppResult<Response> {
    if !state
        .db
        .set_cover(&session.sub, &id, body.image.as_deref())
        .await?
    {
        return Err(AppError::NotFound);
    }
    Ok(match body.image {
        Some(image) => Html(viewer_html(&state, &session, &image).await?).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

pub async fn delete_album(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let images = state
        .db
        .delete_album(&session.sub, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let count = images.len();
    state.uploads.jobs.forget(&images).await;
    crate::store::discard(&state.store, &state.db, images).await;
    tracing::info!(album = %id, images = count, "album deleted");
    Ok(StatusCode::NO_CONTENT)
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
        .set_caption(&session.sub, &id, &cut(&body.caption, TEXT_MAX))
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
                .create_album(&session.sub, &cut(&body.title, TITLE_MAX))
                .await?
                .id
        }
    };
    let moved = state.db.move_images(&session.sub, body.ids, &to).await?;
    Ok(Json(serde_json::json!({ "moved": moved, "to": to })))
}

pub async fn delete_images(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Json(body): Json<Ids>,
) -> AppResult<Json<serde_json::Value>> {
    let deleted = state.db.delete_images(&session.sub, body.ids).await?;
    let count = deleted.len();
    state.uploads.jobs.forget(&deleted).await;
    crate::store::discard(&state.store, &state.db, deleted).await;
    Ok(Json(serde_json::json!({ "deleted": count })))
}

/// The move dialog's list: every other album of the person's.
pub async fn picker(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let cards = state.db.cards(&session.sub).await?;
    Ok(Html(render(&Picker {
        albums: cards
            .iter()
            .filter(|c| c.album.id != id)
            .map(CardView::new)
            .collect(),
    })?))
}

/// An image in the viewer: large, with its caption to edit, the links to
/// share it by, and its neighbours in the album.
pub async fn viewer(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    Ok(Html(viewer_html(&state, &session, &id).await?))
}

async fn viewer_html(state: &AppState, session: &Session, id: &str) -> AppResult<String> {
    if !crate::id::valid(id) {
        return Err(AppError::NotFound);
    }
    let (image, album) = state.db.image(id).await?.ok_or(AppError::NotFound)?;
    if album.owner != session.sub {
        return Err(AppError::NotFound);
    }
    let images = state.db.images(&album.id).await?;
    let at = images.iter().position(|i| i.id == id).unwrap_or_default();
    let public = &state.config.public_url;
    let file = format!("/{}.{}", image.id, image.kind.ext());
    let direct = format!("{public}{file}");
    let page = format!("{public}/{}", image.id);
    let alt = image
        .caption
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
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};
    let mut changes = state.db.changes();
    let mut ticks = state.uploads.jobs.ticks();
    let me: std::sync::Arc<str> = session.sub.as_str().into();
    let mut stopping = state.stopping.clone();
    let events = async_stream::stream! {
        let mut sent: HashMap<&'static str, String> = HashMap::new();
        // The album's images as last drawn: progress is sent for these.
        let mut shown = std::collections::HashSet::new();
        loop {
            let parts = match &q.album {
                Some(id) => match owned(&state, &session, id).await {
                    Ok(album) => album_parts(&state, &album).await.map(|(g, s, ids)| {
                        shown = ids.into_iter().collect();
                        vec![("grid", g), ("summary", s)]
                    }),
                    // Only a missing album ends the stream. A database that
                    // could not answer (a long write holding the lock) is
                    // logged below and asked again on the next write.
                    Err(AppError::NotFound) => {
                        match Deleted.render() {
                            Ok(html) => yield Ok(patch(&html)),
                            Err(e) => tracing::error!(error = %e, "drawing a deleted album failed"),
                        }
                        break;
                    }
                    Err(e) => Err(e),
                },
                None => library_parts(&state, &session).await.map(|(a, u)| vec![("albums", a), ("usage", u)]),
            };
            match parts {
                Ok(parts) => {
                    for (key, html) in parts {
                        if sent.get(key) != Some(&html) {
                            yield Ok(patch(&html));
                            sent.insert(key, html);
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "drawing a live part failed"),
            }
            let woken = loop {
                let tick = tokio::select! {
                    change = changes.recv() => match change {
                        Ok(owner) if owner == me => break true,
                        Ok(_) => continue,
                        // Missed some, which may have been theirs.
                        Err(RecvError::Lagged(_)) => break true,
                        Err(RecvError::Closed) => break false,
                    },
                    tick = ticks.recv(), if q.album.is_some() => tick,
                    _ = stopping.wait_for(|s| *s) => break false,
                };
                if let Ok(tick) = tick
                    && tick.owner == me
                    && shown.contains(&tick.id)
                {
                    yield Ok(patch(&format!(
                        "<progress id=\"vp-{}\" class=\"meter\" max=\"1000\" value=\"{}\"></progress>",
                        tick.id, tick.done
                    )));
                }
            };
            if !woken {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            // The redraw covers whatever arrived meanwhile.
            while !matches!(changes.try_recv(), Err(TryRecvError::Empty | TryRecvError::Closed)) {}
        }
    };
    sse(events)
}
