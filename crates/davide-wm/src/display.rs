//! Screen blanking until the next input (`blk`), idle blanking (the
//! screensaver of `monitor-manager`) and the lid switch inhibitor
//! (`lid-inhibit-toggle`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// What the idle timer is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Idle {
    Blank,
    /// Power down the monitors kept on with black frames.
    PowerDown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screensaver {
    pub enabled: bool,
    /// Chosen by hand: plugging or unplugging a monitor leaves it alone.
    pub manual: bool,
    /// Seconds without input before blanking.
    pub timeout: u32,
}

impl Default for Screensaver {
    fn default() -> Screensaver {
        Screensaver { enabled: false, manual: false, timeout: config::SCREENSAVER_AUTO_TIMEOUT }
    }
}

fn state_path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    dir.join("davide-wm/screensaver")
}

impl Screensaver {
    /// "enabled manual timeout", e.g. "1 0 60".
    pub fn parse(text: &str) -> Option<Screensaver> {
        let mut it = text.split_whitespace().map(str::parse::<u32>);
        match (it.next(), it.next(), it.next()) {
            (Some(Ok(e)), Some(Ok(m)), Some(Ok(t))) if t > 0 => Some(Screensaver { enabled: e != 0, manual: m != 0, timeout: t }),
            _ => None,
        }
    }

    pub fn load() -> Screensaver {
        std::fs::read_to_string(state_path()).ok().and_then(|t| Screensaver::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = state_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!("{} {} {}\n", self.enabled as u8, self.manual as u8, self.timeout);
        if let Err(e) = std::fs::write(&path, text) {
            log!("cannot save {}: {e}", path.display());
        }
    }

    /// "OFF", or the timeout: "1m", "90s".
    /// Seconds after blanking before black-blanked monitors power down too.
    pub fn power_off_after(&self) -> u64 {
        let t = self.timeout;
        config::BLANK_POWER_OFF.iter().find(|(k, _)| *k == t).map_or(900, |(_, total)| total.saturating_sub(t)) as u64
    }

    pub fn label(&self) -> String {
        match self.timeout {
            _ if !self.enabled => "OFF".into(),
            t if t % 60 == 0 => format!("{}m", t / 60),
            t => format!("{t}s"),
        }
    }

    /// The bar's click: 1m -> 5m -> 30m -> OFF -> 1m, always a manual choice.
    pub fn cycle(&mut self) {
        let cycle = config::SCREENSAVER_CYCLE;
        self.manual = true;
        match cycle.iter().position(|t| *t == self.timeout) {
            _ if !self.enabled => (self.enabled, self.timeout) = (true, cycle[0]),
            Some(i) if i + 1 < cycle.len() => self.timeout = cycle[i + 1],
            Some(_) => self.enabled = false,
            None => self.timeout = cycle[0],
        }
    }

    /// The automatic policy: on while an exclusive monitor is connected,
    /// unless chosen by hand.
    pub fn auto(&mut self, exclusive: bool) {
        if self.manual {
            return;
        }
        self.enabled = exclusive;
        if exclusive {
            self.timeout = config::SCREENSAVER_AUTO_TIMEOUT;
        }
    }
}

/// RGB multipliers for a colour temperature (Tanner Helland's fit of the
/// black-body curve), daylight being 6500 K.
pub fn kelvin(k: u32) -> (f32, f32, f32) {
    let t = k.clamp(1000, 6500) as f32 / 100.0;
    let g = (99.470_8 * t.ln() - 161.119_57) / 255.0;
    let b = if t <= 19.0 { 0.0 } else { (138.517_73 * (t - 10.0).ln() - 305.044_8) / 255.0 };
    (1.0, g.clamp(0.0, 1.0), b.clamp(0.0, 1.0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Night {
    /// On between the hours of `config::NIGHT_LIGHT`.
    Auto,
    On,
    Off,
}

impl Night {
    /// The multipliers to use at `hour`, None for neutral colours.
    pub fn gamma(self, hour: u32) -> Option<(f32, f32, f32)> {
        let (start, end, k) = config::NIGHT_LIGHT;
        let night = if start > end { hour >= start || hour < end } else { hour >= start && hour < end };
        match self {
            Night::On => Some(kelvin(k)),
            Night::Auto if night => Some(kelvin(k)),
            _ => None,
        }
    }
}

pub struct Display {
    pub screensaver: Screensaver,
    timer: OwnedFd,
    last_input: Instant,
    pub blanked: bool,
    blanked_at: Instant,
    /// Blanked long enough that every monitor is powered down.
    pub powered_down: bool,
    grace_until: Instant,
    /// Heads that already show their black frame while blanked.
    pub black_shown: Vec<usize>,
    /// Held while closing the lid must not suspend.
    pub lid_inhibitor: Option<OwnedFd>,
}

impl Display {
    pub fn new(lid_inhibitor: Option<OwnedFd>) -> io::Result<Display> {
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let d = Display {
            screensaver: Screensaver::load(),
            timer: unsafe { OwnedFd::from_raw_fd(fd) },
            last_input: Instant::now(),
            blanked: false,
            blanked_at: Instant::now(),
            powered_down: false,
            grace_until: Instant::now(),
            black_shown: Vec::new(),
            lid_inhibitor,
        };
        d.arm();
        Ok(d)
    }

    pub fn timer_fd(&self) -> RawFd {
        self.timer.as_raw_fd()
    }

    /// What the idle timer waits for, and how long is left.
    fn next(&self) -> Option<(Idle, Duration)> {
        let left = |total: u64, since: Instant| Duration::from_secs(total).saturating_sub(since.elapsed());
        match (self.blanked, self.powered_down) {
            (false, _) if self.screensaver.enabled => Some((Idle::Blank, left(self.screensaver.timeout as u64, self.last_input))),
            // Screensaver off: kept on by choice, black included.
            (true, false) if self.screensaver.enabled => Some((Idle::PowerDown, left(self.screensaver.power_off_after(), self.blanked_at))),
            _ => None,
        }
    }

    /// Arms the idle timer for the remaining time, or disarms it.
    fn arm(&self) {
        let due = self.next().map(|(_, d)| d.max(Duration::from_millis(10)));
        let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        if let Some(d) = due {
            spec.it_value.tv_sec = d.as_secs() as libc::time_t;
            spec.it_value.tv_nsec = d.subsec_nanos() as libc::c_long;
        }
        unsafe { libc::timerfd_settime(self.timer.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
    }

    /// The idle timer fired: what is due now, if anything.
    pub fn timer_fired(&mut self) -> Option<Idle> {
        let mut n = 0u64;
        unsafe { libc::read(self.timer.as_raw_fd(), (&mut n as *mut u64).cast(), 8) };
        match self.next() {
            Some((what, left)) if left.is_zero() => Some(what),
            _ => {
                self.arm();
                None
            }
        }
    }

    pub fn power_down(&mut self) {
        self.powered_down = true;
        self.arm();
    }

    /// Input arrived. Returns true when it must not reach anyone: the screen
    /// is blank (or was until this event, which only wakes it).
    pub fn input(&mut self) -> bool {
        self.last_input = Instant::now();
        self.blanked
    }

    /// Something kept the screen busy (an idle inhibitor): count as input.
    pub fn activity(&mut self) {
        self.last_input = Instant::now();
        self.arm();
    }

    pub fn blank(&mut self) {
        self.blanked = true;
        self.blanked_at = Instant::now();
        self.black_shown.clear();
        self.grace_until = Instant::now() + Duration::from_millis(config::BLANK_GRACE_MS);
        self.arm();
    }

    /// True once the grace after blanking is over: input then wakes.
    pub fn may_wake(&self) -> bool {
        Instant::now() >= self.grace_until
    }

    pub fn wake(&mut self) {
        self.blanked = false;
        self.powered_down = false;
        self.black_shown.clear();
        self.last_input = Instant::now();
        self.arm();
    }

    pub fn set_screensaver(&mut self, s: Screensaver) {
        if s != self.screensaver {
            self.screensaver = s;
            s.save();
        }
        self.arm();
    }

    pub fn toggle_lid_inhibitor(&mut self) -> io::Result<()> {
        if self.lid_inhibitor.take().is_none() {
            self.lid_inhibitor = Some(crate::backend::logind::inhibit("handle-lid-switch", "Closing the lid keeps the session running")?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_goes_round_through_off() {
        let mut s = Screensaver { enabled: false, manual: false, timeout: 60 };
        let mut seen = Vec::new();
        for _ in 0..5 {
            s.cycle();
            seen.push(s.label());
        }
        assert_eq!(seen, ["1m", "5m", "30m", "OFF", "1m"]);
        assert!(s.manual);
    }

    #[test]
    fn manual_choice_survives_the_auto_policy() {
        let mut s = Screensaver::default();
        s.auto(true);
        assert_eq!((s.enabled, s.timeout), (true, 60));
        s.auto(false);
        assert!(!s.enabled);
        s.cycle();
        s.cycle();
        s.auto(false);
        assert_eq!((s.enabled, s.timeout), (true, 300));
    }

    #[test]
    fn night_hours_and_colours() {
        assert_eq!(Night::Auto.gamma(12), None);
        assert!(Night::Auto.gamma(22).is_some() && Night::Auto.gamma(3).is_some());
        assert_eq!(Night::Off.gamma(23), None);
        let (r, g, b) = kelvin(4000);
        assert!(r == 1.0 && g > 0.7 && g < 0.9 && b > 0.5 && b < 0.7, "{g} {b}");
        let (_, g, b) = kelvin(6500);
        assert!(g > 0.95 && b > 0.95);
    }

    #[test]
    fn power_off_follows_the_timeout() {
        let at = |timeout| Screensaver { enabled: true, manual: true, timeout }.power_off_after() + timeout as u64;
        assert_eq!((at(60), at(300), at(1800)), (600, 1800, 2700));
        assert_eq!(at(90), 990);
    }

    #[test]
    fn parse_roundtrip() {
        assert_eq!(Screensaver::parse("1 1 300\n"), Some(Screensaver { enabled: true, manual: true, timeout: 300 }));
        assert_eq!(Screensaver::parse("garbage"), None);
        assert_eq!(Screensaver::parse("1 0 0"), None);
    }
}
