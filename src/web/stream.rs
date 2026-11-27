//! The live streams: one per page with live regions, each keeping its page
//! current. Every stream has the same shape ([`Live`]): it opens from what the
//! page shows (its address, among its signals) and draws the regions a change
//! touches. [`serve`] runs any of them. Nothing is polled: a stream waits on
//! a change to its owner's albums or images (`Db::changes`), and on a video's
//! progress when it asks for it, paced by `askama_datastar::paced` to at most
//! one redraw every quarter second, and ended after a minute for the page's
//! `reopen_on` attributes to open another. A region is sent only when it
//! differs from what was last sent (`askama_datastar::Last`).
//!
//! A stream also ends when the server begins to stop, or when its account
//! changes (a new role or password, its deletion): the page's next request is
//! then authenticated as things stand.
//!
//! Streams open often, so an opening is cheap: the page notes how far its
//! owner's changes had gone as its body was drawn (`seen`), and the stream's
//! first pass draws only if they have moved since. After each pass the stream
//! sets `seen` to where it has drawn up to, so the stream that replaces it
//! carries on from there.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use askama_datastar::{Pace, PatchSignals, Signals, paced};
use axum::Extension;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::Event;
use futures_util::StreamExt as _;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, watch};

use super::AppState;
use crate::auth::session::Session;

pub(super) mod album;
pub(super) mod albums;

/// A page's live stream.
pub(super) trait Live: Sized + Send + 'static {
    /// Whether it draws videos' progress as they are made.
    const PROGRESS: bool = false;
    /// Ready to draw the page `shown` describes.
    fn open(s: &AppState, session: &Session, shown: &Shown) -> impl Future<Output = Self> + Send;
    /// The regions `changed` touches, drawn and sent if they differ from what
    /// was last sent.
    fn draw(
        &mut self,
        s: &AppState,
        session: &Session,
        changed: &Changed,
    ) -> impl Future<Output = Vec<Event>> + Send;
}

/// A page with nothing live of its own: held open, so the page's stream
/// reopens like any other, and ending as the others do.
pub(super) struct Quiet;

impl Live for Quiet {
    async fn open(_: &AppState, _: &Session, _: &Shown) -> Self {
        Self
    }
    async fn draw(&mut self, _: &AppState, _: &Session, _: &Changed) -> Vec<Event> {
        Vec::new()
    }
}

/// What the page shows, from the signals Datastar sends with the stream's
/// request.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub(super) struct Shown {
    /// The page's address, path and query.
    pub(super) url: String,
    /// How far its owner's changes had gone when it was last drawn. None from
    /// a page that does not say, which is drawn whole.
    pub(super) seen: Option<u64>,
}

/// What changed since the last redraw.
pub(super) struct Changed {
    /// The owner's albums or images.
    pub(super) mine: bool,
    /// Videos' progress, in thousandths, by image.
    pub(super) progress: HashMap<String, u16>,
    /// How far the owner's changes have gone, as this redraw draws them.
    seen: u64,
}

/// What changes the page, gathered between redraws.
struct Sources {
    state: AppState,
    owner: Arc<str>,
    changes: broadcast::Receiver<Arc<str>>,
    ticks: Option<broadcast::Receiver<crate::jobs::Tick>>,
    stopping: watch::Receiver<bool>,
    /// Ends when the account changes.
    account: Pin<Box<dyn Future<Output = ()> + Send>>,
    first: Option<Changed>,
}

impl Sources {
    /// The next change, gathering whatever else came in with it; `None` once
    /// the stream should end.
    async fn next(&mut self) -> Option<Changed> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        let mut changed = Changed {
            mine: false,
            progress: HashMap::new(),
            seen: 0,
        };
        loop {
            tokio::select! {
                change = self.changes.recv() => match change {
                    Ok(owner) if owner == self.owner => changed.mine = true,
                    Ok(_) => continue,
                    // Missed some, which may have been theirs.
                    Err(RecvError::Lagged(_)) => changed.mine = true,
                    Err(RecvError::Closed) => return None,
                },
                Some(tick) = async {
                    match self.ticks.as_mut() {
                        Some(rx) => Some(rx.recv().await),
                        None => std::future::pending().await,
                    }
                } => match tick {
                    Ok(t) if t.owner == self.owner => {
                        changed.progress.insert(t.id, t.done);
                    }
                    _ => continue,
                },
                _ = self.stopping.wait_for(|s| *s) => return None,
                () = &mut self.account => return None,
            }
            break;
        }
        // Whatever else came in meanwhile is drawn in the same pass.
        loop {
            match self.changes.try_recv() {
                Ok(owner) if owner == self.owner => changed.mine = true,
                Ok(_) => {}
                Err(TryRecvError::Lagged(_)) => changed.mine = true,
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        if let Some(rx) = self.ticks.as_mut() {
            while let Ok(t) = rx.try_recv() {
                if t.owner == self.owner {
                    changed.progress.insert(t.id, t.done);
                }
            }
        }
        changed.seen = self.state.db.seen(&self.owner);
        Some(changed)
    }
}

/// Run stream `L` for the page that sent `shown`.
pub(super) async fn serve<L: Live>(
    State(s): State<AppState>,
    Extension(session): Extension<Session>,
    Signals(shown): Signals<Shown>,
) -> impl IntoResponse {
    let owner: Arc<str> = session.username.as_str().into();
    let changes = s.db.changes();
    let ticks = L::PROGRESS.then(|| s.uploads.jobs.ticks());
    // Read after subscribing, so a change landing between is seen twice
    // rather than missed.
    let seen = s.db.seen(&owner);
    let first = Changed {
        mine: shown.seen != Some(seen),
        progress: HashMap::new(),
        seen,
    };
    let page = L::open(&s, &session, &shown).await;
    let sources = Sources {
        owner,
        changes,
        ticks,
        stopping: s.stopping.clone(),
        account: Box::pin(koan_auth::changes::account_changed_since(
            session.user_id,
            session.mark,
        )),
        first: Some(first),
        state: s.clone(),
    };
    let changes = futures_util::stream::unfold(sources, |mut sources| async move {
        let changed = sources.next().await?;
        Some((changed, sources))
    });
    let changes = Box::pin(paced(changes, Pace::default()));
    let events = futures_util::stream::unfold(
        (changes, page, shown.seen),
        move |(mut changes, mut page, reported)| {
            let (s, session) = (s.clone(), session.clone());
            async move {
                let changed = changes.next().await?;
                let mut events = page.draw(&s, &session, &changed).await;
                // The page now has everything up to here, so a stream opened
                // after this one carries on from it.
                if reported != Some(changed.seen) {
                    let seen = serde_json::json!({ "seen": changed.seen });
                    events.push(PatchSignals::new(seen.to_string()).into());
                }
                Some((events, (changes, page, Some(changed.seen))))
            }
        },
    )
    .flat_map(futures_util::stream::iter);
    askama_datastar::stream(events)
}
