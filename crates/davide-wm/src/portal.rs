//! The xdg-desktop-portal backend (`org.freedesktop.impl.portal.desktop.davidewm`):
//! ScreenCast and Screenshot. xdg-desktop-portal forwards the apps' requests
//! here; the source is chosen in the launcher box and streamed by `cast.rs`.

use std::collections::HashMap;
use std::os::fd::RawFd;

use dbus::{Bus, Message, Value};

const NAME: &str = "org.freedesktop.impl.portal.desktop.davidewm";
const SCREENCAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const SCREENSHOT: &str = "org.freedesktop.impl.portal.Screenshot";
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
}

enum Pending {
    /// ScreenCast.Start: waits for a pick, then for the stream's node.
    Start { call: Message, handle: String, session: String, cast: Option<u32>, source_type: u32 },
    Shot { call: Message, handle: String },
}

/// What the compositor has to do for the portal.
pub enum Action {
    PickSource { request: u32, types: u32 },
    CancelPick { request: u32 },
    StopCast(u32),
    Screenshot { request: u32, interactive: bool },
}

pub struct Portal {
    bus: Option<Bus>,
    sessions: HashMap<String, Session>,
    pending: HashMap<u32, Pending>,
    next: u32,
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
        Portal { bus, sessions: HashMap::new(), pending: HashMap::new(), next: 0 }
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
            self.sessions.insert(path(b.get(1)), Session { types: MONITOR, cast: None });
            self.send(m.reply(vec![Value::U32(SUCCESS), empty()]));
        } else if m.is_call(SCREENCAST, "SelectSources") {
            let types = option(b.get(3), "types").and_then(Value::as_u32).filter(|t| t & (MONITOR | WINDOW) != 0).unwrap_or(MONITOR);
            if let Some(s) = self.sessions.get_mut(&path(b.get(1))) {
                s.types = types;
            }
            self.send(m.reply(vec![Value::U32(SUCCESS), empty()]));
        } else if m.is_call(SCREENCAST, "Start") {
            let session = path(b.get(1));
            let Some(types) = self.sessions.get(&session).map(|s| s.types) else {
                self.send(m.reply(vec![Value::U32(FAILED), empty()]));
                return;
            };
            self.next += 1;
            let request = self.next;
            let handle = path(b.first());
            self.pending.insert(request, Pending::Start { call: m, handle, session, cast: None, source_type: MONITOR });
            actions.push(Action::PickSource { request, types });
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
        } else if m.is_call(REQUEST, "Close") {
            let p = m.path.clone().unwrap_or_default();
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
    pub fn picked(&mut self, request: u32, cast_id: u32, window: bool) {
        if let Some(Pending::Start { cast, source_type, session, .. }) = self.pending.get_mut(&request) {
            *cast = Some(cast_id);
            *source_type = if window { WINDOW } else { MONITOR };
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
        let Some(Pending::Start { source_type, .. }) = self.pending.get(&request) else { return };
        let props = dict(vec![
            ("position", Value::Struct(vec![Value::I32(0), Value::I32(0)])),
            ("size", Value::Struct(vec![Value::I32(size.0), Value::I32(size.1)])),
            ("source_type", Value::U32(*source_type)),
        ]);
        let streams = Value::Array("(ua{sv})".into(), vec![Value::Struct(vec![Value::U32(node), props])]);
        let results = vec![("streams", streams)];
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
