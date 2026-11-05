//! CPU power levels 1–5 (absorbs dawn's `cpu-cycle` and `cpu-levels`): the
//! Huawei WMI policy, frequencies and TDP. Those writes need root: they go
//! through `sudo -n` to the same helpers as before, on the worker.

use std::process::{Command, Stdio};

struct Level {
    policy: [u64; 3],
    freq: (u32, u32, u32),
    tdp: (u32, u32, u32),
    label: &'static str,
}

const LEVELS: [Level; 5] = [
    Level { policy: [0, 0, 0], freq: (1_400_000, 1_400_000, 0), tdp: (4000, 5000, 4000), label: "Livello 1 — minimo (4W, 1.4 GHz)" },
    Level { policy: [0, 0, 0], freq: (1_400_000, 1_700_000, 0), tdp: (12000, 15000, 12000), label: "Livello 2 — basso (12W, 1.7 GHz)" },
    Level { policy: [1, 0, 0], freq: (1_400_000, 3_000_000, 0), tdp: (18000, 22000, 18000), label: "Livello 3 — medio (18W, 3.0 GHz)" },
    Level { policy: [1, 0, 1], freq: (1_400_000, 3_000_000, 0), tdp: (28000, 35000, 28000), label: "Livello 4 — alto (28W, 3.0 GHz)" },
    Level { policy: [1, 1, 1], freq: (1_400_000, 3_000_000, 1), tdp: (35000, 60000, 42000), label: "Livello 5 — massimo (35W, boost)" },
];

const HUAWEI_SCRIPT: &str = r#"printf "%s" "$1" > /sys/kernel/debug/huawei-wmi/arg && cat /sys/kernel/debug/huawei-wmi/call >/dev/null"#;

fn state() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/cpu-level")
}

fn sudo(args: &[&str]) {
    let _ = Command::new("sudo").arg("-n").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// The level applied last (1–5).
pub fn current() -> Option<usize> {
    std::fs::read_to_string(state()).ok().and_then(|t| t.trim().parse().ok()).filter(|n| (1..=5).contains(n))
}

pub fn label(level: usize) -> &'static str {
    LEVELS[level.clamp(1, 5) - 1].label
}

/// Applies the level after the current one (1 after 5); returns its label.
pub fn cycle() -> String {
    set(current().unwrap_or(0) % 5 + 1)
}

/// Applies a level (1–5); returns its label.
pub fn set(next: usize) -> String {
    let next = next.clamp(1, 5);
    let l = &LEVELS[next - 1];
    for (command, value) in [0x1304u64, 0x0f04, 0x1104].into_iter().zip(l.policy) {
        sudo(&["sh", "-c", HUAWEI_SCRIPT, "sh", &format!("0x{:016x}", command | (value << 16))]);
    }
    let (fmin, fmax, boost) = l.freq;
    sudo(&["/usr/local/sbin/cpu-freq-set", "schedutil", &fmin.to_string(), &fmax.to_string(), &boost.to_string()]);
    let (tmin, tmax, twork) = l.tdp;
    sudo(&["/usr/local/sbin/ryzen-tdp-set", &tmin.to_string(), &tmax.to_string(), &twork.to_string()]);
    let _ = std::fs::write(state(), format!("{next}\n"));
    l.label.to_string()
}
