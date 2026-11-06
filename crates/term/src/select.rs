//! Text selection in absolute line numbering (see [`Point`]).

use crate::cell::Cell;
use crate::scrollback;
use crate::terminal::{Point, SelectionMode, Terminal};

const DELIMITERS: &str = " \t,│`|\"'()[]{}<>";

impl Terminal {
    /// Absolute point under visible cell (`col`, `row`).
    pub fn point_at(&self, col: usize, row: usize) -> Point {
        Point { line: self.line_at(row.min(self.rows - 1)), col: col.min(self.cols - 1) }
    }

    /// Cells and wrapped flag of an absolute line still in memory.
    fn line(&self, line: u64, buf: &mut Vec<Cell>) -> Option<bool> {
        if line >= self.base {
            let r = self.screen().get((line - self.base) as usize)?;
            buf.clear();
            buf.extend_from_slice(&r.cells);
            return Some(r.wrapped);
        }
        if self.alt_active {
            return None;
        }
        let back = (self.base - line) as usize;
        let i = self.sb.len().checked_sub(back)?;
        Some(scrollback::decode(&self.sb.lines[i], self.cols, &self.clusters, buf))
    }

    fn last_line(&self) -> u64 {
        self.base + self.rows as u64 - 1
    }

    /// Selects from `a` to `b` (inclusive, either order), expanded to words
    /// or whole (soft-wrapped) lines by `mode`.
    pub fn set_selection(&mut self, a: Point, b: Point, mode: SelectionMode) {
        let lo = if self.alt_active { self.base } else { self.first_line() };
        let clamp = |p: Point| Point { line: p.line.clamp(lo, self.last_line()), col: p.col.min(self.cols - 1) };
        let (mut s, mut e) = (clamp(a.min(b)), clamp(a.max(b)));
        let mut buf = Vec::new();
        match mode {
            SelectionMode::Char => {}
            SelectionMode::Word => {
                if self.line(s.line, &mut buf).is_some() && self.is_word(&buf[s.col]) {
                    while s.col > 0 && self.is_word(&buf[s.col - 1]) {
                        s.col -= 1;
                    }
                }
                if self.line(e.line, &mut buf).is_some() && self.is_word(&buf[e.col]) {
                    while e.col + 1 < buf.len() && self.is_word(&buf[e.col + 1]) {
                        e.col += 1;
                    }
                }
            }
            SelectionMode::Line => {
                while s.line > lo && self.line(s.line - 1, &mut buf) == Some(true) {
                    s.line -= 1;
                }
                while e.line < self.last_line() && self.line(e.line, &mut buf) == Some(true) {
                    e.line += 1;
                }
                s.col = 0;
                e.col = self.cols - 1;
            }
        }
        self.selection = Some((s, e));
        for r in self.grid() {
            r.touched = false;
        }
        self.full_damage = true;
    }

    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.full_damage = true;
        }
    }

    pub fn selection(&self) -> Option<(Point, Point)> {
        self.selection
    }

    fn is_word(&self, c: &Cell) -> bool {
        c.is_spacer() || !DELIMITERS.contains(self.clusters.base(c))
    }

    /// Whether visible cell (`col`, `row`) is selected.
    pub fn is_selected(&self, col: usize, row: usize) -> bool {
        self.selection.is_some_and(|(s, e)| (s..=e).contains(&Point { line: self.line_at(row), col }))
    }

    /// Selected text: soft-wrapped lines joined, trailing blanks of hard
    /// line ends trimmed.
    pub fn selected_text(&self) -> String {
        let Some((s, e)) = self.selection else { return String::new() };
        let mut out = String::new();
        let mut buf = Vec::new();
        for line in s.line..=e.line {
            let Some(wrapped) = self.line(line, &mut buf) else { continue };
            let from = if line == s.line { s.col } else { 0 };
            let to = if line == e.line { e.col + 1 } else { buf.len() };
            let start = out.len();
            for c in buf.get(from..to.min(buf.len())).unwrap_or(&[]) {
                if !c.is_spacer() {
                    self.clusters.push_str(c, &mut out);
                }
            }
            if !wrapped || line == e.line {
                out.truncate(start + out[start..].trim_end_matches(' ').len());
                if line != e.line {
                    out.push('\n');
                }
            }
        }
        out
    }
}
