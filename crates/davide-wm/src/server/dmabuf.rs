//! linux-dmabuf: GPU buffers from clients, imported at render time.

use render::Plane;
use wire::*;

use super::*;

pub fn send_formats(s: &mut State, c: ClientId, id: u32) {
    let formats = s.dmabuf_formats.clone();
    let mut sent = Vec::new();
    for (f, m) in formats {
        if !sent.contains(&f) {
            sent.push(f);
            s.send(c, Msg::new(id, zwp_linux_dmabuf_v1::evt::FORMAT).uint(f));
        }
        s.send(c, Msg::new(id, zwp_linux_dmabuf_v1::evt::MODIFIER).uint(f).uint((m >> 32) as u32).uint(m as u32));
    }
}

pub fn request(s: &mut State, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, mut args: Args) {
    let key = SKey { client: c, id };
    match (kind, op) {
        (Kind::Dmabuf, zwp_linux_dmabuf_v1::req::CREATE_PARAMS) => {
            let pid = args.obj(0);
            if s.new_object(c, pid, &zwp_linux_buffer_params_v1::INTERFACE, version, Kind::DmabufParams) {
                s.dmabuf_params.insert(SKey { client: c, id: pid }, DmabufParams::default());
            }
        }
        (Kind::DmabufParams, zwp_linux_buffer_params_v1::req::DESTROY) => {
            s.dmabuf_params.remove(&key);
        }
        (Kind::DmabufParams, zwp_linux_buffer_params_v1::req::ADD) => {
            let fd = args.take_fd(0);
            let (idx, offset, stride) = (args.uint(1), args.uint(2), args.uint(3));
            let modifier = ((args.uint(4) as u64) << 32) | args.uint(5) as u64;
            let Some(p) = s.dmabuf_params.get_mut(&key) else { return };
            if idx > 3 || p.planes.iter().any(|(i, _)| *i == idx) {
                s.error(c, id, zwp_linux_buffer_params_v1::error::PLANE_SET, "plane already set");
                return;
            }
            p.modifier = modifier;
            p.planes.push((idx, Plane { fd, offset, stride }));
        }
        (Kind::DmabufParams, zwp_linux_buffer_params_v1::req::CREATE | zwp_linux_buffer_params_v1::req::CREATE_IMMED) => {
            let immed = op == zwp_linux_buffer_params_v1::req::CREATE_IMMED;
            let base = if immed { 1 } else { 0 };
            let (width, height, format) = (args.int(base), args.int(base + 1), args.uint(base + 2));
            let Some(p) = s.dmabuf_params.get_mut(&key) else { return };
            if p.used {
                s.error(c, id, zwp_linux_buffer_params_v1::error::ALREADY_USED, "params already used");
                return;
            }
            p.used = true;
            let mut planes = std::mem::take(&mut p.planes);
            planes.sort_by_key(|(i, _)| *i);
            let contiguous = planes.iter().enumerate().all(|(n, (i, _))| n as u32 == *i);
            let modifier = p.modifier;
            if planes.is_empty() || !contiguous || width <= 0 || height <= 0 {
                s.error(c, id, zwp_linux_buffer_params_v1::error::INCOMPLETE, "incomplete or invalid planes");
                return;
            }
            let bid = if immed {
                let bid = args.obj(0);
                if !s.new_object(c, bid, &wl_buffer::INTERFACE, 1, Kind::Buffer) {
                    return;
                }
                bid
            } else {
                let Some(bid) = s.new_server_object(c, &wl_buffer::INTERFACE, 1, Kind::Buffer) else { return };
                s.send(c, Msg::new(id, zwp_linux_buffer_params_v1::evt::CREATED).obj(bid));
                bid
            };
            let planes = planes.into_iter().map(|(_, p)| p).collect();
            s.buffers.insert(
                SKey { client: c, id: bid },
                Buffer::Dmabuf { width, height, format, modifier, planes, tex: None, failed: false },
            );
        }
        _ => {}
    }
}
