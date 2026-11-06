//! Live-upgrade serialisation of a whole terminal. The parser restarts in
//! the ground state; selection, view offset and pending replies are dropped.

use state::{load_bytes, persist_struct, save_bytes, Dec, Enc, Error, Persist, Result};

use crate::cell::{Cell, Clusters, Color, Rgb, Theme};
use crate::grid::Row;
use crate::parser::Parser;
use crate::scrollback::Scrollback;
use crate::terminal::{Charset, Cursor, CursorShape, Link, Modes, MouseEncoding, MouseMode, Terminal};

const VERSION: u8 = 1;

persist_struct!(Rgb { r, g, b });

impl Persist for Theme {
    fn save(&self, e: &mut Enc) {
        (self.fg, self.bg, self.cursor).save(e);
        self.base.to_vec().save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let (fg, bg, cursor) = Persist::load(d)?;
        let base = Vec::<Rgb>::load(d)?.try_into().map_err(|_| Error("palette size".into()))?;
        Ok(Theme { fg, bg, cursor, base })
    }
}

impl Persist for Cell {
    fn save(&self, e: &mut Enc) {
        (self.ch, self.fg.0, self.bg.0, self.attrs, self.link).save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let (ch, fg, bg, attrs, link) = Persist::load(d)?;
        Ok(Cell { ch, fg: Color(fg), bg: Color(bg), attrs, link })
    }
}

impl Persist for Row {
    fn save(&self, e: &mut Enc) {
        self.wrapped.save(e);
        self.cells.save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        Ok(Row { wrapped: Persist::load(d)?, cells: Persist::load(d)?, dirty: true, touched: false })
    }
}

macro_rules! persist_enum {
    ($t:ident { $($v:ident),* $(,)? }) => {
        impl Persist for $t {
            fn save(&self, e: &mut Enc) {
                (*self as u8).save(e)
            }
            fn load(d: &mut Dec) -> Result<Self> {
                let n = u8::load(d)?;
                [$($t::$v),*].get(n as usize).copied().ok_or_else(|| Error(format!("bad {}", stringify!($t))))
            }
        }
    };
}

persist_enum!(Charset { Ascii, Graphics });
persist_enum!(CursorShape { Block, Underline, Bar });
persist_enum!(MouseMode { Off, X10, Click, Drag, Motion });
persist_enum!(MouseEncoding { X10, Utf8, Sgr, Urxvt });

impl Persist for Cursor {
    fn save(&self, e: &mut Enc) {
        (self.x, self.y, self.pen, self.pending, self.origin).save(e);
        (self.g[0], self.g[1], self.gl).save(e);
    }
    fn load(d: &mut Dec) -> Result<Self> {
        let (x, y, pen, pending, origin) = Persist::load(d)?;
        let (g0, g1, gl) = Persist::load(d)?;
        Ok(Cursor { x, y, pen, pending, origin, g: [g0, g1], gl })
    }
}

persist_struct!(Modes {
    app_cursor,
    app_keypad,
    autowrap,
    cursor_visible,
    cursor_shape,
    cursor_blink,
    reverse_video,
    insert,
    newline,
    bracketed_paste,
    focus_events,
    sync,
    alt_scroll,
    mouse,
    mouse_encoding,
});

impl Persist for Terminal {
    fn save(&self, e: &mut Enc) {
        VERSION.save(e);
        (self.cols, self.rows, self.alt_active, self.base).save(e);
        self.main.save(e);
        self.alt.save(e);
        self.sb.cap.save(e);
        self.sb.lines.len().save(e);
        for l in &self.sb.lines {
            save_bytes(l, e);
        }
        (self.cur, self.saved[0], self.saved[1], self.top, self.bottom).save(e);
        self.tabs.save(e);
        self.modes.save(e);
        self.title.save(e);
        self.cwd.save(e);
        self.title_stack.save(e);
        self.theme.save(e);
        self.palette.to_vec().save(e);
        (self.fg, self.bg, self.cursor_color).save(e);
        self.clusters.list.iter().map(|s| s.to_string()).collect::<Vec<_>>().save(e);
        self.links.iter().map(|l| (l.id.to_string(), l.uri.to_string())).collect::<Vec<_>>().save(e);
        (self.cell_px.0, self.cell_px.1).save(e);
    }

    fn load(d: &mut Dec) -> Result<Self> {
        let v = u8::load(d)?;
        if v != VERSION {
            return Err(Error(format!("terminal version {v}")));
        }
        let (cols, rows, alt_active, base): (usize, usize, bool, u64) = Persist::load(d)?;
        let main: Vec<Row> = Persist::load(d)?;
        let alt: Vec<Row> = Persist::load(d)?;
        let screen_ok = |g: &Vec<Row>| g.len() == rows && g.iter().all(|r| r.cells.len() == cols);
        if cols == 0 || rows == 0 || !screen_ok(&main) || (alt_active && !screen_ok(&alt)) {
            return Err(Error("terminal geometry".into()));
        }
        let mut sb = Scrollback::new(Persist::load(d)?);
        for _ in 0..usize::load(d)? {
            sb.push_blob(load_bytes(d)?.into_boxed_slice());
        }
        let (cur, s0, s1, top, bottom): (Cursor, Cursor, Cursor, usize, usize) = Persist::load(d)?;
        let tabs: Vec<bool> = Persist::load(d)?;
        let modes = Persist::load(d)?;
        let title = Persist::load(d)?;
        let cwd = Persist::load(d)?;
        let title_stack = Persist::load(d)?;
        let theme = Persist::load(d)?;
        let palette = Vec::<Rgb>::load(d)?.try_into().map_err(|_| Error("palette size".into()))?;
        let (fg, bg, cursor_color) = Persist::load(d)?;
        let clusters = Clusters::from_list(Vec::<String>::load(d)?.into_iter().map(String::into_boxed_str).collect());
        let links = Vec::<(String, String)>::load(d)?.into_iter().map(|(id, uri)| Link { id: id.into(), uri: uri.into() }).collect();
        let cell_px = Persist::load(d)?;
        let ok = |c: &Cursor| c.x < cols && c.y < rows;
        if !ok(&cur) || !ok(&s0) || !ok(&s1) || top >= bottom || bottom >= rows || tabs.len() != cols || base < sb.len() as u64 {
            return Err(Error("terminal cursor".into()));
        }
        let mut t = Terminal::new(cols, rows, 0, theme);
        t.parser = Parser::default();
        t.main = main;
        t.alt = if alt_active { alt } else { Vec::new() };
        t.alt_active = alt_active;
        t.sb = sb;
        t.base = base;
        t.cur = cur;
        t.saved = [s0, s1];
        t.top = top;
        t.bottom = bottom;
        t.tabs = tabs;
        t.modes = modes;
        t.title = title;
        t.cwd = cwd;
        t.title_stack = title_stack;
        t.palette = palette;
        (t.fg, t.bg, t.cursor_color) = (fg, bg, cursor_color);
        t.clusters = clusters;
        t.links = links;
        t.cell_px = cell_px;
        Ok(t)
    }
}
