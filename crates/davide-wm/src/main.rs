macro_rules! log {
    ($($t:tt)*) => { eprintln!("davide-wm: {}", format_args!($($t)*)) };
}

mod backend;
mod event_loop;
mod ipc;
mod keyboard;
mod server;
mod worker;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use config::{mods, Action};
use event_loop::{Epoll, HANGUP, READABLE};
use backend::{nested::Nested, Drm, Event};
use render::{Renderer, Target};
use server::{seat, workspaces, ClientId, State};

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
    fn acquire(&mut self, r: &mut Renderer) -> io::Result<Option<usize>> {
        match self {
            Backend::Nested(n) => n.acquire(r),
            Backend::Drm(d) => Ok(d.acquire()),
        }
    }

    fn target(&self, idx: usize) -> &Target {
        match self {
            Backend::Nested(n) => n.target(idx),
            Backend::Drm(d) => d.target(idx),
        }
    }

    fn present(&mut self, idx: usize) -> io::Result<()> {
        match self {
            Backend::Nested(n) => {
                n.present(idx);
                Ok(())
            }
            Backend::Drm(d) => d.present(idx),
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
    running: bool,
    upgrade: bool,
    /// Exports the environment to systemd (`--session`).
    session: bool,
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
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                event_loop::reset_signals_in_child();
                Ok(())
            });
        }
        if let Err(e) = cmd.spawn() {
            log!("cannot run {line:?}: {e}");
        }
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        let changed = self.state.keyboard.update(keycode, pressed);
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
            }
            Action::SwitchVt(vt) => {
                if let Backend::Drm(d) = &mut self.backend {
                    d.switch_vt(vt);
                }
            }
            Action::Upgrade => self.upgrade = true,
            Action::Exit => self.running = false,
        }
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
                    let title = match s.surfaces.get(key).and_then(|sf| sf.xdg.as_ref()).map(|x| &x.role) {
                        Some(server::XdgRole::Toplevel { title, .. }) => title.as_str(),
                        _ => "",
                    };
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
        let snapshot = self.state.bar_snapshot();
        if self.state.bar.sent.as_ref() != Some(&snapshot) {
            bar.send(bar::ToBar::Workspaces(snapshot.0.clone()));
            bar.send(bar::ToBar::Clients(snapshot.1.clone()));
            self.state.bar.sent = Some(snapshot);
        }
    }

    fn backend_event(&mut self, e: Event) {
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
            // Lid handling (blank, lid-action) arrives with M4.
            Event::Lid(closed) => log!("lid {}", if closed { "closed" } else { "open" }),
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

fn new_app(backend: Backend, state: State, socket: Socket, ipc: ipc::Ipc, main_mod: u32, session: bool) -> App {
    App { backend, bar: None, bar_width: 0, state, socket, ipc, binds: config::binds(), main_mod, swallowed: Vec::new(), running: true, upgrade: false, session }
}

/// Registers every fd with a fresh epoll and starts background jobs.
fn finish(mut app: App, renderer: Renderer, exe: PathBuf) -> io::Result<Ctx> {
    // Programs started by the bar inherit these.
    std::env::set_var("WAYLAND_DISPLAY", &app.socket.name);
    std::env::set_var(ipc::ENV, &app.ipc.path);
    std::env::set_var("XDG_CURRENT_DESKTOP", "davide-wm");
    std::env::remove_var("WAYLAND_SOCKET");
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
    app.bar = Some(bar);
    for (id, fd) in app.state.client_fds() {
        ep.add(fd, TOKEN_CLIENT + id as u64)?;
    }
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
    let mut app = new_app(backend, state, socket, ipc, main_mod, session);
    if let Backend::Drm(d) = &mut app.backend {
        for e in d.setup_output(&mut renderer)? {
            app.backend_event(e);
        }
        app.state.pointer.x = (app.state.output.width / 2) as f64;
        app.state.pointer.y = (app.state.output.height / 2) as f64;
    }
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
const BLOB_FORMAT: u32 = 2;

fn prev_binary() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("davide-wm-prev"))
}

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
    let state = State::restore(&mut d, keyboard, formats, &mut renderer).map_err(err)?;
    let app = new_app(backend, state, socket, ipc, main_mod, session);
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
    match decode(&blob) {
        Ok(ctx) => Ok(ctx),
        Err(e) => {
            log!("cannot restore the state: {e}");
            let Some(prev) = prev_binary().filter(|p| p.exists()) else { return Err(e) };
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
                        app.state.needs_redraw = true;
                    }
                    (Backend::Drm(d), _) => backend_events = d.dispatch_uevent(renderer)?,
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
                app.backend_event(e);
            }
        }
        for id in app.state.dead_clients() {
            if let Some(c) = app.state.clients.get(&id) {
                ep.delete(c.conn.raw_fd());
            }
            app.state.remove_client(id);
        }
        app.update_bar();
        if app.state.needs_redraw {
            if let Some(i) = app.backend.acquire(renderer)? {
                app.state.render(renderer, app.backend.target(i));
                if let Err(e) = app.backend.present(i) {
                    if e.to_string().starts_with("modeset failed") {
                        return Err(e);
                    }
                    // Expected while switching VT: logind has already revoked DRM master.
                    log!("present failed: {e}");
                }
            }
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
    if let Err(e) = result {
        log!("fatal: {e}");
        std::process::exit(1);
    }
}
