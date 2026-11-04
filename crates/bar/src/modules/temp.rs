//! Highest temperature across all hwmon sensors, as "N°C".
//!
//! Replaces the Waybar `custom/temp` module, which ran `sensors` and took the
//! maximum reading. Reading /sys/class/hwmon directly is the same data without
//! the shell pipeline: `sensors` itself is a libsensors client of these files.

use std::path::PathBuf;
use std::sync::OnceLock;

const HWMON: &str = "/sys/class/hwmon";

/// hwmon drivers whose sensors are too expensive to poll: reading an `nvme`
/// temperature issues an admin command to the drive, costing ~7 ms and waking
/// it up. Drive temperature is not what this widget reports anyway.
const SKIP_DRIVERS: &[&str] = &["nvme"];

/// Sensor files, discovered once: the set of sensors does not change.
fn sensor_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut paths = Vec::new();
        let Ok(hwmons) = std::fs::read_dir(HWMON) else {
            return paths;
        };
        for hwmon in hwmons.flatten() {
            let driver = std::fs::read_to_string(hwmon.path().join("name"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if SKIP_DRIVERS.contains(&driver.as_str()) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(hwmon.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // tempN_input only: tempN_crit/_max are limits, which the
                // original awk filtered out via the crit|high|low guard.
                if name.starts_with("temp") && name.ends_with("_input") {
                    paths.push(entry.path());
                }
            }
        }
        paths.sort();
        paths
    })
}

/// Highest sensor reading in millidegrees Celsius.
fn max_millidegrees() -> Option<i64> {
    let mut max: Option<i64> = None;
    for path in sensor_paths() {
        if let Some(v) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
        {
            // Ignore obviously bogus readings (disconnected sensors).
            if (0..=200_000).contains(&v) {
                max = Some(max.map_or(v, |m| m.max(v)));
            }
        }
    }
    max
}

/// Millidegrees -> whole degrees, truncated like `int()` in the original awk.
fn format_millidegrees(mc: i64) -> String {
    format!("{}°C", mc / 1000)
}

pub fn text() -> String {
    match max_millidegrees() {
        Some(mc) => format_millidegrees(mc),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_like_awk_int() {
        assert_eq!(format_millidegrees(45_250), "45°C");
        assert_eq!(format_millidegrees(39_850), "39°C");
        assert_eq!(format_millidegrees(0), "0°C");
    }

    /// Parity with the `sensors` pipeline the Waybar module used. `sensors`
    /// also reports nvme, which we deliberately skip, so ours may be lower —
    /// but never higher, and never absurdly far off.
    #[test]
    fn agrees_with_the_sensors_pipeline() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(r#"sensors | awk '/\+/ && !/crit|high|low/ {for (i=1;i<=NF;i++) if ($i~/^\+[0-9]+\.[0-9]+°C$/) print int(substr($i,2,length($i)-3))}' | sort -nr | head -n1"#)
            .output()
            .expect("sh");
        let sensors_max: i64 = match String::from_utf8_lossy(&out.stdout).trim().parse() {
            Ok(v) => v,
            Err(_) => return, // sensors unavailable: nothing to compare against
        };

        let ours = max_millidegrees().expect("hwmon readable") / 1000;
        assert!(ours > 0 && ours < 120, "implausible reading: {ours}");
        assert!(
            ours <= sensors_max + 3,
            "ours={ours} exceeds sensors={sensors_max}"
        );
    }

    /// The expensive nvme sensors must not be in the polling set.
    #[test]
    fn skips_expensive_drivers() {
        for path in sensor_paths() {
            let driver = path
                .parent()
                .and_then(|p| std::fs::read_to_string(p.join("name")).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            assert!(
                !SKIP_DRIVERS.contains(&driver.as_str()),
                "{path:?} belongs to skipped driver {driver:?}"
            );
        }
    }
}
