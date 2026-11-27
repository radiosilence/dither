//! An album's page live: its grid, count and foot as uploads land and its
//! images change, and each video's bar as it is made, sent alone rather than
//! the grid redrawn for every step. An album deleted elsewhere is drawn as
//! gone, once.

use std::collections::HashSet;

use askama_datastar::Last;

use super::{Changed, Drawn, Live, Shown};
use crate::auth::session::Session;
use crate::error::AppError;
use crate::web::layout::Draw;
use crate::web::page::album::{Deleted, owned, progress, regions};
use crate::web::{AppState, paths};

pub(in crate::web) struct Album {
    /// The album the page's address names.
    id: String,
    grid: Last,
    summary: Last,
    foot: Last,
    /// The images the page shows: progress is sent for these.
    shown: HashSet<String>,
    gone: bool,
}

impl Live for Album {
    const PROGRESS: bool = true;

    async fn open(s: &AppState, session: &Session, shown: &Shown) -> Self {
        let id = paths::album_of(&shown.url).unwrap_or_default().to_string();
        // What the page drew, for the progress sent before the grid is.
        let images = match owned(s, &session.username, &id).await {
            Ok(album) => s.db.images(&album.id).await.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        Self {
            id,
            grid: Last::new(),
            summary: Last::new(),
            foot: Last::new(),
            shown: images.into_iter().map(|i| i.id).collect(),
            gone: false,
        }
    }

    async fn draw(&mut self, s: &AppState, session: &Session, c: &Changed) -> Drawn {
        let mut events = Vec::new();
        let mut failed = false;
        if c.mine && !self.gone {
            match owned(s, &session.username, &self.id).await {
                Ok(album) => match regions(s, &album).await {
                    Ok((grid, summary, foot, ids)) => {
                        self.shown = ids.into_iter().collect();
                        events.extend(self.grid.send(&grid.draw()));
                        events.extend(self.summary.send(&summary.draw()));
                        events.extend(self.foot.send(&foot.draw()));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "drawing an album failed");
                        failed = true;
                    }
                },
                Err(AppError::NotFound) => {
                    self.gone = true;
                    self.shown.clear();
                    events.push(Deleted.draw().patch().into());
                }
                Err(e) => {
                    tracing::warn!(error = %e, "reading an album failed");
                    failed = true;
                }
            }
        }
        for (id, done) in &c.progress {
            if self.shown.contains(id) {
                events.push(progress(id, *done).html().patch().into());
            }
        }
        Drawn { events, failed }
    }
}
