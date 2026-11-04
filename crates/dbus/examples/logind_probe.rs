//! Reads a few logind properties, to check the client against a real bus.
use std::time::Duration;
use dbus::{Bus, Message, Value};
fn main() {
    let mut bus = Bus::system().unwrap();
    let pid = std::process::id();
    let r = bus.call(Message::call("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "GetSessionByPID", vec![Value::U32(pid)]), Duration::from_secs(1));
    println!("session by pid: {r:?}");
    let get = |bus: &mut Bus, path: &str, prop: &str| bus.call(Message::call("org.freedesktop.login1", path, "org.freedesktop.DBus.Properties", "Get", vec![Value::Str("org.freedesktop.login1.Session".into()), Value::Str(prop.into())]), Duration::from_secs(1)).map(|m| m.body);
    let path = "/org/freedesktop/login1/session/auto";
    for p in ["Id", "Active", "VTNr", "Seat", "Type"] {
        println!("{p}: {:?}", get(&mut bus, path, p));
    }
}
