//! The xdg-desktop-portal backend (`org.freedesktop.impl.portal.desktop.davidewm`):
//! ScreenCast, Screenshot and FileChooser. xdg-desktop-portal forwards the
//! apps' requests here; a cast source is chosen in the launcher box and
//! streamed by `cast.rs`, files are chosen in a file manager window. The
//! same connection owns `org.freedesktop.FileManager1` ("show in folder").

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::path::PathBuf;

use dbus::{Bus, Message, Value};

const NAME: &str = "org.freedesktop.impl.portal.desktop.davidewm";
const SCREENCAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const SCREENSHOT: &str = "org.freedesktop.impl.portal.Screenshot";
const FILECHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";
const FILEMANAGER: &str = "org.freedesktop.FileManager1";
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
    /// A file chooser: the app's filters (to name the one in use) and, for
    /// SaveFiles, the names to save in the chosen folder.
    Choose { call: Message, handle: String, filters: Vec<Value>, names: Option<Vec<String>> },
}

/// A file chooser request, as the file manager needs it.
pub struct ChooseFiles {
    pub request: u32,
    pub title: String,
    pub save: bool,
    pub multiple: bool,
    pub directory: bool,
    pub folder: Option<PathBuf>,
    pub name: String,
    /// Filter names and patterns: globs, or `mime:` and a MIME type.
    pub filters: Vec<(String, Vec<String>)>,
    pub current: Option<usize>,
    pub accept: String,
}

/// What the compositor has to do for the portal.
pub enum Action {
    /// Ask the user, unless `reuse` (a choice as `CastChoice` encodes it)
    /// still makes sense.
    PickSource { request: u32, types: u32, reuse: Option<String> },
    CancelPick { request: u32 },
    StopCast(u32),
    Screenshot { request: u32, interactive: bool },
    Choose(Box<ChooseFiles>),
    /// FileManager1: folders to show, or items to show selected.
    ShowFiles { paths: Vec<PathBuf>, items: bool },
}

pub struct Portal {
    /// The last choice: app, choice, when.
    last: Option<(String, String, std::time::Instant)>,
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

/// A NUL-terminated `ay` path.
fn byte_path(v: Option<&Value>) -> Option<PathBuf> {
    let mut b = v?.bytes()?;
    while b.last() == Some(&0) {
        b.pop();
    }
    (!b.is_empty()).then(|| PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(b)))
}

fn strings(v: Option<&Value>) -> Vec<String> {
    match v.map(Value::unwrap_variant) {
        Some(Value::Array(_, items)) => items.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// `a(sa(us))` filters: names and patterns.
fn filters(v: Option<&Value>) -> (Vec<Value>, Vec<(String, Vec<String>)>) {
    let Some(Value::Array(_, list)) = v.map(Value::unwrap_variant) else { return (Vec::new(), Vec::new()) };
    let parsed = list
        .iter()
        .filter_map(|f| {
            let Value::Struct(fields) = f else { return None };
            let name = fields.first()?.as_str()?.to_string();
            let Some(Value::Array(_, pats)) = fields.get(1) else { return None };
            let pats = pats
                .iter()
                .filter_map(|p| match p {
                    Value::Struct(pf) => match (pf.first().and_then(Value::as_u32), pf.get(1).and_then(Value::as_str)) {
                        (Some(0), Some(g)) => Some(g.to_lowercase()),
                        (Some(1), Some(m)) => Some(format!("mime:{}", m.to_lowercase())),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            Some((name, pats))
        })
        .collect();
    (list.clone(), parsed)
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
 <interface name="org.freedesktop.impl.portal.FileChooser">
  <method name="OpenFile"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="SaveFile"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="SaveFiles"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <property name="version" type="u" access="read"/>
 </interface>
 <interface name="org.freedesktop.FileManager1">
  <method name="ShowFolders"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
  <method name="ShowItems"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
  <method name="ShowItemProperties"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
 </interface>
</node>"#;

impl Portal {
    /// Takes the backend name; without a session bus or the name, the
    /// portal stays off.
    pub fn start() -> Portal {
        let bus = match Bus::session() {
            Ok(mut bus) => match bus.request_name(NAME, 4) {
                Ok(true) => {
                    // Taken from another file manager if it allows that.
                    match bus.request_name(FILEMANAGER, 2 | 4) {
                        Ok(true) => {}
                        Ok(false) => log!("portal: {FILEMANAGER} is taken"),
                        Err(e) => log!("portal: {FILEMANAGER}: {e}"),
                    }
                    Some(bus)
                }
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
        Portal { last: None, bus, sessions: HashMap::new(), pending: HashMap::new(), next: 0 }
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
            FILECHOOSER => vec![("version", Value::U32(4))],
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
        } else if m.is_call(FILECHOOSER, "OpenFile") || m.is_call(FILECHOOSER, "SaveFile") || m.is_call(FILECHOOSER, "SaveFiles") {
            let member = m.member.clone().unwrap_or_default();
            let opts = b.get(4);
            let save = member != "OpenFile";
            let many = member == "SaveFiles";
            let (raw, parsed) = filters(option(opts, "filters"));
            let current = option(opts, "current_filter").and_then(|c| raw.iter().position(|f| f == c.unwrap_variant()));
            let file = byte_path(option(opts, "current_file"));
            let names = many.then(|| {
                let Some(Value::Array(_, list)) = option(opts, "files") else { return Vec::new() };
                list.iter().filter_map(|v| byte_path(Some(v))).map(|p| p.to_string_lossy().into_owned()).collect()
            });
            self.next += 1;
            let request = self.next;
            let c = ChooseFiles {
                request,
                title: b.get(3).and_then(Value::as_str).unwrap_or("").to_string(),
                save: save && !many,
                multiple: option(opts, "multiple").and_then(Value::as_bool).unwrap_or(false),
                directory: many || option(opts, "directory").and_then(Value::as_bool).unwrap_or(false),
                folder: byte_path(option(opts, "current_folder")).or_else(|| file.as_ref().and_then(|f| f.parent().map(|p| p.to_path_buf()))),
                name: option(opts, "current_name").and_then(Value::as_str).map(str::to_string).or_else(|| file.as_ref().and_then(|f| f.file_name()).map(|n| n.to_string_lossy().into_owned())).unwrap_or_default(),
                filters: parsed,
                current,
                accept: option(opts, "accept_label").and_then(Value::as_str).unwrap_or("").replace('_', ""),
            };
            let handle = path(b.first());
            self.pending.insert(request, Pending::Choose { call: m, handle, filters: raw, names });
            actions.push(Action::Choose(Box::new(c)));
        } else if m.is_call(FILEMANAGER, "ShowFolders") || m.is_call(FILEMANAGER, "ShowItems") || m.is_call(FILEMANAGER, "ShowItemProperties") {
            let items = m.member.as_deref() != Some("ShowFolders");
            let paths = strings(b.first()).iter().filter_map(|u| files::uri::from_uri(u).or_else(|| u.starts_with('/').then(|| PathBuf::from(u)))).collect();
            actions.push(Action::ShowFiles { paths, items });
            self.send(m.reply(Vec::new()));
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
            let found = self.pending.iter().find(|(_, v)| matches!(v, Pending::Start { handle, .. } | Pending::Shot { handle, .. } | Pending::Choose { handle, .. } if *handle == p)).map(|(k, _)| *k);
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
            Pending::Start { call, .. } | Pending::Shot { call, .. } | Pending::Choose { call, .. } => call,
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

    /// The file chooser closed: the files picked, or None.
    pub fn files_chosen(&mut self, request: u32, paths: Option<Vec<PathBuf>>, filter: Option<usize>) {
        let Some(Pending::Choose { filters, names, .. }) = self.pending.get(&request) else { return };
        let Some(mut paths) = paths else { return self.finish(request, CANCELLED, Vec::new()) };
        if let (Some(names), Some(dir)) = (names, paths.first().cloned()) {
            paths = names.iter().map(|n| dir.join(n)).collect();
        }
        let uris = Value::Array("s".into(), paths.iter().map(|p| Value::Str(files::uri::to_uri(p))).collect());
        let mut results = vec![("uris", uris)];
        if let Some(f) = filter.and_then(|k| filters.get(k)).cloned() {
            results.push(("current_filter", f));
        }
        self.finish(request, SUCCESS, results);
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
