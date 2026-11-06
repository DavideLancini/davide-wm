//! Wayland server: clients, their objects and the shared compositor state.
//!
//! All state is plain owned data keyed by ids so it can later be serialised
//! for the live upgrade (README, ADR-014).

pub mod clipboard;
mod compositor;
mod dmabuf;
pub mod dnd;
mod misc;
pub mod persist;
pub mod seat;
pub mod selection;
pub mod workspaces;
mod xdg;
pub mod xwm;

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
    DmabufFeedback,
    DragOffer,
    XwaylandShell,
    XwaylandSurface { surface: u32 },
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
    /// `Some(None)` resets the input region to the whole surface.
    pub input: Option<Option<Region>>,
    /// Buffer damage; not kept across a live upgrade (all is uploaded).
    pub damage: Vec<Rect>,
}

/// A `wl_region`: rectangles added or subtracted in order.
pub type Region = Vec<(bool, Rect)>;

/// True when the point is in the region (surface-local coordinates).
pub fn region_contains(region: &Region, x: f64, y: f64) -> bool {
    region.iter().fold(false, |inside, (add, r)| if r.contains(x, y) { *add } else { inside })
}

pub enum Role {
    None,
    Cursor,
    /// `sync`: state is applied when the parent commits (the default).
    Subsurface { parent: u32, x: i32, y: i32, sync: bool },
    Xdg,
    /// Paired by Xwayland with an X window.
    Xwayland,
    /// The image of a drag.
    DndIcon,
}

pub enum XdgRole {
    None,
    Toplevel {
        obj: u32,
        title: String,
        app_id: String,
        decoration: Option<u32>,
        sent: Option<(i32, i32, bool, Option<wm::FsMode>)>,
        /// The parent toplevel's surface: a dialog.
        parent: Option<u32>,
        min: (i32, i32),
        max: (i32, i32),
    },
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
    /// What changed in the buffer since the last upload; `None`: all of it.
    pub damage: Option<Vec<Rect>>,
    pub content: Content,
    pub size: (i32, i32),
    pub opaque: bool,
    pub frames: Vec<u32>,
    pub role: Role,
    pub above: Vec<u32>,
    pub below: Vec<u32>,
    pub xdg: Option<Xdg>,
    /// Where the surface takes pointer input; None is everywhere.
    pub input: Option<Region>,
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
    pub constraint_adjustment: u32,
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

/// What the launcher box is showing.
#[derive(Clone, PartialEq)]
pub enum Purpose {
    Apps,
    /// The clipboard history, by hash of each entry's data.
    Clipboard(Vec<u64>),
    /// The notification list; item 0 clears it, then these ids.
    Notifications(Vec<u32>),
    /// Sources for a screen cast asked by the portal (`request`, 0 for none).
    Cast(u32, Vec<CastChoice>),
    Wifi(Vec<crate::wifi::Network>),
    /// Countdown lengths in seconds; 0 stops the timer.
    Timer(Vec<u64>),
    Power,
    Battery,
    /// Sink names.
    Outputs(Vec<String>),
    /// Adapter power, then the paired devices.
    Bluetooth(bool, Vec<crate::services::BtDevice>),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CastChoice {
    Output,
    Window(SKey),
}

/// A password box: the lock screen, or a prompt over the desktop (polkit,
/// Wi-Fi). The compositor draws its card when `dirty`.
#[derive(Default)]
pub struct PromptView {
    pub title: String,
    pub message: String,
    /// Wiped when dropped.
    pub typed: String,
    pub error: String,
    /// Waiting for the answer to a submitted password.
    pub busy: bool,
    pub dirty: bool,
    /// Rendered card: height and pixels, waiting for upload.
    pub card: Option<Card>,
    tex: Option<Texture>,
    size: (i32, i32),
}

impl PromptView {
    pub fn new(title: &str, message: &str) -> PromptView {
        let mut p = PromptView::default();
        (p.title, p.message, p.dirty) = (title.into(), message.into(), true);
        p
    }

    /// Edits from a key: true when the user submitted (Enter).
    pub fn key(&mut self, sym: u32, text: Option<char>) -> bool {
        if self.busy {
            return false;
        }
        match sym {
            0xff0d | 0xff8d => return !self.typed.is_empty(),
            0xff08 => {
                self.typed.pop();
            }
            0xff1b => self.typed_clear(),
            _ => match text.filter(|c| !c.is_control()) {
                Some(c) => self.typed.push(c),
                None => return false,
            },
        }
        self.error.clear();
        self.dirty = true;
        false
    }

    /// Takes the typed password out, leaving the field empty.
    pub fn take(&mut self) -> String {
        self.busy = true;
        self.dirty = true;
        std::mem::take(&mut self.typed)
    }

    fn typed_clear(&mut self) {
        let mut b = std::mem::take(&mut self.typed).into_bytes();
        b.fill(0);
    }

    pub fn failed(&mut self, error: &str) {
        self.busy = false;
        self.error = error.into();
        self.dirty = true;
    }

    fn draw(&mut self, r: &mut Renderer, output: &Output) {
        if let Some((h, px)) = self.card.take() {
            let w = launcher::card::PROMPT_WIDTH as i32;
            self.tex = Some(r.upload(self.tex.take(), w, h as i32, w * 4, &px));
            self.size = (w, h as i32);
        }
        if let Some(t) = &self.tex {
            let (w, h) = self.size;
            let (x, y) = ((output.width - w) / 2, (output.height - h) / 2);
            r.draw(t, Quad { x: x as f32, y: y as f32, w: w as f32, h: h as f32 }, true);
        }
    }
}

impl Drop for PromptView {
    fn drop(&mut self) {
        self.typed_clear();
    }
}

/// The screenshot selector: the frozen screen, dimmed outside the selection.
pub struct ShotView {
    pub pixels: Vec<u8>,
    pub width: i32,
    pub height: i32,
    tex: Option<Texture>,
    /// Where the drag started.
    pub anchor: Option<(f64, f64)>,
    /// The portal request it answers, 0 for the key binding.
    pub request: u32,
}

impl ShotView {
    pub fn new(pixels: Vec<u8>, width: i32, height: i32, request: u32) -> ShotView {
        ShotView { pixels, width, height, tex: None, anchor: None, request }
    }
}

/// A rendered card: height and ARGB pixels.
pub type Card = (u32, Vec<u8>);

/// A notification popup.
pub struct NoticeView {
    pub id: u32,
    pub rect: Rect,
    /// Rendered card waiting for upload.
    pub pixels: Option<Vec<u8>>,
    pub tex: Option<Texture>,
}

pub struct LauncherView {
    pub launcher: launcher::Launcher,
    pub purpose: Purpose,
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
    /// The render node clients should allocate on (a dev_t).
    pub dmabuf_device: u64,
    pub positioners: HashMap<SKey, Positioner>,
    pub regions: HashMap<SKey, Region>,
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
    /// Notification popups, top to bottom.
    pub notices: Vec<NoticeView>,
    /// The volume/brightness indicator.
    pub osd: Option<NoticeView>,
    /// The month, shown under the clock.
    pub calendar: Option<NoticeView>,
    /// A panel in the middle of the screen (the mixer).
    pub panel: Option<NoticeView>,
    pub shot: Option<ShotView>,
    /// The lock screen: while set, nothing but it is drawn or gets input.
    pub lock: Option<PromptView>,
    /// A password prompt over the desktop.
    pub prompt: Option<PromptView>,
    pub drag: Option<dnd::Drag>,
    pub dropped: Option<dnd::Dropped>,
    /// For screenshots: draw without the pointer.
    pub hide_cursor: bool,
    /// Pickers closed without a choice, for whoever opened them.
    pub abandoned: Vec<Purpose>,
    /// The client that is Xwayland: only it may bind `xwayland_shell_v1`.
    pub xwayland: Option<ClientId>,
    /// Serials announced for Xwayland surfaces not paired yet.
    pub xwayland_serials: HashMap<u32, u64>,
    pub xwm: Option<xwm::Xwm>,
    serial: u32,
    start: Instant,
    /// The whole output must be drawn again.
    pub needs_redraw: bool,
    /// A surface committed a buffer not uploaded yet: hidden windows too,
    /// whose clients wait for the buffer back.
    pub uploads_pending: bool,
    /// Areas to draw again when `needs_redraw` is not set.
    pub damage: Vec<Rect>,
    /// The pointer moved or changed image.
    pub cursor_changed: bool,
    /// The cursor surface committed: the hardware cursor needs its image again.
    pub cursor_image_dirty: bool,
    /// Where the software cursor was drawn last.
    pub cursor_drawn: Option<Rect>,
    /// The cursor is on its own plane: the scene does not draw it.
    pub hw_cursor: bool,
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
            dmabuf_device: 0,
            positioners: HashMap::new(),
            regions: HashMap::new(),
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
            notices: Vec::new(),
            osd: None,
            calendar: None,
            panel: None,
            shot: None,
            lock: None,
            prompt: None,
            drag: None,
            dropped: None,
            hide_cursor: false,
            abandoned: Vec::new(),
            xwayland: None,
            xwayland_serials: HashMap::new(),
            xwm: None,
            serial: 0,
            start: Instant::now(),
            needs_redraw: true,
            uploads_pending: false,
            damage: Vec::new(),
            cursor_changed: false,
            cursor_image_dirty: false,
            cursor_drawn: None,
            hw_cursor: false,
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
            (&wire::zwp_linux_dmabuf_v1::INTERFACE, 4),
            (&wire::xwayland_shell_v1::INTERFACE, 1),
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
            Kind::XdgPopup { surface } => xdg::popup(self, c, surface, op, args),
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
            Kind::Dmabuf | Kind::DmabufParams | Kind::DmabufFeedback => dmabuf::request(self, c, id, kind, version, op, args),
            Kind::DragOffer => dnd::offer_request(self, c, id, op, args),
            Kind::XwaylandShell | Kind::XwaylandSurface { .. } => xwm::request(self, c, id, kind, version, op, args),
            Kind::Region => compositor::region(self, c, id, op, args),
            Kind::Callback | Kind::Keyboard | Kind::Touch | Kind::Output => {}
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
        if self.xwayland == Some(client) {
            log!("Xwayland disconnected");
            xwm::stop(self);
            self.xwayland = None;
            self.xwayland_serials.clear();
        }
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
        self.regions.retain(|k, _| k.client != client);
        self.idle_inhibitors.retain(|k, _| k.client != client);
        selection::client_gone(self, client);
        dnd::client_gone(self, client);
        self.needs_redraw = true;
    }

    fn drop_buffer(&mut self, key: SKey) {
        if let Some(Buffer::Dmabuf { tex: Some(t), .. }) = self.buffers.remove(&key) {
            self.dead_textures.push(t);
        }
    }

    pub fn destroy_surface(&mut self, key: SKey) {
        xdg::unmap(self, key);
        xwm::surface_gone(self, key);
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
        if let Some(d) = self.drag.as_mut().filter(|d| d.icon == Some(key)) {
            d.icon = None;
        }
        self.needs_redraw = true;
    }

    pub fn open_launcher(&mut self) {
        self.open_view(launcher::Launcher::open(), Purpose::Apps);
    }

    /// Opens the launcher box (closing whatever it showed before).
    pub fn open_view(&mut self, l: Result<launcher::Launcher, String>, purpose: Purpose) {
        self.close_launcher();
        match l {
            Ok(l) => {
                let prev_focus = self.focus;
                seat::set_focus(self, None);
                self.launcher = Some(LauncherView { launcher: l, purpose, tex: None, dirty: true, prev_focus });
                self.needs_redraw = true;
            }
            Err(e) => log!("launcher: {e}"),
        }
    }

    /// Replaces the popups: (id, height, pixels) top to bottom, or None
    /// to keep the card already drawn for that id.
    pub fn set_notices(&mut self, cards: Vec<(u32, Option<Card>)>) {
        let mut old = std::mem::take(&mut self.notices);
        let x = self.output.width - launcher::card::CARD_WIDTH as i32 - 12;
        let mut y = if self.bar.visible { self.bar.height } else { 0 } + 12;
        for (id, card) in cards {
            let prev = old.iter().position(|n| n.id == id).map(|i| old.remove(i));
            let view = match (card, prev) {
                (Some((h, px)), prev) => {
                    if let Some(t) = prev.and_then(|p| p.tex) {
                        self.dead_textures.push(t);
                    }
                    NoticeView { id, rect: Rect::new(x, y, launcher::card::CARD_WIDTH as i32, h as i32), pixels: Some(px), tex: None }
                }
                (None, Some(mut p)) => {
                    p.rect.x = x;
                    p.rect.y = y;
                    p
                }
                (None, None) => continue,
            };
            y += view.rect.h + 8;
            self.notices.push(view);
        }
        self.dead_textures.extend(old.into_iter().filter_map(|n| n.tex));
        self.needs_redraw = true;
    }

    /// Shows the indicator (bottom centre), or hides it.
    pub fn set_osd(&mut self, card: Option<Card>) {
        if let Some(t) = self.osd.take().and_then(|o| o.tex) {
            self.dead_textures.push(t);
        }
        self.osd = card.map(|(h, px)| {
            let w = launcher::card::OSD_WIDTH as i32;
            let rect = Rect::new((self.output.width - w) / 2, self.output.height - h as i32 - 80, w, h as i32);
            NoticeView { id: 0, rect, pixels: Some(px), tex: None }
        });
        self.needs_redraw = true;
    }

    /// Shows a panel of `width` in the middle of the screen, or hides it.
    pub fn set_panel(&mut self, width: u32, card: Option<Card>) {
        if let Some(t) = self.panel.take().and_then(|o| o.tex) {
            self.dead_textures.push(t);
        }
        self.panel = card.map(|(h, px)| {
            let (w, h) = (width as i32, h as i32);
            NoticeView { id: 0, rect: Rect::new((self.output.width - w) / 2, (self.output.height - h) / 2, w, h), pixels: Some(px), tex: None }
        });
        self.needs_redraw = true;
    }

    /// Shows the calendar under the right end of the bar, or hides it.
    pub fn set_calendar(&mut self, card: Option<Card>) {
        if let Some(t) = self.calendar.take().and_then(|o| o.tex) {
            self.dead_textures.push(t);
        }
        self.calendar = card.map(|(h, px)| {
            let w = launcher::card::CALENDAR_WIDTH as i32;
            let top = if self.bar.visible { self.bar.height } else { 0 } + 8;
            NoticeView { id: 0, rect: Rect::new(self.output.width - w - 12, top, w, h as i32), pixels: Some(px), tex: None }
        });
        self.needs_redraw = true;
    }

    pub fn notice_at(&self, x: f64, y: f64) -> Option<u32> {
        self.notices.iter().find(|n| n.rect.contains(x, y)).map(|n| n.id)
    }

    pub fn close_launcher(&mut self) {
        let Some(l) = self.launcher.take() else { return };
        self.abandoned.push(l.purpose.clone());
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
        xwm::close(self, key);
        if let Some(XdgRole::Toplevel { obj, .. }) = self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            let obj = *obj;
            self.send(key.client, Msg::new(obj, wire::xdg_toplevel::evt::CLOSE));
        }
    }

    pub fn app_id(&self, key: SKey) -> &str {
        if let Some(w) = xwm::window(self, key) {
            return &w.class;
        }
        match self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            Some(XdgRole::Toplevel { app_id, .. }) => app_id,
            _ => "",
        }
    }

    pub fn title(&self, key: SKey) -> &str {
        if let Some(w) = xwm::window(self, key) {
            return &w.title;
        }
        match self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            Some(XdgRole::Toplevel { title, .. }) => title,
            _ => "",
        }
    }

    /// Dialogs and fixed-size windows float instead of tiling.
    pub fn wants_float(&self, key: SKey) -> bool {
        if let Some(w) = xwm::window(self, key) {
            return w.floats();
        }
        match self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            Some(XdgRole::Toplevel { parent, min, max, .. }) => parent.is_some() || (min == max && min.0 > 0 && min.1 > 0),
            _ => false,
        }
    }

    pub fn is_floating(&self, key: SKey) -> bool {
        self.window_ws.get(&key).and_then(|ws| self.workspaces.get(ws)).is_some_and(|w| w.is_floating(key))
    }

    /// Floating windows get their current size; new ones are centred.
    fn place_floats(&mut self, ws: WsId) {
        let work = self.work_area();
        let keys: Vec<SKey> = self.workspaces.get(&ws).map(|w| w.floating.iter().map(|(k, _)| *k).collect()).unwrap_or_default();
        for k in keys {
            let Some((w, h)) = self.window_size(k) else { continue };
            let Some(f) = self.workspaces.get_mut(&ws).and_then(|ws| ws.floating.iter_mut().find(|(t, _)| *t == k)) else { continue };
            let r = &mut f.1;
            if r.w == 0 {
                r.x = work.x + ((work.w - w) / 2).max(0);
                r.y = work.y + ((work.h - h) / 2).max(0);
            }
            (r.w, r.h) = (w, h);
        }
    }

    /// A floating window committed: follow its new size.
    pub fn float_resized(&mut self, key: SKey) {
        let Some(&ws) = self.window_ws.get(&key) else { return };
        let current = self.workspaces.get(&ws).and_then(|w| w.floating.iter().find(|(t, _)| *t == key)).map(|(_, r)| (r.w, r.h));
        if current.is_some() && current != self.window_size(key) {
            self.relayout();
        }
    }

    /// A window that is on screen (as far as its role is concerned).
    pub fn shown(&self, key: SKey) -> bool {
        self.surfaces.get(&key).and_then(|s| s.xdg.as_ref()).is_some_and(|x| x.mapped) || xwm::window(self, key).is_some_and(|w| w.shown)
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
        if self.bar.visible {
            self.damage(Rect::new(0, 0, width as i32, height as i32));
        }
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
        let mut list: Vec<WsId> = self.workspaces.iter().filter(|(_, w)| !w.is_empty()).map(|(id, _)| *id).collect();
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
                let title = self.title(*k).to_string();
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
        self.place_floats(self.active);
        if self.special_shown {
            self.place_floats(WsId::Special);
        }
        let (work, out) = (self.work_area(), self.output_rect());
        let mut layout = self.workspaces.get(&self.active).map(|w| w.layout(work, out)).unwrap_or_default();
        self.special_start = layout.len();
        if self.special_shown {
            layout.extend(self.workspaces.get(&WsId::Special).map(|w| w.layout(work, out)).unwrap_or_default());
        }
        self.layout = layout;
        for (key, rect) in self.layout.clone() {
            xdg::configure_toplevel(self, key, rect.w, rect.h);
            xwm::configure(self, key, rect);
        }
        // Floating windows not placed yet choose their own size.
        let unplaced: Vec<SKey> = [Some(self.active), self.special_shown.then_some(WsId::Special)]
            .into_iter()
            .flatten()
            .filter_map(|ws| self.workspaces.get(&ws))
            .flat_map(|w| w.floating.iter().filter(|(_, r)| r.w == 0).map(|(k, _)| *k))
            .collect();
        for key in unplaced {
            xdg::configure_toplevel(self, key, 0, 0);
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

    pub fn damage(&mut self, r: Rect) {
        if r.w > 0 && r.h > 0 {
            self.damage.push(r);
        }
    }

    /// Where the pointer's image is, if any.
    pub fn cursor_rect(&self) -> Option<Rect> {
        let (x, y) = (self.pointer.x as i32, self.pointer.y as i32);
        match self.pointer.cursor {
            CursorImage::Hidden => None,
            CursorImage::Default => Some(Rect::new(x, y, 10, 10)),
            CursorImage::Surface(k, hx, hy) => self.surfaces.get(&k).map(|s| Rect::new(x - hx, y - hy, s.size.0.max(1), s.size.1.max(1))),
        }
    }

    /// A surface committed with these damaged areas (empty: unknown). In
    /// a tiled window only those areas are redrawn; anything else gets its
    /// whole window redrawn.
    pub fn damage_committed(&mut self, key: SKey, areas: &[Rect]) {
        // Subsurface offsets up to the window's surface.
        let (mut root, mut dx, mut dy) = (key, 0, 0);
        while let Some(Role::Subsurface { parent, x, y, .. }) = self.surfaces.get(&root).map(|s| &s.role) {
            (dx, dy) = (dx + x, dy + y);
            root.id = *parent;
        }
        let xdg = self.surfaces.get(&root).is_some_and(|s| matches!(s.role, Role::Xdg));
        let tile = self.layout.iter().find(|(k, _)| *k == root).map(|(_, r)| *r);
        let (Some(r), true) = (tile, xdg && !areas.is_empty() && areas.len() <= 16) else {
            self.damage_surface(key);
            return;
        };
        let (ox, oy) = self.surface_origin(root, (r.x, r.y));
        let (ox, oy) = (ox + dx, oy + dy);
        for d in areas {
            let (x0, y0) = ((ox + d.x).max(r.x), (oy + d.y).max(r.y));
            let (x1, y1) = (ox.saturating_add(d.x).saturating_add(d.w).min(r.x + r.w), oy.saturating_add(d.y).saturating_add(d.h).min(r.y + r.h));
            self.damage(Rect::new(x0, y0, x1 - x0, y1 - y0));
        }
    }

    /// A surface committed: the area of its window (or popup) on screen
    /// needs drawing; nothing when it is not shown.
    pub fn damage_surface(&mut self, key: SKey) {
        let mut root = key;
        while let Some(Role::Subsurface { parent, .. }) = self.surfaces.get(&root).map(|s| &s.role) {
            root.id = *parent;
        }
        match self.surfaces.get(&root).map(|s| &s.role) {
            Some(Role::Cursor) => {
                self.cursor_changed = true;
                self.cursor_image_dirty = true;
                return;
            }
            Some(Role::DndIcon) => {
                self.needs_redraw = true;
                return;
            }
            None => return,
            _ => {}
        }
        if let Some((_, r)) = self.layout.iter().find(|(k, _)| *k == root) {
            self.damage(*r);
            return;
        }
        let size = self.surfaces.get(&root).map(|s| s.size).unwrap_or((0, 0));
        if let Some(XdgRole::Popup { .. }) = self.surfaces.get(&root).and_then(|s| s.xdg.as_ref()).map(|x| &x.role) {
            if let Some(origin) = self.window_origin(root) {
                let (x, y) = self.surface_origin(root, origin);
                self.damage(Rect::new(x, y, size.0, size.1));
            }
            return;
        }
        if let Some((_, (x, y))) = xwm::floating(self).into_iter().find(|(k, _)| *k == root) {
            self.damage(Rect::new(x, y, size.0, size.1));
        }
    }

    /// Uploads committed buffers (the hardware cursor reads its image).
    pub fn upload_pending(&mut self, r: &mut Renderer) {
        self.upload_buffers(r);
    }

    /// The pointer's image for a cursor plane of at most `max`: width,
    /// height and ARGB8888 bytes (B, G, R, A). None when it does not fit.
    pub fn cursor_pixels(&self, r: &mut Renderer, max: (u32, u32)) -> Option<(u32, u32, Vec<u8>)> {
        match self.pointer.cursor {
            CursorImage::Hidden => Some((1, 1, vec![0; 4])),
            CursorImage::Default => {
                let px = (0..100).flat_map(|i| {
                    let (x, y) = (i % 10, i / 10);
                    let border = x == 0 || y == 0 || x == 9 || y == 9;
                    if border { [0, 0, 0, 255] } else { [255, 255, 255, 255] }
                });
                Some((10, 10, px.collect()))
            }
            CursorImage::Surface(k, ..) => {
                let t = self.texture(k)?;
                let (w, h) = (t.width as u32, t.height as u32);
                if w > max.0 || h > max.1 {
                    return None;
                }
                let mut px = r.read_texture(t);
                for p in px.as_chunks_mut::<4>().0 {
                    p.swap(0, 2);
                }
                Some((w, h, px))
            }
        }
    }

    pub fn render(&mut self, r: &mut Renderer, target: &Target) {
        self.render_damaged(r, target, None);
    }

    /// Draws the scene; with `region` only that area is drawn again (the
    /// rest of `target` must already show the current scene).
    pub fn render_damaged(&mut self, r: &mut Renderer, target: &Target, region: Option<Rect>) {
        self.damage.clear();
        for t in self.dead_textures.drain(..) {
            r.destroy_texture(t);
        }
        if let Some(l) = &mut self.lock {
            // Nothing of the session shows while locked; clients wait.
            r.begin(target, [0.0, 0.0, 0.0, 1.0]);
            l.draw(r, &self.output);
            r.finish();
            self.needs_redraw = false;
            return;
        }
        self.upload_buffers(r);
        let quad = |q: Rect| Quad { x: q.x as f32, y: q.y as f32, w: q.w as f32, h: q.h as f32 };
        r.begin_region(target, config::BACKGROUND, region.map(quad));
        self.draw_background(r);
        if let Some(t) = self.bar.tex.as_ref().filter(|_| self.bar.visible) {
            r.draw(t, Quad { x: 0.0, y: 0.0, w: t.width as f32, h: t.height as f32 }, true);
        }
        for (i, (key, rect)) in self.layout.clone().into_iter().enumerate() {
            if i == self.special_start {
                r.fill(Quad { x: 0.0, y: 0.0, w: self.output.width as f32, h: self.output.height as f32 }, config::SPECIAL_DIM);
            }
            if !self.shown(key) {
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
        for (key, origin) in xwm::floating(self) {
            self.draw_tree(r, key, origin);
        }
        for n in self.notices.iter_mut().chain(self.osd.as_mut()).chain(self.calendar.as_mut()).chain(self.panel.as_mut()) {
            if let Some(px) = n.pixels.take() {
                n.tex = Some(r.upload(n.tex.take(), n.rect.w, n.rect.h, n.rect.w * 4, &px));
            }
            if let Some(t) = &n.tex {
                r.draw(t, Quad { x: n.rect.x as f32, y: n.rect.y as f32, w: n.rect.w as f32, h: n.rect.h as f32 }, true);
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
        if let Some(p) = &mut self.prompt {
            r.fill(Quad { x: 0.0, y: 0.0, w: self.output.width as f32, h: self.output.height as f32 }, [0.0, 0.0, 0.0, 0.5]);
            p.draw(r, &self.output);
        }
        dnd::draw_icon(self, r);
        self.draw_shot(r);
        self.cursor_drawn = None;
        if !self.hide_cursor && !self.hw_cursor {
            seat::draw_cursor(self, r);
            self.cursor_drawn = self.cursor_rect();
        }
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

    /// The selection: the dragged rectangle, else the window under the
    /// pointer, else the whole output.
    pub fn shot_selection(&self) -> Rect {
        let (px, py) = (self.pointer.x, self.pointer.y);
        let dragged = self.shot.as_ref().and_then(|v| v.anchor).filter(|(ax, ay)| (ax - px).abs() > 4.0 || (ay - py).abs() > 4.0);
        match dragged {
            Some((ax, ay)) => {
                let (x0, y0) = (ax.min(px).max(0.0) as i32, ay.min(py).max(0.0) as i32);
                let (x1, y1) = (ax.max(px).min(self.output.width as f64) as i32, ay.max(py).min(self.output.height as f64) as i32);
                Rect::new(x0, y0, (x1 - x0).max(1), (y1 - y0).max(1))
            }
            None => self.layout.iter().rev().find(|(_, r)| r.contains(px, py)).map(|(_, r)| *r).unwrap_or(self.output_rect()),
        }
    }

    /// Removes the lock screen or the prompt, freeing its texture.
    pub fn drop_prompt(&mut self, lock: bool) {
        let slot = if lock { &mut self.lock } else { &mut self.prompt };
        if let Some(mut p) = slot.take() {
            self.dead_textures.extend(p.tex.take());
        }
        self.needs_redraw = true;
        self.pointer_dirty = true;
    }

    /// Closes the selector and hands back what it showed.
    pub fn close_shot(&mut self) -> Option<ShotView> {
        let mut v = self.shot.take()?;
        self.dead_textures.extend(v.tex.take());
        self.needs_redraw = true;
        Some(v)
    }

    fn draw_shot(&mut self, r: &mut Renderer) {
        let sel = self.shot_selection();
        let Some(v) = &mut self.shot else { return };
        let tex = v.tex.get_or_insert_with(|| r.upload_rgba(v.width, v.height, &v.pixels));
        let full = Quad { x: 0.0, y: 0.0, w: v.width as f32, h: v.height as f32 };
        r.draw(tex, full, true);
        r.fill(full, [0.0, 0.0, 0.0, 0.45]);
        let q = |x: i32, y: i32, w: i32, h: i32| Quad { x: x as f32, y: y as f32, w: w as f32, h: h as f32 };
        r.draw_cropped(tex, q(sel.x, sel.y, sel.w, sel.h), q(sel.x, sel.y, sel.w, sel.h));
        let white = [1.0, 1.0, 1.0, 1.0];
        for b in [q(sel.x, sel.y, sel.w, 2), q(sel.x, sel.y + sel.h - 2, sel.w, 2), q(sel.x, sel.y, 2, sel.h), q(sel.x + sel.w - 2, sel.y, 2, sel.h)] {
            r.fill(b, white);
        }
    }

    /// Size of a window's content (its window geometry, else its surface).
    pub fn window_size(&self, key: SKey) -> Option<(i32, i32)> {
        if let Some(w) = xwm::window(self, key) {
            return Some((w.geometry.w, w.geometry.h)).filter(|(w, h)| *w > 0 && *h > 0);
        }
        let sf = self.surfaces.get(&key)?;
        let g = sf.xdg.as_ref().and_then(|x| x.geometry);
        Some(g.map_or(sf.size, |g| (g.w, g.h))).filter(|(w, h)| *w > 0 && *h > 0)
    }

    /// Draws one window alone into `target`, for a window cast.
    pub fn render_window(&mut self, r: &mut Renderer, key: SKey, target: &Target) {
        r.begin(target, [0.0, 0.0, 0.0, 1.0]);
        let o = self.surface_origin(key, (0, 0));
        self.draw_tree(r, key, o);
        r.finish();
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
        self.uploads_pending = false;
        self.import_shown_dmabufs(r);
        let keys: Vec<SKey> = self.surfaces.iter().filter(|(_, s)| s.upload.is_some()).map(|(k, _)| *k).collect();
        for key in keys {
            let sf = self.surfaces.get_mut(&key).unwrap();
            let bid = sf.upload.take().unwrap();
            let damage = sf.damage.replace(Vec::new());
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
                    let areas = damage.and_then(|d| changed_areas(&d, width, height));
                    let (tex, faulted) = compositor::guarded_read(ptr, len, |pixels| match (old, areas) {
                        (Some(t), Some(areas)) if (t.width, t.height) == (width, height) => {
                            r.update(&t, &areas, stride, pixels);
                            t
                        }
                        (old, _) => r.upload(old, width, height, stride, pixels),
                    });
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

/// The buffer areas to upload, clipped to it; `None` when uploading it
/// all is about as cheap.
fn changed_areas(damage: &[Rect], width: i32, height: i32) -> Option<Vec<(i32, i32, i32, i32)>> {
    let mut areas = Vec::with_capacity(damage.len());
    let mut total = 0i64;
    for d in damage {
        let (x0, y0) = (d.x.clamp(0, width), d.y.clamp(0, height));
        let (x1, y1) = (d.x.saturating_add(d.w).clamp(0, width), d.y.saturating_add(d.h).clamp(0, height));
        if x1 > x0 && y1 > y0 {
            areas.push((x0, y0, x1 - x0, y1 - y0));
            total += (x1 - x0) as i64 * (y1 - y0) as i64;
        }
    }
    (total * 2 < width as i64 * height as i64 && areas.len() <= 16).then_some(areas)
}
