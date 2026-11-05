//! Live upgrade of the server state (README, ADR-014).
//!
//! Plain data goes through `Persist`; textures are read back and uploaded
//! again, shm pools are mapped again, everything GL-related is rebuilt.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use render::Renderer;
use state::{persist_struct, Dec, Enc, Persist, Result};

use super::*;

persist_struct!(SKey { client, id });
persist_struct!(Pending { buffer, frames, geometry });
persist_struct!(Xdg { obj, role, geometry, configured, mapped });
persist_struct!(DmabufParams { planes, modifier, used });
persist_struct!(Positioner { size, anchor_rect, anchor, gravity, offset });
persist_struct!(Output { name, width, height });
persist_struct!(PointerState { x, y, focus, cursor });

fn bad<T>(what: &str) -> Result<T> {
    Err(state::Error(format!("bad {what}")))
}

impl Persist for Kind {
    fn save(&self, e: &mut Enc) {
        let (tag, a, b): (u8, u32, u32) = match *self {
            Kind::Display => (0, 0, 0),
            Kind::Registry => (1, 0, 0),
            Kind::Callback => (2, 0, 0),
            Kind::Compositor => (3, 0, 0),
            Kind::Surface => (4, 0, 0),
            Kind::Region => (5, 0, 0),
            Kind::Subcompositor => (6, 0, 0),
            Kind::Subsurface { surface } => (7, surface, 0),
            Kind::Shm => (8, 0, 0),
            Kind::ShmPool => (9, 0, 0),
            Kind::Buffer => (10, 0, 0),
            Kind::Seat => (11, 0, 0),
            Kind::Pointer => (12, 0, 0),
            Kind::Keyboard => (13, 0, 0),
            Kind::Touch => (14, 0, 0),
            Kind::Output => (15, 0, 0),
            Kind::XdgWmBase => (16, 0, 0),
            Kind::XdgPositioner => (17, 0, 0),
            Kind::XdgSurface { surface } => (18, surface, 0),
            Kind::XdgToplevel { surface } => (19, surface, 0),
            Kind::XdgPopup { surface } => (20, surface, 0),
            Kind::DecorationManager => (21, 0, 0),
            Kind::Decoration { surface } => (22, surface, 0),
            Kind::DataDeviceManager => (23, 0, 0),
            Kind::DataSource => (24, 0, 0),
            Kind::DataDevice => (25, 0, 0),
            Kind::DataOffer { generation } => (26, generation, 0),
            Kind::PrimaryManager => (27, 0, 0),
            Kind::PrimarySource => (28, 0, 0),
            Kind::PrimaryDevice => (29, 0, 0),
            Kind::PrimaryOffer { generation } => (30, generation, 0),
            Kind::ControlManager => (31, 0, 0),
            Kind::ControlSource => (32, 0, 0),
            Kind::ControlDevice => (33, 0, 0),
            Kind::ControlOffer { generation, primary } => (34, generation, primary as u32),
            Kind::Activation => (35, 0, 0),
            Kind::ActivationToken => (36, 0, 0),
            Kind::Bell => (37, 0, 0),
            Kind::IdleInhibitManager => (38, 0, 0),
            Kind::IdleInhibitor { surface } => (39, surface, 0),
            Kind::Dmabuf => (40, 0, 0),
            Kind::DmabufParams => (41, 0, 0),
            Kind::XwaylandShell => (42, 0, 0),
            Kind::DmabufFeedback => (44, 0, 0),
            Kind::XwaylandSurface { surface } => (43, surface, 0),
        };
        (tag, a, b).save(e);
    }

    fn load(d: &mut Dec) -> Result<Self> {
        let (tag, a, b) = <(u8, u32, u32)>::load(d)?;
        Ok(match tag {
            0 => Kind::Display,
            1 => Kind::Registry,
            2 => Kind::Callback,
            3 => Kind::Compositor,
            4 => Kind::Surface,
            5 => Kind::Region,
            6 => Kind::Subcompositor,
            7 => Kind::Subsurface { surface: a },
            8 => Kind::Shm,
            9 => Kind::ShmPool,
            10 => Kind::Buffer,
            11 => Kind::Seat,
            12 => Kind::Pointer,
            13 => Kind::Keyboard,
            14 => Kind::Touch,
            15 => Kind::Output,
            16 => Kind::XdgWmBase,
            17 => Kind::XdgPositioner,
            18 => Kind::XdgSurface { surface: a },
            19 => Kind::XdgToplevel { surface: a },
            20 => Kind::XdgPopup { surface: a },
            21 => Kind::DecorationManager,
            22 => Kind::Decoration { surface: a },
            23 => Kind::DataDeviceManager,
            24 => Kind::DataSource,
            25 => Kind::DataDevice,
            26 => Kind::DataOffer { generation: a },
            27 => Kind::PrimaryManager,
            28 => Kind::PrimarySource,
            29 => Kind::PrimaryDevice,
            30 => Kind::PrimaryOffer { generation: a },
            31 => Kind::ControlManager,
            32 => Kind::ControlSource,
            33 => Kind::ControlDevice,
            34 => Kind::ControlOffer { generation: a, primary: b != 0 },
            35 => Kind::Activation,
            36 => Kind::ActivationToken,
            37 => Kind::Bell,
            38 => Kind::IdleInhibitManager,
            39 => Kind::IdleInhibitor { surface: a },
            40 => Kind::Dmabuf,
            41 => Kind::DmabufParams,
            42 => Kind::XwaylandShell,
            44 => Kind::DmabufFeedback,
            43 => Kind::XwaylandSurface { surface: a },
            _ => return bad("object kind"),
        })
    }
}

impl Persist for Object {
    fn save(&self, e: &mut Enc) {
        self.iface.name.to_string().save(e);
        self.version.save(e);
        self.kind.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let name = String::load(d)?;
        let iface = wire::interface_by_name(&name).ok_or_else(|| state::Error(format!("unknown interface {name}")))?;
        Ok(Object { iface, version: Persist::load(d)?, kind: Persist::load(d)? })
    }
}

persist_struct!(Client { conn, objects, dead, next_server_id });

impl Persist for Role {
    fn save(&self, e: &mut Enc) {
        match *self {
            Role::None => (0u8, 0u32, 0i32, 0i32, false).save(e),
            Role::Cursor => (1u8, 0u32, 0i32, 0i32, false).save(e),
            Role::Subsurface { parent, x, y, sync } => (2u8, parent, x, y, sync).save(e),
            Role::Xdg => (3u8, 0u32, 0i32, 0i32, false).save(e),
            Role::Xwayland => (4u8, 0u32, 0i32, 0i32, false).save(e),
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let (tag, parent, x, y, sync) = <(u8, u32, i32, i32, bool)>::load(d)?;
        Ok(match tag {
            0 => Role::None,
            1 => Role::Cursor,
            2 => Role::Subsurface { parent, x, y, sync },
            3 => Role::Xdg,
            4 => Role::Xwayland,
            _ => return bad("role"),
        })
    }
}

impl Persist for XdgRole {
    fn save(&self, e: &mut Enc) {
        match self {
            XdgRole::None => 0u8.save(e),
            XdgRole::Toplevel { obj, title, app_id, decoration, sent } => {
                1u8.save(e);
                (*obj, title.clone(), app_id.clone(), *decoration).save(e);
                sent.save(e);
            }
            XdgRole::Popup { obj, parent, geo } => {
                2u8.save(e);
                (*obj, *parent, *geo).save(e);
            }
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(match u8::load(d)? {
            0 => XdgRole::None,
            1 => {
                let (obj, title, app_id, decoration) = Persist::load(d)?;
                XdgRole::Toplevel { obj, title, app_id, decoration, sent: Persist::load(d)? }
            }
            2 => {
                let (obj, parent, geo) = Persist::load(d)?;
                XdgRole::Popup { obj, parent, geo }
            }
            _ => return bad("xdg role"),
        })
    }
}

impl Persist for CursorImage {
    fn save(&self, e: &mut Enc) {
        match *self {
            CursorImage::Default => (0u8, None::<SKey>, 0i32, 0i32).save(e),
            CursorImage::Hidden => (1u8, None::<SKey>, 0i32, 0i32).save(e),
            CursorImage::Surface(k, x, y) => (2u8, Some(k), x, y).save(e),
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(match <(u8, Option<SKey>, i32, i32)>::load(d)? {
            (2, Some(k), x, y) => CursorImage::Surface(k, x, y),
            (1, ..) => CursorImage::Hidden,
            _ => CursorImage::Default,
        })
    }
}

impl Persist for Buffer {
    fn save(&self, e: &mut Enc) {
        match self {
            Buffer::Shm { pool, offset, width, height, stride, opaque } => {
                0u8.save(e);
                (*pool, *offset, *width, *height).save(e);
                (*stride, *opaque).save(e);
            }
            // The texture is imported again on first use.
            Buffer::Dmabuf { width, height, format, modifier, planes, tex: _, failed: _ } => {
                1u8.save(e);
                (*width, *height, *format, *modifier).save(e);
                planes.save(e);
            }
        }
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(match u8::load(d)? {
            0 => {
                let (pool, offset, width, height) = Persist::load(d)?;
                let (stride, opaque) = Persist::load(d)?;
                Buffer::Shm { pool, offset, width, height, stride, opaque }
            }
            1 => {
                let (width, height, format, modifier) = Persist::load(d)?;
                Buffer::Dmabuf { width, height, format, modifier, planes: Persist::load(d)?, tex: None, failed: false }
            }
            _ => return bad("buffer"),
        })
    }
}

impl Persist for Pool {
    fn save(&self, e: &mut Enc) {
        self._fd.save(e);
        (self.size, self.buffers, self.destroyed).save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let fd = OwnedFd::load(d)?;
        let (size, buffers, destroyed): (usize, u32, bool) = Persist::load(d)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return bad("shm pool mapping");
        }
        Ok(Pool { _fd: fd, ptr: ptr.cast(), size, buffers, destroyed })
    }
}

/// Texture contents travel as RGBA pixels.
fn save_texture(t: &Texture, e: &mut Enc, r: &mut Renderer) {
    (t.width, t.height).save(e);
    state::save_bytes(&r.read_texture(t), e);
}

fn load_texture(d: &mut Dec, r: &mut Renderer, mipmaps: bool) -> Result<Texture> {
    let (w, h): (i32, i32) = Persist::load(d)?;
    let rgba = state::load_bytes(d)?;
    if rgba.len() != (w * h * 4) as usize {
        return bad("texture");
    }
    Ok(if mipmaps { r.upload_mipmapped(w, h, &rgba) } else { r.upload_rgba(w, h, &rgba) })
}

fn save_surface(sf: &Surface, e: &mut Enc, r: &mut Renderer) {
    let Surface { pending, cached, upload, content, size, opaque, frames, role, above, below, xdg } = sf;
    pending.save(e);
    cached.save(e);
    upload.save(e);
    match content {
        Content::None => 0u8.save(e),
        Content::Shm(t) => {
            1u8.save(e);
            save_texture(t, e, r);
        }
        Content::Dmabuf(id) => {
            2u8.save(e);
            id.save(e);
        }
    }
    (*size, *opaque).save(e);
    frames.save(e);
    role.save(e);
    (above.clone(), below.clone()).save(e);
    xdg.save(e);
}

fn load_surface(d: &mut Dec, r: &mut Renderer) -> Result<Surface> {
    let pending = Persist::load(d)?;
    let cached = Persist::load(d)?;
    let upload = Persist::load(d)?;
    let content = match u8::load(d)? {
        0 => Content::None,
        1 => Content::Shm(load_texture(d, r, false)?),
        2 => Content::Dmabuf(Persist::load(d)?),
        _ => return bad("surface content"),
    };
    let (size, opaque) = Persist::load(d)?;
    let frames = Persist::load(d)?;
    let role = Persist::load(d)?;
    let (above, below) = Persist::load(d)?;
    let xdg = Persist::load(d)?;
    Ok(Surface { pending, cached, upload, content, size, opaque, frames, role, above, below, xdg })
}

impl State {
    pub fn save(&self, e: &mut Enc, r: &mut Renderer) {
        let State {
            clients,
            next_client,
            globals: _,
            surfaces,
            pools,
            buffers,
            dmabuf_params,
            dmabuf_formats: _,
            dmabuf_device: _,
            positioners,
            output,
            workspaces,
            window_ws,
            active,
            special_shown,
            layout: _,
            special_start: _,
            popups,
            focus,
            urgent,
            pointer,
            keyboard: _,
            selection,
            idle_inhibitors,
            wallpaper,
            urgent_ws,
            bar,
            launcher: _,
            notices: _,
            shot: _,
            hide_cursor: _,
            abandoned: _,
            xwayland,
            xwayland_serials,
            xwm,
            serial,
            start,
            needs_redraw: _,
            pointer_dirty: _,
            dead_textures: _,
        } = self;
        clients.save(e);
        next_client.save(e);
        surfaces.len().save(e);
        for (k, sf) in surfaces {
            k.save(e);
            save_surface(sf, e, r);
        }
        pools.save(e);
        buffers.save(e);
        dmabuf_params.save(e);
        positioners.save(e);
        output.save(e);
        workspaces.save(e);
        window_ws.save(e);
        (*active, *special_shown).save(e);
        popups.save(e);
        focus.save(e);
        urgent.save(e);
        pointer.save(e);
        selection.save(e);
        idle_inhibitors.save(e);
        urgent_ws.save(e);
        bar.visible.save(e);
        wallpaper.is_some().save(e);
        if let Some(t) = wallpaper {
            save_texture(t, e, r);
        }
        (*xwayland, xwayland_serials.clone()).save(e);
        xwm.save(e);
        serial.save(e);
        (start.elapsed().as_millis() as u64).save(e);
    }

    pub fn restore(d: &mut Dec, keyboard: Keyboard, dmabuf_formats: Vec<(u32, u64)>, r: &mut Renderer) -> Result<State> {
        let mut s = State::new(keyboard, Output { name: String::new(), width: 1, height: 1 }, dmabuf_formats);
        s.clients = Persist::load(d)?;
        s.next_client = Persist::load(d)?;
        let n = usize::load(d)?;
        for _ in 0..n {
            let k = SKey::load(d)?;
            s.surfaces.insert(k, load_surface(d, r)?);
        }
        s.pools = Persist::load(d)?;
        s.buffers = Persist::load(d)?;
        s.dmabuf_params = Persist::load(d)?;
        s.positioners = Persist::load(d)?;
        s.output = Persist::load(d)?;
        s.workspaces = Persist::load(d)?;
        s.window_ws = Persist::load(d)?;
        (s.active, s.special_shown) = Persist::load(d)?;
        s.popups = Persist::load(d)?;
        s.focus = Persist::load(d)?;
        s.urgent = Persist::load(d)?;
        s.pointer = Persist::load(d)?;
        s.selection = Persist::load(d)?;
        s.idle_inhibitors = Persist::load(d)?;
        s.urgent_ws = Persist::load(d)?;
        s.bar.visible = Persist::load(d)?;
        if bool::load(d)? {
            s.wallpaper = Some(load_texture(d, r, true)?);
        }
        (s.xwayland, s.xwayland_serials) = Persist::load(d)?;
        s.xwm = Persist::load(d)?;
        s.serial = Persist::load(d)?;
        let elapsed = u64::load(d)?;
        s.start = Instant::now().checked_sub(Duration::from_millis(elapsed)).unwrap_or_else(Instant::now);
        s.relayout();
        // Keys held during the upgrade are forgotten: clear the client's modifiers.
        if let Some(f) = s.focus {
            let serial = s.next_serial();
            for (kb, _) in s.objects_of(f.client, Kind::Keyboard) {
                s.send(f.client, Msg::new(kb, wire::wl_keyboard::evt::MODIFIERS).uint(serial).uint(0).uint(0).uint(0).uint(0));
            }
        }
        s.needs_redraw = true;
        Ok(s)
    }

    /// Fds of all client connections, to register them again with epoll.
    pub fn client_fds(&self) -> Vec<(ClientId, i32)> {
        self.clients.iter().map(|(id, c)| (*id, c.conn.raw_fd())).collect()
    }
}
