//! The status bar, ported from topbar (`~/0Projects/topbar`).
//!
//! It runs on its own thread: data sources (PulseAudio, D-Bus, sysfs,
//! helpers, Spotify) may block or panic without touching the compositor.
//! The compositor sends window-manager snapshots and clicks; the bar sends
//! back finished pixel frames and the actions that belong to the compositor.

mod actions;
mod dbus;
mod draw;
mod event_loop;
mod exec;
mod icons;
mod json;
mod modules;
mod pulse;
mod settings;
mod zones;

use std::io;
use std::os::fd::RawFd;
use std::sync::{mpsc, Arc};

use event_loop::EventFd;

/// One workspace as the compositor sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct WsEntry {
    /// 1-41 for numbered workspaces; negative for named/special ones.
    pub id: i32,
    /// "1".."41", "chat", "special:resize".
    pub name: String,
    pub active: bool,
    pub urgent: bool,
}

/// One toplevel window, for mapping notifications to workspaces.
#[derive(Clone, Debug, PartialEq)]
pub struct Client {
    pub app_id: String,
    pub title: String,
    pub workspace: i32,
    /// 0 for the focused window, growing with how long ago it had focus.
    pub focus_age: i32,
}

pub enum ToBar {
    /// Output width in pixels; the bar renders at this width.
    Width(u32),
    /// Workspaces to show, already sorted.
    Workspaces(Vec<WsEntry>),
    Clients(Vec<Client>),
    /// Left-button release at bar x coordinate.
    Click { x: i32 },
    /// A notification arrived from this app: the bar answers with the
    /// workspace to mark (`FromBar::Notified`).
    Notification { app: String },
    /// Notifications kept in the list.
    NotificationCount(u32),
    /// Screensaver label ("OFF", "1m") and whether the lid switch is inhibited.
    Display { screensaver: String, lid_inhibited: bool },
    /// Volume keys, handled on the bar's PulseAudio connection; the bar
    /// answers with `FromBar::Audio`.
    Audio(AudioKey),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioKey {
    /// Points to add to the output volume (negative lowers it).
    Volume(i32),
    ToggleMute,
    ToggleMic,
}

pub enum FromBar {
    /// A new frame: `pixels` is `width * height` ARGB8888 little endian
    /// (bytes B, G, R, A), stride `width * 4`.
    Frame { width: u32, height: u32, pixels: Vec<u8> },
    /// A workspace entry was clicked.
    Workspace { id: i32, name: String },
    /// Focus the window of this app (case-insensitive app id match).
    FocusApp(String),
    /// A desktop notification came from an app on this workspace.
    Notified { workspace: i32 },
    /// The blk widget: black screen until the next input.
    Blank,
    CycleScreensaver,
    ToggleLid,
    ToggleNotifications,
    /// The network widget: the Wi-Fi list.
    Wifi,
    /// The audio state after a volume key, for the on-screen indicator:
    /// microphone or output, level and mute.
    Audio { mic: bool, level: u32, muted: bool },
}

/// Handle owned by the compositor.
pub struct Bar {
    height: u32,
    /// None only while dropping, so the thread sees the channel close.
    to_bar: Option<mpsc::Sender<ToBar>>,
    wake: Arc<EventFd>,
    from_bar: mpsc::Receiver<FromBar>,
    ready: Arc<EventFd>,
}

impl Bar {
    /// Starts the bar thread. Commands spawned by bar actions inherit the
    /// process environment (the compositor sets `WAYLAND_DISPLAY` first).
    pub fn spawn() -> io::Result<Bar> {
        let font = draw::load_font().map_err(io::Error::other)?;
        let height = settings::bar_height(&font);
        let wake = Arc::new(EventFd::new()?);
        let ready = Arc::new(EventFd::new()?);
        let (to_bar, inbox) = mpsc::channel();
        let (outbox, from_bar) = mpsc::channel();
        let link = event_loop::Link { inbox, wake: wake.clone(), outbox, ready: ready.clone() };
        std::thread::Builder::new()
            .name("bar".into())
            .spawn(move || event_loop::run(link, font, height))?;
        Ok(Bar { height, to_bar: Some(to_bar), wake, from_bar, ready })
    }

    /// Bar height in pixels, from the font metrics.
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn send(&self, msg: ToBar) {
        if let Some(to_bar) = &self.to_bar {
            if to_bar.send(msg).is_ok() {
                self.wake.signal();
            }
        }
    }

    /// Readable (eventfd) when messages from the bar are waiting.
    pub fn raw_fd(&self) -> RawFd {
        self.ready.raw()
    }

    /// Messages from the bar; call when `raw_fd` is readable.
    pub fn recv(&self) -> Vec<FromBar> {
        self.ready.drain();
        self.from_bar.try_iter().collect()
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        self.to_bar = None;
        self.wake.signal();
    }
}

/// Structural failures only, to stderr and from there to journald (ADR-047).
fn log(msg: &str) {
    eprintln!("davide-wm: bar: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits for messages from the bar the way the compositor does: poll the
    /// eventfd, then drain.
    fn wait_for(bar: &Bar, mut want: impl FnMut(&FromBar) -> bool) -> FromBar {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "nothing came from the bar in time");
            let mut p = libc::pollfd { fd: bar.raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, left.as_millis() as i32) };
            if let Some(msg) = bar.recv().into_iter().find(|m| want(m)) {
                return msg;
            }
        }
    }

    /// End to end: a width yields a frame of the advertised size, and a click
    /// on a workspace comes back as that workspace.
    #[test]
    fn frames_and_workspace_clicks_round_trip() {
        let bar = Bar::spawn().expect("spawn");
        let height = bar.height();
        assert!(height > 10, "height {height} from the font metrics");

        bar.send(ToBar::Workspaces(vec![
            WsEntry { id: 1, name: "1".into(), active: true, urgent: false },
            WsEntry { id: 2, name: "2".into(), active: false, urgent: true },
        ]));
        bar.send(ToBar::Width(1200));
        match wait_for(&bar, |m| matches!(m, FromBar::Frame { .. })) {
            FromBar::Frame { width, height: h, pixels } => {
                assert_eq!((width, h), (1200, height));
                assert_eq!(pixels.len(), 1200 * height as usize * 4);
            }
            _ => unreachable!(),
        }

        bar.send(ToBar::Click { x: settings::PAD + 1 });
        match wait_for(&bar, |m| matches!(m, FromBar::Workspace { .. })) {
            FromBar::Workspace { id, name } => assert_eq!((id, name.as_str()), (1, "1")),
            _ => unreachable!(),
        }
    }
}
