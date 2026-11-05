//! One brightness percentage for every panel (absorbs dawn's `brightness`):
//! the laptop panel through `/sys/class/backlight`, external monitors
//! through DDC/CI (VCP 0x10) on their connector's I2C bus. The user needs
//! write access to both (groups `video` and `i2c`).

use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const BACKLIGHT_DIR: &str = "/sys/class/backlight";
const DRM_DIR: &str = "/sys/class/drm";
const VCP_BRIGHTNESS: u8 = 0x10;
const DDC_ADDR: libc::c_ulong = 0x37;
const I2C_SLAVE: libc::c_ulong = 0x0703;

fn pct_to_raw(pct: u32, max: u32) -> u32 {
    ((pct.min(100) as u64 * max as u64 + 50) / 100) as u32
}

fn raw_to_pct(raw: u32, max: u32) -> u32 {
    if max == 0 {
        return 0;
    }
    (((raw.min(max) as u64) * 100 + max as u64 / 2) / max as u64) as u32
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The first backlight device, alphabetically.
fn panel() -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(BACKLIGHT_DIR).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries.into_iter().next()
}

/// The laptop panel's brightness in percent.
pub fn get() -> Option<u32> {
    let dev = panel()?;
    Some(raw_to_pct(read_u32(&dev.join("brightness"))?, read_u32(&dev.join("max_brightness"))?))
}

/// Sets the laptop panel; returns the percentage applied.
pub fn set_panel(pct: u32) -> Option<u32> {
    let dev = panel()?;
    let max = read_u32(&dev.join("max_brightness"))?;
    let pct = pct.min(100);
    fs::write(dev.join("brightness"), pct_to_raw(pct, max).to_string()).ok()?;
    Some(pct)
}

/// I2C buses of connected external monitors (eDP does not answer DDC/CI).
fn external_buses() -> Vec<u32> {
    let Ok(entries) = fs::read_dir(DRM_DIR) else { return Vec::new() };
    let mut buses: Vec<u32> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.contains('-') && !name.contains("eDP") && fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected")
        })
        .filter_map(|e| {
            fs::read_dir(e.path()).ok()?.flatten().find_map(|c| c.file_name().to_string_lossy().strip_prefix("i2c-").and_then(|n| n.parse().ok()))
        })
        .collect();
    buses.sort_unstable();
    buses
}

/// DDC/CI "set VCP feature": source, length, opcode, code, value, checksum.
fn ddc_packet(code: u8, value: u16) -> [u8; 7] {
    let mut p = [0x51, 0x84, 0x03, code, (value >> 8) as u8, value as u8, 0];
    p[6] = p[1..6].iter().fold(0x6e ^ 0x51, |s, b| s ^ b);
    p
}

fn ddc_write(bus: u32, pct: u32) -> bool {
    let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(format!("/dev/i2c-{bus}")) else { return false };
    let packet = ddc_packet(VCP_BRIGHTNESS, pct.min(100) as u16);
    unsafe {
        if libc::ioctl(f.as_raw_fd(), I2C_SLAVE, DDC_ADDR) != 0 {
            return false;
        }
        let n = libc::write(f.as_raw_fd(), packet.as_ptr().cast(), packet.len());
        // DDC/CI wants 40 ms of quiet after a write.
        std::thread::sleep(std::time::Duration::from_millis(50));
        n == packet.len() as isize
    }
}

/// Mirrors the percentage to external monitors: slow, for the worker.
pub fn set_external(pct: u32) -> usize {
    external_buses().into_iter().filter(|b| ddc_write(*b, pct)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_and_packet() {
        assert_eq!(pct_to_raw(50, 255), 128);
        assert_eq!(raw_to_pct(128, 255), 50);
        assert_eq!(raw_to_pct(5, 0), 0);
        assert_eq!(ddc_packet(0x10, 40), [0x51, 0x84, 0x03, 0x10, 0x00, 0x28, 0x6e ^ 0x51 ^ 0x84 ^ 0x03 ^ 0x10 ^ 0x28]);
    }
}
