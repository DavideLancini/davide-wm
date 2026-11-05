//! Minimal D-Bus client: SASL EXTERNAL authentication, message marshalling
//! (little endian), method calls with timeouts, signals and fd passing.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Double(f64),
    Str(String),
    Path(String),
    Sig(String),
    /// Index into the message's fds.
    Fd(u32),
    Variant(Box<Value>),
    /// Element signature and elements.
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    DictEntry(Box<Value>, Box<Value>),
}

impl Value {
    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".into(),
            Value::Bool(_) => "b".into(),
            Value::I32(_) => "i".into(),
            Value::U32(_) => "u".into(),
            Value::I64(_) => "x".into(),
            Value::U64(_) => "t".into(),
            Value::Double(_) => "d".into(),
            Value::Str(_) => "s".into(),
            Value::Path(_) => "o".into(),
            Value::Sig(_) => "g".into(),
            Value::Fd(_) => "h".into(),
            Value::Variant(_) => "v".into(),
            Value::Array(e, _) => format!("a{e}"),
            Value::Struct(v) => format!("({})", v.iter().map(Value::signature).collect::<String>()),
            Value::DictEntry(k, v) => format!("{{{}{}}}", k.signature(), v.signature()),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) | Value::Path(s) | Value::Sig(s) => Some(s),
            Value::Variant(v) => v.as_str(),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Value::U32(v) | Value::Fd(v) => Some(*v),
            Value::Byte(v) => Some(*v as u32),
            Value::I32(v) => Some(*v as u32),
            Value::Variant(v) => v.as_u32(),
            _ => None,
        }
    }

    /// The value of `key` in a dictionary (`a{s…}`), variants unwrapped.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let Value::Array(_, entries) = self.unwrap_variant() else { return None };
        entries.iter().find_map(|e| match e {
            Value::DictEntry(k, v) if k.as_str() == Some(key) => Some(v.unwrap_variant()),
            _ => None,
        })
    }

    pub fn unwrap_variant(&self) -> &Value {
        match self {
            Value::Variant(v) => v.unwrap_variant(),
            v => v,
        }
    }

    /// The bytes of an `ay`.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let Value::Array(_, items) = self.unwrap_variant() else { return None };
        items.iter().map(|b| match b {
            Value::Byte(b) => Some(*b),
            _ => None,
        }).collect()
    }

    /// An `a{sv}` from pairs.
    pub fn dict(entries: Vec<(&str, Value)>) -> Value {
        Value::Array("{sv}".into(), entries.into_iter().map(|(k, v)| Value::DictEntry(Box::new(Value::Str(k.into())), Box::new(Value::Variant(Box::new(v))))).collect())
    }

    pub fn byte_array(b: &[u8]) -> Value {
        Value::Array("y".into(), b.iter().map(|b| Value::Byte(*b)).collect())
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Variant(v) => v.as_bool(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Protocol(&'static str),
    Timeout,
    /// An error reply: name and message.
    Remote(String, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "D-Bus I/O: {e}"),
            Error::Protocol(s) => write!(f, "D-Bus protocol: {s}"),
            Error::Timeout => f.write_str("D-Bus call timed out"),
            Error::Remote(n, m) => write!(f, "{n}: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;

#[derive(Debug, Default)]
pub struct Message {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
    pub fds: Vec<Option<OwnedFd>>,
}

impl Message {
    pub fn call(dest: &str, path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        Message {
            kind: METHOD_CALL,
            path: Some(path.into()),
            interface: Some(iface.into()),
            member: Some(member.into()),
            destination: Some(dest.into()),
            body,
            ..Default::default()
        }
    }

    /// The method return for this call.
    pub fn reply(&self, body: Vec<Value>) -> Message {
        Message { kind: METHOD_RETURN, reply_serial: Some(self.serial), destination: self.sender.clone(), body, ..Default::default() }
    }

    pub fn error_reply(&self, name: &str, text: &str) -> Message {
        Message {
            kind: ERROR,
            error_name: Some(name.into()),
            reply_serial: Some(self.serial),
            destination: self.sender.clone(),
            body: vec![Value::Str(text.into())],
            ..Default::default()
        }
    }

    pub fn signal(path: &str, iface: &str, member: &str, body: Vec<Value>) -> Message {
        Message { kind: SIGNAL, path: Some(path.into()), interface: Some(iface.into()), member: Some(member.into()), body, ..Default::default() }
    }

    pub fn is_call(&self, iface: &str, member: &str) -> bool {
        self.kind == METHOD_CALL && self.interface.as_deref() == Some(iface) && self.member.as_deref() == Some(member)
    }

    /// The caller expects no reply.
    pub fn no_reply(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn is_signal(&self, iface: &str, member: &str) -> bool {
        self.kind == SIGNAL && self.interface.as_deref() == Some(iface) && self.member.as_deref() == Some(member)
    }

    /// Takes the file descriptor referenced by `Value::Fd` at body position `i`.
    pub fn take_fd(&mut self, i: usize) -> Option<OwnedFd> {
        let idx = self.body.get(i)?.as_u32()? as usize;
        self.fds.get_mut(idx)?.take()
    }
}

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn align(&mut self, n: usize) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(0);
        }
    }

    fn u32(&mut self, v: u32) {
        self.align(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put(&mut self, v: &Value) {
        match v {
            Value::Byte(b) => self.buf.push(*b),
            Value::Bool(b) => self.u32(*b as u32),
            Value::I32(i) => self.u32(*i as u32),
            Value::U32(u) | Value::Fd(u) => self.u32(*u),
            Value::I64(i) => {
                self.align(8);
                self.buf.extend_from_slice(&i.to_le_bytes());
            }
            Value::U64(u) => {
                self.align(8);
                self.buf.extend_from_slice(&u.to_le_bytes());
            }
            Value::Double(d) => {
                self.align(8);
                self.buf.extend_from_slice(&d.to_le_bytes());
            }
            Value::Str(s) | Value::Path(s) => {
                self.u32(s.len() as u32);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Sig(s) => {
                self.buf.push(s.len() as u8);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
            Value::Variant(inner) => {
                self.put(&Value::Sig(inner.signature()));
                self.put(inner);
            }
            Value::Array(elem, items) => {
                self.u32(0);
                let len_at = self.buf.len() - 4;
                self.align(type_alignment(elem));
                let start = self.buf.len();
                for i in items {
                    self.put(i);
                }
                let len = (self.buf.len() - start) as u32;
                self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
            }
            Value::Struct(items) => {
                self.align(8);
                for i in items {
                    self.put(i);
                }
            }
            Value::DictEntry(k, v) => {
                self.align(8);
                self.put(k);
                self.put(v);
            }
        }
    }
}

fn type_alignment(sig: &str) -> usize {
    match sig.as_bytes().first() {
        Some(b'y' | b'g' | b'v') => 1,
        Some(b'n' | b'q') => 2,
        Some(b'x' | b't' | b'd' | b'(' | b'{') => 8,
        _ => 4,
    }
}

/// Splits the first complete type off a signature.
fn split_type(sig: &str) -> Result<(&str, &str), Error> {
    let b = sig.as_bytes();
    let end = match b.first() {
        None => return Err(Error::Protocol("empty signature")),
        Some(b'a') => 1 + split_type(&sig[1..])?.0.len(),
        Some(open @ (b'(' | b'{')) => {
            let close = if *open == b'(' { b')' } else { b'}' };
            let mut depth = 0;
            let mut end = 0;
            for (i, c) in b.iter().enumerate() {
                if *c == *open {
                    depth += 1;
                } else if *c == close {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
            }
            if end == 0 {
                return Err(Error::Protocol("unbalanced signature"));
            }
            end
        }
        Some(_) => 1,
    };
    Ok((&sig[..end], &sig[end..]))
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn align(&mut self, n: usize) {
        self.pos = self.pos.div_ceil(n) * n;
    }

    fn bytes(&mut self, n: usize) -> Result<&[u8], Error> {
        let b = self.buf.get(self.pos..self.pos + n).ok_or(Error::Protocol("message truncated"))?;
        self.pos += n;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.align(4);
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        self.align(8);
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    fn string(&mut self, len: usize) -> Result<String, Error> {
        let s = String::from_utf8_lossy(self.bytes(len)?).into_owned();
        self.bytes(1)?;
        Ok(s)
    }

    fn read_all(&mut self, mut sig: &str) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        while !sig.is_empty() {
            let (t, rest) = split_type(sig)?;
            out.push(self.read(t)?);
            sig = rest;
        }
        Ok(out)
    }

    fn read(&mut self, t: &str) -> Result<Value, Error> {
        Ok(match t.as_bytes()[0] {
            b'y' => Value::Byte(self.bytes(1)?[0]),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' | b'q' => {
                self.align(2);
                Value::U32(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()) as u32)
            }
            b'i' => Value::I32(self.u32()? as i32),
            b'u' => Value::U32(self.u32()?),
            b'h' => Value::Fd(self.u32()?),
            b'x' => Value::I64(self.u64()? as i64),
            b't' => Value::U64(self.u64()?),
            b'd' => Value::Double(f64::from_bits(self.u64()?)),
            b's' | b'o' => {
                let len = self.u32()? as usize;
                let s = self.string(len)?;
                if t == "o" {
                    Value::Path(s)
                } else {
                    Value::Str(s)
                }
            }
            b'g' => {
                let len = self.bytes(1)?[0] as usize;
                Value::Sig(self.string(len)?)
            }
            b'v' => {
                let len = self.bytes(1)?[0] as usize;
                let sig = self.string(len)?;
                let (inner, rest) = split_type(&sig)?;
                if !rest.is_empty() {
                    return Err(Error::Protocol("variant with several types"));
                }
                Value::Variant(Box::new(self.read(inner)?))
            }
            b'a' => {
                let len = self.u32()? as usize;
                let elem = &t[1..];
                self.align(type_alignment(elem));
                let end = self.pos + len;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read(elem)?);
                }
                Value::Array(elem.to_string(), items)
            }
            b'(' => {
                self.align(8);
                Value::Struct(self.read_all(&t[1..t.len() - 1])?)
            }
            b'{' => {
                self.align(8);
                let mut kv = self.read_all(&t[1..t.len() - 1])?;
                if kv.len() != 2 {
                    return Err(Error::Protocol("bad dict entry"));
                }
                let v = kv.pop().unwrap();
                Value::DictEntry(Box::new(kv.pop().unwrap()), Box::new(v))
            }
            _ => return Err(Error::Protocol("unknown type")),
        })
    }
}

fn encode(msg: &Message, serial: u32) -> Vec<u8> {
    let body_sig: String = msg.body.iter().map(Value::signature).collect();
    let mut body = Writer { buf: Vec::new() };
    for v in &msg.body {
        body.put(v);
    }
    let mut fields = Vec::new();
    let mut field = |code: u8, v: Value| fields.push(Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))]));
    if let Some(p) = &msg.path {
        field(1, Value::Path(p.clone()));
    }
    if let Some(i) = &msg.interface {
        field(2, Value::Str(i.clone()));
    }
    if let Some(m) = &msg.member {
        field(3, Value::Str(m.clone()));
    }
    if let Some(e) = &msg.error_name {
        field(4, Value::Str(e.clone()));
    }
    if let Some(r) = msg.reply_serial {
        field(5, Value::U32(r));
    }
    if let Some(d) = &msg.destination {
        field(6, Value::Str(d.clone()));
    }
    if !body_sig.is_empty() {
        field(8, Value::Sig(body_sig));
    }
    let mut w = Writer { buf: vec![b'l', msg.kind, msg.flags, 1] };
    w.u32(body.buf.len() as u32);
    w.u32(serial);
    w.put(&Value::Array("(yv)".into(), fields));
    w.align(8);
    w.buf.extend_from_slice(&body.buf);
    w.buf
}

/// Length of the first complete message in `buf`, if there is one.
fn message_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 16 {
        return None;
    }
    let body = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let fields = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    let total = (16 + fields).div_ceil(8) * 8 + body;
    (buf.len() >= total).then_some(total)
}

fn decode(buf: &[u8]) -> Result<(Message, usize), Error> {
    if buf[0] != b'l' {
        return Err(Error::Protocol("big-endian messages are not supported"));
    }
    let mut r = Reader { buf, pos: 0 };
    let head = r.bytes(4)?;
    let mut m = Message { kind: head[1], flags: head[2], ..Default::default() };
    let _body_len = r.u32()?;
    m.serial = r.u32()?;
    let fields = r.read("a(yv)")?;
    let mut sig = String::new();
    let mut nfds = 0;
    if let Value::Array(_, items) = fields {
        for f in items {
            let Value::Struct(f) = f else { continue };
            let (Some(code), Some(v)) = (f.first().and_then(Value::as_u32), f.get(1)) else { continue };
            let s = v.as_str().map(String::from);
            match code {
                1 => m.path = s,
                2 => m.interface = s,
                3 => m.member = s,
                4 => m.error_name = s,
                5 => m.reply_serial = v.as_u32(),
                6 => m.destination = s,
                7 => m.sender = s,
                8 => sig = s.unwrap_or_default(),
                9 => nfds = v.as_u32().unwrap_or(0),
                _ => {}
            }
        }
    }
    r.align(8);
    m.body = r.read_all(&sig)?;
    Ok((m, nfds as usize))
}

pub struct Bus {
    conn: wire::Connection,
    serial: u32,
    queue: VecDeque<Message>,
}

impl Bus {
    pub fn system() -> Result<Bus, Error> {
        let addr = std::env::var("DBUS_SYSTEM_BUS_ADDRESS").unwrap_or_else(|_| "unix:path=/run/dbus/system_bus_socket".into());
        Bus::connect(&addr)
    }

    pub fn session() -> Result<Bus, Error> {
        let addr = match std::env::var("DBUS_SESSION_BUS_ADDRESS") {
            Ok(a) => a,
            Err(_) => format!("unix:path={}/bus", std::env::var("XDG_RUNTIME_DIR").map_err(|_| Error::Protocol("no session bus address"))?),
        };
        Bus::connect(&addr)
    }

    fn connect(address: &str) -> Result<Bus, Error> {
        let path = address
            .split(';')
            .find_map(|a| a.strip_prefix("unix:path=").map(|p| p.split(',').next().unwrap_or(p)))
            .ok_or(Error::Protocol("only unix:path= addresses are supported"))?;
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let uid = unsafe { libc::getuid() }.to_string();
        let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
        expect_line(&mut stream, "OK ")?;
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        expect_line(&mut stream, "AGREE_UNIX_FD")?;
        stream.write_all(b"BEGIN\r\n")?;
        stream.set_nonblocking(true)?;
        let mut bus = Bus { conn: wire::Connection::new(OwnedFd::from(stream)), serial: 0, queue: VecDeque::new() };
        bus.call(Message::call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", vec![]), Duration::from_secs(2))?;
        Ok(bus)
    }

    pub fn raw_fd(&self) -> RawFd {
        self.conn.raw_fd()
    }

    /// Queues a message; returns its serial.
    pub fn send(&mut self, msg: &Message) -> u32 {
        self.serial += 1;
        self.conn.send_raw(&encode(msg, self.serial), Vec::new());
        self.serial
    }

    pub fn flush(&mut self) -> io::Result<bool> {
        self.conn.flush()
    }

    /// Calls a method and waits at most `timeout` for the reply. Signals
    /// that arrive meanwhile are queued for `pop`.
    pub fn call(&mut self, msg: Message, timeout: Duration) -> Result<Message, Error> {
        let serial = self.send(&msg);
        let deadline = Instant::now() + timeout;
        loop {
            self.flush()?;
            self.read()?;
            if let Some(i) = self.queue.iter().position(|m| m.reply_serial == Some(serial)) {
                let reply = self.queue.remove(i).unwrap();
                if reply.kind == ERROR {
                    let text = reply.body.first().and_then(Value::as_str).unwrap_or("").to_string();
                    return Err(Error::Remote(reply.error_name.unwrap_or_default(), text));
                }
                return Ok(reply);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::Timeout);
            }
            let mut pfd = libc::pollfd { fd: self.conn.raw_fd(), events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        }
    }

    /// Reads what is available into the queue.
    pub fn read(&mut self) -> Result<(), Error> {
        loop {
            match self.conn.fill() {
                Ok(0) => return Err(Error::Protocol("bus closed the connection")),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        while let Some(len) = message_len(self.conn.input()) {
            let (mut m, nfds) = decode(&self.conn.input()[..len])?;
            self.conn.consume(len);
            for _ in 0..nfds {
                let fd = self.conn.take_fd();
                m.fds.push(fd);
            }
            self.queue.push_back(m);
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Message> {
        self.queue.pop_front()
    }

    /// Asks for a well-known name; true when we are its primary owner.
    /// `flags`: 1 allow replacement, 2 replace existing, 4 do not queue.
    pub fn request_name(&mut self, name: &str, flags: u32) -> Result<bool, Error> {
        let m = Message::call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName", vec![Value::Str(name.into()), Value::U32(flags)]);
        let reply = self.call(m, Duration::from_secs(1))?;
        // 1 primary owner, 4 already the owner.
        Ok(matches!(reply.body.first().and_then(Value::as_u32), Some(1 | 4)))
    }

    pub fn add_match(&mut self, rule: &str) -> Result<(), Error> {
        let m = Message::call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", vec![Value::Str(rule.into())]);
        self.call(m, Duration::from_secs(1)).map(|_| ())
    }
}

fn expect_line(stream: &mut UnixStream, prefix: &str) -> Result<(), Error> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if stream.read(&mut b)? == 0 {
            return Err(Error::Protocol("bus closed during authentication"));
        }
        line.push(b[0]);
    }
    if line.starts_with(prefix.as_bytes()) {
        Ok(())
    } else {
        Err(Error::Protocol("authentication rejected"))
    }
}

impl AsRawFd for Bus {
    fn as_raw_fd(&self) -> RawFd {
        self.conn.raw_fd()
    }
}

/// Queued messages are not kept: the session is quiet while upgrading.
impl state::Persist for Bus {
    fn save(&self, e: &mut state::Enc) {
        self.conn.save(e);
        self.serial.save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        Ok(Bus { conn: Persist::load(d)?, serial: Persist::load(d)?, queue: VecDeque::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut m = Message::call("a.b", "/a/b", "a.b.C", "M", vec![
            Value::Str("hello".into()),
            Value::U32(7),
            Value::Array("{sv}".into(), vec![Value::DictEntry(Box::new(Value::Str("k".into())), Box::new(Value::Variant(Box::new(Value::Bool(true)))))]),
        ]);
        m.serial = 0;
        let bytes = encode(&m, 42);
        assert_eq!(message_len(&bytes), Some(bytes.len()));
        let (d, _) = decode(&bytes).unwrap();
        assert_eq!(d.serial, 42);
        assert_eq!(d.member.as_deref(), Some("M"));
        assert_eq!(d.body, m.body);
    }

    #[test]
    fn signatures() {
        assert_eq!(split_type("a{sv}u").unwrap(), ("a{sv}", "u"));
        assert_eq!(split_type("(ua(yv))s").unwrap(), ("(ua(yv))", "s"));
    }
}
