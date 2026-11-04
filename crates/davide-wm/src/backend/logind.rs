//! Session and device access through systemd-logind (no libseat).
//!
//! Calls are synchronous with a short timeout: libinput asks for device fds
//! from inside its own calls and needs the answer right away.

use std::ffi::CString;
use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

use dbus::{Bus, Message, Value};

const DEST: &str = "org.freedesktop.login1";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";
const TIMEOUT: Duration = Duration::from_millis(500);

pub enum Event {
    /// Device paused; DRM pauses of type "pause" must be acknowledged.
    Pause { major: u32, minor: u32, kind: String },
    /// The fd logind passes along is the one we already hold; it is dropped.
    Resume { major: u32, minor: u32 },
}

pub struct Logind {
    pub bus: Bus,
    session: String,
    seat: String,
}

state::persist_struct!(Logind { bus, session, seat });

fn err(e: dbus::Error) -> io::Error {
    io::Error::other(e.to_string())
}

pub fn device_numbers(path: &str) -> io::Result<(u32, u32)> {
    let c = CString::new(path).map_err(io::Error::other)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((libc::major(st.st_rdev), libc::minor(st.st_rdev)))
}

impl Logind {
    pub fn open() -> io::Result<Logind> {
        let mut bus = Bus::system().map_err(err)?;
        let reply = bus
            .call(
                Message::call(DEST, "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "GetSessionByPID", vec![Value::U32(std::process::id())]),
                TIMEOUT,
            )
            .map_err(err)?;
        let session = reply.body.first().and_then(Value::as_str).ok_or_else(|| io::Error::other("no session path"))?.to_string();
        let mut l = Logind { bus, session, seat: "/org/freedesktop/login1/seat/seat0".into() };
        if let Some(Value::Struct(s)) = l.property("Seat")?.first().map(|v| match v {
            Value::Variant(inner) => (**inner).clone(),
            other => other.clone(),
        }) {
            if let Some(p) = s.get(1).and_then(Value::as_str) {
                l.seat = p.to_string();
            }
        }
        let rule = format!("type='signal',sender='{DEST}',path='{}'", l.session);
        l.bus.add_match(&rule).map_err(err)?;
        l.session_call("TakeControl", vec![Value::Bool(false)])?;
        Ok(l)
    }

    fn session_call(&mut self, member: &str, args: Vec<Value>) -> io::Result<Message> {
        let m = Message::call(DEST, &self.session, SESSION_IFACE, member, args);
        self.bus.call(m, TIMEOUT).map_err(err)
    }

    fn property(&mut self, name: &str) -> io::Result<Vec<Value>> {
        let m = Message::call(
            DEST,
            &self.session,
            "org.freedesktop.DBus.Properties",
            "Get",
            vec![Value::Str(SESSION_IFACE.into()), Value::Str(name.into())],
        );
        Ok(self.bus.call(m, TIMEOUT).map_err(err)?.body)
    }

    pub fn take_device(&mut self, path: &str) -> io::Result<OwnedFd> {
        let (major, minor) = device_numbers(path)?;
        let mut reply = self.session_call("TakeDevice", vec![Value::U32(major), Value::U32(minor)])?;
        reply.take_fd(0).ok_or_else(|| io::Error::other("TakeDevice returned no fd"))
    }

    pub fn release_device(&mut self, major: u32, minor: u32) {
        let _ = self.session_call("ReleaseDevice", vec![Value::U32(major), Value::U32(minor)]);
    }

    pub fn pause_complete(&mut self, major: u32, minor: u32) {
        let m = Message::call(DEST, &self.session, SESSION_IFACE, "PauseDeviceComplete", vec![Value::U32(major), Value::U32(minor)]);
        self.bus.send(&m);
        let _ = self.bus.flush();
    }

    pub fn switch_vt(&mut self, vt: u32) {
        let m = Message::call(DEST, &self.seat.clone(), "org.freedesktop.login1.Seat", "SwitchTo", vec![Value::U32(vt)]);
        self.bus.send(&m);
        let _ = self.bus.flush();
    }

    /// Session signals received since the last call.
    pub fn events(&mut self) -> io::Result<Vec<Event>> {
        self.bus.read().map_err(err)?;
        let mut out = Vec::new();
        while let Some(mut m) = self.bus.pop() {
            if m.is_signal(SESSION_IFACE, "PauseDevice") {
                let (Some(major), Some(minor)) = (m.body.first().and_then(Value::as_u32), m.body.get(1).and_then(Value::as_u32)) else { continue };
                let kind = m.body.get(2).and_then(Value::as_str).unwrap_or("").to_string();
                out.push(Event::Pause { major, minor, kind });
            } else if m.is_signal(SESSION_IFACE, "ResumeDevice") {
                let (Some(major), Some(minor)) = (m.body.first().and_then(Value::as_u32), m.body.get(1).and_then(Value::as_u32)) else { continue };
                drop(m.take_fd(2));
                out.push(Event::Resume { major, minor });
            }
        }
        Ok(out)
    }
}
