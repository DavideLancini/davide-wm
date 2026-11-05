//! Password agents on the system bus: polkit's authentication agent (no
//! other runs in this session) and NetworkManager's secret agent (replacing
//! nm-applet's). Both ask through the compositor's password prompt.
//!
//! polkit checks the password itself: the agent hands user, cookie and
//! password to the socket-activated `polkit-agent-helper-1`
//! (`/run/polkit/agent-helper.socket`), which talks to polkitd.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use dbus::{Bus, Message, Value};

const POLKIT_PATH: &str = "/org/davide_wm/PolicyKit1/AuthenticationAgent";
const POLKIT_AGENT: &str = "org.freedesktop.PolicyKit1.AuthenticationAgent";
const NM_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const NM_AGENT: &str = "org.freedesktop.NetworkManager.SecretAgent";
const HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";
/// Wrong passwords before polkit's request is refused.
const TRIES: u32 = 3;

pub enum Request {
    Polkit { call: Message, cookie: String, user: String, message: String, failures: u32 },
    Secret { call: Message, ssid: String, setting: String },
}

impl Request {
    /// Title and explanation for the prompt.
    pub fn texts(&self) -> (String, String) {
        match self {
            Request::Polkit { message, user, .. } => {
                let who = if *user == crate::lock::user() { String::new() } else { format!(" (utente {user})") };
                ("Autenticazione richiesta".into(), format!("{message}{who}"))
            }
            Request::Secret { ssid, .. } => ("Wi-Fi".into(), format!("Password per la rete {ssid}")),
        }
    }
}

/// What the compositor has to do.
pub enum Action {
    /// Show the prompt for the front request.
    Prompt,
    /// The front request went away: close its prompt.
    Close,
}

pub struct Agents {
    bus: Option<Bus>,
    /// Waiting requests; the front one has the prompt.
    pub queue: VecDeque<Request>,
}

fn session_id() -> Option<String> {
    std::env::var("XDG_SESSION_ID").ok().filter(|s| !s.is_empty())
}

impl Agents {
    pub fn start() -> Agents {
        let bus = match Bus::system() {
            Ok(b) => Some(b),
            Err(e) => {
                log!("agents: no system bus: {e}");
                None
            }
        };
        let mut a = Agents { bus, queue: VecDeque::new() };
        a.register();
        a
    }

    fn register(&mut self) {
        let Some(bus) = &mut self.bus else { return };
        match session_id() {
            Some(id) => {
                let subject = Value::Struct(vec![Value::Str("unix-session".into()), Value::dict(vec![("session-id", Value::Str(id))])]);
                let locale = std::env::var("LANG").unwrap_or_else(|_| "C".into());
                let m = Message::call(
                    "org.freedesktop.PolicyKit1",
                    "/org/freedesktop/PolicyKit1/Authority",
                    "org.freedesktop.PolicyKit1.Authority",
                    "RegisterAuthenticationAgent",
                    vec![subject, Value::Str(locale), Value::Str(POLKIT_PATH.into())],
                );
                if let Err(e) = bus.call(m, Duration::from_secs(2)) {
                    log!("polkit agent: {e}");
                }
            }
            None => log!("polkit agent: no XDG_SESSION_ID"),
        }
        let m = Message::call(
            "org.freedesktop.NetworkManager",
            "/org/freedesktop/NetworkManager/AgentManager",
            "org.freedesktop.NetworkManager.AgentManager",
            "Register",
            vec![Value::Str("davide-wm".into())],
        );
        if let Err(e) = bus.call(m, Duration::from_secs(2)) {
            log!("NetworkManager secret agent: {e}");
        }
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

    pub fn dispatch(&mut self) -> Vec<Action> {
        let Some(bus) = &mut self.bus else { return Vec::new() };
        if let Err(e) = bus.read() {
            log!("agents: {e}");
            self.bus = None;
            return Vec::new();
        }
        let mut msgs = Vec::new();
        while let Some(m) = self.bus.as_mut().unwrap().pop() {
            msgs.push(m);
        }
        let mut actions = Vec::new();
        for m in msgs {
            self.call(m, &mut actions);
        }
        actions
    }

    fn enqueue(&mut self, r: Request, actions: &mut Vec<Action>) {
        self.queue.push_back(r);
        if self.queue.len() == 1 {
            actions.push(Action::Prompt);
        }
    }

    fn call(&mut self, m: Message, actions: &mut Vec<Action>) {
        let path = m.path.as_deref().unwrap_or("");
        if m.kind == 1 && path != POLKIT_PATH && path != NM_PATH {
            if !m.no_reply() {
                self.send(m.error_reply("org.freedesktop.DBus.Error.UnknownObject", "no such object"));
            }
            return;
        }
        let b = &m.body;
        if m.is_call(POLKIT_AGENT, "BeginAuthentication") {
            let message = b.get(1).and_then(Value::as_str).unwrap_or("").to_string();
            let cookie = b.get(4).and_then(Value::as_str).unwrap_or("").to_string();
            let me = unsafe { libc::getuid() };
            let users: Vec<u32> = match b.get(5) {
                Some(Value::Array(_, ids)) => ids
                    .iter()
                    .filter_map(|id| match id {
                        Value::Struct(f) if f.first().and_then(Value::as_str) == Some("unix-user") => f.get(1)?.get("uid")?.as_u32(),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let uid = users.iter().copied().find(|u| *u == me).or(users.first().copied());
            let Some(user) = uid.and_then(user_name) else {
                self.send(m.error_reply("org.freedesktop.PolicyKit1.Error.Failed", "no user to authenticate"));
                return;
            };
            self.enqueue(Request::Polkit { call: m, cookie, user, message, failures: 0 }, actions);
        } else if m.is_call(POLKIT_AGENT, "CancelAuthentication") {
            let cookie = b.first().and_then(Value::as_str).unwrap_or("").to_string();
            self.cancel(|r| matches!(r, Request::Polkit { cookie: c, .. } if *c == cookie), actions);
            self.send(m.reply(Vec::new()));
        } else if m.is_call(NM_AGENT, "GetSecrets") {
            let setting = b.get(2).and_then(Value::as_str).unwrap_or("").to_string();
            let flags = b.get(4).and_then(Value::as_u32).unwrap_or(0);
            let conn = b.first();
            let ssid = conn.and_then(|c| c.get("802-11-wireless")).and_then(|w| w.get("ssid")).and_then(Value::bytes);
            match (ssid, setting.as_str(), flags & 1 != 0) {
                (Some(ssid), "802-11-wireless-security", true) => {
                    let ssid = String::from_utf8_lossy(&ssid).into_owned();
                    self.enqueue(Request::Secret { call: m, ssid, setting }, actions);
                }
                _ => self.send(m.error_reply("org.freedesktop.NetworkManager.SecretAgent.NoSecrets", "only Wi-Fi passwords, interactively")),
            }
        } else if m.is_call(NM_AGENT, "CancelGetSecrets") {
            let setting = b.get(1).and_then(Value::as_str).unwrap_or("").to_string();
            self.cancel(|r| matches!(r, Request::Secret { setting: s, .. } if *s == setting), actions);
            self.send(m.reply(Vec::new()));
        } else if m.is_call(NM_AGENT, "SaveSecrets") || m.is_call(NM_AGENT, "DeleteSecrets") {
            self.send(m.reply(Vec::new()));
        } else if m.kind == 1 && !m.no_reply() {
            self.send(m.error_reply("org.freedesktop.DBus.Error.UnknownMethod", "unknown method"));
        }
    }

    /// Drops matching requests, answering them as cancelled.
    fn cancel(&mut self, which: impl Fn(&Request) -> bool, actions: &mut Vec<Action>) {
        let front = self.queue.front().is_some_and(&which);
        let gone: Vec<Request> = {
            let (gone, kept): (Vec<Request>, Vec<Request>) = std::mem::take(&mut self.queue).into_iter().partition(|r| which(r));
            self.queue = kept.into();
            gone
        };
        for r in gone {
            self.refuse(r);
        }
        if front {
            actions.push(Action::Close);
            if !self.queue.is_empty() {
                actions.push(Action::Prompt);
            }
        }
    }

    fn refuse(&mut self, r: Request) {
        let reply = match r {
            Request::Polkit { call, .. } => call.error_reply("org.freedesktop.PolicyKit1.Error.Cancelled", "cancelled by the user"),
            Request::Secret { call, .. } => call.error_reply("org.freedesktop.NetworkManager.SecretAgent.UserCanceled", "cancelled by the user"),
        };
        self.send(reply);
    }

    /// The user dismissed the prompt.
    pub fn dismissed(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        if let Some(r) = self.queue.pop_front() {
            self.refuse(r);
            actions.push(Action::Close);
            if !self.queue.is_empty() {
                actions.push(Action::Prompt);
            }
        }
        actions
    }

    /// The user typed a password. A Wi-Fi secret is answered at once; a
    /// polkit one returns (user, cookie) for `polkit_check` on the worker.
    pub fn answered(&mut self, password: String) -> (Vec<Action>, Option<(String, String, String)>) {
        let mut actions = Vec::new();
        match self.queue.front() {
            Some(Request::Secret { .. }) => {
                let Some(Request::Secret { call, setting, .. }) = self.queue.pop_front() else { unreachable!() };
                let secrets = Value::Array(
                    "{sa{sv}}".into(),
                    vec![Value::DictEntry(Box::new(Value::Str(setting)), Box::new(Value::dict(vec![("psk", Value::Str(password))])))],
                );
                self.send(call.reply(vec![secrets]));
                actions.push(Action::Close);
                if !self.queue.is_empty() {
                    actions.push(Action::Prompt);
                }
                (actions, None)
            }
            Some(Request::Polkit { user, cookie, .. }) => {
                let job = (user.clone(), cookie.clone(), password);
                (actions, Some(job))
            }
            None => (actions, None),
        }
    }

    /// The helper's verdict on the front polkit request: true closes it,
    /// false lets the user try again (up to `TRIES`).
    pub fn polkit_result(&mut self, ok: bool) -> (Vec<Action>, bool) {
        let mut actions = Vec::new();
        let Some(Request::Polkit { failures, .. }) = self.queue.front_mut() else { return (actions, true) };
        if ok {
            let Some(Request::Polkit { call, .. }) = self.queue.pop_front() else { unreachable!() };
            self.send(call.reply(Vec::new()));
        } else {
            *failures += 1;
            if *failures < TRIES {
                return (actions, false);
            }
            let r = self.queue.pop_front().unwrap();
            self.refuse(r);
        }
        actions.push(Action::Close);
        if !self.queue.is_empty() {
            actions.push(Action::Prompt);
        }
        (actions, true)
    }
}

fn user_name(uid: u32) -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(uid);
        (!pw.is_null()).then(|| std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
    }
}

/// Runs polkit's helper: user and cookie, then the password at each prompt.
/// Blocking: for the worker.
pub fn polkit_check(user: &str, cookie: &str, password: String) -> bool {
    let mut password = password.into_bytes();
    let result = (|| -> std::io::Result<bool> {
        let mut s = UnixStream::connect(HELPER_SOCKET)?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        s.write_all(format!("{user}\n{cookie}\n").as_bytes())?;
        let mut reader = BufReader::new(s.try_clone()?);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(false);
            }
            let l = line.trim_end();
            if l.starts_with("PAM_PROMPT_ECHO_OFF") || l.starts_with("PAM_PROMPT_ECHO_ON") {
                s.write_all(&password)?;
                s.write_all(b"\n")?;
            } else if l == "SUCCESS" {
                return Ok(true);
            } else if l == "FAILURE" {
                return Ok(false);
            }
        }
    })();
    password.fill(0);
    result.unwrap_or_else(|e| {
        log!("polkit helper: {e}");
        false
    })
}
