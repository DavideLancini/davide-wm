//! linux-dmabuf: GPU buffers from clients, imported at render time.

use std::os::fd::OwnedFd;

use render::Plane;
use wire::*;

use super::*;

/// Formats and modifiers for clients of version 3 and older.
pub fn send_formats(s: &mut State, c: ClientId, id: u32, version: u32) {
    if version >= 4 {
        return;
    }
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

/// The table of (format, modifier) pairs that feedback refers to by index.
fn format_table(formats: &[(u32, u64)]) -> std::io::Result<OwnedFd> {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::memfd_create(c"dmabuf-formats".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let bytes: Vec<u8> = formats.iter().flat_map(|(fmt, m)| [fmt.to_ne_bytes().as_slice(), &[0; 4], &m.to_ne_bytes()].concat()).collect();
    f.write_all(&bytes)?;
    Ok(OwnedFd::from(f))
}

/// Feedback (version 4): one tranche with every format, on our render node.
fn send_feedback(s: &mut State, c: ClientId, id: u32) {
    let formats = s.dmabuf_formats.clone();
    let table = match format_table(&formats) {
        Ok(t) => t,
        Err(e) => {
            log!("dmabuf format table: {e}");
            return;
        }
    };
    let device = s.dmabuf_device.to_ne_bytes();
    let indices: Vec<u8> = (0..formats.len() as u16).flat_map(|i| i.to_ne_bytes()).collect();
    use zwp_linux_dmabuf_feedback_v1::evt;
    s.send(c, Msg::new(id, evt::FORMAT_TABLE).fd(table).uint(formats.len() as u32 * 16));
    s.send(c, Msg::new(id, evt::MAIN_DEVICE).array(&device));
    s.send(c, Msg::new(id, evt::TRANCHE_TARGET_DEVICE).array(&device));
    s.send(c, Msg::new(id, evt::TRANCHE_FORMATS).array(&indices));
    s.send(c, Msg::new(id, evt::TRANCHE_FLAGS).uint(0));
    s.send(c, Msg::new(id, evt::TRANCHE_DONE));
    s.send(c, Msg::new(id, evt::DONE));
}

pub fn request(s: &mut State, c: ClientId, id: u32, kind: Kind, version: u32, op: u16, mut args: Args) {
    let key = SKey { client: c, id };
    match (kind, op) {
        (Kind::Dmabuf, zwp_linux_dmabuf_v1::req::GET_DEFAULT_FEEDBACK | zwp_linux_dmabuf_v1::req::GET_SURFACE_FEEDBACK) => {
            let fid = args.obj(0);
            if s.new_object(c, fid, &zwp_linux_dmabuf_feedback_v1::INTERFACE, version, Kind::DmabufFeedback) {
                send_feedback(s, c, fid);
            }
        }
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
