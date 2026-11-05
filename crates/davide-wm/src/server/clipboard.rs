//! Clipboard history (replaces `wl-paste --watch cliphist store` and the
//! wofi picker): every new clipboard selection is read from its owner on a
//! worker thread, kept newest first and saved under
//! `$XDG_STATE_HOME/davide-wm/clipboard.d` so it survives reboots: an
//! index with the text, and a file per image.

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
pub fn label(e: &Entry) -> String {
    match &e.data {
        Some(data) if is_text(&e.mime) => {
            let text = String::from_utf8_lossy(data);
            let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut out: String = flat.chars().take(100).collect();
            if flat.chars().count() > 100 {
                out.push('…');
            }
            out
        }
        _ => format!("[{}, {} KB]", e.mime, e.len.div_ceil(1024)),
    }
}

/// A history entry. Text stays in memory; anything else (images) lives in
/// a file of its own, named after the hash, and is read when picked.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub mime: String,
    /// FNV-1a of the data.
    pub hash: u64,
    pub len: usize,
    data: Option<Vec<u8>>,
}

impl Entry {
    fn same(&self, hash: u64, len: usize, mime: &str) -> bool {
        self.hash == hash && self.len == len && is_text(&self.mime) == is_text(mime)
    }
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}

#[derive(Default)]
pub struct History {
    /// Newest first.
    pub entries: Vec<Entry>,
    /// Where entries kept out of memory go; without it all stay in memory.
    dir: Option<PathBuf>,
}

impl state::Persist for History {
    fn save(&self, e: &mut state::Enc) {
        self.entries.len().save(e);
        for x in &self.entries {
            (x.mime.clone(), x.hash, x.len).save(e);
            x.data.is_some().save(e);
            if let Some(d) = &x.data {
                state::save_bytes(d, e);
            }
        }
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        if crate::server::persist::FORMAT.load(std::sync::atomic::Ordering::Relaxed) < 10 {
            let clips: Vec<Clip> = state::Persist::load(d)?;
            let mut h = History { entries: Vec::new(), dir: Some(dir()) };
            h.import(clips);
            return Ok(h);
        }
        let n = usize::load(d)?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let (mime, hash, len) = state::Persist::load(d)?;
            let data = if bool::load(d)? { Some(state::load_bytes(d)?) } else { None };
            entries.push(Entry { mime, hash, len, data });
        }
        Ok(History { entries, dir: Some(dir()) })
    }
}

const MAGIC: &[u8] = b"DWCL1\n";
const INDEX_MAGIC: &[u8] = b"DWCL2\n";

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("davide-wm")
}

fn dir() -> PathBuf {
    state_dir().join("clipboard.d")
}

fn blob_name(hash: u64) -> String {
    format!("{hash:016x}")
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
        let (hash, len) = (fnv(&clip.data), clip.data.len());
        self.entries.retain(|e| !e.same(hash, len, &clip.mime));
        let entry = self.store(clip, hash);
        self.entries.insert(0, entry);
        for gone in self.entries.split_off(self.entries.len().min(config::CLIPBOARD_HISTORY)) {
            self.forget(&gone);
        }
    }

    /// Keeps text in memory and writes anything else to its file.
    fn store(&self, clip: Clip, hash: u64) -> Entry {
        let len = clip.data.len();
        let data = match &self.dir {
            Some(dir) if !is_text(&clip.mime) => match write_private(&dir.join(blob_name(hash)), &clip.data) {
                Ok(()) => None,
                Err(e) => {
                    log!("clipboard: keeping an entry in memory: {e}");
                    Some(clip.data)
                }
            },
            _ => Some(clip.data),
        };
        Entry { mime: clip.mime, hash, len, data }
    }

    /// Deletes the file of an entry no longer in the history.
    fn forget(&self, gone: &Entry) {
        if let (None, Some(dir)) = (&gone.data, &self.dir) {
            if !self.entries.iter().any(|e| e.hash == gone.hash && e.data.is_none()) {
                let _ = std::fs::remove_file(dir.join(blob_name(gone.hash)));
            }
        }
    }

    /// The clip of entry `i`, read from its file when not in memory.
    pub fn get(&self, i: usize) -> Option<Clip> {
        let e = self.entries.get(i)?;
        let data = match &e.data {
            Some(d) => d.clone(),
            None => std::fs::read(self.dir.as_ref()?.join(blob_name(e.hash))).inspect_err(|err| log!("clipboard entry {:016x}: {err}", e.hash)).ok()?,
        };
        Some(Clip { mime: e.mime.clone(), data })
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.entries.len() {
            let gone = self.entries.remove(i);
            self.forget(&gone);
        }
    }

    /// Bytes of entry data held in memory.
    pub fn in_memory(&self) -> usize {
        self.entries.iter().filter_map(|e| e.data.as_ref()).map(Vec::len).sum()
    }

    /// Appends older entries below ours, skipping what we already have;
    /// returns how many were added.
    pub fn import(&mut self, older: Vec<Clip>) -> usize {
        let before = self.entries.len();
        for clip in older {
            if self.entries.len() >= config::CLIPBOARD_HISTORY {
                break;
            }
            let (hash, len) = (fnv(&clip.data), clip.data.len());
            if len == 0 || len > config::CLIPBOARD_MAX_BYTES || self.entries.iter().any(|e| e.same(hash, len, &clip.mime)) {
                continue;
            }
            let entry = self.store(clip, hash);
            self.entries.push(entry);
        }
        self.entries.len() - before
    }

    /// The index: every entry's type, hash and size, and the text itself.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = INDEX_MAGIC.to_vec();
        for e in &self.entries {
            out.extend_from_slice(&(e.mime.len() as u32).to_le_bytes());
            out.extend_from_slice(e.mime.as_bytes());
            out.extend_from_slice(&e.hash.to_le_bytes());
            out.extend_from_slice(&(e.len as u64).to_le_bytes());
            match &e.data {
                Some(d) => {
                    out.push(1);
                    out.extend_from_slice(d);
                }
                None => out.push(0),
            }
        }
        out
    }

    fn decode(mut b: &[u8], dir: Option<PathBuf>) -> History {
        let mut h = History { entries: Vec::new(), dir };
        if !b.starts_with(INDEX_MAGIC) {
            return h;
        }
        b = &b[INDEX_MAGIC.len()..];
        let mut take = |n: usize| -> Option<&[u8]> {
            let (head, rest) = (b.get(..n)?, b.get(n..)?);
            b = rest;
            Some(head)
        };
        let mut next = || {
            let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let mime = String::from_utf8(take(n)?.to_vec()).ok()?;
            let hash = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let len = u64::from_le_bytes(take(8)?.try_into().ok()?) as usize;
            let data = match take(1)?[0] {
                1 => Some(take(len)?.to_vec()),
                _ => None,
            };
            Some(Entry { mime, hash, len, data })
        };
        while let Some(e) = next() {
            h.entries.push(e);
        }
        h
    }

    /// The saved history; the single-file format of before is moved over.
    pub fn load() -> History {
        let dir = dir();
        if let Ok(b) = std::fs::read(dir.join("index")) {
            let h = History::decode(&b, Some(dir));
            h.collect_garbage();
            return h;
        }
        let mut h = History { entries: Vec::new(), dir: Some(dir) };
        if let Ok(b) = std::fs::read(state_dir().join("clipboard")) {
            h.import(decode_v1(&b));
            let _ = save(&h.encode());
        }
        h
    }

    /// Removes files no entry refers to (left by a crash).
    fn collect_garbage(&self) {
        let Some(dir) = &self.dir else { return };
        let keep: std::collections::HashSet<String> = self.entries.iter().filter(|e| e.data.is_none()).map(|e| blob_name(e.hash)).collect();
        for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.len() == 16 && !keep.contains(&name) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

/// Clips in the single-file format of before.
pub fn decode_v1(mut b: &[u8]) -> Vec<Clip> {
    let mut out = Vec::new();
    if !b.starts_with(MAGIC) {
        return out;
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
        out.push(c);
    }
    out
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?.write_all(bytes)?;
    std::fs::rename(tmp, path)
}

/// Writes the encoded index, readable by the owner only; the single file
/// of before goes once the index holds its entries.
pub fn save(bytes: &[u8]) -> io::Result<()> {
    write_private(&dir().join("index"), bytes)?;
    let _ = std::fs::remove_file(state_dir().join("clipboard"));
    Ok(())
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
        let back = History::decode(&h.encode(), None);
        assert_eq!(back.entries, h.entries);
        assert!(History::decode(b"junk", None).entries.is_empty());
        assert_eq!(h.get(1), Some(text("b")));
    }

    #[test]
    fn mime_choice() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_mime(&m(&["image/png", "UTF8_STRING", "text/plain"])).as_deref(), Some("text/plain"));
        assert_eq!(pick_mime(&m(&["image/jpeg", "image/png"])).as_deref(), Some("image/png"));
        assert_eq!(pick_mime(&m(&["text/plain", SECRET])), None);
        assert_eq!(pick_mime(&m(&["application/x-foo"])), None);
        let mut h = History::default();
        h.push(text("uno\n  due\ttre"));
        assert_eq!(label(&h.entries[0]), "uno due tre");
    }
}
