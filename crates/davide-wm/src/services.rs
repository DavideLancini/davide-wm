//! Small system services driven from the compositor: power actions
//! (logind), media keys (MPRIS), Bluetooth devices (BlueZ) and the battery
//! watch. The D-Bus calls block: they run on the worker, each on its own
//! connection.

use std::time::Duration;

use dbus::{Bus, Message, Value};

const TIMEOUT: Duration = Duration::from_secs(3);

fn err(e: dbus::Error) -> String {
    e.to_string()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Power {
    Suspend,
    Reboot,
    Shutdown,
}

/// Asks logind to suspend, reboot or power off.
pub fn power(p: Power) -> Result<(), String> {
    let member = match p {
        Power::Suspend => "Suspend",
        Power::Reboot => "Reboot",
        Power::Shutdown => "PowerOff",
    };
    let mut bus = Bus::system().map_err(err)?;
    let m = Message::call("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", member, vec![Value::Bool(false)]);
    bus.call(m, TIMEOUT).map(|_| ()).map_err(err)
}

pub use config::Media;

/// Sends a media key to the player that is playing, else to the first one.
pub fn media(key: Media) -> Result<(), String> {
    let mut bus = Bus::session().map_err(err)?;
    let names = bus.call(Message::call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames", Vec::new()), TIMEOUT).map_err(err)?;
    let players: Vec<String> = match names.body.first() {
        Some(Value::Array(_, n)) => n.iter().filter_map(|v| v.as_str()).filter(|n| n.starts_with("org.mpris.MediaPlayer2.")).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    let playing = |bus: &mut Bus, p: &str| {
        let m = Message::call(p, "/org/mpris/MediaPlayer2", "org.freedesktop.DBus.Properties", "Get", vec![Value::Str("org.mpris.MediaPlayer2.Player".into()), Value::Str("PlaybackStatus".into())]);
        bus.call(m, TIMEOUT).ok().and_then(|r| r.body.first().and_then(|v| v.as_str().map(|s| s == "Playing"))).unwrap_or(false)
    };
    let target = players.iter().find(|p| playing(&mut bus, p)).or(players.first()).ok_or("no media player")?.clone();
    let member = match key {
        Media::PlayPause => "PlayPause",
        Media::Next => "Next",
        Media::Previous => "Previous",
        Media::Stop => "Stop",
    };
    bus.call(Message::call(&target, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", member, Vec::new()), TIMEOUT).map(|_| ()).map_err(err)
}

#[derive(Clone, Debug, PartialEq)]
pub struct BtDevice {
    pub path: String,
    pub name: String,
    pub connected: bool,
}

/// The adapter's power and the paired devices.
pub fn bt_devices() -> Result<(bool, Vec<BtDevice>), String> {
    let mut bus = Bus::system().map_err(err)?;
    let reply = bus.call(Message::call("org.bluez", "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", Vec::new()), TIMEOUT).map_err(err)?;
    let Some(Value::Array(_, objects)) = reply.body.first() else { return Err("no BlueZ objects".into()) };
    let mut powered = false;
    let mut devices = Vec::new();
    for obj in objects {
        let Value::DictEntry(path, ifaces) = obj else { continue };
        if let Some(a) = ifaces.get("org.bluez.Adapter1") {
            powered |= a.get("Powered").and_then(Value::as_bool).unwrap_or(false);
        }
        let Some(d) = ifaces.get("org.bluez.Device1") else { continue };
        if d.get("Paired").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = d.get("Alias").or(d.get("Name")).and_then(Value::as_str).unwrap_or("?").to_string();
        let connected = d.get("Connected").and_then(Value::as_bool).unwrap_or(false);
        devices.push(BtDevice { path: path.as_str().unwrap_or("").to_string(), name, connected });
    }
    devices.sort_by(|a, b| b.connected.cmp(&a.connected).then(a.name.cmp(&b.name)));
    Ok((powered, devices))
}

/// Connects or disconnects a device; returns a line for the indicator.
pub fn bt_toggle(d: &BtDevice) -> Result<String, String> {
    let mut bus = Bus::system().map_err(err)?;
    let member = if d.connected { "Disconnect" } else { "Connect" };
    bus.call(Message::call("org.bluez", &d.path, "org.bluez.Device1", member, Vec::new()), Duration::from_secs(20)).map_err(err)?;
    Ok(format!("{} {}", d.name, if d.connected { "disconnesso" } else { "connesso" }))
}

/// Turns the first adapter on or off.
pub fn bt_power(on: bool) -> Result<String, String> {
    let mut bus = Bus::system().map_err(err)?;
    let set = Message::call(
        "org.bluez",
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties",
        "Set",
        vec![Value::Str("org.bluez.Adapter1".into()), Value::Str("Powered".into()), Value::Variant(Box::new(Value::Bool(on)))],
    );
    bus.call(set, TIMEOUT).map_err(err)?;
    Ok(format!("Bluetooth {}", if on { "acceso" } else { "spento" }))
}

/// Battery level and whether it is discharging, from sysfs.
pub fn battery() -> Option<(u32, bool)> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?.flatten().map(|e| e.path()).find(|p| {
        std::fs::read_to_string(p.join("type")).is_ok_and(|t| t.trim() == "Battery")
    })?;
    let level = std::fs::read_to_string(dir.join("capacity")).ok()?.trim().parse().ok()?;
    let discharging = std::fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "Discharging");
    Some((level, discharging))
}

/// What the battery watch says: nothing, a warning, or suspend now.
#[derive(Debug, PartialEq)]
pub enum BatteryAlert {
    None,
    Low(u32),
    Critical(u32),
    Suspend,
}

/// The battery watch's state: each threshold warns once per discharge.
#[derive(Default)]
pub struct BatteryWatch {
    warned: u32,
}

impl BatteryWatch {
    pub fn check(&mut self, level: u32, discharging: bool) -> BatteryAlert {
        if !discharging {
            self.warned = 0;
            return BatteryAlert::None;
        }
        let (low, critical, suspend) = config::BATTERY_THRESHOLDS;
        match level {
            l if l <= suspend => BatteryAlert::Suspend,
            l if l <= critical && self.warned != 2 => {
                self.warned = 2;
                BatteryAlert::Critical(l)
            }
            l if l <= low && self.warned == 0 => {
                self.warned = 1;
                BatteryAlert::Low(l)
            }
            _ => BatteryAlert::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_warns_once_per_threshold() {
        let mut w = BatteryWatch::default();
        assert_eq!(w.check(50, true), BatteryAlert::None);
        assert_eq!(w.check(15, true), BatteryAlert::Low(15));
        assert_eq!(w.check(14, true), BatteryAlert::None);
        assert_eq!(w.check(5, true), BatteryAlert::Critical(5));
        assert_eq!(w.check(4, true), BatteryAlert::None);
        assert_eq!(w.check(3, true), BatteryAlert::Suspend);
        assert_eq!(w.check(3, false), BatteryAlert::None);
        assert_eq!(w.check(15, true), BatteryAlert::Low(15));
    }
}
