//! Removable volumes through UDisks2 on the system bus (blocking calls).

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

use dbus::{Bus, Message, Value};

const DEST: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FS: &str = "org.freedesktop.UDisks2.Filesystem";
const DRIVE: &str = "org.freedesktop.UDisks2.Drive";
const TIMEOUT: Duration = Duration::from_secs(5);
const SLOW: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    /// Block device object path.
    pub object: String,
    pub label: String,
    pub device: String,
    pub size: u64,
    pub mount: Option<PathBuf>,
    /// Drive object path.
    pub drive: String,
    pub removable: bool,
    pub can_power_off: bool,
}

fn err(e: dbus::Error) -> String {
    e.to_string()
}

fn flag(props: &Value, key: &str) -> bool {
    props.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Bytes of an `ay` without the trailing NULs.
fn bytes_z(v: &Value) -> Option<Vec<u8>> {
    let mut b = v.bytes()?;
    while b.last() == Some(&0) {
        b.pop();
    }
    Some(b)
}

struct Drive {
    removable: bool,
    can_power_off: bool,
}

/// Every filesystem block and whether it should be shown.
fn filesystems(bus: &mut Bus) -> Result<Vec<(Volume, bool)>, String> {
    let call = Message::call(DEST, "/org/freedesktop/UDisks2", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", Vec::new());
    let reply = bus.call(call, TIMEOUT).map_err(err)?;
    let Some(Value::Array(_, objects)) = reply.body.first() else { return Err("nessun oggetto UDisks2".into()) };
    let entries: Vec<(&str, &Value)> = objects
        .iter()
        .filter_map(|o| match o {
            Value::DictEntry(path, ifaces) => Some((path.as_str()?, &**ifaces)),
            _ => None,
        })
        .collect();
    let drives: HashMap<&str, Drive> = entries
        .iter()
        .filter_map(|(path, ifaces)| {
            let d = ifaces.get(DRIVE)?;
            let usb = d.get("ConnectionBus").and_then(Value::as_str) == Some("usb");
            let removable = flag(d, "Removable") || flag(d, "MediaRemovable") || flag(d, "Ejectable") || usb;
            Some((*path, Drive { removable, can_power_off: flag(d, "CanPowerOff") }))
        })
        .collect();
    let mut out = Vec::new();
    for (path, ifaces) in entries {
        let (Some(b), Some(f)) = (ifaces.get(BLOCK), ifaces.get(FS)) else { continue };
        let drive = b.get("Drive").and_then(Value::as_str).unwrap_or("/").to_string();
        let d = drives.get(drive.as_str());
        let removable = d.is_some_and(|d| d.removable);
        let size = match b.get("Size") {
            Some(Value::U64(n)) => *n,
            _ => 0,
        };
        let device = b.get("PreferredDevice").or(b.get("Device")).and_then(bytes_z).map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default();
        let mount = match f.get("MountPoints") {
            Some(Value::Array(_, mps)) => mps.iter().filter_map(bytes_z).find(|m| !m.is_empty()).map(|m| PathBuf::from(OsString::from_vec(m))),
            _ => None,
        };
        let label = b.get("IdLabel").and_then(Value::as_str).filter(|l| !l.is_empty()).map_or_else(|| format!("{} Volume", crate::dir::human_size(size)), String::from);
        let visible = !flag(b, "HintIgnore") && (!flag(b, "HintSystem") || removable);
        let can_power_off = d.is_some_and(|d| d.can_power_off);
        out.push((Volume { object: path.into(), label, device, size, mount, drive, removable, can_power_off }, visible));
    }
    out.sort_by(|a, b| a.0.device.cmp(&b.0.device));
    Ok(out)
}

/// Filesystems worth showing: not ignored, and not system ones unless on a
/// removable drive. Sorted by device.
pub fn volumes() -> Result<Vec<Volume>, String> {
    let mut bus = Bus::system().map_err(err)?;
    Ok(filesystems(&mut bus)?.into_iter().filter(|v| v.1).map(|v| v.0).collect())
}

fn no_options() -> Value {
    Value::Array("{sv}".into(), Vec::new())
}

pub fn mount(object: &str) -> Result<PathBuf, String> {
    let mut bus = Bus::system().map_err(err)?;
    let opts = Value::dict(vec![("auth.no_user_interaction", Value::Bool(false))]);
    let reply = bus.call(Message::call(DEST, object, FS, "Mount", vec![opts]), SLOW).map_err(err)?;
    reply.body.first().and_then(Value::as_str).map(PathBuf::from).ok_or_else(|| "risposta di Mount non valida".into())
}

fn unmount_on(bus: &mut Bus, object: &str) -> Result<(), String> {
    bus.call(Message::call(DEST, object, FS, "Unmount", vec![no_options()]), SLOW).map(|_| ()).map_err(err)
}

pub fn unmount(object: &str) -> Result<(), String> {
    unmount_on(&mut Bus::system().map_err(err)?, object)
}

/// Unmounts every mounted filesystem of `drive`, then powers it off.
pub fn power_off(drive: &str) -> Result<(), String> {
    let mut bus = Bus::system().map_err(err)?;
    for (v, _) in filesystems(&mut bus)? {
        if v.drive == drive && v.mount.is_some() {
            unmount_on(&mut bus, &v.object)?;
        }
    }
    bus.call(Message::call(DEST, drive, DRIVE, "PowerOff", vec![no_options()]), SLOW).map(|_| ()).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_terminated() {
        assert_eq!(bytes_z(&Value::byte_array(b"/dev/sda1\0")), Some(b"/dev/sda1".to_vec()));
        assert_eq!(bytes_z(&Value::Str("x".into())), None);
    }

    /// Read-only: lists the volumes of this machine.
    #[test]
    #[ignore]
    fn list_volumes() {
        let mut bus = Bus::system().unwrap();
        for (v, visible) in filesystems(&mut bus).unwrap() {
            eprintln!("{visible} {v:?}");
        }
    }
}
