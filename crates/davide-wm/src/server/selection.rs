//! Clipboard and primary selection: `wl_data_device`, primary-selection and
//! ext-data-control. Drag and drop is not implemented.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use wire::*;

use super::clipboard::{self, Clip, History};
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    Clipboard,
    Primary,
}

#[derive(Default)]
pub struct Selection {
    /// Owner: client, source object and its kind.
    source: Option<(ClientId, u32, Kind)>,
    /// Without a client owner: a clip the compositor serves itself (picked
    /// from the history, or kept after its owner went away).
    server: Option<Clip>,
    /// The top of the history holds this selection's content, so it can be
    /// kept when its owner goes away.
    captured: bool,
    /// Owned by an X client, through the X window manager.
    x11: bool,
    mimes: Vec<String>,
    /// Bumped on every change, so offers of an old selection stop working.
    generation: u32,
}

#[derive(Default)]
pub struct Selections {
    clipboard: Selection,
    primary: Selection,
    /// Mime types offered by each source object.
    mimes: HashMap<SKey, Vec<String>>,
    pub history: History,
    /// Pipes the clipboard owner is writing into, with the generation they
    /// belong to; the event loop hands them to the worker.
    pub captures: Vec<(OwnedFd, u32, String)>,
}

impl Selections {
    fn get(&mut self, which: Which) -> &mut Selection {
        match which {
            Which::Clipboard => &mut self.clipboard,
            Which::Primary => &mut self.primary,
        }
    }
}

pub fn request(s: &mut State, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, mut args: Args) {
    let key = SKey { client: c, id };
    match kind {
        Kind::DataDeviceManager => match op {
            wl_data_device_manager::req::CREATE_DATA_SOURCE => new_source(s, c, args.obj(0), &wl_data_source::INTERFACE, version, Kind::DataSource),
            wl_data_device_manager::req::GET_DATA_DEVICE
                if s.new_object(c, args.obj(0), &wl_data_device::INTERFACE, version, Kind::DataDevice) && focused_client(s) == Some(c) => {
                    offer_to_client(s, c, Which::Clipboard);
                }
            _ => {}
        },
        Kind::PrimaryManager => match op {
            zwp_primary_selection_device_manager_v1::req::CREATE_SOURCE => {
                new_source(s, c, args.obj(0), &zwp_primary_selection_source_v1::INTERFACE, version, Kind::PrimarySource)
            }
            zwp_primary_selection_device_manager_v1::req::GET_DEVICE => {
                let dev = args.obj(0);
                if s.new_object(c, dev, &zwp_primary_selection_device_v1::INTERFACE, version, Kind::PrimaryDevice)
                    && focused_client(s) == Some(c)
                {
                    offer_to_client(s, c, Which::Primary);
                }
            }
            _ => {}
        },
        Kind::ControlManager => match op {
            ext_data_control_manager_v1::req::CREATE_DATA_SOURCE => {
                new_source(s, c, args.obj(0), &ext_data_control_source_v1::INTERFACE, version, Kind::ControlSource)
            }
            ext_data_control_manager_v1::req::GET_DATA_DEVICE => {
                let dev = args.obj(0);
                if s.new_object(c, dev, &ext_data_control_device_v1::INTERFACE, version, Kind::ControlDevice) {
                    offer_to_control(s, c, dev, version, Which::Clipboard);
                    offer_to_control(s, c, dev, version, Which::Primary);
                }
            }
            _ => {}
        },
        Kind::DataSource | Kind::PrimarySource | Kind::ControlSource => {
            // Opcode 0 is `offer` and 1 is `destroy` in all three interfaces.
            match op {
                0 => {
                    let mime = args.str(0).unwrap_or_default().to_string();
                    s.selection.mimes.entry(key).or_default().push(mime);
                }
                1 => source_gone(s, c, id),
                _ => {}
            }
        }
        Kind::DataDevice if op == wl_data_device::req::SET_SELECTION => set(s, Which::Clipboard, c, args.obj(0), Kind::DataSource),
        Kind::PrimaryDevice if op == zwp_primary_selection_device_v1::req::SET_SELECTION => {
            set(s, Which::Primary, c, args.obj(0), Kind::PrimarySource)
        }
        Kind::ControlDevice => match op {
            ext_data_control_device_v1::req::SET_SELECTION => set(s, Which::Clipboard, c, args.obj(0), Kind::ControlSource),
            ext_data_control_device_v1::req::SET_PRIMARY_SELECTION => set(s, Which::Primary, c, args.obj(0), Kind::ControlSource),
            _ => {}
        },
        Kind::DataOffer { generation } if op == wl_data_offer::req::RECEIVE => {
            receive(s, Which::Clipboard, generation, args.str(0).unwrap_or_default().to_string(), args.take_fd(1))
        }
        Kind::PrimaryOffer { generation } if op == zwp_primary_selection_offer_v1::req::RECEIVE => {
            receive(s, Which::Primary, generation, args.str(0).unwrap_or_default().to_string(), args.take_fd(1))
        }
        Kind::ControlOffer { generation, primary } if op == ext_data_control_offer_v1::req::RECEIVE => {
            let which = if primary { Which::Primary } else { Which::Clipboard };
            receive(s, which, generation, args.str(0).unwrap_or_default().to_string(), args.take_fd(1))
        }
        _ => {}
    }
}

fn new_source(s: &mut State, c: ClientId, id: u32, iface: &'static Interface, version: u32, kind: Kind) {
    if s.new_object(c, id, iface, version, kind) {
        s.selection.mimes.insert(SKey { client: c, id }, Vec::new());
    }
}

fn focused_client(s: &State) -> Option<ClientId> {
    s.focus.map(|f| f.client)
}

fn set(s: &mut State, which: Which, c: ClientId, source: u32, kind: Kind) {
    let new = (source != 0).then_some((c, source, kind));
    let mimes = new.and_then(|_| s.selection.mimes.get(&SKey { client: c, id: source }).cloned()).unwrap_or_default();
    let sel = s.selection.get(which);
    let old = sel.source;
    if old == new {
        return;
    }
    sel.source = new;
    sel.server = None;
    sel.captured = false;
    sel.x11 = false;
    sel.mimes = mimes;
    sel.generation += 1;
    let generation = sel.generation;
    if let Some((oc, oid, okind)) = old {
        let op = match okind {
            Kind::DataSource => wl_data_source::evt::CANCELLED,
            Kind::PrimarySource => zwp_primary_selection_source_v1::evt::CANCELLED,
            _ => ext_data_control_source_v1::evt::CANCELLED,
        };
        s.send(oc, Msg::new(oid, op));
    }
    if which == Which::Clipboard {
        if let Some((mime, (c, id, kind))) = clipboard::pick_mime(&s.selection.clipboard.mimes).zip(new) {
            match clipboard::pipe() {
                Ok((read, write)) => {
                    s.send(c, Msg::new(id, send_op(kind)).str(&mime).fd(write));
                    s.selection.captures.push((read, generation, mime));
                }
                Err(e) => log!("clipboard pipe: {e}"),
            }
        }
    }
    broadcast(s, which);
}

fn send_op(kind: Kind) -> u16 {
    match kind {
        Kind::DataSource => wl_data_source::evt::SEND,
        Kind::PrimarySource => zwp_primary_selection_source_v1::evt::SEND,
        _ => ext_data_control_source_v1::evt::SEND,
    }
}

/// A capture finished: the clip joins the history.
pub fn captured(s: &mut State, generation: u32, clip: Clip) {
    s.selection.history.push(clip);
    let sel = &mut s.selection.clipboard;
    if sel.generation == generation && (sel.source.is_some() || sel.x11) {
        sel.captured = true;
    }
}

/// Makes the compositor the clipboard owner, serving `clip` (picked from
/// the history, which moves it to the top).
pub fn set_server(s: &mut State, clip: Clip) {
    let sel = &mut s.selection.clipboard;
    let old = sel.source.take();
    sel.mimes = clipboard::offered(&clip);
    sel.server = Some(clip.clone());
    sel.captured = false;
    sel.x11 = false;
    sel.generation += 1;
    if let Some((oc, oid, okind)) = old {
        let op = match okind {
            Kind::DataSource => wl_data_source::evt::CANCELLED,
            _ => ext_data_control_source_v1::evt::CANCELLED,
        };
        s.send(oc, Msg::new(oid, op));
    }
    s.selection.history.push(clip);
    broadcast(s, Which::Clipboard);
}

fn cancel(s: &mut State, old: Option<(ClientId, u32, Kind)>) {
    if let Some((oc, oid, okind)) = old {
        let op = match okind {
            Kind::DataSource => wl_data_source::evt::CANCELLED,
            Kind::PrimarySource => zwp_primary_selection_source_v1::evt::CANCELLED,
            _ => ext_data_control_source_v1::evt::CANCELLED,
        };
        s.send(oc, Msg::new(oid, op));
    }
}

/// An X client took the selection: Wayland clients see these types, read
/// from it through the X window manager.
pub fn set_x11(s: &mut State, which: Which, mimes: Vec<String>) {
    let sel = s.selection.get(which);
    let old = sel.source.take();
    sel.server = None;
    sel.captured = false;
    sel.x11 = true;
    sel.mimes = mimes;
    sel.generation += 1;
    let generation = sel.generation;
    cancel(s, old);
    if which == Which::Clipboard {
        if let Some(mime) = clipboard::pick_mime(&s.selection.clipboard.mimes) {
            match clipboard::pipe() {
                Ok((read, write)) => {
                    super::xwm::selection::receive(s, which, &mime, write);
                    s.selection.captures.push((read, generation, mime));
                }
                Err(e) => log!("clipboard pipe: {e}"),
            }
        }
    }
    broadcast(s, which);
}

/// The X owner let the selection go (or Xwayland went away).
pub fn clear_x11(s: &mut State, which: Which) {
    let sel = s.selection.get(which);
    if !std::mem::take(&mut sel.x11) {
        return;
    }
    sel.mimes.clear();
    sel.generation += 1;
    if std::mem::take(&mut sel.captured) {
        if let Some(top) = s.selection.history.entries.first().cloned() {
            let sel = s.selection.get(which);
            sel.mimes = clipboard::offered(&top);
            sel.server = Some(top);
        }
    }
    broadcast(s, which);
}

/// The selection belongs to a Wayland client or to the compositor.
pub fn has_wayland(s: &mut State, which: Which) -> bool {
    let sel = s.selection.get(which);
    sel.source.is_some() || sel.server.is_some()
}

pub fn mimes(s: &mut State, which: Which) -> Vec<String> {
    s.selection.get(which).mimes.clone()
}

/// Has the Wayland owner (or the compositor) write the selection into `fd`.
pub fn request_data(s: &mut State, which: Which, mime: &str, fd: OwnedFd) {
    let sel = s.selection.get(which);
    if let Some(clip) = &sel.server {
        clipboard::serve(fd, clip.data.clone());
    } else if let Some((c, id, kind)) = sel.source {
        s.send(c, Msg::new(id, send_op(kind)).str(mime).fd(fd));
    }
}

fn broadcast(s: &mut State, which: Which) {
    super::xwm::selection::wayland_changed(s, which);
    if let Some(c) = focused_client(s) {
        offer_to_client(s, c, which);
    }
    let clients: Vec<ClientId> = s.clients.keys().copied().collect();
    for c in clients {
        for (dev, version) in s.objects_of(c, Kind::ControlDevice) {
            offer_to_control(s, c, dev, version, which);
        }
    }
}

/// Sends the current selection to the regular devices of a client (the
/// focused one).
pub fn offer_to_client(s: &mut State, c: ClientId, which: Which) {
    let (device_kind, offer_iface, data_offer_op, offer_op, selection_op) = match which {
        Which::Clipboard => (
            Kind::DataDevice,
            &wl_data_offer::INTERFACE,
            wl_data_device::evt::DATA_OFFER,
            wl_data_offer::evt::OFFER,
            wl_data_device::evt::SELECTION,
        ),
        Which::Primary => (
            Kind::PrimaryDevice,
            &zwp_primary_selection_offer_v1::INTERFACE,
            zwp_primary_selection_device_v1::evt::DATA_OFFER,
            zwp_primary_selection_offer_v1::evt::OFFER,
            zwp_primary_selection_device_v1::evt::SELECTION,
        ),
    };
    let sel = s.selection.get(which);
    let (has, mimes, generation) = (sel.source.is_some() || sel.server.is_some() || sel.x11, sel.mimes.clone(), sel.generation);
    for (dev, version) in s.objects_of(c, device_kind) {
        let mut offer = 0;
        if has {
            let kind = match which {
                Which::Clipboard => Kind::DataOffer { generation },
                Which::Primary => Kind::PrimaryOffer { generation },
            };
            let Some(id) = s.new_server_object(c, offer_iface, version, kind) else { return };
            offer = id;
            s.send(c, Msg::new(dev, data_offer_op).obj(offer));
            for m in &mimes {
                s.send(c, Msg::new(offer, offer_op).str(m));
            }
        }
        s.send(c, Msg::new(dev, selection_op).obj(offer));
    }
}

fn offer_to_control(s: &mut State, c: ClientId, dev: u32, version: u32, which: Which) {
    let sel = s.selection.get(which);
    let (has, mimes, generation) = (sel.source.is_some() || sel.server.is_some() || sel.x11, sel.mimes.clone(), sel.generation);
    let mut offer = 0;
    if has {
        let kind = Kind::ControlOffer { generation, primary: which == Which::Primary };
        let Some(id) = s.new_server_object(c, &ext_data_control_offer_v1::INTERFACE, version, kind) else { return };
        offer = id;
        s.send(c, Msg::new(dev, ext_data_control_device_v1::evt::DATA_OFFER).obj(offer));
        for m in &mimes {
            s.send(c, Msg::new(offer, ext_data_control_offer_v1::evt::OFFER).str(m));
        }
    }
    let op = match which {
        Which::Clipboard => ext_data_control_device_v1::evt::SELECTION,
        Which::Primary => ext_data_control_device_v1::evt::PRIMARY_SELECTION,
    };
    s.send(c, Msg::new(dev, op).obj(offer));
}

fn receive(s: &mut State, which: Which, generation: u32, mime: String, fd: OwnedFd) {
    let sel = s.selection.get(which);
    if sel.generation != generation {
        return;
    }
    if sel.x11 {
        super::xwm::selection::receive(s, which, &mime, fd);
        return;
    }
    request_data(s, which, &mime, fd);
}

fn source_gone(s: &mut State, c: ClientId, id: u32) {
    s.selection.mimes.remove(&SKey { client: c, id });
    for which in [Which::Clipboard, Which::Primary] {
        let sel = s.selection.get(which);
        if sel.source.is_some_and(|(sc, sid, _)| sc == c && sid == id) {
            sel.source = None;
            sel.mimes.clear();
            sel.generation += 1;
            // The clipboard outlives the program it was copied from.
            if std::mem::take(&mut sel.captured) {
                if let Some(top) = s.selection.history.entries.first().cloned() {
                    let sel = s.selection.get(which);
                    sel.mimes = clipboard::offered(&top);
                    sel.server = Some(top);
                }
            }
            broadcast(s, which);
        }
    }
}

pub fn client_gone(s: &mut State, c: ClientId) {
    let ids: Vec<u32> = s.selection.mimes.keys().filter(|k| k.client == c).map(|k| k.id).collect();
    for id in ids {
        source_gone(s, c, id);
    }
}

/// Called when the keyboard focus moves to another client.
pub fn focus_changed(s: &mut State, c: ClientId) {
    offer_to_client(s, c, Which::Clipboard);
    offer_to_client(s, c, Which::Primary);
}

state::persist_struct!(Selection { source, server, captured, x11, mimes, generation });

/// Captures in flight are dropped: their owner still has the selection.
impl state::Persist for Selections {
    fn save(&self, e: &mut state::Enc) {
        self.clipboard.save(e);
        self.primary.save(e);
        self.mimes.save(e);
        self.history.save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        Ok(Selections { clipboard: Persist::load(d)?, primary: Persist::load(d)?, mimes: Persist::load(d)?, history: Persist::load(d)?, captures: Vec::new() })
    }
}
