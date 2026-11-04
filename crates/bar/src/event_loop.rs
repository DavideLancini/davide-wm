//! The bar thread: one epoll loop over the compositor's messages, the tick and
//! the push sources the widgets hold open.
//!
//! Single-threaded inside, like topbar's loop: slow I/O is bounded by its own
//! timeout rather than moved elsewhere (ADR-052). What changed is that the
//! whole loop now runs beside the compositor, so even a bounded wait never
//! stalls a frame, and a panic restarts the loop instead of the session.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::actions::{self, Target, Widget};
use crate::modules::layout::State;
use crate::zones::Zones;
use crate::{log, settings, Client, FromBar, ToBar, WsEntry};

/// Pause before a panicked loop starts again, so a widget that panics on every
/// pass costs one log line a second rather than a core.
const RESTART_DELAY: Duration = Duration::from_secs(1);

/// A non-blocking eventfd used as a doorbell between the two threads.
pub struct EventFd(OwnedFd);

impl EventFd {
    pub fn new() -> io::Result<EventFd> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(EventFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn signal(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.raw(), (&one as *const u64).cast(), 8) };
    }

    /// Resets the counter. EAGAIN just means nobody rang.
    pub fn drain(&self) {
        let mut count: u64 = 0;
        unsafe { libc::read(self.raw(), (&mut count as *mut u64).cast(), 8) };
    }

    pub fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// Both directions of the channel between the compositor and the bar thread.
pub struct Link {
    pub inbox: Receiver<ToBar>,
    pub wake: Arc<EventFd>,
    pub outbox: Sender<FromBar>,
    pub ready: Arc<EventFd>,
}

impl Link {
    fn send(&self, msg: FromBar) {
        if self.outbox.send(msg).is_ok() {
            self.ready.signal();
        }
    }
}

/// What the compositor told the bar. Kept outside the loop so it survives a
/// restart: a loop that panicked must come back drawing the same workspaces at
/// the same width, not wait for the next change.
#[derive(Default)]
pub struct Snapshot {
    width: Option<u32>,
    workspaces: Vec<WsEntry>,
    clients: Vec<Client>,
}

/// Body of the bar thread.
pub fn run(link: Link, font: fontdue::Font, height: u32) {
    supervise(|snap| Loop::new(&link, &font, height)?.run(snap));
}

/// Runs `body` until it returns, restarting it after a panic or an error.
fn supervise(mut body: impl FnMut(&mut Snapshot) -> io::Result<()>) {
    let mut snap = Snapshot::default();
    loop {
        match panic::catch_unwind(AssertUnwindSafe(|| body(&mut snap))) {
            Ok(Ok(())) => return,
            Ok(Err(e)) => log(&format!("event loop failed: {e}; restarting")),
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown payload".to_string());
                log(&format!("panicked: {msg}; restarting"));
            }
        }
        std::thread::sleep(RESTART_DELAY);
    }
}

/// Fds the loop watches besides the push sources.
const TAG_WAKE: u64 = 0;
const TAG_TICK: u64 = 1;
const TAG_SOURCES: u64 = 2;

/// The widgets' long-lived connections that push changes.
#[derive(Clone, Copy, Debug)]
enum Source {
    Audio,
    Bluetooth,
    Spotify,
}

const SOURCES: [Source; 3] = [Source::Audio, Source::Bluetooth, Source::Spotify];

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Audio => "audio server",
            Source::Bluetooth => "bluetooth bus",
            Source::Spotify => "spotify bus",
        }
    }

    fn fd(self, m: &State) -> Option<RawFd> {
        match self {
            Source::Audio => m.audio.raw_fd(),
            Source::Bluetooth => m.bluetooth.raw_fd(),
            Source::Spotify => m.spotify.raw_fd(),
        }
    }

    /// A dead connection is not a quiet one: its descriptor stays readable for
    /// ever under level-triggered epoll, so it must leave the set instead of
    /// spinning the loop (ADR-049).
    fn closed(self, m: &State) -> bool {
        match self {
            Source::Audio => m.audio.server_closed(),
            Source::Bluetooth => m.bluetooth.bus_closed(),
            Source::Spotify => m.spotify.bus_closed(),
        }
    }
}

/// Identity of an open file: the fd number alone is not enough, since a
/// widget that reconnects can get the same number back for a new socket.
fn inode(fd: RawFd) -> Option<u64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(st.st_ino)
}

struct Loop<'a> {
    link: &'a Link,
    font: &'a fontdue::Font,
    height: u32,
    epoll: OwnedFd,
    timer: OwnedFd,
    zones: Zones,
    /// What each push source is registered with, as (fd, inode).
    watched: [Option<(RawFd, u64)>; SOURCES.len()],
    next_mega: Instant,
    next_spotify: Instant,
    /// Width of the last frame sent; None forces the next one out.
    rendered: Option<u32>,
}

impl<'a> Loop<'a> {
    fn new(link: &'a Link, font: &'a fontdue::Font, height: u32) -> io::Result<Self> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(io::Error::last_os_error());
        }
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };

        let timer = unsafe {
            libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
        };
        if timer < 0 {
            return Err(io::Error::last_os_error());
        }
        let timer = unsafe { OwnedFd::from_raw_fd(timer) };
        let tick = libc::timespec { tv_sec: settings::TICK_INTERVAL_SECS as libc::time_t, tv_nsec: 0 };
        let spec = libc::itimerspec { it_interval: tick, it_value: tick };
        if unsafe { libc::timerfd_settime(timer.as_raw_fd(), 0, &spec, std::ptr::null_mut()) } < 0 {
            return Err(io::Error::last_os_error());
        }

        let this = Loop {
            link,
            font,
            height,
            epoll,
            timer,
            zones: Zones::default(),
            watched: [None; SOURCES.len()],
            next_mega: Instant::now(),
            next_spotify: Instant::now() + crate::modules::spotify::NowPlaying::poll_interval(),
            rendered: None,
        };
        this.ctl(libc::EPOLL_CTL_ADD, link.wake.raw(), TAG_WAKE)?;
        this.ctl(libc::EPOLL_CTL_ADD, this.timer.as_raw_fd(), TAG_TICK)?;
        Ok(this)
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd, tag: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: tag };
        if unsafe { libc::epoll_ctl(self.epoll.as_raw_fd(), op, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn run(&mut self, snap: &mut Snapshot) -> io::Result<()> {
        // MPRIS pushes the whole metadata on every track change.
        self.zones.modules.spotify.connect();
        self.redraw(snap);

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        loop {
            self.sync_sources();
            let n = unsafe {
                libc::epoll_wait(self.epoll.as_raw_fd(), events.as_mut_ptr(), events.len() as i32, -1)
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }

            let mut dirty = false;
            for ev in &events[..n as usize] {
                let tag = ev.u64;
                match tag {
                    TAG_WAKE => self.link.wake.drain(),
                    TAG_TICK => {
                        let mut expirations: u64 = 0;
                        unsafe {
                            libc::read(self.timer.as_raw_fd(), (&mut expirations as *mut u64).cast(), 8)
                        };
                        self.tick();
                        dirty = true;
                    }
                    _ => {
                        let Some(&source) = SOURCES.get((tag - TAG_SOURCES) as usize) else {
                            continue;
                        };
                        dirty |= self.drain(source);
                    }
                }
            }

            // Messages are read on every wake-up, not only the doorbell's: a
            // ring consumed just before a restart must not strand them.
            loop {
                match self.link.inbox.try_recv() {
                    Ok(msg) => dirty |= self.handle(msg, snap),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }

            if dirty {
                self.redraw(snap);
            }
        }
    }

    /// The slower intervals, checked on the shared tick.
    fn tick(&mut self) {
        let now = Instant::now();
        // MEGA sync: its own slow interval, the widest in the bar. Polling it
        // on every tick would run an expensive query far more than needed.
        if now >= self.next_mega {
            self.zones.modules.mega = crate::modules::mega::state();
            self.next_mega = now + Duration::from_secs(crate::modules::mega::INTERVAL_SECS);
        }
        // With Spotify closed there is no MPRIS signal to wake on, so the API
        // side is asked on an interval of its own. It costs nothing while the
        // desktop client is running: MPRIS answers first and the request is
        // never made.
        if now >= self.next_spotify {
            self.zones.modules.spotify.tick();
            self.next_spotify = now + crate::modules::spotify::NowPlaying::poll_interval();
        }
    }

    /// Consumes what a push source has pending; true when the bar may change.
    fn drain(&mut self, source: Source) -> bool {
        let m = &mut self.zones.modules;
        match source {
            Source::Audio => m.audio.drain_events(),
            Source::Bluetooth => m.bluetooth.drain_events(),
            Source::Spotify => m.spotify.drain_events(),
        }
    }

    /// Keeps the epoll set in step with the widgets' connections, which come
    /// and go: the audio and bluetooth widgets reconnect after a server
    /// restart, and a closed bus has to leave the set.
    fn sync_sources(&mut self) {
        let wanted: Vec<Option<(RawFd, u64)>> = SOURCES
            .iter()
            .map(|s| {
                let m = &self.zones.modules;
                let fd = s.fd(m).filter(|_| !s.closed(m))?;
                Some((fd, inode(fd)?))
            })
            .collect();

        // Removals first, so a number freed by one source and taken by another
        // is not deleted after being added.
        for (i, want) in wanted.iter().enumerate() {
            let Some((fd, ino)) = self.watched[i] else { continue };
            if *want == self.watched[i] {
                continue;
            }
            // A closed fd already left the set on its own; deleting by number
            // would hit whatever file took the number since.
            if inode(fd) == Some(ino) {
                let _ = self.ctl(libc::EPOLL_CTL_DEL, fd, 0);
            }
            if want.is_none() && SOURCES[i].closed(&self.zones.modules) {
                log(&format!("{} closed, source removed", SOURCES[i].name()));
            }
            self.watched[i] = None;
        }

        for (i, want) in wanted.into_iter().enumerate() {
            let Some((fd, _)) = want else { continue };
            if self.watched[i] == want {
                continue;
            }
            let tag = TAG_SOURCES + i as u64;
            let added = match self.ctl(libc::EPOLL_CTL_ADD, fd, tag) {
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                    self.ctl(libc::EPOLL_CTL_MOD, fd, tag)
                }
                other => other,
            };
            match added {
                Ok(()) => self.watched[i] = want,
                Err(e) => log(&format!("cannot watch the {}: {e}", SOURCES[i].name())),
            }
        }
    }

    /// Applies one message from the compositor; true when the bar may change.
    fn handle(&mut self, msg: ToBar, snap: &mut Snapshot) -> bool {
        match msg {
            ToBar::Width(width) => {
                snap.width = Some(width);
                true
            }
            ToBar::Workspaces(list) => {
                snap.workspaces = list;
                true
            }
            ToBar::Clients(list) => {
                snap.clients = list;
                false
            }
            ToBar::Click { x } => self.click(x),
            ToBar::Notification { app } => {
                if let Some(workspace) = crate::modules::workspaces::workspace_of_app(&app, &snap.clients) {
                    self.link.send(FromBar::Notified { workspace });
                }
                false
            }
            ToBar::NotificationCount(n) => {
                self.zones.modules.notifications = n.to_string();
                true
            }
            ToBar::Display { screensaver, lid_inhibited } => {
                let m = &mut self.zones.modules;
                m.screensaver = screensaver;
                m.lid = if lid_inhibited { "LID:ON" } else { "LID:OFF" }.into();
                true
            }
        }
    }

    /// A left-button release on the bar: one action per widget (ADR-042).
    fn click(&mut self, x: i32) -> bool {
        let Some(target) = self.zones.clickable.target_at(x).cloned() else {
            return false;
        };
        let widget = match target {
            Target::Workspace { id, name } => {
                self.link.send(FromBar::Workspace { id, name });
                return false;
            }
            Target::Widget(widget) => widget,
        };
        let m = &mut self.zones.modules;
        match widget {
            // The audio toggles go through the PulseAudio connection the bar
            // already holds, and the bar repaints at once rather than waiting
            // for the subscription event to come back.
            Widget::Volume => {
                m.audio.toggle_sink();
            }
            Widget::Microphone => {
                m.audio.toggle_mic();
            }
            Widget::Clock => m.clock_date = !m.clock_date,
            Widget::Spotify => self.link.send(FromBar::FocusApp("spotify".into())),
            // Compositor state: it answers with ToBar::Display.
            Widget::Blk => self.link.send(FromBar::Blank),
            Widget::Screensaver => self.link.send(FromBar::CycleScreensaver),
            Widget::Lid => self.link.send(FromBar::ToggleLid),
            Widget::Notifications => self.link.send(FromBar::ToggleNotifications),
            _ => {
                if let Some(command) = widget.command() {
                    actions::run_detached(command);
                }
            }
        }
        // A click ran a command that likely changed what a widget reports; the
        // helpers cache their output, so drop it first.
        self.zones.modules.invalidate_cached();
        true
    }

    /// Recomputes the bar and sends a frame only if its appearance or the
    /// width changed. Nothing is drawn before the compositor gives a width.
    fn redraw(&mut self, snap: &Snapshot) {
        let Some(width) = snap.width.filter(|w| *w > 0) else {
            return;
        };
        let changed = self.zones.refresh(self.font, width, &snap.workspaces);
        if !changed && self.rendered == Some(width) {
            return;
        }
        let pixels = self.zones.render(self.font, width, self.height);
        self.rendered = Some(width);
        self.link.send(FromBar::Frame { width, height: self.height, pixels });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A panicking loop is restarted rather than taking the thread down, and
    /// what the compositor said survives the restart.
    #[test]
    fn a_panic_restarts_the_loop() {
        let runs = Cell::new(0);
        supervise(|snap| {
            runs.set(runs.get() + 1);
            if runs.get() == 1 {
                snap.width = Some(640);
                panic!("widget exploded");
            }
            assert_eq!(snap.width, Some(640), "the snapshot must survive");
            Ok(())
        });
        assert_eq!(runs.get(), 2);
    }

    /// The doorbell coalesces rings and reads as quiet once drained.
    #[test]
    fn the_eventfd_rings_and_drains() {
        let fd = EventFd::new().unwrap();
        let readable = |fd: &EventFd| {
            let mut p = libc::pollfd { fd: fd.raw(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, 0) == 1 }
        };
        assert!(!readable(&fd));
        fd.signal();
        fd.signal();
        assert!(readable(&fd));
        fd.drain();
        assert!(!readable(&fd));
        fd.drain();
    }
}
