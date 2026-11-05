//! Clipboard and primary selection between X and Wayland clients.
//!
//! A Wayland selection is announced in X by owning the X selection with a
//! window of ours; an X client asking for it gets the data the Wayland owner
//! writes into a pipe, read on the worker. An X selection becomes a Wayland
//! selection whose data is converted from the X owner on demand, one
//! transfer at a time (INCR included).

use std::collections::VecDeque;
use std::io;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use x11::{atom, mask, Event};

use super::super::clipboard;
use super::super::selection::{self as wsel, Which};
use super::*;

const INTEGER: u32 = 19;
/// An X owner that does not answer within this time is given up on.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// A conversion asked of an X selection owner.
struct Transfer {
    which: Which,
    target: u32,
    /// Where the data goes; None while reading the offered targets.
    fd: Option<OwnedFd>,
    data: Vec<u8>,
    incr: bool,
    started: Option<Instant>,
}

/// An X client asking for a Wayland selection, answered once the data is read.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    requestor: u32,
    selection: u32,
    target: u32,
    property: u32,
    time: u32,
}

pub struct Bridge {
    window: u32,
    xfixes_event: u8,
    /// We own the X selection on behalf of a Wayland client.
    owns: [bool; 2],
    transfers: VecDeque<Transfer>,
    /// Pipes from Wayland owners for the worker to read.
    pub outgoing: Vec<(OwnedFd, Request)>,
}

impl Bridge {
    pub fn window(&self) -> u32 {
        self.window
    }
}

fn index(which: Which) -> usize {
    match which {
        Which::Clipboard => 0,
        Which::Primary => 1,
    }
}

fn selection_atom(a: &Atoms, which: Which) -> u32 {
    match which {
        Which::Clipboard => a.CLIPBOARD,
        Which::Primary => atom::PRIMARY,
    }
}

fn which_of(a: &Atoms, selection: u32) -> Option<Which> {
    match selection {
        s if s == a.CLIPBOARD => Some(Which::Clipboard),
        atom::PRIMARY => Some(Which::Primary),
        _ => None,
    }
}

/// What X offers as UTF8_STRING and STRING becomes these.
const TEXT_MIMES: [&str; 2] = ["text/plain;charset=utf-8", "text/plain"];
/// Text types Wayland clients offer, best first.
const WAYLAND_TEXT: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];

pub(super) fn init(conn: &mut x11::Conn, a: &Atoms) -> io::Result<Bridge> {
    let window = conn.new_id();
    conn.create_window(window, conn.setup.root, -100, -100, 1, 1, true, x11::cw::EVENT_MASK, &[mask::PROPERTY_CHANGE]);
    let (xfixes, xfixes_event) = conn.query_extension("XFIXES")?.ok_or_else(|| io::Error::other("no XFIXES extension"))?;
    conn.extension_call(xfixes, 0, &[5, 0])?;
    for selection in [a.CLIPBOARD, atom::PRIMARY] {
        // Owner changes, owner window destroyed, owner client gone.
        conn.extension(xfixes, 2, &[window, selection, 1 | 2 | 4]);
    }
    Ok(Bridge { window, xfixes_event, owns: [false; 2], transfers: VecDeque::new(), outgoing: Vec::new() })
}

/// Handles selection events; false for events that are not about selections.
pub fn event(s: &mut State, ev: &Event) -> io::Result<bool> {
    let x = s.xwm.as_mut().unwrap();
    let a = x.atoms;
    let window = x.sel.window;
    match *ev {
        Event::Other(code, raw) if code == x.sel.xfixes_event => {
            let owner = u32::from_le_bytes(raw[8..12].try_into().unwrap());
            let selection = u32::from_le_bytes(raw[12..16].try_into().unwrap());
            if let Some(which) = which_of(&a, selection) {
                owner_changed(s, which, owner)?;
            }
        }
        Event::SelectionNotify { requestor, property, .. } if requestor == window => {
            if x.sel.transfers.front().is_none_or(|t| t.started.is_none()) {
                return Ok(true);
            }
            if property == atom::NONE {
                finish(s, None)?;
                return Ok(true);
            }
            let p = x.conn.take_property(window, a._WL_SELECTION)?;
            match p {
                Some(p) if p.ty == a.INCR => x.sel.transfers.front_mut().unwrap().incr = true,
                p => finish(s, Some(p.map(|p| p.data).unwrap_or_default()))?,
            }
        }
        Event::Property { window: w, atom: prop, deleted: false } if w == window && prop == a._WL_SELECTION => {
            if !x.sel.transfers.front().is_some_and(|t| t.incr) {
                return Ok(true);
            }
            let chunk = x.conn.take_property(window, a._WL_SELECTION)?.map(|p| p.data).unwrap_or_default();
            let t = x.sel.transfers.front_mut().unwrap();
            if chunk.is_empty() {
                let data = std::mem::take(&mut t.data);
                finish(s, Some(data))?;
            } else if t.data.len() + chunk.len() > config::CLIPBOARD_MAX_BYTES {
                finish(s, None)?;
            } else {
                t.data.extend_from_slice(&chunk);
            }
        }
        Event::Property { window: w, .. } if w == window => {}
        Event::SelectionClear { selection, .. } => {
            if let Some(which) = which_of(&a, selection) {
                x.sel.owns[index(which)] = false;
            }
        }
        Event::SelectionRequest { time, requestor, selection, target, property, .. } => {
            let property = if property == atom::NONE { target } else { property };
            request(s, Request { requestor, selection, target, property, time })?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn owner_changed(s: &mut State, which: Which, owner: u32) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    if owner == x.sel.window {
        return Ok(());
    }
    x.sel.owns[index(which)] = false;
    if owner == atom::NONE {
        wsel::clear_x11(s, which);
        return Ok(());
    }
    let targets = x.atoms.TARGETS;
    queue(s, Transfer { which, target: targets, fd: None, data: Vec::new(), incr: false, started: None })
}

fn queue(s: &mut State, t: Transfer) -> io::Result<()> {
    s.xwm.as_mut().unwrap().sel.transfers.push_back(t);
    pump(s)
}

/// Starts the next transfer unless one is running; drops a stuck one.
fn pump(s: &mut State) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    while x.sel.transfers.front().and_then(|t| t.started).is_some_and(|t| t.elapsed() > TRANSFER_TIMEOUT) {
        log!("X selection owner did not answer, giving up");
        x.sel.transfers.pop_front();
    }
    let a = x.atoms;
    let Some(t) = x.sel.transfers.front_mut().filter(|t| t.started.is_none()) else { return Ok(()) };
    t.started = Some(Instant::now());
    let (selection, target) = (selection_atom(&a, t.which), t.target);
    x.conn.convert_selection(x.sel.window, selection, target, a._WL_SELECTION, 0);
    Ok(())
}

fn finish(s: &mut State, data: Option<Vec<u8>>) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    let Some(t) = x.sel.transfers.pop_front() else { return Ok(()) };
    if t.target == x.atoms.TARGETS {
        let atoms: Vec<u32> = data.unwrap_or_default().as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        let mimes = mimes_of(x, &atoms)?;
        if mimes.is_empty() {
            wsel::clear_x11(s, t.which);
        } else {
            wsel::set_x11(s, t.which, mimes);
        }
    } else if let (Some(fd), Some(data)) = (t.fd, data) {
        clipboard::serve(fd, data);
    }
    pump(s)
}

/// Mime types for the targets an X owner offers.
fn mimes_of(x: &mut Xwm, atoms: &[u32]) -> io::Result<Vec<String>> {
    let a = x.atoms;
    let mut out: Vec<String> = Vec::new();
    for at in atoms {
        let mime = match *at {
            t if t == a.UTF8_STRING => Some(TEXT_MIMES[0].to_string()),
            t if t == a.TEXT || t == atom::STRING => Some(TEXT_MIMES[1].to_string()),
            t if t == a.TARGETS || t == a.TIMESTAMP || t == a.MULTIPLE || t == a.SAVE_TARGETS => None,
            t => x.conn.atom_name(t)?.filter(|n| n.contains('/')),
        };
        if let Some(m) = mime.filter(|m| !out.contains(m)) {
            out.push(m);
        }
    }
    Ok(out)
}

/// A Wayland client wants data of the X selection.
pub fn receive(s: &mut State, which: Which, mime: &str, fd: OwnedFd) {
    let Some(x) = s.xwm.as_mut() else { return };
    let a = x.atoms;
    let target = match mime {
        m if m == TEXT_MIMES[0] => a.UTF8_STRING,
        m if m == TEXT_MIMES[1] => atom::STRING,
        m => match x.conn.intern_atoms(&[m]) {
            Ok(v) => v[0],
            Err(_) => return,
        },
    };
    let t = Transfer { which, target, fd: Some(fd), data: Vec::new(), incr: false, started: None };
    if let Err(e) = queue(s, t) {
        log!("X selection: {e}");
    }
}

/// The Wayland selection changed: own the X selection, or let it go.
pub fn wayland_changed(s: &mut State, which: Which) {
    let has = wsel::has_wayland(s, which);
    let Some(x) = s.xwm.as_mut() else { return };
    let selection = selection_atom(&x.atoms, which);
    let owns = &mut x.sel.owns[index(which)];
    if has {
        *owns = true;
        x.conn.set_selection_owner(x.sel.window, selection, 0);
    } else if std::mem::take(owns) {
        x.conn.set_selection_owner(atom::NONE, selection, 0);
    }
}

fn notify(x: &mut Xwm, r: &Request, property: u32) {
    let ev = x11::selection_notify(r.time, r.requestor, r.selection, r.target, property);
    x.conn.send_event(r.requestor, 0, &ev);
}

/// An X client asks for the Wayland selection we announced.
fn request(s: &mut State, r: Request) -> io::Result<()> {
    let x = s.xwm.as_mut().unwrap();
    let a = x.atoms;
    let Some(which) = which_of(&a, r.selection).filter(|w| x.sel.owns[index(*w)]) else {
        notify(x, &r, atom::NONE);
        return Ok(());
    };
    let mimes = wsel::mimes(s, which);
    let x = s.xwm.as_mut().unwrap();
    if r.target == a.TARGETS {
        let mut targets = vec![a.TARGETS, a.TIMESTAMP];
        if mimes.iter().any(|m| WAYLAND_TEXT.contains(&m.as_str())) {
            targets.extend([a.UTF8_STRING, a.TEXT, atom::STRING]);
        }
        let others: Vec<&str> = mimes.iter().filter(|m| m.contains('/')).map(|m| m.as_str()).collect();
        targets.extend(x.conn.intern_atoms(&others)?);
        x.conn.change_property32(r.requestor, r.property, atom::ATOM, &targets);
        notify(x, &r, r.property);
        return Ok(());
    }
    if r.target == a.TIMESTAMP {
        x.conn.change_property32(r.requestor, r.property, INTEGER, &[0]);
        notify(x, &r, r.property);
        return Ok(());
    }
    let wanted = if [a.UTF8_STRING, a.TEXT, atom::STRING].contains(&r.target) {
        WAYLAND_TEXT.into_iter().find(|t| mimes.iter().any(|m| m == t)).map(str::to_string)
    } else {
        x.conn.atom_name(r.target)?.filter(|n| mimes.contains(n))
    };
    let Some(mime) = wanted else {
        notify(x, &r, atom::NONE);
        return Ok(());
    };
    let (read, write) = clipboard::pipe()?;
    wsel::request_data(s, which, &mime, write);
    s.xwm.as_mut().unwrap().sel.outgoing.push((read, r));
    Ok(())
}

/// The worker read what an X client asked for.
pub fn answer(s: &mut State, r: Request, data: std::result::Result<Vec<u8>, String>) {
    let Some(x) = s.xwm.as_mut() else { return };
    let ty = if r.target == x.atoms.TEXT { x.atoms.UTF8_STRING } else { r.target };
    let sent = match data {
        Ok(d) => x.conn.change_property8(r.requestor, r.property, ty, &d) || {
            log!("X selection request too large ({} bytes)", d.len());
            false
        },
        Err(e) => {
            log!("X selection request: {e}");
            false
        }
    };
    notify(x, &r, if sent { r.property } else { atom::NONE });
}

/// Selections X owned are gone with Xwayland.
pub fn stop(s: &mut State) {
    for which in [Which::Clipboard, Which::Primary] {
        wsel::clear_x11(s, which);
    }
}

impl state::Persist for Bridge {
    fn save(&self, e: &mut state::Enc) {
        (self.window, self.xfixes_event, self.owns[0], self.owns[1]).save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let (window, xfixes_event, c, p) = state::Persist::load(d)?;
        Ok(Bridge { window, xfixes_event, owns: [c, p], transfers: VecDeque::new(), outgoing: Vec::new() })
    }
}
