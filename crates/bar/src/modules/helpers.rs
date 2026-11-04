//! Widgets backed by the helper binaries in `~/0Projects/dawn`.
//!
//! These tools are already Rust; the bar invokes them rather than duplicating
//! their logic, so `dawn` stays the single source of truth (QUESTIONS.md #3).
//!
//! All of them were `interval: "once"` plus a signal in Waybar, i.e. read at
//! startup and refreshed on demand — never polled. `Helper` keeps that shape by
//! caching the value until `refresh()` is called.

use crate::exec;
use crate::json;

/// Drops characters the bar font cannot draw — emoji and Nerd Font glyphs,
/// which the helpers emit for Waybar's icon-capable font. Noto Sans Mono has no
/// glyph for them, so they would render as replacement boxes.
fn strip_unrenderable(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| {
            let u = *c as u32;
            let emoji = (0x1F000..=0x1FAFF).contains(&u) || (0x2600..=0x27BF).contains(&u);
            // Private Use Area: where Nerd Fonts put their icons.
            let private_use = (0xE000..=0xF8FF).contains(&u) || (0xF0000..=0xFFFFD).contains(&u);
            !emoji && !private_use && *c != '\u{fffd}'
        })
        .collect();
    // Collapse the whitespace an removed icon leaves behind.
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A helper command whose value is cached until explicitly refreshed.
pub struct Helper {
    command: &'static str,
    text: String,
    loaded: bool,
}

impl Helper {
    pub const fn new(command: &'static str) -> Self {
        Helper { command, text: String::new(), loaded: false }
    }

    /// Cached text, running the helper on first use.
    pub fn text(&mut self) -> String {
        if !self.loaded {
            self.refresh();
        }
        self.text.clone()
    }

    /// Marks the cached value stale, so the next `text()` re-runs the helper.
    /// Used after a click: the command just changed the state the helper
    /// reports, and the cache would otherwise keep showing the old value.
    pub fn invalidate(&mut self) {
        self.loaded = false;
    }

    /// Re-runs the helper. Returns true when the text changed.
    pub fn refresh(&mut self) -> bool {
        let out = exec::run(self.command);
        // Helpers emit either JSON with a "text" field or bare text.
        let raw = json::string_field(&out, "text").unwrap_or(out);
        let new = strip_unrenderable(&raw);
        self.loaded = true;
        let changed = new != self.text;
        self.text = new;
        changed
    }
}

/// Shortens `text` to at most `max` characters, marking the cut with an
/// ellipsis when something was dropped.
///
/// The ellipsis matters: a title cut mid-word reads as a wrong title, while
/// "Rings of Saturn - Unh…" reads as a shortened one. It is counted inside the
/// budget, so the result never exceeds `max` — a widget that promises twenty
/// characters must not draw twenty-one.
///
/// Characters, not bytes: accented titles are common and slicing bytes would
/// split them.
pub fn shorten(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    // Trailing space before the ellipsis reads as a typo.
    while out.ends_with(' ') {
        out.pop();
    }
    out.push('…');
    out
}

/// Bluetooth headphone battery, e.g. "L 80% R 75%".
pub fn bt_headphones() -> Helper {
    Helper::new("/home/davidelancini/7Scripts/bt-headphones-battery 2>/dev/null")
}

#[cfg(test)]
mod shorten_tests {
    use super::shorten;

    /// Text within budget is untouched; longer text loses its tail to an
    /// ellipsis that itself fits inside the budget.
    #[test]
    fn the_limit_includes_the_ellipsis() {
        assert_eq!(shorten("short", 20), "short");
        assert_eq!(shorten("12345678901234567890", 20), "12345678901234567890");
        // Twenty-one characters: one goes, the ellipsis takes its place.
        let cut = shorten("123456789012345678901", 20);
        assert_eq!(cut.chars().count(), 20, "{cut:?} must not exceed the budget");
        assert!(cut.ends_with('…'));
    }

    /// A real track name, which is what prompted the limit.
    #[test]
    fn a_long_title_says_it_was_cut() {
        let line = "Rings of Saturn - Unhallowed (2017), Ultu Ulla";
        let cut = shorten(line, 20);
        assert_eq!(cut, "Rings of Saturn - U…");
        assert_eq!(cut.chars().count(), 20);
    }

    /// The cut must not leave a space hanging before the ellipsis, and must
    /// count characters rather than bytes so accents survive.
    #[test]
    fn cuts_land_cleanly() {
        assert_eq!(shorten("long text here", 6), "long…", "no space before …");
        // Five accented characters, ten bytes: slicing bytes would panic or
        // produce mojibake.
        assert_eq!(shorten("èèèèè", 5), "èèèèè");
        assert_eq!(shorten("èèèèèè", 5), "èèèè…");
        assert_eq!(shorten("anything", 0), "", "a zero budget draws nothing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Icons the bar font cannot draw are dropped, without leaving a gap.
    #[test]
    fn strips_icons_the_font_cannot_draw() {
        // The headphone emoji bt-headphones-battery emits.
        assert_eq!(strip_unrenderable("\u{1f3a7} 64%"), "64%");
        // Nerd Font glyphs live in the Private Use Area.
        assert_eq!(strip_unrenderable("\u{f240}   74%"), "74%");
        assert_eq!(strip_unrenderable("\u{fffd} ok"), "ok");
        // Plain text is untouched apart from whitespace normalisation.
        assert_eq!(strip_unrenderable("LID:OFF"), "LID:OFF");
        assert_eq!(strip_unrenderable("  a   b  "), "a b");
        // Accented Latin and the degree sign must survive.
        assert_eq!(strip_unrenderable("45\u{b0}C \u{e0}"), "45\u{b0}C \u{e0}");
    }

    /// Caching: the first call runs the helper, later calls do not.
    #[test]
    fn caches_until_refreshed() {
        let mut h = Helper::new("echo hello");
        assert_eq!(h.text(), "hello");
        assert!(h.loaded);
        // A second call returns the cache without re-running.
        assert_eq!(h.text(), "hello");
        // refresh() reports "unchanged" when the output is stable.
        assert!(!h.refresh());
    }

    /// After a click the helper's state changed, so the cache must be dropped:
    /// without this the widget kept showing the old value forever.
    #[test]
    fn invalidate_forces_a_re_read() {
        let mut h = Helper::new("echo first");
        assert_eq!(h.text(), "first");

        // Simulate the state changing behind the helper's back.
        h.command = "echo second";
        assert_eq!(h.text(), "first", "cache still serves the old value");

        h.invalidate();
        assert_eq!(h.text(), "second", "invalidate must force a re-read");
    }

    #[test]
    fn extracts_the_text_field_from_json() {
        let mut h = Helper::new(r#"echo '{"text":"LID:ON","class":"on"}'"#);
        assert_eq!(h.text(), "LID:ON");
    }

    #[test]
    fn a_failing_helper_yields_empty() {
        let mut h = Helper::new("exit 1");
        assert_eq!(h.text(), "");
        let mut missing = Helper::new("/nonexistent/binary 2>/dev/null");
        assert_eq!(missing.text(), "");
    }

    #[test]
    fn detects_changes() {
        let mut h = Helper::new("echo first");
        assert_eq!(h.text(), "first");
        h.command = "echo second";
        assert!(h.refresh(), "changed output must report true");
        assert_eq!(h.text(), "second");
    }
}
