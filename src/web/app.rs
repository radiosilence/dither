//! The app: a person's albums, an album to edit, and the image viewer.
//!
//! Pages are drawn whole on the server. Datastar patches parts of them in
//! place: a live stream redraws the album grid and the library whenever a
//! write lands (an upload finishing, another device reordering), sending a
//! part only when it differs from what was sent. Fields save as they are
//! typed in. The upload queue, selection and drag-to-reorder are app.js's,
//! since they live in the browser.

use std::collections::HashMap;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Extension, Json};

use crate::auth::session::Session;
use crate::db::{AlbumEdit, Card, Image};
use crate::error::{AppError, AppResult};
use crate::media::{DISPLAY_MAX, fit};
use crate::upload::CHUNK;
use crate::web::Helpers as _;
use crate::web::{AppState, count, human, on_public_host, patch, sse};

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
    chunk: u64,
}

impl Shell {
    fn max_label(&self) -> String {
        human(self.max_upload)
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
    meta: String,
    cover: Option<String>,
    private: bool,
}

impl CardView {
    fn new(c: &Card) -> Self {
        let mut meta = count(c.count, "image", "images");
        if c.bytes > 0 {
            meta.push_str(&format!(" · {}", human(c.bytes)));
        }
        Self {
            id: c.album.id.clone(),
            title: c.album.title.clone(),
            meta,
            cover: c.cover.as_ref().map(|i| format!("/t/{}.webp", i.id)),
            private: c.album.private,
        }
    }
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
}

impl Tile {
    fn new(i: &Image) -> Self {
        Self {
            id: i.id.clone(),
            thumb: format!("/t/{}.webp", i.id),
            animated: i.animated,
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
    position: String,
    prev: Option<String>,
    next: Option<String>,
    is_cover: bool,
    links: Vec<(&'static str, String)>,
    file: String,
    download: String,
    meta: String,
}

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
    let (n, bytes) = state.db.usage(&session.sub).await?;
    let usage = format!(
        "<p id=\"usage\" class=\"mt-8 text-fine text-muted\">{} · {} · {}</p>",
        count(cards.len() as u64, "album", "albums"),
        count(n, "image", "images"),
        human(bytes)
    );
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

pub async fn album(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let album = owned(&state, &session, &id).await?;
    let (grid, summary) = album_parts(&state, &album).await?;
    Ok(Html(render(&AlbumPage {
        shell: shell(&state, &session).await,
        share: format!("{}/a/{}", state.config.public_url, album.id),
        id: album.id,
        title: album.title,
        description: album.description,
        private: album.private,
        grid,
        summary,
    })?))
}

/// An album page's live parts: the grid of images and the line counting them.
async fn album_parts(state: &AppState, album: &crate::db::Album) -> AppResult<(String, String)> {
    let images = state.db.images(&album.id).await?;
    let cover = album
        .cover
        .clone()
        .or_else(|| images.first().map(|i| i.id.clone()))
        .unwrap_or_default();
    let grid = render(&Grid {
        album: album.id.clone(),
        cover,
        tiles: images.iter().map(Tile::new).collect(),
    })?;
    let bytes: u64 = images.iter().map(|i| i.bytes).sum();
    let summary = format!(
        "<span id=\"summary\">{}{}</span>",
        count(images.len() as u64, "image", "images"),
        if bytes > 0 {
            format!(" · {}", human(bytes))
        } else {
            String::new()
        }
    );
    Ok((grid, summary))
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
    for image in &images {
        state.store.delete_image(image).await;
    }
    tracing::info!(album = %id, images = images.len(), "album deleted");
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
    for image in &deleted {
        state.store.delete_image(image).await;
    }
    Ok(Json(serde_json::json!({ "deleted": deleted.len() })))
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
    let (src, width, height) = if image.display {
        let (w, h) = fit(image.width, image.height, DISPLAY_MAX);
        (format!("/d/{}.webp", image.id), w, h)
    } else {
        (file.clone(), image.width, image.height)
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
        position: format!("{} of {}", at + 1, images.len()),
        prev: at
            .checked_sub(1)
            .and_then(|p| images.get(p))
            .map(|i| i.id.clone()),
        next: images.get(at + 1).map(|i| i.id.clone()),
        is_cover: cover == Some(image.id.as_str()),
        links: vec![
            ("page", page),
            ("image", direct.clone()),
            ("markdown", format!("![{alt}]({direct})")),
            ("bbcode", format!("[img]{direct}[/img]")),
            ("html", format!("<img src=\"{direct}\" alt=\"{alt}\">")),
        ],
        download: format!("{}.{}", image.id, image.kind.ext()),
        file,
        meta: format!(
            "{} · {}×{} · {}{}",
            name,
            image.width,
            image.height,
            human(image.bytes),
            if image.animated { " · animated" } else { "" }
        ),
        id: image.id,
    })
}

#[derive(serde::Deserialize)]
pub struct StreamQuery {
    album: Option<String>,
}

/// The page's live parts, redrawn after every write and sent when they
/// changed. Writes come in bursts (an upload of three hundred images is three
/// hundred), so a redraw waits a moment for the burst to settle.
pub async fn stream(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Query(q): Query<StreamQuery>,
) -> Response {
    let mut writes = state.db.writes();
    let mut stopping = state.stopping.clone();
    let events = async_stream::stream! {
        let mut sent: HashMap<&'static str, String> = HashMap::new();
        loop {
            let parts = match &q.album {
                Some(id) => match owned(&state, &session, id).await {
                    Ok(album) => album_parts(&state, &album).await.map(|(g, s)| vec![("grid", g), ("summary", s)]),
                    Err(_) => {
                        yield Ok(patch("<div id=\"grid\" class=\"py-16 text-muted\">This album has been deleted.</div>"));
                        break;
                    }
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
            tokio::select! {
                changed = writes.changed() => if changed.is_err() { break },
                _ = stopping.wait_for(|s| *s) => break,
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            writes.borrow_and_update();
        }
    };
    sse(events)
}
