//! Extracting one string field out of the small, flat JSON objects that the
//! Waybar helper scripts emit, e.g. `{"text":"⏱ 05:00","class":"running"}`.
//!
//! Deliberately not a JSON parser: these payloads are single flat objects from
//! known producers. What it does handle correctly is escapes — a naive
//! `find("\"text\":\"")` plus "up to the next quote" breaks on `\"` inside the
//! value, which is exactly the kind of silent corruption we want to avoid.

/// Value of a top-level string field, with escape sequences resolved.
pub fn string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut from = 0;
    loop {
        let at = json[from..].find(&needle)? + from;
        let after = &json[at + needle.len()..];
        let trimmed = after.trim_start();
        // Must be followed by a colon, otherwise this was a value, not a key.
        if let Some(rest) = trimmed.strip_prefix(':') {
            let rest = rest.trim_start();
            if let Some(body) = rest.strip_prefix('"') {
                return Some(unescape(body));
            }
            return None; // field exists but is not a string
        }
        from = at + needle.len();
    }
}

/// The slice holding the object at `key`, braces included.
///
/// Needed because these payloads are no longer flat: a Spotify track has an
/// `album` object whose `"name"` comes *before* the track's own, so
/// `string_field` — which takes the first match — reads the album title as the
/// song title. Narrowing to the right object first is what keeps them apart.
pub fn object_field<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\"", key);
    let mut from = 0;
    loop {
        let at = json[from..].find(&needle)? + from;
        let after = &json[at + needle.len()..];
        if let Some(rest) = after.trim_start().strip_prefix(':') {
            let rest = rest.trim_start();
            if rest.starts_with('{') {
                let start = json.len() - rest.len();
                return Some(&json[start..start + span(rest)?]);
            }
        }
        from = at + needle.len();
    }
}

/// The first element of the array at `key`, or None when it is empty.
///
/// `artists` is a list even when there is one name, which is the usual case.
pub fn array_first<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\"", key);
    let at = json.find(&needle)?;
    let rest = json[at + needle.len()..].trim_start().strip_prefix(':')?.trim_start();
    let inner = rest.strip_prefix('[')?.trim_start();
    if inner.starts_with('{') {
        let start = json.len() - inner.len();
        return Some(&json[start..start + span(inner)?]);
    }
    None
}

/// Length of the object starting at the first byte of `s`, braces included.
///
/// Counts nesting depth while ignoring braces inside strings — a track title
/// with a `{` in it would otherwise close the object early.
fn span(s: &str) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Reads a JSON string body up to its closing quote, resolving escapes.
fn unescape(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    // \uXXXX, plus the low half when this is a UTF-16
                    // surrogate pair — emoji arrive that way, e.g. 🎧 as
                    // 🎧. Decoding the halves separately would yield
                    // two invalid characters.
                    let hex: String = chars.by_ref().take(4).collect();
                    let Some(unit) = u32::from_str_radix(&hex, 16).ok() else {
                        out.push('\u{fffd}');
                        continue;
                    };
                    let code = if (0xD800..0xDC00).contains(&unit) {
                        // High surrogate: expect a following \uXXXX low half.
                        let mut lookahead = chars.clone();
                        let low = (lookahead.next() == Some('\\')
                            && lookahead.next() == Some('u'))
                        .then(|| lookahead.by_ref().take(4).collect::<String>())
                        .and_then(|h| u32::from_str_radix(&h, 16).ok())
                        .filter(|u| (0xDC00..0xE000).contains(u));
                        match low {
                            Some(low) => {
                                chars = lookahead;
                                0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                            }
                            None => unit,
                        }
                    } else {
                        unit
                    };
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                Some(other) => out.push(other), // covers \" \\ \/
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_simple_field() {
        let json = r#"{"text":"⏱ 05:00","class":"running"}"#;
        assert_eq!(string_field(json, "text").as_deref(), Some("⏱ 05:00"));
        assert_eq!(string_field(json, "class").as_deref(), Some("running"));
        assert_eq!(string_field(json, "missing"), None);
    }

    #[test]
    fn tolerates_whitespace() {
        let json = r#"{ "text" : "value" , "n": 1 }"#;
        assert_eq!(string_field(json, "text").as_deref(), Some("value"));
    }

    /// The naive approach truncated at the first `"`, losing everything after
    /// an escaped quote.
    #[test]
    fn resolves_escaped_quotes() {
        let json = r#"{"text":"say \"hi\" now","class":"x"}"#;
        assert_eq!(string_field(json, "text").as_deref(), Some(r#"say "hi" now"#));
    }

    #[test]
    fn resolves_other_escapes() {
        let json = r#"{"text":"a\\b\tc\nd","x":1}"#;
        assert_eq!(string_field(json, "text").as_deref(), Some("a\\b\tc\nd"));
        // \uXXXX, as battery-power.sh emits for its icons.
        let uni = r#"{"text":"   74%"}"#;
        assert_eq!(string_field(uni, "text").as_deref(), Some("\u{f240}   74%"));
    }

    /// Emoji arrive as UTF-16 surrogate pairs; decoding the halves separately
    /// produces two invalid characters instead of the glyph.
    #[test]
    fn resolves_surrogate_pairs() {
        // The exact payload bt-headphones-battery emits for the headphone icon.
        let json = r#"{"text": "\ud83c\udfa7 64%"}"#;
        assert_eq!(string_field(json, "text").as_deref(), Some("\u{1f3a7} 64%"));

        // A lone high surrogate has no low half: keep a replacement char
        // rather than swallowing what follows.
        let lone = r#"{"text":"\ud83c ok"}"#;
        assert_eq!(string_field(lone, "text").as_deref(), Some("\u{fffd} ok"));

        // A high surrogate followed by a normal escape is not a pair.
        let mixed = r#"{"text":"\ud83c\n"}"#;
        assert_eq!(string_field(mixed, "text").as_deref(), Some("\u{fffd}\n"));
    }

    /// A key appearing as a *value* must not be mistaken for the field.
    #[test]
    fn ignores_the_key_appearing_as_a_value() {
        let json = r#"{"class":"text","text":"real"}"#;
        assert_eq!(string_field(json, "text").as_deref(), Some("real"));
    }

    #[test]
    fn handles_non_string_and_malformed_input() {
        assert_eq!(string_field(r#"{"text":42}"#, "text"), None);
        assert_eq!(string_field("", "text"), None);
        assert_eq!(string_field("not json", "text"), None);
        // Unterminated string: return what is there rather than panicking.
        assert_eq!(string_field(r#"{"text":"abc"#, "text").as_deref(), Some("abc"));
    }
}
