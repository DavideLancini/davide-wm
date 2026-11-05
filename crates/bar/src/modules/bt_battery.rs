//! Battery of connected Bluetooth audio devices (absorbs dawn's
//! `bt-headphones-battery`): BlueZ's `Battery1` over D-Bus, or, for earbuds
//! that speak the Mi protocol, left/right/case read over RFCOMM channel 6.
//! Read when first shown and after a click, as before; a read can take up to
//! two seconds per Mi device.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use dbus::{Bus, Message, Value};

const MI_DEVICES: &[&str] = &["Redmi Buds 6 Play"];
const RFCOMM_CHANNEL: u8 = 6;
const TIMEOUT: Duration = Duration::from_secs(2);
const BATTERY_REQUEST: &[u8] = &[0xfe, 0xdc, 0xba, 0xc4, 0x02, 0x00, 0x05, 0x0b, 0xff, 0xff, 0xff, 0xff, 0xef, 0x4f];
const BATTERY_PATTERN: &[u8] = &[0x02, 0x02, 0x04, 0x07];

#[derive(Default)]
pub struct BtBattery {
    text: String,
    loaded: bool,
}

impl BtBattery {
    pub fn text(&mut self) -> String {
        if !self.loaded {
            self.loaded = true;
            self.text = format(&gather());
        }
        self.text.clone()
    }

    pub fn invalidate(&mut self) {
        self.loaded = false;
    }
}

/// (name, [(label, percent)]); an empty label for a single value.
type Devices = Vec<(String, Vec<(String, u8)>)>;

/// "80%", or "L:80% R:75% C:50%", devices separated by " | ".
fn format(devices: &Devices) -> String {
    devices
        .iter()
        .map(|(_, batts)| match batts.as_slice() {
            [(l, p)] if l.is_empty() => format!("{p}%"),
            b => b.iter().map(|(l, p)| format!("{l}:{p}%")).collect::<Vec<_>>().join(" "),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn gather() -> Devices {
    let Ok(mut bus) = Bus::system() else { return Vec::new() };
    let call = Message::call("org.bluez", "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", Vec::new());
    let Ok(reply) = bus.call(call, TIMEOUT) else { return Vec::new() };
    let Some(Value::Array(_, objects)) = reply.body.first() else { return Vec::new() };
    let mut out = Devices::new();
    for obj in objects {
        let Value::DictEntry(_, ifaces) = obj else { continue };
        let Some(dev) = ifaces.get("org.bluez.Device1") else { continue };
        if dev.get("Connected").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = dev.get("Name").or(dev.get("Alias")).and_then(Value::as_str).unwrap_or("").to_string();
        let address = dev.get("Address").and_then(Value::as_str).unwrap_or("").to_string();
        if MI_DEVICES.iter().any(|m| name.contains(m)) {
            if let Some((l, r, c)) = mi_battery(&address) {
                let batts: Vec<(String, u8)> = [("L", l), ("R", r), ("C", c)].into_iter().filter(|(_, p)| *p != 0xff).map(|(k, p)| (k.to_string(), p)).collect();
                if !batts.is_empty() {
                    out.push((name, batts));
                    continue;
                }
            }
        }
        if let Some(p) = ifaces.get("org.bluez.Battery1").and_then(|b| b.get("Percentage")).and_then(Value::as_u32) {
            out.push((name, vec![(String::new(), p as u8)]));
        }
    }
    out
}

fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = mac.split(':').map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().ok()?;
    let mut b: [u8; 6] = parts.try_into().ok()?;
    // bdaddr_t is little endian.
    b.reverse();
    Some(b)
}

#[repr(C)]
struct SockaddrRc {
    family: u16,
    bdaddr: [u8; 6],
    channel: u8,
}

/// Left, right and case levels (0xff: unknown) over the Mi protocol.
fn mi_battery(mac: &str) -> Option<(u8, u8, u8)> {
    const AF_BLUETOOTH: i32 = 31;
    const BTPROTO_RFCOMM: i32 = 3;
    let bdaddr = parse_mac(mac)?;
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, BTPROTO_RFCOMM) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = SockaddrRc { family: AF_BLUETOOTH as u16, bdaddr, channel: RFCOMM_CHANNEL };
    let wait = |events: i16| {
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 };
        unsafe { libc::poll(&mut p, 1, TIMEOUT.as_millis() as i32) > 0 }
    };
    unsafe {
        let r = libc::connect(fd.as_raw_fd(), (&addr as *const SockaddrRc).cast(), std::mem::size_of::<SockaddrRc>() as u32);
        if r != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINPROGRESS) {
            return None;
        }
        if !wait(libc::POLLOUT) {
            return None;
        }
        let mut err: i32 = 0;
        let mut len = 4u32;
        if libc::getsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ERROR, (&mut err as *mut i32).cast(), &mut len) < 0 || err != 0 {
            return None;
        }
        if libc::send(fd.as_raw_fd(), BATTERY_REQUEST.as_ptr().cast(), BATTERY_REQUEST.len(), 0) < 0 {
            return None;
        }
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while Instant::now() < deadline && wait(libc::POLLIN) {
        let n = unsafe { libc::recv(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len(), 0) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        if let Some(i) = buf.windows(BATTERY_PATTERN.len()).position(|w| w == BATTERY_PATTERN) {
            if buf.len() >= i + 7 {
                return Some((buf[i + 4], buf[i + 5], buf[i + 6]));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses() {
        let d: Devices = vec![("Cuffie".into(), vec![(String::new(), 80)]), ("Buds".into(), vec![("L".into(), 70), ("R".into(), 65)])];
        assert_eq!(format(&d), "80% | L:70% R:65%");
        assert_eq!(parse_mac("AA:BB:CC:DD:EE:01"), Some([0x01, 0xee, 0xdd, 0xcc, 0xbb, 0xaa]));
        assert_eq!(parse_mac("nope"), None);
    }
}
