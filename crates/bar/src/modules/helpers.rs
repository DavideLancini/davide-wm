//! Text helpers shared by widgets.

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
