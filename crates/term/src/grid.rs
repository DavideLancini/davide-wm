use crate::cell::{attr, Cell};

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub cells: Vec<Cell>,
    /// The line continues on the next row (autowrap).
    pub wrapped: bool,
    /// Changed since the last damage report.
    pub dirty: bool,
    /// Changed during the current `feed`; clears an overlapping selection.
    pub touched: bool,
}

impl Row {
    pub fn new(cols: usize, blank: Cell) -> Row {
        Row { cells: vec![blank; cols], wrapped: false, dirty: true, touched: true }
    }

    pub fn mark(&mut self) {
        self.dirty = true;
        self.touched = true;
    }

    pub fn reset(&mut self, blank: Cell) {
        self.cells.fill(blank);
        self.wrapped = false;
        self.mark();
    }

    /// Breaks a wide character straddling columns `x - 1` and `x`.
    pub fn split(&mut self, x: usize, blank: Cell) {
        if x > 0 && x < self.cells.len() && self.cells[x].is_spacer() {
            self.cells[x - 1] = blank;
            self.cells[x] = blank;
        }
    }

    pub fn resize(&mut self, cols: usize) {
        if cols < self.cells.len() {
            self.split(cols, Cell::default());
            self.cells.truncate(cols);
            self.cells.shrink_to_fit();
        } else {
            self.cells.resize(cols, Cell::default());
        }
        self.mark();
    }

    /// Erases `[a, b)`.
    pub fn erase(&mut self, a: usize, b: usize, blank: Cell) {
        let b = b.min(self.cells.len());
        if a >= b {
            return;
        }
        self.split(a, blank);
        self.split(b, blank);
        self.cells[a..b].fill(blank);
        self.mark();
    }

    /// Inserts `n` blanks at `x`, shifting the rest right and off the edge.
    pub fn insert(&mut self, x: usize, n: usize, blank: Cell) {
        let len = self.cells.len();
        if x >= len {
            return;
        }
        let n = n.min(len - x);
        self.split(x, blank);
        self.split(len - n, blank);
        self.cells[x..].rotate_right(n);
        self.cells[x..x + n].fill(blank);
        self.mark();
    }

    /// Deletes `n` cells at `x`, shifting the rest left and padding with blanks.
    pub fn delete(&mut self, x: usize, n: usize, blank: Cell) {
        let len = self.cells.len();
        if x >= len {
            return;
        }
        let n = n.min(len - x);
        self.split(x, blank);
        self.split(x + n, blank);
        self.cells[x..].rotate_left(n);
        self.cells[len - n..].fill(blank);
        self.mark();
    }
}

pub(crate) fn fill_rows(rows: &mut [Row], blank: Cell) {
    for r in rows {
        r.reset(blank);
    }
}

pub(crate) fn wide_pair(c: Cell) -> (Cell, Cell) {
    let first = Cell { attrs: (c.attrs & !attr::LAYOUT) | attr::WIDE, ..c };
    let second = Cell { ch: ' ' as u32, attrs: (c.attrs & !attr::LAYOUT) | attr::SPACER, ..c };
    (first, second)
}
