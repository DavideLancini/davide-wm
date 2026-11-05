//! A minimal X11 client: the part of the core protocol (and of the Composite
//! and XFixes extensions) a window manager for Xwayland needs.
//!
//! Requests are queued and written by `flush`. Replies are waited for
//! synchronously, in the order the requests were sent; events read meanwhile
//! are queued for `next_event`.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub mod atom {
    pub const NONE: u32 = 0;
    pub const PRIMARY: u32 = 1;
    pub const ATOM: u32 = 4;
    pub const CARDINAL: u32 = 6;
    pub const STRING: u32 = 31;
    pub const WINDOW: u32 = 33;
    pub const WM_HINTS: u32 = 35;
    pub const WM_NAME: u32 = 39;
    pub const WM_NORMAL_HINTS: u32 = 40;
    pub const WM_CLASS: u32 = 67;
    pub const WM_TRANSIENT_FOR: u32 = 68;
}

pub mod mask {
    pub const STRUCTURE_NOTIFY: u32 = 0x2_0000;
    pub const SUBSTRUCTURE_NOTIFY: u32 = 0x8_0000;
    pub const SUBSTRUCTURE_REDIRECT: u32 = 0x10_0000;
    pub const FOCUS_CHANGE: u32 = 0x20_0000;
    pub const PROPERTY_CHANGE: u32 = 0x40_0000;
}

/// Window attribute bits for `create_window` and `change_attributes`.
pub mod cw {
    pub const OVERRIDE_REDIRECT: u32 = 0x200;
    pub const EVENT_MASK: u32 = 0x800;
}

/// Value bits of `configure_window` and of a ConfigureRequest.
pub mod config {
    pub const X: u16 = 1;
    pub const Y: u16 = 2;
    pub const WIDTH: u16 = 4;
    pub const HEIGHT: u16 = 8;
    pub const BORDER: u16 = 16;
    pub const SIBLING: u16 = 32;
    pub const STACK_MODE: u16 = 64;
}

const TIMEOUT: Duration = Duration::from_secs(2);

/// The largest request without the BIG-REQUESTS extension, in bytes.
pub const MAX_REQUEST: usize = 0xffff * 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct Setup {
    pub root: u32,
    pub root_visual: u32,
    pub id_base: u32,
    pub id_mask: u32,
}

#[derive(Debug)]
pub struct Property {
    pub ty: u32,
    pub format: u8,
    pub data: Vec<u8>,
}

impl Property {
    pub fn u32s(&self) -> Vec<u32> {
        if self.format != 32 {
            return Vec::new();
        }
        self.data.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
    }

    pub fn text(&self) -> String {
        if self.format != 8 {
            return String::new();
        }
        match self.ty {
            // Latin-1.
            atom::STRING => self.data.iter().map(|b| *b as char).collect(),
            _ => String::from_utf8_lossy(&self.data).into_owned(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Event {
    FocusIn { window: u32, mode: u8 },
    Create { parent: u32, window: u32, override_redirect: bool, x: i16, y: i16, w: u16, h: u16 },
    Destroy { window: u32 },
    Unmap { window: u32 },
    Map { window: u32, override_redirect: bool },
    MapRequest { window: u32 },
    Configure { window: u32, x: i16, y: i16, w: u16, h: u16, override_redirect: bool },
    ConfigureRequest { window: u32, mask: u16, x: i16, y: i16, w: u16, h: u16, sibling: u32, stack_mode: u8 },
    Property { window: u32, atom: u32, deleted: bool },
    SelectionClear { owner: u32, selection: u32, time: u32 },
    SelectionRequest { time: u32, owner: u32, requestor: u32, selection: u32, target: u32, property: u32 },
    SelectionNotify { time: u32, requestor: u32, selection: u32, target: u32, property: u32 },
    ClientMessage { window: u32, ty: u32, format: u8, data: [u32; 5] },
    /// Anything else, extension events included: code and raw bytes.
    Other(u8, [u8; 32]),
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn i16_at(b: &[u8], i: usize) -> i16 {
    u16_at(b, i) as i16
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

impl Event {
    pub fn parse(b: &[u8; 32]) -> Event {
        // The top bit marks events sent by another client.
        match b[0] & 0x7f {
            9 => Event::FocusIn { window: u32_at(b, 4), mode: b[8] },
            16 => Event::Create {
                parent: u32_at(b, 4),
                window: u32_at(b, 8),
                x: i16_at(b, 12),
                y: i16_at(b, 14),
                w: u16_at(b, 16),
                h: u16_at(b, 18),
                override_redirect: b[22] != 0,
            },
            17 => Event::Destroy { window: u32_at(b, 8) },
            18 => Event::Unmap { window: u32_at(b, 8) },
            19 => Event::Map { window: u32_at(b, 8), override_redirect: b[12] != 0 },
            20 => Event::MapRequest { window: u32_at(b, 8) },
            22 => Event::Configure {
                window: u32_at(b, 8),
                x: i16_at(b, 16),
                y: i16_at(b, 18),
                w: u16_at(b, 20),
                h: u16_at(b, 22),
                override_redirect: b[26] != 0,
            },
            23 => Event::ConfigureRequest {
                stack_mode: b[1],
                window: u32_at(b, 8),
                sibling: u32_at(b, 12),
                x: i16_at(b, 16),
                y: i16_at(b, 18),
                w: u16_at(b, 20),
                h: u16_at(b, 22),
                mask: u16_at(b, 26),
            },
            28 => Event::Property { window: u32_at(b, 4), atom: u32_at(b, 8), deleted: b[16] != 0 },
            29 => Event::SelectionClear { time: u32_at(b, 4), owner: u32_at(b, 8), selection: u32_at(b, 12) },
            30 => Event::SelectionRequest {
                time: u32_at(b, 4),
                owner: u32_at(b, 8),
                requestor: u32_at(b, 12),
                selection: u32_at(b, 16),
                target: u32_at(b, 20),
                property: u32_at(b, 24),
            },
            31 => Event::SelectionNotify {
                time: u32_at(b, 4),
                requestor: u32_at(b, 8),
                selection: u32_at(b, 12),
                target: u32_at(b, 16),
                property: u32_at(b, 20),
            },
            33 => {
                let mut data = [0u32; 5];
                for (i, d) in data.iter_mut().enumerate() {
                    *d = u32_at(b, 12 + 4 * i);
                }
                Event::ClientMessage { window: u32_at(b, 4), ty: u32_at(b, 8), format: b[1], data }
            }
            code => Event::Other(code, *b),
        }
    }
}

/// Builds the 32 bytes of an event to send with `send_event`.
pub fn client_message(window: u32, ty: u32, data: [u32; 5]) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0] = 33;
    b[1] = 32;
    b[4..8].copy_from_slice(&window.to_le_bytes());
    b[8..12].copy_from_slice(&ty.to_le_bytes());
    for (i, d) in data.iter().enumerate() {
        b[12 + 4 * i..16 + 4 * i].copy_from_slice(&d.to_le_bytes());
    }
    b
}

/// A ConfigureNotify telling a client its geometry (ICCCM 4.1.5).
pub fn configure_notify(window: u32, x: i16, y: i16, w: u16, h: u16) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0] = 22;
    b[4..8].copy_from_slice(&window.to_le_bytes());
    b[8..12].copy_from_slice(&window.to_le_bytes());
    b[16..18].copy_from_slice(&x.to_le_bytes());
    b[18..20].copy_from_slice(&y.to_le_bytes());
    b[20..22].copy_from_slice(&w.to_le_bytes());
    b[22..24].copy_from_slice(&h.to_le_bytes());
    b
}

/// A SelectionNotify answering a SelectionRequest; `property` 0 refuses it.
pub fn selection_notify(time: u32, requestor: u32, selection: u32, target: u32, property: u32) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[0] = 31;
    for (i, v) in [time, requestor, selection, target, property].iter().enumerate() {
        b[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// Request bytes: fixed words, then a byte payload padded to 4.
struct Req(Vec<u8>);

impl Req {
    fn new() -> Req {
        Req(Vec::new())
    }
    fn u32(mut self, v: u32) -> Req {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u16(mut self, v: u16) -> Req {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(mut self, b: &[u8]) -> Req {
        self.0.extend_from_slice(b);
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
}

pub struct Conn {
    fd: OwnedFd,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// Requests sent so far; the wire carries the low 16 bits.
    seq: u64,
    events: VecDeque<[u8; 32]>,
    pub setup: Setup,
    next_id: u32,
}

fn poll(fd: i32, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "X server not responding"));
        }
        let mut p = libc::pollfd { fd, events, revents: 0 };
        let n = unsafe { libc::poll(&mut p, 1, left.as_millis().max(1) as i32) };
        if n > 0 {
            return Ok(());
        }
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}

impl Conn {
    /// Sets up the connection on an fd already connected to the server
    /// (Xwayland's `-wm` socket needs no authorisation).
    pub fn connect(fd: OwnedFd) -> io::Result<Conn> {
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut c = Conn { fd, inbuf: Vec::new(), out: Vec::new(), seq: 0, events: VecDeque::new(), setup: Setup::default(), next_id: 1 };
        c.out.extend_from_slice(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        c.flush()?;
        let deadline = Instant::now() + TIMEOUT;
        while c.inbuf.len() < 8 || c.inbuf.len() < 8 + 4 * u16_at(&c.inbuf, 6) as usize {
            c.read_until(deadline)?;
        }
        let total = 8 + 4 * u16_at(&c.inbuf, 6) as usize;
        let b: Vec<u8> = c.inbuf.drain(..total).collect();
        if b[0] != 1 {
            let reason = String::from_utf8_lossy(b.get(8..8 + b[1] as usize).unwrap_or_default()).into_owned();
            return Err(io::Error::other(format!("X connection refused: {reason}")));
        }
        let vendor_len = u16_at(&b, 24) as usize;
        let formats = b[29] as usize;
        let screen = 40 + vendor_len.div_ceil(4) * 4 + 8 * formats;
        if b.len() < screen + 40 {
            return Err(io::Error::other("short X setup reply"));
        }
        c.setup = Setup { root: u32_at(&b, screen), root_visual: u32_at(&b, screen + 32), id_base: u32_at(&b, 12), id_mask: u32_at(&b, 16) };
        Ok(c)
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    pub fn new_id(&mut self) -> u32 {
        let id = self.setup.id_base | (self.next_id & self.setup.id_mask);
        self.next_id += 1;
        id
    }

    fn request(&mut self, opcode: u8, data: u8, body: Req) -> u64 {
        let len = (4 + body.0.len()) / 4;
        self.out.extend_from_slice(&[opcode, data]);
        self.out.extend_from_slice(&(len as u16).to_le_bytes());
        self.out.extend_from_slice(&body.0);
        self.seq += 1;
        self.seq
    }

    /// Writes the queued requests, waiting if the socket is full.
    pub fn flush(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        while !self.out.is_empty() {
            let n = unsafe { libc::write(self.raw_fd(), self.out.as_ptr().cast(), self.out.len()) };
            if n >= 0 {
                self.out.drain(..n as usize);
                continue;
            }
            match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::WouldBlock => poll(self.raw_fd(), libc::POLLOUT, deadline)?,
                e if e.kind() == io::ErrorKind::Interrupted => {}
                e => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads what is available; false when nothing was.
    fn read_some(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 16384];
        let n = unsafe { libc::read(self.raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            0 => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "X server closed the connection")),
            n if n > 0 => {
                self.inbuf.extend_from_slice(&buf[..n as usize]);
                Ok(true)
            }
            _ => match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => Ok(false),
                e => Err(e),
            },
        }
    }

    fn read_until(&mut self, deadline: Instant) -> io::Result<()> {
        while !self.read_some()? {
            poll(self.raw_fd(), libc::POLLIN, deadline)?;
        }
        Ok(())
    }

    /// Takes the next complete packet: an error, a reply or an event.
    fn packet(&mut self) -> Option<Vec<u8>> {
        if self.inbuf.len() < 32 {
            return None;
        }
        // Replies and generic events carry extra length.
        let len = match self.inbuf[0] {
            1 | 35 => 32 + 4 * u32_at(&self.inbuf, 4) as usize,
            _ => 32,
        };
        (self.inbuf.len() >= len).then(|| self.inbuf.drain(..len).collect())
    }

    /// Queues events; returns the packet if it answers request `seq`.
    fn route(&mut self, p: Vec<u8>, seq: Option<u16>) -> Option<Vec<u8>> {
        let mine = seq == Some(u16_at(&p, 2));
        match p[0] {
            0 if mine => Some(p),
            // BadWindow: the window went away while we were managing it.
            0 if p[1] == 3 => None,
            0 => {
                eprintln!("davide-wm: X error {} on request {}.{} (resource {:#x})", p[1], p[10], u16_at(&p, 8), u32_at(&p, 4));
                None
            }
            1 if mine => Some(p),
            1 => None,
            _ => {
                self.events.push_back(p[..32].try_into().unwrap());
                None
            }
        }
    }

    /// Waits for the reply to request `seq`; None if the server answered
    /// with an error.
    fn reply(&mut self, seq: u64) -> io::Result<Option<Vec<u8>>> {
        self.flush()?;
        let want = Some(seq as u16);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            while let Some(p) = self.packet() {
                if let Some(p) = self.route(p, want) {
                    return Ok((p[0] == 1).then_some(p));
                }
            }
            self.read_until(deadline)?;
        }
    }

    /// Reads from the socket and returns the next event, if any.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        loop {
            if let Some(e) = self.events.pop_front() {
                return Ok(Some(Event::parse(&e)));
            }
            while let Some(p) = self.packet() {
                self.route(p, None);
            }
            if self.events.is_empty() && !self.read_some()? {
                return Ok(None);
            }
        }
    }

    pub fn intern_atoms(&mut self, names: &[&str]) -> io::Result<Vec<u32>> {
        let seqs: Vec<u64> =
            names.iter().map(|n| self.request(16, 0, Req::new().u16(n.len() as u16).u16(0).bytes(n.as_bytes()))).collect();
        seqs.into_iter().map(|s| Ok(self.reply(s)?.map_or(0, |r| u32_at(&r, 8)))).collect()
    }

    pub fn query_extension(&mut self, name: &str) -> io::Result<Option<(u8, u8)>> {
        let seq = self.request(98, 0, Req::new().u16(name.len() as u16).u16(0).bytes(name.as_bytes()));
        Ok(self.reply(seq)?.filter(|r| r[8] != 0).map(|r| (r[9], r[10])))
    }

    /// Sends an extension request and waits for its reply.
    pub fn extension_call(&mut self, major: u8, minor: u8, words: &[u32]) -> io::Result<Option<Vec<u8>>> {
        let seq = self.extension(major, minor, words);
        self.reply(seq)
    }

    pub fn extension(&mut self, major: u8, minor: u8, words: &[u32]) -> u64 {
        let body = words.iter().fold(Req::new(), |r, w| r.u32(*w));
        self.request(major, minor, body)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_window(&mut self, id: u32, parent: u32, x: i16, y: i16, w: u16, h: u16, input_only: bool, mask: u32, values: &[u32]) {
        let mut r = Req::new().u32(id).u32(parent).u16(x as u16).u16(y as u16).u16(w).u16(h).u16(0).u16(if input_only { 2 } else { 1 }).u32(0).u32(mask);
        for v in values {
            r = r.u32(*v);
        }
        self.request(1, 0, r);
    }

    pub fn change_attributes(&mut self, window: u32, mask: u32, values: &[u32]) {
        let r = values.iter().fold(Req::new().u32(window).u32(mask), |r, v| r.u32(*v));
        self.request(2, 0, r);
    }

    /// (map state, override redirect) of a window; map state 2 is viewable.
    pub fn window_attributes(&mut self, window: u32) -> io::Result<Option<(u8, bool)>> {
        let seq = self.request(3, 0, Req::new().u32(window));
        Ok(self.reply(seq)?.map(|r| (r[26], r[27] != 0)))
    }

    pub fn map_window(&mut self, window: u32) {
        self.request(8, 0, Req::new().u32(window));
    }

    /// `values` in the order of the bits set in `mask`.
    pub fn configure_window(&mut self, window: u32, mask: u16, values: &[u32]) {
        let r = values.iter().fold(Req::new().u32(window).u16(mask).u16(0), |r, v| r.u32(*v));
        self.request(12, 0, r);
    }

    /// The children of a window, bottom to top.
    pub fn children(&mut self, window: u32) -> io::Result<Vec<u32>> {
        let seq = self.request(15, 0, Req::new().u32(window));
        let Some(r) = self.reply(seq)? else { return Ok(Vec::new()) };
        let n = u16_at(&r, 16) as usize;
        Ok((0..n).filter_map(|i| r.get(32 + 4 * i..36 + 4 * i).map(|b| u32_at(b, 0))).collect())
    }

    /// False when the data does not fit in one request.
    pub fn change_property(&mut self, window: u32, property: u32, ty: u32, format: u8, data: &[u8]) -> bool {
        if 24 + data.len() + 3 > MAX_REQUEST {
            return false;
        }
        let units = data.len() / (format as usize / 8);
        let r = Req::new().u32(window).u32(property).u32(ty).u32(format as u32).u32(units as u32).bytes(data);
        self.request(18, 0, r);
        true
    }

    pub fn change_property32(&mut self, window: u32, property: u32, ty: u32, values: &[u32]) {
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.change_property(window, property, ty, 32, &data);
    }

    pub fn change_property8(&mut self, window: u32, property: u32, ty: u32, data: &[u8]) -> bool {
        self.change_property(window, property, ty, 8, data)
    }

    pub fn delete_property(&mut self, window: u32, property: u32) {
        self.request(19, 0, Req::new().u32(window).u32(property));
    }

    /// Reads several properties of a window at the cost of one round trip.
    pub fn properties(&mut self, window: u32, names: &[u32], max_bytes: u32) -> io::Result<Vec<Option<Property>>> {
        let seqs: Vec<u64> = names.iter().map(|p| self.request(20, 0, Req::new().u32(window).u32(*p).u32(0).u32(0).u32(max_bytes / 4))).collect();
        seqs.into_iter().map(|s| Ok(self.reply(s)?.and_then(|r| parse_property(&r)))).collect()
    }

    /// Reads and deletes a property (selection transfers).
    pub fn take_property(&mut self, window: u32, name: u32) -> io::Result<Option<Property>> {
        let seq = self.request(20, 1, Req::new().u32(window).u32(name).u32(0).u32(0).u32(u32::MAX / 4));
        Ok(self.reply(seq)?.and_then(|r| parse_property(&r)))
    }

    pub fn atom_name(&mut self, atom: u32) -> io::Result<Option<String>> {
        let seq = self.request(17, 0, Req::new().u32(atom));
        Ok(self.reply(seq)?.and_then(|r| {
            let n = u16_at(&r, 8) as usize;
            r.get(32..32 + n).map(|b| String::from_utf8_lossy(b).into_owned())
        }))
    }

    pub fn property(&mut self, window: u32, name: u32, max_bytes: u32) -> io::Result<Option<Property>> {
        Ok(self.properties(window, &[name], max_bytes)?.pop().flatten())
    }

    pub fn set_selection_owner(&mut self, owner: u32, selection: u32, time: u32) {
        self.request(22, 0, Req::new().u32(owner).u32(selection).u32(time));
    }

    pub fn convert_selection(&mut self, requestor: u32, selection: u32, target: u32, property: u32, time: u32) {
        self.request(24, 0, Req::new().u32(requestor).u32(selection).u32(target).u32(property).u32(time));
    }

    pub fn send_event(&mut self, destination: u32, mask: u32, event: &[u8; 32]) {
        self.request(25, 0, Req::new().u32(destination).u32(mask).bytes(event));
    }

    /// `revert` 1 is PointerRoot; `window` 0 is None.
    pub fn set_input_focus(&mut self, window: u32, revert: u8, time: u32) {
        self.request(42, revert, Req::new().u32(window).u32(time));
    }

    pub fn kill_client(&mut self, resource: u32) {
        self.request(113, 0, Req::new().u32(resource));
    }
}

fn parse_property(r: &[u8]) -> Option<Property> {
    let (format, ty) = (r[1], u32_at(r, 8));
    if ty == atom::NONE || !matches!(format, 8 | 16 | 32) {
        return None;
    }
    let len = u32_at(r, 16) as usize * (format as usize / 8);
    Some(Property { ty, format, data: r.get(32..32 + len)?.to_vec() })
}

impl state::Persist for Conn {
    fn save(&self, e: &mut state::Enc) {
        self.fd.save(e);
        state::save_bytes(&self.inbuf, e);
        state::save_bytes(&self.out, e);
        self.seq.save(e);
        let events: Vec<u8> = self.events.iter().flatten().copied().collect();
        state::save_bytes(&events, e);
        let s = self.setup;
        (s.root, s.root_visual, s.id_base, s.id_mask).save(e);
        self.next_id.save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let fd = OwnedFd::load(d)?;
        let inbuf = state::load_bytes(d)?;
        let out = state::load_bytes(d)?;
        let seq = u64::load(d)?;
        let events = state::load_bytes(d)?.as_chunks::<32>().0.to_vec().into();
        let (root, root_visual, id_base, id_mask) = state::Persist::load(d)?;
        let next_id = u32::load(d)?;
        Ok(Conn { fd, inbuf, out, seq, events, setup: Setup { root, root_visual, id_base, id_mask }, next_id })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A fake server: answers the setup, then one InternAtom.
    #[test]
    fn setup_reply_and_event_routing() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut s = theirs;
            let mut hello = [0u8; 12];
            s.read_exact(&mut hello).unwrap();
            assert_eq!(hello[0], b'l');
            let vendor = b"test";
            let mut body = vec![0u8; 32];
            body[4..8].copy_from_slice(&0x0040_0000u32.to_le_bytes());
            body[8..12].copy_from_slice(&0x001f_ffffu32.to_le_bytes());
            body[16..18].copy_from_slice(&(vendor.len() as u16).to_le_bytes());
            body[21] = 0;
            body.extend_from_slice(vendor);
            let mut screen = vec![0u8; 40];
            screen[..4].copy_from_slice(&0x3c5u32.to_le_bytes());
            screen[32..36].copy_from_slice(&0x21u32.to_le_bytes());
            body.extend_from_slice(&screen);
            let mut reply = vec![1, 0, 11, 0, 0, 0];
            reply.extend_from_slice(&((body.len() / 4) as u16).to_le_bytes());
            reply.extend_from_slice(&body);
            s.write_all(&reply).unwrap();
            let mut req = [0u8; 12];
            s.read_exact(&mut req).unwrap();
            assert_eq!(req[0], 16);
            // An event first, then the reply to request 1.
            let mut ev = [0u8; 32];
            ev[0] = 20;
            ev[8..12].copy_from_slice(&7u32.to_le_bytes());
            s.write_all(&ev).unwrap();
            let mut rep = [0u8; 32];
            rep[0] = 1;
            rep[2] = 1;
            rep[8..12].copy_from_slice(&99u32.to_le_bytes());
            s.write_all(&rep).unwrap();
        });
        let mut c = Conn::connect(OwnedFd::from(ours)).unwrap();
        assert_eq!(c.setup.root, 0x3c5);
        assert_eq!(c.setup.root_visual, 0x21);
        assert_eq!(c.new_id(), 0x0040_0001);
        assert_eq!(c.intern_atoms(&["FOO"]).unwrap(), vec![99]);
        assert_eq!(c.next_event().unwrap(), Some(Event::MapRequest { window: 7 }));
        server.join().unwrap();
    }

    #[test]
    fn property_text() {
        let p = Property { ty: atom::STRING, format: 8, data: vec![b'c', 0xe0] };
        assert_eq!(p.text(), "cà");
        let q = Property { ty: atom::ATOM, format: 32, data: [1u32, 2].iter().flat_map(|v| v.to_le_bytes()).collect() };
        assert_eq!(q.u32s(), vec![1, 2]);
    }
}
