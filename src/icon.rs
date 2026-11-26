//! The glyphs dither draws that koan-kit-web's set lacks, all the video
//! player's. Every other glyph is the kit's (`kit::icon`). These are drawn on
//! the same grid and stroke rule, and as the kit's own type, so a template
//! draws either the same way: `{{ crate::icon::MUTED.svg(18) }}`.

use koan_kit_web::icon::Glyph;

/// The speaker of the kit's `VOLUME`, crossed out where its waves were.
pub const MUTED: Glyph = Glyph {
    name: "muted",
    strokes: "M3.5 9L7 9L12 4.5L12 19.5L7 15L3.5 15ZM15 9.5L20 14.5M20 9.5L15 14.5",
    fills: "",
};

pub const FULL_SCREEN: Glyph = Glyph {
    name: "full-screen",
    strokes: "M4 9L4 4L9 4M15 4L20 4L20 9M20 15L20 20L15 20M9 20L4 20L4 15",
    fills: "",
};

pub const LEAVE_FULL_SCREEN: Glyph = Glyph {
    name: "leave-full-screen",
    strokes: "M9 4L9 9L4 9M15 4L15 9L20 9M20 15L15 15L15 20M9 20L9 15L4 15",
    fills: "",
};

/// A screen with a smaller one in its corner.
pub const PICTURE_IN_PICTURE: Glyph = Glyph {
    name: "picture-in-picture",
    strokes: "M3.5 5.5L20.5 5.5L20.5 18.5L3.5 18.5ZM12.5 12L17.5 12L17.5 15.5L12.5 15.5Z",
    fills: "",
};
