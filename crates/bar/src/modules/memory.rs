//! Memory usage as a percentage, read from /proc/meminfo.
//! Matches Waybar's `memory` module with format "{}%".

/// Percentage of memory in use, using MemAvailable like Waybar does.
fn percent_from(meminfo: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut available = 0u64;
    for line in meminfo.lines() {
        let value = || -> Option<u64> { line.split_whitespace().nth(1)?.parse().ok() };
        if line.starts_with("MemTotal:") {
            total = value()?;
        } else if line.starts_with("MemAvailable:") {
            available = value()?;
        }
    }
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(available) * 100) / total)
}

pub fn text() -> String {
    match std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| percent_from(&s))
    {
        Some(pct) => format!("{}%", pct),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_used_percentage() {
        let sample = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    4000000 kB\nBuffers:          100000 kB\n";
        assert_eq!(percent_from(sample), Some(75));
    }

    #[test]
    fn missing_or_zero_fields_yield_none() {
        assert_eq!(percent_from("MemTotal: 0 kB\nMemAvailable: 0 kB\n"), None);
        assert_eq!(percent_from("Nothing: 1 kB\n"), None);
        assert_eq!(percent_from(""), None);
    }

    #[test]
    fn reads_the_real_system() {
        let t = text();
        assert!(t.ends_with('%'), "got {t:?}");
        let pct: u64 = t.trim_end_matches('%').parse().expect("numeric");
        assert!(pct <= 100);
    }
}
