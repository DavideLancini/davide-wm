//! The built-in terminal: one in-process Wayland client (like foot's
//! server) whose windows are drawn by the compositor straight from the
//! terminal cells, with no pixel buffers of their own.
//!
//! The client side talks to our own server over a socket pair, so windows,
//! focus, workspaces and the clipboard work as for any other program. Only
//! the content is special: instead of attaching buffers, a configured window
//! gets `Content::Term` and `State::draw_tree` asks `Terms::draw` for it.
//! Keys are handed over directly by the key handler (the compositor's own
//! keymap state), pointer and selection events arrive over the wire.

pub mod glyphs;
pub mod pty;

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use render::{Batch, Quad, Renderer};
use term::{attr, MouseAction, MouseButton, MouseEvent, Point, Rgb, SelectionMode, Terminal};
use wire::*;

use crate::server::{selection, workspaces, ClientId, SKey, State};

/// How long a synchronized update (mode 2026) may hold the screen.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Bytes read from one terminal per loop pass, so a flood cannot stall the
/// compositor.
const READ_BUDGET: usize = 256 << 10;

/// A terminal window to open: where, and what to run instead of a shell.
#[derive(Clone, Default)]
pub struct Open {
    pub cwd: Option<String>,
    pub cmd: Option<String>,
    /// After `cmd`, the window stays open on a shell.
    pub hold: bool,
}

pub struct Window {
    pub surface: u32,
    xdg: u32,
    toplevel: u32,
    pty: pty::Pty,
    engine: Terminal,
    /// Window size in pixels, and the one of the configure being acked.
    size: (i32, i32),
    pending: (i32, i32),
    title: String,
    /// Output held back by a synchronized update, since when.
    held: Option<Instant>,
    /// Bytes for the shell that did not fit in the pty yet.
    out: Vec<u8>,
}

/// The mouse over a terminal: position in cells, buttons, last click (for
/// double and triple clicks) and a selection being dragged.
#[derive(Default)]
struct Mouse {
    surface: u32,
    pos: (f64, f64),
    clicks: (u32, Option<Instant>),
    /// Where a selection being dragged started.
    anchor: Option<(Point, SelectionMode)>,
    buttons: u32,
}

pub struct Terms {
    conn: Connection,
    /// Our id as a client of the server.
    pub client: ClientId,
    objects: HashMap<u32, &'static Interface>,
    next_id: u32,
    compositor: u32,
    wm_base: u32,
    seat: u32,
    pointer: u32,
    data_device: u32,
    data_manager: u32,
    /// Offers of the data device: id and types.
    offers: HashMap<u32, Vec<String>>,
    clipboard: Option<u32>,
    pub windows: Vec<Window>,
    /// Windows asked for before the globals were bound: working directory.
    queued: Vec<Open>,
    mouse: Mouse,
    /// Pastes being read: pipe, window.
    pastes: Vec<(OwnedFd, u32, Vec<u8>)>,
    /// Key repeat: key code, next time.
    repeat: Option<(u32, Instant)>,
    pub timer: OwnedFd,
    /// Fds of closed windows and pastes, kept open until the event loop
    /// stopped watching them (their numbers must not be reused before).
    pub graveyard: Vec<OwnedFd>,
    glyphs: RefCell<Option<glyphs::Glyphs>>,
    batch: RefCell<Batch>,
    row: RefCell<Vec<term::Cell>>,
}

/// Fds the event loop watches for the terminals.
pub enum Fd {
    Client,
    Pty(u32),
    Paste,
    Timer,
}

impl Terms {
    /// Connects a new terminal client to the server.
    pub fn start(s: &mut State) -> io::Result<Terms> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        theirs.set_nonblocking(true)?;
        let client = s.add_client(OwnedFd::from(theirs));
        let mut objects = HashMap::new();
        objects.insert(1, &wl_display::INTERFACE);
        let mut t = Terms {
            conn: Connection::new(OwnedFd::from(ours)),
            client,
            objects,
            next_id: 2,
            compositor: 0,
            wm_base: 0,
            seat: 0,
            pointer: 0,
            data_device: 0,
            data_manager: 0,
            offers: HashMap::new(),
            clipboard: None,
            windows: Vec::new(),
            queued: Vec::new(),
            mouse: Mouse::default(),
            pastes: Vec::new(),
            repeat: None,
            timer: crate::event_loop::timer_fd()?,
            graveyard: Vec::new(),
            glyphs: RefCell::new(None),
            batch: RefCell::new(Batch::default()),
            row: RefCell::new(Vec::new()),
        };
        let registry = t.new_id(&wl_registry::INTERFACE);
        t.send(Msg::new(1, wl_display::req::GET_REGISTRY).obj(registry));
        t.flush();
        Ok(t)
    }

    fn new_id(&mut self, iface: &'static Interface) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, iface);
        id
    }

    fn send(&mut self, m: Msg) {
        self.conn.send(m);
    }

    pub fn flush(&mut self) {
        let _ = self.conn.flush();
    }

    pub fn fds(&self) -> Vec<(RawFd, Fd)> {
        let mut v = vec![(self.conn.raw_fd(), Fd::Client), (self.timer.as_raw_fd(), Fd::Timer)];
        v.extend(self.windows.iter().map(|w| (w.pty.master.as_raw_fd(), Fd::Pty(w.surface))));
        v.extend(self.pastes.iter().map(|(fd, ..)| (fd.as_raw_fd(), Fd::Paste)));
        v
    }

    /// Per window: scrollback lines and heap bytes of the engine; then the
    /// glyph cache entries.
    pub fn usage(&self) -> (Vec<(usize, usize)>, usize) {
        let windows = self.windows.iter().map(|w| (w.engine.scrollback_len(), w.engine.heap_bytes() + w.out.capacity())).collect();
        (windows, self.glyphs.borrow().as_ref().map_or(0, |g| g.cached()))
    }

    pub fn is_ours(&self, key: SKey) -> bool {
        key.client == self.client && self.windows.iter().any(|w| w.surface == key.id)
    }

    fn cell(&self) -> (i32, i32) {
        let mut g = self.glyphs.borrow_mut();
        if g.is_none() {
            *g = glyphs::Glyphs::new();
        }
        g.as_ref().map_or((10, 20), |g| g.cell)
    }

    fn grid(&self, size: (i32, i32)) -> (u16, u16) {
        let (cw, ch) = self.cell();
        let cols = ((size.0 - 2 * config::TERM_PAD) / cw).max(2);
        let rows = ((size.1 - 2 * config::TERM_PAD) / ch).max(1);
        (cols as u16, rows as u16)
    }

    /// A new window, its shell (or command) started in `o.cwd`.
    pub fn open(&mut self, o: Open) {
        if self.compositor == 0 || self.wm_base == 0 {
            self.queued.push(o);
            return;
        }
        let (cols, rows) = (80, 24);
        let pty = match pty::spawn(cols, rows, self.cell(), &o) {
            Ok(p) => p,
            Err(e) => {
                log!("terminal: cannot start the shell: {e}");
                return;
            }
        };
        let surface = self.new_id(&wl_surface::INTERFACE);
        self.send(Msg::new(self.compositor, wl_compositor::req::CREATE_SURFACE).obj(surface));
        let xdg = self.new_id(&xdg_surface::INTERFACE);
        self.send(Msg::new(self.wm_base, xdg_wm_base::req::GET_XDG_SURFACE).obj(xdg).obj(surface));
        let toplevel = self.new_id(&xdg_toplevel::INTERFACE);
        self.send(Msg::new(xdg, xdg_surface::req::GET_TOPLEVEL).obj(toplevel));
        self.send(Msg::new(toplevel, xdg_toplevel::req::SET_APP_ID).str(config::TERM_APP_ID));
        self.send(Msg::new(toplevel, xdg_toplevel::req::SET_TITLE).str("terminal"));
        self.send(Msg::new(surface, wl_surface::req::COMMIT));
        let mut engine = Terminal::new(cols as usize, rows as usize, config::TERM_SCROLLBACK, theme());
        let cell = self.cell();
        engine.set_cell_size(cell.0 as u16, cell.1 as u16);
        self.windows.push(Window { surface, xdg, toplevel, pty, engine, size: (0, 0), pending: (0, 0), title: String::new(), held: None, out: Vec::new() });
        self.flush();
    }

    /// The working directory of the focused terminal, for a new one.
    pub fn cwd_of(&self, surface: u32) -> Option<String> {
        self.windows.iter().find(|w| w.surface == surface).and_then(|w| pty::cwd(w.pty.pid))
    }

    /// Reads and handles the server's events.
    pub fn dispatch(&mut self, s: &mut State) -> io::Result<()> {
        loop {
            match self.conn.fill() {
                Ok(0) => return Err(io::Error::other("the server closed the terminal connection")),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        while let Some(h) = self.conn.peek() {
            let Some(iface) = self.objects.get(&h.object).copied() else {
                self.conn.skip(&h);
                continue;
            };
            let Some(desc) = iface.events.get(h.opcode as usize) else {
                self.conn.skip(&h);
                continue;
            };
            let args = self.conn.decode(&h, desc).map_err(|e| io::Error::other(format!("terminal {iface:?}: {e}")))?;
            self.event(s, h.object, iface, h.opcode, args);
        }
        self.flush();
        Ok(())
    }

    fn event(&mut self, s: &mut State, id: u32, iface: &'static Interface, op: u16, args: Args) {
        match (iface.name, op) {
            ("wl_display", wl_display::evt::ERROR) => log!("terminal: protocol error on {}: {}", args.obj(0), args.str(2).unwrap_or("")),
            ("wl_display", wl_display::evt::DELETE_ID) => {
                self.objects.remove(&args.uint(0));
            }
            ("wl_registry", wl_registry::evt::GLOBAL) => {
                let (name, iface_name, version) = (args.uint(0), args.str(1).unwrap_or("").to_string(), args.uint(2));
                let want: Option<(&'static Interface, u32)> = match iface_name.as_str() {
                    "wl_compositor" => Some((&wl_compositor::INTERFACE, 4)),
                    "xdg_wm_base" => Some((&xdg_wm_base::INTERFACE, 1)),
                    "wl_seat" => Some((&wl_seat::INTERFACE, 5)),
                    "wl_data_device_manager" => Some((&wl_data_device_manager::INTERFACE, 3)),
                    _ => None,
                };
                if let Some((i, v)) = want {
                    let nid = self.new_id(i);
                    self.send(Msg::new(id, wl_registry::req::BIND).uint(name).str(i.name).uint(v.min(version)).obj(nid));
                    match i.name {
                        "wl_compositor" => self.compositor = nid,
                        "xdg_wm_base" => self.wm_base = nid,
                        "wl_seat" => {
                            self.seat = nid;
                            self.pointer = self.new_id(&wl_pointer::INTERFACE);
                            self.send(Msg::new(nid, wl_seat::req::GET_POINTER).obj(self.pointer));
                        }
                        _ => self.data_manager = nid,
                    }
                    if self.data_manager != 0 && self.seat != 0 && self.data_device == 0 {
                        self.data_device = self.new_id(&wl_data_device::INTERFACE);
                        self.send(Msg::new(self.data_manager, wl_data_device_manager::req::GET_DATA_DEVICE).obj(self.data_device).obj(self.seat));
                    }
                    if self.compositor != 0 && self.wm_base != 0 {
                        for o in std::mem::take(&mut self.queued) {
                            self.open(o);
                        }
                    }
                }
            }
            ("xdg_wm_base", xdg_wm_base::evt::PING) => self.send(Msg::new(id, xdg_wm_base::req::PONG).uint(args.uint(0))),
            ("xdg_toplevel", xdg_toplevel::evt::CONFIGURE) => {
                if let Some(w) = self.windows.iter_mut().find(|w| w.toplevel == id) {
                    w.pending = (args.int(0), args.int(1));
                }
            }
            ("xdg_toplevel", xdg_toplevel::evt::CLOSE) => {
                if let Some(w) = self.windows.iter().find(|w| w.toplevel == id) {
                    pty::hang_up(w.pty.pid);
                }
            }
            ("xdg_surface", xdg_surface::evt::CONFIGURE) => {
                let serial = args.uint(0);
                let Some(i) = self.windows.iter().position(|w| w.xdg == id) else { return };
                let (mut w, mut h) = self.windows[i].pending;
                if w <= 0 || h <= 0 {
                    let (cw, ch) = self.cell();
                    (w, h) = (80 * cw + 2 * config::TERM_PAD, 24 * ch + 2 * config::TERM_PAD);
                }
                let (cols, rows) = self.grid((w, h));
                let cell = self.cell();
                let win = &mut self.windows[i];
                if win.size != (w, h) {
                    win.size = (w, h);
                    win.engine.resize(cols as usize, rows as usize);
                    let _ = pty::resize(win.pty.master.as_raw_fd(), cols, rows, cell);
                }
                let (surface, xdg) = (win.surface, win.xdg);
                s.set_term_content(SKey { client: self.client, id: surface }, w, h);
                self.send(Msg::new(xdg, xdg_surface::req::ACK_CONFIGURE).uint(serial));
                self.send(Msg::new(surface, wl_surface::req::COMMIT));
            }
            ("wl_pointer", wl_pointer::evt::ENTER) => {
                self.mouse.surface = args.obj(1);
                self.mouse.pos = (args.fixed(2), args.fixed(3));
            }
            ("wl_pointer", wl_pointer::evt::LEAVE) => {
                self.mouse.surface = 0;
                self.mouse.buttons = 0;
            }
            ("wl_pointer", wl_pointer::evt::MOTION) => {
                self.mouse.pos = (args.fixed(1), args.fixed(2));
                self.pointer_motion(s);
            }
            ("wl_pointer", wl_pointer::evt::BUTTON) => {
                let (button, pressed) = (args.uint(2), args.uint(3) == wl_pointer::button_state::PRESSED);
                self.pointer_button(s, button, pressed);
            }
            ("wl_pointer", wl_pointer::evt::AXIS) => {
                if args.uint(1) == wl_pointer::axis::VERTICAL_SCROLL {
                    self.wheel(s, args.fixed(2));
                }
            }
            ("wl_data_device", wl_data_device::evt::DATA_OFFER) => {
                let offer = args.obj(0);
                self.objects.insert(offer, &wl_data_offer::INTERFACE);
                self.offers.insert(offer, Vec::new());
            }
            ("wl_data_offer", wl_data_offer::evt::OFFER) => {
                if let Some(m) = self.offers.get_mut(&id) {
                    m.push(args.str(0).unwrap_or("").to_string());
                }
            }
            ("wl_data_device", wl_data_device::evt::SELECTION) => {
                let offer = args.obj(0);
                if let Some(old) = self.clipboard.take().filter(|o| *o != offer) {
                    self.offers.remove(&old);
                    self.send(Msg::new(old, wl_data_offer::req::DESTROY));
                }
                self.clipboard = (offer != 0).then_some(offer);
            }
            _ => {}
        }
    }

    // ---- pty -------------------------------------------------------------

    /// Output from a shell: fed to its terminal, the changed rows redrawn.
    pub fn pty_ready(&mut self, s: &mut State, surface: u32) {
        let Some(i) = self.windows.iter().position(|w| w.surface == surface) else { return };
        let mut buf = [0u8; 64 << 10];
        let mut total = 0;
        let mut closed = false;
        let fd = self.windows[i].pty.master.as_raw_fd();
        while total < READ_BUDGET {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                // A bug in the engine must not take the compositor down.
                let engine = &mut self.windows[i].engine;
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.feed(&buf[..n as usize]))).is_err() {
                    log!("terminal: the engine failed; closing the window");
                    closed = true;
                    break;
                }
                total += n as usize;
                continue;
            }
            if n == 0 || io::Error::last_os_error().raw_os_error().is_some_and(|e| e != libc::EAGAIN && e != libc::EINTR) {
                closed = true;
            }
            break;
        }
        if closed {
            self.close(s, i);
            return;
        }
        self.after_output(s, i);
    }

    /// Replies, title, bell, clipboard and the redraw after the terminal changed.
    fn after_output(&mut self, s: &mut State, i: usize) {
        let client = self.client;
        let w = &mut self.windows[i];
        let replies = w.engine.take_responses();
        if !replies.is_empty() {
            w.out.extend_from_slice(&replies);
        }
        write_out(w);
        if w.engine.take_title_changed() && w.engine.title() != w.title {
            w.title = w.engine.title().to_string();
            let (toplevel, t) = (w.toplevel, w.title.clone());
            self.send(Msg::new(toplevel, xdg_toplevel::req::SET_TITLE).str(&t));
        }
        let w = &mut self.windows[i];
        let key = SKey { client, id: w.surface };
        if w.engine.take_bell() > 0 && s.focus != Some(key) {
            workspaces::mark_urgent(s, key);
        }
        if let Some(text) = w.engine.take_clipboard() {
            selection::set_server(s, crate::server::clipboard::Clip { mime: "text/plain;charset=utf-8".into(), data: text.into_bytes() });
        }
        self.redraw(s, i);
        self.flush();
    }

    /// Damages what changed on screen, unless a synchronized update holds it.
    fn redraw(&mut self, s: &mut State, i: usize) {
        let cell = self.cell();
        let client = self.client;
        let w = &mut self.windows[i];
        if w.engine.sync() {
            if w.held.is_none() {
                w.held = Some(Instant::now());
                crate::event_loop::arm_timer(&self.timer, SYNC_TIMEOUT);
            }
            if w.held.is_some_and(|t| t.elapsed() < SYNC_TIMEOUT) {
                return;
            }
        }
        w.held = None;
        let key = SKey { client, id: w.surface };
        let damage = w.engine.take_damage();
        if damage.is_empty() {
            return;
        }
        let rows: Vec<usize> = if damage.full { Vec::new() } else { damage.rows().collect() };
        // Runs of dirty rows become one rectangle each.
        let mut areas: Vec<wm::Rect> = Vec::new();
        for y in rows {
            let top = config::TERM_PAD + y as i32 * cell.1;
            match areas.last_mut() {
                Some(a) if a.y + a.h == top => a.h += cell.1,
                _ => areas.push(wm::Rect::new(0, top, w.size.0, cell.1)),
            }
        }
        s.damage_committed(key, &areas);
    }

    fn close(&mut self, s: &mut State, i: usize) {
        let w = self.windows.remove(i);
        pty::hang_up(w.pty.pid);
        self.graveyard.push(w.pty.master);
        self.send(Msg::new(w.toplevel, xdg_toplevel::req::DESTROY));
        self.send(Msg::new(w.xdg, xdg_surface::req::DESTROY));
        self.send(Msg::new(w.surface, wl_surface::req::DESTROY));
        self.flush();
        if s.focus.is_some_and(|f| f.client == self.client && f.id == w.surface) {
            self.repeat = None;
        }
    }

    pub fn paste_ready(&mut self, fd: RawFd) {
        let Some(i) = self.pastes.iter().position(|(p, ..)| p.as_raw_fd() == fd) else { return };
        let mut buf = [0u8; 16 << 10];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.pastes[i].2.extend_from_slice(&buf[..n as usize]);
                continue;
            }
            if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                return;
            }
            break;
        }
        let (pipe, surface, data) = self.pastes.remove(i);
        self.graveyard.push(pipe);
        if let Some(w) = self.windows.iter_mut().find(|w| w.surface == surface) {
            let text = String::from_utf8_lossy(&data);
            let bytes = w.engine.encode_paste(&text);
            w.out.extend_from_slice(&bytes);
            write_out(w);
        }
    }

    /// Key repeat and the end of held synchronized updates.
    pub fn timer_fired(&mut self, s: &mut State) {
        crate::event_loop::read_timer(&self.timer);
        for i in 0..self.windows.len() {
            if self.windows[i].held.is_some() {
                self.redraw(s, i);
            }
        }
        if let Some((code, at)) = self.repeat {
            if Instant::now() >= at {
                if let Some(f) = s.focus.filter(|f| self.is_ours(*f)) {
                    self.key_press(s, f.id, code);
                }
                self.repeat = Some((code, Instant::now() + Duration::from_millis(1000 / config::REPEAT_RATE.max(1) as u64)));
            }
        }
        self.arm();
    }

    fn arm(&self) {
        let mut next: Option<Instant> = self.repeat.map(|(_, t)| t);
        for w in &self.windows {
            if let Some(h) = w.held {
                let t = h + SYNC_TIMEOUT;
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        }
        if let Some(t) = next {
            crate::event_loop::arm_timer(&self.timer, t.saturating_duration_since(Instant::now()));
        }
    }

    // ---- keyboard --------------------------------------------------------

    /// A key on a focused terminal window; the compositor's bindings have
    /// had their go already.
    pub fn key(&mut self, s: &mut State, surface: u32, code: u32, pressed: bool) {
        if !pressed {
            if self.repeat.is_some_and(|(c, _)| c == code) {
                self.repeat = None;
            }
            return;
        }
        let sym = s.keyboard.sym(code);
        let modifier = (0xffe1..=0xffee).contains(&sym) || sym == 0xfe03;
        if modifier {
            return;
        }
        self.key_press(s, surface, code);
        self.repeat = Some((code, Instant::now() + Duration::from_millis(config::REPEAT_DELAY.max(0) as u64)));
        self.arm();
    }

    fn key_press(&mut self, s: &mut State, surface: u32, code: u32) {
        let Some(i) = self.windows.iter().position(|w| w.surface == surface) else { return };
        let sym = s.keyboard.sym(code);
        let text = s.keyboard.text(code);
        let m = s.keyboard.bind_mods();
        let (shift, ctrl, alt) = (m & config::mods::SHIFT != 0, m & config::mods::CTRL != 0, m & config::mods::ALT != 0);
        // The terminal's own shortcuts, as in foot.
        if ctrl && shift {
            match s.keyboard.base_sym(code) {
                Some(0x63) => return self.copy(s, i),
                Some(0x76) => return self.paste(i),
                Some(0x6e) => {
                    let cwd = pty::cwd(self.windows[i].pty.pid);
                    return self.open(Open { cwd, ..Default::default() });
                }
                _ => {}
            }
        }
        if shift && !ctrl && !alt {
            let rows = self.windows[i].engine.rows() as i32;
            match sym {
                0xff55 => return self.scroll(s, i, rows - 1),
                0xff56 => return self.scroll(s, i, -(rows - 1)),
                _ => {}
            }
        }
        let Some(key) = key_for(sym, text, ctrl) else { return };
        let w = &mut self.windows[i];
        if let Some(bytes) = w.engine.encode_key(key, term::Mods { shift, ctrl, alt }) {
            if w.engine.view_offset() != 0 {
                w.engine.reset_view();
            }
            w.engine.clear_selection();
            w.out.extend_from_slice(&bytes);
            write_out(w);
            self.redraw(s, i);
        }
    }

    fn copy(&mut self, s: &mut State, i: usize) {
        let text = self.windows[i].engine.selected_text();
        if !text.is_empty() {
            selection::set_server(s, crate::server::clipboard::Clip { mime: "text/plain;charset=utf-8".into(), data: text.into_bytes() });
        }
    }

    fn paste(&mut self, i: usize) {
        let Some(offer) = self.clipboard else { return };
        let mimes = self.offers.get(&offer).cloned().unwrap_or_default();
        let Some(mime) = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"].into_iter().find(|m| mimes.iter().any(|x| x == m)) else { return };
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return;
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        self.send(Msg::new(offer, wl_data_offer::req::RECEIVE).str(mime).fd(write));
        self.flush();
        self.pastes.push((read, self.windows[i].surface, Vec::new()));
    }

    // ---- pointer ---------------------------------------------------------

    /// The cell under the pointer, clamped to the grid.
    fn cell_at(&self, i: usize) -> (usize, usize) {
        let (cw, ch) = self.cell();
        let w = &self.windows[i];
        let x = ((self.mouse.pos.0 as i32 - config::TERM_PAD) / cw).clamp(0, w.engine.cols() as i32 - 1);
        let y = ((self.mouse.pos.1 as i32 - config::TERM_PAD) / ch).clamp(0, w.engine.rows() as i32 - 1);
        (x as usize, y as usize)
    }

    fn mouse_window(&self) -> Option<usize> {
        self.windows.iter().position(|w| w.surface == self.mouse.surface)
    }

    fn reporting(&self, s: &State, i: usize) -> bool {
        self.windows[i].engine.wants_mouse() && s.keyboard.bind_mods() & config::mods::SHIFT == 0
    }

    fn pointer_button(&mut self, s: &mut State, button: u32, pressed: bool) {
        let Some(i) = self.mouse_window() else { return };
        let (x, y) = self.cell_at(i);
        let bit = 1 << button.saturating_sub(0x110).min(31);
        if pressed {
            self.mouse.buttons |= bit;
        } else {
            self.mouse.buttons &= !bit;
        }
        if self.reporting(s, i) {
            let b = mouse_button(button);
            let action = if pressed { MouseAction::Press(b) } else { MouseAction::Release(b) };
            let ev = MouseEvent { action, col: x, row: y, mods: mods_of(s.keyboard.bind_mods()) };
            let w = &mut self.windows[i];
            if let Some(bytes) = w.engine.encode_mouse(ev) {
                w.out.extend_from_slice(&bytes);
                write_out(w);
            }
            return;
        }
        match (button, pressed) {
            (0x110, true) => {
                let now = Instant::now();
                let quick = self.mouse.clicks.1.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400));
                self.mouse.clicks = (if quick { self.mouse.clicks.0 % 3 + 1 } else { 1 }, Some(now));
                let mode = match self.mouse.clicks.0 {
                    2 => SelectionMode::Word,
                    3 => SelectionMode::Line,
                    _ => SelectionMode::Char,
                };
                let w = &mut self.windows[i];
                let p = w.engine.point_at(x, y);
                if mode == SelectionMode::Char {
                    w.engine.clear_selection();
                } else {
                    w.engine.set_selection(p, p, mode);
                }
                self.mouse.anchor = Some((p, mode));
                self.redraw(s, i);
            }
            (0x110, false) => {
                self.mouse.anchor = None;
            }
            (0x112, true) => self.paste(i),
            _ => {}
        }
    }

    fn pointer_motion(&mut self, s: &mut State) {
        let Some(i) = self.mouse_window() else { return };
        let (x, y) = self.cell_at(i);
        if self.reporting(s, i) {
            let ev = MouseEvent { action: MouseAction::Motion(held_button(self.mouse.buttons)), col: x, row: y, mods: mods_of(s.keyboard.bind_mods()) };
            let w = &mut self.windows[i];
            if let Some(bytes) = w.engine.encode_mouse(ev) {
                w.out.extend_from_slice(&bytes);
                write_out(w);
            }
            return;
        }
        if let Some((anchor, mode)) = self.mouse.anchor {
            let w = &mut self.windows[i];
            let p = w.engine.point_at(x, y);
            if p != anchor || mode != SelectionMode::Char {
                w.engine.set_selection(anchor, p, mode);
                self.redraw(s, i);
            }
        }
    }

    fn wheel(&mut self, s: &mut State, value: f64) {
        let Some(i) = self.mouse_window() else { return };
        // The server sends 15 per wheel detent.
        let steps = (value / 15.0).round() as i32;
        if steps == 0 {
            return;
        }
        let (x, y) = self.cell_at(i);
        if self.reporting(s, i) {
            let mods = mods_of(s.keyboard.bind_mods());
            let button = if steps < 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            let w = &mut self.windows[i];
            for _ in 0..steps.unsigned_abs() {
                if let Some(bytes) = w.engine.encode_mouse(MouseEvent { action: MouseAction::Press(button), col: x, row: y, mods }) {
                    w.out.extend_from_slice(&bytes);
                }
            }
            write_out(w);
            return;
        }
        let w = &mut self.windows[i];
        if w.engine.is_alt_screen() {
            // Alternate scroll: programs on the alternate screen get arrows.
            let key = if steps < 0 { term::Key::Up } else { term::Key::Down };
            for _ in 0..(steps.unsigned_abs() as i32 * config::TERM_SCROLL_LINES) {
                if let Some(bytes) = w.engine.encode_key(key, term::Mods::default()) {
                    w.out.extend_from_slice(&bytes);
                }
            }
            write_out(w);
            return;
        }
        self.scroll(s, i, -steps * config::TERM_SCROLL_LINES);
    }

    fn scroll(&mut self, s: &mut State, i: usize, lines: i32) {
        self.windows[i].engine.scroll_view(lines);
        self.redraw(s, i);
    }

    // ---- drawing ---------------------------------------------------------

    /// Draws a terminal window with its top-left corner at `(x, y)`;
    /// rows outside the frame's region are skipped.
    pub fn draw(&self, r: &mut Renderer, surface: u32, x: i32, y: i32, focused: bool) {
        let Some(w) = self.windows.iter().find(|w| w.surface == surface) else { return };
        let mut g = self.glyphs.borrow_mut();
        if g.is_none() {
            *g = glyphs::Glyphs::new();
        }
        let Some(g) = g.as_mut() else { return };
        let mut batch = self.batch.borrow_mut();
        let mut row = self.row.borrow_mut();
        let row = &mut *row;
        batch.clear();
        let (cw, ch) = g.cell;
        let (_, default_bg, cursor_color) = w.engine.default_colors();
        batch.fill(Quad { x: x as f32, y: y as f32, w: w.size.0 as f32, h: w.size.1 as f32 }, rgb(default_bg));
        let (ox, oy) = (x + config::TERM_PAD, y + config::TERM_PAD);
        let rows = w.engine.rows() as i32;
        let (first, last) = match r.region() {
            Some(q) => (((q.y as i32 - oy) / ch).max(0), (((q.y + q.h) as i32 - oy + ch - 1) / ch).clamp(0, rows)),
            None => (0, rows),
        };
        let cursor = w.engine.cursor();
        let mut utf = [0u8; 4];
        for ry in first..last {
            let cells = w.engine.visible_row(ry as usize, row);
            let py = (oy + ry * ch) as f32;
            for (cx, c) in cells.iter().enumerate() {
                if c.is_spacer() {
                    continue;
                }
                let px = (ox + cx as i32 * cw) as f32;
                let wide = c.is_wide();
                let span = if wide { 2.0 } else { 1.0 };
                let (mut fg, mut bg) = w.engine.cell_colors(c);
                if w.engine.is_selected(cx, ry as usize) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if bg != default_bg {
                    batch.fill(Quad { x: px, y: py, w: cw as f32 * span, h: ch as f32 }, rgb(bg));
                }
                if c.attrs & attr::INVISIBLE != 0 {
                    continue;
                }
                let color = rgb(fg);
                let chr = c.char().or_else(|| w.engine.cell_str(c, &mut utf).chars().next());
                if let Some(chr) = chr.filter(|ch| *ch != ' ') {
                    if let Some(slot) = g.get(r, chr, c.attrs & attr::BOLD != 0, wide) {
                        if let Some(atlas) = g.atlas() {
                            batch.glyph(atlas, slot.src, Quad { x: px + slot.dx, y: py + slot.dy, w: slot.src.w, h: slot.src.h }, color);
                        }
                    }
                }
                if c.attrs & attr::UNDERLINE != 0 {
                    batch.fill(Quad { x: px, y: py + ch as f32 - 2.0, w: cw as f32 * span, h: 1.0 }, color);
                }
                if c.attrs & attr::STRIKE != 0 {
                    batch.fill(Quad { x: px, y: py + (ch / 2) as f32, w: cw as f32 * span, h: 1.0 }, color);
                }
            }
            if cursor.visible && cursor.row as i32 == ry {
                let px = (ox + cursor.col as i32 * cw) as f32;
                let color = rgb(cursor_color);
                let (fw, fh) = (cw as f32, ch as f32);
                match (focused, cursor.shape) {
                    (false, _) => {
                        batch.fill(Quad { x: px, y: py, w: fw, h: 1.0 }, color);
                        batch.fill(Quad { x: px, y: py + fh - 1.0, w: fw, h: 1.0 }, color);
                        batch.fill(Quad { x: px, y: py, w: 1.0, h: fh }, color);
                        batch.fill(Quad { x: px + fw - 1.0, y: py, w: 1.0, h: fh }, color);
                    }
                    (true, term::CursorShape::Bar) => batch.fill(Quad { x: px, y: py, w: 2.0, h: fh }, color),
                    (true, term::CursorShape::Underline) => batch.fill(Quad { x: px, y: py + fh - 2.0, w: fw, h: 2.0 }, color),
                    (true, term::CursorShape::Block) => {
                        batch.fill(Quad { x: px, y: py, w: fw, h: fh }, color);
                        let under = w.engine.visible_row(ry as usize, row).get(cursor.col).copied();
                        if let Some(c) = under {
                            let chr = c.char().filter(|ch| *ch != ' ');
                            if let Some(slot) = chr.and_then(|chr| g.get(r, chr, c.attrs & attr::BOLD != 0, c.is_wide())) {
                                if let Some(atlas) = g.atlas() {
                                    batch.glyph(atlas, slot.src, Quad { x: px + slot.dx, y: py + slot.dy, w: slot.src.w, h: slot.src.h }, rgb(default_bg));
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(atlas) = g.atlas() {
            r.draw_batch(atlas, &batch);
        } else if !batch.is_empty() {
            let atlas = r.create_atlas(1, 1);
            r.draw_batch(&atlas, &batch);
            r.destroy_texture(atlas);
        }
    }

}

fn write_out(w: &mut Window) {
    while !w.out.is_empty() {
        let n = unsafe { libc::write(w.pty.master.as_raw_fd(), w.out.as_ptr().cast(), w.out.len()) };
        if n <= 0 {
            break;
        }
        w.out.drain(..n as usize);
    }
}

fn hex(c: u32) -> Rgb {
    Rgb::new((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn theme() -> term::Theme {
    term::Theme { fg: hex(config::TERM_FG), bg: hex(config::TERM_BG), cursor: hex(config::TERM_FG), ..term::Theme::default() }
}

fn rgb(c: Rgb) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, 1.0]
}

fn mods_of(m: u32) -> term::Mods {
    term::Mods { shift: m & config::mods::SHIFT != 0, ctrl: m & config::mods::CTRL != 0, alt: m & config::mods::ALT != 0 }
}

fn mouse_button(code: u32) -> MouseButton {
    match code {
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

fn held_button(buttons: u32) -> Option<MouseButton> {
    match buttons {
        b if b & 1 != 0 => Some(MouseButton::Left),
        b if b & 4 != 0 => Some(MouseButton::Middle),
        b if b & 2 != 0 => Some(MouseButton::Right),
        _ => None,
    }
}

/// The terminal key for an xkb keysym and the text it types.
fn key_for(sym: u32, text: Option<char>, ctrl: bool) -> Option<term::Key> {
    use term::Key;
    Some(match sym {
        0xff0d | 0xff8d => Key::Enter,
        0xff09 | 0xfe20 => Key::Tab,
        0xff08 => Key::Backspace,
        0xff1b => Key::Escape,
        0xff51 | 0xff96 => Key::Left,
        0xff52 | 0xff97 => Key::Up,
        0xff53 | 0xff98 => Key::Right,
        0xff54 | 0xff99 => Key::Down,
        0xff50 | 0xff95 => Key::Home,
        0xff57 | 0xff9c => Key::End,
        0xff55 | 0xff9a => Key::PageUp,
        0xff56 | 0xff9b => Key::PageDown,
        0xff63 | 0xff9e => Key::Insert,
        0xffff | 0xff9f => Key::Delete,
        0xffbe..=0xffc9 => Key::F((sym - 0xffbe + 1) as u8),
        // With Ctrl, xkb already turned letters into control codes; the
        // terminal wants the letter and applies Ctrl itself.
        _ if ctrl && (0x20..0x7f).contains(&sym) => Key::Char(char::from_u32(sym)?),
        _ => Key::Char(text?),
    })
}

impl State {
    /// A terminal window took its size: it shows terminal cells.
    pub fn set_term_content(&mut self, key: SKey, w: i32, h: i32) {
        if let Some(sf) = self.surfaces.get_mut(&key) {
            sf.size = (w, h);
            sf.content = crate::server::Content::Term;
            sf.opaque = true;
            sf.damage = None;
        }
    }
}

impl state::Persist for Window {
    fn save(&self, e: &mut state::Enc) {
        (self.surface, self.xdg, self.toplevel).save(e);
        self.pty.master.save(e);
        self.pty.pid.save(e);
        self.engine.save(e);
        (self.size, self.pending, self.title.clone()).save(e);
        state::save_bytes(&self.out, e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        let (surface, xdg, toplevel) = Persist::load(d)?;
        let pty = pty::Pty { master: Persist::load(d)?, pid: Persist::load(d)? };
        let engine = Persist::load(d)?;
        let (size, pending, title) = Persist::load(d)?;
        let out = state::load_bytes(d)?;
        Ok(Window { surface, xdg, toplevel, pty, engine, size, pending, title, held: None, out })
    }
}

/// Pastes in flight and the mouse state are dropped; the glyph atlas is
/// rebuilt in the new GL context.
impl state::Persist for Terms {
    fn save(&self, e: &mut state::Enc) {
        self.conn.save(e);
        self.client.save(e);
        let objects: HashMap<u32, String> = self.objects.iter().map(|(id, i)| (*id, i.name.to_string())).collect();
        objects.save(e);
        (self.next_id, self.compositor, self.wm_base, self.seat, self.pointer).save(e);
        (self.data_device, self.data_manager, self.clipboard).save(e);
        self.offers.save(e);
        self.windows.save(e);
        // Only where they open: a command waiting across an upgrade is lost.
        self.queued.iter().map(|o| o.cwd.clone()).collect::<Vec<_>>().save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        let conn = Persist::load(d)?;
        let client = Persist::load(d)?;
        let names: HashMap<u32, String> = Persist::load(d)?;
        let objects = names.into_iter().filter_map(|(id, n)| Some((id, wire::interface_by_name(&n)?))).collect();
        let (next_id, compositor, wm_base, seat, pointer) = Persist::load(d)?;
        let (data_device, data_manager, clipboard) = Persist::load(d)?;
        let offers = Persist::load(d)?;
        let windows = Persist::load(d)?;
        let cwds: Vec<Option<String>> = Persist::load(d)?;
        let queued = cwds.into_iter().map(|cwd| Open { cwd, ..Default::default() }).collect();
        Ok(Terms {
            conn,
            client,
            objects,
            next_id,
            compositor,
            wm_base,
            seat,
            pointer,
            data_device,
            data_manager,
            offers,
            clipboard,
            windows,
            queued,
            mouse: Mouse::default(),
            pastes: Vec::new(),
            repeat: None,
            timer: crate::event_loop::timer_fd().map_err(|e| state::Error(e.to_string()))?,
            graveyard: Vec::new(),
            glyphs: RefCell::new(None),
            batch: RefCell::new(Batch::default()),
            row: RefCell::new(Vec::new()),
        })
    }
}
