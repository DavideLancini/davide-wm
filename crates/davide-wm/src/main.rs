macro_rules! log {
    ($($t:tt)*) => { eprintln!("davide-wm: {}", format_args!($($t)*)) };
}

mod backend;
mod display;
mod event_loop;
mod ipc;
mod keyboard;
mod notify;
mod png;
mod server;
mod worker;
mod xwayland;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use config::{mods, Action};
use event_loop::{Epoll, HANGUP, READABLE};
use backend::{nested::Nested, outputs, Drm, Event};
use render::{Renderer, Target};
use server::{clipboard, seat, selection, workspaces, xwm, ClientId, Purpose, State};

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
}

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
            Action::Exit => self.running = false,
        }
    }

    fn open_clipboard(&mut self) {
        let entries = &self.state.selection.history.entries;
        let labels = entries.iter().map(clipboard::label).collect();
        let hashes = entries.iter().map(clip_hash).collect();
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
        self.state.selection.history.entries.iter().position(|c| clip_hash(c) == *h)
    }

    /// Enter in a picker.
    fn picked(&mut self, purpose: Purpose, i: usize) {
        match purpose {
            Purpose::Apps => {}
            Purpose::Clipboard(hashes) => {
                if let Some(clip) = self.clip_index(&hashes, i).map(|j| self.state.selection.history.entries[j].clone()) {
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
            Purpose::Apps => {}
            Purpose::Clipboard(hashes) => {
                if let Some(j) = self.clip_index(&hashes, i) {
                    self.state.selection.history.entries.remove(j);
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
            ipc::Command::Focused => {
                let s = &self.state;
                return match s.focus {
                    Some(k) => format!("{} on {}\n", s.app_id(k), s.window_ws.get(&k).map_or("?".into(), |w| ipc::ws_name(*w))),
                    None => "none\n".into(),
                };
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
            if let Err(e) = d.set_blank(true) {
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
            if let Err(e) = d.set_blank(false) {
                log!("wake: {e}");
            }
        }
        self.last_frame = None;
        self.state.needs_redraw = true;
    }

    /// The idle timer fired.
    fn idle(&mut self) {
        if !self.display.timer_fired() {
            return;
        }
        if self.state.idle_inhibited() {
            self.display.activity();
            return;
        }
        log!("idle for {}s", self.display.screensaver.timeout);
        self.blank();
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
        self.mirrors_stale.clear();
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
    fn render(&mut self, r: &mut Renderer) -> io::Result<()> {
        let heads = self.backend.heads().len();
        if self.display.blanked {
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
        if self.state.needs_redraw {
            if let Some(i) = self.backend.acquire(0, r)? {
                self.state.render(r, self.backend.target(0, i));
                self.last_frame = Some(i);
                self.mirrors_stale = (1..heads).collect();
                self.present(0, i, r)?;
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
    }
}

impl App {
    fn start_xwayland(&mut self) {
        match xwayland::spawn() {
            Ok(sp) => {
                let id = self.state.add_client(sp.wayland);
                self.state.xwayland = Some(id);
                self.xwayland = Some(sp.xwayland);
            }
            Err(e) => log!("cannot start Xwayland: {e}"),
        }
    }

    /// Watches Xwayland's fds: its exit, its readiness, the WM connection.
    fn watch_xwayland(&self, ep: &Epoll) -> io::Result<()> {
        if let Some(x) = &self.xwayland {
            ep.add(x.pidfd.as_raw_fd(), TOKEN_XWAYLAND_EXIT)?;
            if let Some((ready, _)) = &x.pending {
                ep.add(ready.as_raw_fd(), TOKEN_XWAYLAND_READY)?;
            }
        }
        if let Some(x) = &self.state.xwm {
            ep.add(x.conn.raw_fd(), TOKEN_XWM)?;
        }
        Ok(())
    }

    fn xwayland_ready(&mut self, ep: &Epoll) -> io::Result<()> {
        let Some(wm) = self.xwayland.as_mut().and_then(|x| x.ready()) else { return Ok(()) };
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

    fn xwayland_exited(&mut self, ep: &Epoll) -> io::Result<()> {
        let Some(x) = self.xwayland.take() else { return Ok(()) };
        log!("Xwayland exited");
        self.xwm_stop(ep);
        ep.delete(x.pidfd.as_raw_fd());
        let again = x.lived();
        drop(x);
        if again {
            self.start_xwayland();
            if let Some(id) = self.state.xwayland {
                ep.add(self.state.clients[&id].conn.raw_fd(), TOKEN_CLIENT + id as u64)?;
            }
            self.watch_xwayland(ep)?;
            if let Some(x) = &self.xwayland {
                std::env::set_var("DISPLAY", format!(":{}", x.display));
            }
        }
        Ok(())
    }
}

fn clip_hash(c: &clipboard::Clip) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    c.data.hash(&mut h);
    h.finish()
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
    app.bar = Some(bar);
    for (id, fd) in app.state.client_fds() {
        ep.add(fd, TOKEN_CLIENT + id as u64)?;
    }
    app.watch_xwayland(&ep)?;
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
    app.start_xwayland();
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
const BLOB_FORMAT: u32 = 5;

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
    let Ctx { app, mut renderer, exe, .. } = ctx;
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
    if format != BLOB_FORMAT {
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
        for (token, events) in ep.wait(-1)? {
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
                TOKEN_XWAYLAND_READY => app.xwayland_ready(ep)?,
                TOKEN_XWAYLAND_EXIT => app.xwayland_exited(ep)?,
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
        app.update_bar();
        app.render(renderer)?;
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
