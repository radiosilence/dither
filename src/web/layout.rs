//! What every signed-in page shares: the document around its body, its title,
//! the header, and the helpers every template calls.

use askama::Template;
use askama_datastar::Html;
use koan_kit_web as kit;

use super::{AppState, assets, paths};
use crate::auth::session::Session;

/// The document every signed-in page is drawn in, around the page's own body.
#[derive(Template)]
#[template(path = "layout.html")]
pub(super) struct Layout {
    pub(super) shell: Shell,
    pub(super) title: Html,
    pub(super) header: Html,
    pub(super) main: Html,
    /// The page's starting signals, as JSON.
    pub(super) signals: String,
}

/// Facts about the server and the person the layout needs on every page.
pub(super) struct Shell {
    pub(super) max_upload: u64,
    /// The limit for a video; `None` when the server takes none.
    pub(super) max_video: Option<u64>,
    pub(super) chunk: u64,
    /// The running version, shown to the signed-in only: the public pages do
    /// not say what they run.
    pub(super) version: &'static str,
}

impl Shell {
    pub(super) fn of(state: &AppState) -> Self {
        Self {
            max_upload: state.config.max_upload_bytes,
            max_video: state.uploads.max_video_bytes(),
            chunk: crate::upload::CHUNK,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// A signed-in page, as its title and the header name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Name {
    Albums,
    Album,
    Account,
    Accounts,
}

/// The document's title: what the page shows, then the app.
#[derive(Template)]
#[template(path = "layout/title.html")]
pub(super) struct Title<'a> {
    name: Name,
    /// An album's title; empty for one called nothing, which the template words.
    heading: &'a str,
}

impl<'a> Title<'a> {
    pub(super) fn new(name: Name, heading: &'a str) -> Self {
        Self { name, heading }
    }
}

/// The header: who is signed in, the pages they can reach, and signing out.
#[derive(Template)]
#[template(path = "layout/header.html")]
pub(super) struct Header<'a> {
    login: &'a str,
    /// Whether to offer the accounts page.
    admin: bool,
    name: Name,
}

impl<'a> Header<'a> {
    pub(super) fn new(session: &'a Session, name: Name) -> Self {
        Self {
            login: &session.username,
            admin: session.admin(),
            name,
        }
    }
}

/// The page's own region, which a link loading in place replaces whole, with
/// the page's live stream: its address changes with the page, and the stream
/// is opened afresh whenever `$url` does.
#[derive(Template)]
#[template(path = "layout/main.html")]
pub(super) struct Main<'a> {
    pub(super) stream: &'a str,
    pub(super) body: Html,
}

impl Main<'_> {
    fn live(&self) -> Html {
        askama_datastar::reopen_on(self.stream, &["url"])
    }
}

crate::i18n::translated!(Layout, Title<'_>, Header<'_>, Main<'_>, FlashRegion);

/// A view rendered as markup, which another template embeds as it is:
/// everything in it was escaped as the view was rendered. A view that fails to
/// render is logged and drawn as nothing.
pub(super) trait Draw: Template + Sized {
    fn draw(&self) -> Html {
        Html::render(self).unwrap_or_else(|e| {
            tracing::error!(error = %e, "could not render a view");
            Html::new()
        })
    }

    /// The view as markup for one of the kit's slots.
    fn markup(&self) -> kit::Markup {
        kit::Markup::render(self).unwrap_or_else(|e| {
            tracing::error!(error = %e, "could not render a view");
            kit::Markup::default()
        })
    }
}

impl<T: Template> Draw for T {}

/// What the templates call: askama makes a call in a template a method on the
/// template, so every template has these.
pub trait Helpers {
    fn css_url(&self) -> String {
        assets::css_url()
    }

    fn app_js_url(&self) -> String {
        assets::app_js_url()
    }

    fn player_js_url(&self) -> String {
        assets::player_js_url()
    }

    fn join_js_url(&self) -> String {
        assets::join_js_url()
    }

    fn icon_svg_url(&self) -> String {
        assets::icon_svg_url()
    }

    fn icon_png_url(&self) -> String {
        assets::icon_png_url()
    }

    /// The page's language, for `<html lang>`.
    fn lang(&self) -> &'static str {
        crate::i18n::LOCALE
    }

    /// A count from each plural form of the page's language, for the copy
    /// the browser's script reads.
    fn plural_samples(&self) -> impl Iterator<Item = u64> {
        crate::i18n::plural_samples().iter().copied()
    }

    /// The longest title kept, for a field's `maxlength`.
    fn title_max(&self) -> String {
        crate::db::TITLE_MAX.to_string()
    }

    /// The longest description or caption kept.
    fn text_max(&self) -> String {
        crate::db::TEXT_MAX.to_string()
    }
}

impl<T: Template> Helpers for T {}

/// `n` bytes for a person to read.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    match u {
        0 => format!("{bytes} B"),
        _ if v >= 100.0 => format!("{v:.0} {}", UNITS[u]),
        _ => format!("{v:.1} {}", UNITS[u]),
    }
}

/// The paths the header and the layout name.
impl Header<'_> {
    fn albums(&self) -> &'static str {
        paths::ALBUMS
    }
    fn account(&self) -> &'static str {
        paths::ACCOUNT
    }
    fn accounts(&self) -> &'static str {
        paths::ACCOUNTS
    }
    fn logout(&self) -> &'static str {
        paths::LOGOUT
    }
}

/// The kit's flash region, which says over the page when a request fails and
/// sends a request refused for want of a session to sign in. Placed in the
/// layout, and sent empty as a page loads in place.
#[derive(Template)]
#[template(path = "layout/flash.html")]
pub(super) struct FlashRegion;

pub(super) fn flash_cleared() -> Html {
    FlashRegion.draw()
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn sizes_read_as_people_say_them() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1500), "1.5 KB");
        assert_eq!(human(3_400_000), "3.4 MB");
        assert_eq!(human(250_000_000), "250 MB");
    }
}
