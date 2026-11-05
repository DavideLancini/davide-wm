//! The audio mixer (replaces pavucontrol): an output and an input tab, each
//! with every device, every application stream and, on the output tab, the
//! sound cards; nothing is hidden. The data and the changes go through the
//! bar's PulseAudio connection (`ToBar::Mixer`, `FromBar::Mixer`).

use bar::{MixAction, Mixer};
use launcher::card::{MixRow, MIXER_ROWS};

const STEP: i32 = 5;
/// Like pavucontrol, volumes go up to 150 %.
const MAX: i32 = 150;

pub const HELP: &str = "Tab: uscita/ingresso · \u{2191}\u{2193} scegli · \u{2190}\u{2192} volume · M muto · C canale · D predefinito · P porta · Invio: sposta app / profilo scheda · Esc";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Item {
    Header(&'static str),
    Device(usize),
    Stream(usize),
    Card(usize),
}

pub struct MixerUi {
    pub data: Option<Mixer>,
    /// The output tab (else the input one).
    pub output: bool,
    selected: usize,
    /// The channel being edited, or all of them.
    channel: Option<usize>,
}

pub enum Outcome {
    Nothing,
    Redraw,
    Close,
    Action(MixAction),
}

impl MixerUi {
    pub fn new(output: bool) -> MixerUi {
        MixerUi { data: None, output, selected: 1, channel: None }
    }

    fn items(&self) -> Vec<Item> {
        let Some(m) = &self.data else { return Vec::new() };
        let (devices, streams) = if self.output { (m.sinks.len(), m.playback.len()) } else { (m.sources.len(), m.recording.len()) };
        let mut out = vec![Item::Header("Dispositivi")];
        out.extend((0..devices).map(Item::Device));
        out.push(Item::Header(if self.output { "Applicazioni che suonano" } else { "Applicazioni che registrano" }));
        out.extend((0..streams).map(Item::Stream));
        if self.output {
            out.push(Item::Header("Schede audio"));
            out.extend((0..m.cards.len()).map(Item::Card));
        }
        out
    }

    /// Keeps the selection on a row that exists and is not a title.
    pub fn settle(&mut self) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        self.selected = self.selected.min(items.len() - 1);
        if matches!(items[self.selected], Item::Header(_)) {
            self.selected = (self.selected + 1..items.len()).find(|i| !matches!(items[*i], Item::Header(_))).unwrap_or(0);
        }
    }

    pub fn scroll(&self) -> usize {
        self.selected.saturating_sub(MIXER_ROWS - 1)
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn rows(&self) -> Vec<MixRow> {
        let Some(m) = &self.data else { return Vec::new() };
        let (devices, streams) = if self.output { (&m.sinks, &m.playback) } else { (&m.sources, &m.recording) };
        let default = if self.output { &m.default_sink } else { &m.default_source };
        let device_name = |index: u32| devices.iter().find(|d| d.index == index).map_or("?".to_string(), |d| d.description.clone());
        self.items()
            .into_iter()
            .enumerate()
            .map(|(i, item)| {
                let channel = (i == self.selected).then_some(self.channel).flatten();
                match item {
                    Item::Header(t) => MixRow { title: t.into(), detail: String::new(), volume: None, header: true, marked: false },
                    Item::Device(d) => {
                        let d = &devices[d];
                        let port = d.ports.iter().find(|p| p.0 == d.active_port).map(|p| format!("porta: {}", p.1)).unwrap_or_default();
                        let ports = if d.ports.len() > 1 { format!(" ({} porte)", d.ports.len()) } else { String::new() };
                        let monitor = if d.monitor { "monitor · " } else { "" };
                        MixRow {
                            title: d.description.clone(),
                            detail: format!("{monitor}{port}{ports}"),
                            volume: Some((d.volume.clone(), d.muted, channel)),
                            header: false,
                            marked: d.name == *default,
                        }
                    }
                    Item::Stream(s) => {
                        let s = &streams[s];
                        let media = if s.media.is_empty() || s.media == s.app { String::new() } else { format!(" — {}", s.media) };
                        MixRow {
                            title: format!("{}{media}", s.app),
                            detail: format!("{} {}", if self.output { "su" } else { "da" }, device_name(s.device)),
                            volume: Some((s.volume.clone(), s.muted, channel)),
                            header: false,
                            marked: false,
                        }
                    }
                    Item::Card(c) => {
                        let c = &m.cards[c];
                        let profile = c.profiles.iter().find(|p| p.0 == c.active).map_or(c.active.clone(), |p| p.1.clone());
                        MixRow {
                            title: c.description.clone(),
                            detail: format!("profilo: {profile} ({} profili)", c.profiles.len()),
                            volume: None,
                            header: false,
                            marked: false,
                        }
                    }
                }
            })
            .collect()
    }

    fn volume_action(&self, item: Item, delta: i32) -> Option<MixAction> {
        let m = self.data.as_ref()?;
        let change = |vol: &[u32]| -> Vec<u32> {
            vol.iter()
                .enumerate()
                .map(|(c, v)| if self.channel.is_none_or(|ch| ch == c) { (*v as i32 + delta).clamp(0, MAX) as u32 } else { *v })
                .collect()
        };
        match item {
            Item::Device(d) => {
                let d = if self.output { &m.sinks[d] } else { &m.sources[d] };
                Some(MixAction::DeviceVolume { output: self.output, index: d.index, volume: change(&d.volume) })
            }
            Item::Stream(s) => {
                let s = if self.output { &m.playback[s] } else { &m.recording[s] };
                Some(MixAction::StreamVolume { output: self.output, index: s.index, volume: change(&s.volume) })
            }
            _ => None,
        }
    }

    /// The wheel over the panel: the selected row's volume.
    pub fn wheel(&self, up: bool) -> Option<MixAction> {
        let item = *self.items().get(self.selected)?;
        self.volume_action(item, if up { STEP } else { -STEP })
    }

    /// A click on row `n` of the visible ones.
    pub fn click(&mut self, n: usize) -> Outcome {
        self.selected = self.scroll() + n;
        self.channel = None;
        self.settle();
        Outcome::Redraw
    }

    pub fn key(&mut self, sym: u32, text: Option<char>) -> Outcome {
        let items = self.items();
        let Some(m) = self.data.as_ref() else {
            return if sym == 0xff1b { Outcome::Close } else { Outcome::Nothing };
        };
        let item = items.get(self.selected).copied();
        let next = |list: &[(String, String, bool)], current: &str| -> Option<String> {
            let i = list.iter().position(|p| p.0 == current).map_or(0, |i| i + 1);
            list.iter().cycle().skip(i).take(list.len()).find(|p| p.2).map(|p| p.0.clone())
        };
        match (sym, text.map(|c| c.to_ascii_lowercase())) {
            (0xff1b, _) => Outcome::Close,
            (0xff09, _) | (0xfe20, _) => {
                self.output = !self.output;
                self.selected = 1;
                self.channel = None;
                self.settle();
                Outcome::Redraw
            }
            (0xff52 | 0xff54, _) => {
                let down = sym == 0xff54;
                let mut i = self.selected;
                loop {
                    let n = if down { i + 1 } else { i.wrapping_sub(1) };
                    match items.get(n) {
                        Some(Item::Header(_)) => i = n,
                        Some(_) => {
                            self.selected = n;
                            break;
                        }
                        None => break,
                    }
                }
                self.channel = None;
                Outcome::Redraw
            }
            (0xff51 | 0xff53, _) => item.and_then(|i| self.volume_action(i, if sym == 0xff53 { STEP } else { -STEP })).map_or(Outcome::Nothing, Outcome::Action),
            (_, Some('c')) => {
                let channels = match item {
                    Some(Item::Device(d)) => if self.output { &m.sinks[d] } else { &m.sources[d] }.volume.len(),
                    Some(Item::Stream(s)) => if self.output { &m.playback[s] } else { &m.recording[s] }.volume.len(),
                    _ => 0,
                };
                self.channel = match self.channel {
                    None if channels > 1 => Some(0),
                    Some(c) if c + 1 < channels => Some(c + 1),
                    _ => None,
                };
                Outcome::Redraw
            }
            (_, Some('m')) => match item {
                Some(Item::Device(d)) => {
                    let d = if self.output { &m.sinks[d] } else { &m.sources[d] };
                    Outcome::Action(MixAction::DeviceMute { output: self.output, index: d.index, mute: !d.muted })
                }
                Some(Item::Stream(s)) => {
                    let s = if self.output { &m.playback[s] } else { &m.recording[s] };
                    Outcome::Action(MixAction::StreamMute { output: self.output, index: s.index, mute: !s.muted })
                }
                _ => Outcome::Nothing,
            },
            (_, Some('p')) => match item {
                Some(Item::Device(d)) => {
                    let d = if self.output { &m.sinks[d] } else { &m.sources[d] };
                    next(&d.ports, &d.active_port).map_or(Outcome::Nothing, |port| Outcome::Action(MixAction::Port { output: self.output, index: d.index, port }))
                }
                _ => Outcome::Nothing,
            },
            (0xff0d | 0xff8d, _) | (_, Some('d')) => match item {
                Some(Item::Device(d)) => {
                    let d = if self.output { &m.sinks[d] } else { &m.sources[d] };
                    Outcome::Action(MixAction::Default { output: self.output, name: d.name.clone() })
                }
                Some(Item::Stream(s)) if sym != 0x64 => {
                    let (s, devices) = if self.output { (&m.playback[s], &m.sinks) } else { (&m.recording[s], &m.sources) };
                    let i = devices.iter().position(|d| d.index == s.device).map_or(0, |i| i + 1);
                    match devices.iter().cycle().skip(i).take(devices.len()).find(|d| d.index != s.device) {
                        Some(d) => Outcome::Action(MixAction::Move { output: self.output, stream: s.index, device: d.index }),
                        None => Outcome::Nothing,
                    }
                }
                Some(Item::Card(c)) => {
                    let c = &m.cards[c];
                    next(&c.profiles, &c.active).map_or(Outcome::Nothing, |profile| Outcome::Action(MixAction::Profile { card: c.index, profile }))
                }
                _ => Outcome::Nothing,
            },
            _ => Outcome::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bar::{MixDevice, MixStream};

    fn dev(index: u32, name: &str) -> MixDevice {
        MixDevice { index, name: name.into(), description: name.into(), volume: vec![50, 50], muted: false, ports: Vec::new(), active_port: String::new(), monitor: false, card: 0 }
    }

    #[test]
    fn navigation_and_actions() {
        let mut ui = MixerUi::new(true);
        ui.data = Some(Mixer {
            sinks: vec![dev(1, "a"), dev(2, "b")],
            playback: vec![MixStream { index: 7, app: "app".into(), media: String::new(), device: 1, volume: vec![100], muted: false }],
            default_sink: "a".into(),
            ..Default::default()
        });
        ui.settle();
        assert_eq!(ui.selected(), 1);
        assert!(matches!(ui.key(0xff53, None), Outcome::Action(MixAction::DeviceVolume { index: 1, ref volume, .. }) if *volume == vec![55, 55]));
        ui.key(0xff54, None);
        ui.key(0xff54, None);
        // The title between devices and streams is skipped.
        assert_eq!(ui.selected(), 4);
        assert!(matches!(ui.key(0xff0d, None), Outcome::Action(MixAction::Move { stream: 7, device: 2, .. })));
        ui.key(0xff52, None);
        ui.key(0, Some('c'));
        assert!(matches!(ui.key(0xff51, None), Outcome::Action(MixAction::DeviceVolume { index: 2, ref volume, .. }) if *volume == vec![45, 50]));
        assert_eq!(ui.rows().len(), 6);
    }
}
