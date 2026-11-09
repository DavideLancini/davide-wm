//! Geometry and drawing of file manager windows: text from the terminals'
//! glyph atlas, thumbnails as small textures.

use std::path::PathBuf;

use files::dir::Sort;
use render::{Batch, Quad, Renderer};
use unicode_width::UnicodeWidthChar;

use super::super::glyphs::Glyphs;
use super::super::Terms;
use super::{textual, Hit, Item, Prompt, SideTarget, KEPT};

const THUMB: i32 = files::thumb::NORMAL as i32;
/// Size of the preview pane's image, and how many such images stay loaded.
const PREVIEW: u32 = 512;
const PREVIEWS_KEPT: usize = 4;

/// Thumbnails to draw this frame and where.
type Wanted = Vec<(super::ThumbKey, (i32, i32, i32, i32))>;

const FG: [f32; 4] = rgb(0xe6e6e6);
const DIM: [f32; 4] = rgb(0x8a8a8a);
const BG: [f32; 4] = rgb(0x000000);
const PANEL: [f32; 4] = rgb(0x111111);
const LINE: [f32; 4] = rgb(0x2a2a2a);
const SELECTED: [f32; 4] = rgb(0x264f78);
const CURSOR: [f32; 4] = rgb(0x1f1f1f);
const DIR: [f32; 4] = rgb(0x6cb6ff);
const EXEC: [f32; 4] = rgb(0x8fd18f);
const IMAGE: [f32; 4] = rgb(0xd7a35f);
const MEDIA: [f32; 4] = rgb(0xc792ea);
const DROP: [f32; 4] = rgb(0x2f6b3a);
const PROGRESS: [f32; 4] = rgb(0x2f6f9f);
const MENU: [f32; 4] = rgb(0x1c1c1c);
const ERROR: [f32; 4] = rgb(0xff7b72);

const fn rgb(c: u32) -> [f32; 4] {
    [((c >> 16) & 0xff) as f32 / 255.0, ((c >> 8) & 0xff) as f32 / 255.0, (c & 0xff) as f32 / 255.0, 1.0]
}

/// Where everything goes in a window, in surface pixels.
pub(in super::super) struct Layout {
    pub cw: i32,
    pub ch: i32,
    /// Height of a list row.
    pub rh: i32,
    pub w: i32,
    pub h: i32,
    /// Sidebar width (0: none), the left and right edges of the entries
    /// and the preview pane's left edge (0: none).
    pub side: i32,
    pub x0: i32,
    pub right: i32,
    pub pane: i32,
    /// A line at the bottom for a question, a message or progress, and
    /// where the content above it ends.
    pub strip: bool,
    pub base: i32,
    /// Top of the first row of entries, and the bottom of the last.
    pub top: i32,
    pub bottom: i32,
    pub cols: usize,
    pub tile: (i32, i32),
    pub grid: bool,
    /// Width in cells of the size and date columns (0: hidden).
    pub size_cols: i32,
    pub date_cols: i32,
}

impl Layout {
    /// Rows of entries (or of tiles) that fit.
    pub fn page(&self) -> usize {
        let h = if self.grid { self.tile.1 } else { self.rh };
        ((self.bottom - self.top) / h.max(1)).max(1) as usize
    }

    fn status_y(&self) -> i32 {
        self.h - config::TERM_PAD - self.ch
    }
}

impl Terms {
    pub(in super::super) fn fm_layout(&self, i: usize) -> Layout {
        let (cw, ch) = self.cell();
        let w = &self.fm.windows[i];
        let pad = config::TERM_PAD;
        let (width, height) = w.size;
        let total = (width - 2 * pad) / cw;
        let side = if total >= 70 { 22 * cw } else { 0 };
        let x0 = pad + if side > 0 { side + 2 * cw } else { 0 };
        let room = width - pad - x0;
        let pane_w = if !self.fm.no_preview && room / cw >= 56 { (room * 2 / 5).clamp(26 * cw, 64 * cw) } else { 0 };
        let right = width - pad - if pane_w > 0 { pane_w + 2 * cw } else { 0 };
        let pane = if pane_w > 0 { right + cw } else { 0 };
        let main = (right - x0) / cw;
        let date_cols = if main >= 50 { 12 } else { 0 };
        let size_cols = if main >= 34 { 9 } else { 0 };
        let rh = ch + 8;
        let strip = w.prompt.is_some() || w.status.is_some() || !self.fm.ops.is_empty();
        let base = if strip { height - pad - ch - 6 } else { height - pad };
        let chooser_rows = w.chooser.as_ref().map_or(0, |c| 1 + c.save as i32);
        let bottom = base - chooser_rows * (rh + 4);
        let top = if w.grid { pad + ch + 14 } else { pad + 2 * ch + 18 };
        let tile = ((THUMB + 16).max(14 * cw), THUMB + 12 + 2 * ch);
        let cols = ((right - x0) / tile.0).max(1) as usize;
        Layout { cw, ch, rh, w: width, h: height, side, x0, right, pane, strip, base, top, bottom, cols, tile, grid: w.grid, size_cols, date_cols }
    }

    /// The path bar: each part's x, width and target.
    fn crumbs(&self, i: usize, l: &Layout) -> Vec<(i32, i32, String, PathBuf)> {
        let w = &self.fm.windows[i];
        let home = super::home();
        let mut parts: Vec<(String, PathBuf)> = Vec::new();
        let mut acc = PathBuf::new();
        let under_home = w.dir.starts_with(&home) && home != std::path::Path::new("/");
        let rest = if under_home {
            parts.push(("~".into(), home.clone()));
            acc = home.clone();
            w.dir.strip_prefix(&home).map(|p| p.to_path_buf()).unwrap_or_default()
        } else {
            parts.push(("/".into(), PathBuf::from("/")));
            acc.push("/");
            w.dir.strip_prefix("/").map(|p| p.to_path_buf()).unwrap_or_default()
        };
        for c in rest.components() {
            acc.push(c);
            parts.push((c.as_os_str().to_string_lossy().into_owned(), acc.clone()));
        }
        let room = (l.right - l.x0) / l.cw;
        let width = |p: &[(String, PathBuf)]| p.iter().map(|(s, _)| cells(s) + 3).sum::<i32>();
        let mut skipped = false;
        while parts.len() > 1 && width(&parts) > room - 2 {
            parts.remove(0);
            skipped = true;
        }
        let mut x = l.x0 + if skipped { 2 * l.cw } else { 0 };
        parts
            .into_iter()
            .map(|(s, p)| {
                let wd = cells(&s) * l.cw;
                let r = (x, wd, s, p);
                x += wd + 3 * l.cw;
                r
            })
            .collect()
    }

    /// Sidebar rows: entry index and top; the volumes come after a heading.
    fn side_rows(&self, l: &Layout) -> Vec<(usize, i32)> {
        let start = config::TERM_PAD + l.ch + 14;
        let places = self.fm.places.len();
        self.side_entries()
            .iter()
            .enumerate()
            .map(|(k, _)| (k, start + k as i32 * l.rh + if k >= places { l.rh + 6 } else { 0 }))
            .filter(|(_, y)| *y + l.rh <= l.bottom)
            .collect()
    }

    fn menu_rect(&self, i: usize, l: &Layout) -> Option<(i32, i32, i32, i32)> {
        let m = self.fm.windows[i].menu.as_ref()?;
        let width = (m.items.iter().map(|(s, _)| cells(s)).max().unwrap_or(4) + 4) * l.cw;
        let height = m.items.len() as i32 * l.rh + 8;
        let x = m.x.min(l.w - width - 4).max(4);
        let y = m.y.min(l.h - height - 4).max(4);
        Some((x, y, width, height))
    }

    /// The chooser's last row: its top, the filter labels (x, width) and
    /// the buttons (x, width, label).
    #[allow(clippy::type_complexity)]
    fn chooser_row(&self, i: usize, l: &Layout) -> (i32, Vec<(i32, i32)>, Vec<(i32, i32, String)>) {
        let w = &self.fm.windows[i];
        let y = l.base - l.rh;
        let Some(c) = &w.chooser else { return (y, Vec::new(), Vec::new()) };
        let mut x = l.x0;
        let filters = c
            .filters
            .iter()
            .map(|(name, _)| {
                let wd = (cells(name) + 2) * l.cw;
                let r = (x, wd);
                x += wd + l.cw;
                r
            })
            .collect();
        let accept = if !c.accept.is_empty() { c.accept.clone() } else if c.save { "Salva".into() } else if c.directory { "Scegli".into() } else { "Apri".into() };
        let labels = [accept, "Annulla".to_string()];
        let mut right = l.right;
        let buttons = labels
            .into_iter()
            .map(|s| {
                let wd = (cells(&s) + 4) * l.cw;
                right -= wd;
                let r = (right, wd, s);
                right -= l.cw;
                r
            })
            .collect();
        (y, filters, buttons)
    }

    pub(in super::super) fn fm_hit(&self, i: usize, x: f64, y: f64) -> Hit {
        let l = self.fm_layout(i);
        let w = &self.fm.windows[i];
        let (x, y) = (x as i32, y as i32);
        if let Some((mx, my, mw, mh)) = self.menu_rect(i, &l) {
            if x >= mx && x < mx + mw && y >= my + 4 && y < my + mh - 4 {
                return Hit::Menu(((y - my - 4) / l.rh) as usize);
            }
            return Hit::None;
        }
        let pad = config::TERM_PAD;
        if l.pane > 0 && x >= l.pane {
            return Hit::None;
        }
        if y >= pad && y < pad + l.ch + 4 {
            return self.crumbs(i, &l).into_iter().find(|(cx, cw, ..)| x >= *cx && x < cx + cw).map_or(Hit::None, |(.., p)| Hit::Crumb(p));
        }
        if l.side > 0 && x < pad + l.side {
            for (k, ry) in self.side_rows(&l) {
                if y >= ry && y < ry + l.rh {
                    let ejectable = self.side_entries().get(k).is_some_and(|e| e.ejectable);
                    if let (true, SideTarget::Volume(v)) = (ejectable && x >= pad + l.side - 3 * l.cw, &self.side_entries()[k].target) {
                        return Hit::Eject(*v);
                    }
                    return Hit::Side(k);
                }
            }
            return Hit::None;
        }
        if w.chooser.is_some() {
            let (ry, filters, buttons) = self.chooser_row(i, &l);
            if y >= ry && y < ry + l.rh {
                if let Some(k) = filters.iter().position(|(fx, fw)| x >= *fx && x < fx + fw) {
                    return Hit::Filter(k);
                }
                if let Some(k) = buttons.iter().position(|(bx, bw, _)| x >= *bx && x < bx + bw) {
                    return Hit::Button(k);
                }
                return Hit::None;
            }
        }
        if !l.grid && y >= l.top - l.ch - 10 && y < l.top {
            let right = l.right;
            let date_x = right - l.date_cols * l.cw;
            let size_x = date_x - l.size_cols * l.cw - if l.date_cols > 0 { l.cw } else { 0 };
            return Hit::Header(if l.date_cols > 0 && x >= date_x {
                Sort::Modified
            } else if l.size_cols > 0 && x >= size_x {
                Sort::Size
            } else {
                Sort::Name
            });
        }
        if y < l.top || y >= l.bottom || x < l.x0 {
            return Hit::None;
        }
        let k = if l.grid {
            let (col, row) = ((x - l.x0) / l.tile.0, (y - l.top) / l.tile.1);
            if col as usize >= l.cols {
                return Hit::Empty;
            }
            (w.top + row as usize) * l.cols + col as usize
        } else {
            w.top + ((y - l.top) / l.rh) as usize
        };
        if k < w.shown.len() {
            Hit::Item(k)
        } else {
            Hit::Empty
        }
    }

    // ---- drawing ---------------------------------------------------------

    pub(in super::super) fn draw_files(&self, r: &mut Renderer, surface: u32, x: i32, y: i32, focused: bool) {
        let Some(i) = self.fm.windows.iter().position(|w| w.surface == surface) else { return };
        let l = self.fm_layout(i);
        let mut g = self.glyphs.borrow_mut();
        if g.is_none() {
            *g = Glyphs::new();
        }
        let Some(g) = g.as_mut() else { return };
        let mut batch = self.batch.borrow_mut();
        batch.clear();
        let w = &self.fm.windows[i];
        let mut p = Paint { r, g, b: &mut batch, ox: x, oy: y, cw: l.cw, ch: l.ch };
        p.fill(0, 0, l.w, l.h, BG);
        let pad = config::TERM_PAD;

        // Path bar.
        let crumbs = self.crumbs(i, &l);
        if crumbs.first().is_some_and(|c| c.0 > l.x0) {
            p.text(l.x0, pad, "…", 1, DIM, false);
        }
        let last = crumbs.len().saturating_sub(1);
        for (k, (cx, _, s, _)) in crumbs.iter().enumerate() {
            p.text(*cx, pad, s, 200, if k == last { FG } else { DIM }, k == last);
            if k < last {
                p.text(cx + (cells(s) + 1) * l.cw, pad, "›", 1, DIM, false);
            }
        }
        let mut flags = Vec::new();
        if !w.selected.is_empty() {
            let size: u64 = w.items.iter().filter(|it| w.selected.contains(&it.entry.name) && !it.entry.is_dir()).map(|it| it.entry.size).sum();
            flags.push(format!("{} selezionati, {}", w.selected.len(), files::dir::human_size(size)));
        }
        if super::is_trash(&w.dir) {
            flags.push("Ctrl+R ripristina".into());
        }
        if w.hidden {
            flags.push("nascosti".into());
        }
        if !w.filter.is_empty() {
            flags.push(format!("filtro: {}", w.filter));
        }
        let flags = flags.join("  ·  ");
        if !flags.is_empty() {
            let fx = l.right - cells(&flags) * l.cw;
            p.text(fx, pad, &flags, 200, DIR, false);
        }

        // Sidebar.
        if l.side > 0 {
            p.fill(0, 0, pad + l.side, l.h, PANEL);
            let entries = self.side_entries();
            let places = self.fm.places.len();
            let rows = self.side_rows(&l);
            if let Some((_, y)) = rows.iter().find(|(k, _)| *k == places) {
                p.text(pad + l.cw, y - l.rh - 2, "Dispositivi", 20, DIM, true);
            }
            for (k, ry) in rows {
                let e = &entries[k];
                let here = match &e.target {
                    SideTarget::Path(pp) => *pp == w.dir,
                    SideTarget::Volume(v) => self.fm.volumes.get(*v).and_then(|v| v.mount.as_ref()) == Some(&w.dir),
                };
                if here {
                    p.fill(pad, ry, l.side, l.rh, CURSOR);
                }
                let color = if matches!(e.target, SideTarget::Volume(_)) && !e.mounted { DIM } else { FG };
                p.icon(pad + l.cw, ry + 4, e.icon, DIR);
                let room = l.side / l.cw - 5 - if e.ejectable { 3 } else { 0 };
                p.text(pad + 4 * l.cw, ry + 4, &e.label, room, color, here);
                if e.ejectable {
                    p.text(pad + l.side - 2 * l.cw, ry + 4, "⏏", 1, DIM, false);
                }
            }
        }

        // Column headings.
        let right = l.right;
        let date_x = right - l.date_cols * l.cw;
        let size_x = date_x - l.size_cols * l.cw - if l.date_cols > 0 { l.cw } else { 0 };
        let arrow = if w.reverse { " ↑" } else { " ↓" };
        if !l.grid {
            let hy = l.top - l.ch - 10;
            let head = |by: Sort, s: &str| if w.sort == by { format!("{s}{arrow}") } else { s.to_string() };
            p.text(l.x0 + 3 * l.cw, hy, &head(Sort::Name, "Nome"), 30, DIM, w.sort == Sort::Name);
            if l.size_cols > 0 {
                let s = head(Sort::Size, "Dim.");
                p.text(size_x + (l.size_cols - cells(&s)) * l.cw, hy, &s, l.size_cols, DIM, w.sort == Sort::Size);
            }
            if l.date_cols > 0 {
                p.text(date_x, hy, &head(Sort::Modified, "Modificato"), l.date_cols, DIM, w.sort == Sort::Modified);
            }
            p.fill(l.x0, l.top - 5, right - l.x0, 1, LINE);
        }

        // Entries.
        let drop = self.fm.drop.as_ref().filter(|d| d.surface == surface);
        let now = now();
        let mut thumbs: Wanted = Vec::new();
        let (first, count) = if l.grid { (w.top * l.cols, l.page() * l.cols) } else { (w.top, l.page()) };
        if w.loading != 0 && w.items.is_empty() {
            p.text(l.x0, l.top, "…", 1, DIM, false);
        } else if w.shown.is_empty() {
            let msg = if w.filter.is_empty() { "Cartella vuota" } else { "Nessun risultato" };
            p.text(l.x0 + 3 * l.cw, l.top + 4, msg, 40, DIM, false);
            if focused && w.chooser.is_none() && w.items.is_empty() {
                p.text(l.x0 + 3 * l.cw, l.top + 4 + l.rh, "F4 terminale · Ctrl+L vai a · Ctrl+H nascosti", (right - l.x0) / l.cw - 3, DIM, false);
            }
        }
        for k in first..(first + count).min(w.shown.len()) {
            let it = &w.items[w.shown[k]];
            let e = &it.entry;
            let selected = w.selected.contains(&e.name);
            let target = drop.and_then(|d| d.target.as_ref()).is_some_and(|t| e.is_dir() && *t == w.dir.join(&e.name));
            let bg = if target {
                Some(DROP)
            } else if selected {
                Some(SELECTED)
            } else if k == w.cursor && focused {
                Some(CURSOR)
            } else {
                None
            };
            let color = color_of(it);
            let name = e.display();
            if l.grid {
                let n = k - first;
                let (tx, ty) = (l.x0 + (n % l.cols) as i32 * l.tile.0, l.top + (n / l.cols) as i32 * l.tile.1);
                if let Some(bg) = bg {
                    p.fill(tx + 2, ty + 2, l.tile.0 - 4, l.tile.1 - 4, bg);
                }
                if k == w.cursor && focused {
                    p.outline(tx + 2, ty + 2, l.tile.0 - 4, l.tile.1 - 4, LINE);
                }
                let (ix, iy) = (tx + (l.tile.0 - THUMB) / 2, ty + 6);
                let visual = it.mime.starts_with("image/") || it.mime.starts_with("video/");
                if visual {
                    thumbs.push(((w.dir.join(&e.name), files::thumb::NORMAL), (ix, iy, THUMB, THUMB)));
                }
                p.fill(ix + THUMB / 2 - 2 * l.cw, iy + THUMB / 2 - l.ch, 4 * l.cw, 2 * l.ch, PANEL);
                p.icon(ix + THUMB / 2 - l.cw, iy + THUMB / 2 - l.ch / 2, it.icon, color);
                let room = l.tile.0 / l.cw - 1;
                let (a, b) = split_name(&name, room);
                for (row, part) in [a, b].iter().enumerate() {
                    if part.is_empty() {
                        continue;
                    }
                    let px = tx + (l.tile.0 - cells(part).min(room) * l.cw) / 2;
                    p.text(px, iy + THUMB + 4 + row as i32 * l.ch, part, room, color, false);
                }
            } else {
                let ry = l.top + (k - first) as i32 * l.rh;
                if let Some(bg) = bg {
                    p.fill(l.x0 - l.cw / 2, ry, right - l.x0 + l.cw / 2, l.rh, bg);
                }
                let ty = ry + 4;
                p.icon(l.x0, ty, it.icon, color);
                let name_room = (size_x - l.x0) / l.cw - 4;
                let label = if e.link { format!("{name} →") } else { name.into_owned() };
                p.text(l.x0 + 3 * l.cw, ty, &label, name_room, color, false);
                if l.size_cols > 0 {
                    let s = if e.is_dir() { String::new() } else { files::dir::human_size(e.size) };
                    p.text(size_x + (l.size_cols - cells(&s)).max(0) * l.cw, ty, &s, l.size_cols, DIM, false);
                }
                if l.date_cols > 0 {
                    p.text(date_x, ty, &files::dir::human_time(e.mtime, now), l.date_cols, DIM, false);
                }
            }
        }
        if drop.is_some_and(|d| d.target.as_ref() == Some(&w.dir)) {
            p.outline(l.x0 - l.cw / 2, l.top - 2, right - l.x0 + l.cw / 2, l.bottom - l.top + 2, DROP);
        }

        // Scroll position.
        let total = if l.grid { w.shown.len().div_ceil(l.cols) } else { w.shown.len() };
        if total > l.page() {
            let track = l.bottom - l.top;
            let h = (track as usize * l.page() / total).max(12) as i32;
            let y0 = l.top + ((track - h) as usize * w.top / (total - l.page()).max(1)) as i32;
            p.fill(right + 2, y0, 3, h, LINE);
        }

        // Chooser rows.
        if let Some(c) = &w.chooser {
            let (ry, filters, buttons) = self.chooser_row(i, &l);
            if c.save {
                let ny = ry - l.rh - 4;
                p.text(l.x0, ny + 4, "Nome:", 6, DIM, false);
                p.field(l.x0 + 6 * l.cw, ny, right - l.x0 - 6 * l.cw, l.rh, &c.name, focused && w.prompt.is_none());
            }
            for (k, ((fx, fw), (name, _))) in filters.iter().zip(&c.filters).enumerate() {
                if k == c.filter {
                    p.fill(*fx, ry, *fw, l.rh, SELECTED);
                } else {
                    p.outline(*fx, ry, *fw, l.rh, LINE);
                }
                p.text(fx + l.cw, ry + 4, name, cells(name), FG, false);
            }
            for (k, (bx, bw, s)) in buttons.iter().enumerate() {
                p.fill(*bx, ry, *bw, l.rh, if k == 0 { PROGRESS } else { PANEL });
                p.text(bx + 2 * l.cw, ry + 4, s, cells(s), FG, k == 0);
            }
        }

        self.draw_preview(&mut p, i, &l, &mut thumbs);

        // Bottom line, only while there is a question, a message or an operation.
        if l.strip {
            let sy = l.status_y();
            p.fill(0, sy - 4, l.w, l.ch + 8 + pad, PANEL);
            p.fill(0, sy - 4, l.w, 1, LINE);
            let width = l.w - 2 * pad;
            let room = width / l.cw;
            let ask = |s: &str| format!("{s} (Invio sì, Esc no)");
            match &w.prompt {
                Some(Prompt::Rename { text, .. }) => p.labeled(pad, sy, "Rinomina:", text, width),
                Some(Prompt::NewFolder(t)) => p.labeled(pad, sy, "Nuova cartella:", t, width),
                Some(Prompt::NewFile(t)) => p.labeled(pad, sy, "Nuovo file:", t, width),
                Some(Prompt::Path(t)) => p.labeled(pad, sy, "Vai a:", t, width),
                Some(Prompt::Delete(v)) => p.text(pad, sy, &ask(&format!("Eliminare definitivamente {} elementi?", v.len())), room, ERROR, true),
                Some(Prompt::EmptyTrash) => p.text(pad, sy, &ask("Svuotare il cestino?"), room, ERROR, true),
                Some(Prompt::Overwrite(path)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    p.text(pad, sy, &ask(&format!("{name} esiste: sostituirlo?")), room, ERROR, true)
                }
                None => {
                    if let Some(s) = &w.status {
                        p.text(pad, sy, s, room, FG, false);
                    } else if let Some((_, label, prog, _)) = self.fm.ops.first() {
                        let (done, total) = (prog.done.load(std::sync::atomic::Ordering::Relaxed), prog.total.load(std::sync::atomic::Ordering::Relaxed));
                        let pct = (done * 100).checked_div(total).unwrap_or(0).min(100);
                        let bar = 20 * l.cw;
                        p.fill(pad, sy + 2, bar, l.ch - 4, LINE);
                        p.fill(pad, sy + 2, bar * pct as i32 / 100, l.ch - 4, PROGRESS);
                        let cur = prog.current.lock().map(|c| c.clone()).unwrap_or_default();
                        let more = if self.fm.ops.len() > 1 { format!(" (+{})", self.fm.ops.len() - 1) } else { String::new() };
                        p.text(pad + bar + l.cw, sy, &format!("{label}{more} {pct}%  {cur}"), room - 21, FG, false);
                    }
                }
            }
        }
        let flush = |p: &mut Paint| {
            if let Some(atlas) = p.g.atlas() {
                p.r.draw_batch(atlas, p.b);
            } else if !p.b.is_empty() {
                let atlas = p.r.create_atlas(1, 1);
                p.r.draw_batch(&atlas, p.b);
                p.r.destroy_texture(atlas);
            }
            p.b.clear();
        };
        flush(&mut p);

        // Thumbnails over their tiles.
        if !thumbs.is_empty() || !self.fm.thumbs.borrow().ready.is_empty() {
            let mut th = self.fm.thumbs.borrow_mut();
            th.frame += 1;
            let frame = th.frame;
            for (path, img) in std::mem::take(&mut th.ready) {
                let tex = p.r.upload_rgba(img.width as i32, img.height as i32, &img.rgba);
                if let Some((old, ..)) = th.textures.insert(path, (tex, (img.width, img.height), frame)) {
                    p.r.destroy_texture(old);
                }
            }
            let mime_of = |path: &PathBuf| w.items.iter().find(|it| w.dir.join(&it.entry.name) == *path).map(|it| (it.entry.mtime, it.mime.clone()));
            for (key, (tx, ty, tw, tht)) in thumbs {
                if let Some((tex, (iw, ih), used)) = th.textures.get_mut(&key) {
                    *used = frame;
                    let scale = (tw as f32 / *iw as f32).min(tht as f32 / *ih as f32).min(1.0);
                    let (dw, dh) = (*iw as f32 * scale, *ih as f32 * scale);
                    let q = Quad { x: (x + tx) as f32 + (tw as f32 - dw) / 2.0, y: (y + ty) as f32 + (tht as f32 - dh) / 2.0, w: dw, h: dh };
                    p.r.fill(q, BG);
                    p.r.draw(tex, q, false);
                } else if !th.asked.contains(&key) && !th.failed.contains(&key) {
                    if let Some((mtime, mime)) = mime_of(&key.0) {
                        th.asked.insert(key.clone());
                        th.wanted.push((key, mtime, mime));
                    }
                }
            }
            for (large, kept) in [(false, KEPT), (true, PREVIEWS_KEPT)] {
                let mut by_age: Vec<(u64, super::ThumbKey)> = th.textures.iter().filter(|(k, v)| (k.1 > files::thumb::NORMAL) == large && v.2 != frame).map(|(k, v)| (v.2, k.clone())).collect();
                let n = th.textures.keys().filter(|k| (k.1 > files::thumb::NORMAL) == large).count();
                if n > kept {
                    by_age.sort();
                    for (_, k) in by_age.into_iter().take(n - kept) {
                        if let Some((tex, ..)) = th.textures.remove(&k) {
                            p.r.destroy_texture(tex);
                        }
                    }
                }
            }
        }

        // The menu, over everything.
        if let (Some(m), Some((mx, my, mw, mh))) = (&w.menu, self.menu_rect(i, &l)) {
            p.fill(mx, my, mw, mh, MENU);
            p.outline(mx, my, mw, mh, LINE);
            for (k, (label, _)) in m.items.iter().enumerate() {
                let iy = my + 4 + k as i32 * l.rh;
                if k == m.hover {
                    p.fill(mx + 1, iy, mw - 2, l.rh, SELECTED);
                }
                p.text(mx + 2 * l.cw, iy + 4, label, mw / l.cw - 3, FG, false);
            }
            if m.with.is_some() {
                let hint = "Ctrl+Invio: predefinita";
                p.fill(mx, my + mh, mw.max(cells(hint) * l.cw + 2 * l.cw), l.rh, MENU);
                p.text(mx + l.cw, my + mh + 4, hint, 40, DIM, false);
            }
            flush(&mut p);
        }
    }
}

impl Terms {
    /// The pane on the right: the entry under the cursor (an image, its
    /// details and its first lines or entries), or the selection's totals.
    fn draw_preview(&self, p: &mut Paint, i: usize, l: &Layout, thumbs: &mut Wanted) {
        if l.pane == 0 {
            return;
        }
        let w = &self.fm.windows[i];
        let pad = config::TERM_PAD;
        p.fill(l.pane, 0, l.w - l.pane, l.h, PANEL);
        let px = l.pane + 2 * l.cw;
        let width = l.w - pad - l.cw - px;
        let room = width / l.cw;
        let end = if l.strip { l.status_y() - 4 } else { l.h - pad };
        let mut y = pad + l.ch + 14;
        let row = |p: &mut Paint, y: &mut i32, label: &str, value: &str| {
            p.text(px, *y, label, 12, DIM, false);
            p.text(px + 13 * l.cw, *y, value, room - 13, FG, false);
            *y += l.ch + 4;
        };
        if w.selected.len() > 1 {
            let picked: Vec<&Item> = w.items.iter().filter(|it| w.selected.contains(&it.entry.name)).collect();
            let dirs = picked.iter().filter(|it| it.entry.is_dir()).count();
            let size: u64 = picked.iter().filter(|it| !it.entry.is_dir()).map(|it| it.entry.size).sum();
            p.text(px, y, &format!("{} elementi selezionati", picked.len()), room, FG, true);
            y += l.ch + 12;
            row(p, &mut y, "Cartelle", &dirs.to_string());
            row(p, &mut y, "File", &(picked.len() - dirs).to_string());
            row(p, &mut y, "Dimensione", &files::dir::human_size(size));
            return;
        }
        let Some(it) = w.current() else { return };
        let e = &it.entry;
        let path = w.dir.join(&e.name);
        if matches!(it.mime.as_str(), "image/png" | "image/jpeg") {
            let side = width.min(PREVIEW as i32).min((end - y) / 2);
            thumbs.push(((path.clone(), PREVIEW), (px, y, width, side)));
            y += side + 12;
        }
        let name = e.display();
        p.icon(px, y, it.icon, color_of(it));
        let (a, b) = split_name(&name, room - 3);
        p.text(px + 3 * l.cw, y, &a, room - 3, FG, true);
        y += l.ch;
        if !b.is_empty() {
            p.text(px + 3 * l.cw, y, &b, room - 3, FG, true);
            y += l.ch;
        }
        y += 12;

        let mut peek = self.fm.peek.borrow_mut();
        let lines = match &peek.shown {
            Some((at, mtime, lines)) if *at == path && *mtime == e.mtime => Some(lines),
            _ => None,
        };
        let wants = e.is_dir() || (textual(&it.mime) && e.size > 0);
        row(p, &mut y, "Tipo", &it.mime);
        if e.is_dir() {
            let count = lines.map_or("…".into(), |v| format!("{} elementi", v.len()));
            row(p, &mut y, "Contenuto", &count);
        } else {
            row(p, &mut y, "Dimensione", &files::dir::human_size(e.size));
        }
        row(p, &mut y, "Modificato", &files::dir::full_time(e.mtime));
        row(p, &mut y, "Permessi", &files::dir::permissions(e.mode));
        if e.link {
            let to = std::fs::read_link(&path).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default();
            row(p, &mut y, "Collegamento", &to);
        }
        if !wants {
            return;
        }
        y += 8;
        p.fill(px, y, width, 1, LINE);
        y += 12;
        match lines {
            Some(lines) => {
                for (text, dir) in lines {
                    if y + l.ch > end {
                        break;
                    }
                    let shown = if *dir { format!("{text}/") } else { text.clone() };
                    p.text(px, y, &shown, room, if *dir { DIR } else { FG }, false);
                    y += l.ch;
                }
            }
            None => {
                let key = (path.clone(), e.mtime);
                if peek.asked.as_ref() != Some(&key) {
                    peek.asked = Some(key);
                    peek.wanted = Some((path, e.mtime, e.is_dir()));
                }
            }
        }
    }
}

fn color_of(it: &Item) -> [f32; 4] {
    let e = &it.entry;
    if e.is_dir() {
        DIR
    } else if it.mime.starts_with("image/") {
        IMAGE
    } else if it.mime.starts_with("video/") || it.mime.starts_with("audio/") {
        MEDIA
    } else if e.mode & 0o111 != 0 && e.kind == files::dir::Kind::File {
        EXEC
    } else {
        FG
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn cells(s: &str) -> i32 {
    s.chars().map(|c| c.width().unwrap_or(0) as i32).sum()
}

/// A name on up to two lines of `room` cells.
fn split_name(name: &str, room: i32) -> (String, String) {
    if cells(name) <= room {
        return (name.to_string(), String::new());
    }
    let mut first = String::new();
    let mut used = 0;
    let mut rest = name.chars().peekable();
    while let Some(c) = rest.peek() {
        let w = c.width().unwrap_or(0) as i32;
        if used + w > room {
            break;
        }
        used += w;
        first.push(*c);
        rest.next();
    }
    (first, rest.collect())
}

struct Paint<'a> {
    r: &'a mut Renderer,
    g: &'a mut Glyphs,
    b: &'a mut Batch,
    ox: i32,
    oy: i32,
    cw: i32,
    ch: i32,
}

impl Paint<'_> {
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: [f32; 4]) {
        if w > 0 && h > 0 {
            self.b.fill(Quad { x: (self.ox + x) as f32, y: (self.oy + y) as f32, w: w as f32, h: h as f32 }, color);
        }
    }

    fn outline(&mut self, x: i32, y: i32, w: i32, h: i32, color: [f32; 4]) {
        self.fill(x, y, w, 1, color);
        self.fill(x, y + h - 1, w, 1, color);
        self.fill(x, y, 1, h, color);
        self.fill(x + w - 1, y, 1, h, color);
    }

    fn glyph(&mut self, x: i32, y: i32, c: char, color: [f32; 4], bold: bool, wide: bool) {
        if let Some(slot) = self.g.get(self.r, c, bold, wide) {
            if let Some(atlas) = self.g.atlas() {
                let q = Quad { x: (self.ox + x) as f32 + slot.dx, y: (self.oy + y) as f32 + slot.dy, w: slot.src.w, h: slot.src.h };
                self.b.glyph(atlas, slot.src, q, color);
            }
        }
    }

    /// An icon, given two cells.
    fn icon(&mut self, x: i32, y: i32, c: char, color: [f32; 4]) {
        self.glyph(x, y, c, color, false, true);
    }

    /// Text cut to `room` cells, with an ellipsis when it does not fit.
    fn text(&mut self, x: i32, y: i32, s: &str, room: i32, color: [f32; 4], bold: bool) {
        let fits = cells(s) <= room;
        let mut cx = 0;
        for c in s.chars() {
            let w = c.width().unwrap_or(0) as i32;
            if w == 0 {
                continue;
            }
            if !fits && cx + w > room - 1 {
                self.glyph(x + cx * self.cw, y, '…', color, bold, false);
                return;
            }
            if c != ' ' {
                self.glyph(x + cx * self.cw, y, c, color, bold, w == 2);
            }
            cx += w;
        }
    }

    /// An edited line in a box, with its caret and marked range.
    fn field(&mut self, x: i32, y: i32, w: i32, h: i32, t: &super::Text, active: bool) {
        self.fill(x, y, w, h, BG);
        self.outline(x, y, w, h, if active { DIR } else { LINE });
        let room = w / self.cw - 2;
        let before = cells(&t.s[..t.caret]);
        let shift = (before - room + 1).max(0);
        let visible: String = {
            let mut skipped = 0;
            t.s.chars()
                .skip_while(|c| {
                    let keep = skipped < shift;
                    skipped += c.width().unwrap_or(0) as i32;
                    keep
                })
                .collect()
        };
        let tx = x + self.cw;
        if let Some((a, b)) = t.mark {
            let (ma, mb) = (cells(&t.s[..a]) - shift, cells(&t.s[..b]) - shift);
            self.fill(tx + ma.max(0) * self.cw, y + 2, (mb - ma.max(0)).max(0) * self.cw, h - 4, SELECTED);
        }
        self.text(tx, y + 2, &visible, room, FG, false);
        if active {
            self.fill(tx + (before - shift) * self.cw, y + 2, 2, self.ch, FG);
        }
    }

    fn labeled(&mut self, x: i32, y: i32, label: &str, t: &super::Text, width: i32) {
        self.text(x, y, label, 30, DIM, true);
        let lx = x + (cells(label) + 1) * self.cw;
        self.field(lx, y - 3, width - (lx - x), self.ch + 6, t, true);
    }
}
