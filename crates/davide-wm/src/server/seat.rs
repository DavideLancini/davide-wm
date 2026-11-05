//! Seat (keyboard, pointer), outputs and input focus.

use render::{Quad, Renderer};
use wire::*;

use super::*;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CursorImage {
    Default,
    Hidden,
    Surface(SKey, i32, i32),
}

pub fn send_seat_info(s: &mut State, c: ClientId, id: u32, version: u32) {
    let caps = wl_seat::capability::POINTER | wl_seat::capability::KEYBOARD;
    s.send(c, Msg::new(id, wl_seat::evt::CAPABILITIES).uint(caps));
    if version >= 2 {
        s.send(c, Msg::new(id, wl_seat::evt::NAME).str("seat0"));
    }
}

pub fn send_output_info(s: &mut State, c: ClientId, id: u32, version: u32) {
    s.send(
        c,
        Msg::new(id, wl_output::evt::GEOMETRY)
            .int(0)
            .int(0)
            .int(0)
            .int(0)
            .int(wl_output::subpixel::UNKNOWN as i32)
            .str("davide-wm")
            .str(&s.output.name.clone())
            .int(wl_output::transform::NORMAL as i32),
    );
    if version >= 2 {
        s.send(c, Msg::new(id, wl_output::evt::SCALE).int(1));
    }
    if version >= 4 {
        let name = s.output.name.clone();
        s.send(c, Msg::new(id, wl_output::evt::NAME).str(&name));
        s.send(c, Msg::new(id, wl_output::evt::DESCRIPTION).str(&name));
    }
    send_output_mode(s, c, id, version);
}

pub fn send_output_mode(s: &mut State, c: ClientId, id: u32, version: u32) {
    let flags = wl_output::mode::CURRENT | wl_output::mode::PREFERRED;
    let (w, h) = (s.output.width, s.output.height);
    s.send(c, Msg::new(id, wl_output::evt::MODE).uint(flags).int(w).int(h).int(60000));
    if version >= 2 {
        s.send(c, Msg::new(id, wl_output::evt::DONE));
    }
}

pub fn seat(s: &mut State, c: ClientId, version: u32, op: u16, args: Args) {
    let id = args.obj(0);
    match op {
        wl_seat::req::GET_POINTER => {
            s.new_object(c, id, &wl_pointer::INTERFACE, version, Kind::Pointer);
        }
        wl_seat::req::GET_KEYBOARD => {
            if !s.new_object(c, id, &wl_keyboard::INTERFACE, version, Kind::Keyboard) {
                return;
            }
            match s.keyboard.keymap_fd() {
                Ok(fd) => {
                    let size = s.keyboard.keymap_size();
                    s.send(c, Msg::new(id, wl_keyboard::evt::KEYMAP).uint(wl_keyboard::keymap_format::XKB_V1).fd(fd).uint(size));
                }
                Err(e) => log!("keymap fd: {e}"),
            }
            if version >= 4 {
                s.send(c, Msg::new(id, wl_keyboard::evt::REPEAT_INFO).int(config::REPEAT_RATE).int(config::REPEAT_DELAY));
            }
            if let Some(f) = s.focus.filter(|f| f.client == c) {
                keyboard_enter(s, f, Some(id));
            }
        }
        wl_seat::req::GET_TOUCH => {
            s.new_object(c, id, &wl_touch::INTERFACE, version, Kind::Touch);
        }
        _ => {}
    }
}

pub fn pointer(s: &mut State, c: ClientId, op: u16, args: Args) {
    if op != wl_pointer::req::SET_CURSOR {
        return;
    }
    if s.pointer.focus.map(|f| f.client) != Some(c) {
        return;
    }
    let (surface, hx, hy) = (args.obj(1), args.int(2), args.int(3));
    if surface == 0 {
        s.pointer.cursor = CursorImage::Hidden;
    } else {
        let key = SKey { client: c, id: surface };
        let Some(sf) = s.surfaces.get_mut(&key) else { return };
        match sf.role {
            Role::None | Role::Cursor => sf.role = Role::Cursor,
            _ => {
                s.error(c, surface, wl_pointer::error::ROLE, "surface already has a role");
                return;
            }
        }
        s.pointer.cursor = CursorImage::Surface(key, hx, hy);
    }
    s.cursor_changed = true;
    s.cursor_image_dirty = true;
}

fn keyboard_enter(s: &mut State, key: SKey, only: Option<u32>) {
    let serial = s.next_serial();
    let mods = s.keyboard.wire_mods();
    let keys: Vec<u8> = s.keyboard.pressed().iter().flat_map(|k| k.to_ne_bytes()).collect();
    for (kb, _) in s.objects_of(key.client, Kind::Keyboard) {
        if only.is_some_and(|o| o != kb) {
            continue;
        }
        s.send(key.client, Msg::new(kb, wl_keyboard::evt::ENTER).uint(serial).obj(key.id).array(&keys));
        s.send(
            key.client,
            Msg::new(kb, wl_keyboard::evt::MODIFIERS).uint(serial).uint(mods.0).uint(mods.1).uint(mods.2).uint(mods.3),
        );
    }
}

/// Moves keyboard focus to a toplevel (or nowhere).
pub fn set_focus(s: &mut State, key: Option<SKey>) {
    if s.focus == key {
        return;
    }
    if (s.lock.is_some() || s.prompt.is_some()) && key.is_some() {
        // Remembered for when the lock or the prompt goes away.
        if let Some(ws) = key.and_then(|k| s.window_ws.get(&k)).and_then(|w| s.workspaces.get_mut(w)) {
            ws.focus = key;
        }
        return;
    }
    if let Some(cur) = s.focus {
        let pinned = s.rule(cur).is_some_and(|r| r.stay_focused) && s.shown(cur);
        if pinned && key.is_some() {
            return;
        }
    }
    let old_client = s.focus.map(|f| f.client);
    if let Some(old) = s.focus.take() {
        if s.surfaces.contains_key(&old) {
            let serial = s.next_serial();
            for (kb, _) in s.objects_of(old.client, Kind::Keyboard) {
                s.send(old.client, Msg::new(kb, wl_keyboard::evt::LEAVE).uint(serial).obj(old.id));
            }
        }
    }
    s.focus = key;
    if let Some(k) = key {
        s.urgent.remove(&k);
        if let Some(ws) = s.window_ws.get(&k).and_then(|w| s.workspaces.get_mut(w)) {
            ws.focus = Some(k);
            ws.raise(k);
        }
        keyboard_enter(s, k, None);
        if old_client != Some(k.client) {
            selection::focus_changed(s, k.client);
        }
    }
    xwm::focus(s, key);
    s.relayout();
}

/// Forwards a key to the focused client.
pub fn key(s: &mut State, keycode: u32, pressed: bool, mods_changed: bool) {
    let Some(f) = s.focus else { return };
    let serial = s.next_serial();
    let time = s.time_ms();
    let state = if pressed { wl_keyboard::key_state::PRESSED } else { wl_keyboard::key_state::RELEASED };
    let mods = s.keyboard.wire_mods();
    for (kb, _) in s.objects_of(f.client, Kind::Keyboard) {
        s.send(f.client, Msg::new(kb, wl_keyboard::evt::KEY).uint(serial).uint(time).uint(keycode).uint(state));
        if mods_changed {
            s.send(f.client, Msg::new(kb, wl_keyboard::evt::MODIFIERS).uint(serial).uint(mods.0).uint(mods.1).uint(mods.2).uint(mods.3));
        }
    }
}

fn surface_hit(s: &State, key: SKey, (x, y): (i32, i32), px: f64, py: f64) -> Option<(SKey, f64, f64)> {
    let sf = s.surfaces.get(&key)?;
    let child = |id: &u32| {
        let k = SKey { client: key.client, id: *id };
        match s.surfaces.get(&k).map(|c| &c.role) {
            Some(Role::Subsurface { x: cx, y: cy, .. }) => surface_hit(s, k, (x + cx, y + cy), px, py),
            _ => None,
        }
    };
    if let Some(hit) = sf.above.iter().rev().find_map(child) {
        return Some(hit);
    }
    if s.texture(key).is_some() && Rect::new(x, y, sf.size.0, sf.size.1).contains(px, py) {
        return Some((key, px - x as f64, py - y as f64));
    }
    sf.below.iter().rev().find_map(child)
}

/// The surface under a point, with surface-local coordinates.
pub fn surface_at(s: &State, px: f64, py: f64) -> Option<(SKey, f64, f64)> {
    for (key, origin) in xwm::floating(s).into_iter().rev() {
        if let Some(hit) = surface_hit(s, key, origin, px, py) {
            return Some(hit);
        }
    }
    for key in s.popups.iter().rev() {
        if let Some(origin) = s.window_origin(*key) {
            if let Some(hit) = surface_hit(s, *key, s.surface_origin(*key, origin), px, py) {
                return Some(hit);
            }
        }
    }
    for (key, rect) in s.layout.iter().rev() {
        if rect.contains(px, py) && s.shown(*key) {
            return surface_hit(s, *key, s.surface_origin(*key, (rect.x, rect.y)), px, py);
        }
    }
    None
}

/// The toplevel that owns a surface (itself, a subsurface or a popup of it).
pub fn toplevel_of(s: &State, mut key: SKey) -> Option<SKey> {
    loop {
        let sf = s.surfaces.get(&key)?;
        match (&sf.role, sf.xdg.as_ref().map(|x| &x.role)) {
            (Role::Subsurface { parent, .. }, _) => key.id = *parent,
            (_, Some(XdgRole::Popup { parent, .. })) => key.id = *parent,
            (_, Some(XdgRole::Toplevel { .. })) => return Some(key),
            (Role::Xwayland, _) => return xwm::window(s, key).filter(|w| w.managed).map(|_| key),
            _ => return None,
        }
    }
}

fn pointer_frame(s: &mut State, c: ClientId) {
    for (p, v) in s.objects_of(c, Kind::Pointer) {
        if v >= 5 {
            s.send(c, Msg::new(p, wl_pointer::evt::FRAME));
        }
    }
}

/// The pointer moved: the window under it gets the keyboard focus too
/// (focus follows the mouse, like Hyprland's default).
pub fn pointer_motion(s: &mut State, x: f64, y: f64) {
    if s.drag.is_some() {
        s.pointer.x = x.clamp(0.0, (s.output.width - 1).max(0) as f64);
        s.pointer.y = y.clamp(0.0, (s.output.height - 1).max(0) as f64);
        dnd::motion(s);
        return;
    }
    update_pointer(s, x, y);
    let top = s.pointer.focus.and_then(|k| toplevel_of(s, k));
    if top.is_some() && top != s.focus {
        set_focus(s, top);
    }
}

/// Puts the pointer at the centre of a window on screen, so focus follows
/// the keyboard action rather than snapping back on the next motion.
pub fn warp_to(s: &mut State, key: SKey) {
    let Some((_, r)) = s.layout.iter().find(|(k, _)| *k == key).copied() else { return };
    let (x, y) = ((r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64);
    update_pointer(s, x, y);
}

/// Re-targets the pointer after the windows under it changed (workspace
/// switch, new layout) without moving the keyboard focus.
pub fn refresh_pointer(s: &mut State) {
    s.pointer_dirty = false;
    if s.drag.is_some() {
        return;
    }
    update_pointer(s, s.pointer.x, s.pointer.y);
}

fn update_pointer(s: &mut State, x: f64, y: f64) {
    s.pointer.x = x.clamp(0.0, (s.output.width - 1).max(0) as f64);
    s.pointer.y = y.clamp(0.0, (s.output.height - 1).max(0) as f64);
    let hit = surface_at(s, s.pointer.x, s.pointer.y).filter(|_| s.lock.is_none() && s.prompt.is_none());
    let time = s.time_ms();
    let new = hit.map(|h| h.0);
    if new != s.pointer.focus {
        if let Some(old) = s.pointer.focus.take() {
            let serial = s.next_serial();
            for (p, _) in s.objects_of(old.client, Kind::Pointer) {
                s.send(old.client, Msg::new(p, wl_pointer::evt::LEAVE).uint(serial).obj(old.id));
            }
            pointer_frame(s, old.client);
        }
        s.pointer.cursor = CursorImage::Default;
        if let Some((k, lx, ly)) = hit {
            let serial = s.next_serial();
            for (p, _) in s.objects_of(k.client, Kind::Pointer) {
                s.send(k.client, Msg::new(p, wl_pointer::evt::ENTER).uint(serial).obj(k.id).fixed(lx).fixed(ly));
            }
            pointer_frame(s, k.client);
        }
        s.pointer.focus = new;
    } else if let Some((k, lx, ly)) = hit {
        for (p, _) in s.objects_of(k.client, Kind::Pointer) {
            s.send(k.client, Msg::new(p, wl_pointer::evt::MOTION).uint(time).fixed(lx).fixed(ly));
        }
        pointer_frame(s, k.client);
    }
    s.cursor_changed = true;
}

pub fn pointer_button(s: &mut State, button: u32, pressed: bool) {
    if s.drag.is_some() {
        if !pressed {
            dnd::drop(s);
        }
        return;
    }
    if pressed {
        let top = s.pointer.focus.and_then(|k| toplevel_of(s, k));
        if top.is_some() && top != s.focus {
            set_focus(s, top);
        }
    }
    let Some(k) = s.pointer.focus else { return };
    let serial = s.next_serial();
    let time = s.time_ms();
    let state = if pressed { wl_pointer::button_state::PRESSED } else { wl_pointer::button_state::RELEASED };
    for (p, _) in s.objects_of(k.client, Kind::Pointer) {
        s.send(k.client, Msg::new(p, wl_pointer::evt::BUTTON).uint(serial).uint(time).uint(button).uint(state));
    }
    pointer_frame(s, k.client);
}

pub fn pointer_axis(s: &mut State, axis: u32, value: f64) {
    if s.drag.is_some() {
        return;
    }
    let Some(k) = s.pointer.focus else { return };
    let time = s.time_ms();
    for (p, _) in s.objects_of(k.client, Kind::Pointer) {
        s.send(k.client, Msg::new(p, wl_pointer::evt::AXIS).uint(time).uint(axis).fixed(value));
    }
    pointer_frame(s, k.client);
}

pub fn draw_cursor(s: &State, r: &mut Renderer) {
    let (x, y) = (s.pointer.x as f32, s.pointer.y as f32);
    match s.pointer.cursor {
        CursorImage::Hidden => {}
        CursorImage::Surface(key, hx, hy) => {
            s.draw_tree(r, key, (x as i32 - hx, y as i32 - hy));
        }
        CursorImage::Default => {
            r.fill(Quad { x, y, w: 10.0, h: 10.0 }, [0.0, 0.0, 0.0, 1.0]);
            r.fill(Quad { x: x + 1.0, y: y + 1.0, w: 8.0, h: 8.0 }, [1.0, 1.0, 1.0, 1.0]);
        }
    }
}
