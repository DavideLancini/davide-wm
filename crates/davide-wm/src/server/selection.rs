//! Clipboard and primary selection: `wl_data_device`, primary-selection and
//! ext-data-control. Drag and drop is not implemented.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use wire::*;

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
    sel.mimes = mimes;
    sel.generation += 1;
    if let Some((oc, oid, okind)) = old {
        let op = match okind {
            Kind::DataSource => wl_data_source::evt::CANCELLED,
            Kind::PrimarySource => zwp_primary_selection_source_v1::evt::CANCELLED,
            _ => ext_data_control_source_v1::evt::CANCELLED,
        };
        s.send(oc, Msg::new(oid, op));
    }
    broadcast(s, which);
}

fn broadcast(s: &mut State, which: Which) {
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
    let (has, mimes, generation) = (sel.source.is_some(), sel.mimes.clone(), sel.generation);
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
    let (has, mimes, generation) = (sel.source.is_some(), sel.mimes.clone(), sel.generation);
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
    let Some((c, id, kind)) = sel.source else { return };
    let op = match kind {
        Kind::DataSource => wl_data_source::evt::SEND,
        Kind::PrimarySource => zwp_primary_selection_source_v1::evt::SEND,
        _ => ext_data_control_source_v1::evt::SEND,
    };
    s.send(c, Msg::new(id, op).str(&mime).fd(fd));
}

fn source_gone(s: &mut State, c: ClientId, id: u32) {
    s.selection.mimes.remove(&SKey { client: c, id });
    for which in [Which::Clipboard, Which::Primary] {
        let sel = s.selection.get(which);
        if sel.source.is_some_and(|(sc, sid, _)| sc == c && sid == id) {
            sel.source = None;
            sel.mimes.clear();
            sel.generation += 1;
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
