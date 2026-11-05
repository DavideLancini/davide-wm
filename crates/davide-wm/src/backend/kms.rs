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
    pub mm: (u32, u32),
}

/// An output being driven.
#[derive(Clone)]
pub struct Output {
    pub connector: u32,
    pub crtc: u32,
    pub plane: u32,
    pub mode: ModeInfo,
    pub name: String,
    pub model: String,
}

struct ScanBuffer {
    target: Target,
    fb: u32,
    /// GEM handle of the imported buffer on the KMS fd.
    handle: u32,
}

/// The cursor plane of a head and its dumb buffer, mapped for writing.
struct Cursor {
    plane: u32,
    fb: u32,
    handle: u32,
    map: *mut u8,
    len: usize,
    pitch: u32,
    size: (u32, u32),
}

struct Head {
    output: Output,
    cursor: Option<Cursor>,
    /// Where the cursor should be (None: hidden) and what the CRTC has.
    cursor_want: Option<(i32, i32)>,
    cursor_sent: Option<Option<(i32, i32)>>,
    buffers: Vec<ScanBuffer>,
    front: Option<usize>,
    pending: Option<usize>,
    modeset: bool,
    mode_blob: Option<u32>,
    /// False while powered down (DPMS off): the CRTC is disabled.
    on: bool,
    /// The colour multipliers on the CRTC now, and their LUT blob.
    gamma_sent: Option<(f32, f32, f32)>,
    gamma_blob: Option<u32>,
}

pub struct Kms {
    fd: OwnedFd,
    props: HashMap<(u32, &'static str), u32>,
    crtcs: Vec<u32>,
    planes: Vec<(u32, u32, u64)>,
    connectors_all: Vec<u32>,
    /// The primary output first.
    heads: Vec<Head>,
    /// Framebuffers left by the process before a live upgrade, freed after
    /// the first page flip of this one.
    stale: Vec<(u32, u32)>,
    /// GAMMA_LUT_SIZE of each CRTC that has a gamma LUT.
    gamma_size: HashMap<u32, usize>,
    /// Wanted colour multipliers (night light); `None` is neutral.
    pub gamma: Option<(f32, f32, f32)>,
}

/// What survives a live upgrade.
pub struct Saved {
    pub fd: OwnedFd,
    pub heads: Vec<(Output, Option<u32>)>,
    pub stale: Vec<(u32, u32)>,
}

state::persist_struct!(Saved { fd, heads, stale });

impl state::Persist for Output {
    fn save(&self, e: &mut state::Enc) {
        let mode: [u8; std::mem::size_of::<ModeInfo>()] = unsafe { std::mem::transmute(self.mode) };
        (self.connector, self.crtc, self.plane, self.name.clone(), self.model.clone()).save(e);
        state::save_bytes(&mode, e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let (connector, crtc, plane, name, model) = state::Persist::load(d)?;
        let bytes = state::load_bytes(d)?;
        let bytes: [u8; std::mem::size_of::<ModeInfo>()] = bytes.try_into().map_err(|_| state::Error("bad mode".into()))?;
        let mode = unsafe { std::mem::transmute::<[u8; std::mem::size_of::<ModeInfo>()], ModeInfo>(bytes) };
        Ok(Output { connector, crtc, plane, mode, name, model })
    }
}

fn vec_ptr<T>(v: &mut [T]) -> u64 {
    v.as_mut_ptr() as u64
}

fn prop_names() -> [&'static str; 16] {
    ["CRTC_ID", "MODE_ID", "ACTIVE", "FB_ID", "SRC_X", "SRC_Y", "SRC_W", "SRC_H", "CRTC_X", "CRTC_Y", "CRTC_W", "CRTC_H", "type", "DPMS", "GAMMA_LUT", "GAMMA_LUT_SIZE"]
}

/// A colour ramp scaled by `mul` (red, green, blue), as `struct drm_color_lut`.
fn gamma_ramp(size: usize, mul: (f32, f32, f32)) -> Vec<u8> {
    let mut out = Vec::with_capacity(size * 8);
    for i in 0..size {
        let v = i as f32 / (size.max(2) - 1) as f32 * 65535.0;
        for m in [mul.0, mul.1, mul.2] {
            out.extend_from_slice(&((v * m.clamp(0.0, 1.0)) as u16).to_ne_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

fn same_mode(a: &ModeInfo, b: &ModeInfo) -> bool {
    (a.hdisplay, a.vdisplay, a.clock, a.htotal, a.vtotal, a.flags) == (b.hdisplay, b.vdisplay, b.clock, b.htotal, b.vtotal, b.flags)
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
            heads: Vec::new(),
            stale: Vec::new(),
            gamma_size: HashMap::new(),
            gamma: None,
        };
        for c in &crtcs {
            let values = kms.load_props(*c, drm::OBJECT_CRTC)?;
            if let (Some(size), true) = (values.get("GAMMA_LUT_SIZE"), kms.props.contains_key(&(*c, "GAMMA_LUT"))) {
                kms.gamma_size.insert(*c, *size as usize);
            }
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
            let mut c2 = drm::GetConnector { connector_id: *id, modes_ptr: vec_ptr(&mut modes), count_modes: c.count_modes, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETCONNECTOR, &mut c2)?;
            // The mode count can change between the two calls.
            modes.truncate(c2.count_modes.min(c.count_modes) as usize);
            out.push(Connector {
                id: *id,
                name: format!("{}-{}", drm::connector_type_name(c2.connector_type), c2.connector_type_id),
                connected: c2.connection == drm::MODE_CONNECTED,
                modes,
                mm: (c2.mm_width, c2.mm_height),
            });
        }
        Ok(out)
    }

    /// Assigns a CRTC and a primary plane to each (connector, mode), keeping
    /// the CRTC a connector already has.
    pub fn assign(&self, wanted: Vec<(u32, String, String, ModeInfo)>) -> io::Result<Vec<Output>> {
        let raw = self.fd.as_raw_fd();
        let mut used: Vec<u32> = Vec::new();
        let mut out = Vec::new();
        let mut order: Vec<usize> = (0..wanted.len()).collect();
        // Connectors keeping their CRTC claim it first.
        order.sort_by_key(|i| !self.heads.iter().any(|h| h.output.connector == wanted[*i].0));
        let mut slots: Vec<Option<Output>> = vec![None; wanted.len()];
        for i in order {
            let (connector, name, model, mode) = wanted[i].clone();
            let mut gc = drm::GetConnector { connector_id: connector, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETCONNECTOR, &mut gc)?;
            let mut encoders = vec![0u32; gc.count_encoders as usize];
            let mut gc2 = drm::GetConnector { connector_id: connector, encoders_ptr: vec_ptr(&mut encoders), count_encoders: gc.count_encoders, ..Default::default() };
            ioctl(raw, drm::IOCTL_MODE_GETCONNECTOR, &mut gc2)?;
            let mut possible = 0u32;
            for e in &encoders {
                let mut enc = drm::GetEncoder { encoder_id: *e, ..Default::default() };
                if ioctl(raw, drm::IOCTL_MODE_GETENCODER, &mut enc).is_ok() {
                    possible |= enc.possible_crtcs;
                }
            }
            let current = self.heads.iter().find(|h| h.output.connector == connector).map(|h| h.output.crtc);
            let mut candidates: Vec<usize> = (0..self.crtcs.len()).filter(|i| possible & (1 << i) != 0 && !used.contains(&self.crtcs[*i])).collect();
            candidates.sort_by_key(|i| Some(self.crtcs[*i]) != current);
            let pick = candidates.into_iter().find_map(|i| {
                let plane = self.planes.iter().find(|(p, pc, kind)| pc & (1 << i) != 0 && *kind == drm::PLANE_TYPE_PRIMARY && !out.iter().any(|o: &Output| o.plane == *p));
                plane.map(|(p, ..)| (self.crtcs[i], *p))
            });
            let Some((crtc, plane)) = pick else {
                log!("no CRTC/plane left for {name}");
                continue;
            };
            used.push(crtc);
            let o = Output { connector, crtc, plane, mode, name, model };
            out.push(o.clone());
            slots[i] = Some(o);
        }
        Ok(slots.into_iter().flatten().collect())
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

    /// A free cursor plane for `crtc` with a mapped ARGB buffer.
    fn cursor_for(&self, crtc: u32) -> io::Result<Option<Cursor>> {
        let raw = self.fd.as_raw_fd();
        let Some(index) = self.crtcs.iter().position(|c| *c == crtc) else { return Ok(None) };
        let taken: Vec<u32> = self.heads.iter().filter_map(|h| h.cursor.as_ref().map(|c| c.plane)).collect();
        let Some(plane) = self.planes.iter().find(|(p, pc, kind)| pc & (1 << index) != 0 && *kind == drm::PLANE_TYPE_CURSOR && !taken.contains(p)).map(|(p, ..)| *p) else {
            return Ok(None);
        };
        let cap = |c| {
            let mut g = drm::GetCap { capability: c, value: 0 };
            ioctl(raw, drm::IOCTL_GET_CAP, &mut g).map(|_| g.value as u32).unwrap_or(64).max(1)
        };
        let size = (cap(drm::CAP_CURSOR_WIDTH), cap(drm::CAP_CURSOR_HEIGHT));
        let mut dumb = drm::CreateDumb { width: size.0, height: size.1, bpp: 32, ..Default::default() };
        ioctl(raw, drm::IOCTL_MODE_CREATE_DUMB, &mut dumb)?;
        let mut cmd = drm::FbCmd2 { width: size.0, height: size.1, pixel_format: drm::FORMAT_ARGB8888, ..Default::default() };
        cmd.handles[0] = dumb.handle;
        cmd.pitches[0] = dumb.pitch;
        if let Err(e) = ioctl(raw, drm::IOCTL_MODE_ADDFB2, &mut cmd) {
            self.free_fb(0, dumb.handle);
            return Err(e);
        }
        let mut map = drm::MapDumb { handle: dumb.handle, ..Default::default() };
        ioctl(raw, drm::IOCTL_MODE_MAP_DUMB, &mut map)?;
        let len = dumb.size as usize;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, raw, map.offset as libc::off_t) };
        if ptr == libc::MAP_FAILED {
            self.free_fb(cmd.fb_id, dumb.handle);
            return Err(io::Error::last_os_error());
        }
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, len) };
        Ok(Some(Cursor { plane, fb: cmd.fb_id, handle: dumb.handle, map: ptr.cast(), len, pitch: dumb.pitch, size }))
    }

    /// Size of the hardware cursor of `head`, if it has one.
    pub fn cursor_size(&self, head: usize) -> Option<(u32, u32)> {
        self.heads.get(head)?.cursor.as_ref().map(|c| c.size)
    }

    /// Writes the cursor image: ARGB8888 (bytes B, G, R, A, premultiplied),
    /// `w`x`h` at most the cursor size; the rest is cleared.
    pub fn set_cursor_image(&mut self, head: usize, w: u32, h: u32, argb: &[u8]) {
        let Some(c) = self.heads.get_mut(head).and_then(|h| h.cursor.as_mut()) else { return };
        unsafe { std::ptr::write_bytes(c.map, 0, c.len) };
        for row in 0..h.min(c.size.1) {
            let n = (w.min(c.size.0) * 4) as usize;
            let src = &argb[(row * w * 4) as usize..][..n];
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), c.map.add((row * c.pitch) as usize), n) };
        }
        // The plane may show the old image until the next commit.
        if let Some(h) = self.heads.get_mut(head) {
            h.cursor_sent = None;
        }
    }

    /// Where the cursor's top-left goes (None hides it), from the next commit.
    pub fn set_cursor_pos(&mut self, head: usize, pos: Option<(i32, i32)>) {
        if let Some(h) = self.heads.get_mut(head) {
            h.cursor_want = pos;
        }
    }

    /// Moves the cursor without a new frame: the shown buffer again, with the
    /// cursor plane updated. False while a flip is pending (try again after).
    pub fn commit_cursor(&mut self, head: usize) -> io::Result<bool> {
        let Some(h) = self.heads.get(head) else { return Ok(true) };
        if h.cursor_sent == Some(h.cursor_want) {
            return Ok(true);
        }
        match (h.pending, h.front, h.modeset) {
            (None, Some(front), false) => self.present(head, front).map(|_| true),
            _ => Ok(false),
        }
    }

    fn create_blob_bytes(&self, data: &[u8]) -> io::Result<u32> {
        let mut b = drm::CreateBlob { data: data.as_ptr() as u64, length: data.len() as u32, blob_id: 0 };
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_CREATEPROPBLOB, &mut b)?;
        Ok(b.blob_id)
    }

    fn create_blob<T>(&self, data: &T) -> io::Result<u32> {
        let mut b = drm::CreateBlob { data: data as *const T as u64, length: std::mem::size_of::<T>() as u32, blob_id: 0 };
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_CREATEPROPBLOB, &mut b)?;
        Ok(b.blob_id)
    }

    /// Imports a buffer allocated on the render node and wraps it in a framebuffer.
    fn add_fb(&self, t: &Target) -> io::Result<(u32, u32)> {
        let dmabuf = t.export_fd().map_err(io::Error::other)?;
        let mut prime = drm::PrimeHandle { fd: dmabuf.as_raw_fd(), ..Default::default() };
        ioctl(self.fd.as_raw_fd(), drm::IOCTL_PRIME_FD_TO_HANDLE, &mut prime)?;
        let mut cmd = drm::FbCmd2 { width: t.width as u32, height: t.height as u32, pixel_format: t.format, ..Default::default() };
        cmd.handles[0] = prime.handle;
        cmd.pitches[0] = t.stride;
        cmd.offsets[0] = t.offset;
        if t.modifier != render::MOD_INVALID {
            cmd.flags = drm::MODE_FB_MODIFIERS;
            cmd.modifier[0] = t.modifier;
        }
        if let Err(e) = ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_ADDFB2, &mut cmd) {
            self.free_fb(0, prime.handle);
            return Err(e);
        }
        Ok((cmd.fb_id, prime.handle))
    }

    fn free_fb(&self, fb: u32, handle: u32) {
        let raw = self.fd.as_raw_fd();
        if fb != 0 {
            let mut fb = fb;
            let _ = ioctl(raw, drm::IOCTL_MODE_RMFB, &mut fb);
        }
        let mut close = drm::GemClose { handle, pad: 0 };
        let _ = ioctl(raw, drm::IOCTL_GEM_CLOSE, &mut close);
    }

    fn drop_head(&mut self, mut h: Head, r: &mut Renderer) {
        for b in h.buffers.drain(..) {
            self.free_fb(b.fb, b.handle);
            r.destroy_target(b.target);
        }
        if let Some(blob) = h.mode_blob {
            self.destroy_blob(blob);
        }
        if let Some(blob) = h.gamma_blob {
            self.destroy_blob(blob);
        }
        if let Some(c) = h.cursor.take() {
            unsafe { libc::munmap(c.map.cast(), c.len) };
            self.free_fb(c.fb, c.handle);
        }
    }

    fn destroy_blob(&self, blob: u32) {
        let mut b = drm::DestroyBlob { blob_id: blob };
        let _ = ioctl(self.fd.as_raw_fd(), drm::IOCTL_MODE_DESTROYPROPBLOB, &mut b);
    }

    /// The outputs being driven, the primary first.
    pub fn outputs(&self) -> impl Iterator<Item = &Output> {
        self.heads.iter().map(|h| &h.output)
    }

    /// Drives exactly `outputs` (the primary first); unchanged heads keep
    /// their buffers and skip the modeset.
    pub fn configure(&mut self, outputs: Vec<Output>, r: &mut Renderer) -> io::Result<()> {
        let mut old = std::mem::take(&mut self.heads);
        let mut dropped = false;
        for o in outputs {
            let same = |h: &Head| h.output.connector == o.connector && h.output.crtc == o.crtc && h.output.plane == o.plane && same_mode(&h.output.mode, &o.mode);
            if let Some(i) = old.iter().position(same) {
                self.heads.push(old.remove(i));
                continue;
            }
            let size = (o.mode.hdisplay as i32, o.mode.vdisplay as i32);
            let mut buffers = Vec::new();
            if let Some(i) = old.iter().position(|h| h.output.connector == o.connector) {
                let h = old.remove(i);
                self.drop_head(h, r);
                dropped = true;
            }
            for _ in 0..2 {
                let target = r.create_target(size.0, size.1, sys::gbm::BO_USE_SCANOUT).map_err(io::Error::other)?;
                let (fb, handle) = self.add_fb(&target)?;
                buffers.push(ScanBuffer { target, fb, handle });
            }
            log!("output {} ({}): {}x{}@{:.2}", o.name, o.model, size.0, size.1, o.mode.refresh_mhz() as f64 / 1000.0);
            let cursor = self.cursor_for(o.crtc).inspect_err(|e| log!("no hardware cursor on {}: {e}", o.name)).ok().flatten();
            self.heads.push(Head {
                output: o,
                cursor,
                cursor_want: None,
                cursor_sent: None,
                buffers,
                front: None,
                pending: None,
                modeset: true,
                mode_blob: None,
                on: true,
                gamma_sent: None,
                gamma_blob: None,
            });
        }
        for h in old {
            log!("output {} released", h.output.name);
            self.drop_head(h, r);
            dropped = true;
        }
        if dropped {
            for h in &mut self.heads {
                h.modeset = true;
            }
            if self.heads.is_empty() {
                self.disable_others()?;
            }
        }
        Ok(())
    }

    /// Powers a head down (CRTC off) or back up (modeset on the next present).
    pub fn set_power(&mut self, head: usize, on: bool) -> io::Result<()> {
        let Some(h) = self.heads.get_mut(head) else { return Ok(()) };
        if h.on == on {
            return Ok(());
        }
        h.on = on;
        h.pending = None;
        h.front = None;
        h.modeset = true;
        if on {
            return Ok(());
        }
        let o = h.output.clone();
        log!("output {} off", o.name);
        let objs = vec![(o.plane, vec![("FB_ID", 0), ("CRTC_ID", 0)]), (o.connector, vec![("CRTC_ID", 0)]), (o.crtc, vec![("ACTIVE", 0), ("MODE_ID", 0)])];
        self.commit(objs, drm::MODE_ATOMIC_ALLOW_MODESET)
    }

    pub fn request_modeset(&mut self) {
        for h in &mut self.heads {
            h.modeset = true;
            h.pending = None;
        }
    }

    pub fn acquire(&self, head: usize) -> Option<usize> {
        let h = self.heads.get(head).filter(|h| h.on && h.pending.is_none())?;
        (0..h.buffers.len()).find(|i| Some(*i) != h.front)
    }

    pub fn target(&self, head: usize, idx: usize) -> &Target {
        &self.heads[head].buffers[idx].target
    }

    pub fn present(&mut self, head: usize, idx: usize) -> io::Result<()> {
        if self.heads[head].modeset {
            self.disable_others()?;
        }
        let h = &self.heads[head];
        let o = &h.output;
        let fb = h.buffers[idx].fb as u64;
        let (w, h_px) = (o.mode.hdisplay as u64, o.mode.vdisplay as u64);
        let mut objs = vec![(
            o.plane,
            vec![
                ("FB_ID", fb),
                ("CRTC_ID", o.crtc as u64),
                ("SRC_X", 0),
                ("SRC_Y", 0),
                ("SRC_W", w << 16),
                ("SRC_H", h_px << 16),
                ("CRTC_X", 0),
                ("CRTC_Y", 0),
                ("CRTC_W", w),
                ("CRTC_H", h_px),
            ],
        )];
        if let Some(c) = h.cursor.as_ref().filter(|_| h.cursor_sent != Some(h.cursor_want) || h.modeset) {
            let props = match h.cursor_want {
                Some((x, y)) => vec![
                    ("FB_ID", c.fb as u64),
                    ("CRTC_ID", o.crtc as u64),
                    ("SRC_X", 0),
                    ("SRC_Y", 0),
                    ("SRC_W", (c.size.0 as u64) << 16),
                    ("SRC_H", (c.size.1 as u64) << 16),
                    ("CRTC_X", x as i64 as u64),
                    ("CRTC_Y", y as i64 as u64),
                    ("CRTC_W", c.size.0 as u64),
                    ("CRTC_H", c.size.1 as u64),
                ],
                None => vec![("FB_ID", 0), ("CRTC_ID", 0)],
            };
            objs.push((c.plane, props));
        }
        let cursor_now = h.cursor.as_ref().map(|_| h.cursor_want);
        let mut flags = drm::MODE_PAGE_FLIP_EVENT;
        // The night light rides along with the next frame of each head.
        let want = self.gamma.unwrap_or((1.0, 1.0, 1.0));
        let mut new_gamma = None;
        // Never in a modeset: a refused LUT must not cost the output.
        if h.gamma_sent != Some(want) && !h.modeset {
            if let Some(size) = self.gamma_size.get(&o.crtc).copied() {
                let blob = if self.gamma.is_some() { self.create_blob_bytes(&gamma_ramp(size, want))? } else { 0 };
                objs.push((o.crtc, vec![("GAMMA_LUT", blob as u64)]));
                new_gamma = Some(blob);
            }
        }
        let h = &self.heads[head];
        let o = &h.output;
        if h.modeset {
            let blob = self.create_blob(&o.mode)?;
            objs.push((o.connector, vec![("CRTC_ID", o.crtc as u64)]));
            objs.push((o.crtc, vec![("MODE_ID", blob as u64), ("ACTIVE", 1)]));
            flags |= drm::MODE_ATOMIC_ALLOW_MODESET;
            let name = o.name.clone();
            let res = self.commit(objs, flags);
            if let Some(old) = self.heads[head].mode_blob.replace(blob) {
                self.destroy_blob(old);
            }
            res.map_err(|e| io::Error::other(format!("modeset of {name} failed: {e}")))?;
            // The LUT goes with the next frame.
            self.heads[head].gamma_sent = None;
        } else {
            flags |= drm::MODE_ATOMIC_NONBLOCK;
            if let Err(e) = self.commit(objs.clone(), flags) {
                if new_gamma.is_none() {
                    return Err(e);
                }
                // A driver that refuses the LUT must not stop the frames.
                let crtc = self.heads[head].output.crtc;
                log!("night light refused on CRTC {crtc}: {e}");
                self.gamma_size.remove(&crtc);
                if let Some(blob) = new_gamma.take().filter(|b| *b != 0) {
                    self.destroy_blob(blob);
                }
                objs.retain(|(obj, list)| !(*obj == crtc && list.iter().any(|(n, _)| *n == "GAMMA_LUT")));
                self.commit(objs, flags)?;
            }
        }
        if let Some(blob) = new_gamma {
            let h = &mut self.heads[head];
            h.gamma_sent = Some(want);
            let old = std::mem::replace(&mut h.gamma_blob, (blob != 0).then_some(blob));
            if let Some(old) = old {
                self.destroy_blob(old);
            }
        }
        let h = &mut self.heads[head];
        if let Some(c) = cursor_now {
            h.cursor_sent = Some(c);
        }
        h.modeset = false;
        h.pending = Some(idx);
        Ok(())
    }

    /// Turns off what the previous DRM master left on, in its own commit:
    /// a commit asking for a page-flip event must not contain disabled CRTCs.
    fn disable_others(&mut self) -> io::Result<()> {
        let mine = |f: fn(&Output) -> u32| self.heads.iter().filter(|h| h.on).map(|h| f(&h.output)).collect::<Vec<u32>>();
        let (connectors, crtcs, planes) = (mine(|o| o.connector), mine(|o| o.crtc), mine(|o| o.plane));
        let mut objs = Vec::new();
        for c in self.connectors_all.clone() {
            if !connectors.contains(&c) && self.current(c, drm::OBJECT_CONNECTOR, "CRTC_ID")? != 0 {
                objs.push((c, vec![("CRTC_ID", 0)]));
            }
        }
        for (p, ..) in self.planes.clone() {
            if !planes.contains(&p) && (self.current(p, drm::OBJECT_PLANE, "FB_ID")? != 0 || self.current(p, drm::OBJECT_PLANE, "CRTC_ID")? != 0) {
                objs.push((p, vec![("FB_ID", 0), ("CRTC_ID", 0)]));
            }
        }
        for c in self.crtcs.clone() {
            if !crtcs.contains(&c) && self.current(c, drm::OBJECT_CRTC, "ACTIVE")? != 0 {
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
            if kind == drm::EVENT_FLIP_COMPLETE && off + std::mem::size_of::<drm::EventVblank>() <= n as usize {
                let crtc = u32::from_ne_bytes(buf[off + 28..off + 32].try_into().unwrap());
                if let Some(h) = self.heads.iter_mut().find(|h| h.output.crtc == crtc) {
                    if let Some(p) = h.pending.take() {
                        h.front = Some(p);
                    }
                }
                flipped = true;
            }
            off += len.max(8);
        }
        if flipped {
            for (fb, handle) in std::mem::take(&mut self.stale) {
                self.free_fb(fb, handle);
            }
        }
        flipped
    }

    /// Hands the device over to the next process; the current framebuffers
    /// stay on screen until it presents its first frame.
    pub fn save(mut self) -> Saved {
        let mut stale = std::mem::take(&mut self.stale);
        let mut heads = Vec::new();
        for mut h in std::mem::take(&mut self.heads) {
            stale.extend(h.buffers.iter().map(|b| (b.fb, b.handle)));
            // The targets live on the render node, which closes with the process.
            h.buffers.clear();
            heads.push((h.output, h.mode_blob));
        }
        Saved { fd: self.fd, heads, stale }
    }

    pub fn restore(saved: Saved, r: &mut Renderer) -> io::Result<Kms> {
        let mut kms = Kms::new(saved.fd)?;
        kms.stale = saved.stale;
        let blobs: Vec<Option<u32>> = saved.heads.iter().map(|(_, b)| *b).collect();
        kms.configure(saved.heads.into_iter().map(|(o, _)| o).collect(), r)?;
        for (h, b) in kms.heads.iter_mut().zip(blobs) {
            h.mode_blob = b;
        }
        Ok(kms)
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
