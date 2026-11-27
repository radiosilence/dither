//! The signed-in pages, one module per page, named as the header and the
//! title name them. Every page has the same shape ([`Page`]): its name, the
//! stream that keeps it current, what it reads from the request, and its
//! body. [`respond`] draws any of them: a browser asking for the address gets
//! the layout around the body; the page itself asking (a link that loads in
//! place) gets the body alone, morphed into `main`, with the address it now
//! shows.

use std::future::Future;

use askama::Template;
use askama_datastar::{Html, PatchSignals};
use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::layout::{Draw, Header, Layout, Main, Name, Shell, Title, flash_cleared};
use crate::auth::session::Session;
use crate::error::AppResult;

pub(super) mod account;
pub(super) mod accounts;
pub(super) mod album;
pub(super) mod albums;

/// A signed-in page: what it is called, the stream that keeps it current,
/// what it reads from the request, and how its body is drawn.
pub(super) trait Page: Send + 'static {
    const NAME: Name;
    /// The address of the live stream the page holds open.
    const STREAM: &'static str;
    type Args: Send;
    fn body(
        s: &AppState,
        session: &Session,
        args: Self::Args,
    ) -> impl Future<Output = AppResult<Body>> + Send;
}

/// A page's body as its module draws it, with what the layout needs to know
/// of it.
pub(super) struct Body {
    html: Html,
    /// What a person called what the page shows, such as an album's title.
    heading: String,
}

impl Body {
    pub(super) fn of(view: &impl Template) -> Self {
        Self {
            html: view.draw(),
            heading: String::new(),
        }
    }

    pub(super) fn heading(mut self, heading: String) -> Self {
        self.heading = heading;
        self
    }
}

/// The signals every page starts with: the address it shows, and how far the
/// owner's changes had gone as its body was drawn, which its stream carries
/// on from.
fn signals(url: &str, seen: &str) -> serde_json::Value {
    serde_json::json!({ "url": url, "seen": seen })
}

/// Page `P` for `session`: whole for a browser, or in place for the page
/// itself, at the address `url`.
pub(super) async fn respond<P: Page>(
    s: &AppState,
    session: &Session,
    headers: &HeaderMap,
    url: &str,
    args: P::Args,
) -> AppResult<Response> {
    // Read before the body is drawn: a change landing meanwhile is drawn
    // again by the stream rather than missed.
    let seen = s.db.seen(&session.username);
    let body = P::body(s, session, args).await?;
    let title = Title::new(P::NAME, &body.heading).draw();
    let header = Header::new(session, P::NAME).draw();
    let main = Main {
        stream: P::STREAM,
        body: body.html,
    }
    .draw();
    if askama_datastar::is_datastar(headers) {
        let events = [
            Html::from_iter([main, title, header, flash_cleared()])
                .patch()
                .into(),
            PatchSignals::new(signals(url, &seen).to_string()).into(),
        ];
        return Ok(askama_datastar::events(events));
    }
    let layout = Layout {
        shell: Shell::of(s),
        title,
        header,
        main,
        signals: signals(url, &seen).to_string(),
    };
    Ok(layout.draw().into_response())
}

/// Draw page `P` behind the session layer.
pub(super) async fn serve<P: Page>(
    State(s): State<AppState>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
) -> Response
where
    P::Args: Default,
{
    let url = askama_datastar::address(&uri);
    match respond::<P>(&s, &session, &headers, &url, P::Args::default()).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}
