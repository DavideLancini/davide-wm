//! Wayland server: clients, their objects and the shared compositor state.
//!
//! All state is plain owned data keyed by ids so it can later be serialised
//! for the live upgrade (README, ADR-014).

mod compositor;
pub mod seat;
mod xdg;

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Instant;

use render::{Quad, Renderer, Target, Texture};
use wire::{wl_callback, wl_display, Args, Connection, Interface, Msg};
use wm::{Dwindle, Rect};

use crate::keyboard::Keyboard;

pub use seat::CursorImage;

pub type ClientId = u32;

/// Identifies a `wl_surface` across clients.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SKey {
    pub client: ClientId,
    pub id: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Display,
    Registry,
    Callback,
    Compositor,
    Surface,
    Region,
    Subcompositor,
    Subsurface { surface: u32 },
    Shm,
    ShmPool,
    Buffer,
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    XdgWmBase,
    XdgPositioner,
    XdgSurface { surface: u32 },
    XdgToplevel { surface: u32 },
    XdgPopup { surface: u32 },
    DecorationManager,
    Decoration { surface: u32 },
    DataDeviceManager,
    DataSource,
    DataDevice,
}

pub struct Object {
    pub iface: &'static Interface,
    pub version: u32,
    pub kind: Kind,
}

pub struct Client {
    pub conn: Connection,
    pub objects: HashMap<u32, Object>,
    pub dead: bool,
}

pub struct Global {
    pub name: u32,
    pub iface: &'static Interface,
    pub version: u32,
}

#[derive(Default)]
pub struct Pending {
    /// `Some(None)` detaches the buffer.
    pub buffer: Option<Option<u32>>,
    pub frames: Vec<u32>,
    pub geometry: Option<Rect>,
}

pub enum Role {
    None,
    Cursor,
    Subsurface { parent: u32, x: i32, y: i32 },
    Xdg,
}

pub enum XdgRole {
    None,
    Toplevel { obj: u32, title: String, app_id: String, decoration: Option<u32>, sent: Option<(i32, i32, bool)> },
    Popup { obj: u32, parent: u32, geo: Rect },
}

pub struct Xdg {
    pub obj: u32,
    pub role: XdgRole,
    pub geometry: Option<Rect>,
    pub configured: bool,
    pub mapped: bool,
}

pub struct Surface {
    pub pending: Pending,
    /// Committed buffer, kept until uploaded and released.
    pub buffer: Option<u32>,
    pub upload: bool,
    pub size: (i32, i32),
    pub opaque: bool,
    pub tex: Option<Texture>,
    pub frames: Vec<u32>,
    pub role: Role,
    pub above: Vec<u32>,
    pub below: Vec<u32>,
    pub xdg: Option<Xdg>,
}

pub struct Pool {
    pub _fd: OwnedFd,
    pub ptr: *mut u8,
    pub size: usize,
    pub buffers: u32,
    pub destroyed: bool,
}

pub struct ShmBuffer {
    pub pool: u32,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub opaque: bool,
}

#[derive(Default, Clone, Copy)]
pub struct Positioner {
    pub size: (i32, i32),
    pub anchor_rect: Rect,
    pub anchor: u32,
    pub gravity: u32,
    pub offset: (i32, i32),
}

pub struct Output {
    pub name: String,
    pub width: i32,
    pub height: i32,
}

pub struct PointerState {
    pub x: f64,
    pub y: f64,
    pub focus: Option<SKey>,
    pub cursor: CursorImage,
}

pub struct State {
    pub clients: HashMap<ClientId, Client>,
    next_client: ClientId,
    pub globals: Vec<Global>,
    pub surfaces: HashMap<SKey, Surface>,
    pub pools: HashMap<SKey, Pool>,
    pub buffers: HashMap<SKey, ShmBuffer>,
    pub positioners: HashMap<SKey, Positioner>,
    pub output: Output,
    pub tiling: Dwindle<SKey>,
    pub layout: Vec<(SKey, Rect)>,
    pub popups: Vec<SKey>,
    pub focus: Option<SKey>,
    pub pointer: PointerState,
    pub keyboard: Keyboard,
    serial: u32,
    start: Instant,
    pub needs_redraw: bool,
    dead_textures: Vec<Texture>,
}

impl State {
    pub fn new(keyboard: Keyboard, output: Output) -> State {
        let mut s = State {
            clients: HashMap::new(),
            next_client: 1,
            globals: Vec::new(),
            surfaces: HashMap::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            positioners: HashMap::new(),
            output,
            tiling: Dwindle::default(),
            layout: Vec::new(),
            popups: Vec::new(),
            focus: None,
            pointer: PointerState { x: 0.0, y: 0.0, focus: None, cursor: CursorImage::Default },
            keyboard,
            serial: 0,
            start: Instant::now(),
            needs_redraw: true,
            dead_textures: Vec::new(),
        };
        for (iface, version) in [
            (&wire::wl_compositor::INTERFACE, 6),
            (&wire::wl_subcompositor::INTERFACE, 1),
            (&wire::wl_shm::INTERFACE, 1),
            (&wire::wl_seat::INTERFACE, 7),
            (&wire::wl_output::INTERFACE, 4),
            (&wire::xdg_wm_base::INTERFACE, 5),
            (&wire::zxdg_decoration_manager_v1::INTERFACE, 1),
            (&wire::wl_data_device_manager::INTERFACE, 3),
        ] {
            let name = s.globals.len() as u32 + 1;
            s.globals.push(Global { name, iface, version: version.min(iface.version) });
        }
        s
    }

    pub fn add_client(&mut self, fd: OwnedFd) -> ClientId {
        let id = self.next_client;
        self.next_client += 1;
        let mut objects = HashMap::new();
        objects.insert(1, Object { iface: &wl_display::INTERFACE, version: 1, kind: Kind::Display });
        self.clients.insert(id, Client { conn: Connection::new(fd), objects, dead: false });
        id
    }

    pub fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    pub fn time_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn send(&mut self, client: ClientId, msg: Msg) {
        if let Some(c) = self.clients.get_mut(&client) {
            c.conn.send(msg);
        }
    }

    pub fn version(&self, client: ClientId, id: u32) -> u32 {
        self.clients.get(&client).and_then(|c| c.objects.get(&id)).map_or(0, |o| o.version)
    }

    /// Ids and versions of the client's objects of a given kind.
    pub fn objects_of(&self, client: ClientId, kind: Kind) -> Vec<(u32, u32)> {
        self.clients
            .get(&client)
            .map(|c| c.objects.iter().filter(|(_, o)| o.kind == kind).map(|(id, o)| (*id, o.version)).collect())
            .unwrap_or_default()
    }

    /// Sends a protocol error and schedules the disconnection.
    pub fn error(&mut self, client: ClientId, object: u32, code: u32, message: &str) {
        log!("client {client}: protocol error on object {object}: {message}");
        if let Some(c) = self.clients.get_mut(&client) {
            c.conn.send(Msg::new(1, wl_display::evt::ERROR).obj(object).uint(code).str(message));
            c.dead = true;
        }
    }

    pub fn new_object(&mut self, client: ClientId, id: u32, iface: &'static Interface, version: u32, kind: Kind) -> bool {
        let Some(c) = self.clients.get_mut(&client) else { return false };
        if id == 0 || c.objects.contains_key(&id) {
            self.error(client, 1, wl_display::error::INVALID_OBJECT, &format!("id {id} already in use"));
            return false;
        }
        c.objects.insert(id, Object { iface, version, kind });
        true
    }

    /// Removes a client-created object and acknowledges its id.
    pub fn remove_object(&mut self, client: ClientId, id: u32) {
        if let Some(c) = self.clients.get_mut(&client) {
            if c.objects.remove(&id).is_some() {
                c.conn.send(Msg::new(1, wl_display::evt::DELETE_ID).uint(id));
            }
        }
    }

    pub fn send_callback_done(&mut self, client: ClientId, id: u32, data: u32) {
        self.send(client, Msg::new(id, wl_callback::evt::DONE).uint(data));
        self.remove_object(client, id);
    }

    /// Reads and dispatches everything the client sent.
    pub fn dispatch_client(&mut self, client: ClientId) {
        loop {
            let Some(c) = self.clients.get_mut(&client) else { return };
            match c.conn.fill() {
                Ok(0) => {
                    c.dead = true;
                    return;
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log!("client {client}: read error: {e}");
                    c.dead = true;
                    return;
                }
            }
        }
        loop {
            let Some(c) = self.clients.get_mut(&client) else { return };
            if c.dead {
                return;
            }
            let Some(h) = c.conn.peek() else { return };
            let Some(obj) = c.objects.get(&h.object) else {
                c.conn.skip(&h);
                // Requests to objects destroyed by the server are ignored.
                continue;
            };
            let (iface, version, kind) = (obj.iface, obj.version, obj.kind);
            let Some(desc) = iface.requests.get(h.opcode as usize) else {
                c.conn.skip(&h);
                self.error(client, h.object, wl_display::error::INVALID_METHOD, &format!("{iface:?}: bad opcode {}", h.opcode));
                return;
            };
            if desc.since > version {
                c.conn.skip(&h);
                self.error(client, h.object, wl_display::error::INVALID_METHOD, &format!("{iface:?}.{} needs version {}", desc.name, desc.since));
                return;
            }
            let args = match c.conn.decode(&h, desc) {
                Ok(a) => a,
                Err(e) => {
                    self.error(client, h.object, wl_display::error::INVALID_METHOD, &format!("{iface:?}.{}: {e}", desc.name));
                    return;
                }
            };
            self.request(client, h.object, kind, version, h.opcode, args);
            if desc.destructor {
                self.remove_object(client, h.object);
            }
        }
    }

    fn request(&mut self, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, args: Args) {
        match kind {
            Kind::Display => compositor::display(self, c, op, args),
            Kind::Registry => compositor::registry(self, c, op, args),
            Kind::Callback => {}
            Kind::Compositor => compositor::compositor(self, c, version, op, args),
            Kind::Surface => compositor::surface(self, c, id, op, args),
            Kind::Region => {}
            Kind::Subcompositor => compositor::subcompositor(self, c, version, op, args),
            Kind::Subsurface { surface } => compositor::subsurface(self, c, surface, op, args),
            Kind::Shm => compositor::shm(self, c, version, op, args),
            Kind::ShmPool => compositor::shm_pool(self, c, id, op, args),
            Kind::Buffer => compositor::buffer(self, c, id, op),
            Kind::Seat => seat::seat(self, c, version, op, args),
            Kind::Pointer => seat::pointer(self, c, op, args),
            Kind::Keyboard | Kind::Touch | Kind::Output => {}
            Kind::XdgWmBase => xdg::wm_base(self, c, version, op, args),
            Kind::XdgPositioner => xdg::positioner(self, c, id, op, args),
            Kind::XdgSurface { surface } => xdg::xdg_surface(self, c, surface, version, op, args),
            Kind::XdgToplevel { surface } => xdg::toplevel(self, c, surface, op, args),
            Kind::XdgPopup { surface } => xdg::popup(self, c, surface, op),
            Kind::DecorationManager => xdg::decoration_manager(self, c, version, op, args),
            Kind::Decoration { surface } => xdg::decoration(self, c, id, surface, op),
            Kind::DataDeviceManager => compositor::data_device_manager(self, c, version, op, args),
            // Clipboard and drag and drop arrive in M1.
            Kind::DataSource | Kind::DataDevice => {}
        }
    }

    /// Writes pending output; returns the clients that still have data queued.
    pub fn flush_clients(&mut self) -> Vec<(ClientId, bool)> {
        let mut out = Vec::new();
        for (id, c) in self.clients.iter_mut() {
            if !c.conn.has_pending_output() {
                continue;
            }
            match c.conn.flush() {
                Ok(done) => out.push((*id, !done)),
                Err(e) => {
                    log!("client {id}: write error: {e}");
                    c.dead = true;
                }
            }
            if c.conn.overflowed() {
                log!("client {id}: not reading its events, disconnecting");
                c.dead = true;
            }
        }
        out
    }

    pub fn dead_clients(&self) -> Vec<ClientId> {
        self.clients.iter().filter(|(_, c)| c.dead).map(|(id, _)| *id).collect()
    }

    pub fn remove_client(&mut self, client: ClientId) {
        if let Some(mut c) = self.clients.remove(&client) {
            let _ = c.conn.flush();
        }
        let surfaces: Vec<SKey> = self.surfaces.keys().filter(|k| k.client == client).copied().collect();
        for k in surfaces {
            self.destroy_surface(k);
        }
        let pools: Vec<SKey> = self.pools.keys().filter(|k| k.client == client).copied().collect();
        for k in pools {
            if let Some(p) = self.pools.remove(&k) {
                compositor::unmap_pool(&p);
            }
        }
        self.buffers.retain(|k, _| k.client != client);
        self.positioners.retain(|k, _| k.client != client);
        self.needs_redraw = true;
    }

    pub fn destroy_surface(&mut self, key: SKey) {
        xdg::unmap(self, key);
        let Some(s) = self.surfaces.remove(&key) else { return };
        if let Some(t) = s.tex {
            self.dead_textures.push(t);
        }
        if let Role::Subsurface { parent, .. } = s.role {
            if let Some(p) = self.surfaces.get_mut(&SKey { client: key.client, id: parent }) {
                p.above.retain(|c| *c != key.id);
                p.below.retain(|c| *c != key.id);
            }
        }
        if self.pointer.focus == Some(key) {
            self.pointer.focus = None;
        }
        if let CursorImage::Surface(k, ..) = self.pointer.cursor {
            if k == key {
                self.pointer.cursor = CursorImage::Hidden;
            }
        }
        self.needs_redraw = true;
    }

    /// Asks a toplevel to close.
    pub fn close_window(&mut self, key: SKey) {
        if let Some(XdgRole::Toplevel { obj, .. }) = self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            let obj = *obj;
            self.send(key.client, Msg::new(obj, wire::xdg_toplevel::evt::CLOSE));
        }
    }

    /// Area available to tiled windows.
    pub fn work_area(&self) -> Rect {
        Rect::new(0, 0, self.output.width, self.output.height)
    }

    pub fn set_output_size(&mut self, width: i32, height: i32) {
        if (self.output.width, self.output.height) == (width, height) {
            return;
        }
        self.output.width = width;
        self.output.height = height;
        let clients: Vec<ClientId> = self.clients.keys().copied().collect();
        for c in clients {
            for (id, version) in self.objects_of(c, Kind::Output) {
                seat::send_output_mode(self, c, id, version);
            }
        }
        self.relayout();
    }

    pub fn relayout(&mut self) {
        self.layout = self.tiling.layout(self.work_area());
        for (key, rect) in self.layout.clone() {
            xdg::configure_toplevel(self, key, rect.w, rect.h);
        }
        self.needs_redraw = true;
    }

    /// Top-left of the window geometry of a mapped xdg surface, in output coordinates.
    pub fn window_origin(&self, key: SKey) -> Option<(i32, i32)> {
        let s = self.surfaces.get(&key)?;
        match &s.xdg.as_ref()?.role {
            XdgRole::Toplevel { .. } => self.layout.iter().find(|(k, _)| *k == key).map(|(_, r)| (r.x, r.y)),
            XdgRole::Popup { parent, geo, .. } => {
                let (px, py) = self.window_origin(SKey { client: key.client, id: *parent })?;
                Some((px + geo.x, py + geo.y))
            }
            XdgRole::None => None,
        }
    }

    /// Where the surface's buffer origin goes for a window at `origin`.
    pub fn surface_origin(&self, key: SKey, origin: (i32, i32)) -> (i32, i32) {
        let g = self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).and_then(|x| x.geometry).unwrap_or_default();
        (origin.0 - g.x, origin.1 - g.y)
    }

    pub fn render(&mut self, r: &mut Renderer, target: &Target) {
        for t in self.dead_textures.drain(..) {
            r.destroy_texture(t);
        }
        self.upload_buffers(r);
        r.begin(target, config::BACKGROUND);
        for (key, rect) in self.layout.clone() {
            if !self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).is_some_and(|x| x.mapped) {
                continue;
            }
            r.clip(Some(Quad { x: rect.x as f32, y: rect.y as f32, w: rect.w as f32, h: rect.h as f32 }));
            let o = self.surface_origin(key, (rect.x, rect.y));
            self.draw_tree(r, key, o);
        }
        r.clip(None);
        for key in self.popups.clone() {
            if let Some(origin) = self.window_origin(key) {
                let o = self.surface_origin(key, origin);
                self.draw_tree(r, key, o);
            }
        }
        seat::draw_cursor(self, r);
        r.finish();
        let time = self.time_ms();
        let mut done = Vec::new();
        for (k, s) in self.surfaces.iter_mut() {
            done.extend(s.frames.drain(..).map(|f| (k.client, f)));
        }
        for (c, f) in done {
            self.send_callback_done(c, f, time);
        }
        self.needs_redraw = false;
    }

    pub fn draw_tree(&self, r: &mut Renderer, key: SKey, (x, y): (i32, i32)) {
        let Some(s) = self.surfaces.get(&key) else { return };
        let child = |id: &u32| {
            let k = SKey { client: key.client, id: *id };
            match self.surfaces.get(&k).map(|c| &c.role) {
                Some(Role::Subsurface { x: cx, y: cy, .. }) => Some((k, (x + cx, y + cy))),
                _ => None,
            }
        };
        for (k, o) in s.below.iter().filter_map(child) {
            self.draw_tree(r, k, o);
        }
        if let Some(t) = &s.tex {
            r.draw(t, Quad { x: x as f32, y: y as f32, w: s.size.0 as f32, h: s.size.1 as f32 }, s.opaque);
        }
        for (k, o) in s.above.iter().filter_map(child) {
            self.draw_tree(r, k, o);
        }
    }

    fn upload_buffers(&mut self, r: &mut Renderer) {
        let keys: Vec<SKey> = self.surfaces.iter().filter(|(_, s)| s.upload).map(|(k, _)| *k).collect();
        for key in keys {
            let s = self.surfaces.get_mut(&key).unwrap();
            s.upload = false;
            let Some(bid) = s.buffer.take() else { continue };
            let bkey = SKey { client: key.client, id: bid };
            let Some(b) = self.buffers.get(&bkey) else { continue };
            let Some(p) = self.pools.get(&SKey { client: key.client, id: b.pool }) else { continue };
            let len = (b.stride * (b.height - 1) + b.width * 4) as usize;
            let pixels = unsafe { std::slice::from_raw_parts(p.ptr.add(b.offset as usize), len) };
            s.tex = Some(r.upload(s.tex.take(), b.width, b.height, b.stride, pixels));
            self.send(key.client, Msg::new(bid, wire::wl_buffer::evt::RELEASE));
        }
    }
}
