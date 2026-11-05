//! Running external commands (the command itself is a compile-time constant).

use std::process::Command;

/// How long a helper may take before it is abandoned.
///
/// These calls are **synchronous inside the event loop**: while one runs, the
/// bar draws nothing and answers no clicks.
///
/// Measured worst cases over 25 runs each: `timer.sh` 12,1 ms, `cliphist`
/// 11,2 ms, `lid-status` 6,8 ms. Twice the worst is ~25 ms, but a fork under
/// memory pressure can take far longer than one on an idle machine, so this
/// keeps the 500 ms floor the project uses for anything that spawns a process
/// (§8). It is still eighty times the normal case.
const TIMEOUT: &str = "0.5";

/// A moment more before SIGKILL, for a command that ignores SIGTERM.
///
/// `mega-cmd-server` is exactly that kind of program, so the escalation is not
/// hypothetical.
const KILL_AFTER: &str = "1";

/// Same, but in a cgroup of its own, for commands that leave a daemon behind.
///
/// `mega-sync` starts `mega-cmd-server` when it is not already up. A child
/// inherits our cgroup, so that daemon ended up inside the service running the
/// bar (then `topbar.service`) — where systemd waited for it on every stop, and
/// `mega-cmd-server` ignores SIGTERM. Stopping stalled for the whole
/// `TimeoutStopSec`: the freeze this was tracked down from.
///
/// **`setsid` does not fix it**, which was the first attempt: a session is not
/// a cgroup, and the child stays in ours — measured, not assumed.
/// `systemd-run --user --scope` does, by placing the command in its own scope.
///
/// Kept separate from `run` because it costs **21.5 ms against 3.6 ms**, six
/// times as much. That is irrelevant for a widget sampled every two minutes
/// and unacceptable for one sampled every tick, so only the callers that
/// actually spawn a daemon pay it.
///
/// Falls back to a plain shell where `systemd-run` is missing: the adoption
/// comes back, but the bar keeps working.
pub fn run_detached_scope(cmd: &str) -> String {
    let output = Command::new("systemd-run")
        .args([
            "--user", "--scope", "--quiet", "--collect",
            "timeout", "--kill-after", KILL_AFTER, TIMEOUT, "sh", "-c", cmd,
        ])
        .output()
        .or_else(|_| Command::new("sh").args(["-c", cmd]).output());
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    /// Output and exit status must survive the extra `setsid` layer: it is
    /// easy to wrap a command in a way that swallows what it printed.
    #[test]
    fn output_still_comes_back() {
        assert_eq!(run_detached_scope("echo hello"), "hello");
        assert_eq!(run_detached_scope("printf '  spaces  '"), "spaces", "trimmed");
        // A failing command yields nothing rather than its partial output.
        assert_eq!(run_detached_scope("echo discarded; false"), "");
        assert_eq!(run_detached_scope("command-that-does-not-exist 2>/dev/null"), "");
    }

    /// The scoped form must land in a different cgroup from ours: that is the
    /// whole point, and a session id would not prove it — `setsid` changed the
    /// session and the cgroup stayed the same.
    #[test]
    fn the_scoped_form_leaves_our_cgroup() {
        let ours = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let ours = ours.trim().to_string();
        if ours.is_empty() {
            return; // no cgroup v2 here
        }
        let scoped = run_detached_scope("cat /proc/self/cgroup").trim().to_string();
        if scoped.is_empty() {
            return; // no systemd-run
        }
        assert_ne!(
            scoped, ours,
            "the command shares our cgroup: a daemon it starts would be adopted"
        );
    }

    /// The scoped form must still return output like the plain one, or the
    /// widget using it silently reads nothing.
    #[test]
    fn the_scoped_form_still_returns_output() {
        let out = run_detached_scope("echo hello");
        // Empty when systemd-run is absent; otherwise it must match.
        if !out.is_empty() {
            assert_eq!(out, "hello");
        }
    }
}
