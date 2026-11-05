//! What a click on the bar hits. Every widget acts through the bar's own
//! connections or through a message to the compositor: none runs a program.
//!
//! One action per widget, on the left button only (topbar's ADR-042): an
//! action hidden behind another button is an action nobody finds.

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
    Battery,
}
