//! Backends: `drm` drives the hardware through logind, `nested` runs as a
//! window of another compositor.

mod input;
mod kms;
pub mod logind;
pub mod nested;

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use render::{Renderer, Target};

pub enum Event {
    Resize(i32, i32),
    OutputName(String),
    Close,
    PointerMotion(f64, f64),
    PointerMotionRelative(f64, f64),
    PointerButton(u32, bool),
    PointerAxis(u32, f64),
    Key(u32, bool),
    KeyboardLeave,
    /// true when the lid closes.
    Lid(bool),
}

pub struct Saved {
    kms: kms::Saved,
    logind: logind::Logind,
    card: (u32, u32),
    active: bool,
}

state::persist_struct!(Saved { kms, logind, card, active });

pub struct Drm {
    // Dropped first: libinput releases its devices through logind.
    input: input::Input,
    kms: kms::Kms,
    pub logind: Box<logind::Logind>,
    uevent: OwnedFd,
    card: (u32, u32),
    pub active: bool,
}

/// The DRM card driving the displays: the boot VGA device, else the first.
pub fn find_card() -> io::Result<String> {
    let mut cards: Vec<String> = std::fs::read_dir("/sys/class/drm")?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("card") && !n.contains('-'))
        .collect();
    cards.sort();
    let boot = cards.iter().find(|c| std::fs::read_to_string(format!("/sys/class/drm/{c}/device/boot_vga")).is_ok_and(|v| v.trim() == "1"));
    boot.or(cards.first()).map(|c| format!("/dev/dri/{c}")).ok_or_else(|| io::Error::other("no DRM card"))
}

/// The render node of a card: GPU memory allocated there is freed when the
/// process exits, which matters for the live upgrade (the KMS fd survives it).
fn render_node(card: &str) -> io::Result<String> {
    let name = card.rsplit('/').next().unwrap_or(card);
    std::fs::read_dir(format!("/sys/class/drm/{name}/device/drm"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("renderD"))
        .map(|n| format!("/dev/dri/{n}"))
        .ok_or_else(|| io::Error::other(format!("no render node for {card}")))
}

fn uevent_socket() -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, libc::NETLINK_KOBJECT_UEVENT);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_groups = 1;
        if libc::bind(fd.as_raw_fd(), (&addr as *const libc::sockaddr_nl).cast(), std::mem::size_of::<libc::sockaddr_nl>() as u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

/// Lists connectors and modes without taking the device (debug helper).
pub fn describe_outputs() -> io::Result<String> {
    let card = find_card()?;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&card)?;
    let kms = kms::Kms::new(OwnedFd::from(file))?;
    let chosen = match kms.choose()? {
        Some(o) => format!("would drive {} with CRTC {} and plane {}\n", o.name, o.crtc, o.plane),
        None => "nothing to drive\n".into(),
    };
    Ok(format!("{card}\n{}{chosen}", kms.describe()?))
}

impl Drm {
    pub fn open() -> io::Result<(Drm, Renderer)> {
        let mut logind = Box::new(logind::Logind::open()?);
        let card = find_card()?;
        log!("using {card}");
        let fd = logind.take_device(&card)?;
        let renderer = Renderer::new(&render_node(&card)?).map_err(io::Error::other)?;
        let kms = kms::Kms::new(fd)?;
        let input = input::Input::new(&mut *logind)?;
        let drm = Drm { input, kms, logind, uevent: uevent_socket()?, card: logind::device_numbers(&card)?, active: true };
        Ok((drm, renderer))
    }

    /// Releases the input devices and hands the rest over for a live upgrade.
    pub fn save(self) -> Saved {
        drop(self.input);
        Saved { kms: self.kms.save(), logind: *self.logind, card: self.card, active: self.active }
    }

    pub fn restore(saved: Saved) -> io::Result<(Drm, Renderer)> {
        let card = find_card()?;
        let mut renderer = Renderer::new(&render_node(&card)?).map_err(io::Error::other)?;
        let kms = kms::Kms::restore(saved.kms, &mut renderer)?;
        let mut logind = Box::new(saved.logind);
        let input = input::Input::new(&mut *logind)?;
        Ok((Drm { input, kms, logind, uevent: uevent_socket()?, card: saved.card, active: saved.active }, renderer))
    }

    pub fn fds(&self) -> [RawFd; 4] {
        [self.kms.raw_fd(), self.input.raw_fd(), self.logind.bus.raw_fd(), self.uevent.as_raw_fd()]
    }

    /// Picks and configures the output; reports its name and size.
    pub fn setup_output(&mut self, r: &mut Renderer) -> io::Result<Vec<Event>> {
        let Some(out) = self.kms.choose()? else {
            log!("no connected output");
            return Ok(Vec::new());
        };
        let same = self.kms.output.as_ref().is_some_and(|o| o.connector == out.connector && o.mode.hdisplay == out.mode.hdisplay && o.mode.vdisplay == out.mode.vdisplay && o.mode.clock == out.mode.clock);
        if same {
            return Ok(Vec::new());
        }
        let (name, w, h) = (out.name.clone(), out.mode.hdisplay as i32, out.mode.vdisplay as i32);
        self.kms.set_output(out, r)?;
        Ok(vec![Event::OutputName(name), Event::Resize(w, h)])
    }

    /// True when a page flip completed.
    pub fn dispatch_kms(&mut self) -> bool {
        self.kms.dispatch()
    }

    pub fn dispatch_input(&mut self) -> Vec<Event> {
        self.input.dispatch()
    }

    pub fn dispatch_logind(&mut self) -> io::Result<Vec<Event>> {
        let mut out = Vec::new();
        for e in self.logind.events()? {
            match e {
                logind::Event::Pause { major, minor, kind } => {
                    if (major, minor) == self.card {
                        log!("session paused");
                        self.active = false;
                        self.input.suspend();
                        out.push(Event::KeyboardLeave);
                    }
                    if kind == "pause" {
                        self.logind.pause_complete(major, minor);
                    }
                }
                logind::Event::Resume { major, minor } => {
                    if (major, minor) == self.card {
                        log!("session resumed");
                        self.active = true;
                        self.kms.request_modeset();
                        self.input.resume();
                    }
                }
            }
        }
        Ok(out)
    }

    /// Handles kernel uevents; rescans outputs on DRM hotplug.
    pub fn dispatch_uevent(&mut self, r: &mut Renderer) -> io::Result<Vec<Event>> {
        let mut hotplug = false;
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe { libc::recv(self.uevent.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                break;
            }
            let msg = &buf[..n as usize];
            let has = |s: &[u8]| msg.split(|b| *b == 0).any(|f| f == s);
            if has(b"SUBSYSTEM=drm") && has(b"HOTPLUG=1") {
                hotplug = true;
            }
        }
        if hotplug && self.active {
            log!("display hotplug");
            return self.setup_output(r);
        }
        Ok(Vec::new())
    }

    pub fn acquire(&self) -> Option<usize> {
        if !self.active {
            return None;
        }
        self.kms.acquire()
    }

    pub fn target(&self, idx: usize) -> &Target {
        self.kms.target(idx)
    }

    pub fn present(&mut self, idx: usize) -> io::Result<()> {
        self.kms.present(idx)
    }

    pub fn switch_vt(&mut self, vt: u32) {
        self.logind.switch_vt(vt);
    }
}
