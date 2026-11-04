//! Minimal D-Bus client: enough to read properties and receive signals.
//!
//! Only the subset the bar needs, so there is no dependency on a D-Bus crate:
//! SASL EXTERNAL auth, method calls with a `s`/`ss` signature, replies holding
//! a variant, and match rules for push notifications (ADR-015).
//!
//! Wire format: a fixed 16-byte header, an array of header fields, then the
//! body. Everything is aligned to its natural size, which is what most of the
//! fiddly code below is about.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const LITTLE_ENDIAN: u8 = b'l';
const PROTOCOL_VERSION: u8 = 1;

// Message types.
const MSG_METHOD_CALL: u8 = 1;
const MSG_METHOD_RETURN: u8 = 2;
const MSG_ERROR: u8 = 3;
const MSG_SIGNAL: u8 = 4;

// Header field codes.
const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_DESTINATION: u8 = 6;
const FIELD_SIGNATURE: u8 = 8;

/// Appends padding so that `buf` is aligned to `align` bytes.
fn pad_to(buf: &mut Vec<u8>, align: usize) {
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    pad_to(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

/// A string: u32 length, bytes, NUL.
fn put_string(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// A signature: u8 length, bytes, NUL.
fn put_signature(buf: &mut Vec<u8>, s: &str) {
    buf.push(s.len() as u8);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

/// One header field: a struct of (byte code, variant value).
fn put_header_field(buf: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
    pad_to(buf, 8); // struct alignment
    buf.push(code);
    put_signature(buf, sig);
    if sig == "o" || sig == "s" {
        put_string(buf, value);
    } else if sig == "g" {
        put_signature(buf, value);
    }
}

/// Reads primitives out of a message body.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn align(&mut self, align: usize) {
        while !self.pos.is_multiple_of(align) && self.pos < self.buf.len() {
            self.pos += 1;
        }
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let bytes = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    pub fn u16(&mut self) -> Option<u16> {
        self.align(2);
        let bytes = self.buf.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(u16::from_le_bytes(bytes.try_into().ok()?))
    }

    pub fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u32()? != 0)
    }

    pub fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len + 1; // skip NUL
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    pub fn signature(&mut self) -> Option<String> {
        let len = self.byte()? as usize;
        let bytes = self.buf.get(self.pos..self.pos + len)?;
        self.pos += len + 1;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Reads a variant, returning it as a display string for the scalar types
    /// this crate cares about (boolean, string, byte, integers).
    pub fn variant(&mut self) -> Option<Value> {
        let sig = self.signature()?;
        match sig.as_str() {
            "b" => Some(Value::Bool(self.bool()?)),
            "s" | "o" => Some(Value::Str(self.string()?)),
            "y" => Some(Value::Byte(self.byte()?)),
            "q" => Some(Value::Int(self.u16()? as i64)),
            "u" | "i" => Some(Value::Int(self.u32()? as i64)),
            "x" | "t" => Some(Value::Int(self.u64()? as i64)),
            "d" => Some(Value::Int(self.u64()? as i64)), // doubles are ignored
            // An array of strings collapses to its first element: the callers
            // that meet one (MPRIS `xesam:artist`) want a single name, not a
            // list, and joining here would hide which artist came first.
            "as" => {
                let items = self.string_array()?;
                items.into_iter().next().map(Value::Str)
            }
            _ => None, // other container types are not needed here
        }
    }

    pub fn u64(&mut self) -> Option<u64> {
        self.align(8);
        let bytes = self.buf.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// Reads an array of strings: a byte length, then that many bytes of
    /// string data. The length counts the *contents*, not the elements.
    pub fn string_array(&mut self) -> Option<Vec<String>> {
        let len = self.u32()? as usize;
        let end = self.pos + len;
        let mut out = Vec::new();
        while self.pos < end && self.pos < self.buf.len() {
            out.push(self.string()?);
        }
        Some(out)
    }

    /// Reads a dictionary of string keys to variant values, the shape MPRIS
    /// uses for track metadata.
    ///
    /// Entries whose value is a container this reader cannot decode are
    /// skipped rather than aborting the whole dictionary: `mpris:artUrl` and
    /// friends sit between the fields that matter, and one unknown type must
    /// not cost the rest.
    pub fn dict(&mut self) -> Option<Vec<(String, Value)>> {
        let len = self.u32()? as usize;
        // Array contents start on an 8-byte boundary for struct elements,
        // which a dict entry is.
        self.align(8);
        let end = self.pos + len;
        let mut out = Vec::new();
        while self.pos < end && self.pos < self.buf.len() {
            self.align(8);
            let Some(key) = self.string() else { break };
            match self.variant() {
                Some(value) => out.push((key, value)),
                // Unknown type: without its length there is no way to step
                // over it, so the rest of the dictionary is lost. Callers get
                // whatever was read before that point.
                None => break,
            }
        }
        Some(out)
    }
}

/// A scalar D-Bus value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Str(String),
    Byte(u8),
    Int(i64),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Used by the battery-percentage properties (bt-headphones widget).
    #[allow(dead_code)]
    pub fn as_u8(&self) -> Option<u8> {
        match self {
            Value::Byte(b) => Some(*b),
            Value::Int(i) => u8::try_from(*i).ok(),
            _ => None,
        }
    }
}

/// A parsed incoming message.
pub struct Message {
    pub msg_type: u8,
    /// Signal name, for callers that need to tell signals apart.
    #[allow(dead_code)]
    pub member: Option<String>,
    pub body: Vec<u8>,
}

/// A connection to a D-Bus daemon.
pub struct DBus {
    stream: UnixStream,
    serial: u32,
    /// Set once the peer closed. A dead connection is not the same as a quiet
    /// one: its descriptor stays readable for ever in level-triggered epoll,
    /// so the caller must unregister it rather than keep polling (ADR-049).
    closed: bool,
}

impl DBus {
    /// Connects to the system bus and completes SASL EXTERNAL auth.
    pub fn system() -> Option<Self> {
        let path = std::env::var("DBUS_SYSTEM_BUS_ADDRESS")
            .ok()
            .and_then(|a| a.strip_prefix("unix:path=").map(String::from))
            .unwrap_or_else(|| "/run/dbus/system_bus_socket".to_string());
        Self::connect(&path)
    }

    /// Connects to the session bus, where desktop services like nwg-notifications live.
    pub fn session() -> Option<Self> {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().or_else(|| {
            let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
            Some(format!("unix:path={}/bus", runtime))
        })?;
        // The address can list several transports; take the first unix path.
        let path = address
            .split(';')
            .find_map(|part| {
                part.split(',')
                    .find_map(|kv| kv.trim().strip_prefix("unix:path=").or_else(|| kv.trim().strip_prefix("path=")))
            })?
            .to_string();
        Self::connect(&path)
    }

    fn connect(path: &str) -> Option<Self> {
        let mut stream = UnixStream::connect(path).ok()?;

        // AUTH EXTERNAL with the hex-encoded uid, then BEGIN.
        let uid = unsafe { getuid() };
        let hex: String = uid
            .to_string()
            .bytes()
            .map(|b| format!("{:02x}", b))
            .collect();
        stream
            .write_all(format!("\0AUTH EXTERNAL {}\r\n", hex).as_bytes())
            .ok()?;
        let mut reply = [0u8; 256];
        let n = stream.read(&mut reply).ok()?;
        if !reply[..n].starts_with(b"OK") {
            return None;
        }
        stream.write_all(b"BEGIN\r\n").ok()?;

        let mut bus = DBus { stream, serial: 0, closed: false };
        // Hello() is mandatory before any other call.
        bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        Some(bus)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// Sends a method call whose arguments are all strings, returning the reply
    /// body. None on an error reply.
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[&str],
    ) -> Option<Vec<u8>> {
        let mut body = Vec::new();
        for arg in args {
            put_string(&mut body, arg);
        }
        let signature: String = "s".repeat(args.len());
        self.call_raw(destination, path, interface, member, &signature, body)
    }

    /// Same, for a call whose body is already encoded. Needed by the few
    /// methods that take something other than plain strings.
    fn call_raw(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: Vec<u8>,
    ) -> Option<Vec<u8>> {
        let serial = self.next_serial();

        let mut fields = Vec::new();
        put_header_field(&mut fields, FIELD_PATH, "o", path);
        put_header_field(&mut fields, FIELD_DESTINATION, "s", destination);
        put_header_field(&mut fields, FIELD_INTERFACE, "s", interface);
        put_header_field(&mut fields, FIELD_MEMBER, "s", member);
        if !signature.is_empty() {
            put_header_field(&mut fields, FIELD_SIGNATURE, "g", signature);
        }

        let mut msg = vec![LITTLE_ENDIAN, MSG_METHOD_CALL, 0 /* flags */, PROTOCOL_VERSION];
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8); // body starts on an 8-byte boundary
        msg.extend_from_slice(&body);

        self.stream.write_all(&msg).ok()?;
        self.stream.flush().ok()?;

        // Skip signals until the matching reply arrives.
        loop {
            let msg = self.read_message()?;
            match msg.msg_type {
                MSG_METHOD_RETURN => return Some(msg.body),
                MSG_ERROR => return None,
                _ => continue,
            }
        }
    }

    /// Reads and parses one message.
    fn read_message(&mut self) -> Option<Message> {
        let mut header = [0u8; 16];
        self.stream.read_exact(&mut header).ok()?;
        if header[0] != LITTLE_ENDIAN {
            return None;
        }
        let msg_type = header[1];
        let body_len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        let fields_len = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
        if body_len > 1 << 24 || fields_len > 1 << 16 {
            return None;
        }

        let mut fields = vec![0u8; fields_len];
        self.stream.read_exact(&mut fields).ok()?;
        // Header fields are padded to an 8-byte boundary before the body.
        let padding = (8 - (fields_len % 8)) % 8;
        if padding > 0 {
            let mut skip = vec![0u8; padding];
            self.stream.read_exact(&mut skip).ok()?;
        }
        let mut body = vec![0u8; body_len];
        self.stream.read_exact(&mut body).ok()?;

        Some(Message {
            msg_type,
            member: parse_member(&fields),
            body,
        })
    }

    /// Adds a match rule so the daemon forwards matching signals.
    pub fn add_match(&mut self, rule: &str) -> Option<()> {
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[rule],
        )
        .map(|_| ())
    }

    /// Turns the connection into a monitor, receiving a copy of every message
    /// matching `rules` even when addressed to someone else.
    ///
    /// This replaces the `eavesdrop=true` match flag, which dbus-daemon has
    /// ignored since 1.10 — `AddMatch` still succeeds, it just never delivers
    /// anything. A monitor connection is receive-only: the bus rejects any
    /// further method call on it, so this must be the last thing asked of it.
    pub fn become_monitor(&mut self, rules: &[&str]) -> Option<()> {
        // Signature "asu": an array of match rules, then a flags word.
        let mut body = Vec::new();
        let mut array = Vec::new();
        for rule in rules {
            put_string(&mut array, rule);
        }
        put_u32(&mut body, array.len() as u32);
        body.extend_from_slice(&array);
        put_u32(&mut body, 0); // flags, reserved

        self.call_raw(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Monitoring",
            "BecomeMonitor",
            "asu",
            body,
        )
        .map(|_| ())
    }

    /// Calls a method taking no arguments and returning a single u32.
    pub fn call_u32(&mut self, destination: &str, path: &str, interface: &str, member: &str) -> Option<u32> {
        let body = self.call(destination, path, interface, member, &[])?;
        Reader::new(&body).u32()
    }

    /// Reads a property via org.freedesktop.DBus.Properties.Get.
    pub fn get_property(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        property: &str,
    ) -> Option<Value> {
        let body = self.call(
            destination,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            &[interface, property],
        )?;
        Reader::new(&body).variant()
    }

    /// Same, but hands back the raw reply body.
    ///
    /// `get_property` decodes the variant into a scalar, which loses container
    /// types — and MPRIS metadata is a dictionary. The caller reads it with its
    /// own `Reader` instead.
    pub fn get_property_raw(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        property: &str,
    ) -> Option<Vec<u8>> {
        self.call(
            destination,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            &[interface, property],
        )
    }

    /// Drains pending messages, returning them. Unlike `drain_signals` this
    /// keeps the bodies, which is what the notification watcher needs: the app
    /// name lives in the first argument of each `Notify` call.
    pub fn drain_messages(&mut self) -> Vec<Message> {
        let mut out = Vec::new();
        self.stream.set_nonblocking(true).ok();
        loop {
            let mut header = [0u8; 16];
            match self.stream.read_exact(&mut header) {
                Ok(()) => {
                    // The header arrived, so the rest is on its way: finish it
                    // blocking rather than losing sync with the stream.
                    self.stream.set_nonblocking(false).ok();
                    let msg = self.read_body(&header);
                    self.stream.set_nonblocking(true).ok();
                    match msg {
                        Some(m) => out.push(m),
                        None => break,
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        self.stream.set_nonblocking(false).ok();
        out
    }

    /// Reads the fields and body of a message whose header was consumed.
    fn read_body(&mut self, header: &[u8; 16]) -> Option<Message> {
        let body_len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        let fields_len = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
        if body_len > 1 << 24 || fields_len > 1 << 16 {
            return None;
        }
        let mut fields = vec![0u8; fields_len];
        self.stream.read_exact(&mut fields).ok()?;
        let padding = (8 - (fields_len % 8)) % 8;
        if padding > 0 {
            let mut skip = vec![0u8; padding];
            self.stream.read_exact(&mut skip).ok()?;
        }
        let mut body = vec![0u8; body_len];
        self.stream.read_exact(&mut body).ok()?;
        Some(Message {
            msg_type: header[1],
            member: parse_member(&fields),
            body,
        })
    }

    /// Drains pending signal messages; true if any arrived.
    pub fn drain_signals(&mut self) -> bool {
        let mut saw = false;
        self.stream.set_nonblocking(true).ok();
        loop {
            let mut peek = [0u8; 16];
            match self.stream.read_exact(&mut peek) {
                Ok(()) => {
                    // Header read: the rest is arriving, so finish it blocking.
                    self.stream.set_nonblocking(false).ok();
                    let ok = self.finish_message(&peek).is_some();
                    self.stream.set_nonblocking(true).ok();
                    if !ok {
                        break;
                    }
                    if peek[1] == MSG_SIGNAL {
                        saw = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // UnexpectedEof, or anything a retry will not fix: the bus is
                // gone.
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        self.stream.set_nonblocking(false).ok();
        saw
    }

    /// Reads the remainder of a message whose header has already been consumed.
    fn finish_message(&mut self, header: &[u8; 16]) -> Option<()> {
        let body_len = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
        let fields_len = u32::from_le_bytes(header[12..16].try_into().ok()?) as usize;
        if body_len > 1 << 24 || fields_len > 1 << 16 {
            return None;
        }
        let padding = (8 - (fields_len % 8)) % 8;
        let mut rest = vec![0u8; fields_len + padding + body_len];
        self.stream.read_exact(&mut rest).ok()?;
        Some(())
    }

    /// Whether the peer has closed this connection.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

extern "C" {
    fn getuid() -> u32;
}

/// Extracts the MEMBER header field, used to tell signals apart.
fn parse_member(fields: &[u8]) -> Option<String> {
    let mut r = Reader::new(fields);
    while r.pos < fields.len() {
        r.align(8);
        let code = r.byte()?;
        let sig = r.signature()?;
        let value = match sig.as_str() {
            "s" | "o" => r.string()?,
            "g" => r.signature()?,
            "u" => {
                r.u32()?;
                continue;
            }
            _ => return None,
        };
        if code == FIELD_MEMBER {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_and_encodes_strings() {
        let mut buf = Vec::new();
        put_string(&mut buf, "abc");
        // u32 length, bytes, NUL.
        assert_eq!(buf, vec![3, 0, 0, 0, b'a', b'b', b'c', 0]);

        let mut buf = vec![0u8]; // force misalignment
        put_u32(&mut buf, 1);
        assert_eq!(buf.len(), 8, "u32 must start on a 4-byte boundary");
    }

    #[test]
    fn reads_back_what_it_writes() {
        let mut buf = Vec::new();
        put_string(&mut buf, "org.bluez.Adapter1");
        put_string(&mut buf, "Powered");
        let mut r = Reader::new(&buf);
        assert_eq!(r.string().as_deref(), Some("org.bluez.Adapter1"));
        assert_eq!(r.string().as_deref(), Some("Powered"));
        assert_eq!(r.string(), None, "past the end");
    }

    #[test]
    fn parses_variants() {
        // variant holding a boolean true
        let mut buf = Vec::new();
        put_signature(&mut buf, "b");
        put_u32(&mut buf, 1);
        assert_eq!(Reader::new(&buf).variant(), Some(Value::Bool(true)));

        // variant holding a byte
        let mut buf = Vec::new();
        put_signature(&mut buf, "y");
        buf.push(85);
        assert_eq!(Reader::new(&buf).variant(), Some(Value::Byte(85)));
        assert_eq!(Value::Byte(85).as_u8(), Some(85));

        // variant holding a string
        let mut buf = Vec::new();
        put_signature(&mut buf, "s");
        put_string(&mut buf, "hello");
        assert_eq!(Reader::new(&buf).variant(), Some(Value::Str("hello".into())));
    }

    #[test]
    fn truncated_input_yields_none() {
        assert_eq!(Reader::new(&[]).variant(), None);
        assert_eq!(Reader::new(&[1, b'b']).variant(), None);
    }

    /// Live check against the system bus, if reachable.
    #[test]
    fn reads_a_real_property() {
        let Some(mut bus) = DBus::system() else {
            return;
        };
        // BlueZ may be absent; only assert when it answers.
        if let Some(v) = bus.get_property("org.bluez", "/org/bluez/hci0", "org.bluez.Adapter1", "Powered") {
            assert!(v.as_bool().is_some(), "Powered must be a boolean, got {v:?}");
        }
    }
}
