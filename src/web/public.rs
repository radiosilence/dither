//! What anyone with a link sees: an album, an image's page, and the image
//! files themselves.
//!
//! The pages are plain HTML and CSS, no script, with the Open Graph and
//! Twitter card tags link previews are built from (Reddit, Discord, iMessage,
//! Slack and the rest), and `noindex`, since an unlisted album should be
//! found by its link and not by a search engine. `robots.txt` allows every
//! crawler: some preview fetchers obey it, and a disallow would leave a link
//! without its picture.
//!
//! Image files are served with their sniffed type, `nosniff` and a sandboxing
//! policy, so no browser treats one as a document, and with permission to be
//! embedded anywhere, which is the point of an image host. Their bytes never
//! change under an address, so they are cached for a year.

use askama::Template;
use askama_gettext::Translate as _;
use axum::body::Body as HttpBody;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use futures_util::StreamExt;

use crate::db::{Album, Image};
use crate::media::{DISPLAY_MAX, fit};
use crate::metrics::{self, Served};
use crate::store::{self, Variant};
use crate::web::Helpers as _;
use crate::web::{AppState, human, on_public_host};

/// Public pages: no script at all, images and styles from here only.
const CSP_PUBLIC: &str = "default-src 'none'; img-src 'self'; style-src 'self'; font-src 'self'; \
     form-action 'none'; frame-ancestors 'none'; base-uri 'none'";
/// Image files: nothing at all, in a sandbox, should a browser ever render
/// one as a document.
const CSP_FILE: &str = "default-src 'none'; sandbox";
/// Previews of an og:image larger than this fail on some sites (X's limit is
/// 5 MB), so a larger original is named by its display copy instead.
const OG_IMAGE_MAX: u64 = 5_000_000;

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
        }
    }
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
}

fn og_image(public_url: &str, i: &Image) -> OgImage {
    if i.bytes > OG_IMAGE_MAX && i.display {
        let (width, height) = fit(i.width, i.height, DISPLAY_MAX);
        OgImage {
            url: format!("{public_url}/d/{}.webp", i.id),
            mime: "image/webp",
            width,
            height,
            alt: i.caption.clone(),
        }
    } else {
        OgImage {
            url: format!("{public_url}/{}.{}", i.id, i.kind.ext()),
            mime: i.kind.mime(),
            width: i.width,
            height: i.height,
            alt: i.caption.clone(),
        }
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

fn page(html: String, private: bool) -> Response {
    let cache = if private {
        "private, no-store"
    } else {
        "public, max-age=60"
    };
    (
        [
            (header::CONTENT_SECURITY_POLICY, CSP_PUBLIC),
            (header::CACHE_CONTROL, cache),
            (header::HeaderName::from_static("x-robots-tag"), "noindex"),
        ],
        Html(html),
    )
        .into_response()
}

pub async fn not_found() -> Response {
    let html = Missing.render().unwrap_or_default();
    let mut response = page(html, false);
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// The public domain's front page, which says nothing: a name there would
/// be a thread from the domain back to whoever runs it.
pub async fn landing() -> Response {
    page(Landing.render().unwrap_or_default(), false)
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
async fn visible(state: &AppState, headers: &HeaderMap, album: &Album) -> bool {
    if !album.private {
        return true;
    }
    if on_public_host(&state.config, headers) {
        return false;
    }
    crate::auth::extract::current(state, headers)
        .await
        .is_some_and(|s| s.sub == album.owner)
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
    if !visible(&state, &headers, &album).await {
        return not_found().await;
    }
    let Ok(images) = state.db.images(&id).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
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
    };
    let html = AlbumPage {
        og,
        title: album.title.clone(),
        description: album.description.clone(),
        count: images.len() as u64,
        pics: images.iter().map(Pic::new).collect(),
    }
    .render();
    metrics::served(Served::Page, 0);
    match html {
        Ok(html) => page(html, album.private),
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
    if !visible(state, headers, &album).await {
        return not_found().await;
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
    };
    let meta = format!(
        "{}×{} · {} · {}",
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
        pic: Pic::new(&image),
        meta,
        album: album_link,
    }
    .render();
    metrics::served(Served::Page, 0);
    match html {
        Ok(html) => page(html, album.private),
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
    match file.strip_suffix(".webp") {
        Some(id) => self::file(&state, &headers, id, Variant::Display, "webp").await,
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
    if !visible(state, headers, &album).await {
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
        Variant::Display => ("image/webp", Served::Display, 'd'),
    };
    let etag = image.etag(tag);
    let cache = if album.private {
        "private, no-store"
    } else {
        "public, max-age=31536000, immutable"
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
        HeaderValue::from_static(CSP_FILE),
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
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
    {
        metrics::not_modified();
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    let body = match state.store.get(&store::key(variant, id)).await {
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
            tracing::error!(image = id, error = %e, "reading the store failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    let filename = format!(
        "{id}.{}",
        if variant == Variant::Original {
            image.kind.ext()
        } else {
            "webp"
        }
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("inline; filename=\"{filename}\""))
            .expect("an id and an extension"),
    );
    match body {
        store::Body::Bytes(bytes) => {
            metrics::served(served, bytes.len() as u64);
            (h, bytes).into_response()
        }
        store::Body::Stream { size, stream } => {
            metrics::served(served, size);
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            let stream = stream.map(|r| r.map_err(std::io::Error::other));
            (h, HttpBody::from_stream(stream)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_is_the_first_line_cut_at_a_word() {
        assert_eq!(summary("\n  first line\nsecond", 50), "first line");
        assert_eq!(summary("one two three four", 9), "one two…");
        assert_eq!(summary("", 10), "");
    }
}
