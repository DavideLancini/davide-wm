macro_rules! log {
    ($($t:tt)*) => { eprintln!("davide-wm: {}", format_args!($($t)*)) };
}

mod event_loop;
mod keyboard;
mod nested;
mod server;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use config::{mods, Action};
use event_loop::{Epoll, HANGUP, READABLE};
use render::Renderer;
use server::{seat, ClientId, State};

const TOKEN_LISTENER: u64 = 1;
const TOKEN_HOST: u64 = 2;
const TOKEN_SIGNAL: u64 = 3;
const TOKEN_CLIENT: u64 = 1 << 32;

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

impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn spawn(line: &str, socket: &str) {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(line).env("WAYLAND_DISPLAY", socket).env_remove("WAYLAND_SOCKET").stdin(Stdio::null());
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

struct App {
    state: State,
    socket: Socket,
    main_mod: u32,
    /// Keys that triggered a binding: their release is not forwarded.
    swallowed: Vec<u32>,
    running: bool,
}

impl App {
    fn key(&mut self, keycode: u32, pressed: bool) {
        let changed = self.state.keyboard.update(keycode, pressed);
        if pressed {
            let active = self.state.keyboard.bind_mods();
            let sym = self.state.keyboard.base_sym(keycode);
            let main = self.main_mod;
            let resolve = |m: u32| if m & mods::MAIN != 0 { (m & !mods::MAIN) | main } else { m };
            if let Some(b) = config::BINDS.iter().find(|b| resolve(b.mods) == active && Some(b.key) == sym) {
                self.swallowed.push(keycode);
                self.action(b.action);
                return;
            }
        } else if let Some(i) = self.swallowed.iter().position(|k| *k == keycode) {
            self.swallowed.remove(i);
            return;
        }
        seat::key(&mut self.state, keycode, pressed, changed);
    }

    fn action(&mut self, action: Action) {
        match action {
            Action::Spawn(line) => spawn(line, &self.socket.name),
            Action::CloseWindow => {
                if let Some(f) = self.state.focus {
                    self.state.close_window(f);
                }
            }
            Action::Focus(dir) => {
                if let Some(next) = self.state.focus.and_then(|f| wm::neighbor(&self.state.layout, f, dir)) {
                    seat::set_focus(&mut self.state, Some(next));
                }
            }
            Action::Exit => self.running = false,
        }
    }

    fn backend_event(&mut self, e: nested::Event) {
        match e {
            nested::Event::Resize(w, h) => self.state.set_output_size(w, h),
            nested::Event::Close => self.running = false,
            nested::Event::PointerMotion(x, y) => seat::pointer_motion(&mut self.state, x, y),
            nested::Event::PointerButton(b, p) => seat::pointer_button(&mut self.state, b, p),
            nested::Event::PointerAxis(a, v) => seat::pointer_axis(&mut self.state, a, v),
            nested::Event::Key(k, p) => self.key(k, p),
            nested::Event::KeyboardLeave => {
                self.state.keyboard.reset();
                self.swallowed.clear();
            }
        }
    }
}

fn run() -> io::Result<()> {
    let signals = event_loop::signal_fd()?;
    let mut renderer = Renderer::new(config::RENDER_NODE).map_err(io::Error::other)?;
    let mut host = nested::Nested::connect()?;
    let keyboard = keyboard::Keyboard::new(config::KEYBOARD_LAYOUT)?;
    let (w, h) = host.size;
    let output = server::Output { name: "NESTED-1".into(), width: w, height: h };
    let socket = Socket::bind()?;
    log!("listening on {}", socket.name);

    let ep = Epoll::new()?;
    ep.add(socket.listener.as_raw_fd(), TOKEN_LISTENER)?;
    ep.add(host.raw_fd(), TOKEN_HOST)?;
    ep.add(signals.as_raw_fd(), TOKEN_SIGNAL)?;

    let mut app = App { state: State::new(keyboard, output), socket, main_mod: mods::ALT, swallowed: Vec::new(), running: true };
    while app.running {
        for (token, events) in ep.wait(-1)? {
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
                TOKEN_HOST => {
                    for e in host.dispatch()? {
                        app.backend_event(e);
                    }
                }
                TOKEN_SIGNAL => {
                    log!("signal received, exiting");
                    app.running = false;
                }
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
        }
        for id in app.state.dead_clients() {
            if let Some(c) = app.state.clients.get(&id) {
                ep.delete(c.conn.raw_fd());
            }
            app.state.remove_client(id);
        }
        if app.state.needs_redraw {
            if let Some(i) = host.acquire(&mut renderer)? {
                app.state.render(&mut renderer, host.target(i));
                host.present(i);
            }
        }
        host.flush()?;
        for (id, pending) in app.state.flush_clients() {
            if let Some(c) = app.state.clients.get(&id) {
                ep.modify(c.conn.raw_fd(), TOKEN_CLIENT + id as u64, pending)?;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        log!("fatal: {e}");
        std::process::exit(1);
    }
}
