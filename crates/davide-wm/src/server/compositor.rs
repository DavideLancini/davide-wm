//! Core protocol: display, registry, compositor, surfaces, subsurfaces, shm.

use std::os::fd::AsRawFd;

use wire::*;

use super::*;

pub fn display(s: &mut State, c: ClientId, op: u16, args: Args) {
    match op {
        wl_display::req::SYNC => {
            let id = args.obj(0);
            if s.new_object(c, id, &wl_callback::INTERFACE, 1, Kind::Callback) {
                let serial = s.next_serial();
                s.send_callback_done(c, id, serial);
            }
        }
        wl_display::req::GET_REGISTRY => {
            let id = args.obj(0);
            if !s.new_object(c, id, &wl_registry::INTERFACE, 1, Kind::Registry) {
                return;
            }
            let globals: Vec<_> = s.globals.iter().map(|g| (g.name, g.iface.name, g.version)).collect();
            for (name, iface, version) in globals {
                s.send(c, Msg::new(id, wl_registry::evt::GLOBAL).uint(name).str(iface).uint(version));
            }
        }
        _ => {}
    }
}

pub fn registry(s: &mut State, c: ClientId, op: u16, args: Args) {
    if op != wl_registry::req::BIND {
        return;
    }
    let (name, iface, version, id) = (args.uint(0), args.str(1).unwrap_or(""), args.uint(2), args.uint(3));
    let Some(g) = s.globals.iter().find(|g| g.name == name) else {
        s.error(c, 1, wl_display::error::INVALID_OBJECT, &format!("no global {name}"));
        return;
    };
    let gi = g.iface;
    if gi.name != iface || version == 0 || version > g.version {
        s.error(c, 1, wl_display::error::INVALID_OBJECT, &format!("bad bind of {iface} v{version}"));
        return;
    }
    let kind = match gi.name {
        "wl_compositor" => Kind::Compositor,
        "wl_subcompositor" => Kind::Subcompositor,
        "wl_shm" => Kind::Shm,
        "wl_seat" => Kind::Seat,
        "wl_output" => Kind::Output,
        "xdg_wm_base" => Kind::XdgWmBase,
        "zxdg_decoration_manager_v1" => Kind::DecorationManager,
        "wl_data_device_manager" => Kind::DataDeviceManager,
        _ => unreachable!("global without kind"),
    };
    if !s.new_object(c, id, gi, version, kind) {
        return;
    }
    match kind {
        Kind::Shm => {
            for f in [wl_shm::format::ARGB8888, wl_shm::format::XRGB8888] {
                s.send(c, Msg::new(id, wl_shm::evt::FORMAT).uint(f));
            }
        }
        Kind::Seat => seat::send_seat_info(s, c, id, version),
        Kind::Output => seat::send_output_info(s, c, id, version),
        _ => {}
    }
}

pub fn compositor(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    let id = args.obj(0);
    match op {
        wl_compositor::req::CREATE_SURFACE => {
            if s.new_object(c, id, &wl_surface::INTERFACE, version, Kind::Surface) {
                s.surfaces.insert(
                    SKey { client: c, id },
                    Surface {
                        pending: Pending::default(),
                        buffer: None,
                        upload: false,
                        size: (0, 0),
                        opaque: false,
                        tex: None,
                        frames: Vec::new(),
                        role: Role::None,
                        above: Vec::new(),
                        below: Vec::new(),
                        xdg: None,
                    },
                );
            }
        }
        wl_compositor::req::CREATE_REGION => {
            s.new_object(c, id, &wl_region::INTERFACE, version, Kind::Region);
        }
        _ => {}
    }
}

pub fn surface(s: &mut State, c: ClientId, id: u32, op: u16, args: Args) {
    let key = SKey { client: c, id };
    match op {
        wl_surface::req::DESTROY => s.destroy_surface(key),
        wl_surface::req::ATTACH => {
            let buffer = args.obj(0);
            if let Some(sf) = s.surfaces.get_mut(&key) {
                sf.pending.buffer = Some((buffer != 0).then_some(buffer));
            }
        }
        wl_surface::req::FRAME => {
            let cb = args.obj(0);
            if s.new_object(c, cb, &wl_callback::INTERFACE, 1, Kind::Callback) {
                if let Some(sf) = s.surfaces.get_mut(&key) {
                    sf.pending.frames.push(cb);
                }
            }
        }
        wl_surface::req::COMMIT => commit(s, key),
        wl_surface::req::SET_BUFFER_SCALE if args.int(0) != 1 => {
            log!("client {c}: buffer scale {} not supported yet", args.int(0));
        }
        _ => {}
    }
}

fn commit(s: &mut State, key: SKey) {
    let Some(sf) = s.surfaces.get_mut(&key) else { return };
    if let Some(b) = sf.pending.buffer.take() {
        match b.and_then(|id| s.buffers.get(&SKey { client: key.client, id }).map(|buf| (id, buf))) {
            Some((id, buf)) => {
                sf.buffer = Some(id);
                sf.upload = true;
                sf.size = (buf.width, buf.height);
                sf.opaque = buf.opaque;
            }
            None => {
                sf.buffer = None;
                sf.upload = false;
                sf.size = (0, 0);
                if let Some(t) = sf.tex.take() {
                    s.dead_textures.push(t);
                }
            }
        }
    }
    sf.frames.append(&mut sf.pending.frames);
    if let Some(x) = sf.xdg.as_mut() {
        if let Some(g) = sf.pending.geometry.take() {
            x.geometry = Some(g);
        }
    }
    s.needs_redraw = true;
    xdg::commit(s, key);
}

pub fn subcompositor(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    if op != wl_subcompositor::req::GET_SUBSURFACE {
        return;
    }
    let (id, surface, parent) = (args.obj(0), args.obj(1), args.obj(2));
    let key = SKey { client: c, id: surface };
    let ok = s.surfaces.get(&key).is_some_and(|sf| matches!(sf.role, Role::None))
        && s.surfaces.contains_key(&SKey { client: c, id: parent })
        && surface != parent;
    if !ok {
        s.error(c, id, wl_subcompositor::error::BAD_SURFACE, "surface cannot become a subsurface");
        return;
    }
    if s.new_object(c, id, &wl_subsurface::INTERFACE, version, Kind::Subsurface { surface }) {
        s.surfaces.get_mut(&key).unwrap().role = Role::Subsurface { parent, x: 0, y: 0 };
        s.surfaces.get_mut(&SKey { client: c, id: parent }).unwrap().above.push(surface);
    }
}

pub fn subsurface(s: &mut State, c: ClientId, surface: u32, op: u16, args: Args) {
    let key = SKey { client: c, id: surface };
    let Some(Role::Subsurface { parent, .. }) = s.surfaces.get(&key).map(|sf| &sf.role) else { return };
    let parent = *parent;
    match op {
        wl_subsurface::req::DESTROY => {
            if let Some(p) = s.surfaces.get_mut(&SKey { client: c, id: parent }) {
                p.above.retain(|x| *x != surface);
                p.below.retain(|x| *x != surface);
            }
            if let Some(sf) = s.surfaces.get_mut(&key) {
                sf.role = Role::None;
            }
        }
        wl_subsurface::req::SET_POSITION => {
            if let Some(Role::Subsurface { x, y, .. }) = s.surfaces.get_mut(&key).map(|sf| &mut sf.role) {
                *x = args.int(0);
                *y = args.int(1);
            }
        }
        wl_subsurface::req::PLACE_ABOVE | wl_subsurface::req::PLACE_BELOW => {
            let sibling = args.obj(0);
            let Some(p) = s.surfaces.get_mut(&SKey { client: c, id: parent }) else { return };
            p.above.retain(|x| *x != surface);
            p.below.retain(|x| *x != surface);
            let above = op == wl_subsurface::req::PLACE_ABOVE;
            if sibling == parent {
                if above {
                    p.above.insert(0, surface);
                } else {
                    p.below.push(surface);
                }
            } else if let Some(i) = p.above.iter().position(|x| *x == sibling) {
                p.above.insert(if above { i + 1 } else { i }, surface);
            } else if let Some(i) = p.below.iter().position(|x| *x == sibling) {
                p.below.insert(if above { i + 1 } else { i }, surface);
            } else {
                p.above.push(surface);
            }
        }
        _ => {}
    }
    s.needs_redraw = true;
}

pub fn shm(s: &mut State, c: ClientId, version: u32, op: u16, mut args: Args) {
    if op != wl_shm::req::CREATE_POOL {
        return;
    }
    let (id, fd, size) = (args.obj(0), args.take_fd(1), args.int(2));
    if size <= 0 {
        s.error(c, id, wl_shm::error::INVALID_STRIDE, "invalid pool size");
        return;
    }
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        s.error(c, id, wl_shm::error::INVALID_FD, "mmap failed");
        return;
    }
    if s.new_object(c, id, &wl_shm_pool::INTERFACE, version, Kind::ShmPool) {
        s.pools.insert(SKey { client: c, id }, Pool { _fd: fd, ptr: ptr.cast(), size: size as usize, buffers: 0, destroyed: false });
    } else {
        unsafe { libc::munmap(ptr, size as usize) };
    }
}

pub fn unmap_pool(p: &Pool) {
    unsafe { libc::munmap(p.ptr.cast(), p.size) };
}

pub fn shm_pool(s: &mut State, c: ClientId, id: u32, op: u16, args: Args) {
    let key = SKey { client: c, id };
    match op {
        wl_shm_pool::req::CREATE_BUFFER => {
            let (bid, offset, width, height, stride, format) =
                (args.obj(0), args.int(1), args.int(2), args.int(3), args.int(4), args.uint(5));
            let Some(p) = s.pools.get(&key) else { return };
            if format != wl_shm::format::ARGB8888 && format != wl_shm::format::XRGB8888 {
                s.error(c, id, wl_shm::error::INVALID_FORMAT, "unsupported format");
                return;
            }
            let fits = width > 0
                && height > 0
                && offset >= 0
                && stride >= width * 4
                && (offset as i64 + stride as i64 * height as i64) <= p.size as i64;
            if !fits {
                s.error(c, id, wl_shm::error::INVALID_STRIDE, "buffer does not fit the pool");
                return;
            }
            if s.new_object(c, bid, &wl_buffer::INTERFACE, 1, Kind::Buffer) {
                s.pools.get_mut(&key).unwrap().buffers += 1;
                let opaque = format == wl_shm::format::XRGB8888;
                s.buffers.insert(SKey { client: c, id: bid }, ShmBuffer { pool: id, offset, width, height, stride, opaque });
            }
        }
        wl_shm_pool::req::DESTROY => {
            if let Some(p) = s.pools.get_mut(&key) {
                p.destroyed = true;
                if p.buffers == 0 {
                    let p = s.pools.remove(&key).unwrap();
                    unmap_pool(&p);
                }
            }
        }
        wl_shm_pool::req::RESIZE => {
            let size = args.int(0);
            let Some(p) = s.pools.get_mut(&key) else { return };
            if size < p.size as i32 {
                s.error(c, id, wl_shm::error::INVALID_STRIDE, "pools can only grow");
                return;
            }
            let ptr = unsafe { libc::mremap(p.ptr.cast(), p.size, size as usize, libc::MREMAP_MAYMOVE) };
            if ptr == libc::MAP_FAILED {
                s.error(c, id, wl_shm::error::INVALID_FD, "mremap failed");
                return;
            }
            p.ptr = ptr.cast();
            p.size = size as usize;
        }
        _ => {}
    }
}

pub fn buffer(s: &mut State, c: ClientId, id: u32, op: u16) {
    if op != wl_buffer::req::DESTROY {
        return;
    }
    let Some(b) = s.buffers.remove(&SKey { client: c, id }) else { return };
    let pkey = SKey { client: c, id: b.pool };
    if let Some(p) = s.pools.get_mut(&pkey) {
        p.buffers -= 1;
        if p.destroyed && p.buffers == 0 {
            let p = s.pools.remove(&pkey).unwrap();
            unmap_pool(&p);
        }
    }
    for sf in s.surfaces.iter_mut().filter(|(k, _)| k.client == c).map(|(_, sf)| sf) {
        if sf.buffer == Some(id) {
            sf.buffer = None;
            sf.upload = false;
        }
    }
}

pub fn data_device_manager(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    let id = args.obj(0);
    match op {
        wl_data_device_manager::req::CREATE_DATA_SOURCE => {
            s.new_object(c, id, &wl_data_source::INTERFACE, version, Kind::DataSource);
        }
        wl_data_device_manager::req::GET_DATA_DEVICE => {
            s.new_object(c, id, &wl_data_device::INTERFACE, version, Kind::DataDevice);
        }
        _ => {}
    }
}
