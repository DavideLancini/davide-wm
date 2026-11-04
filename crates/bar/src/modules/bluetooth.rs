//! Bluetooth controller state, via BlueZ over D-Bus.
//!
//! Waybar's `bluetooth` module showed an icon per state; as a text bar we
//! render "BT" when powered, "BT off" when not, and nothing at all when there
//! is no adapter. Property changes arrive as PropertiesChanged signals, so the
//! widget refreshes on trigger rather than on a timer (ADR-015).

use crate::dbus::DBus;
use crate::icons;

const BLUEZ: &str = "org.bluez";
const ADAPTER_PATH: &str = "/org/bluez/hci0";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";

/// Long-lived D-Bus connection for the bluetooth widget.
#[derive(Default)]
pub struct Bluetooth {
    bus: Option<DBus>,
}

fn render(powered: Option<bool>) -> String {
    match powered {
        Some(true) => icons::BLUETOOTH.to_string(),
        Some(false) => format!("{} off", icons::BLUETOOTH),
        None => String::new(),
    }
}

impl Bluetooth {
    /// Connects and subscribes to adapter property changes.
    fn ensure(&mut self) -> Option<&mut DBus> {
        if self.bus.is_none() {
            let mut bus = DBus::system()?;
            bus.add_match(&format!(
                "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path='{}'",
                ADAPTER_PATH
            ));
            self.bus = Some(bus);
        }
        self.bus.as_mut()
    }

    pub fn text(&mut self) -> String {
        let Some(bus) = self.ensure() else {
            return String::new();
        };
        let powered = bus
            .get_property(BLUEZ, ADAPTER_PATH, ADAPTER_IFACE, "Powered")
            .and_then(|v| v.as_bool());
        if powered.is_none() {
            // Either no adapter, or the connection went stale: retry next time.
            self.bus = None;
        }
        render(powered)
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
    fn renders_each_state() {
        assert_eq!(render(Some(true)), icons::BLUETOOTH.to_string());
        assert_eq!(render(Some(false)), format!("{} off", icons::BLUETOOTH));
        assert_eq!(render(None), "", "no adapter renders nothing");
    }

    /// Parity with `bluetoothctl show`, when BlueZ is present.
    #[test]
    fn agrees_with_bluetoothctl() {
        let mut bt = Bluetooth::default();
        let ours = bt.text();

        let out = std::process::Command::new("bluetoothctl").arg("show").output();
        let Ok(out) = out else { return };
        let text = String::from_utf8_lossy(&out.stdout);
        if !text.contains("Powered:") {
            return; // no adapter to compare against
        }
        let expected = render(Some(text.contains("Powered: yes")));
        assert_eq!(ours, expected, "bluetoothctl disagrees");
    }
}
