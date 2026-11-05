//! Click actions per widget, mirroring what the Waybar config bound.
//!
//! Every command runs detached (`setsid`) so a GUI tool the user leaves open —
//! `pavucontrol`, `nmgui` — cannot outlive-block or kill the bar, and so the
//! bar never waits on it.

/// Which widget a hit box belongs to. Workspaces carry their target instead,
/// since each entry maps to a different workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A workspace entry: switch to it.
    Workspace { id: i32, name: String },
    /// A widget with fixed commands.
    Widget(Widget),
}

/// Widgets that respond to clicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    CpuFreq,
    Volume,
    Network,
    BtHeadphones,
    Backlight,
    Blk,
    Screensaver,
    Timer,
    Lid,
    Notifications,
    Screenshot,
    Spotify,
    Microphone,
    Clock,
}

impl Widget {
    /// Shell command for a left click, or None when the widget runs nothing.
    ///
    /// **One action per widget, on the left button only.** The Waybar setup
    /// hid extras behind the middle and right buttons — dismiss a timer, wake
    /// the screens, toggle do-not-disturb — and nothing on screen said they
    /// were there: a bar is clicked by sight, so an action you cannot see is
    /// an action you do not use (ADR-042).
    ///
    /// `Volume` and `Microphone` are absent on purpose: they mute through the
    /// PulseAudio connection the bar already holds, not through a command.
    /// `Spotify` asks the compositor to focus its window.
    pub fn command(self) -> Option<&'static str> {
        use Widget::*;
        Some(match self {
            CpuFreq => "/home/davidelancini/7Scripts/cpu-cycle",
            BtHeadphones => "blueman-manager",
            // Through `brightness`, not a write to sysfs: that only reached
            // the laptop panel, leaving any external monitor lit. The helper
            // drives those over DDC as well.
            Backlight => "/home/davidelancini/7Scripts/brightness set 0",
            Timer => "/home/davidelancini/0Projects/myArch/configs/waybar/set-timer.sh",
            // The same command the Print key runs: select a region, then hand
            // it to the editor rather than straight to a file.
            Screenshot => "grim -g \"$(slurp)\" - | swappy -f -",
            // Handled by the bar thread or the compositor: these change state
            // rather than running anything.
            Volume | Microphone | Clock | Spotify | Blk | Screensaver | Lid | Notifications | Network => return None,
        })
    }
}

/// Runs a click action detached from the bar, so the loop never waits on it.
pub fn run_detached(command: &str) {
    use std::process::{Command, Stdio};
    let _ = Command::new("setsid")
        .args(["-f", "sh", "-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        // `setsid -f` forks and exits at once: reaping it here is immediate
        // and leaves no zombie behind in the compositor.
        .and_then(|mut child| child.wait());
}

/// The screenshot command relies on the shell for both the pipe and the
/// command substitution around `slurp`, so it must not be split into argv.
#[cfg(test)]
#[test]
fn screenshot_needs_a_shell() {
    let cmd = Widget::Screenshot.command().expect("bound to left click");
    assert!(cmd.contains("$(slurp)"), "the region has to be picked interactively");
    assert!(cmd.contains('|'), "grim's output is piped to the editor");
}

#[cfg(test)]
mod tests {
    use super::Widget::*;
    use super::*;

    /// One action per widget, all of it on the left button.
    #[test]
    fn covers_the_waybar_bindings() {
        assert_eq!(CpuFreq.command().unwrap(), "/home/davidelancini/7Scripts/cpu-cycle");
        assert_eq!(Network.command().unwrap(), "nmgui");
        assert_eq!(BtHeadphones.command().unwrap(), "blueman-manager");
        assert!(Backlight.command().unwrap().ends_with("brightness set 0"));
        assert!(Timer.command().unwrap().contains("set-timer.sh"));
    }

    /// The audio widgets mute through the PulseAudio connection the bar keeps
    /// open, the clock toggles bar state, and Spotify, blanking, the
    /// screensaver and the lid inhibitor belong to the compositor, so none
    /// of them may shell out.
    #[test]
    fn stateful_widgets_have_no_command() {
        for w in [Volume, Microphone, Clock, Spotify, Blk, Screensaver, Lid, Notifications] {
            assert!(w.command().is_none(), "{w:?} must not shell out");
        }
    }

    /// Every other widget keeps its action.
    #[test]
    fn command_widgets_keep_their_action() {
        let widgets = [
            CpuFreq, Network, BtHeadphones, Backlight, Timer, Screenshot,
        ];
        for w in widgets {
            assert!(w.command().is_some(), "{w:?} lost its action");
        }
    }

    /// The command actually runs, detached, and the bar does not wait on it.
    #[test]
    fn run_detached_executes_and_returns_immediately() {
        let marker = std::env::temp_dir().join(format!("bar-click-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        // Sleep first: if run_detached waited on the child, this test would
        // take a second rather than milliseconds.
        let start = std::time::Instant::now();
        run_detached(&format!("sleep 0.4; touch {}", marker.display()));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "run_detached must not block on the child"
        );

        // The detached command still completes.
        for _ in 0..40 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(marker.exists(), "detached command did not run");
        let _ = std::fs::remove_file(&marker);
    }
}
