//! Wi-Fi through NetworkManager's D-Bus API (replaces nmgui and nm-applet's
//! menu). Blocking calls: these run on the worker, each on its own system
//! bus connection; passwords come back through the secret agent (`agents.rs`).

use std::time::Duration;

use dbus::{Bus, Message, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const PROPS: &str = "org.freedesktop.DBus.Properties";
const TIMEOUT: Duration = Duration::from_secs(3);
const DEVICE_WIFI: u32 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub ssid: String,
    pub strength: u8,
    pub secured: bool,
    pub active: bool,
    pub ap: String,
    pub device: String,
}

impl Network {
    pub fn label(&self) -> String {
        let lock = if self.secured { " · protetta" } else { "" };
        let on = if self.active { " · connessa" } else { "" };
        format!("{} · {}%{lock}{on}", self.ssid, self.strength)
    }
}

fn call(bus: &mut Bus, path: &str, iface: &str, member: &str, args: Vec<Value>) -> Result<Vec<Value>, String> {
    bus.call(Message::call(NM, path, iface, member, args), TIMEOUT).map(|m| m.body).map_err(|e| e.to_string())
}

fn prop(bus: &mut Bus, path: &str, iface: &str, name: &str) -> Result<Value, String> {
    let body = call(bus, path, PROPS, "Get", vec![Value::Str(iface.into()), Value::Str(name.into())])?;
    body.into_iter().next().map(|v| v.unwrap_variant().clone()).ok_or_else(|| format!("no {name}"))
}

fn paths(v: &Value) -> Vec<String> {
    match v.unwrap_variant() {
        Value::Array(_, items) => items.iter().filter_map(|p| p.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// The Wi-Fi devices.
fn wifi_devices(bus: &mut Bus) -> Result<Vec<String>, String> {
    let devices = call(bus, NM_PATH, NM, "GetDevices", Vec::new())?;
    let all = devices.first().map(paths).unwrap_or_default();
    Ok(all.into_iter().filter(|d| prop(bus, d, "org.freedesktop.NetworkManager.Device", "DeviceType").ok().and_then(|v| v.as_u32()) == Some(DEVICE_WIFI)).collect())
}

/// Visible networks, strongest first, one entry per name. Asks for a new
/// scan too; its results show the next time.
pub fn scan() -> Result<Vec<Network>, String> {
    let mut bus = Bus::system().map_err(|e| e.to_string())?;
    let mut out: Vec<Network> = Vec::new();
    for dev in wifi_devices(&mut bus)? {
        let wireless = "org.freedesktop.NetworkManager.Device.Wireless";
        let _ = call(&mut bus, &dev, wireless, "RequestScan", vec![Value::dict(Vec::new())]);
        let active = prop(&mut bus, &dev, wireless, "ActiveAccessPoint").ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        let aps = paths(&prop(&mut bus, &dev, wireless, "AccessPoints")?);
        for ap in aps {
            let Ok(body) = call(&mut bus, &ap, PROPS, "GetAll", vec![Value::Str("org.freedesktop.NetworkManager.AccessPoint".into())]) else { continue };
            let Some(all) = body.first() else { continue };
            let ssid = all.get("Ssid").and_then(Value::bytes).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            if ssid.is_empty() {
                continue;
            }
            let strength = all.get("Strength").and_then(Value::as_u32).unwrap_or(0) as u8;
            let flags = ["Flags", "WpaFlags", "RsnFlags"].map(|k| all.get(k).and_then(Value::as_u32).unwrap_or(0));
            let net = Network { ssid, strength, secured: flags.iter().any(|f| *f != 0), active: ap == active, ap: ap.clone(), device: dev.clone() };
            match out.iter_mut().find(|n| n.ssid == net.ssid) {
                Some(n) if net.active || (!n.active && net.strength > n.strength) => *n = net,
                Some(_) => {}
                None => out.push(net),
            }
        }
    }
    out.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.cmp(&a.strength)));
    Ok(out)
}

/// Connects: a saved connection for the network if there is one, else a
/// new one (NetworkManager then asks the secret agent for the password).
pub fn connect(net: &Network) -> Result<String, String> {
    let mut bus = Bus::system().map_err(|e| e.to_string())?;
    let settings = "/org/freedesktop/NetworkManager/Settings";
    let conns = call(&mut bus, settings, "org.freedesktop.NetworkManager.Settings", "ListConnections", Vec::new())?;
    let saved = conns.first().map(paths).unwrap_or_default().into_iter().find(|c| {
        call(&mut bus, c, "org.freedesktop.NetworkManager.Settings.Connection", "GetSettings", Vec::new())
            .ok()
            .and_then(|b| b.into_iter().next())
            .and_then(|s| s.get("802-11-wireless").and_then(|w| w.get("ssid")).and_then(Value::bytes))
            .is_some_and(|s| s == net.ssid.as_bytes())
    });
    match saved {
        Some(c) => call(&mut bus, NM_PATH, NM, "ActivateConnection", vec![Value::Path(c), Value::Path(net.device.clone()), Value::Path(net.ap.clone())])?,
        None => {
            let mut groups = vec![Value::DictEntry(
                Box::new(Value::Str("802-11-wireless".into())),
                Box::new(Value::dict(vec![("ssid", Value::byte_array(net.ssid.as_bytes()))])),
            )];
            if net.secured {
                groups.push(Value::DictEntry(
                    Box::new(Value::Str("802-11-wireless-security".into())),
                    Box::new(Value::dict(vec![("key-mgmt", Value::Str("wpa-psk".into()))])),
                ));
            }
            let conn = Value::Array("{sa{sv}}".into(), groups);
            call(&mut bus, NM_PATH, NM, "AddAndActivateConnection", vec![conn, Value::Path(net.device.clone()), Value::Path(net.ap.clone())])?
        }
    };
    Ok(net.ssid.clone())
}
