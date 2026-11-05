//! Clipboard history (replaces `wl-paste --watch cliphist store` and the
//! wofi picker): every new clipboard selection is read from its owner on a
//! worker thread, kept newest first and saved to
//! `$XDG_STATE_HOME/davide-wm/clipboard` so it survives reboots.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub mime: String,
    pub data: Vec<u8>,
}

impl state::Persist for Clip {
    fn save(&self, e: &mut state::Enc) {
        self.mime.save(e);
        state::save_bytes(&self.data, e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        Ok(Clip { mime: state::Persist::load(d)?, data: state::load_bytes(d)? })
    }
}

/// Text types in order of preference; a text clip is offered under all of them.
const TEXT: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
/// Password managers mark their entries so that history tools skip them.
const SECRET: &str = "x-kde-passwordManagerHint";

/// The type to record from an offer, or None to leave it out of the history.
pub fn pick_mime(mimes: &[String]) -> Option<String> {
    if mimes.iter().any(|m| m == SECRET) {
        return None;
    }
    let has = |m: &str| mimes.iter().any(|x| x == m);
    TEXT.iter()
        .find(|t| has(t))
        .map(|t| t.to_string())
        .or_else(|| mimes.iter().find(|m| *m == "image/png").cloned())
        .or_else(|| mimes.iter().find(|m| m.starts_with("image/")).cloned())
        .or_else(|| mimes.iter().find(|m| m.starts_with("text/")).cloned())
}

fn is_text(mime: &str) -> bool {
    TEXT.contains(&mime) || mime.starts_with("text/")
}

/// The types under which the compositor offers a clip it serves itself.
pub fn offered(clip: &Clip) -> Vec<String> {
    if TEXT.contains(&clip.mime.as_str()) {
        TEXT.iter().map(|t| t.to_string()).collect()
    } else {
        vec![clip.mime.clone()]
    }
}

/// One line for the picker.
pub fn label(clip: &Clip) -> String {
    if is_text(&clip.mime) {
        let text = String::from_utf8_lossy(&clip.data);
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out: String = flat.chars().take(100).collect();
        if flat.chars().count() > 100 {
            out.push('…');
        }
        out
    } else {
        format!("[{}, {} KB]", clip.mime, clip.data.len().div_ceil(1024))
    }
}

#[derive(Default)]
pub struct History {
    /// Newest first.
    pub entries: Vec<Clip>,
}

state::persist_struct!(History { entries });

const MAGIC: &[u8] = b"DWCL1\n";

fn path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    dir.join("davide-wm/clipboard")
}

impl History {
    /// Adds a clip on top; an identical one already present moves up instead.
    pub fn push(&mut self, clip: Clip) {
        if clip.data.is_empty() || clip.data.len() > config::CLIPBOARD_MAX_BYTES {
            return;
        }
        if is_text(&clip.mime) && clip.data.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let same = |c: &Clip| c.data == clip.data && is_text(&c.mime) == is_text(&clip.mime);
        self.entries.retain(|c| !same(c));
        self.entries.insert(0, clip);
        self.entries.truncate(config::CLIPBOARD_HISTORY);
    }

    /// Appends older entries (from another history) below ours, skipping
    /// what we already have; returns how many were added.
    pub fn import(&mut self, older: History) -> usize {
        let before = self.entries.len();
        for clip in older.entries {
            let dup = self.entries.iter().any(|c| c.data == clip.data && is_text(&c.mime) == is_text(&clip.mime));
            if !dup && !clip.data.is_empty() && clip.data.len() <= config::CLIPBOARD_MAX_BYTES {
                self.entries.push(clip);
            }
        }
        self.entries.truncate(config::CLIPBOARD_HISTORY);
        self.entries.len().saturating_sub(before)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for c in &self.entries {
            out.extend_from_slice(&(c.mime.len() as u32).to_le_bytes());
            out.extend_from_slice(c.mime.as_bytes());
            out.extend_from_slice(&(c.data.len() as u64).to_le_bytes());
            out.extend_from_slice(&c.data);
        }
        out
    }

    pub fn decode(mut b: &[u8]) -> History {
        let mut h = History::default();
        if !b.starts_with(MAGIC) {
            return h;
        }
        b = &b[MAGIC.len()..];
        let mut take = |n: usize| -> Option<Vec<u8>> {
            let (head, rest) = (b.get(..n)?, b.get(n..)?);
            b = rest;
            Some(head.to_vec())
        };
        let mut next = || {
            let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let mime = String::from_utf8(take(n)?).ok()?;
            let n = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
            Some(Clip { mime, data: take(n)? })
        };
        while let Some(c) = next() {
            h.entries.push(c);
        }
        h
    }

    pub fn load() -> History {
        std::fs::read(path()).map(|b| History::decode(&b)).unwrap_or_default()
    }
}

/// Writes the encoded history, readable by the owner only.
pub fn save(bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(bytes)?;
    std::fs::rename(tmp, path)
}

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Reads what the selection owner writes, for at most a few seconds.
pub fn capture(fd: OwnedFd) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("clipboard owner too slow".into());
        }
        let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            continue;
        }
        match file.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > config::CLIPBOARD_MAX_BYTES {
                    return Err("clipboard entry too large".into());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Writes a clip to a client's pipe on its own thread: the client reads at
/// its pace and may never finish.
pub fn serve(fd: OwnedFd, data: Vec<u8>) {
    std::thread::spawn(move || {
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
        let _ = std::fs::File::from(fd).write_all(&data);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Clip {
        Clip { mime: "text/plain;charset=utf-8".into(), data: s.as_bytes().to_vec() }
    }

    #[test]
    fn newest_first_without_duplicates() {
        let mut h = History::default();
        h.push(text("a"));
        h.push(text("b"));
        h.push(Clip { mime: "UTF8_STRING".into(), data: b"a".to_vec() });
        h.push(text("  \n"));
        assert_eq!(h.entries.iter().map(label).collect::<Vec<_>>(), ["a", "b"]);
        let back = History::decode(&h.encode());
        assert_eq!(back.entries, h.entries);
        assert!(History::decode(b"junk").entries.is_empty());
    }

    #[test]
    fn mime_choice() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_mime(&m(&["image/png", "UTF8_STRING", "text/plain"])).as_deref(), Some("text/plain"));
        assert_eq!(pick_mime(&m(&["image/jpeg", "image/png"])).as_deref(), Some("image/png"));
        assert_eq!(pick_mime(&m(&["text/plain", SECRET])), None);
        assert_eq!(pick_mime(&m(&["application/x-foo"])), None);
        assert_eq!(label(&text("uno\n  due\ttre")), "uno due tre");
    }
}
