//! MEGA sync: synced or not, nothing else.
//!
//! The widget answers one question — is `~/1Mega` in sync? — so it reports a
//! state rather than a string, and the colour carries the answer.
//!
//! **It talks to `mega-cmd-server` directly**, over the same unix socket its
//! own client uses. The command-line route was `sh -c` → `mega-sync` (a bash
//! wrapper) → `mega-exec` → that socket, and measuring the chain showed the
//! plumbing costing more than the work: **11.5 ms**, of which `mega-exec`'s
//! own startup was 4.3 ms even for `--help`. The socket answers in **0.7 ms**
//! and forks nothing.
//!
//! The protocol is not documented, so it was read off the wire: a request is
//! the command **NUL-terminated** — a plain newline gets `Command not found`,
//! which is what made an earlier attempt conclude it was unusable — and the
//! reply is a magic `0xffffffc2`, a little-endian `u64` length, then the text
//! the command line would have printed. The parser below is unchanged: it
//! reads the same table either way.

use crate::exec;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// Refresh interval. Waybar polled every 30 s; four times slower is still far
/// quicker than sync state actually changes, and cuts the cost by 75%.
pub const INTERVAL_SECS: u64 = 120;

/// What the bar needs to know about the sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sync {
    /// Everything uploaded: the folder matches the cloud.
    Synced,
    /// Running but still working, or paused, or failed — all the same to the
    /// reader: what is on disk is not in the cloud yet.
    Busy,
    /// No sync configured, MEGAcmd not logged in, or the command failed.
    #[default]
    Unknown,
}

impl Sync {
    /// Colour for the icon, which is the whole message.
    pub fn color(self) -> u32 {
        match self {
            Sync::Synced => crate::icons::color::MEGA_SYNCED,
            Sync::Busy => crate::icons::color::MEGA_BUSY,
            Sync::Unknown => crate::icons::color::MEGA_UNKNOWN,
        }
    }
}

/// Reads the sync state out of `mega-sync`'s table.
///
/// Columns are located by name from the header rather than by position. The
/// previous version took field 5 of the second line, which it documented as
/// `RUN_STATE` — but in the real output field 5 is `STATUS`, so it had been
/// reading the wrong column all along. Its test encoded the same mistake, with
/// a made-up header, and so agreed with it.
fn parse(output: &str) -> Sync {
    let mut lines = output.lines();
    let Some(header) = lines.next() else {
        return Sync::Unknown;
    };
    let columns: Vec<&str> = header.split_whitespace().collect();
    let (Some(run_at), Some(status_at)) = (
        columns.iter().position(|c| *c == "RUN_STATE"),
        columns.iter().position(|c| *c == "STATUS"),
    ) else {
        return Sync::Unknown;
    };

    // The first data row: one sync is the normal case, and with several the
    // bar reports the first, having one icon to say it with.
    let Some(row) = lines.next() else {
        return Sync::Unknown; // "Not logged in" has no second line
    };
    let fields: Vec<&str> = row.split_whitespace().collect();
    let (Some(run), Some(status)) = (fields.get(run_at), fields.get(status_at)) else {
        return Sync::Unknown;
    };

    match (*run, *status) {
        ("Running", "Synced") => Sync::Synced,
        // Paused, disabled, failed, or running-but-not-finished: either way
        // the folder is not in sync, which is the only distinction drawn.
        _ => Sync::Busy,
    }
}

/// Frame marker on a reply carrying command output.
const REPLY_MAGIC: u32 = 0xffff_ffc2;

/// The server's socket, beside the state MEGAcmd already keeps there.
fn socket_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".megaCmd/megacmd.socket"))
}

/// Runs one command against `mega-cmd-server`, returning its output.
///
/// Returns `None` when the server is not up or answers something unexpected —
/// the caller then falls back to the command line, which can start it.
fn query(command: &str) -> Option<String> {
    let stream = UnixStream::connect(socket_path()?).ok()?;
    // Measured at 0,7 ms; the ceiling is the project's floor for local IPC
    // (§8), since twice that would trip on scheduling jitter alone.
    let timeout = std::time::Duration::from_millis(100);
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let mut stream = stream;
    // NUL-terminated: a newline is rejected with "Command not found".
    stream.write_all(command.as_bytes()).ok()?;
    stream.write_all(&[0]).ok()?;

    let mut header = [0u8; 12];
    stream.read_exact(&mut header).ok()?;
    if u32::from_le_bytes(header[..4].try_into().ok()?) != REPLY_MAGIC {
        return None; // an error frame, or a protocol we no longer understand
    }
    let len = u64::from_le_bytes(header[4..].try_into().ok()?) as usize;
    // A reply larger than this is not a sync table; refuse to allocate for it.
    if len > 1 << 20 {
        return None;
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Current sync state.
///
/// The socket first, the command line only if that fails. The fallback earns
/// its place: `mega-sync` **starts** `mega-cmd-server` when it is not running,
/// which the socket route cannot do — and it runs in a scope of its own, since
/// a daemon adopted into the bar's cgroup once made every stop hang for the
/// full timeout (ADR-048).
pub fn state() -> Sync {
    if let Some(output) = query("sync") {
        return parse(&output);
    }
    parse(&exec::run_detached_scope("mega-sync 2>/dev/null"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str =
        "ID          LOCALPATH             REMOTEPATH RUN_STATE STATUS ERROR SIZE       FILES DIRS";

    /// The real output of a healthy sync, copied from the machine.
    #[test]
    fn a_running_synced_folder_is_synced() {
        let out = format!(
            "{HEADER}\n\
             eaVyZqCjREE /home/dav...ini/1Mega / Running Synced NO 1719.00  B 4 0"
        );
        assert_eq!(parse(&out), Sync::Synced);
    }

    /// Anything short of Running/Synced means what is on disk is not yet in the
    /// cloud, which is the one thing the widget has to get right.
    #[test]
    fn everything_else_is_busy() {
        for (run, status) in [
            ("Running", "Syncing"),
            ("Running", "Pending"),
            ("Paused", "Synced"),
            ("Disabled", "Stopped"),
            ("Failed", "Error"),
        ] {
            let out = format!("{HEADER}\neaVy /home/x / {run} {status} NO 0 0 0");
            assert_eq!(parse(&out), Sync::Busy, "{run}/{status} must not read as synced");
        }
    }

    /// Not logged in, no sync configured, or the command missing: the widget
    /// says it does not know rather than claiming everything is fine.
    #[test]
    fn absent_or_broken_input_is_unknown() {
        assert_eq!(parse(""), Sync::Unknown);
        assert_eq!(parse("Not logged in"), Sync::Unknown, "no data row");
        assert_eq!(parse(HEADER), Sync::Unknown, "header only");
        assert_eq!(parse("ID PATH\nabc /home/x"), Sync::Unknown, "columns missing");
    }

    /// Columns come from the header, so a different order still reads right.
    /// Reading position 4 blindly is what made the old parser return STATUS
    /// while believing it had RUN_STATE.
    #[test]
    fn column_order_comes_from_the_header() {
        let swapped = "ID LOCALPATH REMOTEPATH STATUS RUN_STATE ERROR\n\
                       eaVy /home/x / Synced Running NO";
        assert_eq!(parse(swapped), Sync::Synced);
    }

    /// Each state has its own colour, since the colour *is* the message.
    #[test]
    fn every_state_looks_different() {
        let colors = [
            Sync::Synced.color(),
            Sync::Busy.color(),
            Sync::Unknown.color(),
        ];
        let unique: std::collections::HashSet<_> = colors.iter().collect();
        assert_eq!(unique.len(), 3, "states must be told apart at a glance");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Reads the real sync on this machine. Skipped where MEGAcmd is absent.
    #[test]
    fn reports_the_real_folder() {
        if !std::path::Path::new("/usr/bin/mega-sync").exists() {
            return;
        }
        let state = state();
        println!("stato reale: {state:?} -> #{:06X}", state.color());
        assert_ne!(state, Sync::Unknown, "MEGAcmd is running, so it must answer");
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    /// The socket must agree with the command line: they read the same server,
    /// so a disagreement means the frame is being parsed wrong.
    #[test]
    fn the_socket_agrees_with_the_command_line() {
        let Some(direct) = query("sync") else {
            return; // no server running
        };
        let via_cli = exec::run_detached_scope("mega-sync 2>/dev/null");
        assert_eq!(
            parse(&direct),
            parse(&via_cli),
            "socket:\n{direct}\ncli:\n{via_cli}"
        );
    }

    /// And it must be the faster of the two, which is the whole reason for it.
    #[test]
    #[ignore = "timing, and forks the command line for comparison"]
    fn the_socket_is_faster() {
        use std::time::Instant;
        if query("sync").is_none() {
            return;
        }
        let t = Instant::now();
        for _ in 0..10 {
            let _ = query("sync");
        }
        let socket = t.elapsed().as_secs_f64() / 10.0 * 1000.0;

        let t = Instant::now();
        for _ in 0..5 {
            let _ = exec::run_detached_scope("mega-sync 2>/dev/null");
        }
        let cli = t.elapsed().as_secs_f64() / 5.0 * 1000.0;

        println!("socket {socket:.1} ms | command line {cli:.1} ms");
        assert!(socket < cli, "the socket was not faster");
    }

    /// A missing or silent server must not hang the bar: this runs inside the
    /// event loop, where a blocking read is a frozen screen.
    #[test]
    fn an_unreachable_server_fails_fast() {
        use std::time::Instant;
        let t = Instant::now();
        // A path that cannot be a socket.
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/nonexistent") };
        let out = query("sync");
        if let Some(h) = previous {
            unsafe { std::env::set_var("HOME", h) };
        }
        assert!(out.is_none());
        assert!(t.elapsed() < std::time::Duration::from_secs(1), "too slow");
    }
}
