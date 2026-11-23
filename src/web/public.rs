//! What anyone with a link sees: an album, an image's page, and the image
//! files themselves.
//!
//! The pages are plain HTML and CSS, with script only where there is a video,
//! for the player, which enhances a `<video>` that plays without it; with the
//! Open Graph and
//! Twitter card tags link previews are built from (Reddit, WhatsApp, Discord,
//! iMessage, Slack and the rest), and `noindex`, since an unlisted album should be
//! found by its link and not by a search engine. `robots.txt` allows every
//! crawler: some preview fetchers obey it, and a disallow would leave a link
//! without its picture.
//!
//! Image files are served with their sniffed type, `nosniff` and a sandboxing
//! policy, so no browser treats one as a document, and with permission to be
//! embedded anywhere, which is the point of an image host. Their bytes never
//! change under an address, so they are cached for a year.
//!
//! A preview's picture is its own copy, `/p/<id>.jpg`: a JPEG held to
//! [`crate::media::PREVIEW_BYTES`], since WhatsApp shows nothing for a larger
//! `og:image` and not every fetcher reads WebP. The worker makes it with the
//! other copies; nothing here decodes a picture. An animated picture small
//! enough is its own preview, so it plays where previews do.
//!
//! A video's page names the file that plays as `og:video` and a frame as its
//! picture. Every file is served with byte ranges, which seeking a video
//! needs and Safari requires before it plays one at all.

use askama::Template;
use askama_gettext::Translate as _;
use axum::body::Body as HttpBody;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use futures_util::StreamExt;

use crate::db::{Album, Image};
use crate::media::{DISPLAY_MAX, fit, preview_size};
use crate::metrics::{self, Served};
use crate::store::{self, Variant};
use crate::web::Helpers as _;
use crate::web::clip::Clip;
use crate::web::{AppState, human, on_public_host};

/// Public pages: no script at all, images and styles from here only.
const CSP_PUBLIC: &str = "default-src 'none'; img-src 'self'; style-src 'self'; font-src 'self'; \
     form-action 'none'; frame-ancestors 'none'; base-uri 'none'";
/// A public page holding a video: video, and the player's script, from here
/// too, and still nothing inline.
const CSP_PUBLIC_VIDEO: &str = "default-src 'none'; img-src 'self'; media-src 'self'; \
     script-src 'self'; style-src 'self'; font-src 'self'; form-action 'none'; \
     frame-ancestors 'none'; base-uri 'none'";
/// Image files: nothing at all, in a sandbox, should a browser ever render
/// one as a document.
const CSP_FILE: &str = "default-src 'none'; sandbox";
/// A video file. A browser opening one on its own plays it in a document of
/// its making, which this governs: it loads the video from here and styles
/// itself inline. Not sandboxed, since a sandboxed document has no origin of
/// its own for `'self'` to match, and Safari's then never loads the video; a
/// file served as video with `nosniff` is never read as a page regardless.
const CSP_VIDEO_FILE: &str = "default-src 'none'; media-src 'self'; style-src 'unsafe-inline'";
/// A range from the start ending before this byte is a probe of the file
/// rather than a look at it.
const PROBE_MAX: u64 = 16;
/// An animated picture up to this size is its own `og:image`, so it plays in
/// the previews that play one (X's limit is 5 MB).
const OG_ANIMATED_MAX: u64 = 5_000_000;

/// An image as a page draws it.
pub struct Pic {
    pub id: String,
    /// What to show: the display copy where there is one.
    pub src: String,
    /// The display copy and the original by width, for a browser to choose.
    pub srcset: String,
    /// The original file.
    pub file: String,
    pub width: u32,
    pub height: u32,
    pub caption: String,
    /// Portrait: drawn narrower in an album, so it fits a screen.
    pub tall: bool,
    /// A video, drawn by the player rather than as a picture.
    pub clip: Option<Clip>,
}

/// Short silent videos play on a loop, as a GIF would.
const LOOP_MAX_SECONDS: f64 = 30.0;

/// A video as the player draws it.
pub fn clip(i: &Image) -> Clip {
    let file = format!("/{}.{}", i.id, i.kind.ext());
    let (width, height) = if i.display {
        fit(i.width, i.height, crate::video::PLAY_MAX)
    } else {
        (i.width, i.height)
    };
    let seconds = i.seconds();
    Clip {
        id: i.id.clone(),
        src: if i.display {
            format!("/d/{}.mp4", i.id)
        } else {
            file.clone()
        },
        poster: format!("/p/{}.jpg", i.id),
        file,
        width,
        height,
        seconds,
        audio: i.audio,
        looping: !i.audio && seconds <= LOOP_MAX_SECONDS,
        // Never the name it was uploaded under, which is its owner's: a
        // phone's says little, a laptop's can say a lot.
        title: i
            .caption
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

impl Pic {
    pub fn new(i: &Image) -> Self {
        let file = format!("/{}.{}", i.id, i.kind.ext());
        let (src, srcset) = if i.display {
            let (dw, _) = fit(i.width, i.height, DISPLAY_MAX);
            let display = format!("/d/{}.webp", i.id);
            let srcset = format!("{display} {dw}w, {file} {}w", i.width);
            (display, srcset)
        } else {
            (file.clone(), String::new())
        };
        Self {
            id: i.id.clone(),
            src,
            srcset,
            file,
            width: i.width,
            height: i.height,
            caption: i.caption.clone(),
            tall: u64::from(i.height) * 20 > u64::from(i.width) * 23,
            clip: i.is_video().then(|| clip(i)),
        }
    }
}

/// What a video's preview plays.
pub struct OgVideo {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

pub struct OgImage {
    pub url: String,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub alt: String,
}

/// What a link preview is built from.
pub struct Og {
    /// What a person called it: an album's title, an image's caption. Empty
    /// when they called it nothing, and the template words a fallback.
    pub title: String,
    /// An album's image count, for that fallback; `None` for an image.
    pub album_count: Option<u64>,
    pub description: String,
    pub url: String,
    pub image: Option<OgImage>,
    pub video: Option<OgVideo>,
}

fn og_video(public_url: &str, i: &Image) -> Option<OgVideo> {
    i.is_video().then(|| {
        let c = clip(i);
        OgVideo {
            url: format!("{public_url}{}", c.src),
            width: c.width,
            height: c.height,
        }
    })
}

fn og_image(public_url: &str, i: &Image) -> OgImage {
    if !i.is_video() && i.animated && i.bytes <= OG_ANIMATED_MAX {
        return OgImage {
            url: format!("{public_url}/{}.{}", i.id, i.kind.ext()),
            mime: i.kind.mime(),
            width: i.width,
            height: i.height,
            alt: i.caption.clone(),
        };
    }
    let (width, height) = preview_size(i.width, i.height, i.animated);
    OgImage {
        url: format!("{public_url}/p/{}.jpg", i.id),
        mime: "image/jpeg",
        width,
        height,
        alt: i.caption.clone(),
    }
}

/// The first line of `text`, cut to about `max` characters at a word.
fn summary(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end())
}

#[derive(Template)]
#[template(path = "public/album.html")]
struct AlbumPage {
    og: Og,
    /// Whether it holds a video, and so needs the player's script.
    video: bool,
    title: String,
    description: String,
    count: u64,
    pics: Vec<Pic>,
}

pub struct AlbumLink {
    pub href: String,
    pub title: String,
    pub count: u64,
}

#[derive(Template)]
#[template(path = "public/image.html")]
struct ImagePage {
    og: Og,
    video: bool,
    pic: Pic,
    meta: String,
    /// The album, when it holds more than this image.
    album: Option<AlbumLink>,
}

#[derive(Template)]
#[template(path = "public/missing.html")]
struct Missing;

#[derive(Template)]
#[template(path = "public/landing.html")]
struct Landing;

crate::i18n::translated!(AlbumPage, ImagePage, Missing, Landing);

fn page(html: String, private: bool, video: bool) -> Response {
    let cache = if private {
        "private, no-store"
    } else {
        "public, max-age=60"
    };
    (
        [
            (
                header::CONTENT_SECURITY_POLICY,
                if video { CSP_PUBLIC_VIDEO } else { CSP_PUBLIC },
            ),
            (header::CACHE_CONTROL, cache),
            (header::HeaderName::from_static("x-robots-tag"), "noindex"),
            // The page's own requests for its files say they are its, so the
            // files are not counted as views of their own; other sites learn
            // nothing.
            (header::REFERRER_POLICY, "same-origin"),
        ],
        Html(html),
    )
        .into_response()
}

/// `response`, marked as serving `album` (and `image`) for the counting
/// layer, and as a view or not. A private album is only ever its owner's,
/// whom nothing counts.
fn counted(
    mut response: Response,
    album: &Album,
    image: Option<&str>,
    kind: Served,
    view: bool,
) -> Response {
    if album.private {
        return response;
    }
    response.extensions_mut().insert(crate::views::Counted {
        album: album.id.clone(),
        image: image.map(str::to_string),
        owner: album.owner.clone(),
        kind,
        view,
    });
    response
}

/// What is not there yet, an upload still being made: a 404 that nothing
/// keeps, since its link is handed out before it is ready.
async fn not_yet() -> Response {
    let mut response = not_found().await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub async fn not_found() -> Response {
    let html = Missing.render().unwrap_or_default();
    let mut response = page(html, false, false);
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// The public domain's front page, which says nothing: a name there would
/// be a thread from the domain back to whoever runs it.
pub async fn landing() -> Response {
    page(Landing.render().unwrap_or_default(), false, false)
}

pub async fn robots() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nAllow: /\n",
    )
        .into_response()
}

/// Whether the person asking may see `album`. A private album is its
/// owner's alone, and never on the public domain, which carries no session.
/// Whether `album` is shown to who is asking.
#[derive(PartialEq)]
enum Shown {
    Yes,
    No,
    /// To its owner, whose sign-in is a day old: a page load goes back
    /// through the identity provider first, as the app's pages do.
    Recheck,
}

async fn visible(state: &AppState, headers: &HeaderMap, album: &Album) -> Shown {
    if !album.private {
        return Shown::Yes;
    }
    if on_public_host(&state.config, headers) {
        return Shown::No;
    }
    match crate::auth::extract::current(state, headers).await {
        Some(s) if s.sub == album.owner && crate::auth::extract::stale(state, &s) => Shown::Recheck,
        Some(s) if s.sub == album.owner => Shown::Yes,
        _ => Shown::No,
    }
}

/// A page of a private album its owner asked for on a day-old sign-in.
fn recheck(path: &str) -> Response {
    axum::response::Redirect::to(&crate::auth::routes::login_url(path)).into_response()
}

pub async fn album(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !crate::id::valid(&id) {
        return not_found().await;
    }
    let Ok(Some(album)) = state.db.album(&id).await else {
        return not_found().await;
    };
    match visible(&state, &headers, &album).await {
        Shown::Yes => {}
        Shown::No => return not_found().await,
        Shown::Recheck => return recheck(&format!("/a/{id}")),
    }
    let Ok(images) = state.db.images(&id).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // Videos still being made, or that failed, are their owner's business.
    let images: Vec<Image> = images.into_iter().filter(Image::ready).collect();
    let public = &state.config.public_url;
    let cover = album
        .cover
        .as_ref()
        .and_then(|c| images.iter().find(|i| &i.id == c))
        .or(images.first());
    let description = if album.description.is_empty() {
        images
            .iter()
            .find(|i| !i.caption.is_empty())
            .map(|i| summary(&i.caption, 200))
            .unwrap_or_default()
    } else {
        summary(&album.description, 200)
    };
    let og = Og {
        title: album.title.clone(),
        album_count: Some(images.len() as u64),
        description,
        url: format!("{public}/a/{id}"),
        image: cover.map(|c| og_image(public, c)),
        video: cover.and_then(|c| og_video(public, c)),
    };
    let video = images.iter().any(Image::is_video);
    let html = AlbumPage {
        og,
        video,
        title: album.title.clone(),
        description: album.description.clone(),
        count: images.len() as u64,
        pics: images.iter().map(Pic::new).collect(),
    }
    .render();
    metrics::served(Served::Page, 0);
    match html {
        Ok(html) => counted(
            page(html, album.private, video),
            &album,
            None,
            Served::Page,
            true,
        ),
        Err(e) => {
            tracing::error!(error = %e, "rendering an album failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `/<id>` is an image's page; `/<id>.<ext>` is the image.
pub async fn image(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    match slug.split_once('.') {
        Some((id, ext)) => file(&state, &headers, id, Variant::Original, ext).await,
        None if slug.is_empty() => not_found().await,
        None => image_page(&state, &headers, &slug).await,
    }
}

async fn image_page(state: &AppState, headers: &HeaderMap, id: &str) -> Response {
    if !crate::id::valid(id) {
        return not_found().await;
    }
    let Ok(Some((image, album))) = state.db.image(id).await else {
        return not_found().await;
    };
    match visible(state, headers, &album).await {
        Shown::Yes => {}
        Shown::No => return not_found().await,
        Shown::Recheck => return recheck(&format!("/{id}")),
    }
    if !image.ready() {
        return not_yet().await;
    }
    let siblings = state.db.count(&album.id).await.unwrap_or(1);
    let public = &state.config.public_url;
    let og = Og {
        title: if image.caption.is_empty() {
            album.title.clone()
        } else {
            summary(&image.caption, 90)
        },
        album_count: None,
        description: if image.caption.is_empty() {
            String::new()
        } else {
            summary(&image.caption, 200)
        },
        url: format!("{public}/{id}"),
        image: Some(og_image(public, &image)),
        video: og_video(public, &image),
    };
    let length = if image.is_video() {
        format!(" · {}", crate::video::clock(image.seconds()))
    } else {
        String::new()
    };
    let meta = format!(
        "{}×{}{length} · {} · {}",
        image.width,
        image.height,
        human(image.bytes),
        image.kind.ext()
    );
    let album_link = (siblings > 1).then(|| AlbumLink {
        href: format!("/a/{}#{id}", album.id),
        title: album.title.clone(),
        count: siblings,
    });
    let html = ImagePage {
        og,
        video: image.is_video(),
        pic: Pic::new(&image),
        meta,
        album: album_link,
    }
    .render();
    metrics::served(Served::Page, 0);
    match html {
        Ok(html) => {
            let video = image.is_video();
            counted(
                page(html, album.private, video),
                &album,
                Some(id),
                Served::Page,
                true,
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "rendering an image page failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn thumb(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.strip_suffix(".webp") {
        Some(id) => self::file(&state, &headers, id, Variant::Thumb, "webp").await,
        None => not_found().await,
    }
}

pub async fn display(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.rsplit_once('.') {
        Some((id, ext @ ("webp" | "mp4"))) => {
            self::file(&state, &headers, id, Variant::Display, ext).await
        }
        _ => not_found().await,
    }
}

pub async fn preview(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    match file.strip_suffix(".jpg") {
        Some(id) => self::file(&state, &headers, id, Variant::Preview, "jpg").await,
        None => not_found().await,
    }
}

async fn file(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    variant: Variant,
    ext: &str,
) -> Response {
    if !crate::id::valid(id) {
        return not_found().await;
    }
    let Ok(Some((image, album))) = state.db.image(id).await else {
        return not_found().await;
    };
    if visible(state, headers, &album).await == Shown::No {
        return not_found().await;
    }
    if !image.ready() {
        return not_yet().await;
    }
    // A video's display copy is what plays, an image's a smaller picture.
    let display = if image.is_video() { "mp4" } else { "webp" };
    if variant == Variant::Display && ext != display {
        return not_found().await;
    }
    // A picture without a display copy is its own.
    let variant = if variant == Variant::Display && !image.display {
        Variant::Original
    } else {
        variant
    };
    let (mime, served, tag) = match variant {
        Variant::Original => {
            // `/abc.png` for a JPEG is a typo away from right: send it there.
            if !image.kind.matches_ext(ext) {
                let to = format!("/{id}.{}", image.kind.ext());
                return (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response();
            }
            (image.kind.mime(), Served::Original, 'o')
        }
        Variant::Thumb => ("image/webp", Served::Thumb, 't'),
        Variant::Display if image.is_video() => ("video/mp4", Served::Display, 'd'),
        Variant::Display => ("image/webp", Served::Display, 'd'),
        Variant::Preview => ("image/jpeg", Served::Preview, 'p'),
    };
    let etag = image.etag(tag);
    // What someone looks at on its own: the original, or a video's display
    // copy, which the viewer's HTML embeds. Thumbnails and previews are only
    // ever parts of pages and previews.
    let viewable = matches!(served, Served::Original | Served::Display);
    // A shared cache (a CDN in front) keeps a file ten minutes, so making its
    // album private or deleting it stops it being served within that. A
    // browser keeps it for good: whoever has it has seen it.
    let cache = if album.private {
        "private, no-store"
    } else {
        "public, max-age=31536000, s-maxage=600, immutable"
    };
    let modified = httpdate::fmt_http_date(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(image.created_at as u64),
    );
    let mut h = HeaderMap::new();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&modified).expect("an HTTP date"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if mime.starts_with("video/") {
            CSP_VIDEO_FILE
        } else {
            CSP_FILE
        }),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("cross-origin"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert("x-robots-tag", HeaderValue::from_static("noindex"));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
    {
        metrics::not_modified();
        // A revalidation: neither a view nor anything sent.
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let filename = format!(
        "{id}.{}",
        match variant {
            Variant::Original => image.kind.ext(),
            Variant::Preview => "jpg",
            Variant::Thumb => "webp",
            Variant::Display => display,
        }
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"{filename}\""))
            .expect("an id and an extension"),
    );
    let key = store::key(variant, id);
    if let Some(wanted) = Wanted::from(headers, &etag, &modified) {
        let response = partial(state, &key, wanted, served, h).await;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return response;
        }
        // A player's later ranges are the same view, and so is the probe
        // Safari sends before it plays (`bytes=0-1`).
        let view = viewable
            && match wanted {
                Wanted::Between(0, end) => end >= PROBE_MAX,
                Wanted::From(0) => true,
                _ => false,
            };
        return counted(response, &album, Some(id), served, view);
    }
    let body = match state.store.get(&key).await {
        Ok(Some(body)) => body,
        Ok(None) => {
            tracing::error!(
                image = id,
                ?variant,
                "an image's file is missing from the store"
            );
            return not_found().await;
        }
        Err(e) => {
            tracing::error!(image = id, ?variant, error = %e, "reading or making an image's file failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    match body {
        store::Body::Bytes(bytes) => {
            metrics::served(served, bytes.len() as u64);
            counted(
                (h, bytes).into_response(),
                &album,
                Some(id),
                served,
                viewable,
            )
        }
        store::Body::Stream { size, stream } => {
            metrics::served(served, size);
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            let stream = stream.map(|r| r.map_err(std::io::Error::other));
            let response = (h, HttpBody::from_stream(stream)).into_response();
            counted(response, &album, Some(id), served, viewable)
        }
    }
}

/// The one range of bytes a request asks for. Several ranges in one request
/// are answered with the whole file, which the standard allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    /// `bytes=a-b`, inclusive.
    Between(u64, u64),
    /// `bytes=a-`.
    From(u64),
    /// `bytes=-n`: the last `n`.
    Last(u64),
}

impl Wanted {
    /// The range `headers` ask for, unless an `If-Range` names something
    /// other than this file as it is (`etag`, or its `modified` date).
    fn from(headers: &HeaderMap, etag: &str, modified: &str) -> Option<Self> {
        if let Some(condition) = headers.get(header::IF_RANGE) {
            let condition = condition.to_str().ok()?.trim();
            if condition != etag && condition != modified {
                return None;
            }
        }
        Self::parse(headers.get(header::RANGE)?.to_str().ok()?)
    }

    fn parse(value: &str) -> Option<Self> {
        let spec = value.trim().strip_prefix("bytes=")?.trim();
        if spec.contains(',') {
            return None;
        }
        let (a, b) = spec.split_once('-')?;
        let (a, b) = (a.trim(), b.trim());
        match (a.is_empty(), b.is_empty()) {
            (false, false) => {
                let (a, b) = (a.parse().ok()?, b.parse().ok()?);
                (a <= b).then_some(Wanted::Between(a, b))
            }
            (false, true) => Some(Wanted::From(a.parse().ok()?)),
            (true, false) => Some(Wanted::Last(b.parse().ok()?)),
            (true, true) => None,
        }
    }

    fn range(self) -> object_store::GetRange {
        use object_store::GetRange;
        match self {
            Wanted::Between(a, b) => GetRange::Bounded(a..b.saturating_add(1)),
            Wanted::From(a) => GetRange::Offset(a),
            Wanted::Last(n) => GetRange::Suffix(n),
        }
    }
}

/// Part of a file, as a range request asked: 206 with the part, or 416 when
/// the range is outside the file.
async fn partial(
    state: &AppState,
    key: &object_store::path::Path,
    wanted: Wanted,
    served: Served,
    mut h: HeaderMap,
) -> Response {
    let (body, range, total) = match state.store.get_range(key, wanted.range()).await {
        Ok(Some(store::Part::Within { body, range, total })) if range.start < range.end => {
            (body, range, total)
        }
        // Some stores answer an empty range rather than refusing it.
        Ok(Some(store::Part::Within { total, .. } | store::Part::Outside { total })) => {
            h.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).expect("digits"),
            );
            return (StatusCode::RANGE_NOT_SATISFIABLE, h).into_response();
        }
        Ok(None) => {
            tracing::error!(%key, "an image's file is missing from the store");
            return not_found().await;
        }
        Err(e) => {
            tracing::error!(%key, error = %e, "reading the store failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let len = range.end - range.start;
    h.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {}-{}/{total}", range.start, range.end - 1))
            .expect("digits"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    metrics::served(served, len);
    let body = match body {
        store::Body::Bytes(bytes) => HttpBody::from(bytes),
        store::Body::Stream { stream, .. } => {
            HttpBody::from_stream(stream.map(|r| r.map_err(std::io::Error::other)))
        }
    };
    (StatusCode::PARTIAL_CONTENT, h, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_and_fall_within_the_file() {
        assert_eq!(Wanted::parse("bytes=0-99"), Some(Wanted::Between(0, 99)));
        assert_eq!(Wanted::parse("bytes=100-"), Some(Wanted::From(100)));
        assert_eq!(Wanted::parse("bytes=-500"), Some(Wanted::Last(500)));
        assert_eq!(Wanted::parse("bytes=0-1,5-6"), None);
        assert_eq!(Wanted::parse("bytes=9-3"), None);
        assert_eq!(Wanted::parse("items=0-1"), None);
        assert_eq!(Wanted::parse("bytes=-"), None);
        assert_eq!(
            Wanted::Between(0, 99).range(),
            object_store::GetRange::Bounded(0..100)
        );
    }

    #[test]
    fn a_summary_is_the_first_line_cut_at_a_word() {
        assert_eq!(summary("\n  first line\nsecond", 50), "first line");
        assert_eq!(summary("one two three four", 9), "one two…");
        assert_eq!(summary("", 10), "");
    }
}
