//! A video as the player draws it.

/// What the player macro (`templates/player.html`) needs of a video.
pub struct Clip {
    pub id: String,
    /// What plays: the playback copy, or the original where it plays as is.
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
    /// For the system's media controls.
    pub title: String,
}

impl Clip {
    /// The length as a clock reads it.
    pub fn duration(&self) -> String {
        crate::video::clock(self.seconds)
    }
}
