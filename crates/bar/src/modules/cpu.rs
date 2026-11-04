//! CPU usage as a percentage, from the delta between two /proc/stat samples.
//! Matches Waybar's `cpu` module with format "{usage}%".
//!
//! The previous sample is kept in `Cpu` so the delta spans two ticks: sleeping
//! inside the event loop to take a second reading would stall the whole bar.

/// Cumulative (total, idle) jiffies from the aggregate `cpu` line.
fn parse_totals(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().next()?;
    if !line.starts_with("cpu ") {
        return None;
    }
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if fields.len() < 4 {
        return None;
    }
    // idle + iowait, as Waybar treats both as not-busy.
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    Some((fields.iter().sum(), idle))
}

fn read_totals() -> Option<(u64, u64)> {
    parse_totals(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// Usage percentage between two cumulative samples.
fn usage_between(prev: (u64, u64), cur: (u64, u64)) -> Option<u64> {
    let total = cur.0.checked_sub(prev.0)?;
    let idle = cur.1.checked_sub(prev.1)?;
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(idle) * 100) / total)
}

/// CPU module state: holds the previous sample across ticks.
#[derive(Default)]
pub struct Cpu {
    prev: Option<(u64, u64)>,
    last_pct: Option<u64>,
}

impl Cpu {
    /// Samples /proc/stat and returns the usage text. The first call has no
    /// baseline to compare against and reports 0%.
    pub fn text(&mut self) -> String {
        let Some(cur) = read_totals() else {
            return String::new();
        };
        let pct = match self.prev {
            Some(prev) => usage_between(prev, cur).or(self.last_pct).unwrap_or(0),
            None => 0,
        };
        self.prev = Some(cur);
        self.last_pct = Some(pct);
        format!("{}%", pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_aggregate_line() {
        let stat = "cpu  100 20 30 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 0 0 0 0 0\n";
        // total = sum of all fields, idle = idle(800) + iowait(50)
        assert_eq!(parse_totals(stat), Some((1000, 850)));
        assert_eq!(parse_totals("intr 1 2 3\n"), None);
        assert_eq!(parse_totals(""), None);
    }

    #[test]
    fn computes_usage_from_deltas() {
        // 100 jiffies elapsed, 75 idle -> 25% busy.
        assert_eq!(usage_between((1000, 850), (1100, 925)), Some(25));
        // Fully idle and fully busy.
        assert_eq!(usage_between((0, 0), (100, 100)), Some(0));
        assert_eq!(usage_between((0, 0), (100, 0)), Some(100));
        // No time elapsed, or counters going backwards (suspend/resume).
        assert_eq!(usage_between((100, 50), (100, 50)), None);
        assert_eq!(usage_between((100, 50), (50, 20)), None);
    }

    /// First sample has no baseline; later samples stay in range. Crucially
    /// this must not sleep, so both calls return immediately.
    #[test]
    fn stateful_sampling_never_blocks() {
        let mut cpu = Cpu::default();
        let start = std::time::Instant::now();
        assert_eq!(cpu.text(), "0%", "first sample has no baseline");
        let second = cpu.text();
        assert!(start.elapsed() < std::time::Duration::from_millis(50), "must not sleep");

        let pct: u64 = second.trim_end_matches('%').parse().expect("numeric");
        assert!(pct <= 100, "got {second}");
    }
}
