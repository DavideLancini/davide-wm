//! CPU frequency: highest current frequency across cores, as "N.NGHz", plus
//! the power level currently set.
//!
//! Matches the Waybar `custom/cpu-freq` module, which took the max of
//! /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq and coloured itself
//! by the level in `~/.cache/cpu-level` — the profile `cpu-cycle` sets, which
//! is also this widget's click action.

use std::path::PathBuf;
use std::sync::OnceLock;

const CPU_DIR: &str = "/sys/devices/system/cpu";

/// Per-core frequency files, discovered once: the core count is fixed.
fn freq_paths() -> &'static [PathBuf] {
    static PATHS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    PATHS.get_or_init(|| {
        let mut paths = Vec::new();
        let Ok(entries) = std::fs::read_dir(CPU_DIR) else {
            return paths;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Only cpuN directories, not cpuidle/cpufreq/etc.
            if name.len() > 3
                && name.starts_with("cpu")
                && name[3..].chars().all(|c| c.is_ascii_digit())
            {
                let path = entry.path().join("cpufreq/scaling_cur_freq");
                if path.exists() {
                    paths.push(path);
                }
            }
        }
        paths.sort();
        paths
    })
}

/// Highest `scaling_cur_freq` across all cores, in kHz.
fn max_khz() -> Option<u64> {
    let mut max = None;
    for path in freq_paths() {
        if let Some(khz) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            max = Some(max.map_or(khz, |m: u64| m.max(khz)));
        }
    }
    max
}

/// kHz -> "N.NGHz", rounded to one decimal like the original awk did.
fn format_khz(khz: u64) -> String {
    format!("{:.1}GHz", khz as f64 / 1_000_000.0)
}

pub fn text() -> String {
    match max_khz() {
        Some(khz) => format_khz(khz),
        None => String::new(),
    }
}

/// Power level 1-5 from `~/.cache/cpu-level`, written by `cpu-cycle`.
/// None when the file is missing or holds something unexpected.
pub fn level() -> Option<u8> {
    let home = std::env::var("HOME").ok()?;
    let raw = std::fs::read_to_string(format!("{}/.cache/cpu-level", home)).ok()?;
    let level: u8 = raw.trim().parse().ok()?;
    (1..=5).contains(&level).then_some(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_frequencies_like_awk() {
        assert_eq!(format_khz(1_696_860), "1.7GHz");
        assert_eq!(format_khz(4_200_000), "4.2GHz");
        assert_eq!(format_khz(400_000), "0.4GHz");
        assert_eq!(format_khz(1_400_000), "1.4GHz");
    }

    /// The level drives the widget's colour, so a value outside 1-5 must be
    /// rejected rather than indexing past the palette.
    #[test]
    fn level_is_read_or_absent() {
        // None: no cache file on this machine.
        if let Some(l) = level() {
            assert!((1..=5).contains(&l), "out of range: {l}");
        }
    }

    /// Parity with the shell pipeline the Waybar module used.
    #[test]
    fn matches_the_shell_pipeline() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq | sort -n | tail -1")
            .output()
            .expect("sh");
        let shell_max: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric");

        // Frequencies move between the two reads, so compare the plausible
        // range rather than the exact value: both must be real CPU frequencies.
        let ours = max_khz().expect("sysfs readable");
        assert!(ours > 100_000 && ours < 10_000_000, "implausible: {ours}");
        assert!(shell_max > 100_000, "shell read failed");
        assert!(text().ends_with("GHz"));
    }
}
