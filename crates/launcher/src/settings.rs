//! Launcher look and behaviour (compile-time, like the rest of davide-wm).

pub const WINDOW_WIDTH: u32 = 600;
pub const ROW_HEIGHT: u32 = 32;
pub const VISIBLE_RESULTS: u32 = 8;
pub const WINDOW_HEIGHT: u32 = ROW_HEIGHT * (VISIBLE_RESULTS + 1);

pub const BG_COLOR: u32 = 0x00_00_00;
pub const FG_COLOR: u32 = 0xFF_FF_FF;
pub const SELECTED_BG_COLOR: u32 = 0x33_33_33;
/// Search row separator line.
pub const BORDER_COLOR: u32 = 0xFF_FF_FF;
/// Same size as the bar.
pub const FONT_SIZE: f32 = 24.0;

/// Runs entries with Terminal=true.
pub const TERMINAL_CMD: &str = "foot";

/// Opens bookmark (Type=Link) entries; `--new-window` forces a window
/// instead of a tab in one that is already open.
pub const BROWSER_CMD: &str = "vivaldi-stable --new-window";
