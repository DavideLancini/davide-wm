//! Clock: "HH:MM DD/MM/YY", computed from the system clock without spawning
//! `date`. Civil-date conversion follows Howard Hinnant's algorithm.

use std::time::{SystemTime, UNIX_EPOCH};

/// glibc's `struct tm`; only `tm_gmtoff` is read.
#[repr(C)]
struct Tm {
    fields: [std::os::raw::c_int; 9],
    tm_gmtoff: std::os::raw::c_long,
    tm_zone: *const std::os::raw::c_char,
}

extern "C" {
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
}

/// Local UTC offset in seconds at `unix_secs`, DST included. `daylight` only
/// says the zone *has* DST, so the offset must come from `localtime_r`.
fn local_offset_secs(unix_secs: i64) -> i64 {
    let mut tm = Tm { fields: [0; 9], tm_gmtoff: 0, tm_zone: std::ptr::null() };
    if unsafe { localtime_r(&unix_secs, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

/// Days since 1970-01-01 -> (year, month, day). Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats a Unix timestamp as local "HH:MM DD/MM/YY".
fn format_at(unix_secs: i64, offset_secs: i64, with_date: bool) -> String {
    let local = unix_secs + offset_secs;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let time = format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60);
    if with_date {
        format!("{time} {:02}/{:02}/{:02}", day, month, year.rem_euclid(100))
    } else {
        time
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The bar's clock, with the date only when asked for.
///
/// The date costs 96 px — a third of the left half — and is knowable from a
/// dozen other places, so it is off by default and a click brings it back
/// (ADR-043).
pub fn text(with_date: bool) -> String {
    let now = now_unix();
    format_at(now, local_offset_secs(now), with_date)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_timestamps_in_utc() {
        assert_eq!(format_at(1_785_600_300, 0, false), "16:05");
        assert_eq!(format_at(0, 0, false), "00:00");
        assert_eq!(format_at(1_709_208_000, 0, false), "12:00");
        assert_eq!(format_at(1_709_121_600, 0, false), "12:00");
        assert_eq!(format_at(946_684_800, 0, false), "00:00");

        // With the date, which is what a click reveals. The calendar
        // arithmetic is the part that is easy to get wrong: a leap day, the
        // day before one, and a century boundary.
        assert_eq!(format_at(1_785_600_300, 0, true), "16:05 01/08/26");
        assert_eq!(format_at(0, 0, true), "00:00 01/01/70");
        assert_eq!(format_at(1_709_208_000, 0, true), "12:00 29/02/24");
        assert_eq!(format_at(1_709_121_600, 0, true), "12:00 28/02/24");
        assert_eq!(format_at(946_684_800, 0, true), "00:00 01/01/00");
    }

    #[test]
    fn applies_the_local_offset() {
        // +2h pushes 16:05 to 18:05.
        assert_eq!(format_at(1_785_600_300, 7200, false), "18:05");
        // Midnight UTC crosses into the next or previous day, which only the
        // dated form can show.
        assert_eq!(format_at(1_785_628_800, 7200, true), "02:00 02/08/26");
        assert_eq!(format_at(1_785_628_800, -7200, true), "22:00 01/08/26");
    }

    /// Parity with the command the Waybar setup effectively used.
    #[test]
    fn matches_the_system_date_command() {
        let out = std::process::Command::new("date")
            .arg("+%H:%M")
            .output()
            .expect("date");
        let expected = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(text(false), expected);
    }

}

