//! Volume: "{sink}% {source}%", the sink volume plus the microphone level.
//! Mirrors Waybar's `pulseaudio` format "{volume}% {format_source}", where
//! format-source was "{volume}%" and the muted variants dropped the number.
//!
//! State lives in `Audio` because the connection is long-lived: it doubles as
//! the event subscription that drives refreshes (ADR-015).

use crate::icons;
use crate::pulse::Pulse;

/// Long-lived PulseAudio connection for the volume widget.
#[derive(Default)]
pub struct Audio {
    pulse: Option<Pulse>,
    /// Last good text per device, shown while the server is unreachable.
    last_sink: String,
    last_source: String,
}


/// Renders one device: its icon and level, or the icon alone when muted.
///
/// A muted device drops its number rather than spelling out "muted": the
/// colour says it, and the word cost eight characters of bar (ADR-042).
fn render_one(icon: char, device: Option<(u64, bool)>) -> String {
    match device {
        Some((_, true)) => icon.to_string(),
        Some((v, false)) => format!("{} {}%", icon, v),
        None => String::new(),
    }
}

/// Both devices in one string, kept for the tests that pin the old format.
#[cfg(test)]
fn render(sink: Option<(u64, bool)>, source: Option<(u64, bool)>) -> String {
    [render_one(icons::VOLUME, sink), render_one(icons::MIC, source)]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Audio {
    /// Ensures a live connection, reconnecting if the server restarted.
    fn ensure(&mut self) -> Option<&mut Pulse> {
        if self.pulse.is_none() {
            let mut pulse = Pulse::connect()?;
            pulse.subscribe()?;
            self.pulse = Some(pulse);
        }
        self.pulse.as_mut()
    }

    /// The output's line: icon plus level, icon alone when muted.
    ///
    /// A server that stops answering drops the connection so the next call
    /// reconnects, and the last known text is shown meanwhile — a widget that
    /// blanks on a hiccup is worse than one a second out of date.
    pub fn sink_text(&mut self) -> String {
        let sink = self.ensure().and_then(|p| p.default_sink());
        if sink.is_none() {
            self.pulse = None;
            return std::mem::take(&mut self.last_sink);
        }
        self.last_sink = render_one(icons::VOLUME, sink.map(|d| (d.volume_percent, d.muted)));
        self.last_sink.clone()
    }

    /// The microphone's line, same shape.
    pub fn source_text(&mut self) -> String {
        let source = self.ensure().and_then(|p| p.default_source());
        if source.is_none() {
            self.pulse = None;
            return std::mem::take(&mut self.last_source);
        }
        self.last_source = render_one(icons::MIC, source.map(|d| (d.volume_percent, d.muted)));
        self.last_source.clone()
    }

    /// Whether the output is muted, for the colour the widget takes.
    pub fn sink_muted(&mut self) -> Option<bool> {
        self.ensure()?.default_sink().map(|d| d.muted)
    }

    /// Flips the output's mute state, returning the new one.
    pub fn toggle_sink(&mut self) -> Option<bool> {
        let pulse = self.ensure()?;
        let sink = pulse.default_sink()?;
        pulse.set_sink_mute(sink.index, !sink.muted)?;
        Some(!sink.muted)
    }

    /// Moves the output volume by `delta` points, within 0..=100; returns the
    /// new level and whether the output is muted.
    pub fn change_sink(&mut self, delta: i64) -> Option<(u64, bool)> {
        let pulse = self.ensure()?;
        let sink = pulse.default_sink()?;
        let level = (sink.volume_percent as i64 + delta).clamp(0, 100) as u64;
        pulse.set_sink_volume(sink.index, sink.channels, level)?;
        Some((level, sink.muted))
    }

    /// Level and mute state of the output and of the microphone.
    pub fn sink_state(&mut self) -> Option<(u64, bool)> {
        self.ensure()?.default_sink().map(|d| (d.volume_percent, d.muted))
    }

    pub fn source_state(&mut self) -> Option<(u64, bool)> {
        self.ensure()?.default_source().map(|d| (d.volume_percent, d.muted))
    }

    pub fn mixer(&mut self) -> Option<crate::Mixer> {
        self.ensure()?.mixer()
    }

    pub fn mix(&mut self, a: &crate::MixAction) -> Option<()> {
        self.ensure()?.mix(a)
    }

    /// The outputs as (name, description, default).
    pub fn outputs(&mut self) -> Option<Vec<(String, String, bool)>> {
        let pulse = self.ensure()?;
        let default = pulse.default_sink_name().unwrap_or_default();
        Some(pulse.sinks()?.into_iter().map(|(n, d)| {
            let on = n == default;
            (n, d, on)
        }).collect())
    }

    pub fn set_output(&mut self, name: &str) -> Option<()> {
        self.ensure()?.set_default_sink(name)
    }

    /// Whether the microphone is muted, for the widget that shows it.
    ///
    /// `None` when the server cannot be reached: the button then says it does
    /// not know rather than claiming the mic is live.
    pub fn mic_muted(&mut self) -> Option<bool> {
        self.ensure()?.default_source().map(|d| d.muted)
    }

    /// Flips the microphone's mute state.
    ///
    /// PulseAudio has no toggle command, so the current state is read first and
    /// its opposite sent. Returns the new state, or None if either step failed.
    pub fn toggle_mic(&mut self) -> Option<bool> {
        let pulse = self.ensure()?;
        let source = pulse.default_source()?;
        pulse.set_source_mute(source.index, !source.muted)?;
        let muted = source.muted;
        // The bar repaints from the subscription event that follows, but
        // returning the new state lets the caller repaint immediately.
        Some(!muted)
    }

    /// Consumes pending subscription events; true when something changed.
    pub fn drain_events(&mut self) -> bool {
        match self.pulse.as_mut() {
            Some(pulse) => pulse.drain_events(),
            None => false,
        }
    }

    /// Raw fd of the connection, for registering with the event loop.
    /// Whether the PulseAudio connection died.
    pub fn server_closed(&self) -> bool {
        self.pulse.as_ref().is_some_and(|p| p.is_closed())
    }

    pub fn raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        self.pulse.as_ref().map(|p| p.stream().as_raw_fd())
    }
}

/// Serialises the tests that touch the live audio server: they share one
/// device, so a reader and a toggle running together see each other's state.
#[cfg(test)]
static AUDIO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Handle on that lock, for the tests living in `pulse` rather than here.
///
/// A poisoned mutex is recovered rather than propagated: one failing test
/// should not cascade into every other one reporting a panic it did not cause.
#[cfg(test)]
pub fn audio_test_lock() -> std::sync::MutexGuard<'static, ()> {
    AUDIO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_both_devices() {
        assert_eq!(
            render(Some((80, false)), Some((22, false))),
            format!("{} 80% {} 22%", icons::VOLUME, icons::MIC)
        );
    }

    #[test]
    fn muted_devices_drop_their_number() {
        let (v, m) = (icons::VOLUME, icons::MIC);
        // Muted shows the bare icon: the colour says the rest, and the word
        // "muted" cost eight characters of bar per device.
        assert_eq!(render(Some((80, true)), Some((22, false))), format!("{v} {m} 22%"));
        assert_eq!(render(Some((80, false)), Some((22, true))), format!("{v} 80% {m}"));
        assert_eq!(render(Some((0, true)), Some((0, true))), format!("{v} {m}"));
    }

    #[test]
    fn missing_devices_are_omitted() {
        assert_eq!(render(Some((50, false)), None), format!("{} 50%", icons::VOLUME));
        assert_eq!(render(None, Some((30, false))), format!("{} 30%", icons::MIC));
        assert_eq!(render(None, None), "");
    }

    /// End-to-end against the live server, if present. The two devices render
    /// separately now, and each carries its own icon.
    #[test]
    fn reads_the_live_server() {
        let _guard = AUDIO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut audio = Audio::default();
        let out = audio.sink_text();
        if out.is_empty() {
            return; // no audio server
        }
        assert!(out.starts_with(icons::VOLUME), "output: {out:?}");

        let inp = audio.source_text();
        assert!(inp.starts_with(icons::MIC), "input: {inp:?}");
        // A level, or the bare icon when muted — never the word "muted".
        for t in [&out, &inp] {
            assert!(
                t.contains('%') || t.chars().count() == 1,
                "unexpected: {t:?}"
            );
        }
    }
}

#[cfg(test)]
mod live_mic {
    use super::*;

    /// Flips the real microphone twice, so it ends where it started.
    ///
    /// Serialised against the other live audio tests: they share one server,
    /// and a reader sampling the device mid-flip sees a state that belongs to
    /// neither side of the toggle.
    ///
    /// The round trip is the point: it proves the write reached the server and
    /// that reading it back sees the change, which a one-way call cannot.
    #[test]
    fn toggling_the_microphone_round_trips() {
        let _guard = super::AUDIO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut audio = Audio::default();
        let Some(before) = audio.mic_muted() else {
            return; // no PulseAudio here
        };

        let flipped = audio.toggle_mic().expect("toggle must reach the server");
        assert_eq!(flipped, !before, "the reported state must be the new one");
        assert_eq!(audio.mic_muted(), Some(!before), "the server must agree");

        // Put it back, so running the test does not leave the mic muted.
        audio.toggle_mic().expect("restore");
        assert_eq!(audio.mic_muted(), Some(before), "must end as it started");
    }
}

#[cfg(test)]
mod live_sink {
    use super::*;

    /// Flips the real output twice, so it ends where it started. The round
    /// trip is what proves the write reached the server: a one-way call
    /// cannot tell an accepted command from an ignored one.
    #[test]
    fn toggling_the_output_round_trips() {
        let _guard = super::AUDIO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut audio = Audio::default();
        let Some(before) = audio.sink_muted() else {
            return; // no PulseAudio here
        };

        let flipped = audio.toggle_sink().expect("toggle must reach the server");
        assert_eq!(flipped, !before);
        assert_eq!(audio.sink_muted(), Some(!before), "the server must agree");

        audio.toggle_sink().expect("restore");
        assert_eq!(audio.sink_muted(), Some(before), "must end as it started");
    }
}
