//! Battery: capacity, remaining time and power draw.
//!
//! Rust port of `~/.config/waybar/battery-power.sh`, which produced e.g.
//! "74% 1h11 10W". Watts come from `power_now` when present, otherwise from
//! `voltage_now * current_now`; the time estimate uses `charge_*` when present
//! and falls back to `energy_*`.

use crate::icons;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const SUPPLY_DIR: &str = "/sys/class/power_supply";

/// First power_supply entry whose type is "Battery", like the shell loop over
/// `/sys/class/power_supply/BAT*`. Resolved once: the device does not move.
fn battery_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(SUPPLY_DIR)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("BAT"))
                    .unwrap_or(false)
            })
            .collect();
        candidates.sort();
        candidates.into_iter().find(|p| {
            std::fs::read_to_string(p.join("type"))
                .map(|t| t.trim() == "Battery")
                .unwrap_or(false)
        })
    })
    .as_ref()
}

fn read_str(base: &Path, file: &str) -> Option<String> {
    Some(std::fs::read_to_string(base.join(file)).ok()?.trim().to_string())
}

fn read_u64(base: &Path, file: &str) -> Option<u64> {
    read_str(base, file)?.parse().ok()
}

/// Power draw in whole watts.
fn watts(base: &Path) -> u64 {
    if let Some(uw) = read_u64(base, "power_now") {
        return uw / 1_000_000;
    }
    match (read_u64(base, "voltage_now"), read_u64(base, "current_now")) {
        // µV * µA / 1e12 = W
        (Some(v), Some(c)) => (v as u128 * c as u128 / 1_000_000_000_000) as u64,
        _ => 0,
    }
}

/// "HhMM" for a duration in minutes, empty when not positive.
fn format_minutes(minutes: i64) -> String {
    if minutes <= 0 {
        return String::new();
    }
    format!("{}h{:02}", minutes / 60, minutes % 60)
}

/// Remaining time text for the current status.
fn time_text(base: &Path, status: &str, power_w: u64) -> String {
    let minutes = match status {
        "Charging" => match (
            read_u64(base, "charge_full"),
            read_u64(base, "charge_now"),
            read_u64(base, "current_now"),
        ) {
            (Some(full), Some(now), Some(cur)) if cur > 0 => {
                Some(((full.saturating_sub(now)) as f64 / cur as f64 * 60.0) as i64)
            }
            _ => match (read_u64(base, "energy_full"), read_u64(base, "energy_now")) {
                (Some(full), Some(now)) if power_w > 0 => Some(
                    ((full.saturating_sub(now)) as f64 / 1_000_000.0 / power_w as f64 * 60.0) as i64,
                ),
                _ => None,
            },
        },
        "Discharging" => match (read_u64(base, "charge_now"), read_u64(base, "current_now")) {
            (Some(now), Some(cur)) if cur > 0 => Some((now as f64 / cur as f64 * 60.0) as i64),
            _ => match read_u64(base, "energy_now") {
                Some(now) if power_w > 0 => {
                    Some((now as f64 / 1_000_000.0 / power_w as f64 * 60.0) as i64)
                }
                _ => None,
            },
        },
        _ => None,
    };
    minutes.map(format_minutes).unwrap_or_default()
}

/// Builds the display text from the parts, mirroring the shell's case block.
/// Charging is shown with a plug icon rather than a word.
fn compose(status: &str, capacity: u64, time: &str, power_w: u64) -> String {
    let charging = if status == "Charging" {
        format!("{} ", icons::POWER)
    } else {
        String::new()
    };
    match status {
        "Full" | "Not charging" => format!("{}%", capacity),
        _ if !time.is_empty() => format!("{}{}% {} {}W", charging, capacity, time, power_w),
        _ => format!("{}{}% {}W", charging, capacity, power_w),
    }
}

/// Whether the battery is filling, for the colour the widget takes.
///
/// "Full" counts as charging: the cable is in either way, and the reader's
/// question is "am I on mains?" rather than "is current flowing?".
pub fn is_charging() -> bool {
    let Some(base) = battery_path() else {
        return false;
    };
    matches!(
        read_str(base, "status").as_deref(),
        Some("Charging") | Some("Full") | Some("Not charging")
    )
}

pub fn text() -> String {
    let Some(base) = battery_path() else {
        return String::new();
    };
    let status = read_str(base, "status").unwrap_or_else(|| "Unknown".into());
    let capacity = read_u64(base, "capacity").unwrap_or(0);
    let power_w = watts(base);
    let time = time_text(base, &status, power_w);
    compose(&status, capacity, &time, power_w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_like_awk() {
        assert_eq!(format_minutes(71), "1h11");
        assert_eq!(format_minutes(60), "1h00");
        assert_eq!(format_minutes(5), "0h05");
        assert_eq!(format_minutes(605), "10h05");
        assert_eq!(format_minutes(0), "");
        assert_eq!(format_minutes(-3), "");
    }

    #[test]
    fn composes_the_shell_formats() {
        assert_eq!(compose("Charging", 74, "1h11", 10), format!("{} 74% 1h11 10W", icons::POWER));
        assert_eq!(compose("Discharging", 50, "", 12), "50% 12W");
        // Full and "Not charging" drop time and watts entirely. The shell
        // script prefixed an icon in these states; a text bar shows just the
        // percentage.
        assert_eq!(compose("Full", 100, "2h00", 0), "100%");
        assert_eq!(compose("Not charging", 99, "", 5), "99%");
        assert_eq!(compose("Not charging", 80, "1h00", 5), "80%");
        // Unknown states still report what is known.
        assert_eq!(compose("Unknown", 50, "", 7), "50% 7W");
    }

    /// Parity with the shell script that Waybar ran, comparing the capacity and
    /// watts fields (the time estimate drifts between reads).
    #[test]
    fn agrees_with_the_shell_script() {
        let script = "/home/davidelancini/.config/waybar/battery-power.sh";
        if !std::path::Path::new(script).exists() {
            return;
        }
        let out = std::process::Command::new(script).output().expect("script");
        let json = String::from_utf8_lossy(&out.stdout);
        let Some(start) = json.find("\"text\":\"") else { return };
        let rest = &json[start + 8..];
        let shell_text = &rest[..rest.find('"').unwrap_or(0)];
        if shell_text.is_empty() {
            return; // no battery
        }

        let ours = text();
        // The shell script prefixes an icon escape in the Full / "Not charging"
        // states; a text bar has no icons, so compare the percentage instead of
        // the first field.
        let percent = |s: &str| {
            s.split_whitespace()
                .find(|f| f.ends_with('%'))
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(
            percent(&ours),
            percent(shell_text),
            "capacity: {ours} vs {shell_text}"
        );

        // Watts is the last field when present in both.
        let last = |s: &str| s.split_whitespace().last().unwrap_or("").to_string();
        if last(&ours).ends_with('W') && last(shell_text).ends_with('W') {
            let parse_w = |s: String| s.trim_end_matches('W').parse::<i64>().unwrap_or(-1);
            let (a, b) = (parse_w(last(&ours)), parse_w(last(shell_text)));
            assert!((a - b).abs() <= 3, "watts: {a} vs {b}");
        }
    }
}
