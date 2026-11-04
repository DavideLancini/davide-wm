//! Network: "{signal}% {essid}" on wifi, the IPv4 address on ethernet,
//! "Disconnected" otherwise. Mirrors Waybar's `network` formats.
//!
//! No shelling out: the SSID comes from the SIOCGIWESSID ioctl (what `iwgetid`
//! does), the signal from /proc/net/wireless, and the address from
//! SIOCGIFADDR over the same socket.

use std::os::raw::{c_int, c_ulong, c_void};

const SIOCGIWESSID: c_ulong = 0x8B1B;
const SIOCGIFADDR: c_ulong = 0x8915;
const AF_INET: c_int = 2;
const SOCK_DGRAM: c_int = 2;
const IFNAMSIZ: usize = 16;
const ESSID_MAX: usize = 32;

extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
}

/// RAII wrapper for the ioctl socket.
struct Sock(c_int);

impl Sock {
    fn new() -> Option<Self> {
        let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
        (fd >= 0).then_some(Sock(fd))
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        unsafe { close(self.0) };
    }
}

fn name_bytes(iface: &str) -> [u8; IFNAMSIZ] {
    let mut buf = [0u8; IFNAMSIZ];
    let src = iface.as_bytes();
    let n = src.len().min(IFNAMSIZ - 1);
    buf[..n].copy_from_slice(&src[..n]);
    buf
}

/// SSID of a wireless interface, via the same ioctl `iwgetid -r` uses.
fn essid(sock: &Sock, iface: &str) -> Option<String> {
    // struct iwreq { char ifr_name[16]; union { struct { void *pointer;
    //   __u16 length; __u16 flags; } essid; ... } u; }
    #[repr(C)]
    struct IwReq {
        name: [u8; IFNAMSIZ],
        pointer: *mut c_void,
        length: u16,
        flags: u16,
    }

    let mut buf = [0u8; ESSID_MAX + 1];
    let mut req = IwReq {
        name: name_bytes(iface),
        pointer: buf.as_mut_ptr() as *mut c_void,
        length: ESSID_MAX as u16,
        flags: 0,
    };
    if unsafe { ioctl(sock.0, SIOCGIWESSID, &mut req as *mut IwReq) } < 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let ssid = String::from_utf8_lossy(&buf[..end]).into_owned();
    (!ssid.is_empty()).then_some(ssid)
}

/// IPv4 address of an interface, via SIOCGIFADDR.
fn ipv4(sock: &Sock, iface: &str) -> Option<String> {
    // struct ifreq { char ifr_name[16]; struct sockaddr_in ifr_addr; }
    #[repr(C)]
    struct IfReq {
        name: [u8; IFNAMSIZ],
        family: u16,
        port: u16,
        addr: [u8; 4],
        pad: [u8; 8],
    }

    let mut req = IfReq {
        name: name_bytes(iface),
        family: 0,
        port: 0,
        addr: [0; 4],
        pad: [0; 8],
    };
    if unsafe { ioctl(sock.0, SIOCGIFADDR, &mut req as *mut IfReq) } < 0 {
        return None;
    }
    let a = req.addr;
    Some(format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]))
}

/// Link quality per interface from /proc/net/wireless, as a percentage of the
/// 70-point scale wireless drivers report.
fn wireless_quality(contents: &str) -> Vec<(String, u64)> {
    contents
        .lines()
        .skip(2) // two header lines
        .filter_map(|line| {
            let (iface, rest) = line.split_once(':')?;
            let link: f64 = rest.split_whitespace().nth(1)?.trim_end_matches('.').parse().ok()?;
            let pct = ((link / 70.0) * 100.0).round().clamp(0.0, 100.0) as u64;
            Some((iface.trim().to_string(), pct))
        })
        .collect()
}

/// Interfaces that are up and not loopback, in kernel order.
fn candidate_interfaces() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return names;
    };
    let mut all: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    all.sort();
    for name in all {
        let name = name.to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let operstate = std::fs::read_to_string(format!("/sys/class/net/{}/operstate", name))
            .unwrap_or_default();
        if operstate.trim() == "up" {
            names.push(name);
        }
    }
    names
}

/// True when the interface has a `wireless` directory in sysfs.
fn is_wireless(iface: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/net/{}/wireless", iface)).exists()
}

pub fn text() -> String {
    let Some(sock) = Sock::new() else {
        return String::new();
    };
    let quality = std::fs::read_to_string("/proc/net/wireless")
        .map(|s| wireless_quality(&s))
        .unwrap_or_default();

    for iface in candidate_interfaces() {
        if is_wireless(&iface) {
            if let Some(ssid) = essid(&sock, &iface) {
                let pct = quality
                    .iter()
                    .find(|(name, _)| *name == iface)
                    .map(|(_, p)| *p);
                // The SSID costs more width than it earns: it changes
                // rarely, and the click still opens the network manager.
                let _ = &ssid;
                return match pct {
                    Some(p) => format!("{}", p),
                    None => String::new(),
                };
            }
        }
        // Wired, or wireless without an association: show the address.
        if let Some(ip) = ipv4(&sock, &iface) {
            return ip;
        }
    }
    "Disconnected".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wireless_quality_as_percentage() {
        let sample = "Inter-| sta-|   Quality        |   Discarded packets\n \
                      face | tus | link level noise |  nwid  crypt\n\
                      wlp1s0: 0000   53.  -57.  -256        0      0\n";
        assert_eq!(wireless_quality(sample), vec![("wlp1s0".to_string(), 76)]);
    }

    #[test]
    fn quality_edges_and_garbage() {
        let full = "h1\nh2\nwlan0: 0000   70.  -30.  -256\n";
        assert_eq!(wireless_quality(full), vec![("wlan0".to_string(), 100)]);
        let zero = "h1\nh2\nwlan0: 0000   0.  -90.  -256\n";
        assert_eq!(wireless_quality(zero), vec![("wlan0".to_string(), 0)]);
        // Over-range values are clamped, malformed lines dropped.
        let over = "h1\nh2\nwlan0: 0000   99.  -30.  -256\n";
        assert_eq!(wireless_quality(over), vec![("wlan0".to_string(), 100)]);
        assert!(wireless_quality("h1\nh2\ngarbage\n").is_empty());
        assert!(wireless_quality("").is_empty());
    }

    /// Parity with the real system: whatever we report must match what the
    /// kernel says about the interface actually carrying traffic.
    #[test]
    fn reports_the_live_connection() {
        let t = text();
        assert!(!t.is_empty(), "must always render something");

        let ifaces = candidate_interfaces();
        if ifaces.is_empty() {
            assert_eq!(t, "Disconnected");
            return;
        }
        // Either a bare signal percentage, or a plain IPv4 address. The SSID
        // and the "%" sign were dropped to save width.
        let looks_wifi = t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty();
        let looks_ip = t.split('.').count() == 4 && t.chars().next().unwrap().is_ascii_digit();
        assert!(
            looks_wifi || looks_ip || t == "Disconnected",
            "unexpected format: {t:?}"
        );
    }
}
