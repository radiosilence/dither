//! The album page's viewer: an image large, with its caption to edit, the
//! ways to share it, and its neighbours in the album. Read into the page's
//! viewer dialog when a tile is opened, and drawn again as it moves on.
//!
//! The share snippets are templates of their own, one per format, so the
//! caption that becomes an image's alt text is escaped as each format needs:
//! as an HTML attribute, or as Markdown's link text.

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use koan_kit_web as kit;

use super::view_counts;
use crate::auth::session::Session;
use crate::db::Status;
use crate::error::{AppError, AppResult};
use crate::jobs::Failure;
use crate::media::{DISPLAY_MAX, fit};
use crate::web::layout::{Draw, Helpers as _, human};
use crate::web::public::clip::{Clip, clip};
use crate::web::{AppState, paths};

#[derive(Template)]
#[template(path = "album/viewer/body.html")]
pub(in crate::web) struct Viewer {
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
    /// The original, to download, and the name it downloads as.
    file: String,
    download: String,
    name: String,
    width_full: u32,
    height_full: u32,
    size: String,
    animated: bool,
    /// A video, ready to play.
    clip: Option<Clip>,
    video: bool,
    /// A video's length.
    length: Option<String>,
    /// A video being made: thousandths done.
    progress: Option<u16>,
    /// Why a video could not be made.
    failure: Option<Failure>,
    views: askama_datastar::Html,
}

impl Viewer {
    fn view_url(&self, id: &str) -> String {
        paths::viewer(id)
    }

    /// Its own id per image: morphed into the next image's, a textarea that
    /// has been typed in keeps what was typed, whatever the new one holds.
    fn caption_id(&self) -> String {
        format!("caption-{}", self.id)
    }

    fn caption_url(&self) -> String {
        paths::caption(&self.id)
    }

    fn cover_url(&self) -> String {
        paths::album_cover_of(&self.album, &self.id)
    }

    fn mover_url(&self) -> String {
        paths::album_mover(&self.album, &[&self.id])
    }

    fn delete_url(&self) -> String {
        paths::delete_images(&[&self.id])
    }
}

/// The ways the viewer offers to share an image, each as text to copy.
struct Links {
    page: String,
    file: String,
    markdown: String,
    bbcode: String,
    html: String,
}

/// An image or video as HTML for another site's page.
#[derive(Template)]
#[template(path = "album/viewer/share.html")]
struct ShareHtml<'a> {
    src: &'a str,
    alt: &'a str,
    /// A video's poster: the snippet is a `<video>`.
    poster: Option<&'a str>,
}

/// An image as Markdown.
#[derive(Template)]
#[template(path = "album/viewer/share.md", escape = "none")]
struct ShareMarkdown<'a> {
    src: &'a str,
    alt: &'a str,
}

impl ShareMarkdown<'_> {
    /// `alt` as Markdown's link text: the characters that would end or
    /// escape it, escaped.
    fn text(&self) -> String {
        let mut out = String::with_capacity(self.alt.len());
        for c in self.alt.chars() {
            if matches!(c, '\\' | '[' | ']') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
}

/// An image as forum BBCode.
#[derive(Template)]
#[template(path = "album/viewer/share.bbcode", escape = "none")]
struct ShareBbcode<'a> {
    src: &'a str,
}

crate::i18n::translated!(Viewer);

/// What a template rendered, as the text a copy field holds.
fn text(view: &impl Template) -> String {
    view.draw().into()
}

/// Image `id` in the viewer, if `session`'s.
pub(in crate::web) async fn region(
    state: &AppState,
    session: &Session,
    id: &str,
) -> AppResult<askama_datastar::Html> {
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
    let ext = image.kind.ext();
    let file = paths::original(&image.id, ext);
    let direct = paths::absolute(public, &file);
    let alt = crate::markdown::line(&image.caption);
    let (src, width, height) = if image.display && !image.is_video() {
        let (w, h) = fit(image.width, image.height, DISPLAY_MAX);
        (paths::display_image(&image.id), w, h)
    } else {
        (file.clone(), image.width, image.height)
    };
    let video = image.is_video();
    let player = (video && image.ready()).then(|| clip(&image));
    // An embedded video plays the copy every browser plays.
    let html = if video {
        let c = clip(&image);
        text(&ShareHtml {
            src: &paths::absolute(public, &c.src),
            alt: &alt,
            poster: Some(&paths::absolute(public, &c.poster)),
        })
    } else {
        text(&ShareHtml {
            src: &direct,
            alt: &alt,
            poster: None,
        })
    };
    let links = Links {
        page: paths::absolute(public, &paths::image_page(&image.id)),
        markdown: text(&ShareMarkdown {
            src: &direct,
            alt: &alt,
        }),
        bbcode: text(&ShareBbcode { src: &direct }),
        html,
        file: direct.clone(),
    };
    let cover = album
        .cover
        .as_deref()
        .or(images.first().map(|i| i.id.as_str()));
    let download = format!("{}.{ext}", image.id);
    let viewer = Viewer {
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
        links,
        name: if image.name.is_empty() {
            download.clone()
        } else {
            image.name.clone()
        },
        download,
        file,
        width_full: image.width,
        height_full: image.height,
        size: human(image.bytes),
        animated: image.animated,
        clip: player,
        video,
        length: (video && image.ready()).then(|| crate::video::clock(image.seconds())),
        progress: (image.status == Status::Processing)
            .then(|| state.uploads.jobs.progress(&image.id).unwrap_or(0)),
        failure: (image.status == Status::Failed).then(|| Failure::of(&image.failure)),
        views: view_counts(state, &image.id).await?.draw(),
        id: image.id,
    };
    Ok(viewer.draw())
}

/// `/images/<id>/view`: the viewer's body, morphed into its dialog.
pub(in crate::web) async fn view(
    State(state): State<AppState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<askama_datastar::Html> {
    region(&state, &session, &id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caption_is_escaped_as_each_format_needs() {
        let html = text(&ShareHtml {
            src: "https://x.test/a.jpg",
            alt: r#"a "b" <c> & d"#,
            poster: None,
        });
        assert_eq!(
            html.trim(),
            r#"<img src="https://x.test/a.jpg" alt="a &#34;b&#34; &#60;c&#62; &#38; d">"#
        );
        let md = text(&ShareMarkdown {
            src: "https://x.test/a.jpg",
            alt: r"a [b] \c",
        });
        assert_eq!(md.trim(), r"![a \[b\] \\c](https://x.test/a.jpg)");
    }
}
