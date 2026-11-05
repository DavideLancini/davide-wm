//! Compile-time configuration: edit and rebuild.
//!
//! There is no config file and no parser — the trade behind ADR-001. Nothing
//! can be wrong at startup that the compiler did not already reject.

/// Vertical padding above and below the glyphs, in pixels.
pub const VPAD: i32 = 1;

/// Bar height: the vertical extent the glyphs actually occupy at FONT_SIZE,
/// plus VPAD above and below. Computed at startup from the font metrics rather
/// than hardcoded, so changing FONT_SIZE resizes the bar correctly
/// (FiraCode @24px: 27 px of glyphs -> 29 px tall).
pub fn bar_height(font: &fontdue::Font) -> u32 {
    (crate::draw::glyph_extent(font).1 + 2 * VPAD) as u32
}

/// Opaque black (RGB packed).
pub const BG_COLOR: u32 = 0x00_00_00;
/// Default text colour, for anything without one of its own.
pub const FG_COLOR: u32 = 0xCD_D6_F4;
/// Gap between adjacent widgets, in pixels. Small on purpose: the colour change
/// does the separating, this only keeps glyphs from touching.
pub const WIDGET_GAP: i32 = 6;
/// Bar font size in pixels. Matches foot, which renders its configured
/// `size=18` (points) at 96 DPI as 24 px.
pub const FONT_SIZE: f32 = 24.0;
/// System monospace (fc-match monospace): Noto Sans Mono.
pub const FONT_PATH: &str = "/usr/share/fonts/noto/NotoSansMono-Regular.ttf";
/// Horizontal padding of the left and right zones, in pixels.
pub const PAD: i32 = 2;

// --- Backlight (compiled-in path)
pub const BACKLIGHT_PATH: &str = "/sys/class/backlight/amdgpu_bl1";

// --- Custom timer (script path)

/// Tick for the sampled widgets. Sources fed by push events (workspaces, audio,
/// D-Bus) refresh independently of this.
pub const TICK_INTERVAL_SECS: u64 = 1;

// --- Workspaces
/// Workspaces with an id above this are labelled with a letter instead of the
/// number: 31 -> "A", 32 -> "B", and so on. Mirrors the previous Waybar setup.
pub const WORKSPACE_LETTER_FROM: i32 = 31;
