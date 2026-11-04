//! xdg-shell (toplevels, popups) and xdg-decoration.

use wire::*;
use wm::Rect;

use super::*;

pub fn wm_base(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    let id = args.obj(0);
    match op {
        xdg_wm_base::req::CREATE_POSITIONER => {
            if s.new_object(c, id, &xdg_positioner::INTERFACE, version, Kind::XdgPositioner) {
                s.positioners.insert(SKey { client: c, id }, Positioner::default());
            }
        }
        xdg_wm_base::req::GET_XDG_SURFACE => {
            let surface = args.obj(1);
            let key = SKey { client: c, id: surface };
            let ok = s.surfaces.get(&key).is_some_and(|sf| matches!(sf.role, Role::None | Role::Xdg) && sf.xdg.is_none());
            if !ok {
                s.error(c, id, xdg_wm_base::error::ROLE, "surface already has a role");
                return;
            }
            if s.new_object(c, id, &xdg_surface::INTERFACE, version, Kind::XdgSurface { surface }) {
                let sf = s.surfaces.get_mut(&key).unwrap();
                sf.role = Role::Xdg;
                sf.xdg = Some(Xdg { obj: id, role: XdgRole::None, geometry: None, configured: false, mapped: false });
            }
        }
        _ => {}
    }
}

pub fn positioner(s: &mut State, c: ClientId, id: u32, op: u16, args: Args) {
    let key = SKey { client: c, id };
    if op == xdg_positioner::req::DESTROY {
        s.positioners.remove(&key);
        return;
    }
    let Some(p) = s.positioners.get_mut(&key) else { return };
    match op {
        xdg_positioner::req::SET_SIZE => p.size = (args.int(0), args.int(1)),
        xdg_positioner::req::SET_ANCHOR_RECT => p.anchor_rect = Rect::new(args.int(0), args.int(1), args.int(2), args.int(3)),
        xdg_positioner::req::SET_ANCHOR => p.anchor = args.uint(0),
        xdg_positioner::req::SET_GRAVITY => p.gravity = args.uint(0),
        xdg_positioner::req::SET_OFFSET => p.offset = (args.int(0), args.int(1)),
        _ => {}
    }
}

/// Popup geometry relative to the parent's window geometry. Constraint
/// adjustment is not implemented yet.
fn place(p: &Positioner) -> Rect {
    use xdg_positioner::{anchor as a, gravity as g};
    let r = p.anchor_rect;
    let ax = match p.anchor {
        a::LEFT | a::TOP_LEFT | a::BOTTOM_LEFT => r.x,
        a::RIGHT | a::TOP_RIGHT | a::BOTTOM_RIGHT => r.x + r.w,
        _ => r.x + r.w / 2,
    };
    let ay = match p.anchor {
        a::TOP | a::TOP_LEFT | a::TOP_RIGHT => r.y,
        a::BOTTOM | a::BOTTOM_LEFT | a::BOTTOM_RIGHT => r.y + r.h,
        _ => r.y + r.h / 2,
    };
    let (w, h) = p.size;
    let x = match p.gravity {
        g::LEFT | g::TOP_LEFT | g::BOTTOM_LEFT => ax - w,
        g::RIGHT | g::TOP_RIGHT | g::BOTTOM_RIGHT => ax,
        _ => ax - w / 2,
    };
    let y = match p.gravity {
        g::TOP | g::TOP_LEFT | g::TOP_RIGHT => ay - h,
        g::BOTTOM | g::BOTTOM_LEFT | g::BOTTOM_RIGHT => ay,
        _ => ay - h / 2,
    };
    Rect::new(x + p.offset.0, y + p.offset.1, w, h)
}

pub fn xdg_surface(s: &mut State, c: ClientId, surface: u32, version: u32, op: u16, args: Args) {
    let key = SKey { client: c, id: surface };
    match op {
        xdg_surface::req::DESTROY => {
            unmap(s, key);
            if let Some(sf) = s.surfaces.get_mut(&key) {
                sf.xdg = None;
            }
        }
        xdg_surface::req::GET_TOPLEVEL => {
            let id = args.obj(0);
            if !s.new_object(c, id, &xdg_toplevel::INTERFACE, version, Kind::XdgToplevel { surface }) {
                return;
            }
            if let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) {
                x.role = XdgRole::Toplevel { obj: id, title: String::new(), app_id: String::new(), decoration: None, sent: None };
            }
            if version >= 5 {
                s.send(c, Msg::new(id, xdg_toplevel::evt::WM_CAPABILITIES).array(&[]));
            }
        }
        xdg_surface::req::GET_POPUP => {
            let (id, parent, pos) = (args.obj(0), args.obj(1), args.obj(2));
            let parent_surface = s.clients.get(&c).and_then(|cl| cl.objects.get(&parent)).and_then(|o| match o.kind {
                Kind::XdgSurface { surface } => Some(surface),
                _ => None,
            });
            let Some(parent_surface) = parent_surface else {
                s.error(c, id, xdg_wm_base::error::INVALID_POPUP_PARENT, "popups need an xdg_surface parent");
                return;
            };
            let geo = place(&s.positioners.get(&SKey { client: c, id: pos }).copied().unwrap_or_default());
            if !s.new_object(c, id, &xdg_popup::INTERFACE, version, Kind::XdgPopup { surface }) {
                return;
            }
            if let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) {
                x.role = XdgRole::Popup { obj: id, parent: parent_surface, geo };
            }
        }
        xdg_surface::req::SET_WINDOW_GEOMETRY => {
            let g = Rect::new(args.int(0), args.int(1), args.int(2), args.int(3));
            if let Some(sf) = s.surfaces.get_mut(&key) {
                sf.pending.geometry = Some(g);
            }
        }
        _ => {}
    }
}

pub fn toplevel(s: &mut State, c: ClientId, surface: u32, op: u16, args: Args) {
    let key = SKey { client: c, id: surface };
    match op {
        xdg_toplevel::req::DESTROY => {
            unmap(s, key);
            if let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) {
                x.role = XdgRole::None;
            }
        }
        xdg_toplevel::req::SET_TITLE | xdg_toplevel::req::SET_APP_ID => {
            let v = args.str(0).unwrap_or_default().to_string();
            if let Some(XdgRole::Toplevel { title, app_id, .. }) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()).map(|x| &mut x.role) {
                if op == xdg_toplevel::req::SET_TITLE {
                    *title = v;
                } else {
                    *app_id = v;
                }
            }
        }
        _ => {}
    }
}

pub fn popup(s: &mut State, c: ClientId, surface: u32, op: u16) {
    if op == xdg_popup::req::DESTROY {
        let key = SKey { client: c, id: surface };
        unmap(s, key);
        if let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) {
            x.role = XdgRole::None;
        }
    }
}

pub fn decoration_manager(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    if op != zxdg_decoration_manager_v1::req::GET_TOPLEVEL_DECORATION {
        return;
    }
    let (id, toplevel) = (args.obj(0), args.obj(1));
    let surface = s.clients.get(&c).and_then(|cl| cl.objects.get(&toplevel)).and_then(|o| match o.kind {
        Kind::XdgToplevel { surface } => Some(surface),
        _ => None,
    });
    let Some(surface) = surface else { return };
    if !s.new_object(c, id, &zxdg_toplevel_decoration_v1::INTERFACE, version, Kind::Decoration { surface }) {
        return;
    }
    let key = SKey { client: c, id: surface };
    if let Some(XdgRole::Toplevel { decoration, .. }) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()).map(|x| &mut x.role) {
        *decoration = Some(id);
    }
    send_decoration(s, c, id, key);
}

/// Always server-side: davide-wm draws no decorations at all.
fn send_decoration(s: &mut State, c: ClientId, id: u32, key: SKey) {
    s.send(c, Msg::new(id, zxdg_toplevel_decoration_v1::evt::CONFIGURE).uint(zxdg_toplevel_decoration_v1::mode::SERVER_SIDE));
    let configured = s.surfaces.get(&key).and_then(|sf| sf.xdg.as_ref()).is_some_and(|x| x.configured);
    if configured {
        let xdg_obj = s.surfaces[&key].xdg.as_ref().unwrap().obj;
        let serial = s.next_serial();
        s.send(c, Msg::new(xdg_obj, xdg_surface::evt::CONFIGURE).uint(serial));
    }
}

pub fn decoration(s: &mut State, c: ClientId, id: u32, surface: u32, op: u16) {
    let key = SKey { client: c, id: surface };
    match op {
        zxdg_toplevel_decoration_v1::req::SET_MODE | zxdg_toplevel_decoration_v1::req::UNSET_MODE => send_decoration(s, c, id, key),
        zxdg_toplevel_decoration_v1::req::DESTROY => {
            if let Some(XdgRole::Toplevel { decoration, .. }) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()).map(|x| &mut x.role) {
                *decoration = None;
            }
        }
        _ => {}
    }
}

/// Sends a configure when the size or the activated state changed.
pub fn configure_toplevel(s: &mut State, key: SKey, w: i32, h: i32) {
    let focused = s.focus == Some(key);
    let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) else { return };
    let XdgRole::Toplevel { obj, sent, .. } = &mut x.role else { return };
    if *sent == Some((w, h, focused)) {
        return;
    }
    *sent = Some((w, h, focused));
    let (obj, xdg_obj) = (*obj, x.obj);
    x.configured = true;
    let version = s.version(key.client, obj);
    let mut states = Vec::new();
    if focused {
        states.push(xdg_toplevel::state::ACTIVATED);
    }
    if version >= 2 {
        states.extend([
            xdg_toplevel::state::TILED_LEFT,
            xdg_toplevel::state::TILED_RIGHT,
            xdg_toplevel::state::TILED_TOP,
            xdg_toplevel::state::TILED_BOTTOM,
        ]);
    }
    let states: Vec<u8> = states.iter().flat_map(|v| v.to_ne_bytes()).collect();
    s.send(key.client, Msg::new(obj, xdg_toplevel::evt::CONFIGURE).int(w).int(h).array(&states));
    let serial = s.next_serial();
    s.send(key.client, Msg::new(xdg_obj, xdg_surface::evt::CONFIGURE).uint(serial));
}

fn send_surface_enter(s: &mut State, key: SKey) {
    for (o, _) in s.objects_of(key.client, Kind::Output) {
        s.send(key.client, Msg::new(key.id, wl_surface::evt::ENTER).obj(o));
    }
}

/// Role logic after a `wl_surface.commit`.
pub fn commit(s: &mut State, key: SKey) {
    let Some(sf) = s.surfaces.get_mut(&key) else { return };
    let has_buffer = sf.size != (0, 0);
    let Some(x) = sf.xdg.as_mut() else { return };
    match &x.role {
        XdgRole::None => {}
        XdgRole::Toplevel { .. } => {
            if !x.configured {
                // Initial commit: tile it now so the first configure has the final size.
                x.configured = true;
                let focus = s.focus;
                s.tiling.insert(key, focus);
                s.relayout();
            } else if has_buffer && !x.mapped {
                x.mapped = true;
                send_surface_enter(s, key);
                seat::set_focus(s, Some(key));
            } else if !has_buffer && x.mapped {
                unmap(s, key);
            }
        }
        XdgRole::Popup { obj, geo, .. } => {
            let (obj, geo, xdg_obj) = (*obj, *geo, x.obj);
            if !x.configured {
                x.configured = true;
                s.send(key.client, Msg::new(obj, xdg_popup::evt::CONFIGURE).int(geo.x).int(geo.y).int(geo.w).int(geo.h));
                let serial = s.next_serial();
                s.send(key.client, Msg::new(xdg_obj, xdg_surface::evt::CONFIGURE).uint(serial));
            } else if has_buffer && !x.mapped {
                x.mapped = true;
                s.popups.push(key);
                send_surface_enter(s, key);
            } else if !has_buffer && x.mapped {
                unmap(s, key);
            }
        }
    }
}

/// Takes an xdg surface off screen; the client must configure it again.
pub fn unmap(s: &mut State, key: SKey) {
    let Some(x) = s.surfaces.get_mut(&key).and_then(|sf| sf.xdg.as_mut()) else { return };
    x.mapped = false;
    x.configured = false;
    if let XdgRole::Toplevel { sent, .. } = &mut x.role {
        *sent = None;
    }
    // Popups of this surface go away with it.
    let children: Vec<SKey> = s
        .popups
        .iter()
        .copied()
        .filter(|p| matches!(s.surfaces.get(p).and_then(|sf| sf.xdg.as_ref()).map(|x| &x.role), Some(XdgRole::Popup { parent, .. }) if *parent == key.id && p.client == key.client))
        .collect();
    for p in children {
        if let Some(XdgRole::Popup { obj, .. }) = s.surfaces.get(&p).and_then(|sf| sf.xdg.as_ref()).map(|x| &x.role) {
            let obj = *obj;
            s.send(p.client, Msg::new(obj, xdg_popup::evt::POPUP_DONE));
        }
        unmap(s, p);
    }
    s.popups.retain(|p| *p != key);
    if s.tiling.contains(key) {
        s.tiling.remove(key);
        if s.focus == Some(key) {
            let next = s.tiling.layout(s.work_area()).last().map(|(k, _)| *k);
            seat::set_focus(s, next);
        }
        s.relayout();
    }
    s.needs_redraw = true;
}
