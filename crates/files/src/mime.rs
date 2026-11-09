//! MIME types from shared-mime-info globs (plus a few magic numbers), icons
//! per type family, and desktop applications with their associations
//! (Desktop Entry and MIME Applications Associations specs).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::dir::Kind;

const OCTET: &str = "application/octet-stream";

struct Glob {
    weight: u32,
    /// Lowercase unless `cs`.
    pattern: String,
    mime: String,
    cs: bool,
}

#[derive(Default)]
pub struct Db {
    literal: Vec<Glob>,
    /// "*.ext" globs by their last extension.
    suffix: HashMap<String, Vec<Glob>>,
    other: Vec<Glob>,
    subclasses: HashMap<String, Vec<String>>,
    aliases: HashMap<String, String>,
}

fn wild(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn last_ext(s: &str) -> Option<&str> {
    s.rfind('.').map(|i| &s[i + 1..])
}

/// Shell-style match of `*`, `?` and `[...]` (with `!`/`^` and ranges).
fn fnmatch(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| fnmatch(&p[1..], &s[i..])),
        Some('?') => !s.is_empty() && fnmatch(&p[1..], &s[1..]),
        Some('[') if p[1..].iter().skip(1).any(|c| *c == ']') => {
            let Some(&c) = s.first() else { return false };
            let mut i = 1;
            let negate = matches!(p.get(1), Some('!' | '^'));
            if negate {
                i += 1;
            }
            let mut hit = false;
            let mut first = true;
            while i < p.len() && (p[i] != ']' || first) {
                first = false;
                if p.get(i + 1) == Some(&'-') && p.get(i + 2).is_some_and(|e| *e != ']') {
                    hit |= (p[i]..=p[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= p[i] == c;
                    i += 1;
                }
            }
            hit != negate && i < p.len() && fnmatch(&p[i + 1..], &s[1..])
        }
        Some(c) => s.first() == Some(c) && fnmatch(&p[1..], &s[1..]),
    }
}

/// Keeps the highest weight, then the longest pattern, then a
/// case-sensitive one; the first on ties.
fn better(best: Option<&Glob>, g: &Glob, longest_first: bool) -> bool {
    let Some(b) = best else { return true };
    let (w, l) = (g.weight.cmp(&b.weight), g.pattern.len().cmp(&b.pattern.len()));
    let o = if longest_first { l.then(w) } else { w.then(l) };
    o.then(g.cs.cmp(&b.cs)).is_gt()
}

impl Db {
    pub fn load() -> Db {
        let mut db = Db::default();
        let mut dirs = vec![crate::data_home()];
        dirs.extend(crate::xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
        for d in dirs.iter().map(|d| d.join("mime")) {
            db.add_globs(&fs::read_to_string(d.join("globs2")).unwrap_or_default());
            for (file, aliases) in [("subclasses", false), ("aliases", true)] {
                for line in fs::read_to_string(d.join(file)).unwrap_or_default().lines() {
                    let Some((a, b)) = line.trim().split_once(' ') else { continue };
                    if aliases {
                        db.aliases.entry(a.into()).or_insert_with(|| b.into());
                    } else {
                        let v = db.subclasses.entry(a.into()).or_default();
                        if !v.iter().any(|x| x == b) {
                            v.push(b.into());
                        }
                    }
                }
            }
        }
        db
    }

    fn add_globs(&mut self, text: &str) {
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut f = line.split(':');
            let (Some(Ok(weight)), Some(mime), Some(pattern)) = (f.next().map(str::parse), f.next(), f.next()) else { continue };
            let cs = f.next().is_some_and(|flags| flags.split(',').any(|x| x == "cs"));
            let pattern = if cs { pattern.to_string() } else { pattern.to_lowercase() };
            let g = Glob { weight, mime: mime.into(), cs, pattern };
            match g.pattern.strip_prefix('*') {
                _ if !wild(&g.pattern) => self.literal.push(g),
                Some(rest) if rest.starts_with('.') && !wild(rest) => {
                    let key = last_ext(rest).unwrap_or("").to_string();
                    self.suffix.entry(key).or_default().push(g);
                }
                _ => self.other.push(g),
            }
        }
    }

    /// The canonical name of a possibly aliased type.
    pub fn unalias<'a>(&'a self, mime: &'a str) -> &'a str {
        self.aliases.get(mime).map_or(mime, String::as_str)
    }

    /// The type from the file name's globs alone.
    pub fn glob(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        let pick = |g: &Glob| if g.cs { name } else { lower.as_str() };
        let mut best: Option<&Glob> = None;
        for g in self.literal.iter().filter(|g| pick(g) == g.pattern) {
            if better(best, g, false) {
                best = Some(g);
            }
        }
        if best.is_none() {
            let mut keys = vec![last_ext(&lower)];
            if last_ext(name) != keys[0] {
                keys.push(last_ext(name));
            }
            let cands = keys.into_iter().flatten().filter_map(|k| self.suffix.get(k)).flatten();
            for g in cands.filter(|g| pick(g).ends_with(&g.pattern[1..])) {
                if better(best, g, true) {
                    best = Some(g);
                }
            }
        }
        if best.is_none() {
            for g in &self.other {
                let (p, s): (Vec<char>, Vec<char>) = (g.pattern.chars().collect(), pick(g).chars().collect());
                if fnmatch(&p, &s) && better(best, g, false) {
                    best = Some(g);
                }
            }
        }
        best.map(|g| g.mime.as_str())
    }

    pub fn guess(&self, path: &Path, kind: Kind) -> String {
        match kind {
            Kind::Dir => "inode/directory".into(),
            Kind::Other => special(path).into(),
            Kind::File => {
                let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                self.glob(&name).map_or_else(|| sniff(path).into(), str::to_string)
            }
        }
    }

    /// Supertypes breadth-first, aliases resolved; `text/*` implies
    /// text/plain, and everything but `inode/*` ends with
    /// application/octet-stream.
    pub fn parents(&self, mime: &str) -> Vec<String> {
        let start = self.unalias(mime).to_string();
        let mut out: Vec<String> = Vec::new();
        let mut queue = VecDeque::from([start.clone()]);
        while let Some(m) = queue.pop_front() {
            let mut next: Vec<&str> = self.subclasses.get(&m).into_iter().flatten().map(|p| self.unalias(p)).collect();
            if m.starts_with("text/") && m != "text/plain" {
                next.push("text/plain");
            }
            for p in next {
                if p != start && p != OCTET && !out.iter().any(|x| x == p) {
                    out.push(p.into());
                    queue.push_back(p.into());
                }
            }
        }
        if !start.starts_with("inode/") && start != OCTET {
            out.push(OCTET.into());
        }
        out
    }
}

fn special(path: &Path) -> &'static str {
    let Ok(ft) = fs::symlink_metadata(path).map(|m| m.file_type()) else { return OCTET };
    if ft.is_symlink() {
        "inode/symlink"
    } else if ft.is_fifo() {
        "inode/fifo"
    } else if ft.is_socket() {
        "inode/socket"
    } else if ft.is_char_device() {
        "inode/chardevice"
    } else if ft.is_block_device() {
        "inode/blockdevice"
    } else {
        OCTET
    }
}

const MAGIC: [(&[u8], &str); 8] = [
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"\xff\xd8\xff", "image/jpeg"),
    (b"GIF8", "image/gif"),
    (b"%PDF-", "application/pdf"),
    (b"\x7fELF", "application/x-executable"),
    (b"PK\x03\x04", "application/zip"),
    (b"\x1f\x8b", "application/gzip"),
    (b"#!", "application/x-shellscript"),
];

fn sniff(path: &Path) -> &'static str {
    let Ok(mut f) = File::options().read(true).custom_flags(libc::O_NONBLOCK).open(path) else { return OCTET };
    let mut buf = [0u8; 512];
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return OCTET,
        }
    }
    let b = &buf[..n];
    if b.is_empty() {
        return "application/x-zerosize";
    }
    if let Some((_, m)) = MAGIC.iter().find(|(magic, _)| b.starts_with(magic)) {
        if !m.ends_with("shellscript") || b.starts_with(b"#!/bin/sh") || b.starts_with(b"#!/bin/bash") || b.starts_with(b"#!/usr/bin/env bash") {
            return m;
        }
    }
    let text = match std::str::from_utf8(b) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && n == buf.len(),
    };
    if text && !b.contains(&0) {
        "text/plain"
    } else {
        OCTET
    }
}

pub const ICON_FOLDER: char = '\u{f07b}';
pub const ICON_FILE: char = '\u{f15b}';

/// A Nerd Font glyph for the type's family.
pub fn generic_icon(mime: &str) -> char {
    let (top, sub) = mime.split_once('/').unwrap_or((mime, ""));
    let has = |xs: &[&str]| xs.iter().any(|x| sub.contains(x));
    if mime == "inode/directory" || mime == "inode/mount-point" {
        ICON_FOLDER
    } else if mime == "application/pdf" {
        '\u{f1c1}'
    } else if has(&["spreadsheet", "ms-excel", "csv"]) {
        '\u{f1c3}'
    } else if has(&["presentation", "powerpoint"]) {
        '\u{f1c4}'
    } else if has(&["wordprocessing", "msword", "opendocument.text", "rtf", "abiword"]) {
        '\u{f1c2}'
    } else if has(&["cd-image", "iso9660", "disk-image", "diskimage"]) {
        '\u{f0a0}'
    } else if top == "font" || has(&["font-", "x-font"]) {
        '\u{f031}'
    } else if top == "image" {
        '\u{f1c5}'
    } else if top == "audio" {
        '\u{f1c7}'
    } else if top == "video" {
        '\u{f1c8}'
    } else if has(&["zip", "tar", "gzip", "bzip", "xz", "zstd", "7z", "rar", "lzma", "lzip", "compress", "archive", "cpio", "rpm", "debian-package"]) {
        '\u{f1c6}'
    } else if has(&["executable", "sharedlib", "msdownload", "appimage"]) {
        '\u{f489}'
    } else if has(&["json", "javascript", "xml", "html", "css", "shellscript", "x-python", "x-perl", "x-ruby", "toml", "yaml", "x-php", "sql", "x-desktop"])
        || (top == "text" && sub.starts_with("x-") && !has(&["markdown", "log", "readme", "authors", "copying", "install"]))
    {
        '\u{f1c9}'
    } else if top == "text" {
        '\u{f15c}'
    } else {
        ICON_FILE
    }
}

pub fn icon_for_name(_name: &str, kind: Kind, mime: &str) -> char {
    if kind == Kind::Dir {
        ICON_FOLDER
    } else {
        generic_icon(mime)
    }
}

#[derive(Clone, Debug)]
pub struct App {
    /// Desktop file ID, e.g. "foo.desktop".
    pub id: String,
    pub name: String,
    pub exec: String,
    pub terminal: bool,
    pub mimes: Vec<String>,
    pub path: PathBuf,
    pub no_display: bool,
}

#[derive(Default)]
pub struct Apps {
    apps: Vec<App>,
    index: HashMap<String, usize>,
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
    /// From mimeinfo.cache and MimeType= keys.
    supported: HashMap<String, Vec<String>>,
}

fn app_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::data_home()];
    dirs.extend(crate::xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// Calls `f(id, path)` for each .desktop file under `dir`.
fn walk(root: &Path, dir: &Path, depth: u32, f: &mut dyn FnMut(String, PathBuf)) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth < 8 {
                walk(root, &path, depth + 1, f);
            }
        } else if path.extension().is_some_and(|x| x == "desktop") {
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let Some(rel) = rel.to_str() else { continue };
            f(rel.replace('/', "-"), path);
        }
    }
}

/// The general escapes of desktop entry values: \s \n \t \r \\.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => {
                if o != '\\' {
                    out.push('\\');
                }
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// (section, key, value) of each entry in an INI-style file.
fn ini(text: &str) -> Vec<(&str, &str, &str)> {
    let mut section = "";
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
        } else if let Some((k, v)) = line.split_once('=').filter(|_| !line.starts_with('#')) {
            out.push((section, k.trim(), v.trim()));
        }
    }
    out
}

fn list(v: &str) -> impl Iterator<Item = String> + '_ {
    v.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn in_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).exists();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn parse_desktop(id: String, path: PathBuf) -> Option<App> {
    let text = fs::read_to_string(&path).ok()?;
    let mut kv = HashMap::new();
    for (section, k, v) in ini(&text) {
        if section == "Desktop Entry" {
            kv.entry(k).or_insert(v);
        }
    }
    if kv.get("Type") != Some(&"Application") || kv.get("Hidden") == Some(&"true") || kv.get("TryExec").is_some_and(|t| !in_path(&unescape(t))) {
        return None;
    }
    let name = kv.get("Name[it]").or(kv.get("Name")).map_or_else(|| id.trim_end_matches(".desktop").to_string(), |n| unescape(n));
    Some(App {
        name,
        exec: unescape(kv.get("Exec")?),
        terminal: kv.get("Terminal") == Some(&"true"),
        mimes: kv.get("MimeType").map(|m| list(m).collect()).unwrap_or_default(),
        no_display: kv.get("NoDisplay") == Some(&"true"),
        id,
        path,
    })
}

fn mimeapps_files() -> Vec<PathBuf> {
    let mut v = vec![crate::config_home().join("mimeapps.list")];
    v.extend(crate::xdg_dirs("XDG_CONFIG_DIRS", "/etc/xdg").into_iter().map(|d| d.join("mimeapps.list")));
    v.extend(app_dirs().into_iter().map(|d| d.join("mimeapps.list")));
    v
}

/// `text` with `id` first in `[Default Applications]` for `mime`.
fn rewrite_default(text: &str, mime: &str, id: &str) -> String {
    let key_of = |line: &str| line.split_once('=').map(|(k, _)| k.trim().to_string());
    let mut section = String::new();
    let mut has_key = false;
    let mut has_section = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.into();
            has_section |= line == "[Default Applications]";
        } else if section == "[Default Applications]" && key_of(line).as_deref() == Some(mime) {
            has_key = true;
        }
    }
    let mut out = String::new();
    let mut done = false;
    section.clear();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            section = t.into();
        }
        if !done && section == "[Default Applications]" {
            if !has_key && t == "[Default Applications]" {
                out.push_str(&format!("{line}\n{mime}={id};\n"));
                done = true;
                continue;
            }
            if let Some(v) = t.split_once('=').filter(|_| key_of(t).as_deref() == Some(mime)).map(|(_, v)| v) {
                let rest: String = list(v).filter(|x| x != id).map(|x| x + ";").collect();
                out.push_str(&format!("{mime}={id};{rest}\n"));
                done = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !has_section {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&format!("[Default Applications]\n{mime}={id};\n"));
    }
    out
}

impl Apps {
    pub fn load() -> Apps {
        let mut apps = Apps::default();
        let mut seen = HashSet::new();
        let dirs = app_dirs();
        for dir in &dirs {
            walk(dir, dir, 0, &mut |id, path| {
                if seen.insert(id.clone()) {
                    if let Some(app) = parse_desktop(id, path) {
                        apps.index.insert(app.id.clone(), apps.apps.len());
                        apps.apps.push(app);
                    }
                }
            });
        }
        for file in mimeapps_files() {
            let text = fs::read_to_string(file).unwrap_or_default();
            let entries = ini(&text);
            for &(_, mime, ids) in entries.iter().filter(|e| e.0 == "Removed Associations") {
                apps.removed.entry(mime.into()).or_default().extend(list(ids));
            }
            for &(section, mime, ids) in &entries {
                let target = match section {
                    "Default Applications" => &mut apps.defaults,
                    "Added Associations" => &mut apps.added,
                    _ => continue,
                };
                let removed = apps.removed.get(mime);
                let ids = list(ids).filter(|id| section == "Default Applications" || !removed.is_some_and(|r| r.contains(id)));
                target.entry(mime.into()).or_default().extend(ids);
            }
        }
        for dir in &dirs {
            let text = fs::read_to_string(dir.join("mimeinfo.cache")).unwrap_or_default();
            for (_, mime, ids) in ini(&text).into_iter().filter(|e| e.0 == "MIME Cache") {
                apps.supported.entry(mime.into()).or_default().extend(list(ids));
            }
        }
        for app in &apps.apps {
            for m in &app.mimes {
                apps.supported.entry(m.clone()).or_default().push(app.id.clone());
            }
        }
        apps
    }

    pub fn get(&self, id: &str) -> Option<&App> {
        self.index.get(id).map(|&i| &self.apps[i])
    }

    pub fn all(&self) -> &[App] {
        &self.apps
    }

    fn defaults(&self, mime: &str) -> impl Iterator<Item = &App> {
        self.defaults.get(mime).into_iter().flatten().filter_map(|id| self.get(id))
    }

    /// Added associations, then visible apps declaring the type.
    fn associated(&self, mime: &str) -> impl Iterator<Item = &App> {
        let added = self.added.get(mime).into_iter().flatten().filter_map(|id| self.get(id));
        let removed = self.removed.get(mime);
        let implicit = self.supported.get(mime).into_iter().flatten().filter(move |id| !removed.is_some_and(|r| r.contains(id))).filter_map(|id| self.get(id)).filter(|a| !a.no_display);
        added.chain(implicit)
    }

    fn chain(mime: &str, db: &Db) -> Vec<String> {
        std::iter::once(db.unalias(mime).to_string()).chain(db.parents(mime)).collect()
    }

    pub fn default_for(&self, mime: &str, db: &Db) -> Option<&App> {
        Self::chain(mime, db).iter().find_map(|m| self.defaults(m).next().or_else(|| self.associated(m).next()))
    }

    /// The default first, then the other apps for the type and its parents.
    pub fn all_for(&self, mime: &str, db: &Db) -> Vec<&App> {
        let chain = Self::chain(mime, db);
        let mut out: Vec<&App> = self.default_for(mime, db).into_iter().collect();
        for m in chain.iter().filter(|m| *m != OCTET) {
            out.extend(self.defaults(m).chain(self.associated(m)));
        }
        if out.is_empty() {
            out.extend(self.defaults(OCTET).chain(self.associated(OCTET)));
        }
        let mut seen = HashSet::new();
        out.retain(|a| seen.insert(a.id.as_str()));
        out
    }

    /// Makes `id` the default for `mime` in `$XDG_CONFIG_HOME/mimeapps.list`.
    pub fn set_default(&mut self, mime: &str, id: &str) -> io::Result<()> {
        let path = crate::config_home().join("mimeapps.list");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, rewrite_default(&text, mime, id))?;
        fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })?;
        let v = self.defaults.entry(mime.into()).or_default();
        v.retain(|x| x != id);
        v.insert(0, id.into());
        Ok(())
    }
}

/// Splits Exec into arguments: spaces separate, `"..."` quotes with
/// \" \` \$ \\ escapes.
fn tokenize(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let (mut in_arg, mut quoted) = (false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => match chars.next() {
                Some(n @ ('"' | '`' | '$' | '\\')) => cur.push(n),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            },
            ' ' | '\t' | '\n' if !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            _ => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

/// The field code letters in `exec`, "%%" excluded.
fn codes(exec: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('%') | None => {}
                Some(k) => out.push(k),
            }
        }
    }
    out
}

/// Whether one launch takes all files (%F, %U or no file code at all).
pub fn takes_many(app: &App) -> bool {
    let c = codes(&app.exec);
    c.iter().any(|k| matches!(k, 'F' | 'U')) || !c.iter().any(|k| matches!(k, 'f' | 'u'))
}

/// The argv to launch `app` on `files`, field codes expanded.
pub fn expand_exec(app: &App, files: &[PathBuf]) -> Vec<String> {
    let paths: Vec<String> = files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let uris: Vec<String> = files.iter().map(|p| crate::uri::to_uri(p)).collect();
    let mut out = Vec::new();
    for tok in tokenize(&app.exec) {
        match tok.as_str() {
            "%F" => out.extend(paths.iter().cloned()),
            "%U" => out.extend(uris.iter().cloned()),
            "%f" => out.extend(paths.first().cloned()),
            "%u" => out.extend(uris.first().cloned()),
            "%i" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            _ => {
                let mut s = String::new();
                let mut chars = tok.chars();
                while let Some(c) = chars.next() {
                    if c != '%' {
                        s.push(c);
                        continue;
                    }
                    match chars.next() {
                        Some('%') => s.push('%'),
                        Some('f' | 'F') => s.push_str(paths.first().map_or("", String::as_str)),
                        Some('u' | 'U') => s.push_str(uris.first().map_or("", String::as_str)),
                        Some('c') => s.push_str(&app.name),
                        Some('k') => s.push_str(&app.path.to_string_lossy()),
                        _ => {}
                    }
                }
                out.push(s);
            }
        }
    }
    if !files.is_empty() && !codes(&app.exec).iter().any(|k| matches!(k, 'f' | 'F' | 'u' | 'U')) {
        out.extend(paths);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let mut db = Db::default();
        db.add_globs(
            "# comment\n50:text/plain:*.txt\n50:application/gzip:*.gz\n50:application/x-compressed-tar:*.tar.gz\n\
             50:text/x-csrc:*.c\n50:text/x-c++src:*.C:cs\n50:text/x-makefile:makefile\n10:text/x-readme:README*\n\
             50:application/x-trash:*~\n40:text/x-log:*.[lL]og\n",
        );
        db.subclasses.insert("text/x-csrc".into(), vec!["text/x-generic".into()]);
        db.subclasses.insert("application/x-compressed-tar".into(), vec!["application/gzip".into()]);
        db.aliases.insert("application/x-gzip".into(), "application/gzip".into());
        db
    }

    #[test]
    fn globs() {
        let db = db();
        let g = |n: &str| db.glob(n).map(String::from);
        assert_eq!(g("a.TXT").as_deref(), Some("text/plain"));
        assert_eq!(g("x.tar.gz").as_deref(), Some("application/x-compressed-tar"));
        assert_eq!(g("x.gz").as_deref(), Some("application/gzip"));
        assert_eq!(g("main.c").as_deref(), Some("text/x-csrc"));
        assert_eq!(g("main.C").as_deref(), Some("text/x-c++src"));
        assert_eq!(g("Makefile").as_deref(), Some("text/x-makefile"));
        assert_eq!(g("README.md").as_deref(), Some("text/x-readme"));
        assert_eq!(g("notes~").as_deref(), Some("application/x-trash"));
        assert_eq!(g("x.log").as_deref(), Some("text/x-log"));
        assert_eq!(g("x.dat"), None);
        assert!(fnmatch(&['[', '!', 'a', ']', 'b'], &['c', 'b']));
        assert!(!fnmatch(&['[', 'a', '-', 'c', ']'], &['d']));
        assert!(fnmatch(&['[', 'x'], &['[', 'x']));
    }

    #[test]
    fn parents() {
        let db = db();
        assert_eq!(db.parents("text/x-csrc"), ["text/x-generic", "text/plain", OCTET]);
        assert_eq!(db.parents("application/x-gzip"), [OCTET]);
        assert_eq!(db.parents("application/x-compressed-tar"), ["application/gzip", OCTET]);
        assert!(db.parents("inode/directory").is_empty());
        assert!(db.parents(OCTET).is_empty());
    }

    #[test]
    fn guess_and_sniff() {
        let db = db();
        let d = std::env::temp_dir().join(format!("files-mime-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let cases: [(&str, &[u8], &str); 6] = [
            ("empty", b"", "application/x-zerosize"),
            ("pic", b"\x89PNG\r\n\x1a\nxxxx", "image/png"),
            ("notes", "città\n".as_bytes(), "text/plain"),
            ("bin", b"ab\0cd", OCTET),
            ("arc", b"PK\x03\x04", "application/zip"),
            ("a.txt", b"\0\0", "text/plain"),
        ];
        for (name, data, want) in cases {
            fs::write(d.join(name), data).unwrap();
            assert_eq!(db.guess(&d.join(name), Kind::File), want, "{name}");
        }
        assert_eq!(db.guess(&d, Kind::Dir), "inode/directory");
        std::os::unix::fs::symlink("nowhere", d.join("broken")).unwrap();
        assert_eq!(db.guess(&d.join("broken"), Kind::Other), "inode/symlink");
        assert_eq!(db.guess(&d.join("missing"), Kind::File), OCTET);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn icons() {
        assert_eq!(generic_icon("inode/directory"), ICON_FOLDER);
        assert_eq!(generic_icon("image/png"), '\u{f1c5}');
        assert_eq!(generic_icon("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"), '\u{f1c3}');
        assert_eq!(generic_icon("application/x-compressed-tar"), '\u{f1c6}');
        assert_eq!(generic_icon("text/x-csrc"), '\u{f1c9}');
        assert_eq!(generic_icon("text/plain"), '\u{f15c}');
        assert_eq!(generic_icon("application/x-executable"), '\u{f489}');
        assert_eq!(generic_icon("application/x-whatever"), ICON_FILE);
    }

    fn app(exec: &str) -> App {
        App { id: "t.desktop".into(), name: "Tool".into(), exec: exec.into(), terminal: false, mimes: vec![], path: "/apps/t.desktop".into(), no_display: false }
    }

    #[test]
    fn exec() {
        let files = [PathBuf::from("/a b"), PathBuf::from("/c")];
        assert_eq!(expand_exec(&app("foo %F --x"), &files), ["foo", "/a b", "/c", "--x"]);
        assert_eq!(expand_exec(&app("foo %u"), &files), ["foo", "file:///a%20b"]);
        assert_eq!(expand_exec(&app("foo %i %c %k 100%%"), &[]), ["foo", "Tool", "/apps/t.desktop", "100%"]);
        assert_eq!(expand_exec(&app(r#"sh -c "echo \"\$1\" \\ done" x %f"#), &files[1..]), ["sh", "-c", r#"echo "$1" \ done"#, "x", "/c"]);
        assert_eq!(expand_exec(&app("foo --file=%f"), &files[..1]), ["foo", "--file=/a b"]);
        assert_eq!(expand_exec(&app("foo"), &files), ["foo", "/a b", "/c"]);
        assert_eq!(expand_exec(&app("foo %f"), &[]), ["foo"]);
        assert!(takes_many(&app("foo %U")) && takes_many(&app("foo")) && !takes_many(&app("foo %f")));
        assert_eq!(unescape(r"a\sb\\c\;"), r"a b\c\;");
    }

    #[test]
    fn mimeapps_rewrite() {
        let text = "[Added Associations]\ntext/plain=a.desktop;\n\n[Default Applications]\ntext/plain=b.desktop;c.desktop;\nimage/png=x.desktop\n";
        let out = rewrite_default(text, "text/plain", "c.desktop");
        assert_eq!(out, "[Added Associations]\ntext/plain=a.desktop;\n\n[Default Applications]\ntext/plain=c.desktop;b.desktop;\nimage/png=x.desktop\n");
        let out = rewrite_default(text, "image/gif", "g.desktop");
        assert!(out.contains("[Default Applications]\nimage/gif=g.desktop;\ntext/plain=b.desktop;"));
        assert_eq!(rewrite_default("[Added Associations]\nx/y=a;\n", "x/y", "b"), "[Added Associations]\nx/y=a;\n\n[Default Applications]\nx/y=b;\n");
        assert_eq!(rewrite_default("", "x/y", "b"), "[Default Applications]\nx/y=b;\n");
    }

    #[test]
    fn associations() {
        let mut apps = Apps::default();
        for (i, (id, nd)) in [("a.desktop", false), ("b.desktop", false), ("hidden.desktop", true), ("bin.desktop", false)].into_iter().enumerate() {
            apps.apps.push(App { id: id.into(), no_display: nd, ..app("x") });
            apps.index.insert(id.into(), i);
        }
        apps.supported.insert("text/plain".into(), vec!["hidden.desktop".into(), "a.desktop".into(), "b.desktop".into()]);
        apps.supported.insert(OCTET.into(), vec!["bin.desktop".into()]);
        apps.removed.insert("text/plain".into(), vec!["a.desktop".into()]);
        apps.added.insert("text/x-csrc".into(), vec!["hidden.desktop".into(), "missing.desktop".into()]);
        let db = db();
        let ids = |v: Vec<&App>| v.iter().map(|a| a.id.clone()).collect::<Vec<_>>();
        assert_eq!(apps.default_for("text/x-csrc", &db).unwrap().id, "hidden.desktop");
        assert_eq!(ids(apps.all_for("text/x-csrc", &db)), ["hidden.desktop", "b.desktop"]);
        assert_eq!(apps.default_for("text/plain", &db).unwrap().id, "b.desktop");
        assert_eq!(ids(apps.all_for("image/png", &db)), ["bin.desktop"]);
        apps.defaults.insert("text/plain".into(), vec!["missing.desktop".into(), "a.desktop".into()]);
        assert_eq!(apps.default_for("text/x-csrc", &db).unwrap().id, "hidden.desktop");
        assert_eq!(apps.default_for("text/plain", &db).unwrap().id, "a.desktop");
    }

    #[test]
    #[ignore]
    fn this_machine() {
        let (db, apps) = (Db::load(), Apps::load());
        for m in ["text/plain", "image/png", "image/jpeg", "inode/directory", "application/pdf", "text/x-python", "video/mp4"] {
            let all: Vec<_> = apps.all_for(m, &db).iter().map(|a| a.id.clone()).collect();
            eprintln!("{m}: default {:?}, all {all:?}, parents {:?}", apps.default_for(m, &db).map(|a| &a.id), db.parents(m));
        }
        for f in ["/etc/passwd", "/usr/bin/ls", "x.tar.gz", "Cargo.toml", "/usr/share/pixmaps/filezilla.png", "main.rs", "photo.JPG"] {
            eprintln!("{f}: {}", db.guess(Path::new(f), Kind::File));
        }
    }
}
