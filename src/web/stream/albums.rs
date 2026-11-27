//! The Albums page live: its list and usage line as uploads land and albums
//! change.

use askama_datastar::Last;
use axum::response::sse::Event;

use super::{Changed, Live, Shown};
use crate::auth::session::Session;
use crate::web::AppState;
use crate::web::layout::Draw;
use crate::web::page::albums::regions;

pub(in crate::web) struct Albums {
    list: Last,
    usage: Last,
}

impl Live for Albums {
    async fn open(_: &AppState, _: &Session, _: &Shown) -> Self {
        Self {
            list: Last::new(),
            usage: Last::new(),
        }
    }

    async fn draw(&mut self, s: &AppState, session: &Session, c: &Changed) -> Vec<Event> {
        if !c.mine {
            return Vec::new();
        }
        match regions(s, &session.username).await {
            Ok((list, usage)) => [self.list.send(&list.draw()), self.usage.send(&usage.draw())]
                .into_iter()
                .flatten()
                .collect(),
            // A database that could not answer (a long write holding the
            // lock) is asked again on the next change.
            Err(e) => {
                tracing::warn!(error = %e, "drawing the albums failed");
                Vec::new()
            }
        }
    }
}
