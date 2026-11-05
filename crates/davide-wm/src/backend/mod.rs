//! Backends: `drm` drives the hardware through logind, `nested` runs as a
//! window of another compositor.

mod input;
mod kms;
pub mod logind;
pub mod nested;
pub mod outputs;

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
    /// logind asks to lock the session.
    Lock,
    /// About to suspend (true), or resumed.
    Sleep(bool),
}

pub struct Saved {
    kms: kms::Saved,
    logind: logind::Logind,
    card: (u32, u32),
    active: bool,
    failed: Vec<String>,
}

state::persist_struct!(Saved { kms, logind, card, active, failed });

pub struct Drm {
    // Dropped first: libinput releases its devices through logind.
    input: input::Input,
    kms: kms::Kms,
    pub logind: Box<logind::Logind>,
    uevent: OwnedFd,
    card: (u32, u32),
    card_path: String,
    pub active: bool,
    lid_closed: bool,
    /// Blanked: heads that cannot show black frames are powered down.
    blanked: bool,
    /// Connectors whose link failed to train; skipped until unplugged.
    failed: Vec<String>,
    /// An exclusive output was just modeset: (connector name, epoch seconds)
    /// for the kernel log check.
    pub link_check: Option<(String, u64)>,
}

/// What the compositor needs to know about a lit output.
pub struct HeadInfo {
    pub name: String,
    pub model: String,
    pub size: (i32, i32),
    pub refresh_mhz: u32,
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
    let monitors = monitors(&kms, &card)?;
    let mut out = format!("{card}\n{}", kms.describe()?);
    for m in &monitors {
        out += &format!("{}: {:?}\n", m.name, m.model);
    }
    out += &match outputs::layout(&monitors, outputs::lid_closed_now(), &[]) {
        Some(l) => format!("primary {}, mirrors {:?}\n", monitors[l.primary].name, l.mirrors.iter().map(|i| &monitors[*i].name).collect::<Vec<_>>()),
        None => "nothing to drive\n".into(),
    };
    Ok(out)
}

fn monitors(kms: &kms::Kms, card: &str) -> io::Result<Vec<outputs::Monitor>> {
    Ok(kms
        .connectors()?
        .into_iter()
        .filter(|c| c.connected && !c.modes.is_empty())
        .map(|c| outputs::Monitor { connector: c.id, model: outputs::edid_model(&outputs::read_edid(card, &c.name)), name: c.name, modes: c.modes })
        .collect())
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
        let numbers = logind::device_numbers(&card)?;
        let drm = Drm::assemble(input, kms, logind, numbers, card, true, Vec::new())?;
        Ok((drm, renderer))
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(input: input::Input, kms: kms::Kms, logind: Box<logind::Logind>, card: (u32, u32), card_path: String, active: bool, failed: Vec<String>) -> io::Result<Drm> {
        Ok(Drm {
            input,
            kms,
            logind,
            uevent: uevent_socket()?,
            card,
            card_path,
            active,
            lid_closed: outputs::lid_closed_now(),
            blanked: false,
            failed,
            link_check: None,
        })
    }

    /// Releases the input devices and hands the rest over for a live upgrade.
    pub fn save(self) -> Saved {
        drop(self.input);
        Saved { kms: self.kms.save(), logind: *self.logind, card: self.card, active: self.active, failed: self.failed }
    }

    pub fn restore(saved: Saved) -> io::Result<(Drm, Renderer)> {
        let card = find_card()?;
        let mut renderer = Renderer::new(&render_node(&card)?).map_err(io::Error::other)?;
        let kms = kms::Kms::restore(saved.kms, &mut renderer)?;
        let mut logind = Box::new(saved.logind);
        let input = input::Input::new(&mut *logind)?;
        Ok((Drm::assemble(input, kms, logind, saved.card, card, saved.active, saved.failed)?, renderer))
    }

    pub fn fds(&self) -> [RawFd; 4] {
        [self.kms.raw_fd(), self.input.raw_fd(), self.logind.bus.raw_fd(), self.uevent.as_raw_fd()]
    }

    /// Applies the monitor policy to what is connected; reports the primary
    /// output's name and size when they change.
    pub fn apply_layout(&mut self, r: &mut Renderer) -> io::Result<Vec<Event>> {
        let monitors = monitors(&self.kms, &self.card_path)?;
        let connected: Vec<&str> = monitors.iter().map(|m| m.name.as_str()).collect();
        self.failed.retain(|f| connected.contains(&f.as_str()));
        let before = self.kms.outputs().next().map(|o| (o.name.clone(), o.mode.hdisplay, o.mode.vdisplay));
        let Some(layout) = outputs::layout(&monitors, self.lid_closed, &self.failed) else {
            log!("no connected output");
            self.kms.configure(Vec::new(), r)?;
            return Ok(Vec::new());
        };
        let wanted: Vec<_> = std::iter::once(layout.primary)
            .chain(layout.mirrors)
            .filter_map(|i| {
                let m = &monitors[i];
                outputs::pick_mode(m).map(|mode| (m.connector, m.name.clone(), m.model.clone(), mode))
            })
            .collect();
        let assigned = self.kms.assign(wanted)?;
        let was_driven: Vec<(u32, u16, u16)> = self.kms.outputs().map(|o| (o.connector, o.mode.hdisplay, o.mode.vdisplay)).collect();
        self.kms.configure(assigned, r)?;
        if let Some(p) = self.kms.outputs().next() {
            if outputs::is_exclusive(&p.name, &p.model) && !was_driven.contains(&(p.connector, p.mode.hdisplay, p.mode.vdisplay)) {
                self.link_check = Some((p.name.clone(), epoch_now().saturating_sub(5)));
            }
        }
        self.apply_power()?;
        let after = self.kms.outputs().next().map(|o| (o.name.clone(), o.mode.hdisplay, o.mode.vdisplay));
        Ok(match after {
            Some((name, w, h)) if after != before => vec![Event::OutputName(name), Event::Resize(w as i32, h as i32)],
            _ => Vec::new(),
        })
    }

    /// Powers each head for the blank and lid state: the laptop panel goes
    /// down with the lid closed; blanking powers down whatever cannot show
    /// black frames.
    fn apply_power(&mut self) -> io::Result<()> {
        let heads: Vec<(String, String)> = self.kms.outputs().map(|o| (o.name.clone(), o.model.clone())).collect();
        for (i, (name, model)) in heads.iter().enumerate() {
            let lid_off = self.lid_closed && name == config::INTERNAL_OUTPUT;
            let blank_off = self.blanked && !outputs::blanks_black(name, model);
            self.kms.set_power(i, !(lid_off || blank_off))?;
        }
        Ok(())
    }

    pub fn heads(&self) -> Vec<HeadInfo> {
        self.kms
            .outputs()
            .map(|o| HeadInfo {
                name: o.name.clone(),
                model: o.model.clone(),
                size: (o.mode.hdisplay as i32, o.mode.vdisplay as i32),
                refresh_mhz: o.mode.refresh_mhz(),
            })
            .collect()
    }

    /// Colour multipliers for every head (night light); applied with the
    /// next frame.
    pub fn set_gamma(&mut self, g: Option<(f32, f32, f32)>) {
        self.kms.gamma = g;
    }

    pub fn set_blank(&mut self, blanked: bool) -> io::Result<()> {
        self.blanked = blanked;
        self.apply_power()
    }

    pub fn set_lid(&mut self, closed: bool, r: &mut Renderer) -> io::Result<Vec<Event>> {
        if self.lid_closed == closed {
            return Ok(Vec::new());
        }
        self.lid_closed = closed;
        self.apply_layout(r)
    }

    /// The link of `name` did not train: fall back to the other outputs.
    pub fn link_failed(&mut self, name: &str, r: &mut Renderer) -> io::Result<Vec<Event>> {
        log!("{name}: DP link training failed, falling back");
        if !self.failed.iter().any(|f| f == name) {
            self.failed.push(name.to_string());
        }
        self.apply_layout(r)
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
                logind::Event::Lock => out.push(Event::Lock),
                logind::Event::Sleep(s) => out.push(Event::Sleep(s)),
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

    /// Handles kernel uevents; on a DRM hotplug re-applies the layout.
    pub fn dispatch_uevent(&mut self, r: &mut Renderer) -> Option<io::Result<Vec<Event>>> {
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
        (hotplug && self.active).then(|| {
            log!("display hotplug");
            self.apply_layout(r)
        })
    }

    pub fn acquire(&self, head: usize) -> Option<usize> {
        if !self.active {
            return None;
        }
        self.kms.acquire(head)
    }

    pub fn target(&self, head: usize, idx: usize) -> &Target {
        self.kms.target(head, idx)
    }

    pub fn present(&mut self, head: usize, idx: usize) -> io::Result<()> {
        self.kms.present(head, idx)
    }

    pub fn switch_vt(&mut self, vt: u32) {
        self.logind.switch_vt(vt);
    }
}
