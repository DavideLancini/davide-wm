//! Small protocols: xdg-activation, xdg-system-bell, idle-inhibit.

use wire::*;

use super::*;

pub fn request(s: &mut State, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, args: Args) {
    match (kind, op) {
        (Kind::Activation, xdg_activation_v1::req::GET_ACTIVATION_TOKEN) => {
            s.new_object(c, args.obj(0), &xdg_activation_token_v1::INTERFACE, version, Kind::ActivationToken);
        }
        // Like Hyprland's default: activation requests mark the window urgent
        // instead of stealing the focus.
        (Kind::Activation, xdg_activation_v1::req::ACTIVATE) => urgent_surface(s, c, args.obj(1)),
        (Kind::ActivationToken, xdg_activation_token_v1::req::COMMIT) => {
            let token = format!("davide-wm-{}-{}", c, s.next_serial());
            s.send(c, Msg::new(id, xdg_activation_token_v1::evt::DONE).str(&token));
        }
        (Kind::Bell, xdg_system_bell_v1::req::RING) => urgent_surface(s, c, args.obj(0)),
        (Kind::IdleInhibitManager, zwp_idle_inhibit_manager_v1::req::CREATE_INHIBITOR) => {
            let (inhibitor, surface) = (args.obj(0), args.obj(1));
            if s.new_object(c, inhibitor, &zwp_idle_inhibitor_v1::INTERFACE, version, Kind::IdleInhibitor { surface }) {
                s.idle_inhibitors.insert(SKey { client: c, id: inhibitor }, SKey { client: c, id: surface });
            }
        }
        (Kind::IdleInhibitor { .. }, zwp_idle_inhibitor_v1::req::DESTROY) => {
            s.idle_inhibitors.remove(&SKey { client: c, id });
        }
        _ => {}
    }
}

fn urgent_surface(s: &mut State, c: ClientId, surface: u32) {
    if surface == 0 {
        return;
    }
    if let Some(top) = seat::toplevel_of(s, SKey { client: c, id: surface }) {
        workspaces::mark_urgent(s, top);
    }
}
