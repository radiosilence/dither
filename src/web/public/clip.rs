//! A video as the player draws it (`templates/player.html`), on the public
//! pages and in the app's viewer.

use crate::db::Image;
use crate::media::fit;
use crate::web::paths;

/// What the player macro (`templates/player.html`) needs of a video.
pub struct Clip {
    /// The video's id: the player's own, so a redraw of the app's viewer
    /// replaces a player for another video rather than reshaping it.
    pub id: String,
    /// What plays: the video's display copy where it has one, otherwise the
    /// video as kept.
    pub src: String,
    pub poster: String,
    /// The original, to download.
    pub file: String,
    pub width: u32,
    pub height: u32,
    pub seconds: f64,
    pub audio: bool,
    /// Short and silent: plays on a loop, as a GIF would.
    pub looping: bool,
    /// For the system's media controls: the caption's first line, or empty
    /// for the template to word.
    pub title: String,
}

impl Clip {
    /// The length as a clock reads it.
    pub fn duration(&self) -> String {
        crate::video::clock(self.seconds)
    }
}

/// Short silent videos play on a loop, as a GIF would.
const LOOP_MAX_SECONDS: f64 = 30.0;

/// A video as the player draws it.
pub fn clip(i: &Image) -> Clip {
    let file = paths::original(&i.id, i.kind.ext());
    let (width, height) = if i.display {
        fit(i.width, i.height, crate::video::PLAY_MAX)
    } else {
        (i.width, i.height)
    };
    let seconds = i.seconds();
    Clip {
        id: i.id.clone(),
        src: if i.display {
            paths::display_video(&i.id)
        } else {
            file.clone()
        },
        poster: paths::preview(&i.id),
        file,
        width,
        height,
        seconds,
        audio: i.audio,
        looping: !i.audio && seconds <= LOOP_MAX_SECONDS,
        // Never the name it was uploaded under, which is its owner's: a
        // phone's says little, a laptop's can say a lot.
        title: crate::markdown::line(&i.caption),
    }
}
