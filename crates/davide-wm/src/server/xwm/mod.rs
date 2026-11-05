//! X11 window manager for Xwayland (README, M6).
//!
//! Xwayland gives every mapped X window a wl_surface; both sides announce
//! the same serial (`xwayland_surface_v1.set_serial` and the
//! `WL_SURFACE_SERIAL` client message) and the pair becomes one window,
//! keyed by its surface like an xdg toplevel. Normal windows are tiled;
//! menus, tooltips and other override-redirect windows are drawn where X
//! puts them, on top.

use std::io;
use std::os::fd::OwnedFd;

use wire::*;
use wm::{FsMode, Rect};
use x11::{atom, mask, Conn, Event};

use super::*;

pub mod selection;

macro_rules! atoms {
    ($($field:ident),* $(,)?) => {
        #[allow(non_snake_case)]
        #[derive(Clone, Copy, Default)]
        struct Atoms { $($field: u32),* }

        impl Atoms {
            const NAMES: &'static [&'static str] = &[$(stringify!($field)),*];

            fn from(v: &[u32]) -> Atoms {
                let mut it = v.iter().copied();
                Atoms { $($field: it.next().unwrap_or(0)),* }
            }

            fn list(&self) -> Vec<u32> {
                vec![$(self.$field),*]
            }
        }
    };
}

atoms!(
    WM_PROTOCOLS,
    WM_DELETE_WINDOW,
    WM_TAKE_FOCUS,
    WM_STATE,
    WM_S0,
    UTF8_STRING,
    WL_SURFACE_ID,
    WL_SURFACE_SERIAL,
    CLIPBOARD,
    TARGETS,
    TIMESTAMP,
    INCR,
    TEXT,
    MULTIPLE,
    SAVE_TARGETS,
    _WL_SELECTION,
    _NET_WM_CM_S0,
    _NET_SUPPORTED,
    _NET_SUPPORTING_WM_CHECK,
    _NET_WM_NAME,
    _NET_ACTIVE_WINDOW,
    _NET_CLIENT_LIST,
    _NET_WM_STATE,
    _NET_WM_STATE_FULLSCREEN,
    _NET_WM_STATE_FOCUSED,
    _NET_WM_WINDOW_TYPE,
    _NET_WM_WINDOW_TYPE_MENU,
    _NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
    _NET_WM_WINDOW_TYPE_POPUP_MENU,
    _NET_WM_WINDOW_TYPE_TOOLTIP,
    _NET_WM_WINDOW_TYPE_NOTIFICATION,
    _NET_WM_WINDOW_TYPE_COMBO,
    _NET_WM_WINDOW_TYPE_DND,
    _NET_WM_WINDOW_TYPE_SPLASH,
    _NET_WM_WINDOW_TYPE_DIALOG,
    _NET_WM_WINDOW_TYPE_UTILITY,
);

pub struct XWindow {
    pub override_redirect: bool,
    /// Where X has the window (ConfigureNotify).
    pub geometry: Rect,
    /// Last geometry we asked for.
    sent: Option<Rect>,
    /// Viewable in X.
    pub mapped: bool,
    serial: Option<u64>,
    pub surface: Option<u32>,
    /// Paired and mapped: on screen as a tile or on top.
    pub shown: bool,
    /// Tiled, as opposed to drawn at its own geometry.
    pub managed: bool,
    pub title: String,
    net_title: bool,
    pub class: String,
    delete: bool,
    take_focus: bool,
    input: bool,
    /// A menu, tooltip or the like: never tiled.
    popup: bool,
    /// A dialog (`WM_TRANSIENT_FOR`, dialog or utility type) or a window
    /// whose minimum and maximum size are the same: it floats.
    transient: bool,
    dialog: bool,
    fixed: bool,
    /// Wants to start fullscreen (`_NET_WM_STATE` before mapping).
    wants_fullscreen: bool,
    /// What `_NET_WM_STATE` says now.
    fullscreen: bool,
}

impl XWindow {
    pub fn floats(&self) -> bool {
        self.transient || self.dialog || self.fixed
    }

    fn new(override_redirect: bool, geometry: Rect) -> XWindow {
        XWindow {
            override_redirect,
            geometry,
            sent: None,
            mapped: false,
            serial: None,
            surface: None,
            shown: false,
            managed: false,
            title: String::new(),
            net_title: false,
            class: String::new(),
            delete: false,
            take_focus: false,
            input: true,
            popup: false,
            transient: false,
            dialog: false,
            fixed: false,
            wants_fullscreen: false,
            fullscreen: false,
        }
    }
}

pub struct Xwm {
    pub conn: Conn,
    atoms: Atoms,
    /// The `_NET_SUPPORTING_WM_CHECK` window.
    check: u32,
    pub windows: HashMap<u32, XWindow>,
    /// Shown windows drawn at their own geometry, bottom to top.
    pub floating: Vec<u32>,
    focused: Option<u32>,
    pub sel: selection::Bridge,
}

const MAX_PROPERTY: u32 = 64 << 10;

/// Connects to Xwayland on its `-wm` socket and takes the WM role.
pub fn start(s: &mut State, fd: OwnedFd) -> io::Result<()> {
    let mut conn = Conn::connect(fd)?;
    let atoms = Atoms::from(&conn.intern_atoms(Atoms::NAMES)?);
    let root = conn.setup.root;
    let (composite, _) = conn.query_extension("Composite")?.ok_or_else(|| io::Error::other("no Composite extension"))?;
    conn.extension_call(composite, 0, &[0, 4])?;
    // Manual redirection is what makes Xwayland give windows wl_surfaces.
    conn.extension(composite, 2, &[root, 1]);
    conn.change_attributes(root, x11::cw::EVENT_MASK, &[mask::SUBSTRUCTURE_REDIRECT | mask::SUBSTRUCTURE_NOTIFY | mask::PROPERTY_CHANGE]);
    let check = conn.new_id();
    conn.create_window(check, root, -100, -100, 1, 1, true, x11::cw::OVERRIDE_REDIRECT, &[1]);
    for w in [root, check] {
        conn.change_property32(w, atoms._NET_SUPPORTING_WM_CHECK, atom::WINDOW, &[check]);
    }
    conn.change_property8(check, atoms._NET_WM_NAME, atoms.UTF8_STRING, b"davide-wm");
    let supported = [
        atoms._NET_SUPPORTED,
        atoms._NET_SUPPORTING_WM_CHECK,
        atoms._NET_WM_NAME,
        atoms._NET_ACTIVE_WINDOW,
        atoms._NET_CLIENT_LIST,
        atoms._NET_WM_STATE,
        atoms._NET_WM_STATE_FULLSCREEN,
        atoms._NET_WM_STATE_FOCUSED,
        atoms._NET_WM_WINDOW_TYPE,
    ];
    conn.change_property32(root, atoms._NET_SUPPORTED, atom::ATOM, &supported);
    conn.change_property32(root, atoms._NET_CLIENT_LIST, atom::WINDOW, &[]);
    conn.change_property32(root, atoms._NET_ACTIVE_WINDOW, atom::WINDOW, &[0]);
    conn.set_selection_owner(check, atoms.WM_S0, 0);
    conn.set_selection_owner(check, atoms._NET_WM_CM_S0, 0);
    let sel = selection::init(&mut conn, &atoms)?;
    let existing = conn.children(root)?;
    let ours = [check, sel_window(&sel)];
    s.xwm = Some(Xwm { conn, atoms, check, windows: HashMap::new(), floating: Vec::new(), focused: None, sel });
    // Windows created before we took over map without asking.
    for w in existing.into_iter().filter(|w| !ours.contains(w)) {
        let Some((map_state, override_redirect)) = s.xwm.as_mut().unwrap().conn.window_attributes(w)? else { continue };
        created(s, w, override_redirect, Rect::default())?;
        if map_state == 2 {
            if let Some(win) = s.xwm.as_mut().unwrap().windows.get_mut(&w) {
                win.mapped = true;
            }
        }
    }
    for which in [super::selection::Which::Clipboard, super::selection::Which::Primary] {
        selection::wayland_changed(s, which);
    }
    s.xwm.as_mut().unwrap().conn.flush()
}

fn sel_window(b: &selection::Bridge) -> u32 {
    b.window()
}

/// Xwayland went away: its windows go with it.
pub fn stop(s: &mut State) {
    let Some(x) = s.xwm.as_ref() else { return };
    let ids: Vec<u32> = x.windows.keys().copied().collect();
    for w in ids {
        hide(s, w);
    }
    selection::stop(s);
    s.xwm = None;
}

/// Handles everything X sent. An error means the connection is gone.
pub fn dispatch(s: &mut State) -> io::Result<()> {
    loop {
        let Some(x) = s.xwm.as_mut() else { return Ok(()) };
        let Some(ev) = x.conn.next_event()? else { break };
        event(s, ev)?;
    }
    flush(s)
}

pub fn flush(s: &mut State) -> io::Result<()> {
    match s.xwm.as_mut() {
        Some(x) => x.conn.flush(),
        None => Ok(()),
    }
}

fn event(s: &mut State, ev: Event) -> io::Result<()> {
    if selection::event(s, &ev)? {
        return Ok(());
    }
    let x = s.xwm.as_mut().unwrap();
    let root = x.conn.setup.root;
    match ev {
        Event::Create { parent, window, override_redirect, x: wx, y, w, h } if parent == root && window != x.check => {
            created(s, window, override_redirect, Rect::new(wx as i32, y as i32, w as i32, h as i32))?;
        }
        Event::Destroy { window } => {
            hide(s, window);
            s.xwm.as_mut().unwrap().windows.remove(&window);
            update_client_list(s);
        }
        Event::MapRequest { window } => {
            read_properties(s, window, None)?;
            let x = s.xwm.as_mut().unwrap();
            let wm_state = x.atoms.WM_STATE;
            x.conn.change_property32(window, wm_state, wm_state, &[1, 0]);
            x.conn.map_window(window);
        }
        Event::Map { window, override_redirect } => {
            let Some(w) = x.windows.get_mut(&window) else { return Ok(()) };
            w.mapped = true;
            w.override_redirect = override_redirect;
            show(s, window);
        }
        Event::Unmap { window } => {
            hide(s, window);
            let x = s.xwm.as_mut().unwrap();
            let Some(w) = x.windows.get_mut(&window) else { return Ok(()) };
            (w.mapped, w.serial, w.surface, w.sent) = (false, None, None, None);
            if !w.override_redirect {
                let wm_state = x.atoms.WM_STATE;
                x.conn.change_property32(window, wm_state, wm_state, &[0, 0]);
            }
        }
        Event::Configure { window, x: wx, y, w, h, override_redirect } => {
            let Some(win) = x.windows.get_mut(&window) else { return Ok(()) };
            win.geometry = Rect::new(wx as i32, y as i32, w as i32, h as i32);
            win.override_redirect = override_redirect;
            if win.shown && !win.managed {
                s.needs_redraw = true;
                s.pointer_dirty = true;
            } else if let Some(key) = key_of(s, window) {
                s.float_resized(key);
            }
        }
        Event::ConfigureRequest { window, mask: m, x: wx, y, w, h, sibling, stack_mode } => {
            if !x.windows.contains_key(&window) {
                return Ok(());
            }
            let floating = key_of(s, window).is_some_and(|k| s.is_floating(k));
            let x = s.xwm.as_mut().unwrap();
            let win = &x.windows[&window];
            if floating {
                // A dialog may pick its size; the place stays ours.
                let m = m & (x11::config::WIDTH | x11::config::HEIGHT);
                let values: Vec<u32> = [(x11::config::WIDTH, w as u32), (x11::config::HEIGHT, h as u32)].iter().filter(|(b, _)| m & b != 0).map(|(_, v)| *v).collect();
                if m != 0 {
                    x.conn.configure_window(window, m, &values);
                }
                return Ok(());
            }
            match win.sent.filter(|_| win.managed) {
                // Tiled: the layout decides; tell the client so.
                Some(r) => {
                    let ev = x11::configure_notify(window, r.x as i16, r.y as i16, r.w as u16, r.h as u16);
                    x.conn.send_event(window, mask::STRUCTURE_NOTIFY, &ev);
                }
                None => {
                    use x11::config as c;
                    let all = [
                        (c::X, wx as i32 as u32),
                        (c::Y, y as i32 as u32),
                        (c::WIDTH, w as u32),
                        (c::HEIGHT, h as u32),
                        (c::BORDER, 0),
                        (c::SIBLING, sibling),
                        (c::STACK_MODE, stack_mode as u32),
                    ];
                    let m = m | c::BORDER;
                    let values: Vec<u32> = all.iter().filter(|(bit, _)| m & bit != 0).map(|(_, v)| *v).collect();
                    x.conn.configure_window(window, m, &values);
                }
            }
        }
        Event::Property { window, atom: a, .. } => {
            if x.windows.contains_key(&window) {
                read_properties(s, window, Some(a))?;
            }
        }
        Event::ClientMessage { window, ty, data, .. } => client_message(s, window, ty, data),
        // X clients must not take the focus behind the compositor's back;
        // grabs (modes 1 and 2) are their business.
        Event::FocusIn { window, mode: 0 } => {
            if let Some(f) = x.focused.filter(|f| *f != window) {
                x.conn.set_input_focus(f, 1, 0);
            }
        }
        _ => {}
    }
    Ok(())
}

fn created(s: &mut State, window: u32, override_redirect: bool, geometry: Rect) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    x.windows.insert(window, XWindow::new(override_redirect, geometry));
    x.conn.change_attributes(window, x11::cw::EVENT_MASK, &[mask::PROPERTY_CHANGE | mask::FOCUS_CHANGE]);
    read_properties(s, window, None)
}

/// Reads one property of a window, or all the interesting ones.
fn read_properties(s: &mut State, window: u32, only: Option<u32>) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    let a = x.atoms;
    let all = [
        atom::WM_NAME,
        a._NET_WM_NAME,
        atom::WM_CLASS,
        a.WM_PROTOCOLS,
        atom::WM_HINTS,
        a._NET_WM_WINDOW_TYPE,
        a._NET_WM_STATE,
        atom::WM_TRANSIENT_FOR,
        atom::WM_NORMAL_HINTS,
    ];
    let names: Vec<u32> = match only {
        Some(p) if all.contains(&p) => vec![p],
        Some(_) => return Ok(()),
        None => all.to_vec(),
    };
    let values = x.conn.properties(window, &names, MAX_PROPERTY)?;
    let mut urgent = false;
    let Some(w) = x.windows.get_mut(&window) else { return Ok(()) };
    for (name, p) in names.into_iter().zip(values) {
        match name {
            atom::WM_NAME if !w.net_title => w.title = p.map(|p| p.text()).unwrap_or_default(),
            n if n == a._NET_WM_NAME => {
                w.net_title = p.is_some();
                if let Some(p) = p {
                    w.title = p.text();
                }
            }
            atom::WM_CLASS => {
                // "instance\0class\0"
                let text = p.map(|p| p.text()).unwrap_or_default();
                w.class = text.split('\0').nth(1).unwrap_or_default().to_string();
            }
            n if n == a.WM_PROTOCOLS => {
                let list = p.map(|p| p.u32s()).unwrap_or_default();
                w.delete = list.contains(&a.WM_DELETE_WINDOW);
                w.take_focus = list.contains(&a.WM_TAKE_FOCUS);
            }
            atom::WM_HINTS => {
                let hints = p.map(|p| p.u32s()).unwrap_or_default();
                let flags = hints.first().copied().unwrap_or(0);
                // InputHint, else input is assumed.
                w.input = flags & 1 == 0 || hints.get(1).is_some_and(|v| *v != 0);
                urgent = flags & 256 != 0;
            }
            n if n == a._NET_WM_WINDOW_TYPE => {
                let types = p.map(|p| p.u32s()).unwrap_or_default();
                let popups = [
                    a._NET_WM_WINDOW_TYPE_MENU,
                    a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
                    a._NET_WM_WINDOW_TYPE_POPUP_MENU,
                    a._NET_WM_WINDOW_TYPE_TOOLTIP,
                    a._NET_WM_WINDOW_TYPE_NOTIFICATION,
                    a._NET_WM_WINDOW_TYPE_COMBO,
                    a._NET_WM_WINDOW_TYPE_DND,
                    a._NET_WM_WINDOW_TYPE_SPLASH,
                ];
                // The first type the WM knows wins.
                w.popup = types.first().is_some_and(|t| popups.contains(t));
                w.dialog = types.first().is_some_and(|t| [a._NET_WM_WINDOW_TYPE_DIALOG, a._NET_WM_WINDOW_TYPE_UTILITY].contains(t));
            }
            atom::WM_TRANSIENT_FOR => w.transient = p.map(|p| p.u32s()).is_some_and(|v| v.first().is_some_and(|t| *t != 0)),
            atom::WM_NORMAL_HINTS => {
                // flags, x, y, w, h, min w/h, max w/h: PMinSize 16, PMaxSize 32.
                let h = p.map(|p| p.u32s()).unwrap_or_default();
                w.fixed = h.len() >= 9 && h[0] & 48 == 48 && h[5] > 0 && h[5] == h[7] && h[6] == h[8];
            }
            n if n == a._NET_WM_STATE => {
                let fs = p.map(|p| p.u32s()).unwrap_or_default().contains(&a._NET_WM_STATE_FULLSCREEN);
                if !w.shown {
                    w.wants_fullscreen = fs;
                }
            }
            _ => {}
        }
    }
    if urgent {
        if let Some(key) = key_of(s, window) {
            workspaces::mark_urgent(s, key);
        }
    }
    Ok(())
}

fn client_message(s: &mut State, window: u32, ty: u32, data: [u32; 5]) {
    let Some(a) = s.xwm.as_ref().map(|x| x.atoms) else { return };
    let key = key_of(s, window);
    let x = s.xwm.as_mut().unwrap();
    if ty == a.WL_SURFACE_SERIAL {
        if let Some(w) = x.windows.get_mut(&window) {
            w.serial = Some(data[0] as u64 | (data[1] as u64) << 32);
            pair(s, window);
        }
    } else if ty == a.WL_SURFACE_ID {
        // Xwayland without xwayland_shell_v1 names the surface directly.
        if let Some(w) = x.windows.get_mut(&window) {
            w.surface = Some(data[0]);
            show(s, window);
        }
    } else if ty == a._NET_WM_STATE {
        if data[1] != a._NET_WM_STATE_FULLSCREEN && data[2] != a._NET_WM_STATE_FULLSCREEN {
            return;
        }
        let toggle = |now: bool| match data[0] {
            0 => false,
            1 => true,
            _ => !now,
        };
        let Some(key) = key else {
            // Not shown yet: remember it for the map.
            if let Some(w) = x.windows.get_mut(&window) {
                w.wants_fullscreen = toggle(w.wants_fullscreen);
            }
            return;
        };
        if s.rule(key).is_some_and(|r| r.suppress_fullscreen) {
            return;
        }
        let want = toggle(s.fs_mode(key) == Some(FsMode::Fullscreen));
        workspaces::set_fullscreen(s, key, want.then_some(FsMode::Fullscreen));
    } else if ty == a._NET_ACTIVE_WINDOW {
        let Some(key) = key else { return };
        if s.window_ws.get(&key) == Some(&workspaces::focused_ws(s)) {
            seat::set_focus(s, Some(key));
        } else {
            workspaces::mark_urgent(s, key);
        }
    }
}

/// `xwayland_surface_v1.set_serial`: the Wayland half of a pair.
pub fn set_serial(s: &mut State, surface: u32, serial: u64) {
    s.xwayland_serials.insert(surface, serial);
    let Some(x) = s.xwm.as_ref() else { return };
    if let Some(window) = x.windows.iter().find(|(_, w)| w.serial == Some(serial) && w.surface.is_none()).map(|(id, _)| *id) {
        pair(s, window);
    }
}

fn pair(s: &mut State, window: u32) {
    let Some(w) = s.xwm.as_mut().and_then(|x| x.windows.get_mut(&window)) else { return };
    let Some(serial) = w.serial.filter(|_| w.surface.is_none()) else { return };
    let Some(surface) = s.xwayland_serials.iter().find(|(_, v)| **v == serial).map(|(k, _)| *k) else { return };
    s.xwayland_serials.remove(&surface);
    w.surface = Some(surface);
    show(s, window);
}

/// Puts a window on screen once it is both mapped and paired.
fn show(s: &mut State, window: u32) {
    let Some(client) = s.xwayland else { return };
    let Some(w) = s.xwm.as_ref().and_then(|x| x.windows.get(&window)) else { return };
    let Some(surface) = w.surface.filter(|_| w.mapped && !w.shown) else { return };
    let key = SKey { client, id: surface };
    if !s.surfaces.contains_key(&key) {
        return;
    }
    let w = s.xwm.as_mut().unwrap().windows.get_mut(&window).unwrap();
    w.shown = true;
    w.managed = !w.override_redirect && !w.popup;
    let (managed, fullscreen) = (w.managed, w.wants_fullscreen);
    for (o, _) in s.objects_of(client, Kind::Output) {
        s.send(client, Msg::new(surface, wl_surface::evt::ENTER).obj(o));
    }
    if managed {
        workspaces::add_window(s, key);
        if fullscreen && !s.rule(key).is_some_and(|r| r.suppress_fullscreen) {
            workspaces::set_fullscreen(s, key, Some(FsMode::Fullscreen));
        }
        if s.window_ws.get(&key) == Some(&workspaces::focused_ws(s)) {
            seat::set_focus(s, Some(key));
        } else {
            workspaces::mark_urgent(s, key);
        }
        update_client_list(s);
    } else {
        s.xwm.as_mut().unwrap().floating.push(window);
    }
    s.needs_redraw = true;
    s.pointer_dirty = true;
}

fn hide(s: &mut State, window: u32) {
    let Some(client) = s.xwayland else { return };
    let Some(x) = s.xwm.as_mut() else { return };
    let Some(w) = x.windows.get_mut(&window).filter(|w| w.shown) else { return };
    w.shown = false;
    let (managed, surface) = (std::mem::take(&mut w.managed), w.surface);
    x.floating.retain(|f| *f != window);
    if x.focused == Some(window) {
        x.focused = None;
    }
    if let (true, Some(id)) = (managed, surface) {
        workspaces::remove_window(s, SKey { client, id });
        update_client_list(s);
    }
    s.needs_redraw = true;
    s.pointer_dirty = true;
}

/// A surface of Xwayland was destroyed: its window is off screen.
pub fn surface_gone(s: &mut State, key: SKey) {
    if s.xwayland != Some(key.client) {
        return;
    }
    s.xwayland_serials.remove(&key.id);
    let Some(window) = window_of(s, key) else { return };
    hide(s, window);
    if let Some(w) = s.xwm.as_mut().and_then(|x| x.windows.get_mut(&window)) {
        (w.surface, w.serial) = (None, None);
    }
}

fn update_client_list(s: &mut State) {
    let Some(x) = s.xwm.as_mut() else { return };
    let mut list: Vec<u32> = x.windows.iter().filter(|(_, w)| w.managed).map(|(id, _)| *id).collect();
    list.sort_unstable();
    let (root, prop) = (x.conn.setup.root, x.atoms._NET_CLIENT_LIST);
    x.conn.change_property32(root, prop, atom::WINDOW, &list);
}

pub fn window_of(s: &State, key: SKey) -> Option<u32> {
    if s.xwayland != Some(key.client) {
        return None;
    }
    s.xwm.as_ref()?.windows.iter().find(|(_, w)| w.surface == Some(key.id)).map(|(id, _)| *id)
}

pub fn window(s: &State, key: SKey) -> Option<&XWindow> {
    let id = window_of(s, key)?;
    s.xwm.as_ref()?.windows.get(&id)
}

fn key_of(s: &State, window: u32) -> Option<SKey> {
    let w = s.xwm.as_ref()?.windows.get(&window)?;
    Some(SKey { client: s.xwayland?, id: w.surface.filter(|_| w.shown)? })
}

/// Shown windows drawn at their own geometry, bottom to top.
pub fn floating(s: &State) -> Vec<(SKey, (i32, i32))> {
    let (Some(x), Some(client)) = (s.xwm.as_ref(), s.xwayland) else { return Vec::new() };
    x.floating
        .iter()
        .filter_map(|id| x.windows.get(id))
        .filter_map(|w| Some((SKey { client, id: w.surface? }, (w.geometry.x, w.geometry.y))))
        .collect()
}

/// Gives a tiled window its rectangle (and tells it whether it is fullscreen).
pub fn configure(s: &mut State, key: SKey, r: Rect) {
    let fullscreen = s.fs_mode(key) == Some(FsMode::Fullscreen);
    let Some(window) = window_of(s, key) else { return };
    let x = s.xwm.as_mut().unwrap();
    let a = x.atoms;
    let w = x.windows.get_mut(&window).unwrap();
    if w.sent != Some(r) {
        w.sent = Some(r);
        use x11::config as c;
        let values = [r.x as u32, r.y as u32, r.w.max(1) as u32, r.h.max(1) as u32, 0];
        x.conn.configure_window(window, c::X | c::Y | c::WIDTH | c::HEIGHT | c::BORDER, &values);
    }
    if w.fullscreen != fullscreen {
        w.fullscreen = fullscreen;
        let list: &[u32] = if fullscreen { &[a._NET_WM_STATE_FULLSCREEN] } else { &[] };
        x.conn.change_property32(window, a._NET_WM_STATE, atom::ATOM, list);
    }
}

/// The compositor moved the keyboard focus.
pub fn focus(s: &mut State, key: Option<SKey>) {
    let window = key.and_then(|k| window_of(s, k));
    let Some(x) = s.xwm.as_mut() else { return };
    if x.focused == window {
        return;
    }
    let a = x.atoms;
    let root = x.conn.setup.root;
    match window.and_then(|id| x.windows.get(&id).map(|w| (id, w))) {
        Some((id, w)) => {
            if w.input {
                x.conn.set_input_focus(id, 1, 0);
            }
            if w.take_focus {
                let ev = x11::client_message(id, a.WM_PROTOCOLS, [a.WM_TAKE_FOCUS, 0, 0, 0, 0]);
                x.conn.send_event(id, 0, &ev);
            }
            x.conn.configure_window(id, x11::config::STACK_MODE, &[0]);
            x.conn.change_property32(root, a._NET_ACTIVE_WINDOW, atom::WINDOW, &[id]);
            x.focused = Some(id);
        }
        None => {
            x.conn.set_input_focus(0, 1, 0);
            x.conn.change_property32(root, a._NET_ACTIVE_WINDOW, atom::WINDOW, &[0]);
            x.focused = None;
        }
    }
}

/// Asks a window to close, or disconnects its client if it cannot be asked.
pub fn close(s: &mut State, key: SKey) {
    let Some(window) = window_of(s, key) else { return };
    let x = s.xwm.as_mut().unwrap();
    let a = x.atoms;
    if x.windows[&window].delete {
        let ev = x11::client_message(window, a.WM_PROTOCOLS, [a.WM_DELETE_WINDOW, 0, 0, 0, 0]);
        x.conn.send_event(window, 0, &ev);
    } else {
        x.conn.kill_client(window);
    }
}

/// `xwayland_shell_v1` and `xwayland_surface_v1` requests.
pub fn request(s: &mut State, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, args: Args) {
    match (kind, op) {
        (Kind::XwaylandShell, xwayland_shell_v1::req::GET_XWAYLAND_SURFACE) => {
            let (new, surface) = (args.obj(0), args.obj(1));
            let key = SKey { client: c, id: surface };
            let free = s.surfaces.get(&key).is_some_and(|sf| matches!(sf.role, Role::None));
            if !free {
                s.error(c, id, xwayland_shell_v1::error::ROLE, "surface already has a role");
                return;
            }
            if s.new_object(c, new, &xwayland_surface_v1::INTERFACE, version, Kind::XwaylandSurface { surface }) {
                s.surfaces.get_mut(&key).unwrap().role = Role::Xwayland;
            }
        }
        (Kind::XwaylandSurface { surface }, xwayland_surface_v1::req::SET_SERIAL) => {
            set_serial(s, surface, args.uint(0) as u64 | (args.uint(1) as u64) << 32);
        }
        _ => {}
    }
}

use state::{persist_struct, Dec, Enc, Persist, Result};

persist_struct!(XWindow {
    override_redirect,
    geometry,
    sent,
    mapped,
    serial,
    surface,
    shown,
    managed,
    title,
    net_title,
    class,
    delete,
    take_focus,
    input,
    popup,
    transient,
    dialog,
    fixed,
    wants_fullscreen,
    fullscreen,
});

impl Persist for Xwm {
    fn save(&self, e: &mut Enc) {
        self.conn.save(e);
        self.atoms.list().save(e);
        (self.check, self.focused).save(e);
        self.windows.save(e);
        self.floating.save(e);
        self.sel.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let conn = Persist::load(d)?;
        let atoms = Atoms::from(&Vec::<u32>::load(d)?);
        let (check, focused) = Persist::load(d)?;
        Ok(Xwm { conn, atoms, check, windows: Persist::load(d)?, floating: Persist::load(d)?, focused, sel: Persist::load(d)? })
    }
}
