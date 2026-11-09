//! File operations for a worker thread: copy, move, rename, delete and the
//! freedesktop trash, reporting through a shared [`Progress`].

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use crate::uri;

#[derive(Default)]
pub struct Progress {
    /// Bytes for transfers, entries for deletions and trashing.
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: AtomicBool,
    /// Name of the file being processed.
    pub current: Mutex<String>,
}

impl Progress {
    fn add(&self, n: u64) {
        self.done.fetch_add(n, Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    fn set_current(&self, path: &Path) {
        if let Ok(mut c) = self.current.lock() {
            *c = name(path);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Copy,
    Move,
}

const CANCELLED: &str = "annullato";

enum Fail {
    Cancel,
    Io(PathBuf, io::Error),
}

trait At<T> {
    fn at(self, path: &Path) -> Result<T, Fail>;
}

impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T, Fail> {
        self.map_err(|e| Fail::Io(path.to_path_buf(), e))
    }
}

fn name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

/// The error text without the "(os error N)" suffix.
fn reason(e: &io::Error) -> String {
    let s = e.to_string();
    s.find(" (os error").map_or(s.clone(), |i| s[..i].to_string())
}

fn message(verb: &str, f: Fail) -> String {
    match f {
        Fail::Cancel => CANCELLED.into(),
        Fail::Io(p, e) => format!("impossibile {verb} {}: {}", name(&p), reason(&e)),
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Bytes to transfer under `path`: each entry counts at least 1.
fn tree_size(path: &Path, p: &Progress) -> u64 {
    let Ok(m) = fs::symlink_metadata(path) else { return 1 };
    if !m.is_dir() {
        return if m.is_file() { m.len().max(1) } else { 1 };
    }
    let children = fs::read_dir(path).into_iter().flatten().flatten();
    1 + children.take_while(|_| !p.cancelled()).map(|e| tree_size(&e.path(), p)).sum::<u64>()
}

fn tree_count(path: &Path, p: &Progress) -> u64 {
    let children = fs::read_dir(path).into_iter().flatten().flatten();
    let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    1 + if is_dir { children.take_while(|_| !p.cancelled()).map(|e| tree_count(&e.path(), p)).sum() } else { 0 }
}

fn times(m: &fs::Metadata) -> fs::FileTimes {
    let mut t = fs::FileTimes::new();
    if let Ok(x) = m.modified() {
        t = t.set_modified(x);
    }
    if let Ok(x) = m.accessed() {
        t = t.set_accessed(x);
    }
    t
}

fn copy_data(from: &mut File, to: &mut File, src: &Path, dst: &Path, p: &Progress) -> Result<(), Fail> {
    const CHUNK: usize = 4 << 20;
    let mut buf = Vec::new();
    loop {
        if p.cancelled() {
            return Err(Fail::Cancel);
        }
        let n = if buf.is_empty() {
            // SAFETY: both fds are open for the duration of the call; null offsets use the file positions.
            let r = unsafe { libc::copy_file_range(from.as_raw_fd(), std::ptr::null_mut(), to.as_raw_fd(), std::ptr::null_mut(), CHUNK, 0) };
            if r < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => {}
                    Some(libc::EXDEV | libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM) => buf = vec![0; 1 << 20],
                    _ => return Err(Fail::Io(src.into(), e)),
                }
                continue;
            }
            r as usize
        } else {
            let n = match from.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Fail::Io(src.into(), e)),
            };
            to.write_all(&buf[..n]).at(dst)?;
            n
        };
        if n == 0 {
            return Ok(());
        }
        p.add(n as u64);
    }
}

fn copy_file(src: &Path, dst: &Path, m: &fs::Metadata, p: &Progress) -> Result<(), Fail> {
    p.set_current(src);
    let mut from = File::open(src).at(src)?;
    let mut to = OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst).at(dst)?;
    let r = copy_data(&mut from, &mut to, src, dst, p).and_then(|()| {
        to.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777)).at(dst)?;
        to.set_times(times(m)).at(dst)
    });
    if r.is_err() {
        let _ = fs::remove_file(dst);
    } else if m.len() == 0 {
        p.add(1);
    }
    r
}

/// Copies `src` to the new path `dst`: directories recursively, symlinks as
/// symlinks; fifos, sockets and devices are skipped.
fn copy_tree(src: &Path, dst: &Path, p: &Progress) -> Result<(), Fail> {
    if p.cancelled() {
        return Err(Fail::Cancel);
    }
    let m = fs::symlink_metadata(src).at(src)?;
    let ft = m.file_type();
    if ft.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src).at(src)?, dst).at(dst)?;
        p.add(1);
    } else if ft.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(dst).at(dst)?;
        p.add(1);
        for e in fs::read_dir(src).at(src)? {
            let e = e.at(src)?;
            copy_tree(&e.path(), &dst.join(e.file_name()), p)?;
        }
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777)).at(dst)?;
        let _ = File::open(dst).and_then(|f| f.set_times(times(&m)));
    } else if ft.is_file() {
        copy_file(src, dst, &m, p)?;
    } else {
        p.add(1);
    }
    Ok(())
}

/// Removes `path` recursively without following symlinks, counting entries.
fn remove_tree(path: &Path, p: &Progress) -> Result<(), Fail> {
    if p.cancelled() {
        return Err(Fail::Cancel);
    }
    let m = fs::symlink_metadata(path).at(path)?;
    if m.is_dir() {
        for e in fs::read_dir(path).at(path)? {
            remove_tree(&e.at(path)?.path(), p)?;
        }
        fs::remove_dir(path).at(path)?;
    } else {
        fs::remove_file(path).at(path)?;
    }
    p.add(1);
    Ok(())
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("nome non valido"))
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (a, b) = (cstr(from)?, cstr(to)?);
    // SAFETY: both are valid NUL-terminated paths.
    let r = unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_NOREPLACE) };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
        if fs::symlink_metadata(to).is_ok() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        return fs::rename(from, to);
    }
    Err(e)
}

fn transfer_one(op: Op, src: &Path, dest_dir: &Path, canon_dest: &Path, same_fs: bool, p: &Progress) -> Result<Option<PathBuf>, Fail> {
    let fname = src.file_name().ok_or_else(|| Fail::Io(src.into(), invalid("nome non valido")))?;
    let meta = fs::symlink_metadata(src).at(src)?;
    if op == Op::Move && src.parent().and_then(|d| fs::canonicalize(d).ok()).as_deref() == Some(canon_dest) {
        p.add(1);
        return Ok(None);
    }
    if meta.is_dir() && fs::canonicalize(src).is_ok_and(|c| canon_dest.starts_with(c)) {
        return Err(Fail::Io(src.into(), invalid("la destinazione è dentro la cartella stessa")));
    }
    let target = unique_name(dest_dir, fname);
    p.set_current(src);
    if op == Op::Move && same_fs {
        match rename_noreplace(src, &target) {
            Ok(()) => {
                p.add(1);
                return Ok(Some(target));
            }
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                p.total.fetch_add(tree_size(src, p).saturating_sub(1), Relaxed);
            }
            Err(e) => return Err(Fail::Io(src.into(), e)),
        }
    }
    if let Err(f) = copy_tree(src, &target, p) {
        if matches!(f, Fail::Cancel) || op == Op::Move {
            let _ = remove_tree(&target, &Progress::default());
        }
        return Err(f);
    }
    if op == Op::Move {
        remove_tree(src, &Progress::default())?;
    }
    Ok(Some(target))
}

/// Copies or moves `sources` into `dest_dir`, never overwriting; returns
/// the created paths. An item that fails does not stop the others; the
/// first error is returned.
pub fn transfer(op: Op, sources: &[PathBuf], dest_dir: &Path, p: &Progress) -> Result<Vec<PathBuf>, String> {
    let verb = if op == Op::Copy { "copiare" } else { "spostare" };
    let fail = |e: io::Error| format!("impossibile {verb} in {}: {}", name(dest_dir), reason(&e));
    let dest_dev = fs::metadata(dest_dir).map_err(fail)?.dev();
    let canon_dest = fs::canonicalize(dest_dir).map_err(fail)?;
    let same_fs: Vec<bool> = sources.iter().map(|s| op == Op::Move && fs::symlink_metadata(s).is_ok_and(|m| m.dev() == dest_dev)).collect();
    let total = sources.iter().zip(&same_fs).map(|(s, &fast)| if fast { 1 } else { tree_size(s, p) }).sum();
    p.total.store(total, Relaxed);
    let mut created = Vec::new();
    let mut first_err = None;
    for (src, &fast) in sources.iter().zip(&same_fs) {
        if p.cancelled() {
            return Err(CANCELLED.into());
        }
        match transfer_one(op, src, dest_dir, &canon_dest, fast, p) {
            Ok(path) => created.extend(path),
            Err(Fail::Cancel) => return Err(CANCELLED.into()),
            Err(f) => {
                first_err.get_or_insert(message(verb, f));
            }
        }
    }
    first_err.map_or(Ok(created), Err)
}

/// Where the extension starts: the last dot, or before ".tar" in "x.tar.gz".
fn ext_start(b: &[u8]) -> usize {
    match b.iter().rposition(|c| *c == b'.') {
        Some(dot) if dot > 0 => {
            if dot > 4 && b[..dot].ends_with(b".tar") {
                dot - 4
            } else {
                dot
            }
        }
        _ => b.len(),
    }
}

/// "stem (n).ext", continuing an existing " (k)" suffix of the stem.
fn numbered(b: &[u8], split: usize, n: u64) -> OsString {
    let (stem, ext) = b.split_at(split);
    let mut s = stem.to_vec();
    s.extend_from_slice(format!(" ({n})").as_bytes());
    s.extend_from_slice(ext);
    OsString::from_vec(s)
}

/// The stem without a trailing " (k)" and the next number to try.
fn strip_number(stem: &[u8]) -> (&[u8], u64) {
    let parsed = (|| {
        let inner = stem.strip_suffix(b")")?;
        let open = inner.iter().rposition(|c| *c == b'(')?;
        let n: u64 = std::str::from_utf8(&inner[open + 1..]).ok()?.parse().ok()?;
        let base = inner[..open].strip_suffix(b" ")?;
        (!base.is_empty()).then_some((base, n + 1))
    })();
    parsed.unwrap_or((stem, 2))
}

/// `dir/name` if free, else "stem (2).ext", "stem (3).ext"…
pub fn unique_name(dir: &Path, name: &OsStr) -> PathBuf {
    let first = dir.join(name);
    let Ok(m) = fs::symlink_metadata(&first) else { return first };
    let b = name.as_bytes();
    let split = if m.is_dir() { b.len() } else { ext_start(b) };
    let (stem, mut n) = strip_number(&b[..split]);
    let mut full = stem.to_vec();
    full.extend_from_slice(&b[split..]);
    loop {
        let path = dir.join(numbered(&full, stem.len(), n));
        if fs::symlink_metadata(&path).is_err() {
            return path;
        }
        n += 1;
    }
}

fn check_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(invalid("nome non valido"));
    }
    Ok(())
}

/// Renames within the same directory; fails if the target exists.
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    check_name(new_name)?;
    let target = path.parent().ok_or_else(|| invalid("nome non valido"))?.join(new_name);
    if path.file_name() != Some(OsStr::new(new_name)) {
        rename_noreplace(path, &target)?;
    }
    Ok(target)
}

pub fn mkdir(dir: &Path, name: &str) -> io::Result<PathBuf> {
    check_name(name)?;
    let path = dir.join(name);
    fs::create_dir(&path)?;
    Ok(path)
}

pub fn new_file(dir: &Path, name: &str) -> io::Result<PathBuf> {
    check_name(name)?;
    let path = dir.join(name);
    OpenOptions::new().write(true).create_new(true).open(&path)?;
    Ok(path)
}

/// Deletes permanently and recursively; paths already gone are skipped.
pub fn delete(paths: &[PathBuf], p: &Progress) -> Result<(), String> {
    let paths: Vec<&PathBuf> = paths.iter().filter(|x| fs::symlink_metadata(x).is_ok()).collect();
    p.total.store(paths.iter().map(|x| tree_count(x, p)).sum(), Relaxed);
    let mut first_err = None;
    for path in paths {
        p.set_current(path);
        match remove_tree(path, p) {
            Ok(()) => {}
            Err(Fail::Cancel) => return Err(CANCELLED.into()),
            Err(f) => {
                first_err.get_or_insert(message("eliminare", f));
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

fn home_trash() -> PathBuf {
    crate::data_home().join("Trash")
}

/// The home trash's `files` directory.
pub fn trash_dir() -> PathBuf {
    home_trash().join("files")
}

fn make_trash_dirs(trash: &Path) -> io::Result<()> {
    for d in ["files", "info"] {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(trash.join(d))?;
    }
    Ok(())
}

/// The mount point containing `path` (canonical), from /proc/self/mountinfo.
fn mount_point(path: &Path) -> Option<PathBuf> {
    let info = fs::read_to_string("/proc/self/mountinfo").ok()?;
    info.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|mp| PathBuf::from(OsString::from_vec(unescape_octal(mp))))
        .filter(|mp| path.starts_with(mp))
        .max_by_key(|mp| mp.as_os_str().len())
}

fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = b.get(i + 1..i + 4).filter(|o| o.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], oct) {
            (b'\\', Some(o)) => {
                out.push(o.iter().fold(0u8, |v, c| v.wrapping_mul(8).wrapping_add(c - b'0')));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `$topdir/.Trash-$uid`, created 0700 if missing and checked to be ours.
fn top_trash(top: &Path) -> io::Result<PathBuf> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let t = top.join(format!(".Trash-{uid}"));
    match fs::DirBuilder::new().mode(0o700).create(&t) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let m = fs::symlink_metadata(&t)?;
    if !m.is_dir() || m.uid() != uid {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "cestino non valido"));
    }
    make_trash_dirs(&t)?;
    Ok(t)
}

fn timestamp() -> String {
    let t = crate::local_time(crate::now());
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
}

/// Writes `info/NAME.trashinfo` exclusively for a NAME free in `files/`.
fn reserve(trash: &Path, fname: &OsStr, original: &Path) -> io::Result<(OsString, PathBuf)> {
    let content = format!("[Trash Info]\nPath={}\nDeletionDate={}\n", uri::escape(original.as_os_str().as_bytes()), timestamp());
    let b = fname.as_bytes();
    for n in 1u64.. {
        let cand = if n == 1 { fname.to_os_string() } else { numbered(b, ext_start(b), n) };
        if fs::symlink_metadata(trash.join("files").join(&cand)).is_ok() {
            continue;
        }
        let mut info_name = cand.clone();
        info_name.push(".trashinfo");
        let info = trash.join("info").join(info_name);
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&info) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(content.as_bytes()) {
                    let _ = fs::remove_file(&info);
                    return Err(e);
                }
                return Ok((cand, info));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn trash_one(path: &Path) -> io::Result<()> {
    let abs = std::path::absolute(path)?;
    let fname = abs.file_name().ok_or_else(|| invalid("nome non valido"))?;
    let dev = fs::symlink_metadata(&abs)?.dev();
    let home = home_trash();
    let home_dev = make_trash_dirs(&home).and_then(|()| fs::metadata(&home)).map(|m| m.dev());
    let (trash, original) = if home_dev.as_ref().is_ok_and(|d| *d == dev) {
        (home, abs.clone())
    } else {
        let real = fs::canonicalize(abs.parent().unwrap_or(Path::new("/")))?.join(fname);
        let top = mount_point(&real).ok_or_else(|| io::Error::other("punto di montaggio sconosciuto"))?;
        let t = top_trash(&top)?;
        let rel = real.strip_prefix(&top).map_or(real.clone(), Path::to_path_buf);
        (t, rel)
    };
    let (name, info) = reserve(&trash, fname, &original)?;
    rename_noreplace(&abs, &trash.join("files").join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&info);
    })
}

/// Moves to the freedesktop trash; stops at the first failure.
pub fn trash(paths: &[PathBuf], p: &Progress) -> Result<(), String> {
    p.total.store(paths.len() as u64, Relaxed);
    for path in paths {
        if p.cancelled() {
            return Err(CANCELLED.into());
        }
        p.set_current(path);
        trash_one(path).map_err(|e| format!("impossibile spostare {} nel cestino: {}", name(path), reason(&e)))?;
        p.add(1);
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Trashed {
    /// The name in the trash's `files` directory.
    pub name: OsString,
    pub original: PathBuf,
    /// "YYYY-MM-DDThh:mm:ss", local time.
    pub deleted: String,
}

fn parse_info(text: &str) -> Option<(PathBuf, String)> {
    let (mut section, mut path, mut date) = (false, None, String::new());
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line == "[Trash Info]";
        } else if !section {
            continue;
        } else if let Some(v) = line.strip_prefix("Path=") {
            path = Some(Path::new("/").join(OsString::from_vec(uri::unescape(v))));
        } else if let Some(v) = line.strip_prefix("DeletionDate=") {
            date = v.to_string();
        }
    }
    Some((path?, date))
}

/// The home trash's contents, most recently deleted first.
pub fn trashed() -> Vec<Trashed> {
    let t = home_trash();
    let mut out: Vec<Trashed> = fs::read_dir(t.join("info"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name();
            let name = OsStr::from_bytes(n.as_bytes().strip_suffix(b".trashinfo")?).to_os_string();
            fs::symlink_metadata(t.join("files").join(&name)).ok()?;
            let (original, deleted) = parse_info(&fs::read_to_string(e.path()).ok()?)?;
            Some(Trashed { name, original, deleted })
        })
        .collect();
    out.sort_by(|a, b| b.deleted.cmp(&a.deleted));
    out
}

/// Moves a home trash item back to where it came from (or a free name next
/// to it), recreating missing parent directories.
pub fn restore(name: &OsStr) -> Result<PathBuf, String> {
    let label = name.to_string_lossy();
    let fail = |e: io::Error| format!("impossibile ripristinare {label}: {}", reason(&e));
    if name.is_empty() || name == "." || name == ".." || name.as_bytes().contains(&b'/') {
        return Err(fail(invalid("nome non valido")));
    }
    let t = home_trash();
    let mut info_name = name.to_os_string();
    info_name.push(".trashinfo");
    let info = t.join("info").join(info_name);
    let (original, _) = parse_info(&fs::read_to_string(&info).map_err(fail)?).ok_or_else(|| fail(invalid("informazioni non valide")))?;
    let (Some(parent), Some(fname)) = (original.parent(), original.file_name()) else { return Err(fail(invalid("percorso non valido"))) };
    fs::create_dir_all(parent).map_err(fail)?;
    let target = unique_name(parent, fname);
    let src = t.join("files").join(name);
    match rename_noreplace(&src, &target) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let p = Progress::default();
            copy_tree(&src, &target, &p).and_then(|()| remove_tree(&src, &p)).map_err(|f| message("ripristinare", f))?;
        }
        Err(e) => return Err(fail(e)),
    }
    let _ = fs::remove_file(&info);
    Ok(target)
}

pub fn empty_trash(p: &Progress) -> Result<(), String> {
    let t = home_trash();
    let mut paths: Vec<PathBuf> = ["files", "info"].iter().flat_map(|d| fs::read_dir(t.join(d)).into_iter().flatten().flatten().map(|e| e.path())).collect();
    paths.push(t.join("directorysizes"));
    delete(&paths, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!("files-ops-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unique_names() {
        let d = tmp("unique");
        let u = |n: &str| unique_name(&d, OsStr::new(n)).file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(u("a.txt"), "a.txt");
        for n in ["a.txt", "a (2).txt", "archive.tar.gz", ".bashrc", "noext", "x (9)"] {
            fs::write(d.join(n), "").unwrap();
        }
        fs::create_dir(d.join("dir.d")).unwrap();
        assert_eq!(u("a.txt"), "a (3).txt");
        assert_eq!(u("a (2).txt"), "a (3).txt");
        assert_eq!(u("archive.tar.gz"), "archive (2).tar.gz");
        assert_eq!(u(".bashrc"), ".bashrc (2)");
        assert_eq!(u("noext"), "noext (2)");
        assert_eq!(u("dir.d"), "dir.d (2)");
        assert_eq!(u("x (9)"), "x (10)");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn names() {
        let d = tmp("names");
        let f = new_file(&d, "f.txt").unwrap();
        assert!(new_file(&d, "f.txt").is_err());
        assert!(mkdir(&d, "sub").unwrap().is_dir());
        assert!(mkdir(&d, "sub").is_err());
        for bad in ["", ".", "..", "a/b"] {
            assert!(rename(&f, bad).is_err());
            assert!(mkdir(&d, bad).is_err());
        }
        assert_eq!(rename(&f, "sub").unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let g = rename(&f, "g.txt").unwrap();
        assert!(g.exists() && !f.exists());
        assert_eq!(rename(&g, "g.txt").unwrap(), g);
        fs::remove_dir_all(&d).unwrap();
    }

    fn tree(d: &Path) {
        fs::create_dir_all(d.join("src/inner")).unwrap();
        fs::write(d.join("src/a.txt"), "hello").unwrap();
        fs::write(d.join("src/inner/big.bin"), vec![7u8; 300_000]).unwrap();
        fs::write(d.join("src/empty"), "").unwrap();
        std::os::unix::fs::symlink("a.txt", d.join("src/link")).unwrap();
        fs::set_permissions(d.join("src/a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        File::options().write(true).open(d.join("src/a.txt")).unwrap().set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)).unwrap();
    }

    #[test]
    fn copy_and_move() {
        let d = tmp("copy");
        tree(&d);
        fs::create_dir(d.join("dst")).unwrap();
        let p = Progress::default();
        let out = transfer(Op::Copy, &[d.join("src")], &d.join("dst"), &p).unwrap();
        assert_eq!(out, [d.join("dst/src")]);
        let total = p.total.load(Relaxed);
        assert_eq!(total, 300_000 + 5 + 1 + 1 + 2);
        assert_eq!(p.done.load(Relaxed), total);
        assert_eq!(fs::read(d.join("dst/src/inner/big.bin")).unwrap().len(), 300_000);
        let m = fs::metadata(d.join("dst/src/a.txt")).unwrap();
        assert_eq!((m.mode() & 0o777, m.mtime()), (0o640, 1_000_000));
        assert_eq!(fs::read_link(d.join("dst/src/link")).unwrap(), Path::new("a.txt"));
        // Same directory: a numbered copy.
        let out = transfer(Op::Copy, &[d.join("src/a.txt")], &d.join("src"), &Progress::default()).unwrap();
        assert_eq!(out, [d.join("src/a (2).txt")]);
        // Into itself.
        let e = transfer(Op::Copy, &[d.join("src")], &d.join("src/inner"), &Progress::default()).unwrap_err();
        assert!(e.starts_with("impossibile copiare src:"), "{e}");
        assert!(transfer(Op::Move, &[d.join("src/a.txt")], &d.join("src"), &Progress::default()).unwrap().is_empty());
        let out = transfer(Op::Move, &[d.join("src"), d.join("missing")], &d.join("dst"), &Progress::default());
        assert!(out.unwrap_err().starts_with("impossibile spostare missing:"));
        assert!(!d.join("src").exists() && d.join("dst/src (2)/inner/big.bin").exists());
        let p = Progress::default();
        p.cancel.store(true, Relaxed);
        assert_eq!(transfer(Op::Copy, &[d.join("dst")], &d, &p).unwrap_err(), "annullato");
        assert!(!d.join("dst (2)").exists());
        let p = Progress::default();
        delete(&[d.join("dst"), d.join("gone")], &p).unwrap();
        assert!(!d.join("dst").exists());
        assert_eq!(p.done.load(Relaxed), p.total.load(Relaxed));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn delete_keeps_symlink_targets() {
        let d = tmp("del");
        fs::create_dir_all(d.join("keep")).unwrap();
        fs::write(d.join("keep/x"), "x").unwrap();
        fs::create_dir(d.join("rm")).unwrap();
        std::os::unix::fs::symlink(d.join("keep"), d.join("rm/link")).unwrap();
        delete(&[d.join("rm")], &Progress::default()).unwrap();
        assert!(d.join("keep/x").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn trash_info_round_trip() {
        let d = tmp("trash");
        make_trash_dirs(&d).unwrap();
        let orig = Path::new("/home/u/a b.txt");
        let (n1, i1) = reserve(&d, OsStr::new("a b.txt"), orig).unwrap();
        let (n2, _) = reserve(&d, OsStr::new("a b.txt"), orig).unwrap();
        assert_eq!((n1.to_str().unwrap(), n2.to_str().unwrap()), ("a b.txt", "a b (2).txt"));
        let text = fs::read_to_string(&i1).unwrap();
        assert!(text.starts_with("[Trash Info]\nPath=/home/u/a%20b.txt\nDeletionDate=20"), "{text}");
        let (p, date) = parse_info(&text).unwrap();
        assert_eq!((p.as_path(), date.len()), (orig, 19));
        assert_eq!(unescape_octal(r"/mnt/a\040b"), b"/mnt/a b");
        assert!(mount_point(Path::new("/proc/self")).is_some_and(|m| m == Path::new("/proc")));
        fs::remove_dir_all(&d).unwrap();
    }

    /// Exercises the real trash with an isolated XDG_DATA_HOME on the same
    /// filesystem as the temp dir; env changes need a single-threaded run.
    #[test]
    #[ignore]
    fn trash_restore_empty() {
        let d = tmp("realtrash");
        std::env::set_var("XDG_DATA_HOME", d.join("data"));
        fs::write(d.join("f.txt"), "1").unwrap();
        trash(&[d.join("f.txt")], &Progress::default()).unwrap();
        assert!(!d.join("f.txt").exists());
        let items = trashed();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].original, d.join("f.txt"));
        fs::write(d.join("f.txt"), "2").unwrap();
        assert_eq!(restore(&items[0].name).unwrap(), d.join("f (2).txt"));
        trash(&[d.join("f.txt")], &Progress::default()).unwrap();
        empty_trash(&Progress::default()).unwrap();
        assert!(trashed().is_empty() && fs::read_dir(trash_dir()).unwrap().next().is_none());
        fs::remove_dir_all(&d).unwrap();
    }
}
