//! Notification count from nwg-notifications, over the session bus.
//!
//! Waybar ran a long-lived subscription process for notifications, polled once a second,
//! polled once a second. Here the same data comes from D-Bus method calls, and
//! nwg-notifications' `CountChanged` signal drives the refresh (ADR-015).

use crate::dbus::DBus;

const NWG: &str = "org.nwg.Notifications";
const CC_PATH: &str = "/org/nwg/Notifications";
const CC_IFACE: &str = "org.nwg.Notifications";

/// Long-lived session-bus connection for the notifications widget.
#[derive(Default)]
pub struct Notifications {
    bus: Option<DBus>,
}

/// Renders the count.
fn render(count: Option<u32>) -> String {
    match count {
        None => String::new(),
        Some(n) => n.to_string(),
    }
}

impl Notifications {
    fn ensure(&mut self) -> Option<&mut DBus> {
        if self.bus.is_none() {
            let mut bus = DBus::session()?;
            bus.add_match(&format!(
                "type='signal',interface='{}',path='{}',member='CountChanged'",
                CC_IFACE, CC_PATH
            ));
            self.bus = Some(bus);
        }
        self.bus.as_mut()
    }

    pub fn text(&mut self) -> String {
        let Some(bus) = self.ensure() else {
            return String::new();
        };
        let count = bus.call_u32(NWG, CC_PATH, CC_IFACE, "GetCount");
        if count.is_none() {
            // nwg-notifications not running, or the connection went stale.
            self.bus = None;
            return String::new();
        }
        render(count)
    }

    /// Consumes pending signals; true when something changed.
    pub fn drain_events(&mut self) -> bool {
        match self.bus.as_mut() {
            Some(bus) => bus.drain_signals(),
            None => false,
        }
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

    #[test]
    fn renders_the_count() {
        assert_eq!(render(Some(0)), "0");
        assert_eq!(render(Some(232)), "232");
        assert_eq!(render(None), "", "nwg-notifications absent renders nothing");
    }

    /// Parity with the daemon's count method, when it is running.
    #[test]
    fn agrees_with_nwg_notifications() {
        let mut n = Notifications::default();
        let text = n.text();
        if text.is_empty() {
            return;
        }
        let ours: u32 = text
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .expect("leading count");
        assert!(ours < 100_000, "count should be a sane positive integer");
    }
}
