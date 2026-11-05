//! PulseAudio native protocol over the unix socket, enough to read the default
//! sink and source and to subscribe to change events.
//!
//! Speaking the protocol directly replaces `pactl`: no fork per tick, and the
//! subscription gives push updates instead of polling (ADR-015).
//!
//! Wire format: each message is a 20-byte frame header (length, channel,
//! offset hi/lo, flags) followed by a tagstruct — a sequence of typed fields,
//! each introduced by a one-byte tag.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

pub const PROTOCOL_VERSION: u32 = 35;

// Commands used here (pulsecore/native-common.h ordering).
const CMD_ERROR: u32 = 0;
const CMD_REPLY: u32 = 2;
const CMD_AUTH: u32 = 8;
const CMD_SET_CLIENT_NAME: u32 = 9;
const CMD_GET_SERVER_INFO: u32 = 20;
const CMD_GET_SINK_INFO: u32 = 21;
const CMD_GET_SINK_INFO_LIST: u32 = 22;
const CMD_SET_DEFAULT_SINK: u32 = 44;
const CMD_GET_SOURCE_INFO: u32 = 23;
const CMD_SUBSCRIBE: u32 = 35;
/// Sets every channel of a sink: index, name, cvolume.
const CMD_SET_SINK_VOLUME: u32 = 36;
/// Mutes or unmutes a source. Takes index, name, then the desired state — the
/// server has no "toggle", so the caller reads the state first and sends its
/// opposite.
const CMD_SET_SOURCE_MUTE: u32 = 40;
/// Same, for the output. One opcode below the source's, as the command
/// enumeration pairs them.
const CMD_SET_SINK_MUTE: u32 = 39;
pub const CMD_SUBSCRIBE_EVENT: u32 = 66;

/// Subscription mask: sinks, sources and the server (default device changes).
const MASK_SINK: u32 = 0x0001;
const MASK_SOURCE: u32 = 0x0002;
const MASK_SERVER: u32 = 0x0020;

const INVALID_INDEX: u32 = 0xffff_ffff;
const NORM_VOLUME: u32 = 0x10000; // PA_VOLUME_NORM: 100%

// Tagstruct tags.
const TAG_U32: u8 = b'L';
const TAG_STRING: u8 = b't';
const TAG_STRING_NULL: u8 = b'N';
const TAG_ARBITRARY: u8 = b'x';
const TAG_BOOL_TRUE: u8 = b'1';
const TAG_BOOL_FALSE: u8 = b'0';
const TAG_SAMPLE_SPEC: u8 = b'a';
const TAG_CHANNEL_MAP: u8 = b'm';
const TAG_CVOLUME: u8 = b'v';
const TAG_PROPLIST: u8 = b'P';
const TAG_U8: u8 = b'B';
const TAG_U64: u8 = b'R';
const TAG_S64: u8 = b'r';
const TAG_USEC: u8 = b'U';
const TAG_VOLUME: u8 = b'V';
const TAG_FORMAT_INFO: u8 = b'f';
const TAG_TIMEVAL: u8 = b'T';

/// Builds a tagstruct payload.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u32(mut self, v: u32) -> Self {
        self.0.push(TAG_U32);
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn arbitrary(mut self, b: &[u8]) -> Self {
        self.0.push(TAG_ARBITRARY);
        self.0.extend_from_slice(&(b.len() as u32).to_be_bytes());
        self.0.extend_from_slice(b);
        self
    }
    fn string(mut self, s: &str) -> Self {
        self.0.push(TAG_STRING);
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        self
    }
    fn bool(mut self, v: bool) -> Self {
        self.0.push(if v { TAG_BOOL_TRUE } else { TAG_BOOL_FALSE });
        self
    }
    fn null_string(mut self) -> Self {
        self.0.push(TAG_STRING_NULL);
        self
    }
    /// A proplist with a single application.name entry.
    fn client_proplist(mut self, name: &str) -> Self {
        self.0.push(TAG_PROPLIST);
        self = self.string("application.name");
        let value = format!("{}\0", name);
        self = self.u32(value.len() as u32).arbitrary(value.as_bytes());
        self.0.push(TAG_STRING_NULL);
        self
    }
    fn finish(self) -> Vec<u8> {
        self.0
    }
}

/// Reads typed fields out of a tagstruct.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn tag(&mut self) -> Option<u8> {
        let t = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn u32(&mut self) -> Option<u32> {
        if self.tag()? != TAG_U32 {
            return None;
        }
        let bytes = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    fn string(&mut self) -> Option<Option<String>> {
        match self.tag()? {
            TAG_STRING_NULL => Some(None),
            TAG_STRING => {
                let end = self.buf[self.pos..].iter().position(|&b| b == 0)? + self.pos;
                let s = String::from_utf8_lossy(&self.buf[self.pos..end]).into_owned();
                self.pos = end + 1;
                Some(Some(s))
            }
            _ => None,
        }
    }

    /// Skips a sample_spec: format u8, channels u8, rate u32.
    fn skip_sample_spec(&mut self) -> Option<()> {
        (self.tag()? == TAG_SAMPLE_SPEC).then_some(())?;
        self.pos += 6;
        (self.pos <= self.buf.len()).then_some(())
    }

    /// Skips a channel map: count u8 followed by that many channel positions.
    fn skip_channel_map(&mut self) -> Option<()> {
        (self.tag()? == TAG_CHANNEL_MAP).then_some(())?;
        let n = *self.buf.get(self.pos)? as usize;
        self.pos += 1 + n;
        (self.pos <= self.buf.len()).then_some(())
    }

    /// Reads a cvolume, returning the loudest channel.
    fn cvolume_max(&mut self) -> Option<u32> {
        (self.tag()? == TAG_CVOLUME).then_some(())?;
        let n = *self.buf.get(self.pos)? as usize;
        self.pos += 1;
        let mut max = 0;
        for _ in 0..n {
            let bytes = self.buf.get(self.pos..self.pos + 4)?;
            self.pos += 4;
            max = max.max(u32::from_be_bytes(bytes.try_into().ok()?));
        }
        Some(max)
    }

    /// Skips one value of any type: enough to walk records whose layout
    /// only matters in part.
    fn skip(&mut self) -> Option<()> {
        let tag = self.tag()?;
        let fixed = match tag {
            TAG_U32 | TAG_VOLUME => 4,
            TAG_U8 => 1,
            TAG_U64 | TAG_S64 | TAG_USEC | TAG_TIMEVAL => 8,
            TAG_BOOL_TRUE | TAG_BOOL_FALSE | TAG_STRING_NULL => 0,
            TAG_SAMPLE_SPEC => 6,
            TAG_STRING => {
                self.pos = self.buf[self.pos..].iter().position(|&b| b == 0)? + self.pos + 1;
                return Some(());
            }
            TAG_ARBITRARY => {
                let n = u32::from_be_bytes(self.buf.get(self.pos..self.pos + 4)?.try_into().ok()?) as usize;
                4 + n
            }
            TAG_CHANNEL_MAP => 1 + *self.buf.get(self.pos)? as usize,
            TAG_CVOLUME => 1 + 4 * *self.buf.get(self.pos)? as usize,
            TAG_PROPLIST => {
                // key, length, value; a null string ends it.
                loop {
                    if *self.buf.get(self.pos)? == TAG_STRING_NULL {
                        self.pos += 1;
                        return Some(());
                    }
                    self.skip()?;
                    self.skip()?;
                    self.skip()?;
                }
            }
            TAG_FORMAT_INFO => {
                self.skip()?;
                return self.skip();
            }
            _ => return None,
        };
        self.pos += fixed;
        (self.pos <= self.buf.len()).then_some(())
    }

    fn bool(&mut self) -> Option<bool> {
        match self.tag()? {
            TAG_BOOL_TRUE => Some(true),
            TAG_BOOL_FALSE => Some(false),
            _ => None,
        }
    }
}

/// Volume percentage and mute state of a device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceState {
    pub volume_percent: u64,
    pub muted: bool,
    /// The device's own index. Writes have to name their target: unlike the
    /// info queries, the server rejects `INVALID_INDEX` plus a null name with
    /// PA_ERR_INVALID rather than resolving it to the default device.
    pub index: u32,
    /// Channels, for writing a volume back.
    pub channels: u8,
}

/// Raw volume -> percentage, rounded as `pactl` prints it.
fn to_percent(raw: u32) -> u64 {
    ((raw as f64 / NORM_VOLUME as f64) * 100.0).round() as u64
}

/// Parses the leading fields of a sink/source info reply.
///
/// Layout: index, name, description, sample_spec, channel_map, owner_module,
/// cvolume, mute. The owner_module field between the channel map and the
/// volume is easy to miss and shifts everything after it.
fn parse_device_info(body: &[u8]) -> Option<DeviceState> {
    let mut r = Reader::new(body);
    r.u32()?; // command
    r.u32()?; // tag
    let index = r.u32()?;
    r.string()?; // name
    r.string()?; // description
    r.skip_sample_spec()?;
    r.skip_channel_map()?;
    r.u32()?; // owner_module
    let channels = *body.get(r.pos + 1)?;
    let volume = r.cvolume_max()?;
    let muted = r.bool()?;
    Some(DeviceState {
        volume_percent: to_percent(volume),
        muted,
        index,
        channels,
    })
}

/// A connection to the PulseAudio server.
pub struct Pulse {
    stream: UnixStream,
    tag: u32,
    /// Set once the server closed. A dead descriptor stays readable for ever
    /// under level-triggered epoll (ADR-049).
    closed: bool,
}

impl Pulse {
    /// Connects, authenticates with the cookie and registers a client name.
    pub fn connect() -> Option<Self> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let socket_path = format!("{}/pulse/native", runtime);
        let stream = UnixStream::connect(socket_path).ok()?;
        let mut pulse = Pulse { stream, tag: 0, closed: false };

        let cookie = std::env::var("HOME")
            .ok()
            .and_then(|home| std::fs::read(format!("{}/.config/pulse/cookie", home)).ok())
            .unwrap_or_default();

        let auth = Writer::default()
            .u32(CMD_AUTH)
            .u32(pulse.next_tag())
            .u32(PROTOCOL_VERSION)
            .arbitrary(&cookie)
            .finish();
        pulse.request(&auth)?;

        let name = Writer::default()
            .u32(CMD_SET_CLIENT_NAME)
            .u32(pulse.next_tag())
            .client_proplist("davide-wm")
            .finish();
        pulse.request(&name)?;
        Some(pulse)
    }

    fn next_tag(&mut self) -> u32 {
        self.tag += 1;
        self.tag
    }

    /// Frames and sends a payload.
    fn send(&mut self, payload: &[u8]) -> Option<()> {
        let mut frame = Vec::with_capacity(20 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&INVALID_INDEX.to_be_bytes()); // channel
        frame.extend_from_slice(&[0u8; 12]); // offset hi/lo, flags
        frame.extend_from_slice(payload);
        self.stream.write_all(&frame).ok()?;
        self.stream.flush().ok()
    }

    /// Reads one framed message body.
    fn recv(&mut self) -> Option<Vec<u8>> {
        let mut header = [0u8; 20];
        self.stream.read_exact(&mut header).ok()?;
        let len = u32::from_be_bytes(header[..4].try_into().ok()?) as usize;
        if len > 1 << 20 {
            return None;
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).ok()?;
        Some(body)
    }

    /// Sends a request and returns the reply body, or None on an error reply.
    fn request(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        self.send(payload)?;
        loop {
            let body = self.recv()?;
            let mut r = Reader::new(&body);
            match r.u32()? {
                CMD_REPLY => return Some(body),
                CMD_ERROR => return None,
                // Event frames can arrive between request and reply.
                _ => continue,
            }
        }
    }

    /// State of the default sink (index INVALID + NULL name means "default";
    /// PipeWire rejects an explicit name here).
    pub fn default_sink(&mut self) -> Option<DeviceState> {
        let tag = self.next_tag();
        let req = Writer::default()
            .u32(CMD_GET_SINK_INFO)
            .u32(tag)
            .u32(INVALID_INDEX)
            .null_string()
            .finish();
        parse_device_info(&self.request(&req)?)
    }

    /// State of the default source.
    pub fn default_source(&mut self) -> Option<DeviceState> {
        let tag = self.next_tag();
        let req = Writer::default()
            .u32(CMD_GET_SOURCE_INFO)
            .u32(tag)
            .u32(INVALID_INDEX)
            .null_string()
            .finish();
        parse_device_info(&self.request(&req)?)
    }

    /// Mutes or unmutes the source at `index`.
    ///
    /// The index has to be explicit. Reading accepts `INVALID_INDEX` with a
    /// null name and resolves it to the default device, but writing does not:
    /// the server answers PA_ERR_INVALID. So the caller queries the source
    /// first and passes the index that came back.
    pub fn set_source_mute(&mut self, index: u32, muted: bool) -> Option<()> {
        let tag = self.next_tag();
        let req = Writer::default()
            .u32(CMD_SET_SOURCE_MUTE)
            .u32(tag)
            .u32(index)
            .null_string()
            .bool(muted)
            .finish();
        self.request(&req).map(|_| ())
    }

    /// Sets every channel of the sink at `index` to `percent`.
    pub fn set_sink_volume(&mut self, index: u32, channels: u8, percent: u64) -> Option<()> {
        let tag = self.next_tag();
        let raw = ((percent * NORM_VOLUME as u64 + 50) / 100) as u32;
        let mut req = Writer::default().u32(CMD_SET_SINK_VOLUME).u32(tag).u32(index).null_string().finish();
        req.push(TAG_CVOLUME);
        req.push(channels.max(1));
        for _ in 0..channels.max(1) {
            req.extend_from_slice(&raw.to_be_bytes());
        }
        self.request(&req).map(|_| ())
    }

    /// Every sink: (name, description), in the server's order.
    pub fn sinks(&mut self) -> Option<Vec<(String, String)>> {
        let tag = self.next_tag();
        let body = self.request(&Writer::default().u32(CMD_GET_SINK_INFO_LIST).u32(tag).finish())?;
        let mut r = Reader::new(&body);
        r.u32()?;
        r.u32()?;
        let mut out = Vec::new();
        while r.pos < body.len() {
            r.u32()?; // index
            let name = r.string()?.unwrap_or_default();
            let description = r.string()?.unwrap_or_default();
            // sample spec, channel map, owner module, volume, mute, monitor
            // source and its name, latency, driver, flags, proplist,
            // configured latency, base volume, state, volume steps, card.
            for _ in 0..16 {
                r.skip()?;
            }
            let ports = r.u32()?;
            for _ in 0..ports {
                // name, description, priority, available, group, type
                for _ in 0..6 {
                    r.skip()?;
                }
            }
            r.skip()?; // active port
            let formats = {
                (r.tag()? == TAG_U8).then_some(())?;
                let n = *body.get(r.pos)?;
                r.pos += 1;
                n
            };
            for _ in 0..formats {
                r.skip()?;
            }
            out.push((name, description));
        }
        Some(out)
    }

    /// The default sink's name.
    pub fn default_sink_name(&mut self) -> Option<String> {
        let tag = self.next_tag();
        let body = self.request(&Writer::default().u32(CMD_GET_SERVER_INFO).u32(tag).finish())?;
        let mut r = Reader::new(&body);
        r.u32()?;
        r.u32()?;
        for _ in 0..5 {
            r.skip()?;
        }
        r.string()?
    }

    pub fn set_default_sink(&mut self, name: &str) -> Option<()> {
        let tag = self.next_tag();
        self.request(&Writer::default().u32(CMD_SET_DEFAULT_SINK).u32(tag).string(name).finish()).map(|_| ())
    }

    /// Mutes or unmutes the sink at `index`.
    ///
    /// Like the source, the index must be explicit: writes are not resolved
    /// against the default device the way reads are.
    pub fn set_sink_mute(&mut self, index: u32, muted: bool) -> Option<()> {
        let tag = self.next_tag();
        let req = Writer::default()
            .u32(CMD_SET_SINK_MUTE)
            .u32(tag)
            .u32(index)
            .null_string()
            .bool(muted)
            .finish();
        self.request(&req).map(|_| ())
    }

    /// Subscribes to sink, source and server events.
    pub fn subscribe(&mut self) -> Option<()> {
        let tag = self.next_tag();
        let req = Writer::default()
            .u32(CMD_SUBSCRIBE)
            .u32(tag)
            .u32(MASK_SINK | MASK_SOURCE | MASK_SERVER)
            .finish();
        self.request(&req).map(|_| ())
    }

    /// Drains pending event frames, returning true if any was a change we care
    /// about. Non-blocking: the caller polls the fd via the event loop.
    pub fn drain_events(&mut self) -> bool {
        let mut saw_event = false;
        self.stream.set_nonblocking(true).ok();
        loop {
            let mut header = [0u8; 20];
            match self.stream.read_exact(&mut header) {
                Ok(()) => {
                    let len = u32::from_be_bytes(match header[..4].try_into() {
                        Ok(b) => b,
                        Err(_) => break,
                    }) as usize;
                    if len > 1 << 20 {
                        break;
                    }
                    let mut body = vec![0u8; len];
                    // The frame header arrived, so the body is on its way:
                    // block briefly for it rather than losing sync.
                    self.stream.set_nonblocking(false).ok();
                    let got = self.stream.read_exact(&mut body).is_ok();
                    self.stream.set_nonblocking(true).ok();
                    if !got {
                        break;
                    }
                    if Reader::new(&body).u32() == Some(CMD_SUBSCRIBE_EVENT) {
                        saw_event = true;
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
        saw_event
    }

    /// Whether the server closed the connection.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_raw_volume_to_percent() {
        assert_eq!(to_percent(NORM_VOLUME), 100);
        assert_eq!(to_percent(0), 0);
        assert_eq!(to_percent(52_408), 80); // the value pactl shows as 80%
        assert_eq!(to_percent(14_392), 22);
        assert_eq!(to_percent(NORM_VOLUME / 2), 50);
    }

    /// A captured sink-info reply must parse to the same values `pactl` shows.
    /// Layout exercised: index, name, description, sample_spec, channel_map,
    /// owner_module, cvolume, mute.
    #[test]
    fn parses_a_sink_info_reply() {
        let mut body = Vec::new();
        let push_u32 = |b: &mut Vec<u8>, v: u32| {
            b.push(TAG_U32);
            b.extend_from_slice(&v.to_be_bytes());
        };
        let push_str = |b: &mut Vec<u8>, s: &str| {
            b.push(TAG_STRING);
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        };
        push_u32(&mut body, CMD_REPLY);
        push_u32(&mut body, 2); // tag
        push_u32(&mut body, 58); // index
        push_str(&mut body, "lsa_output.analog-stereo");
        push_str(&mut body, "Analog Stereo");
        body.push(TAG_SAMPLE_SPEC);
        body.extend_from_slice(&[7, 2]);
        body.extend_from_slice(&48_000u32.to_be_bytes());
        body.push(TAG_CHANNEL_MAP);
        body.extend_from_slice(&[2, 1, 2]);
        push_u32(&mut body, INVALID_INDEX); // owner_module
        body.push(TAG_CVOLUME);
        body.push(2);
        body.extend_from_slice(&52_408u32.to_be_bytes());
        body.extend_from_slice(&52_408u32.to_be_bytes());
        body.push(TAG_BOOL_FALSE);

        // The index is read too: writes need it, since the server will not
        // resolve INVALID_INDEX to the default device when setting mute.
        assert_eq!(
            parse_device_info(&body),
            Some(DeviceState { volume_percent: 80, muted: false, index: 58, channels: 2 })
        );
    }

    /// Truncated replies must fail cleanly instead of panicking.
    #[test]
    fn rejects_truncated_replies() {
        assert_eq!(parse_device_info(&[]), None);
        assert_eq!(parse_device_info(&[TAG_U32, 0, 0, 0, 2]), None);
        let mut short = vec![TAG_U32];
        short.extend_from_slice(&CMD_REPLY.to_be_bytes());
        assert_eq!(parse_device_info(&short), None);
    }

    /// Live parity with `pactl`, when a server is reachable.
    ///
    /// Serialised with the mute toggles: they flip the same sink, and a
    /// reading taken mid-flip matches neither side.
    #[test]
    fn agrees_with_pactl() {
        let _guard = crate::modules::pulseaudio::audio_test_lock();
        let Some(mut pulse) = Pulse::connect() else {
            return; // no PulseAudio/PipeWire on this machine
        };
        let sink = pulse.default_sink().expect("sink info");

        let out = std::process::Command::new("pactl")
            .args(["get-sink-volume", "@DEFAULT_SINK@"])
            .output();
        let Ok(out) = out else { return };
        let text = String::from_utf8_lossy(&out.stdout);
        let Some(pct) = text
            .split('/')
            .nth(1)
            .and_then(|s| s.trim().trim_end_matches('%').trim().parse::<u64>().ok())
        else {
            return;
        };
        assert_eq!(sink.volume_percent, pct, "volume mismatch vs pactl");

        let mute_out = std::process::Command::new("pactl")
            .args(["get-sink-mute", "@DEFAULT_SINK@"])
            .output();
        if let Ok(m) = mute_out {
            let muted = String::from_utf8_lossy(&m.stdout).contains("yes");
            assert_eq!(sink.muted, muted, "mute mismatch vs pactl");
        }
    }
}
