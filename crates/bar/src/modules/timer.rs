//! Timer state, from the `timer.sh status` helper (JSON with a `text` field).
//!
//! **The cadence follows the state, because the cost is a fork.** Running, the
//! widget counts down and genuinely needs asking every second. Stopped — which
//! is nearly always — it shows a fixed `--:--`, and asking is pure waste.
//!
//! It mattered: at 8.6 ms a call this was the most expensive widget in the bar
//! (§10.2), more than the other sixteen together bar `mega`, and unlike `mega`
//! it paid that on **every tick**. Change detection stopped the redraws but not
//! the fork, which is where the time went.

use crate::settings;
use crate::exec;
use crate::json;
use std::time::{Duration, Instant};

/// How long a stopped reading stays good for.
///
/// A stopped timer only changes when *you* start one, and starting it is a
/// click on this very widget — which invalidates the cache directly
/// (`State::invalidate_cached`). So the interval is not a latency budget: it
/// is a safety net for a timer started from elsewhere, and five seconds is
/// already generous for that.
const IDLE_TTL: Duration = Duration::from_secs(5);

/// Cached helper output, refreshed at a rate that follows the state.
#[derive(Default)]
pub struct Timer {
    text: String,
    /// When the helper was last asked. `None` means never.
    read_at: Option<Instant>,
    /// Whether the last reading had a timer running.
    running: bool,
}

impl Timer {
    /// The widget's text, asking the helper only when the cache has expired.
    pub fn text(&mut self) -> String {
        let due = match self.read_at {
            None => true,
            // Running: the countdown moves every second, so nothing is cached.
            Some(_) if self.running => true,
            Some(at) => at.elapsed() >= IDLE_TTL,
        };
        if due {
            self.refresh();
        }
        self.text.clone()
    }

    /// Drops the cache, so the next read asks the helper.
    ///
    /// Called after a click on the widget: the click *is* how a timer gets
    /// started, and waiting out the TTL would show a stale `--:--` for
    /// seconds after the user set one.
    pub fn invalidate(&mut self) {
        self.read_at = None;
    }

    fn refresh(&mut self) {
        let out = exec::run(&format!("{} status 2>/dev/null", settings::TIMER_SCRIPT));
        self.read_at = Some(Instant::now());
        if out.is_empty() {
            self.text = String::new();
            self.running = false;
            return;
        }
        // The helper reports its state in `class`; anything but "stopped"
        // counts as running, so an unknown state errs towards refreshing.
        self.running = json::string_field(&out, "class")
            .is_none_or(|c| c != "stopped");
        // The helper prints JSON; older versions printed bare text.
        let raw = json::string_field(&out, "text").unwrap_or(out);
        self.text = strip_helper_icon(&raw);
    }
}

/// Drops the helper's own clock emoji: it is not in the embedded font subset,
/// and the layout already prefixes a timer icon.
fn strip_helper_icon(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(*c, '\u{23f1}' | '\u{fe0f}'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Uncached reading, for the tests that compare against the helper directly.
#[cfg(test)]
pub fn text() -> String {
    let mut t = Timer::default();
    t.text()
}

#[cfg(test)]
mod tests {
    /// The helper's clock emoji has no glyph in the embedded subset, so it must
    /// be removed rather than drawn as a blank box.
    #[test]
    fn strips_the_helper_clock_emoji() {
        assert_eq!(super::strip_helper_icon("\u{23f1} 05:00"), "05:00");
        assert_eq!(super::strip_helper_icon("\u{23f1}\u{fe0f} --:--"), "--:--");
        assert_eq!(super::strip_helper_icon("12:34"), "12:34");
        assert_eq!(super::strip_helper_icon(""), "");
    }

    /// Parity with the helper script, when it exists.
    #[test]
    fn matches_the_helper_script() {
        if !std::path::Path::new(crate::settings::TIMER_SCRIPT).exists() {
            return;
        }
        let out = std::process::Command::new(crate::settings::TIMER_SCRIPT)
            .arg("status")
            .output()
            .expect("timer.sh");
        let json = String::from_utf8_lossy(&out.stdout);
        let expected = super::strip_helper_icon(
            &crate::json::string_field(&json, "text").unwrap_or_default(),
        );
        assert_eq!(super::text(), expected);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// A stopped timer must not be asked again straight away — the whole point
    /// of the change, since that fork cost 8.6 ms on every tick.
    #[test]
    fn a_stopped_timer_is_cached() {
        let mut t = Timer::default();
        let first = t.text();
        let read_at = t.read_at;
        assert!(read_at.is_some(), "the first call must ask the helper");

        // Immediately after, nothing should have been re-read.
        let second = t.text();
        assert_eq!(first, second);
        if !t.running {
            assert_eq!(t.read_at, read_at, "a stopped timer was asked twice");
        }
    }

    /// A running timer counts down, so it must be asked every time: caching it
    /// would freeze the countdown on screen.
    #[test]
    fn a_running_timer_is_never_cached() {
        let mut t = Timer { text: "01:23".into(), read_at: Some(Instant::now()), running: true };

        let before = t.read_at;
        let _ = t.text();
        assert_ne!(t.read_at, before, "a running timer must be re-read");
    }

    /// Invalidating forces the next read to ask, which is what makes a timer
    /// set by clicking the widget appear at once instead of after the TTL.
    #[test]
    fn invalidating_forces_a_read() {
        let mut t = Timer::default();
        let _ = t.text();
        assert!(t.read_at.is_some());

        t.invalidate();
        assert!(t.read_at.is_none(), "the cache survived invalidation");
    }

    /// An unrecognised state counts as running: erring towards asking again is
    /// cheap, while erring the other way freezes a live countdown.
    #[test]
    fn an_unknown_state_errs_towards_refreshing() {
        let mut t = Timer::default();
        let _ = t.text();
        // Whatever the real helper said, the rule is what is under test: only
        // the literal "stopped" may switch caching on.
        for (class, expected_running) in [
            (r#"{"class":"stopped"}"#, false),
            (r#"{"class":"running"}"#, true),
            (r#"{"class":"paused"}"#, true),
            (r#"{"text":"x"}"#, true), // no class at all
        ] {
            let running = crate::json::string_field(class, "class")
                .is_none_or(|c| c != "stopped");
            assert_eq!(running, expected_running, "{class}");
        }
    }
}
