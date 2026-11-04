//! Widget icons, from the Nerd Font private use area.
//!
//! The embedded font is a condensed monospace whose cell is narrower than a
//! Nerd Font's, so the icons are **scaled down and grafted in** by
//! `tools/merge-icons.py`: they keep their shapes but occupy exactly one
//! character, like everything else on the bar.
//!
//! Every codepoint here must be listed in `tools/merge-icons.py`, which builds
//! the font embedded in the binary — a glyph that is not grafted renders as a
//! blank box. `font_has_every_icon` guards that, and `icons_are_single_width`
//! guards the alignment.

/// CPU usage.
pub const CPU: char = '\u{f4bc}';
/// Memory usage.
pub const MEMORY: char = '\u{efc5}';
/// Temperature.
pub const TEMP: char = '\u{f2c9}';
/// CPU frequency.
pub const FREQ: char = '\u{f0e4}';
/// Wireless network.
pub const WIFI: char = '\u{f1eb}';
/// Bluetooth adapter.
pub const BLUETOOTH: char = '\u{f293}';
/// Bluetooth headphones battery.
pub const HEADPHONES: char = '\u{f025}';
/// Battery, drawn upright: the Nerd Font one is wide and flat, so at cell
/// width its terminal all but vanished. Drawn by `tools/draw-icons.py`.
pub const BATTERY: char = '\u{f240}';
/// Power draw / charging.
pub const POWER: char = '\u{f0e7}';
/// Screen brightness.
pub const BRIGHTNESS: char = '\u{f185}';
/// Clock.
pub const CLOCK: char = '\u{f017}';
/// Notifications.
pub const BELL: char = '\u{f0f3}';
/// Cloud sync (MEGA). Drawn by `tools/draw-icons.py`, not taken from the Nerd
/// Font: see `icons_drawn_in_house`.
pub const CLOUD: char = '\u{f1fa}';
/// Clipboard.
pub const CLIPBOARD: char = '\u{f0c5}';
/// Blank the screen (blk).
pub const BLANK: char = '\u{f04d}';
/// Screen saver.
pub const MOON: char = '\u{f186}';
/// Lid inhibit.
pub const LID: char = '\u{f0ab}';
/// Volume.
pub const VOLUME: char = '\u{f028}';
/// Microphone.
pub const MIC: char = '\u{f130}';

/// Now playing.
pub const MUSIC: char = '\u{f001}';

/// Screenshot: takes a region selection, like the Print binding.
pub const CAMERA: char = '\u{f030}';
/// Timer.
pub const TIMER: char = '\u{f017}';

/// Every icon the bar can draw: used both to measure the vertical extent of
/// the drawn area and to check the font subset covers them.
pub const ALL: &[char] = &[
    CPU, MEMORY, TEMP, FREQ, WIFI, BLUETOOTH, HEADPHONES, BATTERY, POWER, BRIGHTNESS, CLOCK, BELL,
    CLOUD, CLIPBOARD, BLANK, MOON, LID, VOLUME, MIC, TIMER, CAMERA, MUSIC,
];

/// Per-widget text colours, derived from the background each widget had in the
/// Waybar stylesheet: same hue, lightened into a pastel that reads on the bar's
/// black background. The colour is what tells one widget from the next, so no
/// separator glyph is drawn (ADR-023).
pub mod color {
    pub const TEMP: u32 = 0x9F_75_E6;
    pub const MEMORY: u32 = 0xE6_9E_74;
    /// Neutral CPU colour, used when the power level is unknown; otherwise the
    /// widget takes its colour from `CPU_LEVEL`.
    pub const CPU: u32 = 0xDE_9A_E6;
    /// One colour per power level 1-5, from the backgrounds Waybar gave
    /// `#custom-cpu-freq.levelN`: blue when throttled, through green and
    /// yellow, to red at full power. Lightened like the rest of the palette.
    pub const CPU_LEVEL: [u32; 5] = [
        0x7A_B8_E6, // level 1 — ultra low
        0x8A_E6_A8, // level 2 — low
        0xE6_D9_7A, // level 3 — mid
        0xE6_A3_70, // level 4 — high
        0xE6_7A_7A, // level 5 — max
    ];
    /// Folder matches the cloud: the same green the volume widget uses, since
    /// both mean "nothing to worry about".
    pub const MEGA_SYNCED: u32 = 0x9E_E6_B1;
    /// Microphone open: green, the same "nothing to worry about" as a synced
    /// folder.
    /// Now playing: Spotify's own green, lightened to sit on black.
    pub const SPOTIFY: u32 = 0x7A_E6_9E;
    /// Microphone level, distinct from the output beside it.
    pub const MIC: u32 = 0x9E_C4_E6;
    /// Either device, muted.
    pub const MUTED: u32 = 0xE6_59_59;
    /// Lid switch inhibited: the laptop stays awake when shut.
    pub const LID_ON: u32 = 0x9E_E6_B1;
    /// Lid switch active: shutting the lid suspends.
    pub const LID_OFF: u32 = 0xE6_59_59;
    /// On mains: the battery is filling.
    pub const BATTERY_CHARGING: u32 = 0x9E_E6_B1;
    /// On battery, whatever the level: amber, not red, because draining is
    /// normal rather than a fault.
    pub const BATTERY_DRAINING: u32 = 0xE6_C4_7A;
    /// Still uploading, paused or failed: amber, not red — nothing is broken,
    /// it just is not finished.
    pub const MEGA_BUSY: u32 = 0xE6_C4_7A;
    /// Not logged in or no sync: dimmed, so an absent answer does not read as
    /// a good one.
    pub const MEGA_UNKNOWN: u32 = 0x6E_6E_7A;
    pub const VOLUME: u32 = 0x9E_E6_B1;
    pub const NETWORK: u32 = 0xE6_B3_92;
    pub const BLUETOOTH: u32 = 0xE6_59_59;
    pub const BT_HEADPHONES: u32 = 0x9A_DE_E6;
    pub const BACKLIGHT: u32 = 0x9F_75_E6;
    /// Notifications, in their own hue: the neighbouring battery turns the
    /// same green while charging, and with no separator between widgets two
    /// identical colours read as one.
    pub const NOTIFICATIONS: u32 = 0xE6_9E_D2;
    pub const BLK: u32 = 0xE6_E6_E6;
    pub const SCREENSHOT: u32 = 0xE6_D9_8A;
    pub const SCREENSAVER: u32 = 0xC8_C8_D8;
    /// Amber: a timer is a thing you are waiting on, not a healthy state.
    pub const TIMER: u32 = 0xE6_C4_7A;
    pub const LID: u32 = 0x9A_DE_E6;
    /// Pale blue, so the time reads as a fixed point rather than a reading.
    pub const CLOCK: u32 = 0x8A_C4_E6;
    pub const WORKSPACES: u32 = 0xA0_B7_E6;
    /// The focused workspace. Bright enough to stand out from the others at a
    /// glance: it replaces the brackets that used to mark it.
    pub const WORKSPACE_ACTIVE: u32 = 0xFF_E0_7A;
    /// A workspace where something rang for attention. Red reads as "look
    /// here" without being confusable with the focused yellow.
    pub const WORKSPACE_URGENT: u32 = 0xFF_6B_6B;
}

/// Prefixes a value with its icon, or returns empty when the value is empty so
/// a missing module leaves no orphan icon behind.
pub fn with(icon: char, value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    format!("{} {}", icon, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_non_empty_values() {
        assert_eq!(with(CPU, "12%"), format!("{} 12%", CPU));
        assert_eq!(with(CLOCK, ""), "", "empty value must not leave an icon");
    }

    /// Every icon must occupy exactly one cell. A double-width glyph — several
    /// Nerd Font and emoji symbols are — would shift everything after it and
    /// break the alignment a monospace bar depends on.
    #[test]
    fn icons_are_single_width() {
        let font = crate::draw::load_font().expect("embedded font");
        let cell = font.metrics('M', crate::settings::FONT_SIZE).advance_width;
        for &icon in ALL {
            let w = font.metrics(icon, crate::settings::FONT_SIZE).advance_width;
            assert!(
                (w - cell).abs() < 0.5,
                "U+{:04X} is {w} px wide, the cell is {cell}: pick a single-width symbol",
                icon as u32
            );
        }
    }

    /// Icons must sit on the same vertical band as the text. Nerd Font glyphs
    /// are drawn low — several dip below the baseline — so grafted as-is they
    /// hang under the letters beside them; `tools/merge-icons.py` lifts them.
    #[test]
    fn icons_are_vertically_centred() {
        let font = crate::draw::load_font().expect("embedded font");
        let fs = crate::settings::FONT_SIZE;

        // Band the text occupies, measured from the baseline.
        let band = |c: char| {
            let m = font.metrics(c, fs);
            (m.ymin, m.ymin + m.height as i32)
        };
        let (mut text_lo, mut text_hi) = (i32::MAX, i32::MIN);
        for c in "ABCXYZ0123456789%".chars() {
            let (lo, hi) = band(c);
            text_lo = text_lo.min(lo);
            text_hi = text_hi.max(hi);
        }
        let text_mid = (text_lo + text_hi) as f32 / 2.0;

        for &icon in ALL {
            let (lo, hi) = band(icon);
            let mid = (lo + hi) as f32 / 2.0;
            assert!(
                (mid - text_mid).abs() <= 2.0,
                "U+{:04X} centred at {mid}, text at {text_mid}: rerun tools/build-font.sh",
                icon as u32
            );
        }
    }

    /// Every icon is drawn to the size of a character: as wide as the cell
    /// allows and filling most of the text's height. Grafted Nerd Font glyphs
    /// failed both ways — some overflowed the cell by nearly double, others
    /// shrank to half a character — which is why the set is drawn in house.
    #[test]
    fn icons_match_the_size_of_a_character() {
        let font = crate::draw::load_font().expect("embedded font");
        let fs = crate::settings::FONT_SIZE;
        let cell = font.metrics('M', fs).advance_width;
        let reference = font.metrics('X', fs);

        for &icon in ALL {
            let m = font.metrics(icon, fs);
            assert!(m.width > 0 && m.height > 0, "U+{:04X} renders nothing", icon as u32);
            // Never wider than the cell: a wide glyph would sit on its
            // neighbour, since every advance is exactly one cell.
            assert!(
                m.width as f32 <= cell + 1.0,
                "U+{:04X} is {} px wide, the cell is {cell}",
                icon as u32,
                m.width
            );
            // And filling it: at least three quarters of the cell across, and
            // a third of a capital's height. Width is the binding constraint —
            // round shapes are capped by it, and a cloud is squat by nature —
            // so the height bar is the looser of the two.
            assert!(
                m.width as f32 >= 0.75 * cell,
                "U+{:04X} is only {} px wide against a {cell} px cell",
                icon as u32,
                m.width
            );
            assert!(
                m.height as f32 >= 0.34 * reference.height as f32,
                "U+{:04X} is {} px tall, 'X' is {}",
                icon as u32,
                m.height,
                reference.height
            );
        }
    }

    /// Every icon must exist in the embedded subset, otherwise it renders as a
    /// blank box. This is the check that catches a codepoint added to icons.rs
    /// without rebuilding the font.
    #[test]
    fn font_has_every_icon() {
        let font = crate::draw::load_font().expect("embedded font");
        for &icon in ALL {
            assert!(
                font.has_glyph(icon),
                "U+{:04X} missing from the font subset: rerun tools/build-font.sh",
                icon as u32
            );
        }
    }

    /// The text the bar renders must be covered too.
    #[test]
    fn font_covers_the_text_we_render() {
        let font = crate::draw::load_font().expect("embedded font");
        for c in "abcXYZ0123456789%°|[]:/. àèìòùé".chars() {
            assert!(font.has_glyph(c), "{c:?} missing from the font subset");
        }
    }
}

#[cfg(test)]
mod camera_probe {
    /// The camera glyph must actually put ink on the bar, not merely exist in
    /// the font: a glyph can be present, well formed, and still rasterise to
    /// nothing at the size the bar draws it.
    #[test]
    fn camera_rasterises_to_something() {
        for ch in [super::CAMERA, super::BLANK, super::MIC] {
            let font = crate::draw::load_font().expect("embedded font");
            let (metrics, bitmap) = font.rasterize(ch, crate::settings::FONT_SIZE);
            let ink = bitmap.iter().filter(|&&p| p > 0).count();
            println!(
                "{ch:?} U+{:04X}: {}x{} ink={ink}",
                ch as u32, metrics.width, metrics.height
            );
            assert!(ink > 0, "{ch:?} draws nothing");
        }
    }
}
