//! The xdg-desktop-portal backend (`org.freedesktop.impl.portal.desktop.davidewm`):
//! ScreenCast, Screenshot, Settings, Inhibit and Notification.
//! xdg-desktop-portal forwards the apps' requests here; a cast source is
//! chosen in the launcher box and streamed by `cast.rs`, settings are fixed
//! (dark theme), inhibitors keep the screen on, notifications go to the
//! compositor's own list. The file chooser is davide-apps'.

use std::collections::HashMap;
use std::os::fd::RawFd;

use dbus::{Bus, Message, Value};

const NAME: &str = "org.freedesktop.impl.portal.desktop.davidewm";
const SCREENCAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const SCREENSHOT: &str = "org.freedesktop.impl.portal.Screenshot";
const SETTINGS: &str = "org.freedesktop.impl.portal.Settings";
const INHIBIT: &str = "org.freedesktop.impl.portal.Inhibit";
const NOTIFICATION: &str = "org.freedesktop.impl.portal.Notification";
const SESSION: &str = "org.freedesktop.impl.portal.Session";
const REQUEST: &str = "org.freedesktop.impl.portal.Request";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Source types: monitor and window.
pub const MONITOR: u32 = 1;
pub const WINDOW: u32 = 2;
/// Cursor modes: embedded only.
const EMBEDDED: u32 = 2;

/// Portal responses.
const SUCCESS: u32 = 0;
const CANCELLED: u32 = 1;
const FAILED: u32 = 2;

struct Session {
    types: u32,
    cast: Option<u32>,
    app_id: String,
    /// The app asked to remember the choice (persist_mode 1 or 2).
    persist: u32,
    /// A choice to reuse: from the app's restore token.
    restore: Option<String>,
}

/// How long a choice is reused for the same app without asking again:
/// Chromium opens a second session right after its own picker.
const REUSE_SECS: u64 = 20;
/// Vendor tag of our restore data.
const VENDOR: &str = "davide-wm";

enum Pending {
    /// ScreenCast.Start: waits for a pick, then for the stream's node.
    Start { call: Message, handle: String, session: String, cast: Option<u32>, source_type: u32, choice: String },
    Shot { call: Message, handle: String },
}

/// What the compositor has to do for the portal.
pub enum Action {
    /// Ask the user, unless `reuse` (a choice as `CastChoice` encodes it)
    /// still makes sense.
    PickSource { request: u32, types: u32, reuse: Option<String> },
    CancelPick { request: u32 },
    StopCast(u32),
    Screenshot { request: u32, interactive: bool },
    /// A notification from a sandboxed app.
    Notify { app: String, summary: String, body: String, urgent: bool },
}

/// Inhibit flag: keep the session from going idle.
const INHIBIT_IDLE: u32 = 8;

/// What Settings reports: a dark session, the fonts and themes in use.
fn settings() -> Vec<(&'static str, Vec<(&'static str, Value)>)> {
    let s = |v: &str| Value::Str(v.into());
    vec![
        ("org.freedesktop.appearance", vec![("color-scheme", Value::U32(1)), ("contrast", Value::U32(0))]),
        (
            "org.gnome.desktop.interface",
            vec![
                ("color-scheme", s("prefer-dark")),
                ("gtk-theme", s("Adwaita")),
                ("icon-theme", s("breeze-dark")),
                ("cursor-theme", s("default")),
                ("cursor-size", Value::I32(24)),
                ("font-name", s("Noto Sans 10")),
                ("document-font-name", s("Adwaita Sans 12")),
                ("monospace-font-name", s("Adwaita Mono 11")),
                ("text-scaling-factor", Value::Double(1.0)),
                ("enable-animations", Value::Bool(false)),
            ],
        ),
    ]
}

/// Namespace patterns of `ReadAll`: exact, or a prefix ending in `*`;
/// none (or an empty one) means all.
fn wanted(ns: &str, patterns: &[String]) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| p.is_empty() || p == ns || p.strip_suffix('*').is_some_and(|pre| ns.starts_with(pre)))
}

pub struct Portal {
    /// The last choice: app, choice, when.
    last: Option<(String, String, std::time::Instant)>,
    bus: Option<Bus>,
    sessions: HashMap<String, Session>,
    pending: HashMap<u32, Pending>,
    next: u32,
    /// Inhibit requests in force: handle and flags.
    inhibits: HashMap<String, u32>,
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Array("{sv}".into(), entries.into_iter().map(|(k, v)| Value::DictEntry(Box::new(Value::Str(k.into())), Box::new(Value::Variant(Box::new(v))))).collect())
}

fn option<'a>(opts: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    let Some(Value::Array(_, entries)) = opts else { return None };
    entries.iter().find_map(|e| match e {
        Value::DictEntry(k, v) if k.as_str() == Some(key) => match &**v {
            Value::Variant(inner) => Some(&**inner),
            v => Some(v),
        },
        _ => None,
    })
}

fn path(v: Option<&Value>) -> String {
    match v {
        Some(Value::Path(p) | Value::Str(p)) => p.clone(),
        _ => String::new(),
    }
}

const INTROSPECTION: &str = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
 <interface name="org.freedesktop.impl.portal.ScreenCast">
  <method name="CreateSession"><arg type="o" direction="in"/><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="SelectSources"><arg type="o" direction="in"/><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="Start"><arg type="o" direction="in"/><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <property name="AvailableSourceTypes" type="u" access="read"/>
  <property name="AvailableCursorModes" type="u" access="read"/>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.impl.portal.Screenshot">
  <method name="Screenshot"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="PickColor"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.impl.portal.Settings">
  <method name="ReadAll"><arg type="as" direction="in"/><arg type="a{sa{sv}}" direction="out"/></method>
  <method name="Read"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
  <method name="ReadOne"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method>
  <signal name="SettingChanged"><arg type="s"/><arg type="s"/><arg type="v"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.impl.portal.Inhibit">
  <method name="Inhibit"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <method name="CreateMonitor"><arg type="o" direction="in"/><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="u" direction="out"/></method>
  <method name="QueryEndResponse"><arg type="o" direction="in"/></method>
  <signal name="StateChanged"><arg type="o"/><arg type="a{sv}"/></signal>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.impl.portal.Notification">
  <method name="AddNotification"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/></method>
  <method name="RemoveNotification"><arg type="s" direction="in"/><arg type="s" direction="in"/></method>
  <signal name="ActionInvoked"><arg type="s"/><arg type="s"/><arg type="s"/><arg type="av"/></signal>
  <property name="SupportedOptions" type="a{sv}" access="read"/>
  <property name="version" type="u" access="read"/>
 </interface>
</node>"#;

impl Portal {
    /// Takes the backend name; without a session bus or the name, the
    /// portal stays off.
    pub fn start() -> Portal {
        let bus = match Bus::session() {
            Ok(mut bus) => match bus.request_name(NAME, 4) {
                Ok(true) => Some(bus),
                Ok(false) => {
                    log!("portal: {NAME} is taken");
                    None
                }
                Err(e) => {
                    log!("portal: {e}");
                    None
                }
            },
            Err(e) => {
                log!("portal: no session bus: {e}");
                None
            }
        };
        Portal { last: None, bus, sessions: HashMap::new(), pending: HashMap::new(), next: 0, inhibits: HashMap::new() }
    }

    pub fn bus_fd(&self) -> Option<RawFd> {
        self.bus.as_ref().map(|b| b.raw_fd())
    }

    fn send(&mut self, m: Message) {
        if let Some(bus) = &mut self.bus {
            bus.send(&m);
            let _ = bus.flush();
        }
    }

    fn properties(iface: &str) -> Vec<(&'static str, Value)> {
        match iface {
            SCREENCAST => vec![("AvailableSourceTypes", Value::U32(MONITOR | WINDOW)), ("AvailableCursorModes", Value::U32(EMBEDDED)), ("version", Value::U32(5))],
            SCREENSHOT => vec![("version", Value::U32(2))],
            SETTINGS => vec![("version", Value::U32(2))],
            INHIBIT => vec![("version", Value::U32(3))],
            NOTIFICATION => vec![("version", Value::U32(2)), ("SupportedOptions", dict(Vec::new()))],
            _ => Vec::new(),
        }
    }

    pub fn dispatch(&mut self) -> Vec<Action> {
        let Some(bus) = &mut self.bus else { return Vec::new() };
        if let Err(e) = bus.read() {
            log!("portal: {e}");
            self.bus = None;
            return Vec::new();
        }
        let mut calls = Vec::new();
        while let Some(m) = self.bus.as_mut().unwrap().pop() {
            calls.push(m);
        }
        let mut actions = Vec::new();
        for m in calls {
            self.call(m, &mut actions);
        }
        actions
    }

    fn call(&mut self, m: Message, actions: &mut Vec<Action>) {
        let empty = || dict(Vec::new());
        let b = &m.body;
        if m.is_call(PROPERTIES, "Get") {
            let (iface, prop) = (b.first().and_then(Value::as_str).unwrap_or(""), b.get(1).and_then(Value::as_str).unwrap_or(""));
            let reply = match Portal::properties(iface).into_iter().find(|(k, _)| *k == prop) {
                Some((_, v)) => m.reply(vec![Value::Variant(Box::new(v))]),
                None => m.error_reply("org.freedesktop.DBus.Error.UnknownProperty", prop),
            };
            self.send(reply);
        } else if m.is_call(PROPERTIES, "GetAll") {
            let props = Portal::properties(b.first().and_then(Value::as_str).unwrap_or(""));
            self.send(m.reply(vec![dict(props)]));
        } else if m.is_call("org.freedesktop.DBus.Introspectable", "Introspect") {
            self.send(m.reply(vec![Value::Str(INTROSPECTION.into())]));
        } else if m.is_call(SCREENCAST, "CreateSession") {
            let app_id = b.get(2).and_then(Value::as_str).unwrap_or("").to_string();
            self.sessions.insert(path(b.get(1)), Session { types: MONITOR, cast: None, app_id, persist: 0, restore: None });
            self.send(m.reply(vec![Value::U32(SUCCESS), empty()]));
        } else if m.is_call(SCREENCAST, "SelectSources") {
            let types = option(b.get(3), "types").and_then(Value::as_u32).filter(|t| t & (MONITOR | WINDOW) != 0).unwrap_or(MONITOR);
            let persist = option(b.get(3), "persist_mode").and_then(Value::as_u32).unwrap_or(0);
            // (vendor, version, data): only ours is understood.
            let restore = match option(b.get(3), "restore_data") {
                Some(Value::Struct(f)) if f.first().and_then(Value::as_str) == Some(VENDOR) => f.get(2).and_then(|v| v.as_str()).map(str::to_string),
                _ => None,
            };
            if let Some(s) = self.sessions.get_mut(&path(b.get(1))) {
                (s.types, s.persist, s.restore) = (types, persist, restore);
            }
            self.send(m.reply(vec![Value::U32(SUCCESS), empty()]));
        } else if m.is_call(SCREENCAST, "Start") {
            let session = path(b.get(1));
            let Some((types, app_id, restore)) = self.sessions.get(&session).map(|s| (s.types, s.app_id.clone(), s.restore.clone())) else {
                self.send(m.reply(vec![Value::U32(FAILED), empty()]));
                return;
            };
            let recent = self.last.as_ref().filter(|(app, _, at)| *app == app_id && at.elapsed().as_secs() < REUSE_SECS).map(|(_, c, _)| c.clone());
            self.next += 1;
            let request = self.next;
            let handle = path(b.first());
            self.pending.insert(request, Pending::Start { call: m, handle, session, cast: None, source_type: MONITOR, choice: String::new() });
            actions.push(Action::PickSource { request, types, reuse: restore.or(recent) });
        } else if m.is_call(SCREENSHOT, "Screenshot") {
            let interactive = option(b.get(3), "interactive").and_then(Value::as_bool).unwrap_or(false);
            self.next += 1;
            let request = self.next;
            let handle = path(b.first());
            self.pending.insert(request, Pending::Shot { call: m, handle });
            actions.push(Action::Screenshot { request, interactive });
        } else if m.is_call(SCREENSHOT, "PickColor") {
            self.send(m.reply(vec![Value::U32(FAILED), empty()]));
        } else if m.is_call(SESSION, "Close") {
            let p = m.path.clone().unwrap_or_default();
            if let Some(cast) = self.sessions.remove(&p).and_then(|s| s.cast) {
                actions.push(Action::StopCast(cast));
            }
            self.send(m.reply(Vec::new()));
        } else if m.is_call(SETTINGS, "ReadAll") {
            let patterns: Vec<String> = match b.first() {
                Some(Value::Array(_, v)) => v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            let all = settings()
                .into_iter()
                .filter(|(ns, _)| wanted(ns, &patterns))
                .map(|(ns, keys)| Value::DictEntry(Box::new(Value::Str(ns.into())), Box::new(dict(keys))))
                .collect();
            self.send(m.reply(vec![Value::Array("{sa{sv}}".into(), all)]));
        } else if m.is_call(SETTINGS, "Read") || m.is_call(SETTINGS, "ReadOne") {
            let (ns, key) = (b.first().and_then(Value::as_str).unwrap_or(""), b.get(1).and_then(Value::as_str).unwrap_or(""));
            let found = settings().into_iter().find(|(n, _)| *n == ns).and_then(|(_, keys)| keys.into_iter().find(|(k, _)| *k == key)).map(|(_, v)| v);
            let reply = match found {
                Some(v) => m.reply(vec![Value::Variant(Box::new(v))]),
                None => m.error_reply("org.freedesktop.portal.Error.NotFound", "no such setting"),
            };
            self.send(reply);
        } else if m.is_call(INHIBIT, "Inhibit") {
            let flags = b.get(3).and_then(Value::as_u32).unwrap_or(0);
            self.inhibits.insert(path(b.first()), flags);
            self.send(m.reply(Vec::new()));
        } else if m.is_call(INHIBIT, "CreateMonitor") {
            self.send(m.reply(vec![Value::U32(SUCCESS)]));
        } else if m.is_call(INHIBIT, "QueryEndResponse") {
            self.send(m.reply(Vec::new()));
        } else if m.is_call(NOTIFICATION, "AddNotification") {
            let app = b.first().and_then(Value::as_str).unwrap_or("").to_string();
            let n = b.get(2);
            let text = |k: &str| option(n, k).and_then(Value::as_str).unwrap_or("").to_string();
            let urgent = matches!(text("priority").as_str(), "urgent");
            actions.push(Action::Notify { app, summary: text("title"), body: text("body"), urgent });
            self.send(m.reply(Vec::new()));
        } else if m.is_call(NOTIFICATION, "RemoveNotification") {
            self.send(m.reply(Vec::new()));
        } else if m.is_call(REQUEST, "Close") {
            let p = m.path.clone().unwrap_or_default();
            self.inhibits.remove(&p);
            let found = self.pending.iter().find(|(_, v)| matches!(v, Pending::Start { handle, .. } | Pending::Shot { handle, .. } if *handle == p)).map(|(k, _)| *k);
            if let Some(request) = found {
                actions.push(Action::CancelPick { request });
                self.finish(request, CANCELLED, Vec::new());
            }
            self.send(m.reply(Vec::new()));
        } else if m.kind == 1 && !m.no_reply() {
            self.send(m.error_reply("org.freedesktop.DBus.Error.UnknownMethod", "unknown method"));
        }
    }

    /// An app asked the session not to go idle.
    pub fn idle_inhibited(&self) -> bool {
        self.inhibits.values().any(|f| f & INHIBIT_IDLE != 0)
    }

    /// Answers a pending request.
    fn finish(&mut self, request: u32, response: u32, results: Vec<(&str, Value)>) {
        let Some(p) = self.pending.remove(&request) else { return };
        let call = match p {
            Pending::Start { call, .. } | Pending::Shot { call, .. } => call,
        };
        self.send(call.reply(vec![Value::U32(response), dict(results)]));
    }

    /// The user chose a source and its cast started; the answer waits for
    /// the stream's node.
    pub fn picked(&mut self, request: u32, cast_id: u32, window: bool, encoded: String) {
        if let Some(Pending::Start { cast, source_type, session, choice, .. }) = self.pending.get_mut(&request) {
            *cast = Some(cast_id);
            *source_type = if window { WINDOW } else { MONITOR };
            *choice = encoded;
            if let Some(s) = self.sessions.get_mut(session) {
                s.cast = Some(cast_id);
            }
        }
    }

    /// The picker closed without a choice, or the cast could not start.
    pub fn cancelled(&mut self, request: u32, failed: bool) {
        self.finish(request, if failed { FAILED } else { CANCELLED }, Vec::new());
    }

    pub fn cast_ready(&mut self, cast_id: u32, node: u32, size: (i32, i32)) {
        let found = self.pending.iter().find(|(_, p)| matches!(p, Pending::Start { cast: Some(c), .. } if *c == cast_id)).map(|(k, _)| *k);
        let Some(request) = found else { return };
        let Some(Pending::Start { source_type, choice, session, .. }) = self.pending.get(&request) else { return };
        let (choice, session) = (choice.clone(), session.clone());
        let props = dict(vec![
            ("position", Value::Struct(vec![Value::I32(0), Value::I32(0)])),
            ("size", Value::Struct(vec![Value::I32(size.0), Value::I32(size.1)])),
            ("source_type", Value::U32(*source_type)),
        ]);
        let streams = Value::Array("(ua{sv})".into(), vec![Value::Struct(vec![Value::U32(node), props])]);
        let mut results = vec![("streams", streams)];
        if let Some(s) = self.sessions.get(&session) {
            self.last = Some((s.app_id.clone(), choice.clone(), std::time::Instant::now()));
            if s.persist > 0 {
                let data = Value::Struct(vec![Value::Str(VENDOR.into()), Value::U32(1), Value::Variant(Box::new(Value::Str(choice)))]);
                results.push(("persist_mode", Value::U32(s.persist)));
                results.push(("restore_data", data));
            }
        }
        let Some(p) = self.pending.remove(&request) else { return };
        if let Pending::Start { call, .. } = p {
            self.send(call.reply(vec![Value::U32(SUCCESS), dict(results)]));
        }
    }

    /// The cast stopped on our side (window closed, stream error).
    pub fn cast_ended(&mut self, cast_id: u32) {
        let pending = self.pending.iter().find(|(_, p)| matches!(p, Pending::Start { cast: Some(c), .. } if *c == cast_id)).map(|(k, _)| *k);
        if let Some(request) = pending {
            self.finish(request, FAILED, Vec::new());
        }
        let sessions: Vec<String> = self.sessions.iter().filter(|(_, s)| s.cast == Some(cast_id)).map(|(k, _)| k.clone()).collect();
        for p in sessions {
            self.sessions.remove(&p);
            self.send(Message::signal(&p, SESSION, "Closed", Vec::new()));
        }
    }

    pub fn screenshot_done(&mut self, request: u32, file: Option<&str>) {
        match file {
            Some(f) => self.finish(request, SUCCESS, vec![("uri", Value::Str(format!("file://{f}")))]),
            None => self.finish(request, FAILED, Vec::new()),
        }
    }

    /// Before a live upgrade: streams do not survive it.
    pub fn close_all(&mut self) {
        for request in self.pending.keys().copied().collect::<Vec<_>>() {
            self.finish(request, FAILED, Vec::new());
        }
        for p in self.sessions.drain().map(|(k, _)| k).collect::<Vec<_>>() {
            self.send(Message::signal(&p, SESSION, "Closed", Vec::new()));
        }
    }
}
