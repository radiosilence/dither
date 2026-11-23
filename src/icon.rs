//! The kōan icon set, as far as dither uses it.
//!
//! The paths are koan's (`crates/koan-server/src/icon/glyphs.rs`, compiled
//! from the SVGs its apps draw), so a glyph here is the same glyph there: 24
//! units square, one stroke in the colour of the text beside it, square caps,
//! mitred joins, and a width set by size rather than scaled with the drawing.

pub struct Glyph {
    strokes: &'static str,
    fills: &'static str,
}

const fn g(strokes: &'static str) -> Glyph {
    Glyph { strokes, fills: "" }
}

pub const PLUS: Glyph = g("M12 4.5L12 19.5M4.5 12L19.5 12");
pub const TRASH: Glyph = g(
    "M4 6.5L20 6.5M9.5 6.5L9.5 4L14.5 4L14.5 6.5M6 6.5L7 20L17 20L18 6.5M10.5 10L10.5 16.5M13.5 10L13.5 16.5",
);
pub const LINK: Glyph = g(
    "M13.13 15.12L8.88 10.87L2.87 16.88L7.12 21.13ZM21.13 7.12L16.88 2.87L10.87 8.88L15.12 13.13ZM9.5 14.5L14.5 9.5",
);
pub const COPY: Glyph = g("M4.5 16.5L4.5 3.5L15.5 3.5M8.5 7.5L19.5 7.5L19.5 20.5L8.5 20.5Z");
pub const DOWNLOAD: Glyph = g("M12 4L12 15M7.5 10.5L12 15L16.5 10.5M5 19.5L19 19.5");
pub const CLOSE: Glyph = g("M6 6L18 18M18 6L6 18");
pub const CHECK: Glyph = g("M5 12.5L10 17.5L19 7");
pub const SELECT: Glyph = g("M4 4L20 4L20 20L4 20Z");
pub const SELECTING: Glyph = Glyph {
    strokes: "M7.5 12L10.5 15L16.5 9",
    fills: "M4 4L20 4L20 20L4 20Z",
};
pub const CHEVRON_LEFT: Glyph = g("M15 4.5L7.5 12L15 19.5");
pub const CHEVRON_RIGHT: Glyph = g("M9 4.5L16.5 12L9 19.5");
pub const EXPORT: Glyph = g("M4 13L4 20L20 20L20 13M12 15.5L12 3.5M8 7.5L12 3.5L16 7.5");
pub const OPEN_EXTERNAL: Glyph =
    g("M11 4.5L4.5 4.5L4.5 19.5L19.5 19.5L19.5 13M14 4.5L19.5 4.5L19.5 10M19.5 4.5L11 13");
pub const ALBUM: Glyph = g("M4 7.5L16.5 7.5L16.5 20L4 20ZM7.5 4L20 4L20 16.5");
pub const WARNING: Glyph = g("M12 3.5L21 19.5L3 19.5ZM12 9L12 14M11.75 16.75L12.25 16.75");
pub const PLAY: Glyph = g("M7 5L19 12L7 19Z");
pub const PAUSE: Glyph = g("M8.5 5L8.5 19M15.5 5L15.5 19");
pub const VOLUME: Glyph = g(
    "M3.5 9L7 9L12 4.5L12 19.5L7 15L3.5 15ZM13.89 8.11A5.5 5.5 0 0 1 13.89 15.89M16.36 5.64A9 9 0 0 1 16.36 18.36",
);
/// The speaker of `VOLUME`, crossed out where its waves were.
pub const MUTED: Glyph = g("M3.5 9L7 9L12 4.5L12 19.5L7 15L3.5 15ZM15 9.5L20 14.5M20 9.5L15 14.5");
pub const FULL_SCREEN: Glyph = g("M4 9L4 4L9 4M15 4L20 4L20 9M20 15L20 20L15 20M9 20L4 20L4 15");
pub const LEAVE_FULL_SCREEN: Glyph =
    g("M9 4L9 9L4 9M15 4L15 9L20 9M20 15L15 15L15 20M9 20L9 15L4 15");
/// A screen with a smaller one in its corner.
pub const PICTURE_IN_PICTURE: Glyph =
    g("M3.5 5.5L20.5 5.5L20.5 18.5L3.5 18.5ZM12.5 12L17.5 12L17.5 15.5L12.5 15.5Z");

/// A glyph by name, drawn `px` square: what the templates ask for.
pub fn named(name: &str, px: f32) -> String {
    let glyph = match name {
        "plus" => &PLUS,
        "trash" => &TRASH,
        "link" => &LINK,
        "copy" => &COPY,
        "download" => &DOWNLOAD,
        "close" => &CLOSE,
        "check" => &CHECK,
        "select" => &SELECT,
        "selecting" => &SELECTING,
        "chevron-left" => &CHEVRON_LEFT,
        "chevron-right" => &CHEVRON_RIGHT,
        "export" => &EXPORT,
        "open-external" => &OPEN_EXTERNAL,
        "album" => &ALBUM,
        "play" => &PLAY,
        "pause" => &PAUSE,
        "volume" => &VOLUME,
        "muted" => &MUTED,
        "full-screen" => &FULL_SCREEN,
        "leave-full-screen" => &LEAVE_FULL_SCREEN,
        "picture-in-picture" => &PICTURE_IN_PICTURE,
        _ => &WARNING,
    };
    glyph.svg(px, "glyph")
}

impl Glyph {
    /// The glyph as an inline `<svg>` drawn `px` CSS pixels square. The
    /// stroke is the theme's rule: 0.6 + 0.04 × the point size of the type
    /// beside it, where the glyph's square is 1.2 times that size.
    pub fn svg(&self, px: f32, class: &str) -> String {
        let points = px / 1.2;
        let width = (0.6 + 0.04 * points) * 24.0 / px;
        let mut out = format!(
            "<svg class=\"{class}\" width=\"{px}\" height=\"{px}\" viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" \
             stroke-width=\"{width:.2}\" stroke-linecap=\"square\" stroke-linejoin=\"miter\" aria-hidden=\"true\">"
        );
        if !self.fills.is_empty() {
            out.push_str(&format!(
                "<path d=\"{}\" fill=\"currentColor\" stroke=\"none\"/>",
                self.fills
            ));
        }
        if !self.strokes.is_empty() {
            let stroke = if self.fills.is_empty() {
                "currentColor"
            } else {
                "var(--bg)"
            };
            out.push_str(&format!(
                "<path d=\"{}\" stroke=\"{stroke}\"/>",
                self.strokes
            ));
        }
        out.push_str("</svg>");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glyph_draws_on_the_theme_stroke_rule() {
        let svg = CLOSE.svg(18.0, "x");
        assert!(svg.contains("stroke-linecap=\"square\"") && svg.contains("class=\"x\""));
        // Beside 15-point type: a 1.2-pixel stroke, 1.6 units of the 24.
        assert!(svg.contains("stroke-width=\"1.60\""), "{svg}");
        assert!(SELECTING.svg(15.0, "").contains("fill=\"currentColor\""));
    }
}
