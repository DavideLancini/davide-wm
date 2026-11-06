//! Compressed history of the main screen.
//!
//! A line is one boxed byte blob: a flags byte, a varint count of attribute
//! spans (only where the pen changes, starting from the default pen; column,
//! colours, attributes and link, varint-packed, 5 bytes for a typical one)
//! and the UTF-8 text of the cells up to the last non-blank
//! one. Spacer halves of wide characters are not stored; widths are
//! recomputed when decoding.

use std::collections::VecDeque;

use crate::cell::{attr, cluster_width, joins, Cell, Clusters, Color};
use crate::grid::{wide_pair, Row};

const WRAPPED: u8 = 1;
static BLANK: Cell = Cell { ch: ' ' as u32, fg: Color(0), bg: Color(0), attrs: 0, link: 0 };

pub(crate) struct Scrollback {
    pub lines: VecDeque<Box<[u8]>>,
    pub cap: usize,
    bytes: usize,
    scratch: Vec<u8>,
}

impl Scrollback {
    pub fn new(cap: usize) -> Scrollback {
        Scrollback { lines: VecDeque::new(), cap, bytes: 0, scratch: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Heap bytes held: blobs plus the ring's slots.
    pub fn bytes(&self) -> usize {
        self.bytes + self.lines.capacity() * std::mem::size_of::<Box<[u8]>>()
    }

    pub fn clear(&mut self) {
        self.lines = VecDeque::new();
        self.bytes = 0;
    }

    pub fn push(&mut self, row: &Row, clusters: &Clusters) {
        if self.cap == 0 {
            return;
        }
        let mut buf = std::mem::take(&mut self.scratch);
        encode(row, clusters, &mut buf);
        self.push_blob(Box::from(&buf[..]));
        self.scratch = buf;
    }

    pub fn push_blob(&mut self, blob: Box<[u8]>) {
        if self.lines.len() >= self.cap {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
        self.bytes += blob.len();
        self.lines.push_back(blob);
    }

    pub fn pop(&mut self) -> Option<Box<[u8]>> {
        let b = self.lines.pop_back()?;
        self.bytes -= b.len();
        Some(b)
    }
}

fn varint(mut v: usize, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_varint(b: &[u8], pos: &mut usize) -> usize {
    let mut v = 0;
    let mut shift = 0;
    while let Some(&x) = b.get(*pos) {
        *pos += 1;
        v |= ((x & 0x7f) as usize) << shift;
        if x & 0x80 == 0 || shift > 56 {
            break;
        }
        shift += 7;
    }
    v
}

pub(crate) fn encode(row: &Row, clusters: &Clusters, out: &mut Vec<u8>) {
    out.clear();
    out.push(if row.wrapped { WRAPPED } else { 0 });
    let end = row.cells.iter().rposition(|c| *c != Cell::default()).map_or(0, |i| i + 1);
    let cells = &row.cells[..end];
    let pens = || cells.iter().enumerate().filter(|(_, c)| !c.is_spacer()).chain(std::iter::once((end, &BLANK)));
    let mut prev = Cell::default();
    let n = pens().filter(|(_, c)| !std::mem::replace(&mut prev, **c).same_pen(c)).count();
    varint(n, out);
    prev = Cell::default();
    for (x, c) in pens() {
        if c.same_pen(&prev) {
            continue;
        }
        varint(x, out);
        color(c.fg, out);
        color(c.bg, out);
        varint((c.attrs & !attr::LAYOUT) as usize, out);
        varint(c.link as usize, out);
        prev = *c;
    }
    for c in cells.iter().filter(|c| !c.is_spacer()) {
        match c.char() {
            Some(ch) => {
                let mut b = [0; 4];
                out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
            }
            None => out.extend_from_slice(clusters.get(c.ch).as_bytes()),
        }
    }
}

/// Tag byte (the colour kind), then 0, 1 or 3 bytes.
fn color(c: Color, out: &mut Vec<u8>) {
    let kind = (c.0 >> 24) as u8;
    out.push(kind);
    match kind {
        1 => out.push(c.0 as u8),
        2 => out.extend_from_slice(&c.0.to_be_bytes()[1..]),
        _ => {}
    }
}

fn read_color(b: &[u8], pos: &mut usize) -> Color {
    let kind = b.get(*pos).copied().unwrap_or(0);
    *pos += 1;
    let n = match kind {
        1 => 1,
        2 => 3,
        _ => 0,
    };
    let v = b.get(*pos..*pos + n).unwrap_or(&[]).iter().fold(0, |v, &x| v << 8 | x as u32);
    *pos += n;
    Color((kind as u32) << 24 | v)
}

fn read_span(b: &[u8], pos: &mut usize) -> (usize, Cell) {
    let x = read_varint(b, pos);
    let (fg, bg) = (read_color(b, pos), read_color(b, pos));
    let (attrs, link) = (read_varint(b, pos) as u16, read_varint(b, pos) as u16);
    (x, Cell { fg, bg, attrs, link, ..Cell::default() })
}

/// Decodes a line into `cols` cells; returns the wrapped flag.
pub(crate) fn decode(blob: &[u8], cols: usize, clusters: &Clusters, out: &mut Vec<Cell>) -> bool {
    out.clear();
    out.resize(cols, Cell::default());
    let Some(&flags) = blob.first() else { return false };
    let mut pos = 1;
    let nspans = read_varint(blob, &mut pos).min(blob.len());
    let spans_at = pos;
    for _ in 0..nspans {
        read_span(blob, &mut pos);
    }
    let text = std::str::from_utf8(blob.get(pos..).unwrap_or(&[])).unwrap_or("");

    let mut x = 0;
    let mut start = 0;
    let mut place = |s: &str, x: &mut usize| {
        if *x >= cols {
            return;
        }
        let mut it = s.chars();
        let first = it.next().unwrap_or(' ');
        let ch = if it.next().is_none() { first as u32 } else { clusters.find(s).unwrap_or(first as u32) };
        let cell = Cell { ch, ..Cell::default() };
        if cluster_width(s) == 2 {
            if *x + 1 < cols {
                (out[*x], out[*x + 1]) = wide_pair(cell);
                *x += 2;
                return;
            }
            if !s.contains('\u{fe0f}') {
                *x += 1;
                return;
            }
        }
        out[*x] = cell;
        *x += 1;
    };
    for (i, c) in text.char_indices() {
        if i > start && !joins(&text[start..i], c) {
            place(&text[start..i], &mut x);
            start = i;
        }
    }
    if start < text.len() {
        place(&text[start..], &mut x);
    }

    let mut apply = |from: usize, to: usize, pen: Cell| {
        for c in out.iter_mut().take(to.min(cols)).skip(from) {
            c.fg = pen.fg;
            c.bg = pen.bg;
            c.attrs = (c.attrs & attr::LAYOUT) | pen.attrs;
            c.link = pen.link;
        }
    };
    let mut pos = spans_at;
    let mut prev: Option<(usize, Cell)> = None;
    for _ in 0..nspans {
        let (x, pen) = read_span(blob, &mut pos);
        if let Some((from, p)) = prev {
            apply(from, x, p);
        }
        prev = Some((x, pen));
    }
    if let Some((from, p)) = prev {
        apply(from, cols, p);
    }
    flags & WRAPPED != 0
}

pub(crate) fn decode_row(blob: &[u8], cols: usize, clusters: &Clusters) -> Row {
    let mut cells = Vec::with_capacity(cols);
    let wrapped = decode(blob, cols, clusters, &mut cells);
    Row { cells, wrapped, dirty: true, touched: true }
}
