//! Wayland server: clients, their objects and the shared compositor state.
//!
//! All state is plain owned data keyed by ids so it can later be serialised
//! for the live upgrade (README, ADR-014).

mod compositor;
mod dmabuf;
mod misc;
mod persist;
pub mod seat;
mod selection;
pub mod workspaces;
mod xdg;

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::time::Instant;

use render::{Plane, Quad, Renderer, Target, Texture};
use wire::{wl_callback, wl_display, Args, Connection, Interface, Msg};
use wm::{Rect, Workspace, WsId};

use crate::keyboard::Keyboard;

pub use compositor::install_sigbus_handler;
pub use seat::CursorImage;

pub type ClientId = u32;

/// Identifies an object (usually a `wl_surface`) across clients.
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
    DataOffer { generation: u32 },
    PrimaryManager,
    PrimarySource,
    PrimaryDevice,
    PrimaryOffer { generation: u32 },
    ControlManager,
    ControlSource,
    ControlDevice,
    ControlOffer { generation: u32, primary: bool },
    Activation,
    ActivationToken,
    Bell,
    IdleInhibitManager,
    IdleInhibitor { surface: u32 },
    Dmabuf,
    DmabufParams,
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
    next_server_id: u32,
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
    /// `sync`: state is applied when the parent commits (the default).
    Subsurface { parent: u32, x: i32, y: i32, sync: bool },
    Xdg,
}

pub enum XdgRole {
    None,
    Toplevel { obj: u32, title: String, app_id: String, decoration: Option<u32>, sent: Option<(i32, i32, bool, Option<wm::FsMode>)> },
    Popup { obj: u32, parent: u32, geo: Rect },
}

pub struct Xdg {
    pub obj: u32,
    pub role: XdgRole,
    pub geometry: Option<Rect>,
    pub configured: bool,
    pub mapped: bool,
}

/// What a surface shows.
pub enum Content {
    None,
    /// Copied from shared memory; the buffer is already released.
    Shm(Texture),
    /// A dmabuf, held until another buffer replaces it.
    Dmabuf(u32),
}

pub struct Surface {
    pub pending: Pending,
    /// State committed by a synchronised subsurface, waiting for the parent.
    pub cached: Option<Pending>,
    /// Committed buffer not uploaded yet.
    pub upload: Option<u32>,
    pub content: Content,
    pub size: (i32, i32),
    pub opaque: bool,
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

pub enum Buffer {
    Shm { pool: u32, offset: i32, width: i32, height: i32, stride: i32, opaque: bool },
    Dmabuf { width: i32, height: i32, format: u32, modifier: u64, planes: Vec<Plane>, tex: Option<Texture>, failed: bool },
}

impl Buffer {
    fn size(&self) -> (i32, i32) {
        match self {
            Buffer::Shm { width, height, .. } | Buffer::Dmabuf { width, height, .. } => (*width, *height),
        }
    }

    fn opaque(&self) -> bool {
        match self {
            Buffer::Shm { opaque, .. } => *opaque,
            // Formats without alpha start with 'X' (e.g. XR24, XB24).
            Buffer::Dmabuf { format, .. } => format.to_le_bytes()[0] == b'X',
        }
    }
}

#[derive(Default)]
pub struct DmabufParams {
    pub planes: Vec<(u32, Plane)>,
    pub modifier: u64,
    pub used: bool,
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

/// The status bar as the compositor sees it: a strip on top of the output.
#[derive(Default)]
pub struct BarSurface {
    pub height: i32,
    pub visible: bool,
    pub tex: Option<Texture>,
    /// Last snapshot sent to the bar thread, to send only changes.
    pub sent: Option<(Vec<bar::WsEntry>, Vec<bar::Client>)>,
}

pub struct LauncherView {
    pub launcher: launcher::Launcher,
    pub tex: Option<Texture>,
    pub dirty: bool,
    /// Focus to give back when it closes.
    pub prev_focus: Option<SKey>,
}

impl LauncherView {
    pub fn rect(&self, output: &Output) -> Rect {
        let (w, h) = (launcher::WIDTH as i32, launcher::HEIGHT as i32);
        Rect::new((output.width - w) / 2, (output.height - h) / 2, w, h)
    }
}

pub struct State {
    pub clients: HashMap<ClientId, Client>,
    next_client: ClientId,
    pub globals: Vec<Global>,
    pub surfaces: HashMap<SKey, Surface>,
    pub pools: HashMap<SKey, Pool>,
    pub buffers: HashMap<SKey, Buffer>,
    pub dmabuf_params: HashMap<SKey, DmabufParams>,
    pub dmabuf_formats: Vec<(u32, u64)>,
    pub positioners: HashMap<SKey, Positioner>,
    pub output: Output,
    pub workspaces: HashMap<WsId, Workspace<SKey>>,
    pub window_ws: HashMap<SKey, WsId>,
    pub active: WsId,
    pub special_shown: bool,
    /// Visible tiles, bottom to top; entries from `special_start` belong to the special workspace.
    pub layout: Vec<(SKey, Rect)>,
    pub special_start: usize,
    pub popups: Vec<SKey>,
    pub focus: Option<SKey>,
    pub urgent: HashSet<SKey>,
    pub pointer: PointerState,
    pub keyboard: Keyboard,
    pub selection: selection::Selections,
    pub idle_inhibitors: HashMap<SKey, SKey>,
    pub wallpaper: Option<Texture>,
    /// Workspaces marked by a desktop notification, until visited.
    pub urgent_ws: HashSet<WsId>,
    pub bar: BarSurface,
    /// The launcher while it is open; it has the keyboard to itself.
    pub launcher: Option<LauncherView>,
    serial: u32,
    start: Instant,
    pub needs_redraw: bool,
    /// The layout changed: the surface under the pointer may be another one.
    pub pointer_dirty: bool,
    dead_textures: Vec<Texture>,
}

impl State {
    pub fn new(keyboard: Keyboard, output: Output, dmabuf_formats: Vec<(u32, u64)>) -> State {
        let mut s = State {
            clients: HashMap::new(),
            next_client: 1,
            globals: Vec::new(),
            surfaces: HashMap::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            dmabuf_params: HashMap::new(),
            dmabuf_formats,
            positioners: HashMap::new(),
            output,
            workspaces: HashMap::new(),
            window_ws: HashMap::new(),
            active: WsId::Num(1),
            special_shown: false,
            layout: Vec::new(),
            special_start: 0,
            popups: Vec::new(),
            focus: None,
            urgent: HashSet::new(),
            pointer: PointerState { x: 0.0, y: 0.0, focus: None, cursor: CursorImage::Default },
            keyboard,
            selection: selection::Selections::default(),
            idle_inhibitors: HashMap::new(),
            wallpaper: None,
            urgent_ws: HashSet::new(),
            bar: BarSurface { visible: true, ..Default::default() },
            launcher: None,
            serial: 0,
            start: Instant::now(),
            needs_redraw: true,
            pointer_dirty: false,
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
            (&wire::zwp_primary_selection_device_manager_v1::INTERFACE, 1),
            (&wire::ext_data_control_manager_v1::INTERFACE, 1),
            (&wire::xdg_activation_v1::INTERFACE, 1),
            (&wire::xdg_system_bell_v1::INTERFACE, 1),
            (&wire::zwp_idle_inhibit_manager_v1::INTERFACE, 1),
            (&wire::zwp_linux_dmabuf_v1::INTERFACE, 3),
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
        self.clients.insert(id, Client { conn: Connection::new(fd), objects, dead: false, next_server_id: 0xff00_0000 });
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
        if id == 0 || id >= 0xff00_0000 || c.objects.contains_key(&id) {
            self.error(client, 1, wl_display::error::INVALID_OBJECT, &format!("id {id} already in use"));
            return false;
        }
        c.objects.insert(id, Object { iface, version, kind });
        true
    }

    /// Creates an object on the server side (e.g. a data offer); returns its id.
    pub fn new_server_object(&mut self, client: ClientId, iface: &'static Interface, version: u32, kind: Kind) -> Option<u32> {
        let c = self.clients.get_mut(&client)?;
        let id = c.next_server_id;
        c.next_server_id += 1;
        c.objects.insert(id, Object { iface, version, kind });
        Some(id)
    }

    /// Removes an object and acknowledges client-created ids.
    pub fn remove_object(&mut self, client: ClientId, id: u32) {
        if let Some(c) = self.clients.get_mut(&client) {
            if c.objects.remove(&id).is_some() && id < 0xff00_0000 {
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
            Kind::Compositor => compositor::compositor(self, c, version, op, args),
            Kind::Surface => compositor::surface(self, c, id, op, args),
            Kind::Subcompositor => compositor::subcompositor(self, c, version, op, args),
            Kind::Subsurface { surface } => compositor::subsurface(self, c, surface, op, args),
            Kind::Shm => compositor::shm(self, c, version, op, args),
            Kind::ShmPool => compositor::shm_pool(self, c, id, op, args),
            Kind::Buffer => compositor::buffer(self, c, id, op),
            Kind::Seat => seat::seat(self, c, version, op, args),
            Kind::Pointer => seat::pointer(self, c, op, args),
            Kind::XdgWmBase => xdg::wm_base(self, c, version, op, args),
            Kind::XdgPositioner => xdg::positioner(self, c, id, op, args),
            Kind::XdgSurface { surface } => xdg::xdg_surface(self, c, surface, version, op, args),
            Kind::XdgToplevel { surface } => xdg::toplevel(self, c, surface, op, args),
            Kind::XdgPopup { surface } => xdg::popup(self, c, surface, op),
            Kind::DecorationManager => xdg::decoration_manager(self, c, version, op, args),
            Kind::Decoration { surface } => xdg::decoration(self, c, id, surface, op),
            Kind::DataDeviceManager
            | Kind::DataSource
            | Kind::DataDevice
            | Kind::DataOffer { .. }
            | Kind::PrimaryManager
            | Kind::PrimarySource
            | Kind::PrimaryDevice
            | Kind::PrimaryOffer { .. }
            | Kind::ControlManager
            | Kind::ControlSource
            | Kind::ControlDevice
            | Kind::ControlOffer { .. } => selection::request(self, c, id, kind, version, op, args),
            Kind::Activation | Kind::ActivationToken | Kind::Bell | Kind::IdleInhibitManager | Kind::IdleInhibitor { .. } => {
                misc::request(self, c, id, kind, version, op, args)
            }
            Kind::Dmabuf | Kind::DmabufParams => dmabuf::request(self, c, id, kind, version, op, args),
            Kind::Callback | Kind::Region | Kind::Keyboard | Kind::Touch | Kind::Output => {}
        }
    }

    /// Writes pending output; returns the clients that had data queued and
    /// whether some is still left.
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
        let buffers: Vec<SKey> = self.buffers.keys().filter(|k| k.client == client).copied().collect();
        for k in buffers {
            self.drop_buffer(k);
        }
        self.dmabuf_params.retain(|k, _| k.client != client);
        self.positioners.retain(|k, _| k.client != client);
        self.idle_inhibitors.retain(|k, _| k.client != client);
        selection::client_gone(self, client);
        self.needs_redraw = true;
    }

    fn drop_buffer(&mut self, key: SKey) {
        if let Some(Buffer::Dmabuf { tex: Some(t), .. }) = self.buffers.remove(&key) {
            self.dead_textures.push(t);
        }
    }

    pub fn destroy_surface(&mut self, key: SKey) {
        xdg::unmap(self, key);
        let Some(s) = self.surfaces.remove(&key) else { return };
        if let Content::Shm(t) = s.content {
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
        self.idle_inhibitors.retain(|_, s| *s != key);
        self.needs_redraw = true;
    }

    pub fn open_launcher(&mut self) {
        match launcher::Launcher::open() {
            Ok(l) => {
                let prev_focus = self.focus;
                seat::set_focus(self, None);
                self.launcher = Some(LauncherView { launcher: l, tex: None, dirty: true, prev_focus });
                self.needs_redraw = true;
            }
            Err(e) => log!("launcher: {e}"),
        }
    }

    pub fn close_launcher(&mut self) {
        let Some(l) = self.launcher.take() else { return };
        if let Some(t) = l.tex {
            self.dead_textures.push(t);
        }
        let back = l.prev_focus.filter(|k| self.window_ws.get(k) == Some(&workspaces::focused_ws(self)));
        match back {
            Some(k) => seat::set_focus(self, Some(k)),
            None => workspaces::refocus(self),
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

    pub fn app_id(&self, key: SKey) -> &str {
        match self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            Some(XdgRole::Toplevel { app_id, .. }) => app_id,
            _ => "",
        }
    }

    pub fn rule(&self, key: SKey) -> Option<&'static config::Rule> {
        let app_id = self.app_id(key);
        config::RULES.iter().find(|r| r.app_id == app_id)
    }

    /// Area available to tiled windows.
    pub fn work_area(&self) -> Rect {
        let top = if self.bar.visible { self.bar.height } else { 0 };
        Rect::new(0, top, self.output.width, self.output.height - top)
    }

    /// True when a point falls on the visible bar (not covered by a fullscreen window).
    pub fn in_bar(&self, y: f64) -> bool {
        let covered = self.workspaces.get(&workspaces::focused_ws(self)).is_some_and(|w| matches!(w.fullscreen, Some((_, wm::FsMode::Fullscreen))));
        self.bar.visible && self.bar.tex.is_some() && y < self.bar.height as f64 && !covered
    }

    pub fn set_bar_frame(&mut self, r: &mut Renderer, width: u32, height: u32, pixels: &[u8]) {
        let old = self.bar.tex.take();
        self.bar.tex = Some(r.upload(old, width as i32, height as i32, width as i32 * 4, pixels));
        self.needs_redraw = true;
    }

    /// What the bar shows about workspaces and windows.
    pub fn bar_snapshot(&self) -> (Vec<bar::WsEntry>, Vec<bar::Client>) {
        let id = |ws: WsId| match ws {
            WsId::Num(n) => n as i32,
            WsId::Chat => -1,
            WsId::Special => -99,
        };
        let name = |ws: WsId| match ws {
            WsId::Num(n) => n.to_string(),
            WsId::Chat => "chat".to_string(),
            WsId::Special => "special:resize".to_string(),
        };
        let mut list: Vec<WsId> = self.workspaces.iter().filter(|(_, w)| !w.tiling.is_empty()).map(|(id, _)| *id).collect();
        if !list.contains(&self.active) {
            list.push(self.active);
        }
        list.sort_by_key(|w| match w {
            WsId::Chat => (0, 0),
            WsId::Num(n) => (1, *n),
            WsId::Special => (2, 0),
        });
        let urgent: HashSet<WsId> = self.urgent.iter().filter_map(|k| self.window_ws.get(k).copied()).chain(self.urgent_ws.iter().copied()).collect();
        let entries = list
            .into_iter()
            .map(|w| bar::WsEntry {
                id: id(w),
                name: name(w),
                active: w == self.active || (w == WsId::Special && self.special_shown),
                urgent: urgent.contains(&w),
            })
            .collect();
        let mut clients: Vec<bar::Client> = self
            .window_ws
            .iter()
            .map(|(k, w)| {
                let title = match self.surfaces.get(k).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
                    Some(XdgRole::Toplevel { title, .. }) => title.clone(),
                    _ => String::new(),
                };
                let focus_age = if self.focus == Some(*k) { 0 } else { 1 };
                bar::Client { app_id: self.app_id(*k).to_string(), title, workspace: id(*w), focus_age }
            })
            .collect();
        clients.sort_by(|a, b| (a.workspace, &a.app_id, &a.title).cmp(&(b.workspace, &b.app_id, &b.title)));
        (entries, clients)
    }

    /// An idle inhibitor belongs to a client with a window on screen.
    pub fn idle_inhibited(&self) -> bool {
        self.idle_inhibitors.keys().any(|i| self.layout.iter().any(|(w, _)| w.client == i.client))
    }

    pub fn output_rect(&self) -> Rect {
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
        let (work, out) = (self.work_area(), self.output_rect());
        let mut layout = self.workspaces.get(&self.active).map(|w| w.layout(work, out)).unwrap_or_default();
        self.special_start = layout.len();
        if self.special_shown {
            layout.extend(self.workspaces.get(&WsId::Special).map(|w| w.layout(work, out)).unwrap_or_default());
        }
        self.layout = layout;
        for (key, rect) in self.layout.clone() {
            xdg::configure_toplevel(self, key, rect.w, rect.h);
        }
        self.needs_redraw = true;
        self.pointer_dirty = true;
    }

    pub fn fs_mode(&self, key: SKey) -> Option<wm::FsMode> {
        let ws = self.window_ws.get(&key)?;
        self.workspaces.get(ws)?.fullscreen.filter(|(k, _)| *k == key).map(|(_, m)| m)
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

    fn draw_background(&self, r: &mut Renderer) {
        let Some(t) = &self.wallpaper else { return };
        // Cover the output, cropping the image symmetrically.
        let (ow, oh) = (self.output.width as f32, self.output.height as f32);
        let scale = (ow / t.width as f32).max(oh / t.height as f32);
        let (sw, sh) = (ow / scale, oh / scale);
        let src = Quad { x: (t.width as f32 - sw) / 2.0, y: (t.height as f32 - sh) / 2.0, w: sw, h: sh };
        r.draw_cropped(t, src, Quad { x: 0.0, y: 0.0, w: ow, h: oh });
    }

    pub fn render(&mut self, r: &mut Renderer, target: &Target) {
        for t in self.dead_textures.drain(..) {
            r.destroy_texture(t);
        }
        self.upload_buffers(r);
        r.begin(target, config::BACKGROUND);
        self.draw_background(r);
        if let Some(t) = self.bar.tex.as_ref().filter(|_| self.bar.visible) {
            r.draw(t, Quad { x: 0.0, y: 0.0, w: t.width as f32, h: t.height as f32 }, true);
        }
        for (i, (key, rect)) in self.layout.clone().into_iter().enumerate() {
            if i == self.special_start {
                r.fill(Quad { x: 0.0, y: 0.0, w: self.output.width as f32, h: self.output.height as f32 }, config::SPECIAL_DIM);
            }
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
        if let Some(l) = &mut self.launcher {
            if l.dirty {
                l.dirty = false;
                let pixels = l.launcher.render();
                let (w, h) = (launcher::WIDTH as i32, launcher::HEIGHT as i32);
                l.tex = Some(r.upload(l.tex.take(), w, h, w * 4, &pixels));
            }
            let rect = l.rect(&self.output);
            if let Some(t) = &l.tex {
                r.draw(t, Quad { x: rect.x as f32, y: rect.y as f32, w: rect.w as f32, h: rect.h as f32 }, true);
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

    fn texture(&self, key: SKey) -> Option<&Texture> {
        match &self.surfaces.get(&key)?.content {
            Content::None => None,
            Content::Shm(t) => Some(t),
            Content::Dmabuf(id) => match self.buffers.get(&SKey { client: key.client, id: *id })? {
                Buffer::Dmabuf { tex, .. } => tex.as_ref(),
                Buffer::Shm { .. } => None,
            },
        }
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
        if let Some(t) = self.texture(key) {
            r.draw(t, Quad { x: x as f32, y: y as f32, w: s.size.0 as f32, h: s.size.1 as f32 }, s.opaque);
        }
        for (k, o) in s.above.iter().filter_map(child) {
            self.draw_tree(r, k, o);
        }
    }

    /// Imports dmabufs that are shown but have no texture yet (after a live upgrade).
    fn import_shown_dmabufs(&mut self, r: &mut Renderer) {
        let shown: Vec<SKey> = self
            .surfaces
            .iter()
            .filter_map(|(k, s)| match s.content {
                Content::Dmabuf(id) => Some(SKey { client: k.client, id }),
                _ => None,
            })
            .collect();
        for key in shown {
            if let Some(Buffer::Dmabuf { width, height, format, modifier, planes, tex: tex @ None, failed: failed @ false }) = self.buffers.get_mut(&key) {
                match r.import_dmabuf(*width, *height, *format, *modifier, planes) {
                    Ok(t) => *tex = Some(t),
                    Err(e) => {
                        log!("client {}: {e}", key.client);
                        *failed = true;
                    }
                }
            }
        }
    }

    fn upload_buffers(&mut self, r: &mut Renderer) {
        self.import_shown_dmabufs(r);
        let keys: Vec<SKey> = self.surfaces.iter().filter(|(_, s)| s.upload.is_some()).map(|(k, _)| *k).collect();
        for key in keys {
            let bid = self.surfaces.get_mut(&key).unwrap().upload.take().unwrap();
            let bkey = SKey { client: key.client, id: bid };
            let new_content = match self.buffers.get_mut(&bkey) {
                None => continue,
                Some(Buffer::Shm { pool, offset, width, height, stride, .. }) => {
                    let (offset, width, height, stride) = (*offset, *width, *height, *stride);
                    let Some(p) = self.pools.get(&SKey { client: key.client, id: *pool }) else { continue };
                    let len = (stride * (height - 1) + width * 4) as usize;
                    let old = match std::mem::replace(&mut self.surfaces.get_mut(&key).unwrap().content, Content::None) {
                        Content::Shm(t) => Some(t),
                        _ => None,
                    };
                    let ptr = unsafe { p.ptr.add(offset as usize) };
                    let (tex, faulted) = compositor::guarded_read(ptr, len, |pixels| r.upload(old, width, height, stride, pixels));
                    if faulted {
                        self.error(key.client, bid, wire::wl_shm::error::INVALID_FD, "shm pool truncated by the client");
                    }
                    self.send(key.client, Msg::new(bid, wire::wl_buffer::evt::RELEASE));
                    Content::Shm(tex)
                }
                Some(Buffer::Dmabuf { width, height, format, modifier, planes, tex, failed }) => {
                    if tex.is_none() && !*failed {
                        match r.import_dmabuf(*width, *height, *format, *modifier, planes) {
                            Ok(t) => *tex = Some(t),
                            Err(e) => {
                                log!("client {}: {e}", key.client);
                                *failed = true;
                            }
                        }
                    }
                    Content::Dmabuf(bid)
                }
            };
            let s = self.surfaces.get_mut(&key).unwrap();
            match std::mem::replace(&mut s.content, new_content) {
                Content::Dmabuf(old) if old != bid => self.send(key.client, Msg::new(old, wire::wl_buffer::evt::RELEASE)),
                Content::Shm(t) => self.dead_textures.push(t),
                _ => {}
            }
        }
    }
}
