//! The built-in file manager: more windows of the terminals' in-process
//! client, drawn by the compositor from their listing the way terminal
//! windows are drawn from their cells. Listing, file operations, MIME and
//! application lookups, thumbnails and volumes run on the worker; a window
//! keeps only the entries of the directory it shows.

mod draw;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use files::dir::{Entry, Sort};
use files::mime::{Apps, Db};
use files::ops::{Op, Progress};
use files::udisks::Volume;
use wire::*;

use super::{Open, Paste, PasteKind, Terms};
use crate::server::{SKey, State};

pub type Job = Box<dyn FnOnce() -> Result<crate::worker::Done, String> + Send>;

const COPY: u32 = 1;
const MOVE: u32 = 2;
/// Pointer travel that turns a press on an entry into a drag.
const DRAG_START: f64 = 8.0;
/// Icon sizes of the grid, and the one Ctrl+2 picks.
pub const ZOOM: [i32; 6] = [48, 64, 96, 128, 192, 256];
const ZOOM_GRID: u8 = 3;
/// Thumbnail textures kept; the ones drawn longest ago go first.
const THUMBS_KEPT: usize = 256;
/// How often a running file operation redraws its progress.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const ROOT_MEDIA: &str = "/run/media";
const DISKS: &str = "/dev/disk/by-uuid";

pub struct Item {
    pub entry: Entry,
    pub mime: String,
    pub icon: char,
}

/// What comes back from the worker.
pub enum Done {
    Listed { surface: u32, generation: u32, items: Result<Vec<Item>, String>, db: Arc<Db> },
    Launch { apps: Arc<Apps>, launches: Vec<Launch>, errors: Vec<String> },
    OpenWith { surface: u32, apps: Arc<Apps>, paths: Vec<PathBuf>, mime: String, choices: Vec<(String, String)> },
    Op { id: u32, result: Result<Vec<PathBuf>, String> },
    Thumbs(Vec<(ThumbKey, Option<files::png::Image>)>),
    Volumes(Vec<Volume>),
    Mounted { surface: u32, result: Result<PathBuf, String> },
    Unmounted(Result<String, String>),
}

pub struct Launch {
    pub argv: Vec<String>,
    pub terminal: bool,
    pub cwd: PathBuf,
}

/// A window opened for the file chooser portal.
pub struct Chooser {
    pub request: u32,
    pub save: bool,
    pub multiple: bool,
    pub directory: bool,
    /// Filter names and their globs, lowercase.
    pub filters: Vec<(String, Vec<String>)>,
    pub filter: usize,
    pub accept: String,
    name: Text,
}

impl Chooser {
    pub fn new(request: u32, save: bool, name: String) -> Chooser {
        let caret = name.len();
        Chooser { request, save, multiple: false, directory: false, filters: Vec::new(), filter: 0, accept: String::new(), name: Text { s: name, caret, mark: None } }
    }
}

/// A window someone asked for.
pub enum Request {
    /// A folder, with an entry under the cursor.
    Show(PathBuf, Option<OsString>),
    /// A file chooser in a folder, with a title.
    Choose(PathBuf, Box<Chooser>, Option<String>),
}

impl Chooser {
    pub fn from_portal(c: &crate::portal::ChooseFiles) -> Chooser {
        let mut ch = Chooser::new(c.request, c.save, c.name.clone());
        ch.multiple = c.multiple;
        ch.directory = c.directory;
        ch.filters = c.filters.clone();
        ch.filter = c.current.unwrap_or(0);
        ch.accept = c.accept.clone();
        ch
    }
}

/// An answer for the portal: the request, the files (None: cancelled) and
/// the filter in use.
pub type Chosen = (u32, Option<Vec<PathBuf>>, Option<usize>);

/// A line being edited.
#[derive(Clone, Default)]
struct Text {
    s: String,
    caret: usize,
    /// A range typing replaces (the name without its extension on rename).
    mark: Option<(usize, usize)>,
}

enum Prompt {
    Rename { from: OsString, text: Text },
    NewFolder(Text),
    NewFile(Text),
    Path(Text),
    Delete(Vec<PathBuf>),
    EmptyTrash,
    Overwrite(PathBuf),
}

#[derive(Clone)]
enum Act {
    Open,
    OpenWith,
    App(String),
    NewWindow,
    Terminal,
    Cut,
    Copy,
    Paste,
    CopyPath,
    Rename,
    Trash,
    Delete,
    Restore,
    EmptyTrash,
    NewFolder,
    NewFile,
    Hidden,
    Bookmark,
    Unbookmark(PathBuf),
    OpenPlace(PathBuf),
    Eject(usize),
}

struct Menu {
    x: i32,
    y: i32,
    items: Vec<(String, Act)>,
    hover: usize,
    /// "Open with": the files and their type, for Ctrl+Enter (make default).
    with: Option<(Vec<PathBuf>, String)>,
}

pub struct Window {
    pub surface: u32,
    xdg: u32,
    toplevel: u32,
    size: (i32, i32),
    pending: (i32, i32),
    dir: PathBuf,
    items: Vec<Item>,
    /// Indices into `items` after hidden files and the filter.
    shown: Vec<usize>,
    cursor: usize,
    /// First list row, or first row of tiles, on screen.
    top: usize,
    selected: HashSet<OsString>,
    anchor: usize,
    filter: String,
    hidden: bool,
    sort: Sort,
    reverse: bool,
    /// 0: list; else the icon size, `ZOOM[zoom - 1]`.
    zoom: u8,
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    prompt: Option<Prompt>,
    menu: Option<Menu>,
    status: Option<String>,
    /// Listing in flight (its generation) and the last one asked for.
    loading: u32,
    generation: u32,
    stale: bool,
    /// Entry to put the cursor on when the listing arrives.
    focus_name: Option<OsString>,
    pub chooser: Option<Chooser>,
}

/// Our own drag: its source and files.
struct Drag {
    source: u32,
    paths: Vec<PathBuf>,
    action: u32,
}

/// A drag over one of our windows.
struct Drop {
    offer: u32,
    surface: u32,
    serial: u32,
    pos: (f64, f64),
    /// Where it would land: a folder under the pointer, or the window's.
    target: Option<PathBuf>,
}

/// A file and the thumbnail size asked for.
pub type ThumbKey = (PathBuf, u32);

#[derive(Default)]
struct Thumbs {
    textures: HashMap<ThumbKey, (render::Texture, (u32, u32), u64)>,
    asked: HashSet<ThumbKey>,
    failed: HashSet<ThumbKey>,
    ready: Vec<(ThumbKey, files::png::Image)>,
    wanted: Vec<(ThumbKey, i64, String)>,
    frame: u64,
}

#[derive(Default)]
pub struct Files {
    pub windows: Vec<Window>,
    db: Option<Arc<Db>>,
    apps: Option<Arc<Apps>>,
    places: Vec<files::places::Place>,
    volumes: Vec<Volume>,
    /// Files cut or copied here while our source owns the clipboard.
    clip: Option<(u32, bool, Vec<PathBuf>)>,
    ops: Vec<(u32, String, Arc<Progress>, u32)>,
    next_op: u32,
    jobs: Vec<Job>,
    spawns: Vec<String>,
    drag: Option<Drag>,
    drop: Option<Drop>,
    /// A left press on an entry: where, for a drag.
    press: Option<(f64, f64)>,
    inotify: Option<OwnedFd>,
    watches: HashMap<i32, PathBuf>,
    thumbs: RefCell<Thumbs>,
    pub chosen: Vec<Chosen>,
}

impl Text {
    fn new(s: &str) -> Text {
        Text { s: s.to_string(), caret: s.len(), mark: None }
    }

    /// Applies an editing key; false if the key is not for the line.
    fn edit(&mut self, sym: u32, text: Option<char>, ctrl: bool) -> bool {
        let prev = |s: &str, i: usize| s[..i].char_indices().next_back().map_or(0, |(j, _)| j);
        let next = |s: &str, i: usize| s[i..].chars().next().map_or(i, |c| i + c.len_utf8());
        if let Some((a, b)) = self.mark.take() {
            match sym {
                0xff08 | 0xffff => {
                    self.s.replace_range(a..b, "");
                    self.caret = a;
                    return true;
                }
                0xff51 => {
                    self.caret = a;
                    return true;
                }
                0xff53 => {
                    self.caret = b;
                    return true;
                }
                _ => {
                    if let Some(c) = text.filter(|c| !c.is_control() && !ctrl) {
                        self.s.replace_range(a..b, "");
                        self.caret = a;
                        self.s.insert(self.caret, c);
                        self.caret += c.len_utf8();
                        return true;
                    }
                }
            }
        }
        match sym {
            0xff08 if ctrl => {
                let t = self.s[..self.caret].trim_end_matches(|c: char| !c.is_alphanumeric());
                let start = t.rfind(|c: char| !c.is_alphanumeric()).map_or(0, |i| i + 1);
                self.s.replace_range(start..self.caret, "");
                self.caret = start;
            }
            0xff08 if self.caret > 0 => {
                let p = prev(&self.s, self.caret);
                self.s.replace_range(p..self.caret, "");
                self.caret = p;
            }
            0xffff if self.caret < self.s.len() => {
                let n = next(&self.s, self.caret);
                self.s.replace_range(self.caret..n, "");
            }
            0xff51 => self.caret = prev(&self.s, self.caret),
            0xff53 => self.caret = next(&self.s, self.caret),
            0xff50 => self.caret = 0,
            0xff57 => self.caret = self.s.len(),
            0xff08 | 0xffff => {}
            _ => match text.filter(|c| !c.is_control() && !ctrl) {
                Some(c) => {
                    self.s.insert(self.caret, c);
                    self.caret += c.len_utf8();
                }
                None => return false,
            },
        }
        true
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

use super::shell_quote as quote;

fn is_trash(dir: &Path) -> bool {
    dir == files::ops::trash_dir()
}

/// Dirs first, then by the chosen key; ties by name.
fn sort_items(items: &mut [Item], by: Sort, reverse: bool) {
    let ext = |e: &Entry| Path::new(&e.name).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
    items.sort_by(|a, b| {
        let (x, y) = (&a.entry, &b.entry);
        let key = match by {
            Sort::Name => std::cmp::Ordering::Equal,
            Sort::Size => x.size.cmp(&y.size),
            Sort::Modified => x.mtime.cmp(&y.mtime),
            Sort::Type => ext(x).cmp(&ext(y)),
        };
        let key = key.then_with(|| files::dir::natural_cmp(&x.display(), &y.display()));
        y.is_dir().cmp(&x.is_dir()).then(if reverse { key.reverse() } else { key })
    });
}

/// Shell glob match, enough for portal filters (`*.png`, `*.tar.*`).
fn glob(pattern: &str, name: &str) -> bool {
    fn m(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| m(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && m(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && m(&p[1..], &n[1..]),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.to_lowercase().chars().collect());
    m(&p, &n)
}

impl Window {
    fn new(surface: u32, xdg: u32, toplevel: u32, dir: PathBuf) -> Window {
        Window {
            surface,
            xdg,
            toplevel,
            size: (0, 0),
            pending: (0, 0),
            dir,
            items: Vec::new(),
            shown: Vec::new(),
            cursor: 0,
            top: 0,
            selected: HashSet::new(),
            anchor: 0,
            filter: String::new(),
            hidden: false,
            sort: Sort::Name,
            reverse: false,
            zoom: 0,
            back: Vec::new(),
            forward: Vec::new(),
            prompt: None,
            menu: None,
            status: None,
            loading: 0,
            generation: 0,
            stale: true,
            focus_name: None,
            chooser: None,
        }
    }

    fn item(&self, shown: usize) -> Option<&Item> {
        self.shown.get(shown).and_then(|i| self.items.get(*i))
    }

    fn grid(&self) -> bool {
        self.zoom > 0
    }

    fn current(&self) -> Option<&Item> {
        self.item(self.cursor)
    }

    fn path_of(&self, it: &Item) -> PathBuf {
        self.dir.join(&it.entry.name)
    }

    /// The selected entries, else the one under the cursor.
    fn targets(&self) -> Vec<&Item> {
        let sel: Vec<&Item> = self.shown.iter().map(|i| &self.items[*i]).filter(|it| self.selected.contains(&it.entry.name)).collect();
        if sel.is_empty() {
            self.current().into_iter().collect()
        } else {
            sel
        }
    }

    fn target_paths(&self) -> Vec<PathBuf> {
        self.targets().into_iter().map(|it| self.path_of(it)).collect()
    }

    fn refilter(&mut self) {
        let name = self.current().map(|it| it.entry.name.clone());
        let needle = self.filter.to_lowercase();
        let chooser = self.chooser.as_ref();
        let globs = chooser.and_then(|c| c.filters.get(c.filter)).map(|(_, g)| g.as_slice()).filter(|g| !g.is_empty());
        let dirs_only = chooser.is_some_and(|c| c.directory);
        self.shown = (0..self.items.len())
            .filter(|i| {
                let e = &self.items[*i].entry;
                let shown = (self.hidden || !e.hidden) && (needle.is_empty() || e.display().to_lowercase().contains(&needle));
                let mime = &self.items[*i].mime;
                let matches = |p: &String| match p.strip_prefix("mime:") {
                    Some(m) => glob(m, mime),
                    None => glob(p, &e.display()),
                };
                let fits = e.is_dir() || (!dirs_only && globs.is_none_or(|g| g.iter().any(matches)));
                shown && fits
            })
            .collect();
        let want = self.focus_name.take().or(name);
        self.cursor = want.and_then(|n| self.shown.iter().position(|i| self.items[*i].entry.name == n)).unwrap_or(0);
        if !needle.is_empty() {
            self.cursor = 0;
        }
        self.anchor = self.cursor;
    }

    fn title(&self) -> String {
        if let Some(c) = &self.chooser {
            return if c.save { "Salva file".into() } else { "Apri file".into() };
        }
        let h = home();
        match self.dir.strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => self.dir.display().to_string(),
        }
    }
}

impl Terms {
    // ---- windows ---------------------------------------------------------

    pub fn is_files(&self, surface: u32) -> bool {
        self.fm.windows.iter().any(|w| w.surface == surface)
    }

    fn fm_index(&self, surface: u32) -> Option<usize> {
        self.fm.windows.iter().position(|w| w.surface == surface)
    }

    pub fn files_dir(&self, surface: u32) -> Option<PathBuf> {
        self.fm.windows.iter().find(|w| w.surface == surface).map(|w| w.dir.clone())
    }

    /// A file manager window on `dir`, with `select` under the cursor.
    pub fn open_files(&mut self, dir: PathBuf, select: Option<OsString>) {
        self.open_files_with(dir, select, None, None);
    }

    pub fn request(&mut self, r: Request) {
        match r {
            Request::Show(dir, select) => self.open_files(dir, select),
            Request::Choose(dir, c, title) => self.open_chooser(dir, *c, title),
        }
    }

    /// The app gave up on its file chooser.
    pub fn cancel_chooser(&mut self, s: &mut State, request: u32) {
        if let Some(i) = self.fm.windows.iter().position(|w| w.chooser.as_ref().is_some_and(|c| c.request == request)) {
            self.fm.windows[i].chooser = None;
            self.fm_close(s, i);
        }
    }

    pub fn open_chooser(&mut self, dir: PathBuf, chooser: Chooser, title: Option<String>) {
        self.open_files_with(dir, None, Some(chooser), title);
    }

    fn open_files_with(&mut self, dir: PathBuf, select: Option<OsString>, chooser: Option<Chooser>, title: Option<String>) {
        if self.compositor == 0 || self.wm_base == 0 {
            self.fm_queued.push(match chooser {
                Some(c) => Request::Choose(dir, Box::new(c), title),
                None => Request::Show(dir, select),
            });
            return;
        }
        let surface = self.new_id(&wl_surface::INTERFACE);
        self.send(Msg::new(self.compositor, wl_compositor::req::CREATE_SURFACE).obj(surface));
        let xdg = self.new_id(&xdg_surface::INTERFACE);
        self.send(Msg::new(self.wm_base, xdg_wm_base::req::GET_XDG_SURFACE).obj(xdg).obj(surface));
        let toplevel = self.new_id(&xdg_toplevel::INTERFACE);
        self.send(Msg::new(xdg, xdg_surface::req::GET_TOPLEVEL).obj(toplevel));
        self.send(Msg::new(toplevel, xdg_toplevel::req::SET_APP_ID).str(config::FILES_APP_ID));
        if chooser.is_some() {
            // Equal minimum and maximum: the window floats as a dialog.
            let (w, h) = config::FILES_CHOOSER_SIZE;
            self.send(Msg::new(toplevel, xdg_toplevel::req::SET_MIN_SIZE).int(w).int(h));
            self.send(Msg::new(toplevel, xdg_toplevel::req::SET_MAX_SIZE).int(w).int(h));
        }
        let mut w = Window::new(surface, xdg, toplevel, if dir.is_dir() { dir } else { home() });
        w.focus_name = select;
        w.chooser = chooser;
        let t = title.unwrap_or_else(|| w.title());
        self.send(Msg::new(toplevel, xdg_toplevel::req::SET_TITLE).str(&t));
        self.send(Msg::new(surface, wl_surface::req::COMMIT));
        if self.fm.windows.is_empty() {
            self.fm.places = files::places::places();
            self.refresh_volumes();
        }
        self.fm.windows.push(w);
        self.flush();
    }

    pub(super) fn fm_configure(&mut self, s: &mut State, i: usize, serial: u32) {
        let (cw, ch) = self.cell();
        let w = &mut self.fm.windows[i];
        let (mut width, mut height) = w.pending;
        if width <= 0 || height <= 0 {
            (width, height) = (100 * cw, 32 * ch);
        }
        w.size = (width, height);
        let (surface, xdg) = (w.surface, w.xdg);
        s.set_term_content(SKey { client: self.client, id: surface }, width, height);
        self.send(Msg::new(xdg, xdg_surface::req::ACK_CONFIGURE).uint(serial));
        self.send(Msg::new(surface, wl_surface::req::COMMIT));
        self.fm_scroll_to_cursor(i);
    }

    pub(super) fn fm_close(&mut self, s: &mut State, i: usize) {
        let w = self.fm.windows.remove(i);
        if let Some(c) = &w.chooser {
            self.fm.chosen.push((c.request, None, None));
        }
        self.send(Msg::new(w.toplevel, xdg_toplevel::req::DESTROY));
        self.send(Msg::new(w.xdg, xdg_surface::req::DESTROY));
        self.send(Msg::new(w.surface, wl_surface::req::DESTROY));
        self.flush();
        if s.focus.is_some_and(|f| f.client == self.client && f.id == w.surface) {
            self.repeat = None;
        }
        if self.fm.windows.is_empty() {
            // Nothing shown: let go of what only the windows need.
            self.fm.db = None;
            self.fm.apps = None;
            self.fm.watches.clear();
            self.fm.inotify = None;
            let mut th = self.fm.thumbs.borrow_mut();
            th.asked.clear();
            th.failed.clear();
            th.ready.clear();
            th.wanted.clear();
        }
    }

    fn fm_damage(&self, s: &mut State, i: usize) {
        let w = &self.fm.windows[i];
        s.damage_committed(SKey { client: self.client, id: w.surface }, &[wm::Rect::new(0, 0, w.size.0, w.size.1)]);
    }

    fn fm_damage_all(&self, s: &mut State) {
        for i in 0..self.fm.windows.len() {
            self.fm_damage(s, i);
        }
    }

    // ---- navigation ------------------------------------------------------

    fn go(&mut self, s: &mut State, i: usize, dir: PathBuf, record: bool) {
        let w = &mut self.fm.windows[i];
        if dir == w.dir {
            w.stale = true;
            return;
        }
        if record {
            let old = std::mem::replace(&mut w.dir, dir);
            w.back.push(old);
            w.forward.clear();
        } else {
            w.dir = dir;
        }
        w.items.clear();
        w.shown.clear();
        w.selected.clear();
        w.filter.clear();
        (w.cursor, w.top, w.anchor) = (0, 0, 0);
        w.stale = true;
        w.prompt = None;
        let (toplevel, title) = (w.toplevel, w.title());
        if w.chooser.is_none() {
            self.send(Msg::new(toplevel, xdg_toplevel::req::SET_TITLE).str(&title));
        }
        self.fm_damage(s, i);
    }

    fn parent(&mut self, s: &mut State, i: usize) {
        let w = &self.fm.windows[i];
        if let Some(p) = w.dir.parent().map(Path::to_path_buf) {
            let name = w.dir.file_name().map(OsStr::to_os_string);
            self.go(s, i, p, true);
            self.fm.windows[i].focus_name = name;
        }
    }

    fn history(&mut self, s: &mut State, i: usize, forward: bool) {
        let w = &mut self.fm.windows[i];
        let Some(to) = (if forward { w.forward.pop() } else { w.back.pop() }) else { return };
        let from = w.dir.clone();
        if forward {
            w.back.push(from.clone());
        } else {
            w.forward.push(from.clone());
        }
        self.go(s, i, to, false);
        self.fm.windows[i].focus_name = from.file_name().map(OsStr::to_os_string);
    }

    // ---- worker ----------------------------------------------------------

    /// Jobs for the worker: listings asked for, thumbnails wanted by the
    /// last frame, and whatever else was queued.
    pub fn take_jobs(&mut self) -> Vec<Job> {
        for i in 0..self.fm.windows.len() {
            if self.fm.windows[i].stale && self.fm.windows[i].loading == 0 {
                self.list(i);
            }
        }
        let wanted = std::mem::take(&mut self.fm.thumbs.borrow_mut().wanted);
        if !wanted.is_empty() {
            self.fm.jobs.push(Box::new(move || {
                let thumbs = wanted.into_iter().map(|((p, size), mtime, mime)| {
                    let t = files::thumb::get(&p, mtime, &mime, size);
                    ((p, size), t)
                });
                Ok(crate::worker::Done::Files(Done::Thumbs(thumbs.collect())))
            }));
        }
        std::mem::take(&mut self.fm.jobs)
    }

    pub fn take_spawns(&mut self) -> Vec<String> {
        std::mem::take(&mut self.fm.spawns)
    }

    pub fn take_chosen(&mut self) -> Vec<Chosen> {
        std::mem::take(&mut self.fm.chosen)
    }

    fn list(&mut self, i: usize) {
        self.watch_dirs();
        let w = &mut self.fm.windows[i];
        w.generation += 1;
        w.loading = w.generation;
        w.stale = false;
        let (surface, generation, dir, db) = (w.surface, w.generation, w.dir.clone(), self.fm.db.clone());
        self.fm.jobs.push(Box::new(move || {
            let db = db.unwrap_or_else(|| Arc::new(Db::load()));
            let items = files::dir::list(&dir)
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|entry| {
                            let mime = db.guess(&dir.join(&entry.name), entry.kind);
                            let icon = files::mime::icon_for_name(&entry.display(), entry.kind, &mime);
                            Item { entry, mime, icon }
                        })
                        .collect()
                })
                .map_err(|e| e.to_string());
            Ok(crate::worker::Done::Files(Done::Listed { surface, generation, items, db }))
        }));
    }

    fn refresh_volumes(&mut self) {
        self.fm.jobs.push(Box::new(|| files::udisks::volumes().map(|v| crate::worker::Done::Files(Done::Volumes(v)))));
    }

    pub fn files_done(&mut self, s: &mut State, done: Done) {
        match done {
            Done::Listed { surface, generation, items, db } => {
                self.fm.db.get_or_insert(db);
                let Some(i) = self.fm_index(surface) else { return };
                let w = &mut self.fm.windows[i];
                if w.loading != generation {
                    return;
                }
                w.loading = 0;
                match items {
                    Ok(mut items) => {
                        sort_items(&mut items, w.sort, w.reverse);
                        w.items = items;
                        let names: HashSet<&OsStr> = w.items.iter().map(|it| it.entry.name.as_os_str()).collect();
                        w.selected.retain(|n| names.contains(n.as_os_str()));
                    }
                    Err(e) => {
                        w.items.clear();
                        w.status = Some(format!("{}: {e}", w.dir.display()));
                    }
                }
                w.refilter();
                self.fm_scroll_to_cursor(i);
                self.fm_damage(s, i);
            }
            Done::Launch { apps, launches, errors } => {
                self.fm.apps = Some(apps);
                for l in launches {
                    let line = l.argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
                    if l.terminal {
                        self.open(Open { cwd: Some(l.cwd.to_string_lossy().into_owned()), cmd: Some(line), hold: false });
                    } else {
                        self.fm.spawns.push(format!("cd {} 2>/dev/null; exec {line}", quote(&l.cwd.to_string_lossy())));
                    }
                }
                if !errors.is_empty() {
                    self.fm_status_all(s, errors.join("; "));
                }
            }
            Done::OpenWith { surface, apps, paths, mime, choices } => {
                self.fm.apps = Some(apps);
                let Some(i) = self.fm_index(surface) else { return };
                if choices.is_empty() {
                    self.fm.windows[i].status = Some(format!("nessuna applicazione per {mime}"));
                } else {
                    let (x, y) = self.fm.windows[i].menu.as_ref().map_or((40, 40), |m| (m.x, m.y));
                    let items = choices.into_iter().map(|(id, name)| (name, Act::App(id))).collect();
                    self.fm.windows[i].menu = Some(Menu { x, y, items, hover: 0, with: Some((paths, mime)) });
                }
                self.fm_damage(s, i);
            }
            Done::Op { id, result } => {
                let Some(k) = self.fm.ops.iter().position(|o| o.0 == id) else { return };
                let (_, label, _, surface) = self.fm.ops.remove(k);
                match result {
                    Ok(created) => {
                        if let Some(i) = self.fm_index(surface) {
                            let w = &mut self.fm.windows[i];
                            let here: Vec<OsString> = created.iter().filter(|p| p.parent() == Some(w.dir.as_path())).filter_map(|p| p.file_name().map(OsStr::to_os_string)).collect();
                            if !here.is_empty() {
                                w.focus_name = here.first().cloned();
                                w.selected = here.into_iter().collect();
                            }
                        }
                    }
                    Err(e) => self.fm_status_all(s, format!("{label}: {e}")),
                }
                for w in &mut self.fm.windows {
                    w.stale = true;
                }
                self.fm_damage_all(s);
            }
            Done::Thumbs(list) => {
                let mut th = self.fm.thumbs.borrow_mut();
                for (p, img) in list {
                    th.asked.remove(&p);
                    match img {
                        Some(img) => th.ready.push((p, img)),
                        None => {
                            th.failed.insert(p);
                        }
                    }
                }
                drop(th);
                self.fm_damage_all(s);
            }
            Done::Volumes(v) => {
                self.fm.volumes = v;
                self.fm_damage_all(s);
            }
            Done::Mounted { surface, result } => {
                self.refresh_volumes();
                match (result, self.fm_index(surface)) {
                    (Ok(path), Some(i)) => self.go(s, i, path, true),
                    (Err(e), _) => self.fm_status_all(s, format!("montaggio non riuscito: {e}")),
                    _ => {}
                }
            }
            Done::Unmounted(r) => {
                self.refresh_volumes();
                match r {
                    Ok(name) => self.fm_status_all(s, format!("{name}: si può scollegare")),
                    Err(e) => self.fm_status_all(s, format!("smontaggio non riuscito: {e}")),
                }
            }
        }
    }

    fn fm_status_all(&mut self, s: &mut State, text: String) {
        for w in &mut self.fm.windows {
            w.status = Some(text.clone());
        }
        self.fm_damage_all(s);
    }

    // ---- watching --------------------------------------------------------

    /// Watches the directories on screen and the places volumes appear in.
    fn watch_dirs(&mut self) {
        if self.fm.inotify.is_none() {
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                return;
            }
            self.fm.inotify = Some(unsafe { OwnedFd::from_raw_fd(fd) });
            let media = PathBuf::from(ROOT_MEDIA).join(std::env::var("USER").unwrap_or_default());
            for p in [PathBuf::from(DISKS), media] {
                self.add_watch(&p);
            }
        }
        let wanted: HashSet<PathBuf> = self.fm.windows.iter().map(|w| w.dir.clone()).collect();
        let fd = self.fm.inotify.as_ref().map_or(-1, |f| f.as_raw_fd());
        let gone: Vec<i32> = self.fm.watches.iter().filter(|(_, p)| !wanted.contains(*p) && !Self::volume_path(p)).map(|(wd, _)| *wd).collect();
        for wd in gone {
            unsafe { libc::inotify_rm_watch(fd, wd) };
            self.fm.watches.remove(&wd);
        }
        for p in wanted {
            if !self.fm.watches.values().any(|w| *w == p) {
                self.add_watch(&p);
            }
        }
    }

    fn volume_path(p: &Path) -> bool {
        p == Path::new(DISKS) || p.starts_with(ROOT_MEDIA)
    }

    fn add_watch(&mut self, p: &Path) {
        let Some(fd) = &self.fm.inotify else { return };
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()) else { return };
        let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_ONLYDIR;
        let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) };
        if wd >= 0 {
            self.fm.watches.insert(wd, p.to_path_buf());
        }
    }

    pub fn inotify_ready(&mut self, s: &mut State) {
        let Some(fd) = self.fm.inotify.as_ref().map(|f| f.as_raw_fd()) else { return };
        let mut buf = [0u8; 8192];
        let mut changed: HashSet<i32> = HashSet::new();
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0;
            while off + 16 <= n as usize {
                let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                if mask & libc::IN_IGNORED != 0 {
                    self.fm.watches.remove(&wd);
                } else {
                    changed.insert(wd);
                }
                off += 16 + len;
            }
        }
        let mut volumes = false;
        for wd in changed {
            let Some(p) = self.fm.watches.get(&wd).cloned() else { continue };
            if Self::volume_path(&p) {
                volumes = true;
            }
            for w in &mut self.fm.windows {
                if w.dir == p {
                    w.stale = true;
                }
            }
        }
        // A directory that went away: show its closest existing parent.
        for i in 0..self.fm.windows.len() {
            if !self.fm.windows[i].dir.exists() {
                let mut p = self.fm.windows[i].dir.clone();
                while !p.is_dir() && p.pop() {}
                self.go(s, i, p, false);
            }
        }
        if volumes {
            self.refresh_volumes();
        }
    }

    // ---- actions ---------------------------------------------------------

    /// Opens entries: folders here (the others in new windows), files with
    /// their default application.
    fn open_items(&mut self, s: &mut State, i: usize, new_window: bool) {
        let w = &self.fm.windows[i];
        let targets: Vec<(PathBuf, String, bool)> = w.targets().into_iter().map(|it| (w.path_of(it), it.mime.clone(), it.entry.is_dir())).collect();
        if let Some(c) = &w.chooser {
            if c.save {
                if let Some((p, ..)) = targets.iter().find(|t| t.2) {
                    self.go(s, i, p.clone(), true);
                }
                return;
            }
            let files: Vec<PathBuf> = targets.iter().filter(|t| !t.2).map(|t| t.0.clone()).collect();
            match targets.first() {
                Some((p, _, true)) if files.is_empty() => self.go(s, i, p.clone(), true),
                Some(_) if !files.is_empty() && !c.directory => {
                    let files = if c.multiple { files } else { files.into_iter().take(1).collect() };
                    self.choose(s, i, files);
                }
                _ => {}
            }
            return;
        }
        let (dirs, files): (Vec<_>, Vec<_>) = targets.into_iter().partition(|t| t.2);
        for (n, (p, ..)) in dirs.into_iter().enumerate() {
            if n == 0 && !new_window {
                self.go(s, i, p, true);
            } else {
                self.open_files(p, None);
            }
        }
        if !files.is_empty() {
            self.launch(files.into_iter().map(|(p, m, _)| (p, m)).collect(), None);
        }
    }

    /// Runs files with `app`, or each with its default application.
    fn launch(&mut self, files: Vec<(PathBuf, String)>, app: Option<String>) {
        let (apps, db) = (self.fm.apps.clone(), self.fm.db.clone());
        self.fm.jobs.push(Box::new(move || {
            let apps = apps.unwrap_or_else(|| Arc::new(Apps::load()));
            let db = db.unwrap_or_else(|| Arc::new(Db::load()));
            let mut groups: Vec<(String, Vec<PathBuf>)> = Vec::new();
            let mut errors = Vec::new();
            for (p, mime) in files {
                let chosen = match &app {
                    Some(id) => apps.get(id),
                    None => apps.default_for(&mime, &db),
                };
                match chosen {
                    Some(a) => match groups.iter_mut().find(|g| g.0 == a.id) {
                        Some(g) => g.1.push(p),
                        None => groups.push((a.id.clone(), vec![p])),
                    },
                    None => errors.push(format!("nessuna applicazione per {mime}")),
                }
            }
            let mut launches = Vec::new();
            for (id, paths) in groups {
                let Some(a) = apps.get(&id) else { continue };
                let cwd = paths.first().and_then(|p| p.parent()).map_or_else(home, Path::to_path_buf);
                let batches: Vec<Vec<PathBuf>> = if files::mime::takes_many(a) { vec![paths] } else { paths.into_iter().map(|p| vec![p]).collect() };
                for b in batches {
                    launches.push(Launch { argv: files::mime::expand_exec(a, &b), terminal: a.terminal, cwd: cwd.clone() });
                }
            }
            Ok(crate::worker::Done::Files(Done::Launch { apps, launches, errors }))
        }));
    }

    fn open_with_menu(&mut self, i: usize) {
        let w = &self.fm.windows[i];
        let targets: Vec<&Item> = w.targets().into_iter().filter(|it| !it.entry.is_dir()).collect();
        let Some(first) = targets.first() else { return };
        let mime = first.mime.clone();
        let paths: Vec<PathBuf> = targets.iter().map(|it| w.path_of(it)).collect();
        let (surface, apps, db) = (w.surface, self.fm.apps.clone(), self.fm.db.clone());
        self.fm.jobs.push(Box::new(move || {
            let apps = apps.unwrap_or_else(|| Arc::new(Apps::load()));
            let db = db.unwrap_or_else(|| Arc::new(Db::load()));
            let choices = apps.all_for(&mime, &db).into_iter().map(|a| (a.id.clone(), a.name.clone())).collect();
            Ok(crate::worker::Done::Files(Done::OpenWith { surface, apps, paths, mime, choices }))
        }));
    }

    fn set_default(&mut self, mime: String, id: String) {
        let apps = self.fm.apps.clone();
        self.fm.jobs.push(Box::new(move || {
            let mut apps = apps.map_or_else(Apps::load, |a| Arc::try_unwrap(a).unwrap_or_else(|_| Apps::load()));
            apps.set_default(&mime, &id).map_err(|e| e.to_string())?;
            Ok(crate::worker::Done::Osd(format!("{mime}: {}", apps.get(&id).map_or(id.as_str(), |a| a.name.as_str()))))
        }));
        self.fm.apps = None;
    }

    fn start_op(&mut self, i: usize, label: String, job: impl FnOnce(&Progress) -> Result<Vec<PathBuf>, String> + Send + 'static) {
        self.fm.next_op += 1;
        let id = self.fm.next_op;
        let p = Arc::new(Progress::default());
        self.fm.ops.push((id, label, p.clone(), self.fm.windows[i].surface));
        self.fm.jobs.push(Box::new(move || Ok(crate::worker::Done::Files(Done::Op { id, result: job(&p) }))));
        self.arm();
    }

    fn transfer(&mut self, i: usize, op: Op, sources: Vec<PathBuf>, dest: PathBuf) {
        if sources.is_empty() {
            return;
        }
        let label = format!("{} di {} elementi", if op == Op::Copy { "Copia" } else { "Spostamento" }, sources.len());
        self.start_op(i, label, move |p| files::ops::transfer(op, &sources, &dest, p));
    }

    fn trash(&mut self, s: &mut State, i: usize, permanent: bool) {
        let paths = self.fm.windows[i].target_paths();
        if paths.is_empty() {
            return;
        }
        if permanent || is_trash(&self.fm.windows[i].dir) {
            self.fm.windows[i].prompt = Some(Prompt::Delete(paths));
            self.fm_damage(s, i);
            return;
        }
        let label = format!("Cestino ({} elementi)", paths.len());
        self.start_op(i, label, move |p| files::ops::trash(&paths, p).map(|_| Vec::new()));
    }

    fn restore(&mut self, i: usize) {
        let names: Vec<OsString> = self.fm.windows[i].targets().into_iter().map(|it| it.entry.name.clone()).collect();
        self.start_op(i, "Ripristino".into(), move |_| names.iter().map(|n| files::ops::restore(n)).collect());
    }

    /// Puts files on the clipboard, offered as files and as text.
    fn set_clipboard(&mut self, s: &mut State, i: usize, cut: bool) {
        let paths = self.fm.windows[i].target_paths();
        if paths.is_empty() || self.data_manager == 0 || self.data_device == 0 {
            return;
        }
        let source = self.new_id(&wl_data_source::INTERFACE);
        self.send(Msg::new(self.data_manager, wl_data_device_manager::req::CREATE_DATA_SOURCE).obj(source));
        for m in ["x-special/gnome-copied-files", "text/uri-list", "text/plain;charset=utf-8", "text/plain"] {
            self.send(Msg::new(source, wl_data_source::req::OFFER).str(m));
        }
        self.send(Msg::new(self.data_device, wl_data_device::req::SET_SELECTION).obj(source).uint(0));
        let n = paths.len();
        self.fm.clip = Some((source, cut, paths));
        self.fm.windows[i].status = Some(format!("{n} elementi {}", if cut { "tagliati" } else { "copiati" }));
        self.fm_damage(s, i);
    }

    fn paste_files(&mut self, s: &mut State, i: usize) {
        let dest = self.fm.windows[i].dir.clone();
        if let Some((_, cut, paths)) = self.fm.clip.clone() {
            self.transfer(i, if cut { Op::Move } else { Op::Copy }, paths, dest);
            if cut {
                self.fm.clip = None;
            }
            return;
        }
        let Some(offer) = self.clipboard else { return };
        let mimes = self.offers.get(&offer).cloned().unwrap_or_default();
        let Some(mime) = ["x-special/gnome-copied-files", "text/uri-list"].into_iter().find(|m| mimes.iter().any(|x| x == m)) else {
            self.fm.windows[i].status = Some("negli appunti non ci sono file".into());
            self.fm_damage(s, i);
            return;
        };
        let surface = self.fm.windows[i].surface;
        self.receive(offer, mime, surface, PasteKind::Files(mime.to_string()));
    }

    /// Reads an offer through a pipe; `paste_ready` gets the data.
    pub(super) fn receive(&mut self, offer: u32, mime: &str, surface: u32, kind: PasteKind) {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return;
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        self.send(Msg::new(offer, wl_data_offer::req::RECEIVE).str(mime).fd(write));
        self.flush();
        self.pastes.push(Paste { fd: read, surface, data: Vec::new(), kind });
    }

    /// Files read from the clipboard or a drop.
    pub(super) fn pasted_files(&mut self, surface: u32, kind: PasteKind, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let Some(i) = self.fm_index(surface) else { return };
        match kind {
            PasteKind::Files(mime) => {
                let (cut, paths) = if mime == "x-special/gnome-copied-files" {
                    files::uri::parse_gnome_copied_files(&text).unwrap_or((false, Vec::new()))
                } else {
                    (false, files::uri::parse_uri_list(&text))
                };
                let dest = self.fm.windows[i].dir.clone();
                self.transfer(i, if cut { Op::Move } else { Op::Copy }, paths, dest);
            }
            PasteKind::Drop { dest, action, offer } => {
                self.transfer(i, if action == MOVE { Op::Move } else { Op::Copy }, files::uri::parse_uri_list(&text), dest);
                self.send(Msg::new(offer, wl_data_offer::req::FINISH));
                self.send(Msg::new(offer, wl_data_offer::req::DESTROY));
                self.flush();
            }
            _ => {}
        }
    }

    /// A data source of ours is asked for its data.
    pub(super) fn source_send(&mut self, source: u32, mime: &str, fd: OwnedFd) {
        let data = match (&self.fm.clip, &self.fm.drag) {
            (Some((id, cut, paths)), _) if *id == source => match mime {
                "x-special/gnome-copied-files" => files::uri::gnome_copied_files(*cut, paths),
                "text/uri-list" => files::uri::uri_list(paths),
                _ => paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n"),
            },
            (_, Some(d)) if d.source == source => files::uri::uri_list(&d.paths),
            _ => return,
        };
        std::thread::spawn(move || {
            let mut f = std::fs::File::from(fd);
            let _ = io::Write::write_all(&mut f, data.as_bytes());
        });
    }

    /// Our source was replaced, or its drag ended.
    pub(super) fn source_done(&mut self, s: &mut State, source: u32, op: u16) {
        if self.fm.clip.as_ref().is_some_and(|c| c.0 == source) && op == wl_data_source::evt::CANCELLED {
            self.fm.clip = None;
        } else if let Some(d) = self.fm.drag.as_mut().filter(|d| d.source == source) {
            match op {
                wl_data_source::evt::ACTION => return,
                wl_data_source::evt::DND_DROP_PERFORMED => return,
                _ => {}
            }
            let moved = d.action == MOVE;
            self.fm.drag = None;
            if moved {
                for w in &mut self.fm.windows {
                    w.stale = true;
                }
            }
            self.fm_damage_all(s);
        } else {
            return;
        }
        self.send(Msg::new(source, wl_data_source::req::DESTROY));
    }

    pub(super) fn source_action(&mut self, source: u32, action: u32) {
        if let Some(d) = self.fm.drag.as_mut().filter(|d| d.source == source) {
            d.action = action;
        }
    }

    fn start_drag(&mut self, i: usize) {
        if self.data_manager == 0 || self.data_device == 0 || self.fm.drag.is_some() {
            return;
        }
        let paths = self.fm.windows[i].target_paths();
        if paths.is_empty() {
            return;
        }
        let source = self.new_id(&wl_data_source::INTERFACE);
        self.send(Msg::new(self.data_manager, wl_data_device_manager::req::CREATE_DATA_SOURCE).obj(source));
        for m in ["text/uri-list", "text/plain;charset=utf-8"] {
            self.send(Msg::new(source, wl_data_source::req::OFFER).str(m));
        }
        self.send(Msg::new(source, wl_data_source::req::SET_ACTIONS).uint(COPY | MOVE));
        let surface = self.fm.windows[i].surface;
        self.send(Msg::new(self.data_device, wl_data_device::req::START_DRAG).obj(source).obj(surface).obj(0).uint(0));
        self.flush();
        self.fm.drag = Some(Drag { source, paths, action: 0 });
        self.mouse.buttons = 0;
        self.fm.press = None;
    }

    // ---- drops -----------------------------------------------------------

    pub(super) fn drag_enter(&mut self, s: &mut State, serial: u32, surface: u32, pos: (f64, f64), offer: u32) {
        self.fm.drop = Some(Drop { offer, surface, serial, pos, target: None });
        self.drag_motion(s, pos);
    }

    pub(super) fn drag_motion(&mut self, s: &mut State, pos: (f64, f64)) {
        let Some(d) = self.fm.drop.as_mut() else { return };
        d.pos = pos;
        let (offer, surface, serial) = (d.offer, d.surface, d.serial);
        let mimes = self.offers.get(&offer).cloned().unwrap_or_default();
        let uris = mimes.iter().any(|m| m == "text/uri-list");
        let target = match self.fm_index(surface) {
            Some(i) => {
                let w = &self.fm.windows[i];
                let hit = match self.fm_hit(i, pos.0, pos.1) {
                    Hit::Item(k) => w.item(k).filter(|it| it.entry.is_dir()).map(|it| w.path_of(it)),
                    _ => None,
                };
                w.chooser.is_none().then(|| hit.unwrap_or_else(|| w.dir.clone()))
            }
            None => None,
        };
        let terminal = self.windows.iter().any(|w| w.surface == surface);
        let accept = uris && (target.is_some() || terminal);
        self.send(Msg::new(offer, wl_data_offer::req::ACCEPT).uint(serial).opt_str(accept.then_some("text/uri-list")));
        if accept {
            let action = self.drop_action(target.as_deref(), s.keyboard.bind_mods());
            self.send(Msg::new(offer, wl_data_offer::req::SET_ACTIONS).uint(COPY | MOVE).uint(action));
        }
        self.flush();
        let changed = self.fm.drop.as_ref().is_some_and(|d| d.target != target);
        if let Some(d) = self.fm.drop.as_mut() {
            d.target = target;
        }
        if changed {
            if let Some(i) = self.fm_index(surface) {
                self.fm_damage(s, i);
            }
        }
    }

    /// Ctrl copies, Shift moves; otherwise our own files move within a
    /// filesystem and everything else is copied.
    fn drop_action(&self, dest: Option<&Path>, m: u32) -> u32 {
        if m & config::mods::CTRL != 0 {
            return COPY;
        }
        if m & config::mods::SHIFT != 0 {
            return MOVE;
        }
        let dev = |p: &Path| std::fs::metadata(p).ok().map(|m| std::os::unix::fs::MetadataExt::dev(&m));
        match (&self.fm.drag, dest) {
            (Some(d), Some(dest)) if d.paths.first().and_then(|p| dev(p)) == dev(dest) && dev(dest).is_some() => MOVE,
            _ => COPY,
        }
    }

    pub(super) fn drag_leave(&mut self, s: &mut State) {
        if let Some(d) = self.fm.drop.take() {
            self.offers.remove(&d.offer);
            self.send(Msg::new(d.offer, wl_data_offer::req::DESTROY));
            self.flush();
            if let Some(i) = self.fm_index(d.surface) {
                self.fm_damage(s, i);
            }
        }
    }

    pub(super) fn drag_drop(&mut self, s: &mut State) {
        let Some(d) = self.fm.drop.take() else { return };
        let terminal = self.windows.iter().any(|w| w.surface == d.surface);
        if terminal {
            self.receive(d.offer, "text/uri-list", d.surface, PasteKind::Uris(d.offer));
            return;
        }
        let Some(dest) = d.target.clone() else { return };
        let action = self.drop_action(Some(&dest), s.keyboard.bind_mods());
        let own = self.fm.drag.as_ref().map(|g| g.paths.clone());
        match (own, self.fm_index(d.surface)) {
            (Some(paths), Some(i)) => {
                if !paths.iter().any(|p| p.parent() == Some(dest.as_path()) && action == MOVE) {
                    self.transfer(i, if action == MOVE { Op::Move } else { Op::Copy }, paths, dest);
                }
                self.send(Msg::new(d.offer, wl_data_offer::req::FINISH));
                self.send(Msg::new(d.offer, wl_data_offer::req::DESTROY));
            }
            (None, Some(_)) => self.receive(d.offer, "text/uri-list", d.surface, PasteKind::Drop { dest, action, offer: d.offer }),
            _ => {}
        }
        if let Some(i) = self.fm_index(d.surface) {
            self.fm_damage(s, i);
        }
        self.flush();
    }

    // ---- chooser ---------------------------------------------------------

    fn choose(&mut self, s: &mut State, i: usize, paths: Vec<PathBuf>) {
        let filter = self.fm.windows[i].chooser.as_ref().map(|c| c.filter);
        if let Some(c) = self.fm.windows[i].chooser.take() {
            self.fm.chosen.push((c.request, Some(paths), filter.filter(|_| !c.filters.is_empty())));
        }
        self.fm_close(s, i);
    }

    fn save_here(&mut self, s: &mut State, i: usize, confirmed: bool) {
        let w = &self.fm.windows[i];
        let Some(c) = &w.chooser else { return };
        let name = c.name.s.trim();
        if name.is_empty() || name.contains('/') {
            return;
        }
        let path = w.dir.join(name);
        if path.is_dir() {
            self.go(s, i, path, true);
            if let Some(c) = self.fm.windows[i].chooser.as_mut() {
                c.name = Text::default();
            }
            return;
        }
        if path.exists() && !confirmed {
            self.fm.windows[i].prompt = Some(Prompt::Overwrite(path));
            self.fm_damage(s, i);
            return;
        }
        self.choose(s, i, vec![path]);
    }

    // ---- keyboard --------------------------------------------------------

    pub(super) fn fm_key(&mut self, s: &mut State, surface: u32, code: u32) {
        let Some(i) = self.fm_index(surface) else { return };
        let sym = s.keyboard.sym(code);
        let text = s.keyboard.text(code);
        let m = s.keyboard.bind_mods();
        let (shift, ctrl, alt) = (m & config::mods::SHIFT != 0, m & config::mods::CTRL != 0, m & config::mods::ALT != 0);
        let base = s.keyboard.base_sym(code).unwrap_or(sym);
        self.fm.windows[i].status = None;
        if self.fm.windows[i].menu.is_some() {
            self.menu_key(s, i, sym, ctrl);
        } else if self.fm.windows[i].prompt.is_some() {
            self.prompt_key(s, i, sym, text, ctrl);
        } else {
            self.browse_key(s, i, (sym, base, text), (shift, ctrl, alt));
        }
        if let Some(i) = self.fm_index(surface) {
            self.fm_scroll_to_cursor(i);
            self.fm_damage(s, i);
        }
        self.flush();
    }

    fn browse_key(&mut self, s: &mut State, i: usize, (sym, base, text): (u32, u32, Option<char>), (shift, ctrl, alt): (bool, bool, bool)) {
        let saving = self.fm.windows[i].chooser.as_ref().is_some_and(|c| c.save);
        let page = self.fm_layout(i).page();
        let cols = self.fm_layout(i).cols.max(1);
        let w = &mut self.fm.windows[i];
        let n = w.shown.len();
        let step = |w: &mut Window, to: isize| {
            if n == 0 {
                return;
            }
            let to = to.clamp(0, n as isize - 1) as usize;
            w.cursor = to;
            if shift {
                let (a, b) = (w.anchor.min(to), w.anchor.max(to));
                w.selected = (a..=b).filter_map(|k| w.item(k).map(|it| it.entry.name.clone())).collect();
            } else if !ctrl {
                w.anchor = to;
            }
        };
        let c = w.cursor as isize;
        let grid = w.grid();
        match (sym, ctrl, alt) {
            (0xff52 | 0xff97, _, true) => self.parent(s, i),
            (0xff51 | 0xff96, _, true) => self.history(s, i, false),
            (0xff53 | 0xff98, _, true) => self.history(s, i, true),
            (0xff52 | 0xff97, _, _) => step(w, c - if grid { cols as isize } else { 1 }),
            (0xff54 | 0xff99, _, _) => step(w, c + if grid { cols as isize } else { 1 }),
            (0xff51 | 0xff96, _, _) if grid => step(w, c - 1),
            (0xff53 | 0xff98, _, _) if grid => step(w, c + 1),
            (0xff55 | 0xff9a, _, _) => step(w, c - page as isize),
            (0xff56 | 0xff9b, _, _) => step(w, c + page as isize),
            (0xff50 | 0xff95, _, _) if !saving => step(w, 0),
            (0xff57 | 0xff9c, _, _) if !saving => step(w, n as isize - 1),
            (0xff0d | 0xff8d, true, _) if w.chooser.as_ref().is_some_and(|c| c.directory) => {
                let pick = w.current().filter(|it| it.entry.is_dir() && !w.selected.is_empty()).map(|it| w.path_of(it)).unwrap_or_else(|| w.dir.clone());
                self.choose(s, i, vec![pick])
            }
            (0xff0d | 0xff8d, _, _) if saving => {
                if w.current().is_some_and(|it| it.entry.is_dir()) && w.chooser.as_ref().is_some_and(|c| c.name.s.is_empty()) {
                    return self.open_items(s, i, false);
                }
                self.save_here(s, i, false)
            }
            (0xff0d | 0xff8d, _, _) => self.open_items(s, i, shift),
            (0xff1b, ..) => {
                if !w.filter.is_empty() {
                    w.filter.clear();
                    w.refilter();
                } else if w.chooser.is_some() {
                    self.fm_close(s, i)
                } else if !w.selected.is_empty() {
                    w.selected.clear();
                } else {
                    for o in &self.fm.ops {
                        o.2.cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            (0xff08, false, false) if saving => {
                if let Some(c) = w.chooser.as_mut() {
                    c.name.edit(sym, text, false);
                }
            }
            (0xff08, ..) => {
                if w.filter.pop().is_some() {
                    w.refilter();
                } else {
                    self.parent(s, i)
                }
            }
            (0xffff, ..) if !saving => self.trash(s, i, shift),
            (0xffbf, ..) if !saving => self.begin_rename(i),
            (0xffc1, ..) => {
                let cwd = Some(self.fm.windows[i].dir.to_string_lossy().into_owned());
                self.open(Open { cwd, ..Default::default() })
            }
            (0xffc2, ..) => {
                w.stale = true;
                self.fm.places = files::places::places();
                self.fm.apps = None;
                self.refresh_volumes();
            }
            (0xff67, ..) => {
                let (x, y) = self.fm_cursor_pos(i);
                self.context_menu(i, x, y, Hit::Item(self.fm.windows[i].cursor));
            }
            (0x20, false, false) if !saving && w.filter.is_empty() => {
                if let Some(name) = w.current().map(|it| it.entry.name.clone()) {
                    if !w.selected.remove(&name) {
                        w.selected.insert(name);
                    }
                    step(w, c + 1);
                }
            }
            _ if ctrl => match (base, shift) {
                (0x61, _) => w.selected = w.shown.iter().map(|k| w.items[*k].entry.name.clone()).collect(),
                (0x63, false) if !saving => self.set_clipboard(s, i, false),
                (0x63, true) => {
                    let text = w.target_paths().iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
                    crate::server::selection::set_server(s, crate::server::clipboard::Clip { mime: "text/plain;charset=utf-8".into(), data: text.into_bytes() });
                    w.status = Some("percorso copiato".into());
                }
                (0x78, _) if !saving => self.set_clipboard(s, i, true),
                (0x76, _) if !saving => self.paste_files(s, i),
                (0x68, _) => {
                    w.hidden = !w.hidden;
                    w.refilter();
                }
                (0x6c, _) => w.prompt = Some(Prompt::Path(Text::new(&format!("{}/", w.dir.display()).replace("//", "/")))),
                (0x6e, true) if !saving => w.prompt = Some(Prompt::NewFolder(Text::new("Nuova cartella"))),
                (0x6e, false) if w.chooser.is_none() => {
                    let dir = w.dir.clone();
                    self.open_files(dir, None);
                }
                (0x77, _) => self.fm_close(s, i),
                (0x64, _) => self.bookmark(i, true),
                (0x31 | 0x32 | 0x2b | 0x3d | 0xffab | 0x2d | 0xffad, _) => {
                    w.zoom = match base {
                        0x31 => 0,
                        0x32 => ZOOM_GRID,
                        0x2d | 0xffad => w.zoom.saturating_sub(1),
                        _ => (w.zoom + 1).min(ZOOM.len() as u8),
                    };
                    w.top = 0;
                }
                (0x72, _) if is_trash(&w.dir) => self.restore(i),
                (0x6d, _) if alt || shift => w.prompt = Some(Prompt::NewFile(Text::new("Nuovo file"))),
                _ => {}
            },
            _ if alt => {
                let sort = match base {
                    0x31 => Some(Sort::Name),
                    0x32 => Some(Sort::Size),
                    0x33 => Some(Sort::Modified),
                    0x34 => Some(Sort::Type),
                    _ => None,
                };
                if let Some(by) = sort {
                    w.reverse = w.sort == by && !w.reverse;
                    w.sort = by;
                    let reverse = w.reverse;
                    sort_items(&mut w.items, by, reverse);
                    w.refilter();
                }
            }
            _ if saving => {
                if let Some(c) = w.chooser.as_mut() {
                    c.name.edit(sym, text, false);
                }
            }
            _ => {
                if let Some(ch) = text.filter(|c| !c.is_control()) {
                    w.filter.push(ch);
                    w.refilter();
                }
            }
        }
    }

    fn begin_rename(&mut self, i: usize) {
        let w = &mut self.fm.windows[i];
        let Some(it) = w.current() else { return };
        let name = it.entry.display().into_owned();
        let stem = if it.entry.is_dir() || name.starts_with('.') && name.matches('.').count() == 1 {
            name.len()
        } else {
            name.find(".tar.").or_else(|| name.rfind('.')).filter(|k| *k > 0).unwrap_or(name.len())
        };
        let from = it.entry.name.clone();
        w.prompt = Some(Prompt::Rename { from, text: Text { s: name, caret: stem, mark: Some((0, stem)) } });
    }

    fn prompt_key(&mut self, s: &mut State, i: usize, sym: u32, text: Option<char>, ctrl: bool) {
        let w = &mut self.fm.windows[i];
        let Some(p) = w.prompt.as_mut() else { return };
        let enter = matches!(sym, 0xff0d | 0xff8d);
        let yes = enter || matches!(text, Some('s' | 'S' | 'y' | 'Y'));
        if sym == 0xff1b {
            w.prompt = None;
            return;
        }
        match p {
            Prompt::Rename { text: t, .. } | Prompt::NewFolder(t) | Prompt::NewFile(t) | Prompt::Path(t) if !enter => {
                if sym == 0xff09 {
                    if let Prompt::Path(t) = p {
                        complete(t);
                    }
                } else {
                    t.edit(sym, text, ctrl);
                }
                return;
            }
            Prompt::Delete(_) | Prompt::EmptyTrash | Prompt::Overwrite(_) if !yes => {
                if matches!(text, Some('n' | 'N')) {
                    w.prompt = None;
                }
                return;
            }
            _ => {}
        }
        let dir = w.dir.clone();
        let prompt = w.prompt.take();
        let result: Result<Option<PathBuf>, String> = match prompt {
            Some(Prompt::Rename { from, text }) => files::ops::rename(&dir.join(&from), text.s.trim()).map(Some).map_err(|e| e.to_string()),
            Some(Prompt::NewFolder(t)) => files::ops::mkdir(&dir, t.s.trim()).map(Some).map_err(|e| e.to_string()),
            Some(Prompt::NewFile(t)) => files::ops::new_file(&dir, t.s.trim()).map(Some).map_err(|e| e.to_string()),
            Some(Prompt::Path(t)) => {
                let raw = t.s.trim();
                let p = match raw.strip_prefix('~') {
                    Some(rest) => home().join(rest.trim_start_matches('/')),
                    None => dir.join(raw),
                };
                if p.is_dir() {
                    self.go(s, i, p, true);
                } else if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
                    let name = name.to_os_string();
                    self.go(s, i, parent.to_path_buf(), true);
                    self.fm.windows[i].focus_name = Some(name);
                }
                return;
            }
            Some(Prompt::Delete(paths)) => {
                let label = format!("Eliminazione di {} elementi", paths.len());
                self.start_op(i, label, move |p| files::ops::delete(&paths, p).map(|_| Vec::new()));
                return;
            }
            Some(Prompt::EmptyTrash) => {
                self.start_op(i, "Svuotamento del cestino".into(), |p| files::ops::empty_trash(p).map(|_| Vec::new()));
                return;
            }
            Some(Prompt::Overwrite(_)) => return self.save_here(s, i, true),
            None => return,
        };
        let w = &mut self.fm.windows[i];
        match result {
            Ok(Some(p)) => {
                w.focus_name = p.file_name().map(OsStr::to_os_string);
                w.stale = true;
            }
            Ok(None) => {}
            Err(e) => w.status = Some(e),
        }
    }

    fn bookmark(&mut self, i: usize, add: bool) {
        let dir = self.fm.windows[i].dir.clone();
        let r = bookmarks_edit(&dir, add);
        self.fm.places = files::places::places();
        self.fm.windows[i].status = Some(match r {
            Ok(()) if add => format!("segnalibro: {}", dir.display()),
            Ok(()) => "segnalibro rimosso".into(),
            Err(e) => e.to_string(),
        });
    }

    // ---- menus -----------------------------------------------------------

    fn context_menu(&mut self, i: usize, x: i32, y: i32, hit: Hit) {
        let side = match hit {
            Hit::Side(k) => self.side_entries().get(k).map(|e| e.target.clone()),
            _ => None,
        };
        let w = &mut self.fm.windows[i];
        let mut items: Vec<(String, Act)> = Vec::new();
        let mut add = |label: &str, act: Act| items.push((label.to_string(), act));
        match hit {
            Hit::Side(_) => match side {
                Some(SideTarget::Path(p)) => {
                    add("Apri in una nuova finestra", Act::OpenPlace(p.clone()));
                    if bookmarked(&p) {
                        add("Rimuovi segnalibro", Act::Unbookmark(p));
                    }
                }
                Some(SideTarget::Volume(v)) => add("Scollega", Act::Eject(v)),
                None => return,
            },
            Hit::Item(k) => {
                if let Some(name) = w.item(k).map(|it| it.entry.name.clone()) {
                    if !w.selected.contains(&name) {
                        w.selected.clear();
                    }
                    w.cursor = k;
                }
                let trash = is_trash(&w.dir);
                let file = w.current().is_some_and(|it| !it.entry.is_dir());
                if trash {
                    add("Ripristina", Act::Restore);
                    add("Elimina definitivamente", Act::Delete);
                    add("Svuota il cestino", Act::EmptyTrash);
                } else {
                    add("Apri", Act::Open);
                    if file {
                        add("Apri con…", Act::OpenWith);
                    } else {
                        add("Apri in una nuova finestra", Act::NewWindow);
                    }
                    add("Taglia", Act::Cut);
                    add("Copia", Act::Copy);
                    add("Incolla", Act::Paste);
                    add("Copia il percorso", Act::CopyPath);
                    add("Rinomina", Act::Rename);
                    add("Sposta nel cestino", Act::Trash);
                    add("Elimina definitivamente", Act::Delete);
                }
            }
            _ => {
                if is_trash(&w.dir) {
                    add("Svuota il cestino", Act::EmptyTrash);
                } else {
                    add("Incolla", Act::Paste);
                    add("Nuova cartella", Act::NewFolder);
                    add("Nuovo file", Act::NewFile);
                    add("Terminale qui", Act::Terminal);
                    add("Aggiungi ai segnalibri", Act::Bookmark);
                }
                add(if w.hidden { "Nascondi i file nascosti" } else { "Mostra i file nascosti" }, Act::Hidden);
            }
        }
        if w.chooser.is_some() {
            items.retain(|(_, a)| matches!(a, Act::Open | Act::Hidden | Act::NewFolder | Act::CopyPath));
        }
        if !items.is_empty() {
            w.menu = Some(Menu { x, y, items, hover: 0, with: None });
        }
    }

    fn menu_key(&mut self, s: &mut State, i: usize, sym: u32, ctrl: bool) {
        let Some(m) = self.fm.windows[i].menu.as_mut() else { return };
        match sym {
            0xff52 | 0xff97 => m.hover = (m.hover + m.items.len() - 1) % m.items.len(),
            0xff54 | 0xff99 | 0xff09 => m.hover = (m.hover + 1) % m.items.len(),
            0xff0d | 0xff8d | 0x20 => {
                let k = m.hover;
                self.menu_pick(s, i, k, ctrl);
            }
            _ => self.fm.windows[i].menu = None,
        }
    }

    fn menu_pick(&mut self, s: &mut State, i: usize, k: usize, make_default: bool) {
        let Some(menu) = self.fm.windows[i].menu.take() else { return };
        let Some((_, act)) = menu.items.get(k).cloned() else { return };
        match act {
            Act::Open => self.open_items(s, i, false),
            Act::NewWindow => self.open_items(s, i, true),
            Act::OpenWith => {
                self.fm.windows[i].menu = Some(Menu { items: Vec::new(), ..menu });
                self.open_with_menu(i);
                self.fm.windows[i].menu = None;
            }
            Act::App(id) => {
                if let Some((paths, mime)) = menu.with {
                    if make_default {
                        self.set_default(mime.clone(), id.clone());
                    }
                    self.launch(paths.into_iter().map(|p| (p, mime.clone())).collect(), Some(id));
                }
            }
            Act::Terminal => {
                let cwd = Some(self.fm.windows[i].dir.to_string_lossy().into_owned());
                self.open(Open { cwd, ..Default::default() });
            }
            Act::Cut => self.set_clipboard(s, i, true),
            Act::Copy => self.set_clipboard(s, i, false),
            Act::Paste => self.paste_files(s, i),
            Act::CopyPath => {
                let text = self.fm.windows[i].target_paths().iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
                crate::server::selection::set_server(s, crate::server::clipboard::Clip { mime: "text/plain;charset=utf-8".into(), data: text.into_bytes() });
            }
            Act::Rename => self.begin_rename(i),
            Act::Trash => self.trash(s, i, false),
            Act::Delete => self.trash(s, i, true),
            Act::Restore => self.restore(i),
            Act::EmptyTrash => self.fm.windows[i].prompt = Some(Prompt::EmptyTrash),
            Act::NewFolder => self.fm.windows[i].prompt = Some(Prompt::NewFolder(Text::new("Nuova cartella"))),
            Act::NewFile => self.fm.windows[i].prompt = Some(Prompt::NewFile(Text::new("Nuovo file"))),
            Act::Hidden => {
                let w = &mut self.fm.windows[i];
                w.hidden = !w.hidden;
                w.refilter();
            }
            Act::Bookmark => self.bookmark(i, true),
            Act::Unbookmark(p) => {
                let _ = bookmarks_edit(&p, false);
                self.fm.places = files::places::places();
            }
            Act::OpenPlace(p) => self.open_files(p, None),
            Act::Eject(v) => self.eject(v),
        }
        if let Some(i) = self.fm_index(self.fm.windows.get(i).map_or(0, |w| w.surface)) {
            self.fm_damage(s, i);
        }
    }

    fn eject(&mut self, v: usize) {
        let Some(vol) = self.fm.volumes.get(v).cloned() else { return };
        self.fm.jobs.push(Box::new(move || {
            let r = if vol.removable && vol.can_power_off { files::udisks::power_off(&vol.drive) } else { files::udisks::unmount(&vol.object) };
            Ok(crate::worker::Done::Files(Done::Unmounted(r.map(|_| vol.label.clone()))))
        }));
    }

    // ---- pointer ---------------------------------------------------------

    pub(super) fn fm_button(&mut self, s: &mut State, i: usize, button: u32, pressed: bool) {
        let (x, y) = self.mouse.pos;
        let surface = self.fm.windows[i].surface;
        if !pressed {
            if button == 0x110 {
                self.fm.press = None;
            }
            return;
        }
        self.fm.windows[i].status = None;
        let hit = self.fm_hit(i, x, y);
        if let Some(m) = &self.fm.windows[i].menu {
            match hit {
                Hit::Menu(k) if button == 0x110 => {
                    let _ = m;
                    let ctrl = s.keyboard.bind_mods() & config::mods::CTRL != 0;
                    self.menu_pick(s, i, k, ctrl);
                }
                _ => self.fm.windows[i].menu = None,
            }
            self.fm_damage(s, i);
            self.flush();
            return;
        }
        let now = Instant::now();
        let quick = self.mouse.clicks.1.is_some_and(|t| now.duration_since(t) < Duration::from_millis(400));
        let double = button == 0x110 && quick && self.mouse.clicks.0 == 1 && self.fm.windows[i].cursor_hit(&hit);
        if button == 0x110 {
            self.mouse.clicks = (if double { 2 } else { 1 }, Some(now));
        }
        let m = s.keyboard.bind_mods();
        let (shift, ctrl) = (m & config::mods::SHIFT != 0, m & config::mods::CTRL != 0);
        match (button, hit) {
            (0x113, _) => self.history(s, i, false),
            (0x114, _) => self.history(s, i, true),
            (0x110, Hit::Item(k)) => {
                let w = &mut self.fm.windows[i];
                let name = w.item(k).map(|it| it.entry.name.clone());
                if double {
                    w.cursor = k;
                    return self.after_pointer(s, surface, |t, s, i| t.open_items(s, i, false));
                }
                if let Some(name) = name {
                    if ctrl {
                        if !w.selected.remove(&name) {
                            w.selected.insert(name);
                        }
                    } else if shift {
                        let (a, b) = (w.anchor.min(k), w.anchor.max(k));
                        w.selected = (a..=b).filter_map(|j| w.item(j).map(|it| it.entry.name.clone())).collect();
                    } else if !w.selected.contains(&name) {
                        w.selected.clear();
                        w.anchor = k;
                    }
                    w.cursor = k;
                    if let Some(c) = w.chooser.as_mut().filter(|c| c.save) {
                        if let Some(it) = w.items.get(w.shown[k]).filter(|it| !it.entry.is_dir()) {
                            c.name = Text::new(&it.entry.display());
                        }
                    }
                    self.fm.press = Some((x, y));
                }
            }
            (0x112, Hit::Item(k)) => {
                if let Some(p) = self.fm.windows[i].item(k).filter(|it| it.entry.is_dir()).map(|it| self.fm.windows[i].path_of(it)) {
                    self.open_files(p, None);
                }
            }
            (0x111, hit @ (Hit::Item(_) | Hit::Empty | Hit::Side(_))) => self.context_menu(i, x as i32, y as i32, hit),
            (0x110, Hit::Empty) => {
                let w = &mut self.fm.windows[i];
                w.selected.clear();
            }
            (0x110, Hit::Side(k)) => match self.side_entries().get(k).map(|e| e.target.clone()) {
                Some(SideTarget::Path(p)) => self.go(s, i, p, true),
                Some(SideTarget::Volume(v)) => {
                    let Some(vol) = self.fm.volumes.get(v).cloned() else { return };
                    match vol.mount {
                        Some(p) => self.go(s, i, p, true),
                        None => self.fm.jobs.push(Box::new(move || {
                            let result = files::udisks::mount(&vol.object);
                            Ok(crate::worker::Done::Files(Done::Mounted { surface, result }))
                        })),
                    }
                }
                None => {}
            },
            (0x110, Hit::Eject(v)) => self.eject(v),
            (0x110, Hit::Crumb(p)) => self.go(s, i, p, true),
            (0x110, Hit::Header(by)) => {
                let w = &mut self.fm.windows[i];
                w.reverse = w.sort == by && !w.reverse;
                w.sort = by;
                let reverse = w.reverse;
                sort_items(&mut w.items, by, reverse);
                w.refilter();
            }
            (0x110, Hit::Filter(k)) => {
                if let Some(c) = self.fm.windows[i].chooser.as_mut() {
                    c.filter = k;
                }
                self.fm.windows[i].refilter();
            }
            (0x110, Hit::Button(b)) => match b {
                0 => {
                    let w = &self.fm.windows[i];
                    if w.chooser.as_ref().is_some_and(|c| c.save) {
                        self.save_here(s, i, false);
                    } else if w.chooser.as_ref().is_some_and(|c| c.directory) {
                        let dir = w.dir.clone();
                        self.choose(s, i, vec![dir]);
                    } else {
                        self.open_items(s, i, false);
                    }
                }
                _ => return self.after_pointer(s, surface, |t, s, i| t.fm_close(s, i)),
            },
            _ => {}
        }
        self.after_pointer(s, surface, |_, _, _| {});
    }

    fn after_pointer(&mut self, s: &mut State, surface: u32, f: impl FnOnce(&mut Terms, &mut State, usize)) {
        if let Some(i) = self.fm_index(surface) {
            f(self, s, i);
        }
        if let Some(i) = self.fm_index(surface) {
            self.fm_scroll_to_cursor(i);
            self.fm_damage(s, i);
        }
        self.flush();
    }

    pub(super) fn fm_motion(&mut self, s: &mut State, i: usize) {
        let (x, y) = self.mouse.pos;
        if let (Some((px, py)), true) = (self.fm.press, self.mouse.buttons & 1 != 0) {
            if (x - px).hypot(y - py) > DRAG_START && self.fm.windows[i].chooser.is_none() {
                return self.start_drag(i);
            }
        }
        if self.fm.windows[i].menu.is_some() {
            if let Hit::Menu(k) = self.fm_hit(i, x, y) {
                if let Some(m) = self.fm.windows[i].menu.as_mut().filter(|m| m.hover != k) {
                    m.hover = k;
                    self.fm_damage(s, i);
                }
            }
        }
    }

    pub(super) fn fm_wheel(&mut self, s: &mut State, i: usize, steps: i32) {
        if s.keyboard.bind_mods() & config::mods::CTRL != 0 {
            let w = &mut self.fm.windows[i];
            w.zoom = (w.zoom as i32 - steps.signum()).clamp(0, ZOOM.len() as i32) as u8;
            w.top = 0;
            self.fm_scroll_to_cursor(i);
            self.fm_damage(s, i);
            return;
        }
        let page = self.fm_layout(i).page();
        let w = &mut self.fm.windows[i];
        let rows = if w.grid() { w.shown.len().div_ceil(self.fm_layout(i).cols.max(1)) } else { w.shown.len() };
        let w = &mut self.fm.windows[i];
        let lines = if w.grid() { steps } else { steps * config::TERM_SCROLL_LINES };
        let max = rows.saturating_sub(page) as isize;
        w.top = (w.top as isize + lines as isize).clamp(0, max.max(0)) as usize;
        self.fm_damage(s, i);
    }

    fn fm_scroll_to_cursor(&mut self, i: usize) {
        let l = self.fm_layout(i);
        let w = &mut self.fm.windows[i];
        let rows = l.page().max(1);
        let row = if w.grid() { w.cursor / l.cols.max(1) } else { w.cursor };
        if row < w.top {
            w.top = row;
        } else if row >= w.top + rows {
            w.top = row + 1 - rows;
        }
    }

    pub(super) fn fm_progress(&mut self, s: &mut State) {
        for i in 0..self.fm.windows.len() {
            self.fm_damage(s, i);
        }
    }
}

impl Window {
    fn cursor_hit(&self, hit: &Hit) -> bool {
        matches!(hit, Hit::Item(k) if *k == self.cursor)
    }
}

/// Completes the last component of a path being typed.
fn complete(t: &mut Text) {
    let raw = t.s.clone();
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home().display()),
        None => raw.clone(),
    };
    let (dir, part) = match expanded.rfind('/') {
        Some(k) => (&expanded[..=k], &expanded[k + 1..]),
        None => return,
    };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) || e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(part) && (part.starts_with('.') || !n.starts_with('.')))
        .collect();
    names.sort();
    let Some(first) = names.first() else { return };
    let Some(kept) = raw.len().checked_sub(part.len()) else { return };
    let common = names.iter().fold(first.clone(), |acc, n| acc.chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect());
    t.s = format!("{}{}{}", &raw[..kept], common, if names.len() == 1 { "/" } else { "" });
    t.caret = t.s.len();
}

fn bookmarks_file() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from);
    config.join("gtk-3.0/bookmarks")
}

fn bookmarked(p: &Path) -> bool {
    let uri = files::uri::to_uri(p);
    std::fs::read_to_string(bookmarks_file()).is_ok_and(|t| t.lines().any(|l| l.split(' ').next() == Some(uri.as_str())))
}

fn bookmarks_edit(p: &Path, add: bool) -> io::Result<()> {
    let file = bookmarks_file();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let uri = files::uri::to_uri(p);
    let mut lines: Vec<&str> = text.lines().filter(|l| l.split(' ').next() != Some(uri.as_str())).collect();
    if add {
        lines.push(&uri);
    }
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&file, lines.join("\n") + "\n")
}

/// What is under the pointer in a file manager window.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Hit {
    None,
    Empty,
    Item(usize),
    Side(usize),
    Eject(usize),
    Crumb(PathBuf),
    Header(Sort),
    Menu(usize),
    Filter(usize),
    /// The chooser's buttons: 0 accept, 1 cancel.
    Button(usize),
}

#[derive(Clone)]
pub(super) enum SideTarget {
    Path(PathBuf),
    Volume(usize),
}

pub(super) struct SideEntry {
    pub label: String,
    pub icon: char,
    pub target: SideTarget,
    pub mounted: bool,
    pub ejectable: bool,
}

impl Terms {
    pub(super) fn side_entries(&self) -> Vec<SideEntry> {
        let mut v: Vec<SideEntry> = self
            .fm
            .places
            .iter()
            .map(|p| SideEntry { label: p.name.clone(), icon: p.icon, target: SideTarget::Path(p.path.clone()), mounted: false, ejectable: false })
            .collect();
        for (k, vol) in self.fm.volumes.iter().enumerate() {
            v.push(SideEntry {
                label: vol.label.clone(),
                icon: '\u{f0a0}',
                target: SideTarget::Volume(k),
                mounted: vol.mount.is_some(),
                ejectable: vol.mount.is_some() || (vol.removable && vol.can_power_off),
            });
        }
        v
    }

    /// Where the cursor's entry is, for a menu opened from the keyboard.
    fn fm_cursor_pos(&self, i: usize) -> (i32, i32) {
        let l = self.fm_layout(i);
        let w = &self.fm.windows[i];
        let row = if w.grid() { w.cursor / l.cols.max(1) } else { w.cursor };
        let col = if w.grid() { w.cursor % l.cols.max(1) } else { 0 };
        let r = row.saturating_sub(w.top) as i32;
        if w.grid() {
            (l.x0 + col as i32 * l.tile.0 + l.tile.0 / 2, l.top + r * l.tile.1 + l.tile.1 / 2)
        } else {
            (l.x0 + 4 * l.cw, l.top + r * l.rh + l.rh)
        }
    }
}

impl state::Persist for Window {
    fn save(&self, e: &mut state::Enc) {
        (self.surface, self.xdg, self.toplevel).save(e);
        (self.size, self.pending).save(e);
        self.dir.to_string_lossy().into_owned().save(e);
        // The zoom rides in the sort byte's high bits, keeping the format.
        (self.hidden, self.zoom > 0, self.reverse, self.sort as u8 | self.zoom << 4).save(e);
        let paths = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
        (paths(&self.back), paths(&self.forward)).save(e);
        self.current().map(|it| it.entry.display().into_owned()).save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        use state::Persist;
        let (surface, xdg, toplevel) = Persist::load(d)?;
        let (size, pending) = Persist::load(d)?;
        let dir: String = Persist::load(d)?;
        let (hidden, grid, reverse, sort): (bool, bool, bool, u8) = Persist::load(d)?;
        let (back, forward): (Vec<String>, Vec<String>) = Persist::load(d)?;
        let current: Option<String> = Persist::load(d)?;
        let mut w = Window::new(surface, xdg, toplevel, PathBuf::from(dir));
        (w.size, w.pending, w.hidden, w.reverse) = (size, pending, hidden, reverse);
        w.zoom = match sort >> 4 {
            0 if grid => ZOOM_GRID,
            z => z.min(ZOOM.len() as u8),
        };
        w.sort = match sort & 0xf {
            1 => Sort::Size,
            2 => Sort::Modified,
            3 => Sort::Type,
            _ => Sort::Name,
        };
        w.back = back.into_iter().map(PathBuf::from).collect();
        w.forward = forward.into_iter().map(PathBuf::from).collect();
        w.focus_name = current.map(OsString::from);
        Ok(w)
    }
}

impl state::Persist for Files {
    /// Only the windows: a chooser's request does not survive the
    /// upgrade (the portal answers it), so it becomes a plain window.
    fn save(&self, e: &mut state::Enc) {
        self.windows.save(e);
    }
    fn load(d: &mut state::Dec) -> state::Result<Self> {
        let windows: Vec<Window> = state::Persist::load(d)?;
        let mut f = Files { windows, ..Default::default() };
        if !f.windows.is_empty() {
            f.places = files::places::places();
            f.jobs.push(Box::new(|| files::udisks::volumes().map(|v| crate::worker::Done::Files(Done::Volumes(v)))));
        }
        Ok(f)
    }
}

impl Files {
    pub fn inotify_fd(&self) -> Option<std::os::fd::RawFd> {
        self.inotify.as_ref().map(|f| f.as_raw_fd())
    }

    pub(super) fn set_pending(&mut self, toplevel: u32, size: (i32, i32)) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.toplevel == toplevel) {
            w.pending = size;
        }
    }

    pub(super) fn by_toplevel(&self, toplevel: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.toplevel == toplevel)
    }

    pub(super) fn by_xdg(&self, xdg: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.xdg == xdg)
    }

    pub(super) fn pointer_left(&mut self) {
        self.press = None;
    }

    pub fn usage(&self) -> (usize, usize) {
        (self.windows.len(), self.windows.iter().map(|w| w.items.len()).sum())
    }

    pub fn thumbs_kept(&self) -> usize {
        self.thumbs.borrow().textures.len()
    }

    pub fn ops_running(&self) -> bool {
        !self.ops.is_empty()
    }
}

pub(super) const KEPT: usize = THUMBS_KEPT;
