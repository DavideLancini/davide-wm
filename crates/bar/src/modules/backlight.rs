//! Screen brightness as a percentage, from /sys/class/backlight.
//! Matches Waybar's `backlight` module with format "{percent}%".

use crate::settings;
use std::sync::OnceLock;

/// Rounds like Waybar, which reports `brightness / max * 100` rounded.
fn percent(brightness: u64, max: u64) -> Option<u64> {
    if max == 0 {
        return None;
    }
    Some(((brightness as f64 / max as f64) * 100.0).round() as u64)
}

fn read_u64(path: &str, file: &str) -> Option<u64> {
    std::fs::read_to_string(format!("{}/{}", path, file))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Path of the backlight device: the configured one, or the first available.
/// Resolved once, since the device does not appear or move at runtime.
pub fn device_path() -> Option<&'static str> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| {
        if std::path::Path::new(settings::BACKLIGHT_PATH).is_dir() {
            return Some(settings::BACKLIGHT_PATH.to_string());
        }
        let entry = std::fs::read_dir("/sys/class/backlight").ok()?.flatten().next()?;
        Some(entry.path().to_string_lossy().into_owned())
    })
    .as_deref()
}

pub fn text() -> String {
    let Some(path) = device_path() else {
        return String::new();
    };
    let brightness = read_u64(path, "brightness");
    let max = read_u64(path, "max_brightness");
    match (brightness, max) {
        (Some(b), Some(m)) => match percent(b, m) {
            Some(p) => format!("{}%", p),
            None => String::new(),
        },
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_rounded_percentages() {
        assert_eq!(percent(255, 255), Some(100));
        assert_eq!(percent(0, 255), Some(0));
        assert_eq!(percent(128, 255), Some(50));
        // 96/255 = 37.6% -> rounds up, unlike integer truncation.
        assert_eq!(percent(96, 255), Some(38));
        assert_eq!(percent(10, 0), None);
    }

    #[test]
    fn reads_the_real_device() {
        let Some(path) = device_path() else {
            return; // no backlight on this machine
        };
        assert!(read_u64(path, "max_brightness").is_some(), "max unreadable");
        let t = text();
        assert!(t.ends_with('%'), "got {t:?}");
        let pct: u64 = t.trim_end_matches('%').parse().expect("numeric");
        assert!(pct <= 100);
    }
}
