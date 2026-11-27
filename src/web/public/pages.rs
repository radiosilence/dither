//! What anyone with a link sees: an album, an image's page, and the public
//! domain's front page.
//!
//! The pages are plain HTML and CSS, with script only where there is a video,
//! for the player, which enhances a `<video>` that plays without it; with the
//! Open Graph and Twitter card tags link previews are built from (Reddit,
//! WhatsApp, Discord, iMessage, Slack and the rest), and `noindex`, since an
//! unlisted album should be found by its link and not by a search engine.
//!
//! A preview's picture is its own copy, `/p/<id>.jpg`: a JPEG held to
//! [`crate::media::PREVIEW_BYTES`], since WhatsApp shows nothing for a larger
//! `og:image` and not every fetcher reads WebP. The worker makes it with the
//! other copies; nothing here decodes a picture. An animated picture small
//! enough is its own preview, so it plays where previews do. A video's page
//! names the file that plays as `og:video` and a frame as its picture.

use std::sync::LazyLock;

use askama::Template;
use askama_datastar::Csp;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use koan_kit_web as kit;

use super::clip::{Clip, clip};
use super::files::{counted, not_found, visible};
use crate::db::Image;
use crate::media::{DISPLAY_MAX, fit, preview_size};
use crate::metrics::{self, Served};
use crate::web::layout::{Helpers as _, human};
use crate::web::{AppState, paths};

/// Public pages: nothing by default and nothing to post or fetch; images,
/// styles and the typeface from here. `data:` images are the kit's own glyphs
/// (a checkbox's tick, a select's chevron), which its stylesheet carries as
/// data URLs. A page without a video runs no script at all; one holding a
/// video plays it from here, with the player's script from here too, and
/// still nothing inline.
fn public_policy(video: bool) -> HeaderValue {
    let policy = Csp::new()
        .deny("default-src")
        .deny("connect-src")
        .deny("form-action");
    if video {
        policy.allow("media-src", "'self'")
    } else {
        policy.deny("script-src")
    }
    .header_value()
}
static CSP_PUBLIC: LazyLock<HeaderValue> = LazyLock::new(|| public_policy(false));
static CSP_PUBLIC_VIDEO: LazyLock<HeaderValue> = LazyLock::new(|| public_policy(true));
/// An animated picture up to this size is its own `og:image`, so it plays in
/// the previews that play one (X's limit is 5 MB).
const OG_ANIMATED_MAX: u64 = 5_000_000;

/// An image as a page draws it.
pub struct Pic {
    pub id: String,
    /// The image's own page.
    pub page: String,
    /// What to show: the display copy where there is one.
    pub src: String,
    /// The display copy and the original by width, for a browser to choose.
    pub srcset: String,
    /// The original file.
    pub file: String,
    pub width: u32,
    pub height: u32,
    /// The caption's words without its Markdown, for alt text.
    pub caption: String,
    /// The caption drawn from its Markdown.
    pub caption_html: askama_datastar::Html,
    /// Portrait: drawn narrower in an album, so it fits a screen.
    pub tall: bool,
    /// A video, drawn by the player rather than as a picture.
    pub clip: Option<Clip>,
}

impl Pic {
    pub fn new(i: &Image) -> Self {
        let file = paths::original(&i.id, i.kind.ext());
        let (src, srcset) = if i.display {
            let (dw, _) = fit(i.width, i.height, DISPLAY_MAX);
            let display = paths::display_image(&i.id);
            let srcset = format!("{display} {dw}w, {file} {}w", i.width);
            (display, srcset)
        } else {
            (file.clone(), String::new())
        };
        Self {
            id: i.id.clone(),
            page: paths::image_page(&i.id),
            src,
            srcset,
            file,
            width: i.width,
            height: i.height,
            caption: crate::markdown::plain(&i.caption),
            caption_html: crate::markdown::html(&i.caption),
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
            url: paths::absolute(public_url, &c.src),
            width: c.width,
            height: c.height,
        }
    })
}

fn og_image(public_url: &str, i: &Image) -> OgImage {
    if !i.is_video() && i.animated && i.bytes <= OG_ANIMATED_MAX {
        return OgImage {
            url: paths::absolute(public_url, &paths::original(&i.id, i.kind.ext())),
            mime: i.kind.mime(),
            width: i.width,
            height: i.height,
            alt: crate::markdown::plain(&i.caption),
        };
    }
    let (width, height) = preview_size(i.width, i.height, i.animated);
    OgImage {
        url: paths::absolute(public_url, &paths::preview(&i.id)),
        mime: "image/jpeg",
        width,
        height,
        alt: crate::markdown::plain(&i.caption),
    }
}

/// The first line of the words of Markdown `text`, cut to about `max`
/// characters at a word.
fn summary(text: &str, max: usize) -> String {
    let line = crate::markdown::line(text);
    if line.chars().count() <= max {
        return line;
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
    /// Drawn from its Markdown.
    description: askama_datastar::Html,
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
pub(super) struct Missing;

#[derive(Template)]
#[template(path = "public/landing.html")]
struct Landing;

crate::i18n::translated!(AlbumPage, ImagePage, Missing, Landing);

pub(super) fn page(html: String, private: bool, video: bool) -> Response {
    let cache = if private {
        "private, no-store"
    } else {
        "public, max-age=60"
    };
    let policy = if video {
        &CSP_PUBLIC_VIDEO
    } else {
        &CSP_PUBLIC
    };
    (
        [(header::CONTENT_SECURITY_POLICY, HeaderValue::clone(policy))],
        [
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

/// The public domain's front page, which says nothing: a name there would
/// be a thread from the domain back to whoever runs it.
pub async fn landing() -> Response {
    page(Landing.render().unwrap_or_default(), false, false)
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
            .map(|i| summary(&i.caption, 200))
            .find(|s| !s.is_empty())
            .unwrap_or_default()
    } else {
        summary(&album.description, 200)
    };
    let og = Og {
        title: album.title.clone(),
        album_count: Some(images.len() as u64),
        description,
        url: paths::absolute(public, &paths::public_album(&id)),
        image: cover.map(|c| og_image(public, c)),
        video: cover.and_then(|c| og_video(public, c)),
    };
    let video = images.iter().any(Image::is_video);
    let html = AlbumPage {
        og,
        video,
        title: album.title.clone(),
        description: crate::markdown::html(&album.description),
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

/// `/<id>`: an image's page. `/<id>.<ext>`, the image itself, is a file.
pub async fn image(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
) -> Response {
    match slug.split_once('.') {
        Some((id, ext)) => super::files::original(&state, &headers, id, ext).await,
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
    if !image.ready() {
        return not_found().await;
    }
    let siblings = state.db.count(&album.id).await.unwrap_or(1);
    let public = &state.config.public_url;
    let title = summary(&image.caption, 90);
    let og = Og {
        title: if title.is_empty() {
            album.title.clone()
        } else {
            title
        },
        album_count: None,
        description: summary(&image.caption, 200),
        url: paths::absolute(public, &paths::image_page(id)),
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
        href: paths::public_album_at(&album.id, id),
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
