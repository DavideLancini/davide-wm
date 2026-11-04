//! Atomic KMS on one DRM device: connectors, modes, scanout buffers, page flips.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use render::{Renderer, Target};
use sys::drm::{self, ioctl, ModeInfo};

pub struct Connector {
    pub id: u32,
    pub name: String,
    pub connected: bool,
    pub modes: Vec<ModeInfo>,
    pub encoders: Vec<u32>,
    pub mm: (u32, u32),
}

/// The output being driven.
pub struct Output {
    pub connector: u32,
    pub crtc: u32,
    pub plane: u32,
    pub mode: ModeInfo,
    pub name: String,
}

struct ScanBuffer {
    target: Target,
    fb: u32,
}

pub struct Kms {
    fd: OwnedFd,
    props: HashMap<(u32, &'static str), u32>,
    crtcs: Vec<u32>,
    planes: Vec<(u32, u32, u64)>,
    connectors_all: Vec<u32>,
    pub output: Option<Output>,
    buffers: Vec<ScanBuffer>,
    front: Option<usize>,
    pending: Option<usize>,
    modeset: bool,
    mode_blob: Option<u32>,
}

fn vec_ptr<T>(v: &mut [T]) -> u64 {
    v.as_mut_ptr() as u64
}

fn prop_names() -> [&'static str; 14] {
    ["CRTC_ID", "MODE_ID", "ACTIVE", "FB_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "type", "DPMS"]
}

impl Kms {
    pub fn new(fd: OwnedFd) -> io::Result<Kms> {
        let raw = fd.as_raw_fd();
        for cap in [drm::CLIENT_CAP_UNIVERSAL_PLANES, drm::CLIENT_CAP_ATOMIC] {
            let mut c = drm::SetClientCap { capability: cap, value: 1 };
            ioctl(raw, drm::IOCTL_SET_CLIENT_CAP, &mut c).map_err(|e| io::Error::other(format!("DRM client cap {cap}: {e}")))?;
        }
        let mut res = drm::CardRes::default();
        ioctl(raw, drm::IOCTL_MODE_GETRESOURCES, &mut res)?;
        let mut crtcs = vec![0u32; res.count_crtcs as usize];
        let mut connectors = vec![0u32; res.count_connectors as usize];
        let mut res2 = drm::CardRes {
            crtc_id_ptr: vec_ptr(&mut crtcs),
            connector_id_ptr: vec_ptr(&mut connectors),
            count_crtcs: res.count_crtcs,
            count_connectors: res.count_connectors,
            ..Default::default()
        };
        ioctl(raw, drm::IOCTL_MODE_GETRESOURCES, &mut res2)?;

        let mut pres = drm::GetPlaneRes::default();
        ioctl(raw, drm::IOCTL_MODE_GETPLANERESOURCES, &mut pres)?;
        let mut plane_ids = vec![0u32; pres.count_planes as usize];
        let mut pres2 = drm::GetPlaneRes { plane_id_ptr: vec_ptr(&mut plane_ids), count_planes: pres.count_planes };
        ioctl(raw, drm::IOCTL_MODE_GETPLANERESOURCES, &mut pres2)?;

        let mut kms = Kms {
            fd,
            props: HashMap::new(),
            crtcs: crtcs.clone(),
            planes: Vec::new(),
            connectors_all: connectors.clone(),
            output: None,
            buffers: Vec::new(),
            front: None,
            pending: None,
            modeset: true,
            mode_blob: None,
        };
        for c in &crtcs {
            kms.load_props(*c, drm::OBJECT_CRTC)?;
        }
        for c in &connectors {
            kms.load_props(*c, drm::OBJECT_CONNECTOR)?;
        }
        for p in plane_ids {
            let values = kms.load_props(p, drm::OBJECT_PLANE)?;
            let mut gp = drm::GetPlane { plane_id: p, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETPLANE, &mut gp)?;
            let kind = values.get("type").copied().unwrap_or(0);
            kms.planes.push((p, gp.possible_crtcs, kind));
        }
        Ok(kms)
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Records the ids of the properties we use; returns their current values.
    fn load_props(&mut self, obj: u32, kind: u32) -> io::Result<HashMap<&'static str, u64>> {
        let raw = self.fd.as_raw_fd();
        let mut gp = drm::ObjGetProperties { obj_id: obj, obj_type: kind, ..Default::default() };
        ioctl(raw, drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut gp)?;
        let mut ids = vec![0u32; gp.count_props as usize];
        let mut values = vec![0u64; gp.count_props as usize];
        let mut gp2 = drm::ObjGetProperties {
            props_ptr: vec_ptr(&mut ids),
            prop_values_ptr: vec_ptr(&mut values),
            count_props: gp.count_props,
            obj_id: obj,
            obj_type: kind,
        };
        ioctl(raw, drm::IOCTL_MODE_OBJ_GETPROPERTIES, &mut gp2)?;
        let mut out = HashMap::new();
        for (id, value) in ids.iter().zip(values) {
            let mut p = drm::GetProperty { prop_id: *id, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETPROPERTY, &mut p)?;
            let name = std::str::from_utf8(&p.name).unwrap_or("").trim_end_matches('\0');
            if let Some(n) = prop_names().into_iter().find(|n| *n == name) {
                self.props.insert((obj, n), *id);
                out.insert(n, value);
            }
        }
        Ok(out)
    }

    pub fn connectors(&self) -> io::Result<Vec<Connector>> {
        let raw = self.fd.as_raw_fd();
        let mut out = Vec::new();
        for id in &self.connectors_all {
            let mut c = drm::GetConnector { connector_id: *id, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETCONNECTOR, &mut c)?;
            let mut modes = vec![ModeInfo::default(); c.count_modes as usize];
            let mut encoders = vec![0u32; c.count_encoders as usize];
            let mut c2 = drm::GetConnector {
                connector_id: *id,
                modes_ptr: vec_ptr(&mut modes),
                encoders_ptr: vec_ptr(&mut encoders),
                count_modes: c.count_modes,
                count_encoders: c.count_encoders,
                ..Default::default()
            };
            ioctl(raw, drm::IOCTL_MODE_GETCONNECTOR, &mut c2)?;
            // The mode count can change between the two calls.
            modes.truncate(c2.count_modes.min(c.count_modes) as usize);
            out.push(Connector {
                id: *id,
                name: format!("{}-{}", drm::connector_type_name(c2.connector_type), c2.connector_type_id),
                connected: c2.connection == drm::MODE_CONNECTED,
                modes,
                encoders,
                mm: (c2.mm_width, c2.mm_height),
            });
        }
        Ok(out)
    }

    /// Picks the connector, mode, CRTC and primary plane to drive.
    pub fn choose(&self) -> io::Result<Option<Output>> {
        let conns: Vec<Connector> = self.connectors()?.into_iter().filter(|c| c.connected && !c.modes.is_empty()).collect();
        let pick = config::OUTPUT_PRIORITY.iter().find_map(|n| conns.iter().find(|c| c.name == *n)).or(conns.first());
        let Some(conn) = pick else { return Ok(None) };
        let wanted = config::OUTPUT_MODES.iter().find(|(n, ..)| *n == conn.name);
        let mode = wanted
            .and_then(|(_, w, h, hz)| {
                conn.modes.iter().copied().filter(|m| m.hdisplay as i32 == *w && m.vdisplay as i32 == *h).min_by_key(|m| m.refresh_mhz().abs_diff(hz * 1000))
            })
            .or(conn.modes.iter().copied().find(|m| m.kind & drm::MODE_TYPE_PREFERRED != 0))
            .unwrap_or(conn.modes[0]);
        let raw = self.fd.as_raw_fd();
        let mut possible = 0u32;
        for e in &conn.encoders {
            let mut enc = drm::GetEncoder { encoder_id: *e, ..Default::default() };
            if ioctl(raw, drm::IOCTL_MODE_GETENCODER, &mut enc).is_ok() {
                possible |= enc.possible_crtcs;
            }
        }
        for (i, crtc) in self.crtcs.iter().enumerate() {
            if possible & (1 << i) == 0 {
                continue;
            }
            let plane = self.planes.iter().find(|(_, pc, kind)| pc & (1 << i) != 0 && *kind == drm::PLANE_TYPE_PRIMARY);
            if let Some((plane, ..)) = plane {
                return Ok(Some(Output { connector: conn.id, crtc: *crtc, plane: *plane, mode, name: conn.name.clone() }));
            }
        }
        Err(io::Error::other(format!("no CRTC/plane for {}", conn.name)))
    }

    fn prop(&self, obj: u32, name: &'static str) -> io::Result<u32> {
        self.props.get(&(obj, name)).copied().ok_or_else(|| io::Error::other(format!("object {obj} has no {name} property")))
    }

    fn commit(&self, objs: Vec<(u32, Vec<(&'static str, u64)>)>, flags: u32) -> io::Result<()> {
        let mut obj_ids = Vec::new();
        let mut counts = Vec::new();
        let mut props = Vec::new();
        let mut values = Vec::new();
        for (obj, list) in objs {
            obj_ids.push(obj);
            counts.push(list.len() as u32);
            for (name, v) in list {
                props.push(self.prop(obj, name)?);
                values.push(v);
            }
        }
        let mut a = drm::Atomic {
            flags,
            count_objs: obj_ids.len() as u32,
            objs_ptr: vec_ptr(&mut obj_ids),
            count_props_ptr: vec_ptr(&mut counts),
            props_ptr: vec_ptr(&mut props),
            prop_values_ptr: vec_ptr(&mut values),
            ..Default::default()
        };
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_ATOMIC, &mut a)
    }

    fn create_blob<T>(&self, data: &T) -> io::Result<u32> {
        let mut b = drm::CreateBlob { data: data as *const T as u64, length: std::mem::size_of::<T>() as u32, blob_id: 0 };
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_CREATEPROPBLOB, &mut b)?;
        Ok(b.blob_id)
    }

    fn add_fb(&self, t: &Target) -> io::Result<u32> {
        let mut cmd = drm::FbCmd2 { width: t.width as u32, height: t.height as u32, pixel_format: t.format, ..Default::default() };
        cmd.handles[0] = t.handle();
        cmd.pitches[0] = t.stride;
        cmd.offsets[0] = t.offset;
        if t.modifier != render::MOD_INVALID {
            cmd.flags = drm::MODE_FB_MODIFIERS;
            cmd.modifier[0] = t.modifier;
        }
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_ADDFB2, &mut cmd)?;
        Ok(cmd.fb_id)
    }

    fn drop_buffers(&mut self, r: &mut Renderer) {
        for b in self.buffers.drain(..) {
            let mut fb = b.fb;
            let _ = ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_RMFB, &mut fb);
            r.destroy_target(b.target);
        }
        self.front = None;
        self.pending = None;
    }

    /// Switches to `output` (or re-applies the current one after a VT switch).
    pub fn set_output(&mut self, output: Output, r: &mut Renderer) -> io::Result<()> {
        let size = (output.mode.hdisplay as i32, output.mode.vdisplay as i32);
        if self.buffers.first().is_none_or(|b| (b.target.width, b.target.height) != size) {
            self.drop_buffers(r);
            for _ in 0..2 {
                let target = r.create_target(size.0, size.1, sys::gbm::BO_USE_SCANOUT).map_err(io::Error::other)?;
                let fb = self.add_fb(&target)?;
                self.buffers.push(ScanBuffer { target, fb });
            }
        }
        log!(
            "output {}: {}x{}@{:.2}",
            output.name,
            output.mode.hdisplay,
            output.mode.vdisplay,
            output.mode.refresh_mhz() as f64 / 1000.0
        );
        self.output = Some(output);
        self.modeset = true;
        self.pending = None;
        self.front = None;
        Ok(())
    }

    pub fn request_modeset(&mut self) {
        self.modeset = true;
        self.pending = None;
    }

    pub fn acquire(&self) -> Option<usize> {
        if self.pending.is_some() || self.output.is_none() {
            return None;
        }
        (0..self.buffers.len()).find(|i| Some(*i) != self.front)
    }

    pub fn target(&self, idx: usize) -> &Target {
        &self.buffers[idx].target
    }

    pub fn present(&mut self, idx: usize) -> io::Result<()> {
        if self.modeset {
            self.disable_others()?;
        }
        let Some(o) = &self.output else { return Ok(()) };
        let fb = self.buffers[idx].fb as u64;
        let (w, h) = (o.mode.hdisplay as u64, o.mode.vdisplay as u64);
        let mut objs = vec![(
            o.plane,
            vec![
                ("FB_ID", fb),
                ("CRTC_ID", o.crtc as u64),
                ("SRC_X", 0),
                ("SRC_Y", 0),
                ("SRC_W", w << 16),
                ("SRC_H", h << 16),
                ("CRTC_X", 0),
                ("CRTC_Y", 0),
                ("CRTC_W", w),
                ("CRTC_H", h),
            ],
        )];
        let mut flags = drm::MODE_PAGE_FLIP_EVENT;
        let modeset = self.modeset;
        if modeset {
            let blob = self.create_blob(&o.mode)?;
            objs.push((o.connector, vec![("CRTC_ID", o.crtc as u64)]));
            objs.push((o.crtc, vec![("MODE_ID", blob as u64), ("ACTIVE", 1)]));
            flags |= drm::MODE_ATOMIC_ALLOW_MODESET;
            let res = self.commit(objs, flags);
            if let Some(old) = self.mode_blob.replace(blob) {
                let mut b = drm::DestroyBlob { blob_id: old };
                let _ = ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_DESTROYPROPBLOB, &mut b);
            }
            // A failed modeset leaves nothing on screen: give up instead of
            // running blind.
            res.map_err(|e| io::Error::other(format!("modeset failed: {e}")))?;
        } else {
            flags |= drm::MODE_ATOMIC_NONBLOCK;
            self.commit(objs, flags)?;
        }
        self.modeset = false;
        self.pending = Some(idx);
        Ok(())
    }

    /// Turns off what the previous DRM master left on, in its own commit:
    /// a commit asking for a page-flip event must not contain disabled CRTCs.
    fn disable_others(&mut self) -> io::Result<()> {
        let Some(o) = &self.output else { return Ok(()) };
        let (connector, crtc, plane) = (o.connector, o.crtc, o.plane);
        let mut objs = Vec::new();
        for c in self.connectors_all.clone() {
            if c != connector && self.current(c, drm::OBJECT_CONNECTOR, "CRTC_ID")? != 0 {
                objs.push((c, vec![("CRTC_ID", 0)]));
            }
        }
        for (p, ..) in self.planes.clone() {
            if p != plane && (self.current(p, drm::OBJECT_PLANE, "FB_ID")? != 0 || self.current(p, drm::OBJECT_PLANE, "CRTC_ID")? != 0) {
                objs.push((p, vec![("FB_ID", 0), ("CRTC_ID", 0)]));
            }
        }
        for c in self.crtcs.clone() {
            if c != crtc && self.current(c, drm::OBJECT_CRTC, "ACTIVE")? != 0 {
                objs.push((c, vec![("ACTIVE", 0), ("MODE_ID", 0)]));
            }
        }
        if objs.is_empty() {
            return Ok(());
        }
        self.commit(objs, drm::MODE_ATOMIC_ALLOW_MODESET).map_err(|e| io::Error::other(format!("disabling unused outputs failed: {e}")))
    }

    fn current(&mut self, obj: u32, kind: u32, name: &str) -> io::Result<u64> {
        Ok(self.load_props(obj, kind)?.get(name).copied().unwrap_or(0))
    }

    /// Reads DRM events; returns true when a page flip completed.
    pub fn dispatch(&mut self) -> bool {
        let mut buf = [0u8; 1024];
        let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        let mut flipped = false;
        let mut off = 0usize;
        while n > 0 && off + 8 <= n as usize {
            let kind = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
            let len = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
            if kind == drm::EVENT_FLIP_COMPLETE {
                flipped = true;
            }
            off += len.max(8);
        }
        if flipped {
            if let Some(p) = self.pending.take() {
                self.front = Some(p);
            }
        }
        flipped
    }

    /// Prints connectors and modes (works without DRM master).
    pub fn describe(&self) -> io::Result<String> {
        let mut out = String::new();
        for c in self.connectors()? {
            out += &format!("{} ({}, {}x{} mm)\n", c.name, if c.connected { "connected" } else { "disconnected" }, c.mm.0, c.mm.1);
            for m in &c.modes {
                let pref = if m.kind & drm::MODE_TYPE_PREFERRED != 0 { " preferred" } else { "" };
                out += &format!("  {}x{}@{:.3}{pref}\n", m.hdisplay, m.vdisplay, m.refresh_mhz() as f64 / 1000.0);
            }
        }
        Ok(out)
    }
}
