//! A video as the player draws it.

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
