//! The bar's three zones and how each source is refreshed.
//!
//! Zones are plain concatenated strings (ADR-010). What differs per module is
//! *when* it recomputes: push events where the source offers them, its own
//! interval otherwise (ADR-015).
//!
//! Each zone also reports what its segments activate, so the click handler
//! can map a click back to a widget (ADR-012). Segments are built here rather
//! than parsed back out of the rendered string.

use crate::actions::{Target, Widget};
use crate::icons;
use crate::WsEntry;

/// Marks the boundary between widgets inside a zone string.
///
/// It is a Unit Separator (U+001F), not a printable character: adjacent widgets
/// are told apart by **alternating colours** (`settings::WIDGET_COLORS`), not by
/// a drawn glyph. The renderer splits on this to know where one widget's colour
/// ends, and the font has no glyph for it, so it costs zero horizontal space.
pub const SEP: &str = "\u{1f}";

/// A zone: the text to draw, what each segment activates, and the colour each
/// segment is drawn in.
pub struct Zone {
    pub text: String,
    pub targets: Vec<Target>,
    pub colors: Vec<u32>,
    /// Index of the segment that may be shortened to fit, if any. The track
    /// preview has no natural length, so it absorbs whatever room the other
    /// widgets leave rather than being cut at an arbitrary character count.
    pub elastic: Option<usize>,
}

/// Builds a zone from (text, target, colour) triples, dropping the empty ones
/// so a missing module leaves neither an orphan separator nor a stray hit box.
fn assemble(parts: Vec<(String, Target, u32)>) -> Zone {
    let kept: Vec<(String, Target, u32)> =
        parts.into_iter().filter(|(text, _, _)| !text.is_empty()).collect();
    Zone {
        text: kept.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>().join(SEP),
        targets: kept.iter().map(|(_, target, _)| target.clone()).collect(),
        colors: kept.iter().map(|(_, _, c)| *c).collect(),
        elastic: None,
    }
}

/// Module state that must persist between refreshes: sampling baselines, live
/// connections and cached helper output.
/// Readings kept for a while: sensors that move slowly are not read on every
/// tick, and a value that did not change does not redraw the bar.
#[derive(Default)]
pub struct Every {
    cache: std::collections::HashMap<&'static str, (String, std::time::Instant)>,
}

impl Every {
    fn get(&mut self, key: &'static str, secs: u64, read: impl FnOnce() -> String) -> String {
        let now = std::time::Instant::now();
        match self.cache.get(key) {
            Some((v, at)) if now.duration_since(*at).as_secs() < secs => v.clone(),
            _ => {
                let v = read();
                self.cache.insert(key, (v.clone(), now));
                v
            }
        }
    }
}

#[derive(Default)]
pub struct State {
    /// Throttled readings.
    pub every: Every,
    pub cpu: super::cpu::Cpu,
    pub audio: super::pulseaudio::Audio,
    pub bluetooth: super::bluetooth::Bluetooth,
    /// Set by the compositor (ToBar::Display).
    pub lid: String,
    pub screensaver: String,
    pub bt_headphones: super::bt_battery::BtBattery,
    /// Notification count, set by the compositor.
    pub notifications: String,
    /// Do not disturb: the bell turns grey.
    pub dnd: bool,
    /// Countdown timer.
    pub timer: super::timer::Timer,
    /// Whether the clock shows the date as well as the time. Off at startup:
    /// the date costs 96 px and is rarely the thing being looked for.
    pub clock_date: bool,
    /// Now playing, pushed by Spotify over MPRIS.
    pub spotify: super::spotify::NowPlaying,
    /// MEGA sync state, refreshed on its own slow interval.
    pub mega: super::mega::Sync,
}

impl State {
    /// Drops the cached values of the widgets a click can change, so the next
    /// render re-reads them. Cheap: these helpers only run when asked.
    pub fn invalidate_cached(&mut self) {
        self.bt_headphones.invalidate();
        self.every.cache.clear();
    }
}

/// The bar's two halves: workspaces and the track on the left, status widgets
/// on the right, each half packed at its own edge (ADR-032).
pub struct Bar {
    pub left: Zone,
    pub right: Zone,
}

/// Builds the whole bar.
pub fn bar(state: &mut State, list: &[WsEntry]) -> Bar {
    // One segment per workspace: each gets its own colour — which is how the
    // focused one is marked — and its own hit box, with no special case.
    let mut left: Vec<(String, Target, u32)> = list
        .iter()
        .map(|w| {
            // Focus wins over urgency: standing on a workspace is itself the
            // acknowledgement.
            let color = if w.active {
                icons::color::WORKSPACE_ACTIVE
            } else if w.urgent {
                icons::color::WORKSPACE_URGENT
            } else {
                icons::color::WORKSPACES
            };
            (super::workspaces::label(w), Target::Workspace { id: w.id, name: w.name.clone() }, color)
        })
        .collect();

    // What Spotify is playing, when it is. It disappears entirely when nothing
    // is playing, rather than leaving an empty slot.
    //
    // It is also the **elastic** segment: the one the renderer shortens when
    // the two halves would collide. That role fell to it when the clipboard
    // widget was removed, and it suits the track line better anyway — it is by
    // far the longest thing on the left, and the only one that can lose its
    // tail without losing its meaning.
    let playing = state.spotify.text();
    let spotify_index = left.len();
    if !playing.is_empty() {
        left.push((
            icons::with(icons::MUSIC, &playing),
            Target::Widget(Widget::Spotify),
            icons::color::SPOTIFY,
        ));
    }

    let mut left_zone = assemble(left);
    if !playing.is_empty() {
        left_zone.elastic = Some(spotify_index);
    }

    Bar { left: left_zone, right: assemble(status_widgets(state)) }
}

/// Muted devices turn red: the state people need to notice at a glance, and
/// the one that catches them out mid-call.
fn sink_color(state: &mut State) -> u32 {
    match state.audio.sink_muted() {
        Some(true) => icons::color::MUTED,
        _ => icons::color::VOLUME,
    }
}

fn source_color(state: &mut State) -> u32 {
    match state.audio.mic_muted() {
        Some(true) => icons::color::MUTED,
        _ => icons::color::MIC,
    }
}

/// The lid widget's label: one letter, since the colour carries the state.
pub const LID_LABEL: &str = "L";

/// Green when the lid switch is inhibited (the laptop stays awake with the lid
/// shut), red when it is not. An unknown state stays neutral rather than
/// claiming either.
fn lid_color(text: &str) -> u32 {
    let upper = text.to_ascii_uppercase();
    if upper.contains("ON") {
        icons::color::LID_ON
    } else if upper.contains("OFF") {
        icons::color::LID_OFF
    } else {
        icons::color::LID
    }
}

/// The timer, showing its icon alone while idle.
fn timer_text(state: &mut State) -> String {
    let text = state.timer.text();
    // "--:--" is the helper's way of saying nothing is running.
    if text.is_empty() || text.contains("--") {
        return icons::TIMER.to_string();
    }
    icons::with(icons::TIMER, &text)
}

/// Green while charging, amber on battery: the distinction people actually
/// look for, which the percentage alone does not give.
fn battery_color() -> u32 {
    if super::battery::is_charging() {
        icons::color::BATTERY_CHARGING
    } else {
        icons::color::BATTERY_DRAINING
    }
}

/// CPU usage and frequency in one widget: they describe the same thing and
/// share a click action, so splitting them cost a second icon and a gap.
fn cpu_text(cpu: &mut super::cpu::Cpu) -> String {
    let usage = cpu.text();
    let freq = super::cpu_freq::text();
    match (usage.is_empty(), freq.is_empty()) {
        (true, true) => String::new(),
        (false, true) => icons::with(icons::CPU, &usage),
        (true, false) => icons::with(icons::CPU, &freq),
        (false, false) => icons::with(icons::CPU, &format!("{} {}", usage, freq)),
    }
}

/// Colour of the CPU widget: one per power level, as Waybar's
/// `#custom-cpu-freq.levelN` did. Falls back to the neutral colour when the
/// level is unknown.
fn cpu_color() -> u32 {
    match super::cpu_freq::level() {
        Some(level) => icons::color::CPU_LEVEL[(level - 1) as usize],
        None => icons::color::CPU,
    }
}

/// The status widgets, in the order Waybar had. Widgets Waybar gave no action
/// to are grouped with a neighbour that had one, matching where the click
/// landed. Each is padded to its slot so the row does not shift as values
/// change width.
fn status_widgets(state: &mut State) -> Vec<(String, Target, u32)> {
    vec![
        (icons::CAMERA.to_string(), Target::Widget(Widget::Screenshot), icons::color::SCREENSHOT),
        (icons::with(icons::TEMP, &state.every.get("temp", 3, super::temp::text)), Target::Widget(Widget::CpuFreq), icons::color::TEMP),
        (icons::with(icons::MEMORY, &state.every.get("memory", 5, super::memory::text)), Target::Widget(Widget::CpuFreq), icons::color::MEMORY),
        ({
            let State { every, cpu, .. } = state;
            every.get("cpu", 2, || cpu_text(cpu))
        }, Target::Widget(Widget::CpuFreq), cpu_color()),
        // Icon only: the sync state is carried by the colour, so there is no
        // text competing with it.
        (icons::CLOUD.to_string(), Target::Widget(Widget::CpuFreq), state.mega.color()),
        // Output and input are separate widgets: each mutes the device it
        // shows, so the click lands where the eye already is.
        (state.audio.sink_text(), Target::Widget(Widget::Volume), sink_color(state)),
        (state.audio.source_text(), Target::Widget(Widget::Microphone), source_color(state)),
        (icons::with(icons::WIFI, &state.every.get("network", 5, super::network::text)), Target::Widget(Widget::Network), icons::color::NETWORK),
        (state.bluetooth.text(), Target::Widget(Widget::BtHeadphones), icons::color::BLUETOOTH),
        (icons::with(icons::HEADPHONES, &state.bt_headphones.text()), Target::Widget(Widget::BtHeadphones), icons::color::BT_HEADPHONES),
        (icons::with(icons::BRIGHTNESS, &super::backlight::text()), Target::Widget(Widget::Backlight), icons::color::BACKLIGHT),
        (icons::with(icons::BATTERY, &state.every.get("battery", 15, super::battery::text)), Target::Widget(Widget::Battery), state.every.get("battery color", 15, || battery_color().to_string()).parse().unwrap_or(icons::color::BATTERY_DRAINING)),
        (
            icons::with(icons::BELL, &if state.dnd { format!("{} DND", state.notifications) } else { state.notifications.clone() }),
            Target::Widget(Widget::Notifications),
            if state.dnd { icons::color::DND } else { icons::color::NOTIFICATIONS },
        ),
        (icons::BLANK.to_string(), Target::Widget(Widget::Blk), icons::color::BLK),
        (icons::with(icons::MOON, &state.screensaver), Target::Widget(Widget::Screensaver), icons::color::SCREENSAVER),
        // Idle timers show the icon alone: "--:--" is five characters saying
        // nothing is running, which the bare icon says in one.
        (timer_text(state), Target::Widget(Widget::Timer), icons::color::TIMER),
        // The lid is one letter, red or green: "LID:OFF" spelled out a state
        // the colour already carries.
        (LID_LABEL.to_string(), Target::Widget(Widget::Lid), lid_color(&state.lid)),
        // No icon: a clock face beside "14:21" says nothing the digits do not.
        // Clicking adds the date, which is off by default to save the width.
        (super::clock::text(state.clock_date), Target::Widget(Widget::Clock), icons::color::CLOCK),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_skips_empty_parts() {
        let t = || Target::Widget(Widget::Blk);
        let c = icons::color::BLK;
        let zone = assemble(vec![("a".into(), t(), c), ("".into(), t(), c), ("b".into(), t(), c)]);
        assert_eq!(zone.text, format!("a{SEP}b"));
        assert_eq!(zone.targets.len(), 2, "the empty part gets no hit box");

        let empty = assemble(vec![("".into(), t(), c)]);
        assert_eq!(empty.text, "");
        assert!(empty.targets.is_empty());
    }

    /// The CPU widget takes one colour per power level, as Waybar's
    /// `#custom-cpu-freq.levelN` did, and falls back to a neutral one when the
    /// level is unknown.
    #[test]
    fn cpu_colour_follows_the_power_level() {
        // Every level maps to a distinct colour, so the state is readable.
        let palette = icons::color::CPU_LEVEL;
        for i in 0..palette.len() {
            for j in (i + 1)..palette.len() {
                assert_ne!(palette[i], palette[j], "levels {i} and {j} share a colour");
            }
        }
        // Whatever this machine reports, the widget picks from the palette or
        // the neutral colour — never an out-of-range index.
        let c = cpu_color();
        assert!(
            palette.contains(&c) || c == icons::color::CPU,
            "unexpected colour {c:06X}"
        );
    }

    /// Text segments and targets must stay aligned, or a click lands on the
    /// wrong widget.
    #[test]
    fn segments_and_targets_line_up() {
        let mut state = State::default();
        let _ = state.cpu.text();
        let zone = bar(&mut state, &[]).right;
        assert_eq!(
            zone.text.split(SEP).count(),
            zone.targets.len(),
            "segment count must match target count: {:?}",
            zone.text
        );
    }

    /// Colours must line up with segments, exactly like targets do: a shifted
    /// palette would paint each widget in its neighbour's colour.
    #[test]
    fn segments_and_colors_line_up() {
        let mut state = State::default();
        let _ = state.cpu.text();
        let zone = bar(&mut state, &[]).right;
        assert_eq!(zone.text.split(SEP).count(), zone.colors.len());
        assert_eq!(zone.targets.len(), zone.colors.len());

        // The right half holds only status widgets, so every neighbour pair
        // must differ: with no separator, colour is the only boundary.
        let same_as_next = zone.colors.windows(2).filter(|w| w[0] == w[1]).count();
        assert_eq!(same_as_next, 0, "adjacent widgets share a colour: {:?}", zone.colors);
    }

    /// The bar must include every ported module's output.
    #[test]
    fn bar_includes_all_modules() {
        let mut state = State::default();
        let _ = state.cpu.text();
        let text = bar(&mut state, &[]).right.text;
        assert!(text.contains('%'), "expected percentages: {text:?}");
        assert!(text.contains("GHz"), "expected cpu freq: {text:?}");
        assert!(text.contains('°'), "expected temperature: {text:?}");
        assert!(text.contains(':'), "expected clock: {text:?}");
        // The lid is a single letter now, its state carried by the colour.
        assert!(text.contains(LID_LABEL), "expected lid letter: {text:?}");
    }

    /// The bar opens with one segment per workspace, each targeting its own,
    /// with the focused one alone in the active colour and urgency shown
    /// on the others.
    #[test]
    fn the_bar_starts_with_workspaces() {
        use super::super::workspaces::{entry, label};
        let mut state = State::default();
        let mut list = vec![entry(-1337, "chat"), entry(1, "1"), entry(2, "2"), entry(31, "31")];
        list[1].active = true;
        list[1].urgent = true;
        list[3].urgent = true;
        let zone = bar(&mut state, &list).left;

        let segments: Vec<&str> = zone.text.split(SEP).collect();
        for (i, ws) in list.iter().enumerate() {
            assert_eq!(segments[i], label(ws), "segment {i}");
            assert_eq!(
                zone.targets[i],
                Target::Workspace { id: ws.id, name: ws.name.clone() }
            );
        }
        assert_eq!(
            zone.colors[..4],
            [
                icons::color::WORKSPACES,
                icons::color::WORKSPACE_ACTIVE,
                icons::color::WORKSPACES,
                icons::color::WORKSPACE_URGENT,
            ]
        );
        assert_eq!(segments.len(), zone.targets.len());
    }
}

#[cfg(test)]
mod mic_hitbox {
    use super::*;

    /// Every segment of the left zone must own a hit box wide enough to click:
    /// a target the pointer never lands on is a dead widget.
    #[test]
    fn the_left_zone_is_clickable() {
        let mut state = State::default();
        let bar = bar(&mut state, &[super::super::workspaces::entry(1, "1")]);
        let font = crate::draw::load_font().expect("embedded font");

        let boxes = crate::draw::segment_hitboxes(&font, crate::settings::PAD, &bar.left.text);
        assert!(!boxes.is_empty(), "no hit boxes at all");
        assert_eq!(boxes.len(), bar.left.targets.len(), "one box per target");
        for (b, t) in boxes.iter().zip(&bar.left.targets) {
            assert!(b.x1 > b.x0, "{t:?} has no width");
            // The midpoint of a box must fall inside it, which is what the
            // pointer handler relies on to resolve a click.
            assert!(b.contains((b.x0 + b.x1) / 2), "{t:?} unreachable");
        }
    }

    /// The clock starts bare and a click adds the date, which is the whole
    /// point of keeping the state on the bar rather than in the module.
    #[test]
    fn the_clock_shows_the_date_only_when_asked() {
        let mut state = State::default();
        assert!(!state.clock_date, "the date must be off at startup");
        let bare = super::super::clock::text(state.clock_date);
        assert!(!bare.contains('/'), "no date expected: {bare:?}");

        state.clock_date = true;
        let dated = super::super::clock::text(state.clock_date);
        assert!(dated.starts_with(&bare), "the time must not move: {dated:?}");
        assert_eq!(dated.matches('/').count(), 2, "expected a date: {dated:?}");
    }
}

#[cfg(test)]
mod width_audit {
    use super::*;

    /// What each segment actually costs on screen, to decide where the room
    /// goes rather than guessing.
    #[test]
    fn audit_segment_widths() {
        let font = crate::draw::load_font().expect("font");
        let mut state = State::default();
        let bar = bar(&mut state, &[]);
        let cell = crate::draw::text_width(&font, "M");

        for (name, zone) in [("LEFT", &bar.left), ("RIGHT", &bar.right)] {
            let segs: Vec<&str> = zone.text.split(SEP).collect();
            let total = crate::draw::zone_width(&font, &zone.text);
            println!("\n=== {name}: {total} px, {} segments ===", segs.len());
            for s in &segs {
                let w = crate::draw::text_width(&font, s);
                println!("  {w:4} px  {:2} chars  {s:?}", s.chars().count());
            }
        }
        println!("\ncell 'M' = {cell} px");
        println!("PAD={} WIDGET_GAP={}", crate::settings::PAD, crate::settings::WIDGET_GAP);
    }
}

#[cfg(test)]
mod elastic_tests {
    use super::*;

    /// The track is the segment that gives way when the halves would collide.
    /// Without this the two would overlap: the renderer has nothing else it is
    /// allowed to shorten.
    #[test]
    fn the_track_is_the_elastic_segment() {
        let mut state = State::default();
        state.spotify.set_track_for_test("1970 - Santana - Samba Pa Ti - Abraxas");
        let bar = bar(&mut state, &[]);
        let Some(index) = bar.left.elastic else {
            panic!("no elastic segment: the halves can collide");
        };
        assert_eq!(
            bar.left.targets[index],
            Target::Widget(Widget::Spotify),
            "the elastic segment must be the track"
        );
    }

    /// With nothing playing there is no elastic segment — and nothing long
    /// enough on the left to need one.
    #[test]
    fn silence_needs_no_elastic_segment() {
        let mut state = State::default();
        state.spotify.set_track_for_test("");
        let bar = bar(&mut state, &[]);
        assert!(bar.left.elastic.is_none());
    }
}
