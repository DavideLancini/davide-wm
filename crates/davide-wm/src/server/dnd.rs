//! Drag and drop between Wayland clients (`wl_data_device.start_drag`).
//!
//! While a drag runs the pointer belongs to it: the surface under the
//! pointer gets enter/motion/leave on its data devices with an offer of the
//! source's types, and the button release drops or cancels.

use std::os::fd::OwnedFd;

use wire::*;

use super::*;

const COPY: u32 = 1;
const MOVE: u32 = 2;
const ASK: u32 = 4;

pub struct Drag {
    /// The client that started it, and its source (None: a drag inside
    /// that client only).
    client: ClientId,
    source: Option<u32>,
    pub icon: Option<SKey>,
    /// Surface under the pointer, its offer and the device it went to.
    target: Option<(SKey, u32)>,
    accepted: Option<String>,
    /// Actions the target wants and prefers.
    target_actions: (u32, u32),
    action: u32,
}

/// After a drop: the offer still reads from the source until `finish`.
pub struct Dropped {
    offer: SKey,
    source: SKey,
}

fn source_version(s: &State, d: &Drag) -> u32 {
    d.source.map_or(0, |id| s.version(d.client, id))
}

pub fn start(s: &mut State, c: ClientId, source: u32, origin: u32, icon: u32) {
    if s.drag.is_some() || s.pointer.focus.map(|f| f.client) != Some(c) {
        // Only the client under the pointer may start one.
        if source != 0 {
            s.send(c, Msg::new(source, wl_data_source::evt::CANCELLED));
        }
        return;
    }
    let icon = (icon != 0).then_some(SKey { client: c, id: icon });
    if let Some(sf) = icon.and_then(|k| s.surfaces.get_mut(&k)) {
        sf.role = Role::DndIcon;
    }
    // The pointer now belongs to the drag.
    if let Some(old) = s.pointer.focus.take() {
        let serial = s.next_serial();
        for (p, _) in s.objects_of(old.client, Kind::Pointer) {
            s.send(old.client, Msg::new(p, wl_pointer::evt::LEAVE).uint(serial).obj(old.id));
        }
    }
    let _ = origin;
    s.drag = Some(Drag { client: c, source: (source != 0).then_some(source), icon, target: None, accepted: None, target_actions: (0, 0), action: 0 });
    motion(s);
}

fn leave(s: &mut State) {
    let Some(d) = s.drag.as_mut() else { return };
    let Some((key, _)) = d.target.take() else { return };
    d.accepted = None;
    d.action = 0;
    for (dev, _) in s.objects_of(key.client, Kind::DataDevice) {
        s.send(key.client, Msg::new(dev, wl_data_device::evt::LEAVE));
    }
    if let Some(src) = s.drag.as_ref().and_then(|d| d.source.map(|id| SKey { client: d.client, id })) {
        s.send(src.client, Msg::new(src.id, wl_data_source::evt::TARGET).opt_str(None));
    }
}

/// The pointer moved during a drag.
pub fn motion(s: &mut State) {
    let Some(d) = s.drag.as_ref() else { return };
    let (px, py) = (s.pointer.x, s.pointer.y);
    let hit = seat::surface_at(s, px, py).filter(|(k, ..)| d.source.is_some() || k.client == d.client);
    let current = d.target.map(|(k, _)| k);
    s.needs_redraw = true;
    match hit {
        Some((key, lx, ly)) if current == Some(key) => {
            let time = s.time_ms();
            for (dev, _) in s.objects_of(key.client, Kind::DataDevice) {
                s.send(key.client, Msg::new(dev, wl_data_device::evt::MOTION).uint(time).fixed(lx).fixed(ly));
            }
        }
        hit => {
            leave(s);
            let Some((key, lx, ly)) = hit else { return };
            enter(s, key, lx, ly);
        }
    }
}

fn enter(s: &mut State, key: SKey, lx: f64, ly: f64) {
    let d = s.drag.as_ref().unwrap();
    let source = d.source.map(|id| SKey { client: d.client, id });
    let mimes = source.and_then(|k| s.selection.mimes.get(&k).cloned()).unwrap_or_default();
    let source_actions = source.and_then(|k| s.selection.dnd_actions.get(&k).copied()).unwrap_or(COPY);
    let serial = s.next_serial();
    let mut offer = 0;
    for (dev, version) in s.objects_of(key.client, Kind::DataDevice) {
        let mut this = 0;
        if source.is_some() {
            let Some(id) = s.new_server_object(key.client, &wl_data_offer::INTERFACE, version, Kind::DragOffer) else { return };
            s.send(key.client, Msg::new(dev, wl_data_device::evt::DATA_OFFER).obj(id));
            for m in &mimes {
                s.send(key.client, Msg::new(id, wl_data_offer::evt::OFFER).str(m));
            }
            if version >= 3 {
                s.send(key.client, Msg::new(id, wl_data_offer::evt::SOURCE_ACTIONS).uint(source_actions));
            }
            this = id;
            offer = id;
        }
        s.send(key.client, Msg::new(dev, wl_data_device::evt::ENTER).uint(serial).obj(key.id).fixed(lx).fixed(ly).obj(this));
    }
    let d = s.drag.as_mut().unwrap();
    d.target = Some((key, offer));
    d.target_actions = (0, 0);
}

/// Picks the action from what both sides allow.
fn negotiate(s: &mut State) {
    let Some(d) = s.drag.as_ref() else { return };
    let Some((target, offer)) = d.target else { return };
    let source = d.source.map(|id| SKey { client: d.client, id });
    let allowed = source.and_then(|k| s.selection.dnd_actions.get(&k).copied()).unwrap_or(COPY);
    let (wanted, preferred) = d.target_actions;
    let both = allowed & wanted;
    let action = if both & preferred != 0 { preferred } else { [COPY, MOVE, ASK].into_iter().find(|a| both & a != 0).unwrap_or(0) };
    if action == d.action {
        return;
    }
    s.drag.as_mut().unwrap().action = action;
    if offer != 0 && s.version(target.client, offer) >= 3 {
        s.send(target.client, Msg::new(offer, wl_data_offer::evt::ACTION).uint(action));
    }
    if let Some(src) = source.filter(|k| s.version(k.client, k.id) >= 3) {
        s.send(src.client, Msg::new(src.id, wl_data_source::evt::ACTION).uint(action));
    }
}

/// Requests on an offer made during a drag.
pub fn offer_request(s: &mut State, c: ClientId, id: u32, op: u16, mut args: Args) {
    let key = SKey { client: c, id };
    let current = s.drag.as_ref().is_some_and(|d| d.target.is_some_and(|(t, o)| t.client == c && o == id));
    match op {
        wl_data_offer::req::ACCEPT if current => {
            let mime = args.str(1).map(str::to_string);
            let d = s.drag.as_mut().unwrap();
            d.accepted = mime.clone();
            if let Some(src) = d.source.map(|sid| SKey { client: d.client, id: sid }) {
                s.send(src.client, Msg::new(src.id, wl_data_source::evt::TARGET).opt_str(mime.as_deref()));
            }
        }
        wl_data_offer::req::SET_ACTIONS if current => {
            s.drag.as_mut().unwrap().target_actions = (args.uint(0), args.uint(1));
            negotiate(s);
        }
        wl_data_offer::req::RECEIVE => {
            let mime = args.str(0).unwrap_or_default().to_string();
            let fd: OwnedFd = args.take_fd(1);
            let source = match (&s.dropped, &s.drag) {
                (Some(d), _) if d.offer == key => Some(d.source),
                (_, Some(d)) if current => d.source.map(|sid| SKey { client: d.client, id: sid }),
                _ => None,
            };
            if let Some(src) = source {
                s.send(src.client, Msg::new(src.id, wl_data_source::evt::SEND).str(&mime).fd(fd));
            }
        }
        wl_data_offer::req::FINISH | wl_data_offer::req::DESTROY => {
            if let Some(d) = s.dropped.take_if(|d| d.offer == key) {
                if op == wl_data_offer::req::FINISH && s.version(d.source.client, d.source.id) >= 3 {
                    s.send(d.source.client, Msg::new(d.source.id, wl_data_source::evt::DND_FINISHED));
                }
            }
        }
        _ => {}
    }
}

/// The button went up: drop on the target, or cancel.
pub fn drop(s: &mut State) {
    let Some(d) = s.drag.take() else { return };
    let source = d.source.map(|id| SKey { client: d.client, id });
    let v3 = source_version(s, &d) >= 3;
    let accepted = d.accepted.is_some() && (d.action != 0 || !v3 || d.target_actions == (0, 0));
    match (d.target, accepted || source.is_none()) {
        (Some((target, offer)), true) => {
            for (dev, _) in s.objects_of(target.client, Kind::DataDevice) {
                s.send(target.client, Msg::new(dev, wl_data_device::evt::DROP));
            }
            if let Some(src) = source {
                if v3 {
                    s.send(src.client, Msg::new(src.id, wl_data_source::evt::DND_DROP_PERFORMED));
                }
                if offer != 0 {
                    s.dropped = Some(Dropped { offer: SKey { client: target.client, id: offer }, source: src });
                }
            }
        }
        (target, _) => {
            if let Some((t, _)) = target {
                for (dev, _) in s.objects_of(t.client, Kind::DataDevice) {
                    s.send(t.client, Msg::new(dev, wl_data_device::evt::LEAVE));
                }
            }
            if let Some(src) = source {
                s.send(src.client, Msg::new(src.id, wl_data_source::evt::CANCELLED));
            }
        }
    }
    s.pointer_dirty = true;
    s.needs_redraw = true;
}

/// A source or a client went away mid-drag.
pub fn source_gone(s: &mut State, c: ClientId, id: u32) {
    if s.drag.as_ref().is_some_and(|d| d.client == c && d.source == Some(id)) {
        leave(s);
        s.drag = None;
        s.pointer_dirty = true;
    }
    if s.dropped.as_ref().is_some_and(|d| d.source == SKey { client: c, id }) {
        s.dropped = None;
    }
}

pub fn client_gone(s: &mut State, c: ClientId) {
    if s.drag.as_ref().is_some_and(|d| d.client == c) {
        leave(s);
        s.drag = None;
        s.pointer_dirty = true;
    } else if s.drag.as_ref().is_some_and(|d| d.target.is_some_and(|(t, _)| t.client == c)) {
        if let Some(d) = s.drag.as_mut() {
            d.target = None;
        }
    }
    if s.dropped.as_ref().is_some_and(|d| d.offer.client == c || d.source.client == c) {
        s.dropped = None;
    }
}

/// The icon follows the pointer.
pub fn draw_icon(s: &State, r: &mut render::Renderer) {
    if let Some(icon) = s.drag.as_ref().and_then(|d| d.icon) {
        s.draw_tree(r, icon, (s.pointer.x as i32, s.pointer.y as i32));
    }
}
