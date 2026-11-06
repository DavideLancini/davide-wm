use std::collections::HashMap;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }
}

/// A colour packed in 32 bits: the top byte says default, indexed or RGB.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Color(pub(crate) u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorKind {
    Default,
    Indexed(u8),
    Rgb(Rgb),
}

impl Color {
    pub const DEFAULT: Color = Color(0);

    pub const fn indexed(i: u8) -> Color {
        Color(1 << 24 | i as u32)
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(2 << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32)
    }

    pub fn kind(self) -> ColorKind {
        match self.0 >> 24 {
            1 => ColorKind::Indexed(self.0 as u8),
            2 => ColorKind::Rgb(Rgb::new((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)),
            _ => ColorKind::Default,
        }
    }
}

/// Bits of [`Cell::attrs`].
pub mod attr {
    pub const BOLD: u16 = 1;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const REVERSE: u16 = 1 << 4;
    pub const INVISIBLE: u16 = 1 << 5;
    pub const STRIKE: u16 = 1 << 6;
    /// First half of a two-column character.
    pub const WIDE: u16 = 1 << 7;
    /// Second half of a two-column character; draw nothing.
    pub const SPACER: u16 = 1 << 8;
    pub(crate) const LAYOUT: u16 = WIDE | SPACER;
}

/// `ch` values from here on index the grapheme cluster table.
pub(crate) const CLUSTER: u32 = 0x11_0000;
const MAX_CLUSTERS: usize = 1 << 16;

/// One screen cell, 16 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Cell {
    pub(crate) ch: u32,
    pub fg: Color,
    pub bg: Color,
    pub attrs: u16,
    /// Hyperlink: 0 for none, else an id for [`crate::Terminal::link`].
    pub link: u16,
}

impl Default for Cell {
    fn default() -> Cell {
        Cell { ch: ' ' as u32, fg: Color::DEFAULT, bg: Color::DEFAULT, attrs: 0, link: 0 }
    }
}

impl Cell {
    /// The character, or `None` for a multi-codepoint cluster (see
    /// [`crate::Terminal::cell_str`]).
    pub fn char(&self) -> Option<char> {
        char::from_u32(self.ch)
    }

    pub fn is_wide(&self) -> bool {
        self.attrs & attr::WIDE != 0
    }

    pub fn is_spacer(&self) -> bool {
        self.attrs & attr::SPACER != 0
    }

    pub(crate) fn blank(bg: Color) -> Cell {
        Cell { bg, ..Cell::default() }
    }

    pub(crate) fn same_pen(&self, o: &Cell) -> bool {
        self.fg == o.fg && self.bg == o.bg && self.link == o.link && (self.attrs ^ o.attrs) & !attr::LAYOUT == 0
    }
}

pub(crate) fn width(c: char) -> usize {
    if (' '..'\x7f').contains(&c) {
        1
    } else {
        c.width().unwrap_or(0)
    }
}

/// Columns of a cluster: its base character, widened by VS16 (emoji
/// presentation) as Ink and most terminals count it.
pub(crate) fn cluster_width(s: &str) -> usize {
    let mut it = s.chars();
    let w = it.next().map_or(1, width);
    if w == 1 && it.any(|c| c == '\u{fe0f}') {
        2
    } else {
        w
    }
}

/// Whether `c` extends `cluster` instead of taking a cell of its own:
/// combining marks, after ZWJ, emoji modifiers, the second regional
/// indicator of a flag.
pub(crate) fn joins(cluster: &str, c: char) -> bool {
    if c.is_ascii() {
        return false;
    }
    let ri = |c: char| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c);
    let mut it = cluster.chars();
    let first = it.next();
    width(c) == 0
        || cluster.ends_with('\u{200d}')
        || ('\u{1f3fb}'..='\u{1f3ff}').contains(&c)
        || (ri(c) && first.is_some_and(ri) && it.next().is_none())
}

/// Interned grapheme clusters, referenced from cells; never shrinks.
#[derive(Default)]
pub(crate) struct Clusters {
    pub list: Vec<Box<str>>,
    map: HashMap<Box<str>, u32>,
}

impl Clusters {
    pub fn get(&self, ch: u32) -> &str {
        self.list.get((ch - CLUSTER) as usize).map_or("\u{fffd}", |s| s)
    }

    pub fn find(&self, s: &str) -> Option<u32> {
        self.map.get(s).map(|i| i + CLUSTER)
    }

    pub fn intern(&mut self, s: &str) -> Option<u32> {
        if let Some(i) = self.find(s) {
            return Some(i);
        }
        if self.list.len() >= MAX_CLUSTERS {
            return None;
        }
        let i = self.list.len() as u32;
        self.list.push(s.into());
        self.map.insert(s.into(), i);
        Some(i + CLUSTER)
    }

    pub fn from_list(list: Vec<Box<str>>) -> Clusters {
        let map = list.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        Clusters { list, map }
    }

    /// Appends the text of `cell` to `out`.
    pub fn push_str(&self, cell: &Cell, out: &mut String) {
        match cell.char() {
            Some(c) => out.push(c),
            None => out.push_str(self.get(cell.ch)),
        }
    }

    pub fn base(&self, cell: &Cell) -> char {
        cell.char().unwrap_or_else(|| self.get(cell.ch).chars().next().unwrap_or(' '))
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Theme {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub base: [Rgb; 16],
}

impl Default for Theme {
    fn default() -> Theme {
        let h = |v: u32| Rgb::new((v >> 16) as u8, (v >> 8) as u8, v as u8);
        let base = [
            0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5, 0x7f7f7f, 0xff0000, 0x00ff00, 0xffff00, 0x5c5cff,
            0xff00ff, 0x00ffff, 0xffffff,
        ]
        .map(h);
        Theme { fg: h(0xe5e5e5), bg: h(0x000000), cursor: h(0xe5e5e5), base }
    }
}

pub(crate) fn default_palette(theme: &Theme) -> [Rgb; 256] {
    let mut p = [Rgb::default(); 256];
    p[..16].copy_from_slice(&theme.base);
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
    for i in 0..216 {
        p[16 + i] = Rgb::new(level(i / 36), level(i / 6 % 6), level(i % 6));
    }
    for i in 0..24 {
        let v = (8 + i * 10) as u8;
        p[232 + i] = Rgb::new(v, v, v);
    }
    p
}

/// DEC special graphics for 0x5f..=0x7e.
pub(crate) fn dec_graphics(c: char) -> char {
    const MAP: [char; 32] = [
        ' ', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼', '⎺', '⎻', '─', '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤',
        '≥', 'π', '≠', '£', '·',
    ];
    match c {
        '\x5f'..='\x7e' => MAP[c as usize - 0x5f],
        _ => c,
    }
}
