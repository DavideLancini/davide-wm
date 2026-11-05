//! The desktop notification server (`org.freedesktop.Notifications` on the
//! session bus), replacing nwg-notifications: popups drawn by the
//! compositor, a list opened with Main+N or the bar, and the workspace of
//! the app that notified marked urgent.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use dbus::{Bus, Message, Value};

const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";

/// NotificationClosed reasons.
const EXPIRED: u32 = 1;
const DISMISSED: u32 = 2;
const CLOSED: u32 = 3;

pub struct Notification {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub body: String,
    /// Action keys the app offered.
    pub actions: Vec<String>,
    /// The `desktop-entry` hint: names the app better than `app`.
    pub entry: String,
    pub critical: bool,
    /// Still shown as a popup; once expired it only stays in the list.
    pub live: bool,
    expires: Option<Instant>,
}

impl state::Persist for Notification {
    fn save(&self, e: &mut state::Enc) {
        (self.id, self.app.clone(), self.summary.clone(), self.body.clone()).save(e);
        (self.actions.clone(), self.entry.clone(), self.critical, self.live).save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let (id, app, summary, body) = state::Persist::load(d)?;
        let (actions, entry, critical, live) = state::Persist::load(d)?;
        let mut n = Notification { id, app, summary, body, actions, entry, critical, live, expires: None };
        n.expires = (live && !critical).then(|| Instant::now() + Duration::from_millis(config::NOTIFICATION_TIMEOUT_MS));
        Ok(n)
    }
}

impl Notification {
    /// "App: summary" for the popup title.
    pub fn title(&self) -> String {
        match (self.app.is_empty(), self.summary.is_empty()) {
            (true, _) => self.summary.clone(),
            (false, true) => self.app.clone(),
            _ => format!("{}: {}", self.app, self.summary),
        }
    }

    /// One line for the list.
    pub fn label(&self) -> String {
        let body: String = self.body.split_whitespace().collect::<Vec<_>>().join(" ");
        if body.is_empty() {
            self.title()
        } else {
            format!("{} — {body}", self.title())
        }
    }
}

fn dnd_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    dir.join("davide-wm/dnd")
}

/// Do not disturb: kept in a file so it survives restarts.
pub fn dnd() -> bool {
    dnd_path().exists()
}

pub fn set_dnd(on: bool) {
    let p = dnd_path();
    let _ = match on {
        true => p.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| std::fs::write(&p, "")),
        false => std::fs::remove_file(&p),
    };
}

pub struct Notifier {
    bus: Option<Bus>,
    /// Oldest first.
    pub list: Vec<Notification>,
    next_id: u32,
    timer: OwnedFd,
}

fn timerfd() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn hint<'a>(hints: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Array(_, entries) = hints else { return None };
    entries.iter().find_map(|e| match e {
        Value::DictEntry(k, v) if k.as_str() == Some(key) => Some(&**v),
        _ => None,
    })
}

impl Notifier {
    /// Takes the bus name; without it (another server running, no session
    /// bus) notifications stay off.
    pub fn start() -> io::Result<Notifier> {
        let bus = match Bus::session() {
            Ok(mut bus) => match bus.request_name(NAME, 2 | 4) {
                Ok(true) => Some(bus),
                Ok(false) => {
                    log!("notifications: another server owns {NAME}");
                    None
                }
                Err(e) => {
                    log!("notifications: {e}");
                    None
                }
            },
            Err(e) => {
                log!("notifications: no session bus: {e}");
                None
            }
        };
        Notifier::with(bus, Vec::new(), 1)
    }

    fn with(bus: Option<Bus>, list: Vec<Notification>, next_id: u32) -> io::Result<Notifier> {
        let n = Notifier { bus, list, next_id, timer: timerfd()? };
        n.arm();
        Ok(n)
    }

    pub fn bus_fd(&self) -> Option<RawFd> {
        self.bus.as_ref().map(|b| b.raw_fd())
    }

    pub fn timer_fd(&self) -> RawFd {
        self.timer.as_raw_fd()
    }

    fn arm(&self) {
        let next = self.list.iter().filter_map(|n| n.expires).min();
        let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        if let Some(at) = next {
            let d = at.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
            spec.it_value.tv_sec = d.as_secs() as libc::time_t;
            spec.it_value.tv_nsec = d.subsec_nanos() as libc::c_long;
        }
        unsafe { libc::timerfd_settime(self.timer.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
    }

    fn emit(&mut self, msg: Message) {
        if let Some(bus) = &mut self.bus {
            bus.send(&msg);
            let _ = bus.flush();
        }
    }

    fn closed(&mut self, id: u32, reason: u32) {
        self.emit(Message::signal(PATH, IFACE, "NotificationClosed", vec![Value::U32(id), Value::U32(reason)]));
    }

    /// Handles the calls waiting on the bus. Returns the app names (and
    /// desktop entries) of new notifications, to mark their workspaces.
    pub fn dispatch(&mut self) -> Vec<(String, String)> {
        let Some(bus) = &mut self.bus else { return Vec::new() };
        if let Err(e) = bus.read() {
            log!("notifications: bus lost: {e}");
            self.bus = None;
            return Vec::new();
        }
        let mut msgs = Vec::new();
        while let Some(m) = bus.pop() {
            msgs.push(m);
        }
        let mut new = Vec::new();
        for m in msgs {
            if m.kind != dbus::METHOD_CALL {
                continue;
            }
            let reply = match (m.interface.as_deref(), m.member.as_deref()) {
                (Some(IFACE), Some("Notify")) => match self.notify(&m) {
                    Some((id, app, entry)) => {
                        new.push((app, entry));
                        m.reply(vec![Value::U32(id)])
                    }
                    None => m.error_reply("org.freedesktop.DBus.Error.InvalidArgs", "bad Notify arguments"),
                },
                (Some(IFACE), Some("CloseNotification")) => {
                    if let Some(id) = m.body.first().and_then(Value::as_u32) {
                        if let Some(i) = self.list.iter().position(|n| n.id == id) {
                            self.list.remove(i);
                            self.closed(id, CLOSED);
                        }
                    }
                    m.reply(vec![])
                }
                (Some(IFACE), Some("GetCapabilities")) => {
                    let caps = ["actions", "body", "persistence"].map(|c| Value::Str(c.into())).to_vec();
                    m.reply(vec![Value::Array("s".into(), caps)])
                }
                (Some(IFACE), Some("GetServerInformation")) => {
                    m.reply(["davide-wm", "Davide Lancini", env!("CARGO_PKG_VERSION"), "1.2"].map(|s| Value::Str(s.into())).to_vec())
                }
                (Some("org.freedesktop.DBus.Peer"), Some("Ping")) => m.reply(vec![]),
                _ => m.error_reply("org.freedesktop.DBus.Error.UnknownMethod", "unknown method"),
            };
            if !m.no_reply() {
                self.emit(reply);
            }
        }
        self.arm();
        new
    }

    fn notify(&mut self, m: &Message) -> Option<(u32, String, String)> {
        let [app, replaces, _icon, summary, body, actions, hints, timeout] = m.body.as_slice() else { return None };
        let Value::Array(_, actions) = actions else { return None };
        // Pairs of key and label.
        let actions: Vec<String> = actions.chunks(2).filter_map(|p| p[0].as_str().map(String::from)).collect();
        let critical = matches!(hint(hints, "urgency"), Some(Value::Variant(v)) if **v == Value::Byte(2));
        let entry = hint(hints, "desktop-entry").and_then(Value::as_str).unwrap_or("").to_string();
        let timeout = match timeout {
            Value::I32(t) => *t,
            _ => -1,
        };
        let expires = match timeout {
            _ if critical => None,
            0 => None,
            t if t < 0 => Some(Duration::from_millis(config::NOTIFICATION_TIMEOUT_MS)),
            t => Some(Duration::from_millis(t as u64)),
        }
        .map(|d| Instant::now() + d);
        let replaces = replaces.as_u32().unwrap_or(0);
        let id = if replaces != 0 && self.list.iter().any(|n| n.id == replaces) {
            replaces
        } else {
            self.next_id += 1;
            self.next_id - 1
        };
        let n = Notification {
            id,
            app: app.as_str()?.to_string(),
            summary: summary.as_str()?.to_string(),
            body: body.as_str()?.to_string(),
            actions,
            entry,
            critical,
            // Do not disturb: to the list only; critical ones still show.
            live: critical || !dnd(),
            expires,
        };
        let out = (id, n.app.clone(), n.entry.clone());
        self.list.retain(|x| x.id != id);
        self.list.push(n);
        let excess = self.list.len().saturating_sub(config::NOTIFICATION_HISTORY);
        self.list.drain(..excess);
        Some(out)
    }

    /// A notification from the compositor itself.
    pub fn local(&mut self, app: &str, summary: &str, body: &str, critical: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let expires = (!critical).then(|| Instant::now() + Duration::from_millis(config::NOTIFICATION_TIMEOUT_MS));
        self.list.push(Notification {
            id,
            app: app.into(),
            summary: summary.into(),
            body: body.into(),
            actions: Vec::new(),
            entry: String::new(),
            critical,
            live: true,
            expires,
        });
        let excess = self.list.len().saturating_sub(config::NOTIFICATION_HISTORY);
        self.list.drain(..excess);
        self.arm();
    }

    /// The timer fired: popups past their time leave the screen (they stay
    /// in the list). True when something changed.
    pub fn expire(&mut self) -> bool {
        let mut n = 0u64;
        unsafe { libc::read(self.timer.as_raw_fd(), (&mut n as *mut u64).cast(), 8) };
        let now = Instant::now();
        let gone: Vec<u32> = self.list.iter().filter(|n| n.expires.is_some_and(|t| t <= now)).map(|n| n.id).collect();
        for id in &gone {
            if let Some(n) = self.list.iter_mut().find(|n| n.id == *id) {
                n.live = false;
                n.expires = None;
            }
            self.closed(*id, EXPIRED);
        }
        self.arm();
        !gone.is_empty()
    }

    /// The popups to show, newest first.
    pub fn popups(&self) -> Vec<&Notification> {
        self.list.iter().rev().filter(|n| n.live).take(config::NOTIFICATION_POPUPS).collect()
    }

    /// Clicked: runs the default action while the app still listens, and
    /// removes it. Returns the app and desktop entry, to focus its window.
    pub fn invoke(&mut self, id: u32) -> Option<(String, String)> {
        let i = self.list.iter().position(|n| n.id == id)?;
        let n = self.list.remove(i);
        if n.live {
            if n.actions.iter().any(|a| a == "default") {
                self.emit(Message::signal(PATH, IFACE, "ActionInvoked", vec![Value::U32(id), Value::Str("default".into())]));
            }
            self.closed(id, DISMISSED);
        }
        self.arm();
        Some((n.app, n.entry))
    }

    pub fn dismiss(&mut self, id: u32) {
        if let Some(i) = self.list.iter().position(|n| n.id == id) {
            if self.list.remove(i).live {
                self.closed(id, DISMISSED);
            }
        }
        self.arm();
    }

    pub fn clear(&mut self) {
        for id in self.list.iter().map(|n| n.id).collect::<Vec<_>>() {
            self.dismiss(id);
        }
    }
}

impl state::Persist for Notifier {
    fn save(&self, e: &mut state::Enc) {
        self.bus.save(e);
        self.list.save(e);
        self.next_id.save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        let (bus, list, next_id) = (Persist::load(d)?, Persist::load(d)?, Persist::load(d)?);
        Notifier::with(bus, list, next_id).map_err(|e| state::Error(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(body: Vec<Value>) -> Message {
        let mut m = Message::call(NAME, PATH, IFACE, "Notify", body);
        m.serial = 7;
        m
    }

    fn notify_args(app: &str, replaces: u32, timeout: i32, critical: bool) -> Vec<Value> {
        let urgency = Value::DictEntry(Box::new(Value::Str("urgency".into())), Box::new(Value::Variant(Box::new(Value::Byte(if critical { 2 } else { 1 })))));
        vec![
            Value::Str(app.into()),
            Value::U32(replaces),
            Value::Str(String::new()),
            Value::Str("Summary".into()),
            Value::Str("Body".into()),
            Value::Array("s".into(), vec![Value::Str("default".into()), Value::Str("Open".into())]),
            Value::Array("{sv}".into(), vec![urgency]),
            Value::I32(timeout),
        ]
    }

    #[test]
    fn notify_replace_expire_and_invoke() {
        let mut n = Notifier::with(None, Vec::new(), 1).unwrap();
        let (id, app, _) = n.notify(&call(notify_args("Signal", 0, 1, false))).unwrap();
        assert_eq!((id, app.as_str()), (1, "Signal"));
        assert_eq!(n.notify(&call(notify_args("Signal", id, 0, false))).unwrap().0, id, "replaces in place");
        assert_eq!(n.list.len(), 1);
        n.notify(&call(notify_args("Mail", 0, 1, false)));
        n.notify(&call(notify_args("Alarm", 0, 1, true)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(n.expire());
        let shown: Vec<&str> = n.popups().iter().map(|p| p.app.as_str()).collect();
        assert_eq!(shown, ["Alarm", "Signal"], "critical and timeout 0 stay");
        assert_eq!(n.invoke(id).map(|x| x.0).as_deref(), Some("Signal"));
        assert_eq!(n.list.len(), 2);
        assert_eq!(n.list[0].label(), "Mail: Summary — Body");
        n.clear();
        assert!(n.list.is_empty());
    }
}
