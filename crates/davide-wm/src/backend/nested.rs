//! Nested backend: davide-wm runs as a window of another Wayland compositor.
//! Frames are rendered into GBM buffers and handed to the host as dmabufs.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use render::{Renderer, Target};
use wire::*;

use super::Event;

struct HostBuffer {
    target: Target,
    wl_buffer: u32,
    busy: bool,
}

#[derive(Default)]
struct Ids {
    compositor: u32,
    wm_base: u32,
    dmabuf: u32,
    seat: u32,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    pointer: u32,
    keyboard: u32,
}

state::persist_struct!(Ids { compositor, wm_base, dmabuf, seat, surface, xdg_surface, toplevel, pointer, keyboard });

/// What survives a live upgrade: the host connection and our objects on it.
pub struct Saved {
    conn: Connection,
    objects: Vec<(u32, String)>,
    next_id: u32,
    ids: Ids,
    globals: Vec<(u32, String, u32)>,
    frame_cb: Option<u32>,
    size: (i32, i32),
    pending_size: (i32, i32),
}

state::persist_struct!(Saved { conn, objects, next_id, ids, globals, frame_cb, size, pending_size });

pub struct Nested {
    conn: Connection,
    objects: HashMap<u32, &'static Interface>,
    next_id: u32,
    ids: Ids,
    globals: Vec<(u32, String, u32)>,
    sync_done: Option<u32>,
    frame_cb: Option<u32>,
    buffers: Vec<HostBuffer>,
    pub size: (i32, i32),
    pending_size: (i32, i32),
    configured: bool,
    events: Vec<Event>,
}

impl Nested {
    pub fn connect() -> io::Result<Nested> {
        let display = std::env::var("WAYLAND_DISPLAY").map_err(|_| io::Error::other("WAYLAND_DISPLAY not set"))?;
        let path = if display.starts_with('/') {
            PathBuf::from(display)
        } else {
            PathBuf::from(std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR not set"))?).join(display)
        };
        let stream = UnixStream::connect(&path)?;
        stream.set_nonblocking(true)?;
        let mut objects = HashMap::new();
        objects.insert(1, &wl_display::INTERFACE);
        let mut n = Nested {
            conn: Connection::new(OwnedFd::from(stream)),
            objects,
            next_id: 2,
            ids: Ids::default(),
            globals: Vec::new(),
            sync_done: None,
            frame_cb: None,
            buffers: Vec::new(),
            size: config::NESTED_SIZE,
            pending_size: (0, 0),
            configured: false,
            events: Vec::new(),
        };
        let registry = n.new_id(&wl_registry::INTERFACE);
        n.conn.send(Msg::new(1, wl_display::req::GET_REGISTRY).obj(registry));
        n.roundtrip()?;

        n.ids.compositor = n.bind(registry, &wl_compositor::INTERFACE, 4)?;
        n.ids.wm_base = n.bind(registry, &xdg_wm_base::INTERFACE, 1)?;
        n.ids.dmabuf = n.bind(registry, &zwp_linux_dmabuf_v1::INTERFACE, 3)?;
        n.ids.seat = n.bind(registry, &wl_seat::INTERFACE, 5)?;

        n.ids.surface = n.new_id(&wl_surface::INTERFACE);
        n.conn.send(Msg::new(n.ids.compositor, wl_compositor::req::CREATE_SURFACE).obj(n.ids.surface));
        n.ids.xdg_surface = n.new_id(&xdg_surface::INTERFACE);
        n.conn.send(Msg::new(n.ids.wm_base, xdg_wm_base::req::GET_XDG_SURFACE).obj(n.ids.xdg_surface).obj(n.ids.surface));
        n.ids.toplevel = n.new_id(&xdg_toplevel::INTERFACE);
        n.conn.send(Msg::new(n.ids.xdg_surface, xdg_surface::req::GET_TOPLEVEL).obj(n.ids.toplevel));
        n.conn.send(Msg::new(n.ids.toplevel, xdg_toplevel::req::SET_TITLE).str("davide-wm"));
        n.conn.send(Msg::new(n.ids.toplevel, xdg_toplevel::req::SET_APP_ID).str("davide-wm"));
        n.conn.send(Msg::new(n.ids.surface, wl_surface::req::COMMIT));
        while !n.configured {
            n.roundtrip()?;
        }
        n.events.clear();
        Ok(n)
    }

    fn new_id(&mut self, iface: &'static Interface) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(id, iface);
        id
    }

    fn bind(&mut self, registry: u32, iface: &'static Interface, version: u32) -> io::Result<u32> {
        let (name, _, available) = self
            .globals
            .iter()
            .find(|(_, i, _)| i == iface.name)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("host compositor has no {}", iface.name)))?;
        let id = self.new_id(iface);
        let msg = Msg::new(registry, wl_registry::req::BIND).uint(name).str(iface.name).uint(version.min(available)).obj(id);
        self.conn.send(msg);
        Ok(id)
    }

    /// Blocks until the host has processed everything sent so far.
    fn roundtrip(&mut self) -> io::Result<()> {
        let cb = self.new_id(&wl_callback::INTERFACE);
        self.conn.send(Msg::new(1, wl_display::req::SYNC).obj(cb));
        while self.sync_done != Some(cb) {
            self.conn.flush()?;
            let mut pfd = libc::pollfd { fd: self.conn.raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 3000) } <= 0 {
                return Err(io::Error::other("host compositor did not answer"));
            }
            self.dispatch()?;
        }
        Ok(())
    }

    pub fn raw_fd(&self) -> i32 {
        self.conn.raw_fd()
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.conn.flush().map(|_| ())
    }

    /// Reads and handles host events; returns what the compositor must act on.
    pub fn dispatch(&mut self) -> io::Result<Vec<Event>> {
        loop {
            match self.conn.fill() {
                Ok(0) => return Err(io::Error::other("host compositor closed the connection")),
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
            let args = self.conn.decode(&h, desc).map_err(|e| io::Error::other(format!("host {iface:?}: {e}")))?;
            self.event(h.object, iface, h.opcode, args)?;
        }
        Ok(std::mem::take(&mut self.events))
    }

    fn event(&mut self, id: u32, iface: &'static Interface, op: u16, args: Args) -> io::Result<()> {
        match iface.name {
            "wl_display" if op == wl_display::evt::ERROR => {
                return Err(io::Error::other(format!("host error on {}: {}", args.obj(0), args.str(2).unwrap_or(""))));
            }
            "wl_display" if op == wl_display::evt::DELETE_ID => {
                self.objects.remove(&args.uint(0));
            }
            "wl_registry" if op == wl_registry::evt::GLOBAL => {
                self.globals.push((args.uint(0), args.str(1).unwrap_or("").to_string(), args.uint(2)));
            }
            "wl_callback" => {
                if self.frame_cb == Some(id) {
                    self.frame_cb = None;
                } else {
                    self.sync_done = Some(id);
                }
            }
            "xdg_wm_base" if op == xdg_wm_base::evt::PING => {
                self.conn.send(Msg::new(id, xdg_wm_base::req::PONG).uint(args.uint(0)));
            }
            "xdg_toplevel" if op == xdg_toplevel::evt::CONFIGURE => self.pending_size = (args.int(0), args.int(1)),
            "xdg_toplevel" if op == xdg_toplevel::evt::CLOSE => self.events.push(Event::Close),
            "xdg_surface" if op == xdg_surface::evt::CONFIGURE => {
                self.conn.send(Msg::new(id, xdg_surface::req::ACK_CONFIGURE).uint(args.uint(0)));
                self.configured = true;
                let (w, h) = self.pending_size;
                if w > 0 && h > 0 && (w, h) != self.size {
                    self.size = (w, h);
                    self.events.push(Event::Resize(w, h));
                }
            }
            "wl_seat" if op == wl_seat::evt::CAPABILITIES => {
                let caps = args.uint(0);
                if caps & wl_seat::capability::POINTER != 0 && self.ids.pointer == 0 {
                    self.ids.pointer = self.new_id(&wl_pointer::INTERFACE);
                    self.conn.send(Msg::new(id, wl_seat::req::GET_POINTER).obj(self.ids.pointer));
                }
                if caps & wl_seat::capability::KEYBOARD != 0 && self.ids.keyboard == 0 {
                    self.ids.keyboard = self.new_id(&wl_keyboard::INTERFACE);
                    self.conn.send(Msg::new(id, wl_seat::req::GET_KEYBOARD).obj(self.ids.keyboard));
                }
            }
            "wl_pointer" => match op {
                wl_pointer::evt::ENTER => {
                    // Hide the host cursor: davide-wm draws its own.
                    self.conn.send(Msg::new(id, wl_pointer::req::SET_CURSOR).uint(args.uint(0)).obj(0).int(0).int(0));
                    self.events.push(Event::PointerMotion(args.fixed(2), args.fixed(3)));
                }
                wl_pointer::evt::MOTION => self.events.push(Event::PointerMotion(args.fixed(1), args.fixed(2))),
                wl_pointer::evt::BUTTON => {
                    self.events.push(Event::PointerButton(args.uint(2), args.uint(3) == wl_pointer::button_state::PRESSED))
                }
                wl_pointer::evt::AXIS => self.events.push(Event::PointerAxis(args.uint(1), args.fixed(2))),
                _ => {}
            },
            "wl_keyboard" => match op {
                wl_keyboard::evt::KEY => self.events.push(Event::Key(args.uint(2), args.uint(3) == wl_keyboard::key_state::PRESSED)),
                wl_keyboard::evt::LEAVE => self.events.push(Event::KeyboardLeave),
                _ => {}
            },
            "wl_buffer" if op == wl_buffer::evt::RELEASE => {
                if let Some(b) = self.buffers.iter_mut().find(|b| b.wl_buffer == id) {
                    b.busy = false;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A buffer the host is not using, (re)creating buffers after a resize.
    pub fn acquire(&mut self, r: &mut Renderer) -> io::Result<Option<usize>> {
        if self.buffers.first().is_some_and(|b| (b.target.width, b.target.height) != self.size) {
            for b in std::mem::take(&mut self.buffers) {
                self.conn.send(Msg::new(b.wl_buffer, wl_buffer::req::DESTROY));
                self.objects.remove(&b.wl_buffer);
                r.destroy_target(b.target);
            }
        }
        while self.buffers.len() < 2 {
            let target = r.create_target(self.size.0, self.size.1, sys::gbm::BO_USE_LINEAR).map_err(io::Error::other)?;
            let fd = target.export_fd().map_err(io::Error::other)?;
            let params = self.new_id(&zwp_linux_buffer_params_v1::INTERFACE);
            self.conn.send(Msg::new(self.ids.dmabuf, zwp_linux_dmabuf_v1::req::CREATE_PARAMS).obj(params));
            self.conn.send(
                Msg::new(params, zwp_linux_buffer_params_v1::req::ADD)
                    .fd(fd)
                    .uint(0)
                    .uint(target.offset)
                    .uint(target.stride)
                    .uint((target.modifier >> 32) as u32)
                    .uint(target.modifier as u32),
            );
            let wl_buffer = self.new_id(&wl_buffer::INTERFACE);
            self.conn.send(
                Msg::new(params, zwp_linux_buffer_params_v1::req::CREATE_IMMED)
                    .obj(wl_buffer)
                    .int(target.width)
                    .int(target.height)
                    .uint(target.format)
                    .uint(0),
            );
            self.conn.send(Msg::new(params, zwp_linux_buffer_params_v1::req::DESTROY));
            self.buffers.push(HostBuffer { target, wl_buffer, busy: false });
        }
        if self.frame_cb.is_some() {
            return Ok(None);
        }
        Ok(self.buffers.iter().position(|b| !b.busy))
    }

    /// Drops our buffers on the host (it keeps showing the last frame) and
    /// hands the connection over.
    pub fn save(mut self) -> Saved {
        for b in std::mem::take(&mut self.buffers) {
            self.conn.send(Msg::new(b.wl_buffer, wl_buffer::req::DESTROY));
            self.objects.remove(&b.wl_buffer);
        }
        let _ = self.conn.flush();
        Saved {
            objects: self.objects.iter().map(|(id, i)| (*id, i.name.to_string())).collect(),
            conn: self.conn,
            next_id: self.next_id,
            ids: self.ids,
            globals: self.globals,
            frame_cb: self.frame_cb,
            size: self.size,
            pending_size: self.pending_size,
        }
    }

    pub fn restore(s: Saved) -> io::Result<Nested> {
        let objects = s
            .objects
            .into_iter()
            .map(|(id, name)| interface_by_name(&name).map(|i| (id, i)).ok_or_else(|| io::Error::other(format!("unknown interface {name}"))))
            .collect::<io::Result<_>>()?;
        Ok(Nested {
            conn: s.conn,
            objects,
            next_id: s.next_id,
            ids: s.ids,
            globals: s.globals,
            sync_done: None,
            frame_cb: s.frame_cb,
            buffers: Vec::new(),
            size: s.size,
            pending_size: s.pending_size,
            configured: true,
            events: Vec::new(),
        })
    }

    pub fn target(&self, idx: usize) -> &Target {
        &self.buffers[idx].target
    }

    pub fn present(&mut self, idx: usize) {
        let b = &mut self.buffers[idx];
        b.busy = true;
        let wl_buffer = b.wl_buffer;
        let s = self.ids.surface;
        self.conn.send(Msg::new(s, wl_surface::req::ATTACH).obj(wl_buffer).int(0).int(0));
        self.conn.send(Msg::new(s, wl_surface::req::DAMAGE_BUFFER).int(0).int(0).int(i32::MAX).int(i32::MAX));
        let cb = self.new_id(&wl_callback::INTERFACE);
        self.frame_cb = Some(cb);
        self.conn.send(Msg::new(s, wl_surface::req::FRAME).obj(cb));
        self.conn.send(Msg::new(s, wl_surface::req::COMMIT));
    }
}
