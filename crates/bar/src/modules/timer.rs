//! Countdown timer. Started from the compositor's picker (`ToBar::Timer`);
//! its end time is kept in `$XDG_STATE_HOME/davide-wm/timer` so it survives
//! restarts. When it runs out the bar tells the compositor (`FromBar::TimerDone`).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Timer {
    /// Epoch seconds the countdown ends at.
    end: Option<u64>,
    loaded: bool,
    /// Ran out since the last look: the bar reports it once.
    pub done: bool,
}

fn path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    dir.join("davide-wm/timer")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// "mm:ss", or "h:mm:ss" from an hour up.
pub fn format(left: u64) -> String {
    match left {
        l if l >= 3600 => format!("{}:{:02}:{:02}", l / 3600, l / 60 % 60, l % 60),
        l => format!("{:02}:{:02}", l / 60, l % 60),
    }
}

impl Timer {
    pub fn text(&mut self) -> String {
        if !self.loaded {
            self.loaded = true;
            self.end = std::fs::read_to_string(path()).ok().and_then(|t| t.trim().parse().ok());
        }
        match self.end.map(|e| e.saturating_sub(now())) {
            Some(0) => {
                self.set(None);
                self.done = true;
                "--:--".into()
            }
            Some(left) => format(left),
            None => "--:--".into(),
        }
    }

    /// Starts a countdown of `secs`, or stops it.
    pub fn set(&mut self, secs: Option<u64>) {
        self.loaded = true;
        self.end = secs.map(|s| now() + s);
        let p = path();
        match self.end {
            Some(e) => {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&p, e.to_string());
            }
            None => {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats() {
        assert_eq!(super::format(65), "01:05");
        assert_eq!(super::format(3725), "1:02:05");
        assert_eq!(super::format(0), "00:00");
    }
}
