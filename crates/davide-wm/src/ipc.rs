//! Command socket for scripts: one line in, a reply out, then the
//! connection closes. `davide-wm msg <command>` is the client.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use wm::WsId;

pub const ENV: &str = "DAVIDE_WM_SOCKET";

/// Every wait is bounded (README sec. 8, timeout rule).
const TIMEOUT: Duration = Duration::from_millis(200);

pub struct Ipc {
    pub listener: UnixListener,
    pub path: PathBuf,
}

impl Ipc {
    pub fn bind(wayland_name: &str) -> io::Result<Ipc> {
        let dir = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR not set"))?);
        let path = dir.join(format!("davide-wm-{wayland_name}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Ipc { listener, path })
    }
}

impl state::Persist for Ipc {
    fn save(&self, e: &mut state::Enc) {
        state::save_fd(&self.listener, e);
        self.path.to_string_lossy().into_owned().save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        let listener = UnixListener::from(std::os::fd::OwnedFd::load(d)?);
        let path: String = Persist::load(d)?;
        Ok(Ipc { listener, path: path.into() })
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub enum Command {
    Workspace(WsId),
    Exec(String),
    Close,
    Windows,
    FocusApp(String),
    Upgrade,
    /// Test helpers: move the pointer, click the left button, report the focus.
    Pointer(f64, f64),
    Click,
    /// Left button down (true) or up, for drags.
    Button(bool),
    /// Press evdev key codes in order, release them in reverse (a chord).
    Key(Vec<u32>),
    Focused,
    /// Black screen until the next input.
    Blank,
    Wake,
    /// Shows the screensaver, or changes it: cycle, on, off, auto, seconds.
    Screensaver(Option<String>),
    /// Shows the lid switch inhibitor; true toggles it.
    LidInhibit(bool),
    Outputs,
    Clipboard,
    Notifications,
    /// Writes what the primary output shows to a PNG file.
    Screenshot(String),
    /// Starts a cast of the output (or of the focused window) without asking;
    /// answers with the PipeWire node.
    Cast(bool),
    CastStop(u32),
    Exit,
}

pub fn parse_ws(s: &str) -> Option<WsId> {
    match s {
        "chat" => Some(WsId::Chat),
        "special" | "resize" => Some(WsId::Special),
        n => n.parse().ok().filter(|n| (1..=41).contains(n)).map(WsId::Num),
    }
}

pub fn ws_name(ws: WsId) -> String {
    match ws {
        WsId::Num(n) => n.to_string(),
        WsId::Chat => "chat".into(),
        WsId::Special => "special".into(),
    }
}

fn parse(line: &str) -> Result<Command, String> {
    let (cmd, arg) = line.trim().split_once(' ').map_or((line.trim(), ""), |(c, a)| (c, a.trim()));
    match cmd {
        "workspace" => parse_ws(arg).map(Command::Workspace).ok_or_else(|| format!("bad workspace {arg:?}")),
        "exec" if !arg.is_empty() => Ok(Command::Exec(arg.to_string())),
        "close" => Ok(Command::Close),
        "windows" => Ok(Command::Windows),
        "focus-app" if !arg.is_empty() => Ok(Command::FocusApp(arg.to_string())),
        "upgrade" => Ok(Command::Upgrade),
        "pointer" => {
            let mut it = arg.split_whitespace().map(str::parse::<f64>);
            match (it.next(), it.next()) {
                (Some(Ok(x)), Some(Ok(y))) => Ok(Command::Pointer(x, y)),
                _ => Err("pointer <x> <y>".into()),
            }
        }
        "click" => Ok(Command::Click),
        "press" => Ok(Command::Button(true)),
        "release" => Ok(Command::Button(false)),
        "key" => match arg.split_whitespace().map(str::parse).collect::<Result<Vec<u32>, _>>() {
            Ok(codes) if !codes.is_empty() => Ok(Command::Key(codes)),
            _ => Err("key <evdev code>...".into()),
        },
        "focused" => Ok(Command::Focused),
        "blank" => Ok(Command::Blank),
        "wake" => Ok(Command::Wake),
        "screensaver" => Ok(Command::Screensaver((!arg.is_empty()).then(|| arg.to_string()))),
        "lid-inhibit" => match arg {
            "" => Ok(Command::LidInhibit(false)),
            "toggle" => Ok(Command::LidInhibit(true)),
            _ => Err("lid-inhibit [toggle]".into()),
        },
        "outputs" => Ok(Command::Outputs),
        "clipboard" => Ok(Command::Clipboard),
        "notifications" => Ok(Command::Notifications),
        "screenshot" if !arg.is_empty() => Ok(Command::Screenshot(arg.to_string())),
        "cast" => Ok(Command::Cast(arg == "window")),
        "cast-stop" => arg.parse().map(Command::CastStop).map_err(|_| format!("bad cast id {arg:?}")),
        "exit" => Ok(Command::Exit),
        _ => Err("commands: workspace <1-41|chat|special>, exec <cmd>, close, windows, focus-app <app-id>, upgrade, pointer <x> <y>, click, press, release, key <code>..., focused, blank, wake, screensaver [cycle|on|off|auto|<secs>], lid-inhibit [toggle], outputs, clipboard, notifications, screenshot <file.png>, cast [window], cast-stop <id>, exit".into()),
    }
}

/// Reads one command from a new connection.
pub fn read_command(stream: &UnixStream) -> Result<Command, String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream.take(4096)).read_line(&mut line).map_err(|e| e.to_string())?;
    parse(&line)
}

pub fn reply(mut stream: &UnixStream, text: &str) {
    let _ = stream.write_all(text.as_bytes());
}

/// `davide-wm msg ...`
pub fn client(args: &[String]) -> io::Result<()> {
    let path = std::env::var(ENV).map_err(|_| io::Error::other(format!("{ENV} not set: not running inside davide-wm?")))?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(format!("{}\n", args.join(" ")).as_bytes())?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut out = String::new();
    stream.read_to_string(&mut out)?;
    print!("{out}");
    if out.starts_with("error") {
        std::process::exit(1);
    }
    Ok(())
}
