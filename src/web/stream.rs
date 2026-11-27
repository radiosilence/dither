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
//! owner's changes had gone as its body was drawn (`seen`, `Db::seen`: the
//! server's boot and a count), and the stream's first pass draws only if they
//! have moved since. After a pass that drew the owner's regions as they stand,
//! the stream sets `seen` to where it has drawn up to, so the stream that
//! replaces it carries on from there. A pass that drew nothing of the owner's
//! leaves `seen` where it was; one that failed to draw it ends the stream with
//! `seen` where it was, so the stream the page opens next draws it.

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
    ) -> impl Future<Output = Drawn> + Send;
}

/// What a pass drew.
pub(super) struct Drawn {
    pub(super) events: Vec<Event>,
    /// The owner's regions could not be drawn (the database did not answer),
    /// so the page is not up to date with the changes the pass was for.
    pub(super) failed: bool,
}

impl Drawn {
    pub(super) fn sent(events: Vec<Event>) -> Self {
        Self {
            events,
            failed: false,
        }
    }

    pub(super) fn failed(events: Vec<Event>) -> Self {
        Self {
            events,
            failed: true,
        }
    }
}

/// Whether a stream's first pass draws: unless the page says it drew the
/// owner's changes as they stand now. A count from another boot, or none,
/// never matches.
fn stale(shown: Option<&str>, now: &str) -> bool {
    shown != Some(now)
}

/// The `seen` a stream has handed its page, and what it hands after each pass.
struct Handover {
    /// What the page holds: what it was drawn with, then what was last sent.
    holds: Option<String>,
}

/// What a stream does after a pass.
#[derive(Debug, PartialEq, Eq)]
enum After {
    /// Hands the page this `seen`: it now has the owner's changes up to it.
    Send(String),
    /// Carries on: the pass drew none of the owner's changes (a video's
    /// progress alone), or the page holds its `seen` already.
    Carry,
    /// Ends, the pass having failed to draw the owner's regions. Its `seen`
    /// is withheld, and the page opens another stream, which draws from the
    /// `seen` the page still holds rather than waiting for the next change.
    End,
}

impl Handover {
    fn after(&mut self, changed: &Changed, failed: bool) -> After {
        if failed {
            return After::End;
        }
        if !changed.mine || self.holds.as_deref() == Some(changed.seen.as_str()) {
            return After::Carry;
        }
        self.holds = Some(changed.seen.clone());
        After::Send(changed.seen.clone())
    }
}

/// A page with nothing live of its own: held open, so the page's stream
/// reopens like any other, and ending as the others do.
pub(super) struct Quiet;

impl Live for Quiet {
    async fn open(_: &AppState, _: &Session, _: &Shown) -> Self {
        Self
    }
    async fn draw(&mut self, _: &AppState, _: &Session, _: &Changed) -> Drawn {
        Drawn::sent(Vec::new())
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
    pub(super) seen: Option<String>,
}

/// What changed since the last redraw.
pub(super) struct Changed {
    /// The owner's albums or images.
    pub(super) mine: bool,
    /// Videos' progress, in thousandths, by image.
    pub(super) progress: HashMap<String, u16>,
    /// How far the owner's changes have gone, as this redraw draws them:
    /// read once whatever came in is gathered, and before anything is drawn.
    seen: String,
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
            seen: String::new(),
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
        mine: stale(shown.seen.as_deref(), &seen),
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
        (changes, page, Some(Handover { holds: shown.seen })),
        move |(mut changes, mut page, handover)| {
            let (s, session) = (s.clone(), session.clone());
            async move {
                let mut handover = handover?;
                let changed = changes.next().await?;
                let Drawn { mut events, failed } = page.draw(&s, &session, &changed).await;
                // The page now has everything up to here, so a stream opened
                // after this one carries on from it.
                let next = match handover.after(&changed, failed) {
                    After::Send(seen) => {
                        let signal = serde_json::json!({ "seen": seen });
                        events.push(PatchSignals::new(signal.to_string()).into());
                        Some(handover)
                    }
                    After::Carry => Some(handover),
                    After::End => None,
                };
                Some((events, (changes, page, next)))
            }
        },
    )
    .flat_map(futures_util::stream::iter);
    askama_datastar::stream(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(mine: bool, seen: &str) -> Changed {
        Changed {
            mine,
            progress: HashMap::new(),
            seen: seen.into(),
        }
    }

    #[test]
    fn a_page_drawn_as_things_stand_is_not_drawn_again() {
        assert!(!stale(Some("00ab.5"), "00ab.5"));
    }

    #[test]
    fn a_page_drawn_before_a_change_or_a_restart_is_drawn_again() {
        // A change since.
        assert!(stale(Some("00ab.4"), "00ab.5"));
        // The same count from another boot: the counts restart with it.
        assert!(stale(Some("00cd.5"), "00ab.5"));
        // A page that does not say.
        assert!(stale(None, "00ab.5"));
    }

    #[test]
    fn seen_moves_only_after_a_pass_that_drew_the_owners_changes() {
        let mut handover = Handover {
            holds: Some("00ab.5".into()),
        };
        // A pass for progress alone drew none of the owner's changes, though
        // one may have been counted meanwhile.
        assert_eq!(
            handover.after(&changed(false, "00ab.6"), false),
            After::Carry
        );
        // Drawn: the page now holds them.
        assert_eq!(
            handover.after(&changed(true, "00ab.6"), false),
            After::Send("00ab.6".into())
        );
        // Already held.
        assert_eq!(
            handover.after(&changed(true, "00ab.6"), false),
            After::Carry
        );
    }

    #[test]
    fn a_failed_pass_ends_the_stream_before_a_later_one_hands_over() {
        let mut handover = Handover {
            holds: Some("00ab.5".into()),
        };
        // Drawing failed: the stream ends there, its `seen` withheld, so no
        // later pass can hand over a `seen` the page was never drawn to, and
        // the next stream draws from the page's own.
        assert_eq!(handover.after(&changed(true, "00ab.6"), true), After::End);
    }
}
