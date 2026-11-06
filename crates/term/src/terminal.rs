use crate::cell::{attr, default_palette, dec_graphics, joins, width, Cell, Clusters, Color, ColorKind, Rgb, Theme};
use crate::grid::{fill_rows, wide_pair, Row};
use crate::parser::Parser;
use crate::scrollback::{self, Scrollback};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseMode {
    #[default]
    Off,
    /// 9: presses only.
    X10,
    /// 1000: presses and releases.
    Click,
    /// 1002: plus motion while a button is held.
    Drag,
    /// 1003: plus all motion.
    Motion,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MouseEncoding {
    #[default]
    X10,
    Utf8,
    Sgr,
    Urxvt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Charset {
    #[default]
    Ascii,
    Graphics,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Modes {
    /// DECCKM.
    pub app_cursor: bool,
    /// DECKPAM.
    pub app_keypad: bool,
    /// DECAWM.
    pub autowrap: bool,
    /// DECTCEM.
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    /// DECSCNM: swap default foreground and background.
    pub reverse_video: bool,
    /// IRM.
    pub insert: bool,
    /// LNM: LF also returns, Enter sends CR LF.
    pub newline: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    /// 2026: the application is mid-frame; defer drawing (with a timeout).
    pub sync: bool,
    /// 1007: wheel sends arrow keys on the alternate screen.
    pub alt_scroll: bool,
    pub mouse: MouseMode,
    pub mouse_encoding: MouseEncoding,
}

impl Default for Modes {
    fn default() -> Modes {
        Modes {
            app_cursor: false,
            app_keypad: false,
            autowrap: true,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            reverse_video: false,
            insert: false,
            newline: false,
            bracketed_paste: false,
            focus_events: false,
            sync: false,
            alt_scroll: true,
            mouse: MouseMode::Off,
            mouse_encoding: MouseEncoding::X10,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Cursor {
    pub x: usize,
    pub y: usize,
    /// Template for printed cells (ch unused).
    pub pen: Cell,
    /// Printed in the last column; the next character wraps first.
    pub pending: bool,
    /// DECOM.
    pub origin: bool,
    pub g: [Charset; 2],
    pub gl: u8,
}

impl Default for Cursor {
    fn default() -> Cursor {
        Cursor { x: 0, y: 0, pen: Cell::default(), pending: false, origin: false, g: [Charset::Ascii; 2], gl: 0 }
    }
}

/// Where the renderer should draw the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CursorInfo {
    pub col: usize,
    /// Visible row (accounts for the scrolled-back view).
    pub row: usize,
    /// DECTCEM on and the cursor row is in view.
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

/// Visible rows to redraw since the last [`Terminal::take_damage`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Damage {
    /// Redraw everything (scroll, view move, resize, screen switch).
    pub full: bool,
    bits: [u64; 8],
}

impl Damage {
    fn set(&mut self, y: usize) {
        match self.bits.get_mut(y / 64) {
            Some(w) => *w |= 1 << (y % 64),
            None => self.full = true,
        }
    }

    pub fn contains(&self, y: usize) -> bool {
        self.full || self.bits.get(y / 64).is_some_and(|w| w & 1 << (y % 64) != 0)
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.bits.iter().all(|w| *w == 0)
    }

    /// Dirty rows; when `full`, every row of the screen is dirty instead.
    pub fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.bits.len() * 64).filter(|y| self.bits[y / 64] & 1 << (y % 64) != 0)
    }
}

/// Point in absolute line numbering: line 0 is the first line the main
/// screen ever showed; scrolling does not change a line's number. Lines
/// dropped from the scrollback are gone (`Terminal::first_line`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Point {
    pub line: u64,
    pub col: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectionMode {
    Char,
    Word,
    Line,
}

pub(crate) struct Link {
    pub id: Box<str>,
    pub uri: Box<str>,
}

pub struct Terminal {
    pub(crate) parser: Parser,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    pub(crate) main: Vec<Row>,
    /// Empty while the main screen is shown.
    pub(crate) alt: Vec<Row>,
    pub(crate) alt_active: bool,
    pub(crate) sb: Scrollback,
    /// Lines that have scrolled off the main screen into history, ever.
    pub(crate) base: u64,
    pub(crate) view: usize,
    pub(crate) cur: Cursor,
    pub(crate) saved: [Cursor; 2],
    pub(crate) top: usize,
    pub(crate) bottom: usize,
    pub(crate) tabs: Vec<bool>,
    pub(crate) modes: Modes,
    pub(crate) last_char: Option<char>,
    pub(crate) title: String,
    pub(crate) title_changed: bool,
    pub(crate) title_stack: Vec<String>,
    pub(crate) cwd: String,
    pub(crate) theme: Theme,
    pub(crate) palette: [Rgb; 256],
    pub(crate) fg: Rgb,
    pub(crate) bg: Rgb,
    pub(crate) cursor_color: Rgb,
    pub(crate) clusters: Clusters,
    pub(crate) links: Vec<Link>,
    pub(crate) responses: Vec<u8>,
    pub(crate) clipboard: Option<String>,
    pub(crate) bells: u32,
    pub(crate) full_damage: bool,
    pub(crate) drawn_cursor: Option<CursorInfo>,
    pub(crate) selection: Option<(Point, Point)>,
    pub(crate) cell_px: (u16, u16),
    pub(crate) mouse_last: Option<(usize, usize)>,
}

impl Terminal {
    /// `scrollback`: history capacity in lines.
    pub fn new(cols: usize, rows: usize, scrollback: usize, theme: Theme) -> Terminal {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Terminal {
            parser: Parser::default(),
            cols,
            rows,
            main: vec![Row::new(cols, Cell::default()); rows],
            alt: Vec::new(),
            alt_active: false,
            sb: Scrollback::new(scrollback),
            base: 0,
            view: 0,
            cur: Cursor::default(),
            saved: [Cursor::default(); 2],
            top: 0,
            bottom: rows - 1,
            tabs: default_tabs(cols),
            modes: Modes::default(),
            last_char: None,
            title: String::new(),
            title_changed: false,
            title_stack: Vec::new(),
            cwd: String::new(),
            palette: default_palette(&theme),
            fg: theme.fg,
            bg: theme.bg,
            cursor_color: theme.cursor,
            theme,
            clusters: Clusters::default(),
            links: Vec::new(),
            responses: Vec::new(),
            clipboard: None,
            bells: 0,
            full_damage: true,
            drawn_cursor: None,
            selection: None,
            cell_px: (8, 16),
            mouse_last: None,
        }
    }

    /// Processes output of the application; sequences may be split anywhere.
    pub fn feed(&mut self, data: &[u8]) {
        let mut p = std::mem::take(&mut self.parser);
        for &b in data {
            p.advance(self, b);
        }
        self.parser = p;
        self.check_selection();
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn modes(&self) -> &Modes {
        &self.modes
    }

    pub fn is_alt_screen(&self) -> bool {
        self.alt_active
    }

    /// Synchronized output (2026) in progress.
    pub fn sync(&self) -> bool {
        self.modes.sync
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.title_changed)
    }

    /// Working directory from OSC 7, percent-decoded path.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Bells rung since the last call.
    pub fn take_bell(&mut self) -> u32 {
        std::mem::take(&mut self.bells)
    }

    /// Text the application put on the clipboard (OSC 52).
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// Bytes to write back to the PTY (DA, CPR, DECRQM, OSC queries).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Cell size in pixels, for XTWINOPS 14/16.
    pub fn set_cell_size(&mut self, w: u16, h: u16) {
        self.cell_px = (w, h);
    }

    /// Hyperlink URI of a cell's `link`.
    pub fn link(&self, id: u16) -> Option<&str> {
        let i = (id as usize).checked_sub(1)?;
        self.links.get(i).map(|l| &*l.uri)
    }

    /// Text of a cell: its character or grapheme cluster.
    pub fn cell_str<'a>(&'a self, cell: &Cell, buf: &'a mut [u8; 4]) -> &'a str {
        match cell.char() {
            Some(c) => c.encode_utf8(buf),
            None => self.clusters.get(cell.ch),
        }
    }

    pub fn palette(&self) -> &[Rgb; 256] {
        &self.palette
    }

    pub fn default_colors(&self) -> (Rgb, Rgb, Rgb) {
        (self.fg, self.bg, self.cursor_color)
    }

    pub fn resolve(&self, c: Color, fg: bool) -> Rgb {
        match c.kind() {
            ColorKind::Default if fg != self.modes.reverse_video => self.fg,
            ColorKind::Default => self.bg,
            ColorKind::Indexed(i) => self.palette[i as usize],
            ColorKind::Rgb(rgb) => rgb,
        }
    }

    /// Final foreground and background of a cell: reverse, dim and invisible
    /// applied.
    pub fn cell_colors(&self, cell: &Cell) -> (Rgb, Rgb) {
        let (mut fg, mut bg) = (self.resolve(cell.fg, true), self.resolve(cell.bg, false));
        if cell.attrs & attr::REVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.attrs & attr::DIM != 0 {
            let mix = |a: u8, b: u8| ((a as u16 * 2 + b as u16) / 3) as u8;
            fg = Rgb::new(mix(fg.r, bg.r), mix(fg.g, bg.g), mix(fg.b, bg.b));
        }
        if cell.attrs & attr::INVISIBLE != 0 {
            fg = bg;
        }
        (fg, bg)
    }

    pub fn cursor(&self) -> CursorInfo {
        let row = self.cur.y + self.view;
        CursorInfo {
            col: self.cur.x,
            row,
            visible: self.modes.cursor_visible && row < self.rows,
            shape: self.modes.cursor_shape,
            blink: self.modes.cursor_blink,
        }
    }

    pub(crate) fn screen(&self) -> &[Row] {
        if self.alt_active {
            &self.alt
        } else {
            &self.main
        }
    }

    pub(crate) fn grid(&mut self) -> &mut Vec<Row> {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    pub(crate) fn row(&mut self) -> &mut Row {
        let y = self.cur.y;
        &mut self.grid()[y]
    }

    /// Cells of visible row `y`: the screen row itself, or a scrollback line
    /// decoded into `buf` when the view is scrolled back.
    pub fn visible_row<'a>(&'a self, y: usize, buf: &'a mut Vec<Cell>) -> &'a [Cell] {
        if y >= self.view {
            return &self.screen()[(y - self.view).min(self.rows - 1)].cells;
        }
        let i = self.sb.len() - self.view + y;
        scrollback::decode(&self.sb.lines[i], self.cols, &self.clusters, buf);
        buf
    }

    /// Whether visible row `y` continues on the next one.
    pub fn visible_row_wrapped(&self, y: usize) -> bool {
        if y >= self.view {
            return self.screen()[(y - self.view).min(self.rows - 1)].wrapped;
        }
        self.sb.lines[self.sb.len() - self.view + y].first().is_some_and(|f| f & 1 != 0)
    }

    pub fn take_damage(&mut self) -> Damage {
        let mut d = Damage { full: std::mem::take(&mut self.full_damage), ..Damage::default() };
        let view = self.view;
        let rows = self.rows;
        for (y, r) in self.grid().iter_mut().enumerate() {
            if std::mem::take(&mut r.dirty) && y + view < rows {
                d.set(y + view);
            }
        }
        let c = self.cursor();
        if self.drawn_cursor != Some(c) {
            for o in self.drawn_cursor.iter().chain([&c]) {
                if o.row < rows {
                    d.set(o.row);
                }
            }
            self.drawn_cursor = Some(c);
        }
        d
    }

    pub fn view_offset(&self) -> usize {
        self.view
    }

    pub fn scrollback_len(&self) -> usize {
        if self.alt_active {
            0
        } else {
            self.sb.len()
        }
    }

    /// Heap bytes of the compressed scrollback (allocator overhead excluded).
    pub fn scrollback_bytes(&self) -> usize {
        self.sb.bytes()
    }

    /// Heap bytes held, roughly: both screens, the scrollback, grapheme
    /// clusters and pending output (allocator overhead excluded).
    pub fn heap_bytes(&self) -> usize {
        let rows = |g: &Vec<Row>| g.capacity() * std::mem::size_of::<Row>() + g.iter().map(|r| r.cells.capacity() * std::mem::size_of::<Cell>()).sum::<usize>();
        let clusters = self.clusters.list.iter().map(|s| 2 * (s.len() + 16)).sum::<usize>();
        rows(&self.main) + rows(&self.alt) + self.sb.bytes() + clusters + self.responses.capacity() + self.title.capacity()
    }

    /// Scrolls the view into history: positive goes back. While scrolled
    /// back, new output keeps the view on the same lines.
    pub fn scroll_view(&mut self, lines: i32) {
        let max = self.scrollback_len() as i64;
        let v = (self.view as i64 + lines as i64).clamp(0, max) as usize;
        if v != self.view {
            self.view = v;
            self.full_damage = true;
        }
    }

    pub fn reset_view(&mut self) {
        self.scroll_view(i32::MIN);
    }

    pub(crate) fn blank(&self) -> Cell {
        Cell::blank(self.cur.pen.bg)
    }

    pub(crate) fn print(&mut self, c: char) {
        let c = if self.cur.g[self.cur.gl as usize] == Charset::Graphics { dec_graphics(c) } else { c };
        let w = width(c);
        if w == 0 || (!c.is_ascii() && self.joins_prev(c)) {
            self.attach(c);
            return;
        }
        let cols = self.cols;
        if w > cols {
            return;
        }
        if self.cur.pending && self.modes.autowrap {
            self.wrap();
        }
        if w == 2 && self.cur.x == cols - 1 {
            if self.modes.autowrap {
                self.wrap();
            } else {
                self.cur.x -= 1;
            }
        }
        let (x, blank) = (self.cur.x, self.blank());
        let insert = self.modes.insert;
        let pen = Cell { ch: c as u32, ..self.cur.pen };
        let row = self.row();
        if insert {
            row.insert(x, w, blank);
        }
        row.split(x, blank);
        row.split(x + w, blank);
        if w == 2 {
            (row.cells[x], row.cells[x + 1]) = wide_pair(pen);
        } else {
            row.cells[x] = pen;
        }
        row.mark();
        self.advance(w);
        self.last_char = Some(c);
    }

    fn advance(&mut self, w: usize) {
        self.cur.x += w;
        if self.cur.x >= self.cols {
            self.cur.x = self.cols - 1;
            self.cur.pending = self.modes.autowrap;
        }
    }

    fn wrap(&mut self) {
        self.row().wrapped = true;
        self.index();
        self.cur.x = 0;
        self.cur.pending = false;
    }

    /// Column of the cell before the cursor, the one combining marks join.
    fn prev_cell(&self) -> Option<usize> {
        let x = if self.cur.pending {
            self.cur.x
        } else {
            self.cur.x.checked_sub(1)?
        };
        let row = &self.screen()[self.cur.y];
        Some(if row.cells[x].is_spacer() && x > 0 { x - 1 } else { x })
    }

    fn joins_prev(&self, c: char) -> bool {
        let Some(x) = self.prev_cell() else { return false };
        let cell = &self.screen()[self.cur.y].cells[x];
        let mut b = [0; 4];
        let s = match cell.char() {
            Some(ch) => ch.encode_utf8(&mut b),
            None => self.clusters.get(cell.ch),
        };
        joins(s, c)
    }

    /// Appends a character to the cluster of the cell before the cursor.
    fn attach(&mut self, c: char) {
        let Some(x) = self.prev_cell() else { return };
        let (cols, pending) = (self.cols, self.cur.pending);
        let cell = self.screen()[self.cur.y].cells[x];
        let mut s = String::new();
        self.clusters.push_str(&cell, &mut s);
        s.push(c);
        let Some(ch) = self.clusters.intern(&s) else { return };
        let widen = c == '\u{fe0f}' && !cell.is_wide() && width(self.clusters.base(&cell)) == 1 && !pending && x + 1 == self.cur.x && x + 1 < cols;
        let blank = self.blank();
        let row = self.row();
        row.cells[x].ch = ch;
        if widen {
            row.split(x + 2, blank);
            (row.cells[x], row.cells[x + 1]) = wide_pair(row.cells[x]);
        }
        row.mark();
        if widen {
            self.advance(1);
        }
    }

    pub(crate) fn rep(&mut self, n: usize) {
        if let Some(c) = self.last_char {
            for _ in 0..n.min(self.cols * self.rows) {
                self.print(c);
            }
        }
    }

    pub(crate) fn index(&mut self) {
        self.cur.pending = false;
        if self.cur.y == self.bottom {
            self.scroll_up(1);
        } else if self.cur.y + 1 < self.rows {
            self.cur.y += 1;
        }
    }

    pub(crate) fn reverse_index(&mut self) {
        self.cur.pending = false;
        if self.cur.y == self.top {
            self.scroll_down(1);
        } else if self.cur.y > 0 {
            self.cur.y -= 1;
        }
    }

    pub(crate) fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.top, self.bottom);
        let n = n.min(bottom - top + 1);
        let blank = self.blank();
        if !self.alt_active && top == 0 && bottom == self.rows - 1 {
            for r in &self.main[..n] {
                self.sb.push(r, &self.clusters);
            }
            self.base += n as u64;
            if self.view > 0 {
                self.view = (self.view + n).min(self.sb.len());
            }
            self.main.rotate_left(n);
            fill_rows(&mut self.main[bottom + 1 - n..], blank);
            self.full_damage = true;
            return;
        }
        self.shift(top, bottom, n, true);
    }

    pub(crate) fn scroll_down(&mut self, n: usize) {
        self.shift(self.top, self.bottom, n, false);
    }

    /// Moves rows `top..=bottom` up or down by `n`, blanking the vacated ones.
    fn shift(&mut self, top: usize, bottom: usize, n: usize, up: bool) {
        let blank = self.blank();
        let g = &mut self.grid()[top..=bottom];
        let n = n.min(g.len());
        let len = g.len();
        if up {
            g.rotate_left(n);
            fill_rows(&mut g[len - n..], blank);
        } else {
            g.rotate_right(n);
            fill_rows(&mut g[..n], blank);
        }
        g.iter_mut().for_each(Row::mark);
    }

    /// IL / DL: only inside the scroll region.
    pub(crate) fn insert_lines(&mut self, n: usize, delete: bool) {
        if self.cur.y < self.top || self.cur.y > self.bottom {
            return;
        }
        self.shift(self.cur.y, self.bottom, n, delete);
        self.cur.x = 0;
        self.cur.pending = false;
    }

    pub(crate) fn erase_display(&mut self, mode: u16) {
        let blank = self.blank();
        let (x, y) = (self.cur.x, self.cur.y);
        let cols = self.cols;
        match mode {
            0 => {
                let g = self.grid();
                g[y].erase(x, cols, blank);
                g[y].wrapped = false;
                fill_rows(&mut g[y + 1..], blank);
            }
            1 => {
                let g = self.grid();
                g[y].erase(0, x + 1, blank);
                fill_rows(&mut g[..y], blank);
            }
            2 => fill_rows(self.grid(), blank),
            3 if !self.alt_active => {
                self.sb.clear();
                self.view = 0;
                self.full_damage = true;
            }
            _ => {}
        }
    }

    pub(crate) fn erase_line(&mut self, mode: u16) {
        let blank = self.blank();
        let (x, cols) = (self.cur.x, self.cols);
        let row = self.row();
        match mode {
            0 => {
                row.erase(x, cols, blank);
                row.wrapped = false;
            }
            1 => row.erase(0, x + 1, blank),
            2 => row.reset(blank),
            _ => {}
        }
    }

    pub(crate) fn goto(&mut self, x: usize, y: usize) {
        let (lo, hi) = if self.cur.origin { (self.top, self.bottom) } else { (0, self.rows - 1) };
        self.cur.x = x.min(self.cols - 1);
        self.cur.y = (lo + y).min(hi);
        self.cur.pending = false;
    }

    pub(crate) fn move_up(&mut self, n: usize) {
        let lo = if self.cur.y >= self.top { self.top } else { 0 };
        self.cur.y = self.cur.y.saturating_sub(n).max(lo);
        self.cur.pending = false;
    }

    pub(crate) fn move_down(&mut self, n: usize) {
        let hi = if self.cur.y <= self.bottom { self.bottom } else { self.rows - 1 };
        self.cur.y = (self.cur.y + n).min(hi);
        self.cur.pending = false;
    }

    pub(crate) fn set_x(&mut self, x: usize) {
        self.cur.x = x.min(self.cols - 1);
        self.cur.pending = false;
    }

    pub(crate) fn tab(&mut self, n: usize) {
        for _ in 0..n {
            let next = (self.cur.x + 1..self.cols).find(|&x| self.tabs[x]);
            self.cur.x = next.unwrap_or(self.cols - 1);
        }
        self.cur.pending = false;
    }

    pub(crate) fn back_tab(&mut self, n: usize) {
        for _ in 0..n {
            self.cur.x = (0..self.cur.x).rev().find(|&x| self.tabs[x]).unwrap_or(0);
        }
        self.cur.pending = false;
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved[self.alt_active as usize] = self.cur;
    }

    pub(crate) fn restore_cursor(&mut self) {
        let mut c = self.saved[self.alt_active as usize];
        c.x = c.x.min(self.cols - 1);
        c.y = c.y.min(self.rows - 1);
        self.cur = c;
    }

    pub(crate) fn set_alt(&mut self, on: bool, cursor: bool) {
        if on == self.alt_active {
            return;
        }
        if on && cursor {
            self.save_cursor();
        }
        if on {
            self.alt = vec![Row::new(self.cols, Cell::blank(self.cur.pen.bg)); self.rows];
        } else {
            self.alt = Vec::new();
        }
        self.alt_active = on;
        if !on && cursor {
            self.restore_cursor();
        }
        self.view = 0;
        self.selection = None;
        self.full_damage = true;
    }

    pub(crate) fn set_margins(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
            self.goto(0, 0);
        }
    }

    pub(crate) fn soft_reset(&mut self) {
        self.modes.cursor_visible = true;
        self.modes.insert = false;
        self.modes.autowrap = true;
        self.modes.app_cursor = false;
        self.modes.app_keypad = false;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.cur.pen = Cell::default();
        self.cur.origin = false;
        self.cur.g = [Charset::Ascii; 2];
        self.cur.gl = 0;
        self.saved = [Cursor::default(); 2];
    }

    pub(crate) fn full_reset(&mut self) {
        self.set_alt(false, false);
        self.modes = Modes::default();
        self.cur = Cursor::default();
        self.saved = [Cursor::default(); 2];
        self.top = 0;
        self.bottom = self.rows - 1;
        self.tabs = default_tabs(self.cols);
        self.palette = default_palette(&self.theme);
        (self.fg, self.bg, self.cursor_color) = (self.theme.fg, self.theme.bg, self.theme.cursor);
        self.last_char = None;
        fill_rows(&mut self.main, Cell::default());
        self.view = 0;
        self.selection = None;
        self.full_damage = true;
    }

    pub(crate) fn intern_link(&mut self, id: &str, uri: &str) -> u16 {
        if let Some(i) = self.links.iter().rposition(|l| &*l.id == id && &*l.uri == uri && !id.is_empty()) {
            return i as u16 + 1;
        }
        if self.links.len() >= u16::MAX as usize {
            return 0;
        }
        self.links.push(Link { id: id.into(), uri: uri.into() });
        self.links.len() as u16
    }

    /// Resizes without reflow. The main screen keeps the cursor line in view:
    /// rows above it go to the scrollback when shrinking, blank rows below it
    /// are dropped; growing pulls lines back from the scrollback.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let main_cursor = if self.alt_active { self.saved[0].y } else { self.cur.y };
        let shift = self.resize_main_rows(rows, main_cursor);
        if self.alt_active {
            self.saved[0].y = (self.saved[0].y as isize + shift).max(0) as usize;
            self.alt.resize_with(rows, || Row::new(cols, Cell::default()));
        } else {
            self.cur.y = (self.cur.y as isize + shift).max(0) as usize;
        }
        for r in self.main.iter_mut().chain(self.alt.iter_mut()) {
            r.resize(cols);
        }
        let old = self.cols;
        self.tabs.resize(cols, false);
        for x in old..cols {
            self.tabs[x] = x % 8 == 0;
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        for c in [&mut self.cur].into_iter().chain(self.saved.iter_mut()) {
            c.x = c.x.min(cols - 1);
            c.y = c.y.min(rows - 1);
            c.pending = false;
        }
        self.view = self.view.min(self.scrollback_len());
        self.selection = None;
        self.full_damage = true;
    }

    /// Returns how far content moved down (negative: up).
    fn resize_main_rows(&mut self, rows: usize, cy: usize) -> isize {
        let old = self.main.len();
        if rows < old {
            let push = (cy + 1).saturating_sub(rows);
            for r in &self.main[..push] {
                self.sb.push(r, &self.clusters);
            }
            self.base += push as u64;
            self.main.drain(..push);
            self.main.truncate(rows);
            return -(push as isize);
        }
        let pull = if self.alt_active { 0 } else { (rows - old).min(self.sb.len()) };
        for _ in 0..pull {
            let blob = self.sb.pop().unwrap();
            self.main.insert(0, scrollback::decode_row(&blob, self.cols, &self.clusters));
        }
        self.base -= pull as u64;
        self.main.resize_with(rows, || Row::new(self.cols, Cell::default()));
        pull as isize
    }

    pub fn first_line(&self) -> u64 {
        self.base - self.sb.len() as u64
    }

    /// Absolute line of visible row `y`.
    pub fn line_at(&self, y: usize) -> u64 {
        self.base + y as u64 - self.view as u64
    }

    pub(crate) fn check_selection(&mut self) {
        let base = self.base;
        let sel = self.selection;
        let mut clear = false;
        for (y, r) in self.grid().iter_mut().enumerate() {
            if std::mem::take(&mut r.touched) {
                if let Some((a, b)) = sel {
                    let l = base + y as u64;
                    clear |= l >= a.line && l <= b.line;
                }
            }
        }
        if clear {
            self.selection = None;
            self.full_damage = true;
        }
    }
}

pub(crate) fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|x| x % 8 == 0 && x > 0).collect()
}
