//! Control functions: what each parsed sequence does to the terminal.

use std::io::Write;

use crate::cell::{attr, default_palette, Cell, Color, Rgb};
use crate::parser::{Params, Perform};
use crate::terminal::{Charset, CursorShape, MouseEncoding, MouseMode, Terminal};

const MAX_TITLE: usize = 4096;

macro_rules! reply {
    ($t:expr, $($a:tt)*) => {{
        let _ = write!($t.responses, $($a)*);
    }};
}

impl Perform for Terminal {
    fn print(&mut self, c: char) {
        Terminal::print(self, c);
    }

    fn execute(&mut self, b: u8) {
        match b {
            0x07 => self.bells = self.bells.saturating_add(1),
            0x08 => {
                self.cur.pending = false;
                self.cur.x = self.cur.x.saturating_sub(1);
            }
            0x09 => self.tab(1),
            0x0a..=0x0c => {
                self.index();
                if self.modes.newline {
                    self.cur.x = 0;
                }
            }
            0x0d => {
                self.cur.x = 0;
                self.cur.pending = false;
            }
            0x0e => self.cur.gl = 1,
            0x0f => self.cur.gl = 0,
            _ => {}
        }
    }

    fn esc(&mut self, inter: &[u8], fin: u8) {
        match (inter, fin) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.index(),
            ([], b'E') => {
                self.index();
                self.cur.x = 0;
            }
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.full_reset(),
            ([], b'=') => self.modes.app_keypad = true,
            ([], b'>') => self.modes.app_keypad = false,
            ([], b'H') => {
                let x = self.cur.x;
                self.tabs[x] = true;
            }
            ([g @ (b'(' | b')')], _) => {
                self.cur.g[(*g == b')') as usize] = if fin == b'0' { Charset::Graphics } else { Charset::Ascii };
            }
            ([b'#'], b'8') => {
                let e = Cell { ch: 'E' as u32, ..Cell::default() };
                for r in self.grid() {
                    r.cells.fill(e);
                    r.wrapped = false;
                    r.mark();
                }
                self.top = 0;
                self.bottom = self.rows - 1;
                self.goto(0, 0);
            }
            _ => {}
        }
    }

    fn csi(&mut self, p: &Params, inter: &[u8], fin: u8) {
        let n = p.get(0, 1) as usize;
        let (x, y) = (self.cur.x, self.cur.y);
        let blank = self.blank();
        match (inter, fin) {
            ([], b'A') => self.move_up(n),
            ([], b'B' | b'e') => self.move_down(n),
            ([], b'C' | b'a') => self.set_x(x + n),
            ([], b'D') => self.set_x(x.saturating_sub(n)),
            ([], b'E') => {
                self.move_down(n);
                self.cur.x = 0;
            }
            ([], b'F') => {
                self.move_up(n);
                self.cur.x = 0;
            }
            ([], b'G' | b'`') => self.set_x(n - 1),
            ([], b'H' | b'f') => self.goto(p.get(1, 1) as usize - 1, n - 1),
            ([], b'd') => self.goto(x, n - 1),
            ([], b'I') => self.tab(n),
            ([], b'Z') => self.back_tab(n),
            ([] | [b'?'], b'J') => self.erase_display(p.raw(0).unwrap_or(0)),
            ([] | [b'?'], b'K') => self.erase_line(p.raw(0).unwrap_or(0)),
            ([], b'L') => self.insert_lines(n, false),
            ([], b'M') => self.insert_lines(n, true),
            ([], b'@') => {
                self.row().insert(x, n, blank);
                self.cur.pending = false;
            }
            ([], b'P') => {
                self.row().delete(x, n, blank);
                self.cur.pending = false;
            }
            ([], b'X') => {
                self.row().erase(x, x + n, blank);
                self.cur.pending = false;
            }
            ([], b'S') => self.scroll_up(n),
            ([], b'T') if p.groups().count() <= 1 => self.scroll_down(n),
            ([], b'b') => self.rep(n),
            ([], b'c') if p.get(0, 0) == 0 => reply!(self, "\x1b[?62;22c"),
            ([b'>'], b'c') if p.get(0, 0) == 0 => reply!(self, "\x1b[>1;10;0c"),
            ([], b'g') => match p.raw(0).unwrap_or(0) {
                0 => self.tabs[x] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            ([], b'h' | b'l') => {
                for g in p.groups() {
                    self.ansi_mode(g[0], fin == b'h');
                }
            }
            ([b'?'], b'h' | b'l') => {
                for g in p.groups() {
                    self.private_mode(g[0], fin == b'h');
                }
            }
            ([], b'm') => self.sgr(p),
            ([], b'n') => match p.get(0, 0) {
                5 => reply!(self, "\x1b[0n"),
                6 => {
                    let row = y.saturating_sub(if self.cur.origin { self.top } else { 0 });
                    reply!(self, "\x1b[{};{}R", row + 1, x + 1);
                }
                _ => {}
            },
            ([b'?'], b'n') if p.get(0, 0) == 6 => {
                let row = y.saturating_sub(if self.cur.origin { self.top } else { 0 });
                reply!(self, "\x1b[?{};{};1R", row + 1, x + 1);
            }
            ([], b'r') => self.set_margins(p.get(0, 1) as usize - 1, p.get(1, self.rows as u16) as usize - 1),
            ([], b's') if p.is_empty() => self.save_cursor(),
            ([], b'u') if p.is_empty() => self.restore_cursor(),
            ([], b't') => self.window_op(p),
            ([b' '], b'q') => {
                let v = p.get(0, 0);
                self.modes.cursor_blink = v == 0 || v % 2 == 1;
                self.modes.cursor_shape = match v {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
            }
            ([b'>'], b'q') => reply!(self, "\x1bP>|davide-term {}\x1b\\", env!("CARGO_PKG_VERSION")),
            ([b'?'], b'u') => reply!(self, "\x1b[?0u"),
            ([b'?', b'$'], b'p') => {
                let m = p.get(0, 0);
                let v = self.private_mode_state(m);
                reply!(self, "\x1b[?{m};{v}$y");
            }
            ([b'$'], b'p') => {
                let m = p.get(0, 0);
                let v = match m {
                    4 => 2 - self.modes.insert as u8,
                    20 => 2 - self.modes.newline as u8,
                    _ => 0,
                };
                reply!(self, "\x1b[{m};{v}$y");
            }
            ([b'!'], b'p') => self.soft_reset(),
            _ => {}
        }
    }

    fn osc(&mut self, data: &[u8], bel: bool) {
        let st = if bel { "\x07" } else { "\x1b\\" };
        let (num, rest) = match data.iter().position(|&b| b == b';') {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &[][..]),
        };
        let Some(num) = std::str::from_utf8(num).ok().and_then(|s| s.parse::<u16>().ok()) else { return };
        let text = String::from_utf8_lossy(rest);
        match num {
            0 | 2 => {
                let t: String = text.chars().filter(|c| !c.is_control()).take(MAX_TITLE).collect();
                if t != self.title {
                    self.title = t;
                    self.title_changed = true;
                }
            }
            7 => {
                let path = text.split_once("://").map_or(&*text, |(_, r)| r.find('/').map_or("", |i| &r[i..]));
                self.cwd = percent_decode(path);
            }
            8 => {
                let (params, uri) = text.split_once(';').unwrap_or((&text, ""));
                self.cur.pen.link = if uri.is_empty() {
                    0
                } else {
                    let id = params.split(':').find_map(|kv| kv.strip_prefix("id=")).unwrap_or("");
                    self.intern_link(id, uri)
                };
            }
            52 => {
                let data = text.split_once(';').map_or("", |(_, d)| d);
                if data != "?" {
                    if let Some(b) = base64(data) {
                        self.clipboard = Some(String::from_utf8_lossy(&b).into_owned());
                    }
                }
            }
            4 => {
                let mut it = text.split(';');
                while let (Some(i), Some(spec)) = (it.next(), it.next()) {
                    let Ok(i) = i.parse::<u8>() else { continue };
                    if spec == "?" {
                        let c = self.palette[i as usize];
                        reply!(self, "\x1b]4;{i};{}{st}", rgb_spec(c));
                    } else if let Some(c) = parse_color(spec) {
                        self.palette[i as usize] = c;
                        self.full_damage = true;
                    }
                }
            }
            10..=12 => {
                for (k, spec) in text.split(';').enumerate() {
                    let code = num as usize + k;
                    if code > 12 {
                        break;
                    }
                    let slot = match code {
                        10 => &mut self.fg,
                        11 => &mut self.bg,
                        _ => &mut self.cursor_color,
                    };
                    if spec == "?" {
                        let c = *slot;
                        reply!(self, "\x1b]{code};{}{st}", rgb_spec(c));
                    } else if let Some(c) = parse_color(spec) {
                        *slot = c;
                        self.full_damage = true;
                    }
                }
            }
            104 => {
                let def = default_palette(&self.theme);
                if text.is_empty() {
                    self.palette = def;
                } else {
                    for i in text.split(';').filter_map(|s| s.parse::<u8>().ok()) {
                        self.palette[i as usize] = def[i as usize];
                    }
                }
                self.full_damage = true;
            }
            110 => {
                self.fg = self.theme.fg;
                self.full_damage = true;
            }
            111 => {
                self.bg = self.theme.bg;
                self.full_damage = true;
            }
            112 => self.cursor_color = self.theme.cursor,
            _ => {}
        }
    }
}

impl Terminal {
    fn ansi_mode(&mut self, m: u16, on: bool) {
        match m {
            4 => self.modes.insert = on,
            20 => self.modes.newline = on,
            _ => {}
        }
    }

    fn private_mode(&mut self, m: u16, on: bool) {
        let mouse = |t: &mut Terminal, mode| t.modes.mouse = if on { mode } else { MouseMode::Off };
        let enc = |t: &mut Terminal, e| {
            if on {
                t.modes.mouse_encoding = e;
            } else if t.modes.mouse_encoding == e {
                t.modes.mouse_encoding = MouseEncoding::X10;
            }
        };
        match m {
            1 => self.modes.app_cursor = on,
            5 => {
                self.modes.reverse_video = on;
                self.full_damage = true;
            }
            6 => {
                self.cur.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            9 => mouse(self, MouseMode::X10),
            12 => self.modes.cursor_blink = on,
            25 => self.modes.cursor_visible = on,
            47 | 1047 => self.set_alt(on, false),
            1048 => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => self.set_alt(on, true),
            1000 => mouse(self, MouseMode::Click),
            1002 => mouse(self, MouseMode::Drag),
            1003 => mouse(self, MouseMode::Motion),
            1004 => self.modes.focus_events = on,
            1005 => enc(self, MouseEncoding::Utf8),
            1006 => enc(self, MouseEncoding::Sgr),
            1015 => enc(self, MouseEncoding::Urxvt),
            1007 => self.modes.alt_scroll = on,
            2004 => self.modes.bracketed_paste = on,
            2026 => self.modes.sync = on,
            _ => {}
        }
    }

    /// DECRQM: 1 set, 2 reset, 0 not recognised.
    fn private_mode_state(&self, m: u16) -> u8 {
        let md = &self.modes;
        let v = match m {
            1 => md.app_cursor,
            5 => md.reverse_video,
            6 => self.cur.origin,
            7 => md.autowrap,
            12 => md.cursor_blink,
            25 => md.cursor_visible,
            47 | 1047 | 1049 => self.alt_active,
            9 => md.mouse == MouseMode::X10,
            1000 => md.mouse == MouseMode::Click,
            1002 => md.mouse == MouseMode::Drag,
            1003 => md.mouse == MouseMode::Motion,
            1004 => md.focus_events,
            1005 => md.mouse_encoding == MouseEncoding::Utf8,
            1006 => md.mouse_encoding == MouseEncoding::Sgr,
            1015 => md.mouse_encoding == MouseEncoding::Urxvt,
            1007 => md.alt_scroll,
            2004 => md.bracketed_paste,
            2026 => md.sync,
            _ => return 0,
        };
        2 - v as u8
    }

    fn window_op(&mut self, p: &Params) {
        let (cw, ch) = (self.cell_px.0 as usize, self.cell_px.1 as usize);
        match p.get(0, 0) {
            14 => reply!(self, "\x1b[4;{};{}t", ch * self.rows, cw * self.cols),
            16 => reply!(self, "\x1b[6;{ch};{cw}t"),
            18 => reply!(self, "\x1b[8;{};{}t", self.rows, self.cols),
            22 => {
                if self.title_stack.len() < 16 {
                    self.title_stack.push(self.title.clone());
                }
            }
            23 => {
                if let Some(t) = self.title_stack.pop() {
                    self.title_changed |= t != self.title;
                    self.title = t;
                }
            }
            _ => {}
        }
    }

    fn sgr(&mut self, p: &Params) {
        if p.is_empty() {
            self.cur.pen = Cell { link: self.cur.pen.link, ..Cell::default() };
            return;
        }
        let mut groups: [&[u16]; 32] = [&[]; 32];
        let mut len = 0;
        for g in p.groups() {
            groups[len] = g;
            len += 1;
        }
        let groups = &groups[..len];
        let pen = &mut self.cur.pen;
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            i += 1;
            match g[0] {
                0 => *pen = Cell { link: pen.link, ..Cell::default() },
                1 => pen.attrs |= attr::BOLD,
                2 => pen.attrs |= attr::DIM,
                3 => pen.attrs |= attr::ITALIC,
                4 if g.get(1) == Some(&0) => pen.attrs &= !attr::UNDERLINE,
                4 | 21 => pen.attrs |= attr::UNDERLINE,
                7 => pen.attrs |= attr::REVERSE,
                8 => pen.attrs |= attr::INVISIBLE,
                9 => pen.attrs |= attr::STRIKE,
                22 => pen.attrs &= !(attr::BOLD | attr::DIM),
                23 => pen.attrs &= !attr::ITALIC,
                24 => pen.attrs &= !attr::UNDERLINE,
                27 => pen.attrs &= !attr::REVERSE,
                28 => pen.attrs &= !attr::INVISIBLE,
                29 => pen.attrs &= !attr::STRIKE,
                v @ 30..=37 => pen.fg = Color::indexed((v - 30) as u8),
                v @ 40..=47 => pen.bg = Color::indexed((v - 40) as u8),
                v @ 90..=97 => pen.fg = Color::indexed((v - 90 + 8) as u8),
                v @ 100..=107 => pen.bg = Color::indexed((v - 100 + 8) as u8),
                39 => pen.fg = Color::DEFAULT,
                49 => pen.bg = Color::DEFAULT,
                v @ (38 | 48 | 58) => {
                    let c = if g.len() > 1 {
                        extended_color(&g[1..])
                    } else {
                        let mut rest = [0; 4];
                        let k = (groups.len() - i).min(4);
                        for (r, g) in rest.iter_mut().zip(&groups[i..i + k]) {
                            *r = g[0];
                        }
                        let (c, used) = extended_color_semi(&rest[..k]);
                        i += used;
                        c
                    };
                    match (v, c) {
                        (38, Some(c)) => pen.fg = c,
                        (48, Some(c)) => pen.bg = c,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// `5:n`, `2:r:g:b` or `2:cs:r:g:b` (colon form).
fn extended_color(v: &[u16]) -> Option<Color> {
    match v {
        [5, n, ..] => Some(Color::indexed(*n as u8)),
        [2, _, r, g, b, ..] => Some(Color::rgb(*r as u8, *g as u8, *b as u8)),
        [2, r, g, b] => Some(Color::rgb(*r as u8, *g as u8, *b as u8)),
        _ => None,
    }
}

/// `5;n` or `2;r;g;b`; returns the colour and the groups consumed.
fn extended_color_semi(v: &[u16]) -> (Option<Color>, usize) {
    match v {
        [5, n, ..] => (Some(Color::indexed(*n as u8)), 2),
        [2, r, g, b, ..] => (Some(Color::rgb(*r as u8, *g as u8, *b as u8)), 4),
        [5] | [2, ..] => (None, v.len()),
        _ => (None, 0),
    }
}

fn rgb_spec(c: Rgb) -> String {
    format!("rgb:{:04x}/{:04x}/{:04x}", c.r as u16 * 257, c.g as u16 * 257, c.b as u16 * 257)
}

/// `rgb:r/g/b` (1–4 hex digits each) or `#rgb` .. `#rrrrggggbbbb`.
pub(crate) fn parse_color(s: &str) -> Option<Rgb> {
    let comp = |h: &str| -> Option<u8> {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = (1u32 << (4 * h.len())) - 1;
        Some((v * 255 / max) as u8)
    };
    if let Some(r) = s.strip_prefix("rgb:") {
        let mut it = r.split('/');
        let c = Rgb::new(comp(it.next()?)?, comp(it.next()?)?, comp(it.next()?)?);
        return it.next().is_none().then_some(c);
    }
    let h = s.strip_prefix('#')?;
    if h.is_empty() || h.len() % 3 != 0 || h.len() > 12 || !h.is_ascii() {
        return None;
    }
    let k = h.len() / 3;
    Some(Rgb::new(comp(&h[..k])?, comp(&h[k..2 * k])?, comp(&h[2 * k..])?))
}

pub(crate) fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
