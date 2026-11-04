//! Desktop notifications, watched on the session bus.
//!
//! nwg-notifications only exposes a count, not the individual notifications, so the app
//! name has to be read from the `Notify` calls themselves as they travel to
//! it. A match rule with `eavesdrop` asks the daemon for a copy of messages
//! addressed to someone else — the same mechanism `dbus-monitor` uses.
//!
//! What this buys: a notification can be attributed to the workspace holding
//! the app that raised it, and the compositor marks that workspace urgent
//! (ADR-036).

use crate::dbus::{DBus, Reader};

const NOTIFICATIONS_IFACE: &str = "org.freedesktop.Notifications";

/// Watches `Notify` calls and reports which app raised each one.
#[derive(Default)]
pub struct Notify {
    bus: Option<DBus>,
}


impl Notify {
    /// Connects and asks for a copy of every `Notify` call on the bus.
    fn ensure(&mut self) -> Option<&mut DBus> {
        if self.bus.is_none() {
            let mut bus = DBus::session()?;
            // Becoming a monitor is what makes the daemon forward calls
            // addressed to the notification server rather than to us. The rule
            // is narrow on purpose: a monitor sees the whole bus otherwise, and
            // decoding traffic nobody asked for would cost more than the widget.
            bus.become_monitor(&[&format!(
                "type='method_call',interface='{}',member='Notify'",
                NOTIFICATIONS_IFACE
            )])?;
            self.bus = Some(bus);
        }
        self.bus.as_mut()
    }

    /// Establishes the connection, if it is not up yet.
    pub fn connect(&mut self) {
        let _ = self.ensure();
    }

    /// App names from the notifications that arrived since the last call.
    pub fn drain(&mut self) -> Vec<String> {
        let Some(bus) = self.bus.as_mut() else {
            return Vec::new();
        };
        bus.drain_messages()
            .into_iter()
            .filter(|m| m.member.as_deref() == Some("Notify"))
            // The first argument of Notify is the application name.
            .filter_map(|m| Reader::new(&m.body).string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// Whether the bus connection died. A closed descriptor stays readable
    /// for ever under level-triggered epoll, so the loop must drop the source
    /// instead of polling it (ADR-049).
    pub fn bus_closed(&self) -> bool {
        self.bus.as_ref().is_some_and(|b| b.is_closed())
    }

    pub fn raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        self.bus.as_ref().map(|b| b.stream().as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connecting is optional: without a session bus the watcher stays inert
    /// rather than failing, exactly like the other D-Bus-backed widgets.
    #[test]
    fn absent_bus_is_harmless() {
        let mut notify = Notify::default();
        assert!(notify.drain().is_empty(), "no bus, no notifications");
        assert!(notify.raw_fd().is_none());
    }

    /// With a live bus the watcher registers and reports nothing until a
    /// notification actually arrives.
    #[test]
    fn connects_without_reporting_anything() {
        let mut notify = Notify::default();
        notify.connect();
        if notify.raw_fd().is_none() {
            return; // no session bus here
        }
        assert!(notify.drain().is_empty(), "nothing raised yet");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Live check: a real notification must reach the watcher. Skipped when
    /// there is no session bus or no `notify-send` to raise one with; ignored
    /// by default because it pops a notification on the desktop.
    #[test]
    #[ignore = "raises a desktop notification"]
    fn sees_a_real_notification() {
        let mut notify = Notify::default();
        notify.connect();
        if notify.raw_fd().is_none() {
            return;
        }
        let sent = std::process::Command::new("notify-send")
            .args(["-a", "BarProbe", "probe"])
            .status();
        if !matches!(sent, Ok(s) if s.success()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        let apps = notify.drain();
        assert!(
            apps.iter().any(|a| a == "BarProbe"),
            "eavesdrop delivered nothing: {apps:?}"
        );
    }
}
