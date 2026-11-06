macro_rules! log {
    ($($t:tt)*) => { eprintln!("davide-wm: {}", format_args!($($t)*)) };
}

mod agents;
mod backend;
mod brightness;
mod cast;
mod cpu;
mod display;
mod event_loop;
mod ipc;
mod keyboard;
mod lock;
mod mixer;
mod notify;
mod png;
mod portal;
mod services;
mod server;
mod wifi;
mod worker;
mod terminal;
mod xwayland;

use std::fs::{File, OpenOptions};
use std::io;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use config::{mods, Action};
use event_loop::{Epoll, HANGUP, READABLE};
use backend::{nested::Nested, outputs, Drm, Event};
use render::{Renderer, Target};
use server::{clipboard, seat, selection, workspaces, xwm, CastChoice, ClientId, Purpose, State};

const TOKEN_LISTENER: u64 = 1;
const TOKEN_HOST: u64 = 2;
const TOKEN_SIGNAL: u64 = 3;
const TOKEN_IPC: u64 = 4;
const TOKEN_WORKER: u64 = 5;
const TOKEN_KMS: u64 = 6;
const TOKEN_INPUT: u64 = 7;
const TOKEN_LOGIND: u64 = 8;
const TOKEN_UEVENT: u64 = 9;
const TOKEN_BAR: u64 = 10;
const TOKEN_IDLE: u64 = 11;
const TOKEN_NOTIFY_BUS: u64 = 12;
const TOKEN_NOTIFY_TIMER: u64 = 13;
const TOKEN_XWAYLAND_READY: u64 = 14;
const TOKEN_XWAYLAND_EXIT: u64 = 15;
const TOKEN_XWM: u64 = 16;
const TOKEN_PIPEWIRE: u64 = 17;
const TOKEN_PORTAL: u64 = 18;
const TOKEN_OSD: u64 = 19;
const TOKEN_AGENTS: u64 = 20;
const TOKEN_BATTERY: u64 = 21;
const TOKEN_XWAYLAND_LISTEN: u64 = 22;
const TOKEN_TERM: u64 = 23;
const TOKEN_TERM_TIMER: u64 = 24;
/// Plus the terminal window's surface id.
const TOKEN_PTY: u64 = 1 << 40;
/// Plus the pipe's fd.
const TOKEN_PASTE: u64 = 1 << 41;
const TOKEN_CLIENT: u64 = 1 << 32;

static WALLPAPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wallpaper.jpg"));

/// The listening socket and its lock file, removed on drop.
struct Socket {
    listener: UnixListener,
    name: String,
    path: PathBuf,
    lock_path: PathBuf,
    _lock: File,
}

impl Socket {
    fn bind() -> io::Result<Socket> {
        let dir = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR not set"))?);
        for n in 1..32 {
            let name = format!("wayland-{n}");
            let path = dir.join(&name);
            let lock_path = dir.join(format!("{name}.lock"));
            let lock = OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                continue;
            }
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            listener.set_nonblocking(true)?;
            return Ok(Socket { listener, name, path, lock_path, _lock: lock });
        }
        Err(io::Error::other("no free wayland socket name"))
    }
}

impl state::Persist for Socket {
    fn save(&self, e: &mut state::Enc) {
        state::save_fd(&self.listener, e);
        state::save_fd(&self._lock, e);
        (self.name.clone(), self.path.to_string_lossy().into_owned(), self.lock_path.to_string_lossy().into_owned()).save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let listener = UnixListener::from(OwnedFd::load(d)?);
        let lock = File::from(OwnedFd::load(d)?);
        let (name, path, lock_path): (String, String, String) = state::Persist::load(d)?;
        Ok(Socket { listener, name, path: path.into(), lock_path: lock_path.into(), _lock: lock })
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

enum Backend {
    Nested(Nested),
    Drm(Drm),
}

impl Backend {
    fn acquire(&mut self, head: usize, r: &mut Renderer) -> io::Result<Option<usize>> {
        match self {
            Backend::Nested(n) if head == 0 => n.acquire(r),
            Backend::Nested(_) => Ok(None),
            Backend::Drm(d) => Ok(d.acquire(head)),
        }
    }

    fn target(&self, head: usize, idx: usize) -> &Target {
        match self {
            Backend::Nested(n) => n.target(idx),
            Backend::Drm(d) => d.target(head, idx),
        }
    }

    fn present(&mut self, head: usize, idx: usize) -> io::Result<()> {
        match self {
            Backend::Nested(n) => {
                n.present(idx);
                Ok(())
            }
            Backend::Drm(d) => d.present(head, idx),
        }
    }

    fn heads(&self) -> Vec<backend::HeadInfo> {
        match self {
            Backend::Nested(n) => vec![backend::HeadInfo { name: "NESTED-1".into(), model: String::new(), size: n.size, refresh_mhz: 0 }],
            Backend::Drm(d) => d.heads(),
        }
    }
}

struct App {
    backend: Backend,
    /// Started once the environment for spawned programs is set.
    bar: Option<bar::Bar>,
    bar_width: u32,
    state: State,
    socket: Socket,
    ipc: ipc::Ipc,
    binds: Vec<config::Bind>,
    main_mod: u32,
    /// Keys that triggered a binding: their release is not forwarded.
    swallowed: Vec<u32>,
    /// The main modifier key went down with nothing else: its release
    /// toggles the launcher (tap-Super).
    tap: Option<u32>,
    running: bool,
    upgrade: bool,
    /// Exports the environment to systemd (`--session`).
    session: bool,
    display: display::Display,
    /// Screensaver label and lid inhibitor state last sent to the bar.
    bar_display: Option<(String, bool)>,
    /// The primary buffer last presented, copied to the mirrors.
    last_frame: Option<usize>,
    /// Mirror heads still showing an older frame than `last_frame`.
    mirrors_stale: Vec<usize>,
    notifier: notify::Notifier,
    cards: Option<launcher::card::Cards>,
    /// Notification count last sent to the bar.
    bar_notifications: Option<usize>,
    /// The clipboard history changed and is not saved yet.
    clipboard_dirty: bool,
    xwayland: Option<xwayland::Xwayland>,
    casts: cast::Casts,
    /// PipeWire's fd is in the epoll (it is opened with the first cast).
    pipewire_watched: bool,
    portal: portal::Portal,
    /// A screenshot to take once the renderer is at hand: the portal
    /// request (0 for the key binding) and whether to let the user select.
    shot_request: Option<(u32, bool)>,
    /// Selected screenshots waiting for the worker: request, RGBA, size.
    shots: Vec<(u32, Vec<u8>, u32, u32)>,
    /// Hides the volume/brightness indicator.
    osd_timer: OwnedFd,
    /// A brightness to mirror to external monitors (slow: on the worker).
    ddc: Option<u32>,
    /// A password to check for the lock screen.
    unlock: Option<String>,
    /// Delays suspend until the lock screen is on screen.
    sleep_inhibitor: Option<OwnedFd>,
    /// Release `sleep_inhibitor` after the next frame.
    sleep_after_frame: bool,
    /// polkit and NetworkManager password agents (on DRM, or nested with
    /// `DAVIDE_WM_AGENTS=1`).
    agents: Option<agents::Agents>,
    /// A polkit password to check: user, cookie, password.
    polkit_job: Option<(String, String, String)>,
    /// Background Wi-Fi work: true lists networks; a network connects.
    wifi_job: Option<Option<wifi::Network>>,
    /// Switch to the next CPU level (on the worker: it runs sudo).
    cpu_job: bool,
    /// Blocking jobs for the worker (D-Bus calls of the small services).
    jobs: Vec<Job>,
    /// Ticks every minute for the battery watch.
    battery_timer: OwnedFd,
    battery: services::BatteryWatch,
    /// The charger as last seen by the automatic CPU level.
    ac: Option<bool>,
    night: display::Night,
    /// The multipliers last handed to the backend.
    night_sent: Option<Option<(f32, f32, f32)>>,
    /// Partial redraws (`davide-wm msg damage off` draws whole frames).
    damage_tracking: bool,
    /// Frames drawn on the primary head, and the one each buffer last got.
    frame_no: u64,
    buffer_frame: std::collections::HashMap<usize, u64>,
    /// Recent frames and what they redrew (None: everything).
    damage_log: std::collections::VecDeque<(u64, Option<wm::Rect>)>,
    /// Hardware cursor (`davide-wm msg cursor sw` draws it in the scene).
    hw_cursor_enabled: bool,
    /// The image on the cursor plane, and whether it fit.
    /// Image on the cursor plane, whether it fit, and the plane's buffer.
    cursor_image: Option<(server::CursorImage, bool, Option<u32>)>,
    /// The cursor plane must move without a frame.
    cursor_moved: bool,
    /// The audio mixer while it is open; it has the keyboard.
    mixer: Option<mixer::MixerUi>,
    /// Terminal windows asked for: working directory.
    term_open: Vec<terminal::Open>,
    /// Terminal fds in the epoll set, with their tokens.
    term_watched: HashMap<RawFd, u64>,
}

type Job = Box<dyn FnOnce() -> Result<worker::Done, String> + Send>;

impl App {
    fn spawn(&self, line: &str) {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(line)
            .env("WAYLAND_DISPLAY", &self.socket.name)
            .env(ipc::ENV, &self.ipc.path)
            .env("XDG_CURRENT_DESKTOP", "davide-wm")
            .env_remove("WAYLAND_SOCKET")
            .stdin(Stdio::null());
        match &self.xwayland {
            Some(x) => cmd.env("DISPLAY", format!(":{}", x.display)),
            None => cmd.env_remove("DISPLAY"),
        };
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                event_loop::reset_signals_in_child();
                // Nothing of ours leaks into programs, whatever forgot CLOEXEC.
                libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32);
                Ok(())
            });
        }
        if let Err(e) = cmd.spawn() {
            log!("cannot run {line:?}: {e}");
        }
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        let changed = self.state.keyboard.update(keycode, pressed);
        if self.state.lock.is_some() || self.state.prompt.is_some() {
            self.tap = None;
            self.prompt_key(keycode, pressed);
            return;
        }
        if self.mixer.is_some() {
            self.tap = None;
            if pressed {
                let (sym, text) = (self.state.keyboard.sym(keycode), self.state.keyboard.text(keycode));
                let out = self.mixer.as_mut().unwrap().key(sym, text);
                self.mixer_outcome(out);
            } else {
                self.swallowed.retain(|k| *k != keycode);
            }
            return;
        }
        let base = self.state.keyboard.base_sym(keycode);
        let main_keys: &[u32] = if self.main_mod == mods::SUPER { &[0xffeb, 0xffec] } else { &[0xffe9] };
        let is_main = base.is_some_and(|b| main_keys.contains(&b));
        if pressed {
            self.tap = (is_main && self.state.keyboard.pressed().len() == 1).then_some(keycode);
        } else if self.tap.take() == Some(keycode) {
            if self.state.launcher.is_some() {
                self.state.close_launcher();
            } else {
                self.state.open_launcher();
            }
            seat::key(&mut self.state, keycode, pressed, changed);
            return;
        }
        if self.state.launcher.is_some() {
            if pressed {
                let sym = self.state.keyboard.sym(keycode);
                let text = self.state.keyboard.text(keycode);
                let view = self.state.launcher.as_mut().unwrap();
                match view.launcher.key(sym, text) {
                    launcher::Outcome::Nothing => {}
                    launcher::Outcome::Redraw => {
                        view.dirty = true;
                        self.state.needs_redraw = true;
                    }
                    launcher::Outcome::Close => self.state.close_launcher(),
                    launcher::Outcome::Launch(cmd) => {
                        self.state.close_launcher();
                        self.spawn(&cmd);
                    }
                    launcher::Outcome::Terminal { cmd, hold } => {
                        self.state.close_launcher();
                        self.term_open.push(terminal::Open { cwd: None, cmd: Some(cmd), hold });
                    }
                    launcher::Outcome::Copy(text) => {
                        self.state.close_launcher();
                        selection::set_server(&mut self.state, clipboard::Clip { mime: "text/plain;charset=utf-8".into(), data: text.into_bytes() });
                        self.clipboard_dirty = true;
                        self.show_osd("Copiato negli appunti", None);
                    }
                    launcher::Outcome::Pick(i) => {
                        let purpose = view.purpose.clone();
                        self.state.close_launcher();
                        self.picked(purpose, i);
                    }
                    launcher::Outcome::Delete(i) => {
                        view.dirty = true;
                        self.state.needs_redraw = true;
                        let purpose = view.purpose.clone();
                        self.deleted(purpose, i);
                    }
                }
            }
            return;
        }
        if pressed {
            let active = self.state.keyboard.bind_mods();
            let sym = self.state.keyboard.base_sym(keycode);
            let main = self.main_mod;
            let resolve = |m: u32| if m & mods::MAIN != 0 { (m & !mods::MAIN) | main } else { m };
            if let Some(action) = self.binds.iter().find(|b| resolve(b.mods) == active && Some(b.key) == sym).map(|b| b.action) {
                self.swallowed.push(keycode);
                self.action(action);
                return;
            }
        } else if let Some(i) = self.swallowed.iter().position(|k| *k == keycode) {
            self.swallowed.remove(i);
            return;
        }
        if let Some(f) = self.state.focus.filter(|f| self.state.terms.as_ref().is_some_and(|t| t.is_ours(*f))) {
            self.with_terms(|t, s| t.key(s, f.id, keycode, pressed));
            return;
        }
        seat::key(&mut self.state, keycode, pressed, changed);
    }

    fn action(&mut self, action: Action) {
        let s = &mut self.state;
        match action {
            Action::Spawn(line) => self.spawn(line),
            Action::CloseWindow => {
                if let Some(f) = s.focus {
                    s.close_window(f);
                }
            }
            Action::Focus(dir) => workspaces::focus_dir(s, dir),
            Action::MoveWindow(dir) => workspaces::move_dir(s, dir),
            Action::Workspace(ws) => workspaces::switch(s, ws),
            Action::MoveToWorkspace(ws) => workspaces::move_to(s, ws),
            Action::ToggleSpecial => workspaces::toggle_special(s),
            Action::ToggleMaximize => workspaces::toggle_maximize(s),
            Action::ToggleBar => {
                s.bar.visible = !s.bar.visible;
                s.relayout();
                self.sync_notices();
            }
            Action::SwitchVt(vt) => {
                if let Backend::Drm(d) = &mut self.backend {
                    d.switch_vt(vt);
                }
            }
            Action::Upgrade => self.upgrade = true,
            Action::ClipboardHistory => self.open_clipboard(),
            Action::Notifications => self.open_notifications(),
            Action::Screenshot => self.shot_request = Some((0, true)),
            Action::Volume(d) => self.audio_key(bar::AudioKey::Volume(d)),
            Action::ToggleMute => self.audio_key(bar::AudioKey::ToggleMute),
            Action::ToggleMic => self.audio_key(bar::AudioKey::ToggleMic),
            Action::Brightness(d) => self.change_brightness(d),
            Action::Lock => self.lock(),
            Action::Wifi => self.wifi_job = Some(None),
            Action::PowerMenu => {
                let labels = ["Blocca", "Sospendi", "Riavvia", "Spegni", "Esci da davide-wm"].map(String::from).to_vec();
                self.state.open_view(launcher::Launcher::picker(labels), Purpose::Power);
            }
            Action::Media(key) => self.jobs.push(Box::new(move || services::media(key).map(|_| worker::Done::Saved))),
            Action::AudioOutputs => self.open_mixer(true),
            Action::Terminal => {
                let cwd = self.state.focus.and_then(|f| self.state.terms.as_ref().filter(|t| t.is_ours(f)).and_then(|t| t.cwd_of(f.id)));
                self.term_open.push(terminal::Open { cwd, ..Default::default() });
            }
            Action::Exit => self.running = false,
        }
    }

    fn open_clipboard(&mut self) {
        let entries = &self.state.selection.history.entries;
        let labels = entries.iter().map(clipboard::label).collect();
        let hashes = entries.iter().map(|e| e.hash).collect();
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Clipboard(hashes));
    }

    fn open_notifications(&mut self) {
        if self.state.launcher.as_ref().is_some_and(|l| matches!(l.purpose, Purpose::Notifications(_))) {
            self.state.close_launcher();
            return;
        }
        let list: Vec<&notify::Notification> = self.notifier.list.iter().rev().collect();
        let labels = std::iter::once(format!("Clear all ({})", list.len())).chain(list.iter().map(|n| n.label())).collect();
        let ids = list.iter().map(|n| n.id).collect();
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Notifications(ids));
    }

    fn clip_index(&self, hashes: &[u64], i: usize) -> Option<usize> {
        let h = hashes.get(i)?;
        self.state.selection.history.entries.iter().position(|e| e.hash == *h)
    }

    /// Enter in a picker.
    fn picked(&mut self, purpose: Purpose, i: usize) {
        self.state.abandoned.retain(|p| *p != purpose);
        match purpose {
            Purpose::Apps => {}
            Purpose::Battery => match i {
                2 => {
                    let on = services::battery_info().and_then(|b| b.limit).is_some_and(|l| l < 100);
                    self.jobs.push(Box::new(move || services::set_charge_limit(!on).map(worker::Done::Osd)));
                }
                3 => {
                    set_cpu_auto(true);
                    self.ac = None;
                    self.cpu_auto();
                }
                n @ 4..=8 => {
                    set_cpu_auto(false);
                    let level = n - 3;
                    self.jobs.push(Box::new(move || Ok(worker::Done::Cpu(cpu::set(level)))));
                }
                _ => {}
            },
            Purpose::Power => match i {
                0 => self.lock(),
                1 => self.jobs.push(Box::new(|| services::power(services::Power::Suspend).map(|_| worker::Done::Saved))),
                2 => self.jobs.push(Box::new(|| services::power(services::Power::Reboot).map(|_| worker::Done::Saved))),
                3 => self.jobs.push(Box::new(|| services::power(services::Power::Shutdown).map(|_| worker::Done::Saved))),
                4 => self.running = false,
                _ => {}
            },
            Purpose::Outputs(names) => {
                if let (Some(n), Some(bar)) = (names.get(i), &self.bar) {
                    bar.send(bar::ToBar::SetOutput(n.clone()));
                }
            }
            Purpose::Bluetooth(powered, devices) => match (i, powered) {
                (0, p) => self.jobs.push(Box::new(move || services::bt_power(!p).map(worker::Done::Osd))),
                (i, _) => {
                    if let Some(d) = devices.get(i - 1).cloned() {
                        self.show_osd(&format!("{}…", d.name), None);
                        self.jobs.push(Box::new(move || services::bt_toggle(&d).map(worker::Done::Osd)));
                    }
                }
            },
            Purpose::Timer(secs) => {
                if let (Some(s), Some(bar)) = (secs.get(i), &self.bar) {
                    bar.send(bar::ToBar::Timer((*s > 0).then_some(*s)));
                }
            }
            Purpose::Wifi(nets) => {
                if let Some(n) = nets.get(i) {
                    self.show_osd(&format!("Wi-Fi: connessione a {}…", n.ssid), None);
                    self.wifi_job = Some(Some(n.clone()));
                }
            }
            Purpose::Cast(request, choices) => match choices.get(i) {
                Some(c) => {
                    self.start_cast(request, *c);
                }
                None => self.portal.cancelled(request, false),
            },
            Purpose::Clipboard(hashes) => {
                if let Some(clip) = self.clip_index(&hashes, i).and_then(|j| self.state.selection.history.get(j)) {
                    selection::set_server(&mut self.state, clip);
                    self.save_clipboard();
                }
            }
            Purpose::Notifications(ids) => match i.checked_sub(1).and_then(|j| ids.get(j)) {
                None => self.notifier.clear(),
                Some(id) => {
                    if let Some((app, entry)) = self.notifier.invoke(*id) {
                        self.focus_notifier(&app, &entry);
                    }
                }
            },
        }
        self.sync_notices();
    }

    /// Delete in a picker.
    fn deleted(&mut self, purpose: Purpose, i: usize) {
        match purpose {
            Purpose::Apps | Purpose::Cast(..) | Purpose::Wifi(_) | Purpose::Timer(_) | Purpose::Power | Purpose::Battery | Purpose::Outputs(_) | Purpose::Bluetooth(..) => {}
            Purpose::Clipboard(hashes) => {
                if let Some(j) = self.clip_index(&hashes, i) {
                    self.state.selection.history.remove(j);
                    self.save_clipboard();
                }
            }
            Purpose::Notifications(ids) => match i.checked_sub(1).and_then(|j| ids.get(j)) {
                None => {
                    self.notifier.clear();
                    self.state.close_launcher();
                }
                Some(id) => self.notifier.dismiss(*id),
            },
        }
        self.sync_notices();
    }

    fn open_mixer(&mut self, output: bool) {
        self.state.close_launcher();
        self.mixer = Some(mixer::MixerUi::new(output));
        seat::set_focus(&mut self.state, None);
        if let Some(bar) = &self.bar {
            bar.send(bar::ToBar::Mixer(None));
        }
        self.draw_mixer();
    }

    fn close_mixer(&mut self) {
        self.mixer = None;
        self.state.set_panel(launcher::card::MIXER_WIDTH, None);
        if let Some(bar) = &self.bar {
            bar.send(bar::ToBar::MixerClosed);
        }
        workspaces::refocus(&mut self.state);
    }

    fn mixer_outcome(&mut self, out: mixer::Outcome) {
        match out {
            mixer::Outcome::Nothing => {}
            mixer::Outcome::Redraw => self.draw_mixer(),
            mixer::Outcome::Close => self.close_mixer(),
            mixer::Outcome::Action(a) => {
                if let Some(bar) = &self.bar {
                    bar.send(bar::ToBar::Mixer(Some(a)));
                }
            }
        }
    }

    fn draw_mixer(&mut self) {
        let Some(ui) = &self.mixer else { return };
        if self.cards.is_none() {
            self.cards = launcher::card::Cards::new().inspect_err(|e| log!("{e}")).ok();
        }
        let Some(cards) = &self.cards else { return };
        let mut rows = ui.rows();
        if ui.data.is_none() {
            rows.push(launcher::card::MixRow { title: "Lettura dell'audio…".into(), detail: String::new(), volume: None, header: true, marked: false });
        }
        let card = cards.mixer(["Uscita", "Ingresso"], if ui.output { 0 } else { 1 }, &rows, ui.selected(), ui.scroll(), mixer::HELP);
        self.state.set_panel(launcher::card::MIXER_WIDTH, Some(card));
    }

    /// Pointer input while the mixer is open: the wheel changes the volume,
    /// a click picks a row, a click outside closes it.
    fn mixer_pointer(&mut self, e: &Event) -> bool {
        let Some(rect) = self.state.panel.as_ref().map(|p| p.rect) else { return false };
        let (x, y) = (self.state.pointer.x, self.state.pointer.y);
        match *e {
            Event::PointerAxis(0, v) if rect.contains(x, y) => {
                if let Some(a) = self.mixer.as_ref().and_then(|ui| ui.wheel(v < 0.0)) {
                    self.mixer_outcome(mixer::Outcome::Action(a));
                }
                true
            }
            Event::PointerButton(_, true) if !rect.contains(x, y) => {
                self.close_mixer();
                true
            }
            Event::PointerButton(_, true) => {
                let top = rect.y + launcher::card::MIXER_TOP as i32;
                let n = (y as i32 - top).div_euclid(launcher::card::MIXER_ROW as i32);
                if n >= 0 && (n as usize) < launcher::card::MIXER_ROWS {
                    let out = self.mixer.as_mut().unwrap().click(n as usize);
                    self.mixer_outcome(out);
                }
                true
            }
            Event::PointerButton(..) | Event::PointerAxis(..) => true,
            _ => false,
        }
    }

    /// Suspend waits for the lock screen to be drawn (DRM only).
    fn take_sleep_inhibitor(&mut self) {
        if matches!(self.backend, Backend::Drm(_)) && self.sleep_inhibitor.is_none() {
            match backend::logind::inhibit_mode("sleep", "Lock the screen first", "delay") {
                Ok(fd) => self.sleep_inhibitor = Some(fd),
                Err(e) => log!("sleep inhibitor: {e}"),
            }
        }
    }

    /// Locks the session: nothing shows or gets input until the password.
    fn lock(&mut self) {
        if self.state.lock.is_some() {
            return;
        }
        log!("locked");
        self.state.close_launcher();
        self.state.close_shot();
        seat::set_focus(&mut self.state, None);
        self.state.lock = Some(server::PromptView::new("Schermo bloccato", &format!("Password di {}", lock::user())));
        self.state.pointer_dirty = true;
        self.state.needs_redraw = true;
    }

    /// Keys while the lock screen or a prompt has the keyboard. VT switches
    /// still work.
    fn prompt_key(&mut self, keycode: u32, pressed: bool) {
        if !pressed {
            self.swallowed.retain(|k| *k != keycode);
            return;
        }
        let active = self.state.keyboard.bind_mods();
        let sym = self.state.keyboard.base_sym(keycode);
        if let Some(Action::SwitchVt(vt)) = self.binds.iter().find(|b| b.mods == active && Some(b.key) == sym).map(|b| b.action) {
            if let Backend::Drm(d) = &mut self.backend {
                d.switch_vt(vt);
            }
            return;
        }
        let sym = self.state.keyboard.sym(keycode);
        let text = self.state.keyboard.text(keycode);
        let locked = self.state.lock.is_some();
        let view = if locked { self.state.lock.as_mut() } else { self.state.prompt.as_mut() };
        let Some(view) = view else { return };
        if !locked && sym == 0xff1b {
            self.prompt_done(None);
            return;
        }
        if view.key(sym, text) {
            let password = view.take();
            match locked {
                true => self.unlock = Some(password),
                false => self.prompt_done(Some(password)),
            }
        }
        self.state.needs_redraw = true;
    }

    /// The prompt over the desktop was answered (None: cancelled).
    fn prompt_done(&mut self, password: Option<String>) {
        let Some(agents) = &mut self.agents else {
            self.state.drop_prompt(false);
            workspaces::refocus(&mut self.state);
            return;
        };
        let actions = match password {
            Some(p) => {
                let (actions, job) = agents.answered(p);
                self.polkit_job = job;
                actions
            }
            None => agents.dismissed(),
        };
        self.agent_actions(actions);
    }

    fn agent_actions(&mut self, actions: Vec<agents::Action>) {
        for a in actions {
            match a {
                agents::Action::Prompt => {
                    let Some((title, message)) = self.agents.as_ref().and_then(|g| g.queue.front()).map(|r| r.texts()) else { continue };
                    self.state.close_launcher();
                    seat::set_focus(&mut self.state, None);
                    self.state.drop_prompt(false);
                    self.state.prompt = Some(server::PromptView::new(&title, &message));
                    self.state.pointer_dirty = true;
                }
                agents::Action::Close => {
                    self.state.drop_prompt(false);
                    workspaces::refocus(&mut self.state);
                }
            }
        }
    }

    fn polkit_checked(&mut self, ok: bool) {
        let Some(agents) = &mut self.agents else { return };
        let (actions, closed) = agents.polkit_result(ok);
        if !closed {
            if let Some(p) = &mut self.state.prompt {
                p.failed("Password errata");
            }
            self.state.needs_redraw = true;
        }
        self.agent_actions(actions);
    }

    fn open_bluetooth(&mut self, powered: bool, devices: Vec<services::BtDevice>) {
        let mut labels = vec![if powered { "Spegni il Bluetooth".to_string() } else { "Accendi il Bluetooth".to_string() }];
        labels.extend(devices.iter().map(|d| if d.connected { format!("{} · connesso", d.name) } else { d.name.clone() }));
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Bluetooth(powered, devices));
    }

    fn set_dnd(&mut self, on: bool) {
        notify::set_dnd(on);
        if let Some(bar) = &self.bar {
            bar.send(bar::ToBar::Dnd(on));
        }
        self.show_osd(if on { "Non disturbare: attivo" } else { "Non disturbare: spento" }, None);
    }

    /// The month under the clock; a second click hides it.
    fn toggle_calendar(&mut self) {
        if self.state.calendar.is_some() {
            self.state.set_calendar(None);
            return;
        }
        if self.cards.is_none() {
            self.cards = launcher::card::Cards::new().inspect_err(|e| log!("{e}")).ok();
        }
        let Some(cards) = &self.cards else { return };
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&now, &mut tm) };
        let (year, month, day) = (tm.tm_year + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32);
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1];
        // Weekday of the 1st, Monday first.
        let first = ((tm.tm_wday + 6 - (day as i32 - 1) % 7) % 7 + 7) as u32 % 7;
        self.state.set_calendar(Some(cards.calendar(year, month, days, first, Some(day))));
    }

    /// Applies the night light for the current hour when it changed.
    fn night_light(&mut self) {
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&now, &mut tm) };
        let g = self.night.gamma(tm.tm_hour as u32);
        if self.night_sent == Some(g) {
            return;
        }
        self.night_sent = Some(g);
        if let Backend::Drm(d) = &mut self.backend {
            d.set_gamma(g);
            self.state.needs_redraw = true;
        }
    }

    /// The battery menu: state, then the charge limit and the CPU level.
    fn open_battery(&mut self) {
        let Some(b) = services::battery_info() else {
            self.show_osd("Nessuna batteria", None);
            return;
        };
        let status = match b.status.as_str() {
            "Charging" => "in carica",
            "Discharging" => "in scarica",
            "Full" => "carica",
            "Not charging" => "ferma",
            other => other,
        };
        let time = b.hours.map(|h| format!(" · {}h{:02} {}", h as u32, (h.fract() * 60.0) as u32, if b.status == "Charging" { "alla carica" } else { "rimanenti" })).unwrap_or_default();
        let mut labels = vec![
            format!("Batteria {}% · {status} · {:.1} W{time}", b.level, b.watts),
            format!(
                "Salute {} · {} cicli",
                b.health.map(|h| format!("{h:.0}%")).unwrap_or("?".into()),
                b.cycles.map(|c| c.to_string()).unwrap_or("?".into())
            ),
            match b.limit {
                Some(l) if l < 100 => format!("Limite di carica {l}%: attivo (Invio per caricare al 100%)"),
                Some(_) => format!("Limite di carica {}%: spento (Invio per attivarlo)", config::CHARGE_LIMIT.1),
                None => "Limite di carica: non disponibile".into(),
            },
        ];
        let (bat, ac) = config::CPU_AUTO_LEVELS;
        let auto = cpu_auto();
        labels.push(format!("CPU automatica (livello {bat} a batteria, {ac} in carica){}", if auto { " · attiva" } else { "" }));
        let current = cpu::current();
        for level in 1..=5 {
            let mark = if !auto && current == Some(level) { " · attuale" } else { "" };
            labels.push(format!("CPU {}{mark}", cpu::label(level)));
        }
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Battery);
    }

    /// Automatic CPU level: applied when the charger comes or goes.
    fn cpu_auto(&mut self) {
        if !cpu_auto() {
            return;
        }
        let ac = services::on_ac();
        if self.ac == Some(ac) {
            return;
        }
        self.ac = Some(ac);
        let (bat, plugged) = config::CPU_AUTO_LEVELS;
        let level = if ac { plugged } else { bat };
        self.jobs.push(Box::new(move || Ok(worker::Done::Cpu(cpu::set(level)))));
    }

    /// The battery watch's minute tick.
    fn battery_tick(&mut self) {
        self.night_light();
        self.cpu_auto();
        let Some((level, discharging)) = services::battery() else { return };
        match self.battery.check(level, discharging) {
            services::BatteryAlert::None => return,
            services::BatteryAlert::Low(l) => self.notifier.local("Batteria", &format!("{l}%: collega l'alimentatore"), "", false),
            services::BatteryAlert::Critical(l) => self.notifier.local("Batteria", &format!("{l}%: tra poco il portatile si sospende"), "", true),
            services::BatteryAlert::Suspend => {
                log!("battery at {level}%: suspending");
                self.jobs.push(Box::new(|| services::power(services::Power::Suspend).map(|_| worker::Done::Saved)));
            }
        }
        self.sync_notices();
    }

    fn open_wifi(&mut self, nets: Vec<wifi::Network>) {
        if nets.is_empty() {
            self.show_osd("Wi-Fi: nessuna rete", None);
            return;
        }
        let labels = nets.iter().map(wifi::Network::label).collect();
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Wifi(nets));
    }

    fn unlocked(&mut self, ok: bool) {
        match ok {
            true => {
                log!("unlocked");
                self.state.drop_prompt(true);
                workspaces::refocus(&mut self.state);
                self.state.relayout();
            }
            false => {
                if let Some(l) = &mut self.state.lock {
                    l.failed("Password errata");
                }
                self.state.needs_redraw = true;
            }
        }
    }

    /// Draws the cards of the lock screen and of the prompt when they changed.
    fn prompt_cards(&mut self) {
        let dirty = [&self.state.lock, &self.state.prompt].iter().any(|p| p.as_ref().is_some_and(|p| p.dirty));
        if !dirty {
            return;
        }
        if self.cards.is_none() {
            self.cards = launcher::card::Cards::new().inspect_err(|e| log!("{e}")).ok();
        }
        let Some(cards) = &self.cards else { return };
        for p in [&mut self.state.lock, &mut self.state.prompt].into_iter().flatten() {
            if !p.dirty {
                continue;
            }
            p.dirty = false;
            if p.title.is_empty() {
                // Restored by a live upgrade: only the state came along.
                p.title = "Schermo bloccato".into();
                p.message = format!("Password di {}", lock::user());
            }
            let status = if p.busy { "Verifica…" } else { p.error.as_str() };
            p.card = Some(cards.prompt(&p.title, &p.message, p.typed.chars().count(), status));
        }
    }

    fn audio_key(&mut self, key: bar::AudioKey) {
        if let Some(bar) = &self.bar {
            bar.send(bar::ToBar::Audio(key));
        }
    }

    fn change_brightness(&mut self, delta: i32) {
        let Some(cur) = brightness::get() else { return };
        let Some(pct) = brightness::set_panel((cur as i32 + delta).clamp(0, 100) as u32) else { return };
        self.ddc = Some(pct);
        self.show_osd(&format!("Luminosità {pct}%"), Some(pct as f32 / 100.0));
    }

    /// The volume/brightness indicator, for `OSD_TIMEOUT_MS`.
    fn show_osd(&mut self, label: &str, fraction: Option<f32>) {
        if self.cards.is_none() {
            self.cards = launcher::card::Cards::new().inspect_err(|e| log!("{e}")).ok();
        }
        let Some(cards) = &self.cards else { return };
        self.state.set_osd(Some(cards.osd(label, fraction)));
        event_loop::arm_timer(&self.osd_timer, std::time::Duration::from_millis(config::OSD_TIMEOUT_MS));
    }

    /// The source list for a cast: the output, then the windows.
    fn open_cast_picker(&mut self, request: u32, types: u32) {
        let mut choices = Vec::new();
        let mut labels = Vec::new();
        if types & portal::MONITOR != 0 {
            choices.push(CastChoice::Output);
            labels.push(format!("Schermo intero ({}x{})", self.state.output.width, self.state.output.height));
        }
        if types & portal::WINDOW != 0 {
            let mut windows: Vec<_> = self.state.window_ws.iter().map(|(k, ws)| (ipc::ws_name(*ws), *k)).collect();
            windows.sort_by(|a, b| a.0.cmp(&b.0));
            for (ws, k) in windows {
                choices.push(CastChoice::Window(k));
                labels.push(format!("{} — {} [{ws}]", self.state.app_id(k), self.state.title(k)));
            }
        }
        if choices.is_empty() {
            self.portal.cancelled(request, true);
            return;
        }
        self.state.open_view(launcher::Launcher::picker(labels), Purpose::Cast(request, choices));
    }

    fn start_cast(&mut self, request: u32, choice: CastChoice) -> Option<u32> {
        let (source, size) = match choice {
            CastChoice::Output => (cast::Source::Output, Some((self.state.output.width, self.state.output.height))),
            CastChoice::Window(k) => (cast::Source::Window(k), self.state.window_size(k)),
        };
        let started = size.ok_or_else(|| io::Error::other("the window has no size")).and_then(|(w, h)| self.casts.start(source, w as u32, h as u32));
        match started {
            Ok(id) => {
                if request != 0 {
                    self.portal.picked(request, id, matches!(choice, CastChoice::Window(_)), encode_choice(choice));
                }
                Some(id)
            }
            Err(e) => {
                log!("cannot start the cast: {e}");
                if request != 0 {
                    self.portal.cancelled(request, true);
                }
                None
            }
        }
    }

    /// For `davide-wm msg cast`: starts a cast and waits for its node.
    fn cast_now(&mut self, window: bool) -> String {
        let choice = match (window, self.state.focus) {
            (true, Some(k)) => CastChoice::Window(k),
            (true, None) => return "error: no focused window\n".into(),
            (false, _) => CastChoice::Output,
        };
        let Some(id) = self.start_cast(0, choice) else { return "error: cannot start the cast\n".into() };
        for _ in 0..200 {
            if let Some(fd) = self.casts.fd() {
                let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                unsafe { libc::poll(&mut p, 1, 10) };
            }
            let mut wake = false;
            for ev in self.casts.dispatch(&mut wake) {
                if let cast::Event::Ready { id: i, node, .. } = ev {
                    if i == id {
                        return format!("cast {id} node {node}\n");
                    }
                }
            }
        }
        "error: PipeWire did not register the stream\n".into()
    }

    fn portal_action(&mut self, a: portal::Action) {
        match a {
            portal::Action::PickSource { request, types, reuse } => {
                // The same choice again (a restore token, or a second session
                // right after the first): no second picker.
                match reuse.and_then(|r| decode_choice(&r)).filter(|c| match c {
                    CastChoice::Output => types & portal::MONITOR != 0,
                    CastChoice::Window(k) => types & portal::WINDOW != 0 && self.state.window_ws.contains_key(k),
                }) {
                    Some(c) => {
                        self.start_cast(request, c);
                    }
                    None => self.open_cast_picker(request, types),
                }
            }
            portal::Action::CancelPick { request } => {
                let open = |p: &Purpose| matches!(p, Purpose::Cast(r, _) if *r == request);
                if self.state.launcher.as_ref().is_some_and(|l| open(&l.purpose)) {
                    self.state.close_launcher();
                }
                if self.state.shot.as_ref().is_some_and(|v| v.request == request) {
                    self.state.close_shot();
                }
                self.state.abandoned.clear();
            }
            portal::Action::StopCast(id) => self.casts.stop(id),
            portal::Action::Screenshot { request, interactive } => self.shot_request = Some((request, interactive)),
        }
    }

    /// Draws the output offscreen without the pointer: RGBA, width, height.
    fn capture(&mut self, r: &mut Renderer) -> Option<(Vec<u8>, i32, i32)> {
        let (w, h) = (self.state.output.width, self.state.output.height);
        let t = r.create_target(w, h, 0).inspect_err(|e| log!("screenshot: {e}")).ok()?;
        self.state.hide_cursor = true;
        self.state.render(r, &t);
        self.state.hide_cursor = false;
        self.state.needs_redraw = true;
        let mut px = r.read_target(&t);
        r.destroy_target(t);
        for p in px.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        Some((px, w, h))
    }

    fn take_screenshot(&mut self, r: &mut Renderer, request: u32, interactive: bool) {
        self.state.close_launcher();
        let Some((px, w, h)) = self.capture(r) else {
            if request != 0 {
                self.portal.screenshot_done(request, None);
            }
            return;
        };
        if interactive {
            self.state.shot = Some(server::ShotView::new(px, w, h, request));
        } else {
            self.shots.push((request, px, w as u32, h as u32));
        }
    }

    /// Input while the screenshot selector is open.
    fn shot_event(&mut self, e: Event) {
        const BTN_LEFT: u32 = 0x110;
        let s = &mut self.state;
        match e {
            Event::PointerMotion(..) | Event::PointerMotionRelative(..) => {
                let (x, y) = match e {
                    Event::PointerMotion(x, y) => (x, y),
                    Event::PointerMotionRelative(dx, dy) => (s.pointer.x + dx, s.pointer.y + dy),
                    _ => unreachable!(),
                };
                s.pointer.x = x.clamp(0.0, (s.output.width - 1) as f64);
                s.pointer.y = y.clamp(0.0, (s.output.height - 1) as f64);
                s.cursor_changed = true;
                s.needs_redraw = true;
            }
            Event::PointerButton(BTN_LEFT, true) => {
                if let Some(v) = &mut s.shot {
                    v.anchor = Some((s.pointer.x, s.pointer.y));
                }
            }
            Event::PointerButton(BTN_LEFT, false) => {
                let sel = s.shot_selection();
                self.finish_shot(Some(sel));
            }
            Event::PointerButton(_, true) => self.finish_shot(None),
            Event::Key(k, pressed) => {
                s.keyboard.update(k, pressed);
                if !pressed {
                    self.swallowed.retain(|x| *x != k);
                    return;
                }
                match s.keyboard.sym(k) {
                    0xff1b => self.finish_shot(None),
                    0xff0d | 0xff8d => {
                        let full = s.output_rect();
                        self.finish_shot(Some(full));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Closes the selector; with a rectangle the shot is saved.
    fn finish_shot(&mut self, sel: Option<wm::Rect>) {
        let Some(v) = self.state.close_shot() else { return };
        match sel {
            Some(r) => {
                let px = png::crop(&v.pixels, v.width as u32, (r.x as u32, r.y as u32, r.w as u32, r.h as u32));
                self.shots.push((v.request, px, r.w as u32, r.h as u32));
            }
            None if v.request != 0 => self.portal.cancelled(v.request, false),
            None => {}
        }
    }

    /// Focuses the window of the app that raised a notification.
    fn focus_notifier(&mut self, app: &str, entry: &str) {
        if [entry, app].iter().any(|a| !a.is_empty() && self.focus_app(a)) {
            return;
        }
        let want = app.to_lowercase();
        let found = self.state.window_ws.keys().find(|k| {
            let id = self.state.app_id(**k).to_lowercase();
            !want.is_empty() && !id.is_empty() && (id.contains(&want) || want.contains(&id))
        });
        if let Some(id) = found.map(|k| self.state.app_id(*k).to_string()) {
            self.focus_app(&id);
        }
    }

    /// Writes the clipboard history on the worker.
    fn save_clipboard(&mut self) {
        self.clipboard_dirty = true;
    }

    /// Redraws the popups from the notification list.
    fn sync_notices(&mut self) {
        if self.cards.is_none() {
            self.cards = launcher::card::Cards::new().map_err(|e| log!("notification cards: {e}")).ok();
        }
        let Some(cards) = &self.cards else { return };
        let list = self.notifier.popups().into_iter().map(|n| (n.id, Some(cards.render(&n.title(), &n.body, n.critical)))).collect();
        self.state.set_notices(list);
    }

    fn notifications_text(&self) -> String {
        self.notifier.list.iter().rev().map(|n| format!("{}\t{}\t{}\n", n.id, if n.live { "popup" } else { "list" }, n.label())).collect()
    }

    fn command(&mut self, cmd: ipc::Command) -> String {
        let s = &mut self.state;
        match cmd {
            ipc::Command::Workspace(ws) => workspaces::switch(s, ws),
            ipc::Command::Exec(line) => self.spawn(&line),
            ipc::Command::Close => {
                if let Some(f) = s.focus {
                    s.close_window(f);
                }
            }
            ipc::Command::Windows => {
                let mut out = String::new();
                for (key, ws) in &s.window_ws {
                    let title = s.title(*key);
                    let focused = if s.focus == Some(*key) { "*" } else { " " };
                    out += &format!("{focused} {}\t{}\t{}\n", ipc::ws_name(*ws), s.app_id(*key), title);
                }
                return out;
            }
            ipc::Command::FocusApp(app_id) => {
                if !self.focus_app(&app_id) {
                    return format!("error: no window with app id {app_id}\n");
                }
            }
            ipc::Command::Upgrade => self.upgrade = true,
            ipc::Command::Pointer(x, y) => self.backend_event(Event::PointerMotion(x, y)),
            ipc::Command::Key(codes) => {
                for c in &codes {
                    self.backend_event(Event::Key(*c, true));
                }
                for c in codes.iter().rev() {
                    self.backend_event(Event::Key(*c, false));
                }
            }
            ipc::Command::Click => {
                self.backend_event(Event::PointerButton(0x110, true));
                self.backend_event(Event::PointerButton(0x110, false));
            }
            ipc::Command::Button(down) => self.backend_event(Event::PointerButton(0x110, down)),
            ipc::Command::Focused => {
                let s = &self.state;
                return match s.focus {
                    Some(k) => format!("{} on {}\n", s.app_id(k), s.window_ws.get(&k).map_or("?".into(), |w| ipc::ws_name(*w))),
                    None => "none\n".into(),
                };
            }
            ipc::Command::Debug => {
                let s = &self.state;
                let name = |k: server::SKey| format!("{}/{} {}", k.client, k.id, s.app_id(k));
                let mut out = format!(
                    "pointer {:.0},{:.0} focus {:?} hit {:?}\nkeyboard {:?}\ndrag {} lock {} prompt {} launcher {} shot {}\n",
                    s.pointer.x,
                    s.pointer.y,
                    s.pointer.focus.map(name),
                    seat::surface_at(s, s.pointer.x, s.pointer.y).map(|(k, x, y)| format!("{} at {x:.0},{y:.0}", name(k))),
                    s.focus.map(name),
                    s.drag.is_some(),
                    s.lock.is_some(),
                    s.prompt.is_some(),
                    s.launcher.is_some(),
                    s.shot.is_some(),
                );
                for (k, r) in &s.layout {
                    let sf = s.surfaces.get(k);
                    out += &format!(
                        "tile {} {:?} float {} subsurfaces {} pending damage {:?}\n",
                        name(*k),
                        r,
                        s.is_floating(*k),
                        sf.map_or(0, |f| f.above.len() + f.below.len()),
                        sf.and_then(|f| f.damage.as_ref().map(Vec::len)),
                    );
                }
                for k in &s.popups {
                    out += &format!("popup {} origin {:?}\n", name(*k), s.window_origin(*k));
                }
                let mib = |b: usize| b as f64 / (1 << 20) as f64;
                let heap = unsafe { libc::mallinfo2() };
                let clips = &s.selection.history.entries;
                out += &format!(
                    "mem: clients {} surfaces {} buffers {} pools {} ({:.1} MiB mapped) clipboard {} ({:.1} MiB) notifications {} heap {:.1} MiB used {:.1} MiB free\n",
                    s.clients.len(),
                    s.surfaces.len(),
                    s.buffers.len(),
                    s.pools.len(),
                    mib(s.pools.values().map(|p| p.size).sum()),
                    clips.len(),
                    mib(s.selection.history.in_memory()),
                    self.notifier.list.len(),
                    mib(heap.uordblks + heap.hblkhd),
                    mib(heap.fordblks),
                );
                if let Some(t) = &s.terms {
                    let (windows, glyphs) = t.usage();
                    out += &format!(
                        "terms: {} windows {:.1} MiB glyphs {} (atlas {:.1} MiB) {}\n",
                        windows.len(),
                        mib(windows.iter().map(|w| w.1).sum()),
                        glyphs,
                        if glyphs > 0 { mib((terminal::glyphs::ATLAS * terminal::glyphs::ATLAS) as usize) } else { 0.0 },
                        windows.iter().map(|(l, b)| format!("[{l} lines {:.1} MiB]", mib(*b))).collect::<Vec<_>>().join(" "),
                    );
                }
                out += &format!("frames: {:?}\n", self.damage_log.iter().map(|(_, r)| r.map(|r| (r.x, r.y, r.w, r.h))).collect::<Vec<_>>());
                return out;
            }
            ipc::Command::Blank => self.blank(),
            ipc::Command::Wake => self.wake(),
            ipc::Command::Screensaver(arg) => {
                let mut ss = self.display.screensaver;
                match arg.as_deref() {
                    None => {}
                    Some("cycle") => ss.cycle(),
                    Some("on") => (ss.enabled, ss.manual) = (true, true),
                    Some("off") => (ss.enabled, ss.manual) = (false, true),
                    Some("auto") => {
                        ss.manual = false;
                        let exclusive = self.backend.heads().iter().any(|h| outputs::is_exclusive(&h.name, &h.model));
                        ss.auto(exclusive);
                    }
                    Some(secs) => match secs.parse::<u32>() {
                        Ok(t) if t > 0 => (ss.enabled, ss.manual, ss.timeout) = (true, true, t),
                        _ => return "error: screensaver [cycle|on|off|auto|<seconds>]\n".into(),
                    },
                }
                self.display.set_screensaver(ss);
                let src = if ss.manual { "manual" } else { "auto" };
                return format!("{} ({src}, {}s)\n", ss.label(), ss.timeout);
            }
            ipc::Command::LidInhibit(toggle) => {
                if toggle {
                    if let Err(e) = self.toggle_lid_inhibitor() {
                        return format!("error: {e}\n");
                    }
                }
                return if self.display.lid_inhibitor.is_some() { "on\n" } else { "off\n" }.into();
            }
            ipc::Command::Outputs => return self.outputs_text(),
            ipc::Command::Clipboard => {
                return self.state.selection.history.entries.iter().enumerate().map(|(i, c)| format!("{i}\t{}\n", clipboard::label(c))).collect();
            }
            ipc::Command::Notifications => return self.notifications_text(),
            // Handled by the event loop, which has the renderer.
            ipc::Command::Screenshot(_) => {}
            ipc::Command::Cast(window) => return self.cast_now(window),
            ipc::Command::CastStop(id) => self.casts.stop(id),
            ipc::Command::Lock => self.lock(),
            ipc::Command::Dnd(on) => {
                if let Some(on) = on {
                    self.set_dnd(on);
                }
                return format!("{}\n", if notify::dnd() { "on" } else { "off" });
            }
            ipc::Command::Damage(on) => {
                self.damage_tracking = on;
                self.state.needs_redraw = true;
            }
            ipc::Command::HwCursor(on) => {
                self.hw_cursor_enabled = on;
                self.state.needs_redraw = true;
            }
            ipc::Command::Night(arg) => {
                match arg.as_deref() {
                    None => {}
                    Some("on") => self.night = display::Night::On,
                    Some("off") => self.night = display::Night::Off,
                    Some("auto") => self.night = display::Night::Auto,
                    Some(other) => return format!("error: night takes on, off or auto, not {other:?}\n"),
                }
                self.night_light();
                return format!("{:?}\n", self.night).to_lowercase();
            }
            ipc::Command::Term(cwd) => self.term_open.push(terminal::Open { cwd, ..Default::default() }),
            ipc::Command::TermRun(cmd) => self.term_open.push(terminal::Open { cwd: None, cmd: Some(cmd), hold: false }),
            ipc::Command::ClipboardImport(path) => {
                let older = match std::fs::read(&path) {
                    Ok(b) => clipboard::decode_v1(&b),
                    Err(e) => return format!("error: {e}\n"),
                };
                let n = self.state.selection.history.import(older);
                self.clipboard_dirty = true;
                return format!("{n} entries added, {} in total\n", self.state.selection.history.entries.len());
            }
            ipc::Command::Exit => self.running = false,
        }
        "ok\n".into()
    }

    /// Switches to the window of an app (app id compared case-insensitively).
    fn focus_app(&mut self, app_id: &str) -> bool {
        let s = &mut self.state;
        let Some((key, ws)) = s.window_ws.iter().find(|(k, _)| s.app_id(**k).eq_ignore_ascii_case(app_id)).map(|(k, w)| (*k, *w)) else {
            return false;
        };
        workspaces::switch(s, ws);
        seat::set_focus(s, Some(key));
        true
    }

    fn bar_message(&mut self, msg: bar::FromBar, r: &mut Renderer) {
        let ws_of = |id: i32| match id {
            -1 => Some(wm::WsId::Chat),
            -99 => Some(wm::WsId::Special),
            n if n > 0 => Some(wm::WsId::Num(n as u32)),
            _ => None,
        };
        match msg {
            bar::FromBar::Frame { width, height, pixels } => self.state.set_bar_frame(r, width, height, &pixels),
            bar::FromBar::Workspace { id, .. } => {
                if let Some(ws) = ws_of(id) {
                    workspaces::switch(&mut self.state, ws);
                }
            }
            bar::FromBar::FocusApp(app) => {
                self.focus_app(&app);
            }
            bar::FromBar::Blank => self.blank(),
            bar::FromBar::ToggleNotifications => self.open_notifications(),
            bar::FromBar::ToggleDnd => self.set_dnd(!notify::dnd()),
            bar::FromBar::Wifi => self.wifi_job = Some(None),
            bar::FromBar::Brightness(pct) => {
                if let Some(pct) = brightness::set_panel(pct) {
                    self.ddc = Some(pct);
                    self.show_osd(&format!("Luminosità {pct}%"), Some(pct as f32 / 100.0));
                }
            }
            bar::FromBar::Timer => {
                const CHOICES: [(u64, &str); 9] = [
                    (300, "5 minuti"),
                    (600, "10 minuti"),
                    (900, "15 minuti"),
                    (1500, "25 minuti"),
                    (1800, "30 minuti"),
                    (2700, "45 minuti"),
                    (3600, "1 ora"),
                    (7200, "2 ore"),
                    (0, "Ferma il timer"),
                ];
                let labels = CHOICES.iter().map(|(_, l)| l.to_string()).collect();
                self.state.open_view(launcher::Launcher::picker(labels), Purpose::Timer(CHOICES.iter().map(|(s, _)| *s).collect()));
            }
            bar::FromBar::TimerDone => {
                self.notifier.local("Timer", "Tempo scaduto", "", true);
                self.sync_notices();
            }
            bar::FromBar::Screenshot => self.shot_request = Some((0, true)),
            bar::FromBar::Outputs(list) => {
                let labels = list.iter().map(|(_, d, on)| if *on { format!("{d} · attiva") } else { d.clone() }).collect();
                let names = list.into_iter().map(|(n, ..)| n).collect();
                self.state.open_view(launcher::Launcher::picker(labels), Purpose::Outputs(names));
            }
            bar::FromBar::Bluetooth => self.jobs.push(Box::new(|| services::bt_devices().map(|(p, d)| worker::Done::Bluetooth(p, d)))),
            bar::FromBar::Calendar => self.toggle_calendar(),
            bar::FromBar::OpenMixer(output) => self.open_mixer(output),
            bar::FromBar::Mixer(m) => {
                if let Some(ui) = &mut self.mixer {
                    ui.replace(m);
                    self.draw_mixer();
                }
            }
            bar::FromBar::BatteryMenu => self.open_battery(),
            bar::FromBar::CpuCycle => {
                set_cpu_auto(false);
                self.cpu_job = true;
            }
            bar::FromBar::CycleScreensaver => {
                let mut ss = self.display.screensaver;
                ss.cycle();
                self.display.set_screensaver(ss);
            }
            bar::FromBar::ToggleLid => {
                if let Err(e) = self.toggle_lid_inhibitor() {
                    log!("lid inhibitor: {e}");
                }
            }
            bar::FromBar::Audio { mic, level, muted } => {
                let what = if mic { "Microfono" } else { "Volume" };
                match muted {
                    true => self.show_osd(&format!("{what}: muto"), None),
                    false => self.show_osd(&format!("{what} {level}%"), Some(level as f32 / 100.0)),
                }
            }
            bar::FromBar::Notified { workspace } => {
                if let Some(ws) = ws_of(workspace).filter(|w| *w != self.state.active) {
                    self.state.urgent_ws.insert(ws);
                }
            }
        }
    }

    /// Tells the bar thread what changed since the last call.
    fn update_bar(&mut self) {
        let Some(bar) = &self.bar else { return };
        let width = self.state.output.width as u32;
        if width != self.bar_width {
            self.bar_width = width;
            bar.send(bar::ToBar::Width(width));
        }
        let display = (self.display.screensaver.label(), self.display.lid_inhibitor.is_some());
        if self.bar_display.as_ref() != Some(&display) {
            bar.send(bar::ToBar::Display { screensaver: display.0.clone(), lid_inhibited: display.1 });
            self.bar_display = Some(display);
        }
        let count = self.notifier.list.len();
        if self.bar_notifications != Some(count) {
            bar.send(bar::ToBar::NotificationCount(count as u32));
            self.bar_notifications = Some(count);
        }
        let snapshot = self.state.bar_snapshot();
        if self.state.bar.sent.as_ref() != Some(&snapshot) {
            bar.send(bar::ToBar::Workspaces(snapshot.0.clone()));
            bar.send(bar::ToBar::Clients(snapshot.1.clone()));
            self.state.bar.sent = Some(snapshot);
        }
    }

    /// Blanks every output until the next input (blk).
    fn blank(&mut self) {
        if self.display.blanked {
            return;
        }
        log!("blank");
        self.display.blank();
        self.state.close_launcher();
        if let Backend::Drm(d) = &mut self.backend {
            if let Err(e) = d.set_blank(true, false) {
                log!("blank: {e}");
            }
        }
    }

    fn wake(&mut self) {
        if !self.display.blanked {
            return;
        }
        log!("wake");
        self.display.wake();
        if let Backend::Drm(d) = &mut self.backend {
            if let Err(e) = d.set_blank(false, false) {
                log!("wake: {e}");
            }
        }
        self.last_frame = None;
        self.forget_buffers();
        self.state.needs_redraw = true;
    }

    /// The idle timer fired.
    fn idle(&mut self) {
        match self.display.timer_fired() {
            None => {}
            Some(display::Idle::Blank) if self.state.idle_inhibited() => self.display.activity(),
            Some(display::Idle::Blank) => {
                log!("idle for {}s", self.display.screensaver.timeout);
                self.blank();
            }
            Some(display::Idle::PowerDown) => {
                log!("blanked for {}s: powering down", self.display.screensaver.power_off_after());
                self.display.power_down();
                if let Backend::Drm(d) = &mut self.backend {
                    if let Err(e) = d.set_blank(true, true) {
                        log!("power down: {e}");
                    }
                }
            }
        }
    }

    /// The set of lit outputs changed: the automatic screensaver policy and
    /// a full redraw.
    fn outputs_changed(&mut self) {
        let exclusive = self.backend.heads().iter().any(|h| outputs::is_exclusive(&h.name, &h.model));
        let mut s = self.display.screensaver;
        s.auto(exclusive);
        self.display.set_screensaver(s);
        self.display.black_shown.clear();
        self.last_frame = None;
        self.forget_buffers();
        self.mirrors_stale.clear();
        // Another output means another cursor plane, blank until written.
        self.cursor_image = None;
        self.state.needs_redraw = true;
        self.sync_notices();
    }

    fn layout_events(&mut self, events: io::Result<Vec<Event>>) {
        match events {
            Ok(events) => {
                for e in events {
                    self.backend_event(e);
                }
            }
            Err(e) => log!("output layout: {e}"),
        }
        self.outputs_changed();
    }

    fn toggle_lid_inhibitor(&mut self) -> io::Result<()> {
        self.display.toggle_lid_inhibitor()?;
        log!("lid inhibitor {}", if self.display.lid_inhibitor.is_some() { "on" } else { "off" });
        Ok(())
    }

    fn outputs_text(&self) -> String {
        let mut out = String::new();
        for (i, h) in self.backend.heads().iter().enumerate() {
            let role = if i == 0 { "primary" } else { "mirror" };
            out += &format!("{}\t{}x{}@{:.2}\t{role}\t{}\n", h.name, h.size.0, h.size.1, h.refresh_mhz as f64 / 1000.0, h.model);
        }
        out
    }

    /// Draws the primary output, copies it to the mirrors, or shows black
    /// while blanked.
    /// Forgets what the buffers show: the next frames are whole.
    fn forget_buffers(&mut self) {
        self.buffer_frame.clear();
        self.damage_log.clear();
    }

    /// The area buffer `idx` must redraw to show the scene, given what it
    /// showed last: None for everything.
    fn region_for(&mut self, idx: usize, full: bool) -> Option<wm::Rect> {
        let union = |a: wm::Rect, b: wm::Rect| {
            let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
            let (x1, y1) = ((a.x + a.w).max(b.x + b.w), (a.y + a.h).max(b.y + b.h));
            wm::Rect::new(x0, y0, x1 - x0, y1 - y0)
        };
        let this = (!full && self.damage_tracking).then(|| self.state.damage.iter().copied().reduce(union)).flatten();
        self.frame_no += 1;
        let region = match (this, self.buffer_frame.get(&idx).copied()) {
            (Some(now), Some(last)) => {
                let covered = self.damage_log.front().is_some_and(|(f, _)| *f <= last + 1);
                let missed: Option<Vec<Option<wm::Rect>>> = covered.then(|| self.damage_log.iter().filter(|(f, _)| *f > last).map(|(_, r)| *r).collect());
                missed.and_then(|m| m.into_iter().try_fold(now, |acc, r| r.map(|r| union(acc, r))))
            }
            _ => None,
        };
        self.damage_log.push_back((self.frame_no, this));
        while self.damage_log.len() > 8 {
            self.damage_log.pop_front();
        }
        self.buffer_frame.insert(idx, self.frame_no);
        region
    }

    /// Chooses between the cursor plane and drawing the pointer in the
    /// scene, and turns pointer changes into plane moves or damage.
    fn update_cursor(&mut self, r: &mut Renderer) {
        let heads = self.backend.heads().len();
        let usable = self.hw_cursor_enabled && heads == 1 && self.casts.is_empty() && self.state.drag.is_none() && !self.display.blanked;
        let mut hw = false;
        if let (Backend::Drm(d), true) = (&mut self.backend, usable) {
            if let Some(max) = d.cursor_size() {
                let image = self.state.pointer.cursor;
                let fb = d.cursor_fb();
                if self.state.cursor_image_dirty || self.cursor_image.is_none_or(|(i, _, f)| i != image || f != fb) {
                    self.state.upload_pending(r);
                    let pixels = self.state.cursor_pixels(r, max);
                    if let Some((w, h, px)) = &pixels {
                        d.set_cursor_image(*w, *h, px);
                    }
                    self.cursor_image = Some((image, pixels.is_some(), fb));
                    self.state.cursor_image_dirty = false;
                    self.state.cursor_changed = true;
                }
                hw = self.cursor_image.is_some_and(|(_, fits, _)| fits);
            }
        }
        if hw != self.state.hw_cursor {
            self.state.hw_cursor = hw;
            self.state.needs_redraw = true;
            self.state.cursor_changed = true;
            if let (false, Backend::Drm(d)) = (hw, &mut self.backend) {
                d.set_cursor_pos(None);
                self.cursor_moved = true;
            }
        }
        if !std::mem::take(&mut self.state.cursor_changed) {
            return;
        }
        if hw {
            let (x, y) = (self.state.pointer.x as i32, self.state.pointer.y as i32);
            let pos = match self.state.pointer.cursor {
                server::CursorImage::Hidden => None,
                server::CursorImage::Default => Some((x, y)),
                server::CursorImage::Surface(_, hx, hy) => Some((x - hx, y - hy)),
            };
            if let Backend::Drm(d) = &mut self.backend {
                d.set_cursor_pos(pos);
            }
            self.cursor_moved = true;
        } else {
            if let Some(old) = self.state.cursor_drawn {
                self.state.damage(old);
            }
            if let Some(new) = self.state.cursor_rect() {
                self.state.damage(new);
            }
        }
    }

    fn render(&mut self, r: &mut Renderer) -> io::Result<()> {
        let heads = self.backend.heads().len();
        self.state.cast_windows = self.casts.windows();
        self.update_cursor(r);
        // Even with nothing to draw (blanked, locked, hidden windows): a
        // client waiting for its buffer back would spin.
        if self.state.uploads_pending {
            self.state.upload_pending(r);
        }
        if self.display.blanked {
            self.forget_buffers();
            for h in 0..heads {
                if self.display.black_shown.contains(&h) {
                    continue;
                }
                if let Some(i) = self.backend.acquire(h, r)? {
                    r.begin(self.backend.target(h, i), [0.0, 0.0, 0.0, 1.0]);
                    r.finish();
                    self.present(h, i, r)?;
                    self.display.black_shown.push(h);
                }
            }
            return Ok(());
        }
        if self.state.needs_redraw || !self.state.damage.is_empty() {
            if let Some(i) = self.backend.acquire(0, r)? {
                let full = self.state.needs_redraw;
                let region = self.region_for(i, full);
                self.state.render_damaged(r, self.backend.target(0, i), region);
                self.cursor_moved = false;
                if !self.casts.is_empty() {
                    self.casts.frames(&mut self.state, r, self.backend.target(0, i));
                }
                self.last_frame = Some(i);
                self.mirrors_stale = (1..heads).collect();
                self.present(0, i, r)?;
            } else if matches!(self.backend, Backend::Nested(_)) && self.casts.streaming() {
                // A host that does not show our window (another workspace)
                // stops taking frames: keep the casts going offscreen.
                if self.casts.due() {
                    let (w, h) = (self.state.output.width, self.state.output.height);
                    if let Ok(t) = r.create_target(w, h, 0) {
                        self.state.render(r, &t);
                        self.casts.frames(&mut self.state, r, &t);
                        r.destroy_target(t);
                    }
                }
                self.casts.catch_up = true;
                self.state.needs_redraw = true;
            }
        } else if self.cursor_moved {
            if let Backend::Drm(d) = &mut self.backend {
                match d.commit_cursor() {
                    Ok(true) => self.cursor_moved = false,
                    // A flip is pending: again after it.
                    Ok(false) => {}
                    Err(e) => {
                        log!("cursor: {e}");
                        self.cursor_moved = false;
                    }
                }
            }
        }
        let Some(src) = self.last_frame else { return Ok(()) };
        for h in std::mem::take(&mut self.mirrors_stale) {
            match self.backend.acquire(h, r)? {
                Some(i) => {
                    r.blit(self.backend.target(0, src), self.backend.target(h, i));
                    r.finish();
                    self.present(h, i, r)?;
                }
                None => self.mirrors_stale.push(h),
            }
        }
        Ok(())
    }

    fn present(&mut self, head: usize, idx: usize, r: &mut Renderer) -> io::Result<()> {
        let Err(e) = self.backend.present(head, idx) else { return Ok(()) };
        let name = self.backend.heads().get(head).map(|h| h.name.clone()).unwrap_or_default();
        if !e.to_string().starts_with("modeset") {
            // Expected while switching VT: logind has already revoked DRM master.
            log!("present failed: {e}");
            return Ok(());
        }
        match &mut self.backend {
            // A monitor that refuses its mode is dropped; the laptop panel
            // refusing leaves nothing to show, so give up instead of running blind.
            Backend::Drm(d) if name != config::INTERNAL_OUTPUT => {
                log!("{e}");
                let events = d.link_failed(&name, r);
                self.layout_events(events);
                Ok(())
            }
            _ => Err(e),
        }
    }

    fn backend_event(&mut self, e: Event) {
        let input = matches!(e, Event::PointerMotion(..) | Event::PointerMotionRelative(..) | Event::PointerButton(..) | Event::PointerAxis(..) | Event::Key(..));
        if input && self.display.input() {
            // Releases go through: a key held when the screen blanked must
            // not stay down (and repeat) in its client.
            let release = matches!(e, Event::Key(_, false) | Event::PointerButton(_, false));
            if !release {
                if self.display.may_wake() {
                    self.wake();
                    if let Event::Key(k, true) = e {
                        self.swallowed.push(k);
                    }
                }
                return;
            }
        }
        if self.state.lock.is_some() || self.state.prompt.is_some() {
            if let Event::Key(k, p) = e {
                self.key(k, p);
            }
            return;
        }
        if self.state.shot.is_some() {
            self.shot_event(e);
            return;
        }
        if self.mixer.is_some() && self.mixer_pointer(&e) {
            return;
        }
        if self.state.launcher.is_some() {
            match e {
                Event::PointerMotion(..) | Event::PointerMotionRelative(..) => {
                    let (x, y) = match e {
                        Event::PointerMotion(x, y) => (x, y),
                        Event::PointerMotionRelative(dx, dy) => (self.state.pointer.x + dx, self.state.pointer.y + dy),
                        _ => unreachable!(),
                    };
                    self.state.pointer.x = x.clamp(0.0, (self.state.output.width - 1) as f64);
                    self.state.pointer.y = y.clamp(0.0, (self.state.output.height - 1) as f64);
                    self.state.needs_redraw = true;
                    return;
                }
                Event::PointerButton(_, true) => {
                    let rect = self.state.launcher.as_ref().unwrap().rect(&self.state.output);
                    if !rect.contains(self.state.pointer.x, self.state.pointer.y) {
                        self.state.close_launcher();
                    }
                    return;
                }
                Event::PointerButton(..) | Event::PointerAxis(..) => return,
                _ => {}
            }
        }
        if let Event::PointerButton(_, true) = e {
            if self.state.calendar.is_some() && !self.state.in_bar(self.state.pointer.y) {
                self.state.set_calendar(None);
            }
        }
        if let Event::PointerButton(b, true) = e {
            if let Some(id) = self.state.notice_at(self.state.pointer.x, self.state.pointer.y) {
                const BTN_LEFT: u32 = 0x110;
                if b == BTN_LEFT {
                    if let Some((app, entry)) = self.notifier.invoke(id) {
                        self.focus_notifier(&app, &entry);
                    }
                } else {
                    self.notifier.dismiss(id);
                }
                self.sync_notices();
                return;
            }
        }
        match e {
            Event::Resize(w, h) => self.state.set_output_size(w, h),
            Event::OutputName(name) => self.state.output.name = name,
            Event::Close => self.running = false,
            Event::PointerMotion(x, y) => seat::pointer_motion(&mut self.state, x, y),
            Event::PointerMotionRelative(dx, dy) => {
                let (x, y) = (self.state.pointer.x + dx, self.state.pointer.y + dy);
                seat::pointer_motion(&mut self.state, x, y);
            }
            Event::PointerButton(b, p) if self.state.drag.is_some() => seat::pointer_button(&mut self.state, b, p),
            Event::PointerButton(b, p) if self.state.in_bar(self.state.pointer.y) => {
                const BTN_LEFT: u32 = 0x110;
                if b == BTN_LEFT && !p {
                    if let Some(bar) = &self.bar {
                        bar.send(bar::ToBar::Click { x: self.state.pointer.x as i32 });
                    }
                }
            }
            Event::PointerButton(b, p) => seat::pointer_button(&mut self.state, b, p),
            Event::PointerAxis(a, v) => seat::pointer_axis(&mut self.state, a, v),
            Event::Key(k, p) => self.key(k, p),
            Event::KeyboardLeave => {
                self.state.keyboard.reset();
                self.swallowed.clear();
            }
            // Handled in the event loop, which has the renderer.
            Event::Lid(_) => {}
            Event::Lock => self.lock(),
            Event::Swipe(step) => {
                // Numbered workspaces only; from chat or special, start at 1.
                let next = match self.state.active {
                    wm::WsId::Num(n) => (n as i32 - step).clamp(1, 41) as u32,
                    _ => 1,
                };
                workspaces::switch(&mut self.state, wm::WsId::Num(next));
            }
            Event::Sleep(true) => {
                self.lock();
                self.sleep_after_frame = true;
            }
            Event::Sleep(false) => self.take_sleep_inhibitor(),
        }
    }
}

/// Everything the event loop works with.
struct Ctx {
    app: App,
    renderer: Renderer,
    ep: Epoll,
    worker: worker::Worker,
    _signals: OwnedFd,
    /// The binary to exec on a live upgrade (as it was when we started).
    exe: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn new_app(backend: Backend, state: State, socket: Socket, ipc: ipc::Ipc, main_mod: u32, session: bool, display: display::Display, notifier: notify::Notifier) -> App {
    App {
        backend,
        bar: None,
        bar_width: 0,
        state,
        socket,
        ipc,
        binds: config::binds(),
        main_mod,
        swallowed: Vec::new(),
        tap: None,
        running: true,
        upgrade: false,
        session,
        display,
        bar_display: None,
        last_frame: None,
        mirrors_stale: Vec::new(),
        notifier,
        cards: None,
        bar_notifications: None,
        clipboard_dirty: false,
        xwayland: None,
        casts: cast::Casts::default(),
        pipewire_watched: false,
        portal: portal::Portal::start(),
        shot_request: None,
        shots: Vec::new(),
        osd_timer: event_loop::timer_fd().expect("timerfd"),
        ddc: None,
        unlock: None,
        sleep_inhibitor: None,
        sleep_after_frame: false,
        agents: None,
        polkit_job: None,
        wifi_job: None,
        cpu_job: false,
        jobs: Vec::new(),
        battery_timer: event_loop::timer_fd().expect("timerfd"),
        battery: services::BatteryWatch::default(),
        ac: None,
        night: display::Night::Auto,
        night_sent: None,
        damage_tracking: true,
        frame_no: 0,
        buffer_frame: std::collections::HashMap::new(),
        damage_log: std::collections::VecDeque::new(),
        hw_cursor_enabled: true,
        cursor_image: None,
        cursor_moved: false,
        mixer: None,
        term_open: Vec::new(),
        term_watched: HashMap::new(),
    }
}

impl App {
    fn claim_x_display(&mut self) {
        match xwayland::Xwayland::claim() {
            Ok(x) => self.xwayland = Some(x),
            Err(e) => log!("no X display: {e}"),
        }
    }

    /// A first X client knocks: starts Xwayland on the display's sockets.
    fn start_xwayland(&mut self, ep: &Epoll) -> io::Result<()> {
        let Some(x) = &mut self.xwayland else { return Ok(()) };
        for fd in x.listen_fds() {
            ep.delete(fd);
        }
        match x.start() {
            Ok(wayland) => {
                let id = self.state.add_client(wayland);
                self.state.xwayland = Some(id);
                ep.add(self.state.clients[&id].conn.raw_fd(), TOKEN_CLIENT + id as u64)?;
            }
            Err(e) => {
                log!("cannot start Xwayland: {e}");
                self.xwayland = None;
                std::env::remove_var("DISPLAY");
            }
        }
        self.watch_xwayland(ep)
    }

    /// Watches the X display: its sockets while Xwayland is not running,
    /// else Xwayland's exit, its readiness and the WM connection.
    fn watch_xwayland(&self, ep: &Epoll) -> io::Result<()> {
        if let Some(x) = &self.xwayland {
            for fd in x.listen_fds() {
                ep.add(fd, TOKEN_XWAYLAND_LISTEN)?;
            }
            if let Some(p) = &x.process {
                ep.add(p.pidfd.as_raw_fd(), TOKEN_XWAYLAND_EXIT)?;
                if let Some((ready, _)) = &p.pending {
                    ep.add(ready.as_raw_fd(), TOKEN_XWAYLAND_READY)?;
                }
            }
        }
        if let Some(x) = &self.state.xwm {
            ep.add(x.conn.raw_fd(), TOKEN_XWM)?;
        }
        Ok(())
    }

    fn xwayland_ready(&mut self, ep: &Epoll) -> io::Result<()> {
        let Some(wm) = self.xwayland.as_mut().and_then(|x| x.process.as_mut()).and_then(|p| p.ready()) else { return Ok(()) };
        match xwm::start(&mut self.state, wm) {
            Ok(()) => {
                ep.add(self.state.xwm.as_ref().unwrap().conn.raw_fd(), TOKEN_XWM)?;
                self.xwm_dispatch(ep);
            }
            Err(e) => log!("cannot manage Xwayland windows: {e}"),
        }
        Ok(())
    }

    /// Renders the primary output into a fresh buffer and saves it as PNG.
    fn screenshot(&mut self, path: &str, r: &mut Renderer) -> String {
        let (w, h) = (self.state.output.width, self.state.output.height);
        let t = match r.create_target(w, h, 0) {
            Ok(t) => t,
            Err(e) => return format!("error: {e}\n"),
        };
        self.state.render(r, &t);
        self.state.needs_redraw = true;
        let mut px = r.read_target(&t);
        r.destroy_target(t);
        for p in px.as_chunks_mut::<4>().0 {
            p[3] = 255;
        }
        match std::fs::write(path, png::encode(w as u32, h as u32, &px)) {
            Ok(()) => "ok\n".into(),
            Err(e) => format!("error: {e}\n"),
        }
    }

    fn xwm_dispatch(&mut self, ep: &Epoll) {
        if let Err(e) = xwm::dispatch(&mut self.state) {
            log!("X connection lost: {e}");
            self.xwm_stop(ep);
        }
    }

    fn xwm_stop(&mut self, ep: &Epoll) {
        if let Some(x) = &self.state.xwm {
            ep.delete(x.conn.raw_fd());
        }
        xwm::stop(&mut self.state);
    }

    /// Stops Xwayland after two checks in a row without X windows; the
    /// next client starts it again.
    fn xwayland_idle_check(&mut self) {
        let busy = xwm::has_clients(&self.state);
        let ready = self.state.xwm.is_some();
        let Some(p) = self.xwayland.as_mut().and_then(|x| x.process.as_mut()) else { return };
        if busy || !ready {
            p.idle = false;
        } else if p.idle {
            log!("no X windows left: stopping Xwayland");
            p.stop();
        } else {
            p.idle = true;
        }
    }

    /// Runs `f` on the terminals, which live in the state they also change.
    fn with_terms<T: Default>(&mut self, f: impl FnOnce(&mut terminal::Terms, &mut server::State) -> T) -> T {
        let Some(mut t) = self.state.terms.take() else { return T::default() };
        let out = f(&mut t, &mut self.state);
        self.state.terms = Some(t);
        out
    }

    /// Opens the terminal windows asked for and keeps the epoll set in step
    /// with the terminals' fds.
    fn terminals(&mut self, ep: &Epoll) -> io::Result<()> {
        if !self.term_open.is_empty() && self.state.terms.is_none() {
            match terminal::Terms::start(&mut self.state) {
                Ok(t) => {
                    let id = t.client;
                    ep.add(self.state.clients[&id].conn.raw_fd(), TOKEN_CLIENT + id as u64)?;
                    self.state.terms = Some(t);
                }
                Err(e) => {
                    log!("terminal: {e}");
                    self.term_open.clear();
                }
            }
        }
        for o in std::mem::take(&mut self.term_open) {
            self.with_terms(|t, _| t.open(o));
        }
        let wanted: HashMap<RawFd, u64> = match &self.state.terms {
            Some(t) => t
                .fds()
                .into_iter()
                .map(|(fd, kind)| {
                    let token = match kind {
                        terminal::Fd::Client => TOKEN_TERM,
                        terminal::Fd::Timer => TOKEN_TERM_TIMER,
                        terminal::Fd::Pty(surface) => TOKEN_PTY + surface as u64,
                        terminal::Fd::Paste => TOKEN_PASTE + fd as u64,
                    };
                    (fd, token)
                })
                .collect(),
            None => HashMap::new(),
        };
        for (fd, token) in &self.term_watched {
            if wanted.get(fd) != Some(token) {
                ep.delete(*fd);
            }
        }
        for (fd, token) in &wanted {
            if self.term_watched.get(fd) != Some(token) {
                ep.add(*fd, *token)?;
            }
        }
        self.term_watched = wanted;
        if let Some(t) = &mut self.state.terms {
            t.graveyard.clear();
        }
        Ok(())
    }

    /// Xwayland quit, idle or crashed: listen again for the next client,
    /// unless it died right after starting.
    fn xwayland_exited(&mut self, ep: &Epoll) -> io::Result<()> {
        let Some(p) = self.xwayland.as_mut().and_then(|x| x.process.take()) else { return Ok(()) };
        log!("Xwayland exited");
        self.xwm_stop(ep);
        ep.delete(p.pidfd.as_raw_fd());
        if !p.lived() {
            log!("Xwayland died right after starting: no X display");
            self.xwayland = None;
            std::env::remove_var("DISPLAY");
            return Ok(());
        }
        drop(p);
        if !self.xwayland.as_ref().is_some_and(|x| x.can_listen()) {
            self.xwayland = None;
            self.claim_x_display();
            match &self.xwayland {
                Some(x) => std::env::set_var("DISPLAY", format!(":{}", x.display)),
                None => std::env::remove_var("DISPLAY"),
            }
        }
        self.watch_xwayland(ep)
    }
}

fn cpu_auto_path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    dir.join("davide-wm/cpu-auto")
}

/// The CPU level follows the charger (a state file, so it survives restarts).
fn cpu_auto() -> bool {
    cpu_auto_path().exists()
}

fn set_cpu_auto(on: bool) {
    let p = cpu_auto_path();
    let _ = match on {
        true => p.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| std::fs::write(&p, "")),
        false => std::fs::remove_file(&p),
    };
}

/// A cast source as text, for the portal's restore data.
fn encode_choice(c: CastChoice) -> String {
    match c {
        CastChoice::Output => "output".into(),
        CastChoice::Window(k) => format!("window:{}:{}", k.client, k.id),
    }
}

fn decode_choice(s: &str) -> Option<CastChoice> {
    if s == "output" {
        return Some(CastChoice::Output);
    }
    let mut it = s.strip_prefix("window:")?.split(':');
    let client = it.next()?.parse().ok()?;
    let id = it.next()?.parse().ok()?;
    Some(CastChoice::Window(server::SKey { client, id }))
}

/// Writes a screenshot to `$HOME/SCREENSHOT_DIR/Screenshot_<local time>.png`.
fn save_screenshot(png: &[u8]) -> Result<String, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let dir = PathBuf::from(home).join(config::SCREENSHOT_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    let name = format!(
        "Screenshot_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.png",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    );
    let path = dir.join(name);
    std::fs::write(&path, png).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// Registers every fd with a fresh epoll and starts background jobs.
fn finish(mut app: App, renderer: Renderer, exe: PathBuf) -> io::Result<Ctx> {
    // Programs started by the bar inherit these.
    std::env::set_var("WAYLAND_DISPLAY", &app.socket.name);
    std::env::set_var(ipc::ENV, &app.ipc.path);
    std::env::set_var("XDG_CURRENT_DESKTOP", "davide-wm");
    std::env::remove_var("WAYLAND_SOCKET");
    match &app.xwayland {
        Some(x) => std::env::set_var("DISPLAY", format!(":{}", x.display)),
        None => std::env::remove_var("DISPLAY"),
    }
    let bar = bar::Bar::spawn()?;
    app.state.bar.height = bar.height() as i32;
    app.state.relayout();
    let signals = event_loop::signal_fd()?;
    let ep = Epoll::new()?;
    let worker = worker::Worker::new()?;
    match &app.backend {
        Backend::Nested(n) => ep.add(n.raw_fd(), TOKEN_HOST)?,
        Backend::Drm(d) => {
            for (fd, token) in d.fds().into_iter().zip([TOKEN_KMS, TOKEN_INPUT, TOKEN_LOGIND, TOKEN_UEVENT]) {
                ep.add(fd, token)?;
            }
        }
    }
    ep.add(app.socket.listener.as_raw_fd(), TOKEN_LISTENER)?;
    ep.add(signals.as_raw_fd(), TOKEN_SIGNAL)?;
    ep.add(app.ipc.listener.as_raw_fd(), TOKEN_IPC)?;
    ep.add(worker.raw_fd(), TOKEN_WORKER)?;
    ep.add(bar.raw_fd(), TOKEN_BAR)?;
    ep.add(app.display.timer_fd(), TOKEN_IDLE)?;
    ep.add(app.notifier.timer_fd(), TOKEN_NOTIFY_TIMER)?;
    if let Some(fd) = app.notifier.bus_fd() {
        ep.add(fd, TOKEN_NOTIFY_BUS)?;
    }
    app.sync_notices();
    bar.send(bar::ToBar::Dnd(notify::dnd()));
    app.bar = Some(bar);
    for (id, fd) in app.state.client_fds() {
        ep.add(fd, TOKEN_CLIENT + id as u64)?;
    }
    app.watch_xwayland(&ep)?;
    app.terminals(&ep)?;
    app.take_sleep_inhibitor();
    let agents_wanted = matches!(app.backend, Backend::Drm(_)) || std::env::var_os("DAVIDE_WM_AGENTS").is_some();
    if agents_wanted && app.agents.is_none() {
        app.agents = Some(agents::Agents::start());
    }
    if let Some(fd) = app.agents.as_ref().and_then(|a| a.bus_fd()) {
        ep.add(fd, TOKEN_AGENTS)?;
    }
    if let Some(fd) = app.portal.bus_fd() {
        ep.add(fd, TOKEN_PORTAL)?;
    }
    ep.add(app.osd_timer.as_raw_fd(), TOKEN_OSD)?;
    event_loop::arm_periodic(&app.battery_timer, std::time::Duration::from_secs(60));
    app.night_light();
    ep.add(app.battery_timer.as_raw_fd(), TOKEN_BATTERY)?;
    if app.state.wallpaper.is_none() && !WALLPAPER.is_empty() {
        worker.run(|| {
            let mut img = jpeg::decode(WALLPAPER).map_err(|e| e.to_string())?;
            let (mw, mh) = config::WALLPAPER_MIN_SIZE;
            while img.width / 2 >= mw && img.height / 2 >= mh {
                img = jpeg::halve(&img);
            }
            Ok(worker::Done::Wallpaper(img))
        });
    }
    keep_previous_binary();
    Ok(Ctx { app, renderer, ep, worker, _signals: signals, exe })
}

fn fresh(nested: bool, session: bool) -> io::Result<Ctx> {
    let exe = std::env::current_exe()?;
    let keyboard = keyboard::Keyboard::new(config::KEYBOARD_LAYOUT)?;
    let (mut renderer, backend, main_mod) = if nested {
        let renderer = Renderer::new(config::RENDER_NODE).map_err(io::Error::other)?;
        (renderer, Backend::Nested(Nested::connect()?), mods::ALT)
    } else {
        let (drm, renderer) = Drm::open()?;
        (renderer, Backend::Drm(drm), mods::SUPER)
    };
    let (w, h) = match &backend {
        Backend::Nested(n) => n.size,
        Backend::Drm(_) => config::NESTED_SIZE,
    };
    let output = server::Output { name: "NESTED-1".into(), width: w, height: h };
    let socket = Socket::bind()?;
    let ipc = ipc::Ipc::bind(&socket.name)?;
    log!("listening on {}", socket.name);
    let state = State::new(keyboard, output, renderer.dmabuf_formats());
    let mut state = state;
    state.dmabuf_device = renderer.device();
    state.selection.history = clipboard::History::load();
    let mut app = new_app(backend, state, socket, ipc, main_mod, session, display::Display::new(None)?, notify::Notifier::start()?);
    if let Backend::Drm(d) = &mut app.backend {
        let events = d.apply_layout(&mut renderer);
        app.layout_events(events);
        app.state.pointer.x = (app.state.output.width / 2) as f64;
        app.state.pointer.y = (app.state.output.height / 2) as f64;
    }
    app.outputs_changed();
    app.claim_x_display();
    if session {
        for line in config::SESSION_AUTOSTART {
            app.spawn(line);
        }
    }
    for line in config::AUTOSTART {
        app.spawn(line);
    }
    finish(app, renderer, exe)
}

/// Bumped when the blob layout changes incompatibly; an older blob is then
/// handed back to the previous binary.
const BLOB_FORMAT: u32 = 11;

/// Per instance: a nested test instance must not replace the copy kept by
/// the session on the TTY. The pid survives the execs of an upgrade.
fn prev_binary() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join(format!("davide-wm-prev-{}", std::process::id())))
}

/// Set across the exec into the previous binary: if that cannot restore the
/// state either, falling back again would loop forever.
const FALLBACK_ENV: &str = "DAVIDE_WM_FALLBACK";

/// Keeps a copy of the running binary, to fall back to if a newer one
/// cannot restore the state.
fn keep_previous_binary() {
    let Some(prev) = prev_binary() else { return };
    let tmp = prev.with_extension("tmp");
    if std::fs::copy("/proc/self/exe", &tmp).and_then(|_| std::fs::rename(&tmp, &prev)).is_err() {
        log!("cannot keep a copy of the binary in {}", prev.display());
    }
}

fn encode(ctx: Ctx) -> state::Enc {
    use state::Persist;
    let Ctx { mut app, mut renderer, exe, .. } = ctx;
    // PipeWire streams cannot be handed over: casts end here.
    app.portal.close_all();
    let mut e = state::Enc::default();
    BLOB_FORMAT.save(&mut e);
    (app.session, app.main_mod, exe.to_string_lossy().into_owned()).save(&mut e);
    app.socket.save(&mut e);
    app.ipc.save(&mut e);
    let inhibitor = app.display.lid_inhibitor;
    inhibitor.save(&mut e);
    std::mem::forget(inhibitor);
    app.notifier.save(&mut e);
    std::mem::forget(app.notifier);
    app.xwayland.save(&mut e);
    std::mem::forget(app.xwayland);
    match app.backend {
        Backend::Nested(n) => {
            0u8.save(&mut e);
            let saved = n.save();
            saved.save(&mut e);
            std::mem::forget(saved);
        }
        Backend::Drm(d) => {
            1u8.save(&mut e);
            let saved = d.save();
            saved.save(&mut e);
            std::mem::forget(saved);
        }
    }
    app.state.save(&mut e, &mut renderer);
    // Their fds now belong to the blob.
    std::mem::forget(app.state);
    std::mem::forget(app.socket);
    std::mem::forget(app.ipc);
    e
}

fn decode(blob: &[u8]) -> io::Result<Ctx> {
    use state::Persist;
    let err = |e: state::Error| io::Error::other(e.to_string());
    let mut d = state::Dec::new(blob);
    let format = u32::load(&mut d).map_err(err)?;
    // Version 7 differs only by the input regions it lacks.
    server::persist::FORMAT.store(format, std::sync::atomic::Ordering::Relaxed);
    if !(7..=BLOB_FORMAT).contains(&format) {
        return Err(io::Error::other(format!("state blob format {format}, this binary reads {BLOB_FORMAT}")));
    }
    let (session, main_mod, exe): (bool, u32, String) = Persist::load(&mut d).map_err(err)?;
    let socket = Socket::load(&mut d).map_err(err)?;
    let ipc = ipc::Ipc::load(&mut d).map_err(err)?;
    let inhibitor: Option<OwnedFd> = Persist::load(&mut d).map_err(err)?;
    let notifier = notify::Notifier::load(&mut d).map_err(err)?;
    let xwayland: Option<xwayland::Xwayland> = Persist::load(&mut d).map_err(err)?;
    let (backend, mut renderer) = match u8::load(&mut d).map_err(err)? {
        0 => {
            let n = Nested::restore(Persist::load(&mut d).map_err(err)?)?;
            (Backend::Nested(n), Renderer::new(config::RENDER_NODE).map_err(io::Error::other)?)
        }
        _ => {
            let (drm, r) = Drm::restore(Persist::load(&mut d).map_err(err)?)?;
            (Backend::Drm(drm), r)
        }
    };
    let keyboard = keyboard::Keyboard::new(config::KEYBOARD_LAYOUT)?;
    let formats = renderer.dmabuf_formats();
    let mut state = State::restore(&mut d, keyboard, formats, &mut renderer).map_err(err)?;
    state.dmabuf_device = renderer.device();
    let mut app = new_app(backend, state, socket, ipc, main_mod, session, display::Display::new(inhibitor)?, notifier);
    app.xwayland = xwayland;
    // An older state may have brought the history in the old layout.
    app.clipboard_dirty = format < BLOB_FORMAT;
    app.outputs_changed();
    log!("live upgrade done");
    finish(app, renderer, PathBuf::from(exe))
}

fn exec_with_blob(exe: &std::path::Path, fd: i32) -> io::Error {
    use std::ffi::CString;
    let path = CString::new(exe.as_os_str().as_encoded_bytes()).unwrap_or_default();
    let args = [c"davide-wm".to_owned(), c"--resume".to_owned(), CString::new(fd.to_string()).unwrap()];
    let argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).chain([std::ptr::null()]).collect();
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    io::Error::last_os_error()
}

/// Replaces the running binary with the newly built one, keeping every
/// client connected. Returns only if the exec failed, with the state
/// restored in this process.
fn upgrade(ctx: Ctx) -> io::Result<Ctx> {
    let exe = ctx.exe.clone();
    log!("live upgrade to {}", exe.display());
    let e = encode(ctx);
    let blob = std::fs::File::from(memfd(&e.buf)?);
    let err = exec_with_blob(&exe, blob.as_raw_fd());
    log!("live upgrade failed: {err}; staying on the running binary");
    decode(&e.buf)
}

fn memfd(data: &[u8]) -> io::Result<OwnedFd> {
    use std::io::Write;
    let fd = unsafe { libc::memfd_create(c"davide-wm-state".as_ptr(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut f = std::fs::File::from(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    f.write_all(data)?;
    Ok(OwnedFd::from(f))
}

/// Started by a live upgrade: the state blob is in memfd `fd`.
fn resume(fd: i32) -> io::Result<Ctx> {
    use std::io::{Read, Seek};
    let mut f = std::fs::File::from(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
    }
    f.rewind()?;
    let mut blob = Vec::new();
    f.read_to_end(&mut blob)?;
    let fallen_back = std::env::var_os(FALLBACK_ENV).is_some();
    std::env::remove_var(FALLBACK_ENV);
    match decode(&blob) {
        Ok(ctx) => {
            if !fallen_back {
                keep_previous_binary();
            }
            Ok(ctx)
        }
        Err(e) => {
            log!("cannot restore the state: {e}");
            let Some(prev) = prev_binary().filter(|p| p.exists() && !fallen_back) else { return Err(e) };
            std::env::set_var(FALLBACK_ENV, "1");
            log!("falling back to {}", prev.display());
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
            let fd = f.as_raw_fd();
            std::mem::forget(f);
            Err(exec_with_blob(&prev, fd))
        }
    }
}

/// Runs until exit, or until a live upgrade is requested.
fn run_loop(ctx: &mut Ctx) -> io::Result<()> {
    let Ctx { app, renderer, ep, worker, .. } = ctx;
    while app.running && !app.upgrade {
        let timeout = if app.casts.catch_up { (1000 / config::CAST_FPS) as i32 } else { -1 };
        let ready = ep.wait(timeout)?;
        if app.casts.catch_up {
            app.state.needs_redraw = true;
        }
        for (token, events) in ready {
            let mut backend_events = Vec::new();
            match token {
                TOKEN_LISTENER => loop {
                    match app.socket.listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(true)?;
                            let fd = OwnedFd::from(stream);
                            let raw = fd.as_raw_fd();
                            let id = app.state.add_client(fd);
                            ep.add(raw, TOKEN_CLIENT + id as u64)?;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            log!("accept: {e}");
                            break;
                        }
                    }
                },
                TOKEN_IPC => {
                    while let Ok((stream, _)) = app.ipc.listener.accept() {
                        let answer = match ipc::read_command(&stream) {
                            Ok(ipc::Command::Screenshot(path)) => app.screenshot(&path, renderer),
                            Ok(cmd) => app.command(cmd),
                            Err(e) => format!("error: {e}\n"),
                        };
                        ipc::reply(&stream, &answer);
                    }
                }
                TOKEN_WORKER => {
                    for done in worker.results() {
                        match done {
                            Ok(worker::Done::Wallpaper(img)) => {
                                let tex = renderer.upload_mipmapped(img.width as i32, img.height as i32, &img.pixels);
                                app.state.wallpaper = Some(tex);
                                app.state.needs_redraw = true;
                            }
                            Ok(worker::Done::Clip { generation, clip }) => {
                                selection::captured(&mut app.state, generation, clip);
                                app.clipboard_dirty = true;
                            }
                            Ok(worker::Done::Saved) => {}
                            Ok(worker::Done::Unlock(ok)) => app.unlocked(ok),
                            Ok(worker::Done::Polkit(ok)) => app.polkit_checked(ok),
                            Ok(worker::Done::Networks(nets)) => app.open_wifi(nets),
                            Ok(worker::Done::Osd(text)) => app.show_osd(&text, None),
                            Ok(worker::Done::Bluetooth(p, d)) => app.open_bluetooth(p, d),
                            Ok(worker::Done::Cpu(label)) => {
                                app.notifier.local("CPU", &label, "", false);
                                app.sync_notices();
                            }
                            Ok(worker::Done::WifiConnect(r)) => match r {
                                Ok(ssid) => app.show_osd(&format!("Wi-Fi: {ssid}"), None),
                                Err(e) => {
                                    log!("wifi: {e}");
                                    app.show_osd("Wi-Fi: connessione non riuscita", None);
                                }
                            },
                            Ok(worker::Done::Screenshot { request, path, png }) => match request {
                                0 => {
                                    match &path {
                                        Ok(p) => log!("screenshot saved to {p}"),
                                        Err(e) => log!("screenshot not saved: {e}"),
                                    }
                                    selection::set_server(&mut app.state, clipboard::Clip { mime: "image/png".into(), data: png });
                                    app.clipboard_dirty = true;
                                }
                                r => app.portal.screenshot_done(r, path.ok().as_deref()),
                            },
                            Ok(worker::Done::XSelection { request, data }) => xwm::selection::answer(&mut app.state, request, data),
                            Ok(worker::Done::LinkCheck { name, failed }) => {
                                let lit = app.backend.heads().iter().any(|h| h.name == name);
                                if let (true, true, Backend::Drm(d)) = (failed, lit, &mut app.backend) {
                                    let events = d.link_failed(&name, renderer);
                                    app.layout_events(events);
                                }
                            }
                            Err(e) => log!("background job failed: {e}"),
                        }
                    }
                }
                TOKEN_BAR => {
                    let msgs = app.bar.as_ref().map(|b| b.recv()).unwrap_or_default();
                    for m in msgs {
                        app.bar_message(m, renderer);
                    }
                }
                TOKEN_IDLE => app.idle(),
                TOKEN_NOTIFY_BUS => {
                    for (app_name, entry) in app.notifier.dispatch() {
                        if let Some(bar) = &app.bar {
                            let name = if entry.is_empty() { app_name } else { entry };
                            bar.send(bar::ToBar::Notification { app: name });
                        }
                    }
                    app.sync_notices();
                }
                TOKEN_NOTIFY_TIMER => {
                    if app.notifier.expire() {
                        app.sync_notices();
                    }
                }
                TOKEN_PIPEWIRE => {
                    let mut wake = false;
                    for ev in app.casts.dispatch(&mut wake) {
                        match ev {
                            cast::Event::Ready { id, node, size } => {
                                log!("cast {id}: PipeWire node {node}");
                                app.portal.cast_ready(id, node, size);
                            }
                            cast::Event::Ended(id) => app.portal.cast_ended(id),
                        }
                    }
                    if wake {
                        app.state.needs_redraw = true;
                    }
                }
                TOKEN_AGENTS => {
                    let actions = app.agents.as_mut().map(|a| a.dispatch()).unwrap_or_default();
                    app.agent_actions(actions);
                }
                TOKEN_BATTERY => {
                    event_loop::read_timer(&app.battery_timer);
                    app.battery_tick();
                    app.xwayland_idle_check();
                    // Freed memory inside the heap goes back to the system.
                    unsafe { libc::malloc_trim(0) };
                }
                TOKEN_OSD => {
                    event_loop::read_timer(&app.osd_timer);
                    app.state.set_osd(None);
                }
                TOKEN_PORTAL => {
                    for a in app.portal.dispatch() {
                        app.portal_action(a);
                    }
                }
                TOKEN_XWAYLAND_READY => app.xwayland_ready(ep)?,
                TOKEN_XWAYLAND_EXIT => app.xwayland_exited(ep)?,
                TOKEN_XWAYLAND_LISTEN => app.start_xwayland(ep)?,
                TOKEN_XWM => app.xwm_dispatch(ep),
                TOKEN_SIGNAL => {
                    log!("signal received, exiting");
                    app.running = false;
                }
                TOKEN_HOST | TOKEN_KMS | TOKEN_INPUT | TOKEN_LOGIND | TOKEN_UEVENT => match (&mut app.backend, token) {
                    (Backend::Nested(n), _) => backend_events = n.dispatch()?,
                    (Backend::Drm(d), TOKEN_KMS) => {
                        d.dispatch_kms();
                    }
                    (Backend::Drm(d), TOKEN_INPUT) => backend_events = d.dispatch_input(),
                    (Backend::Drm(d), TOKEN_LOGIND) => {
                        backend_events = d.dispatch_logind()?;
                        app.outputs_changed();
                    }
                    (Backend::Drm(d), _) => {
                        if let Some(events) = d.dispatch_uevent(renderer) {
                            app.layout_events(events);
                        }
                    }
                },
                TOKEN_TERM => {
                    if let Some(Err(e)) = app.with_terms(|t, s| Some(t.dispatch(s))) {
                        log!("terminal: {e}");
                    }
                }
                TOKEN_TERM_TIMER => app.with_terms(|t, s| t.timer_fired(s)),
                t if t >= TOKEN_PASTE => app.with_terms(|term, _| term.paste_ready((t - TOKEN_PASTE) as RawFd)),
                t if t >= TOKEN_PTY => app.with_terms(|term, s| term.pty_ready(s, (t - TOKEN_PTY) as u32)),
                t => {
                    let id = (t - TOKEN_CLIENT) as ClientId;
                    if events & READABLE != 0 {
                        app.state.dispatch_client(id);
                    } else if events & HANGUP != 0 {
                        if let Some(c) = app.state.clients.get_mut(&id) {
                            c.dead = true;
                        }
                    }
                }
            }
            for e in backend_events {
                match (e, &mut app.backend) {
                    (Event::Lid(closed), Backend::Drm(d)) => {
                        log!("lid {}", if closed { "closed" } else { "open" });
                        let events = d.set_lid(closed, renderer);
                        app.layout_events(events);
                    }
                    (e, _) => app.backend_event(e),
                }
            }
        }
        for id in app.state.dead_clients() {
            if let Some(c) = app.state.clients.get(&id) {
                ep.delete(c.conn.raw_fd());
            }
            app.state.remove_client(id);
        }
        if app.state.pointer_dirty {
            seat::refresh_pointer(&mut app.state);
        }
        if let Backend::Drm(d) = &mut app.backend {
            if let Some((name, since)) = d.link_check.take() {
                worker.run(move || {
                    std::thread::sleep(std::time::Duration::from_millis(config::LINK_CHECK_DELAY_MS));
                    Ok(worker::Done::LinkCheck { failed: outputs::link_failed_since(since), name })
                });
            }
        }
        for (fd, generation, mime) in std::mem::take(&mut app.state.selection.captures) {
            worker.run(move || {
                let data = clipboard::capture(fd)?;
                Ok(worker::Done::Clip { generation, clip: clipboard::Clip { mime, data } })
            });
        }
        if let (false, Some(fd)) = (app.pipewire_watched, app.casts.fd()) {
            ep.add(fd, TOKEN_PIPEWIRE)?;
            app.pipewire_watched = true;
        }
        for (id, source) in app.casts.ids() {
            if let cast::Source::Window(k) = source {
                if !app.state.surfaces.contains_key(&k) {
                    app.casts.stop(id);
                    app.portal.cast_ended(id);
                }
            }
        }
        for p in std::mem::take(&mut app.state.abandoned) {
            if let Purpose::Cast(request, _) = p {
                app.portal.cancelled(request, false);
            }
        }
        if let Some((request, interactive)) = app.shot_request.take() {
            app.take_screenshot(renderer, request, interactive);
        }
        if let Some((user, cookie, password)) = app.polkit_job.take() {
            worker.run(move || Ok(worker::Done::Polkit(agents::polkit_check(&user, &cookie, password))));
        }
        for job in std::mem::take(&mut app.jobs) {
            worker.run(job);
        }
        if std::mem::take(&mut app.cpu_job) {
            worker.run(|| Ok(worker::Done::Cpu(cpu::cycle())));
        }
        match app.wifi_job.take() {
            Some(None) => worker.run(|| wifi::scan().map(worker::Done::Networks)),
            Some(Some(net)) => worker.run(move || Ok(worker::Done::WifiConnect(wifi::connect(&net)))),
            None => {}
        }
        if let Some(password) = app.unlock.take() {
            worker.run(move || Ok(worker::Done::Unlock(lock::authenticate(password))));
        }
        app.prompt_cards();
        if let Some(pct) = app.ddc.take() {
            worker.run(move || {
                brightness::set_external(pct);
                Ok(worker::Done::Saved)
            });
        }
        for (request, px, w, h) in std::mem::take(&mut app.shots) {
            worker.run(move || {
                let png = png::encode(w, h, &px);
                let path = save_screenshot(&png);
                Ok(worker::Done::Screenshot { request, path, png })
            });
        }
        if let Some(x) = &mut app.state.xwm {
            for (fd, request) in std::mem::take(&mut x.sel.outgoing) {
                worker.run(move || Ok(worker::Done::XSelection { request, data: clipboard::capture(fd) }));
            }
        }
        if std::mem::take(&mut app.clipboard_dirty) {
            let bytes = app.state.selection.history.encode();
            worker.run(move || clipboard::save(&bytes).map(|_| worker::Done::Saved).map_err(|e| format!("saving the clipboard history: {e}")));
        }
        if let Err(e) = xwm::flush(&mut app.state) {
            log!("X connection lost: {e}");
            app.xwm_stop(ep);
        }
        app.terminals(ep)?;
        app.update_bar();
        app.render(renderer)?;
        if std::mem::take(&mut app.sleep_after_frame) {
            app.sleep_inhibitor = None;
        }
        if let Backend::Nested(n) = &mut app.backend {
            n.flush()?;
        }
        for (id, pending) in app.state.flush_clients() {
            if let Some(c) = app.state.clients.get(&id) {
                ep.modify(c.conn.raw_fd(), TOKEN_CLIENT + id as u64, pending)?;
            }
        }
    }
    Ok(())
}

fn run(mut ctx: Ctx) -> io::Result<()> {
    loop {
        run_loop(&mut ctx)?;
        if !ctx.app.upgrade {
            return Ok(());
        }
        ctx = upgrade(ctx)?;
    }
}

fn main() {
    server::install_sigbus_handler();
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("msg") => ipc::client(&args[2..]),
        Some("outputs") => backend::describe_outputs().map(|s| print!("{s}")),
        Some("--resume") => match args.get(2).and_then(|f| f.parse().ok()) {
            Some(fd) => resume(fd).and_then(run),
            None => Err(io::Error::other("--resume needs the state fd")),
        },
        Some("--nested") => fresh(true, false).and_then(run),
        Some("--drm") => fresh(false, false).and_then(run),
        // The real session: also exports the environment to systemd and D-Bus.
        Some("--session") => fresh(false, true).and_then(run),
        None => fresh(std::env::var_os("WAYLAND_DISPLAY").is_some(), false).and_then(run),
        Some(other) => Err(io::Error::other(format!("unknown argument {other}; use msg, outputs, --nested, --drm or --session"))),
    };
    if let Some(prev) = prev_binary().filter(|_| !matches!(args.get(1).map(String::as_str), Some("msg" | "outputs"))) {
        let _ = std::fs::remove_file(prev);
    }
    if let Err(e) = result {
        log!("fatal: {e}");
        std::process::exit(1);
    }
}
