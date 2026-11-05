//! Window management logic that knows nothing about Wayland.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x as f64 && py >= self.y as f64 && px < (self.x + self.w) as f64 && py < (self.y + self.h) as f64
    }

    fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug)]
enum Node<T> {
    Leaf(T),
    /// Two children; side by side when the area is wider than tall,
    /// stacked otherwise (decided at layout time, like Hyprland's dwindle).
    Split { ratio: f32, a: Box<Node<T>>, b: Box<Node<T>> },
}

/// Binary-split tiling tree: each new window splits the target window's area.
#[derive(Debug)]
pub struct Dwindle<T> {
    root: Option<Node<T>>,
    last: Option<T>,
}

impl<T: Copy + PartialEq> Default for Dwindle<T> {
    fn default() -> Self {
        Dwindle { root: None, last: None }
    }
}

impl<T: Copy + PartialEq> Dwindle<T> {
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn contains(&self, w: T) -> bool {
        fn walk<T: PartialEq>(n: &Node<T>, w: &T) -> bool {
            match n {
                Node::Leaf(t) => t == w,
                Node::Split { a, b, .. } => walk(a, w) || walk(b, w),
            }
        }
        self.root.as_ref().is_some_and(|r| walk(r, &w))
    }

    /// Inserts `w` by splitting `target`, or the last inserted window when
    /// `target` is `None` or not in the tree.
    pub fn insert(&mut self, w: T, target: Option<T>) {
        self.insert_beside(w, target, false);
    }

    /// Like `insert`; with `first` the new window takes the left (or top)
    /// half of the split instead of the right (or bottom) one.
    pub fn insert_beside(&mut self, w: T, target: Option<T>, first: bool) {
        let target = target.filter(|t| self.contains(*t)).or(self.last.filter(|t| self.contains(*t)));
        self.last = Some(w);
        let Some(root) = self.root.take() else {
            self.root = Some(Node::Leaf(w));
            return;
        };
        fn split<T: Copy + PartialEq>(n: Node<T>, target: Option<T>, w: T, first: bool, done: &mut bool) -> Node<T> {
            match n {
                Node::Leaf(t) if !*done && (target.is_none() || target == Some(t)) => {
                    *done = true;
                    let (a, b) = if first { (w, t) } else { (t, w) };
                    Node::Split { ratio: 0.5, a: Box::new(Node::Leaf(a)), b: Box::new(Node::Leaf(b)) }
                }
                Node::Leaf(t) => Node::Leaf(t),
                Node::Split { ratio, a, b } => {
                    let a = split(*a, target, w, first, done);
                    let b = split(*b, target, w, first, done);
                    Node::Split { ratio, a: Box::new(a), b: Box::new(b) }
                }
            }
        }
        let mut done = false;
        self.root = Some(split(root, target, w, first, &mut done));
    }

    pub fn remove(&mut self, w: T) {
        fn rm<T: PartialEq>(n: Node<T>, w: &T) -> Option<Node<T>> {
            match n {
                Node::Leaf(t) if &t == w => None,
                Node::Leaf(t) => Some(Node::Leaf(t)),
                Node::Split { ratio, a, b } => match (rm(*a, w), rm(*b, w)) {
                    (Some(a), Some(b)) => Some(Node::Split { ratio, a: Box::new(a), b: Box::new(b) }),
                    (Some(n), None) | (None, Some(n)) => Some(n),
                    (None, None) => None,
                },
            }
        }
        if let Some(root) = self.root.take() {
            self.root = rm(root, &w);
        }
        if self.last == Some(w) {
            self.last = None;
        }
    }

    /// Moves `w` next to `target`, as Hyprland's dwindle `movewindow` does:
    /// `w` leaves its place and splits `target`, on the side it came from
    /// when moving left or up.
    pub fn move_next_to(&mut self, w: T, target: T, dir: Direction) {
        if w == target || !self.contains(w) || !self.contains(target) {
            return;
        }
        self.remove(w);
        self.insert_beside(w, Some(target), matches!(dir, Direction::Left | Direction::Up));
    }

    /// Exchanges the positions of two windows.
    pub fn swap(&mut self, x: T, y: T) {
        fn walk<T: Copy + PartialEq>(n: &mut Node<T>, x: T, y: T) {
            match n {
                Node::Leaf(t) if *t == x => *t = y,
                Node::Leaf(t) if *t == y => *t = x,
                Node::Leaf(_) => {}
                Node::Split { a, b, .. } => {
                    walk(a, x, y);
                    walk(b, x, y);
                }
            }
        }
        if x != y && self.contains(x) && self.contains(y) {
            if let Some(r) = self.root.as_mut() {
                walk(r, x, y);
            }
        }
    }

    pub fn windows(&self) -> Vec<T> {
        self.layout(Rect::new(0, 0, 2, 2)).into_iter().map(|(t, _)| t).collect()
    }

    pub fn layout(&self, area: Rect) -> Vec<(T, Rect)> {
        fn walk<T: Copy>(n: &Node<T>, r: Rect, out: &mut Vec<(T, Rect)>) {
            match n {
                Node::Leaf(t) => out.push((*t, r)),
                Node::Split { ratio, a, b } => {
                    let (ra, rb) = if r.w >= r.h {
                        let wa = (r.w as f32 * ratio).round() as i32;
                        (Rect::new(r.x, r.y, wa, r.h), Rect::new(r.x + wa, r.y, r.w - wa, r.h))
                    } else {
                        let ha = (r.h as f32 * ratio).round() as i32;
                        (Rect::new(r.x, r.y, r.w, ha), Rect::new(r.x, r.y + ha, r.w, r.h - ha))
                    };
                    walk(a, ra, out);
                    walk(b, rb, out);
                }
            }
        }
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            walk(r, area, &mut out);
        }
        out
    }
}

/// Workspace identifiers, matching the Hyprland setup this replaces.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WsId {
    /// 1–41.
    Num(u32),
    /// The named workspace `chat`.
    Chat,
    /// The special workspace `resize`, shown on top of the active one.
    Special,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsMode {
    /// Covers the work area.
    Maximized,
    /// Covers the whole output.
    Fullscreen,
}

#[derive(Debug)]
pub struct Workspace<T> {
    pub tiling: Dwindle<T>,
    /// Windows above the tiles at their own size (dialogs), bottom to top.
    /// A zero size means not placed yet.
    pub floating: Vec<(T, Rect)>,
    /// Last focused window, restored when the workspace becomes active again.
    pub focus: Option<T>,
    pub fullscreen: Option<(T, FsMode)>,
}

impl<T: Copy + PartialEq> Default for Workspace<T> {
    fn default() -> Self {
        Workspace { tiling: Dwindle::default(), floating: Vec::new(), focus: None, fullscreen: None }
    }
}

impl<T: Copy + PartialEq> Workspace<T> {
    pub fn is_empty(&self) -> bool {
        self.tiling.is_empty() && self.floating.is_empty()
    }

    pub fn contains(&self, w: T) -> bool {
        self.tiling.contains(w) || self.is_floating(w)
    }

    pub fn is_floating(&self, w: T) -> bool {
        self.floating.iter().any(|(t, _)| *t == w)
    }

    /// Raises a floating window above the others.
    pub fn raise(&mut self, w: T) {
        if let Some(i) = self.floating.iter().position(|(t, _)| *t == w) {
            let f = self.floating.remove(i);
            self.floating.push(f);
        }
    }

    pub fn remove(&mut self, w: T) {
        self.tiling.remove(w);
        self.floating.retain(|(t, _)| *t != w);
        if self.focus == Some(w) {
            self.focus = None;
        }
        if self.fullscreen.is_some_and(|(f, _)| f == w) {
            self.fullscreen = None;
        }
    }

    /// Visible windows and their rectangles, bottom to top.
    pub fn layout(&self, work: Rect, output: Rect) -> Vec<(T, Rect)> {
        match self.fullscreen {
            Some((w, FsMode::Fullscreen)) => vec![(w, output)],
            Some((w, FsMode::Maximized)) => vec![(w, work)],
            None => {
                let mut l = self.tiling.layout(work);
                l.extend(self.floating.iter().filter(|(_, r)| r.w > 0 && r.h > 0).copied());
                l
            }
        }
    }
}

/// The window closest to `from` in direction `dir`, among `layout`.
pub fn neighbor<T: Copy + PartialEq>(layout: &[(T, Rect)], from: T, dir: Direction) -> Option<T> {
    let (_, f) = *layout.iter().find(|(t, _)| *t == from)?;
    let (fx, fy) = f.center();
    layout
        .iter()
        .filter(|(t, r)| {
            *t != from
                && match dir {
                    Direction::Left => r.x + r.w <= f.x && r.y < f.y + f.h && r.y + r.h > f.y,
                    Direction::Right => r.x >= f.x + f.w && r.y < f.y + f.h && r.y + r.h > f.y,
                    Direction::Up => r.y + r.h <= f.y && r.x < f.x + f.w && r.x + r.w > f.x,
                    Direction::Down => r.y >= f.y + f.h && r.x < f.x + f.w && r.x + r.w > f.x,
                }
        })
        .min_by_key(|(_, r)| {
            let (cx, cy) = r.center();
            (cx - fx).abs() + (cy - fy).abs()
        })
        .map(|(t, _)| *t)
}

use state::{persist_struct, Dec, Enc, Persist};

persist_struct!(Rect { x, y, w, h });

impl Persist for WsId {
    fn save(&self, e: &mut Enc) {
        match self {
            WsId::Num(n) => (0u8, *n).save(e),
            WsId::Chat => (1u8, 0u32).save(e),
            WsId::Special => (2u8, 0u32).save(e),
        }
    }
    fn load(d: &mut Dec) -> state::Result<Self> {
        Ok(match <(u8, u32)>::load(d)? {
            (0, n) => WsId::Num(n),
            (1, _) => WsId::Chat,
            _ => WsId::Special,
        })
    }
}

impl Persist for FsMode {
    fn save(&self, e: &mut Enc) {
        (*self == FsMode::Fullscreen).save(e)
    }
    fn load(d: &mut Dec) -> state::Result<Self> {
        Ok(if bool::load(d)? { FsMode::Fullscreen } else { FsMode::Maximized })
    }
}

impl<T: Persist> Persist for Node<T> {
    fn save(&self, e: &mut Enc) {
        match self {
            Node::Leaf(t) => {
                0u8.save(e);
                t.save(e);
            }
            Node::Split { ratio, a, b } => {
                1u8.save(e);
                ratio.save(e);
                a.save(e);
                b.save(e);
            }
        }
    }
    fn load(d: &mut Dec) -> state::Result<Self> {
        Ok(match u8::load(d)? {
            0 => Node::Leaf(T::load(d)?),
            _ => Node::Split { ratio: f32::load(d)?, a: Persist::load(d)?, b: Persist::load(d)? },
        })
    }
}

impl<T: Persist> Persist for Dwindle<T> {
    fn save(&self, e: &mut Enc) {
        self.root.save(e);
        self.last.save(e);
    }
    fn load(d: &mut Dec) -> state::Result<Self> {
        Ok(Dwindle { root: Persist::load(d)?, last: Persist::load(d)? })
    }
}

impl<T: Persist> Persist for Workspace<T> {
    fn save(&self, e: &mut Enc) {
        self.tiling.save(e);
        self.floating.save(e);
        self.focus.save(e);
        self.fullscreen.save(e);
    }
    fn load(d: &mut Dec) -> state::Result<Self> {
        Ok(Workspace { tiling: Persist::load(d)?, floating: Persist::load(d)?, focus: Persist::load(d)?, fullscreen: Persist::load(d)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 1000, 600);

    #[test]
    fn splits_alternate_by_aspect() {
        let mut d = Dwindle::default();
        d.insert(1, None);
        d.insert(2, Some(1));
        d.insert(3, Some(2));
        let l = d.layout(AREA);
        assert_eq!(l, vec![(1, Rect::new(0, 0, 500, 600)), (2, Rect::new(500, 0, 500, 300)), (3, Rect::new(500, 300, 500, 300))]);
    }

    #[test]
    fn remove_collapses_parent() {
        let mut d = Dwindle::default();
        d.insert(1, None);
        d.insert(2, Some(1));
        d.insert(3, Some(2));
        d.remove(2);
        assert_eq!(d.layout(AREA), vec![(1, Rect::new(0, 0, 500, 600)), (3, Rect::new(500, 0, 500, 600))]);
        d.remove(1);
        d.remove(3);
        assert!(d.is_empty());
    }

    #[test]
    fn swap_and_fullscreen() {
        let mut ws = Workspace::default();
        ws.tiling.insert(1, None);
        ws.tiling.insert(2, Some(1));
        ws.tiling.swap(1, 2);
        assert_eq!(ws.tiling.windows(), vec![2, 1]);
        ws.fullscreen = Some((1, FsMode::Maximized));
        assert_eq!(ws.layout(AREA, AREA), vec![(1, AREA)]);
        ws.remove(1);
        assert_eq!(ws.layout(AREA, AREA), vec![(2, AREA)]);
    }

    #[test]
    fn floating_on_top() {
        let mut ws = Workspace::default();
        ws.tiling.insert(1, None);
        ws.floating.push((2, Rect::new(10, 10, 100, 50)));
        ws.floating.push((3, Rect::new(0, 0, 0, 0)));
        assert_eq!(ws.layout(AREA, AREA), vec![(1, AREA), (2, Rect::new(10, 10, 100, 50))]);
        assert!(ws.contains(3) && ws.is_floating(2) && !ws.is_floating(1));
        ws.remove(2);
        ws.remove(3);
        ws.remove(1);
        assert!(ws.is_empty());
    }

    #[test]
    fn move_joins_the_neighbour() {
        // 1 on the left, 2 above 3 on the right.
        let mut d = Dwindle::default();
        d.insert(1, None);
        d.insert(2, Some(1));
        d.insert(3, Some(2));
        d.move_next_to(3, 1, Direction::Left);
        // 1's half splits (taller than wide): 3 on top, 1 below; 2 alone right.
        assert_eq!(
            d.layout(Rect::new(0, 0, 1200, 1000)),
            vec![(3, Rect::new(0, 0, 600, 500)), (1, Rect::new(0, 500, 600, 500)), (2, Rect::new(600, 0, 600, 1000))]
        );
        // Two side by side: moving the right one left swaps them.
        let mut e = Dwindle::default();
        e.insert(1, None);
        e.insert(2, Some(1));
        e.move_next_to(2, 1, Direction::Left);
        assert_eq!(e.windows(), vec![2, 1]);
    }

    #[test]
    fn neighbors() {
        let mut d = Dwindle::default();
        d.insert(1, None);
        d.insert(2, Some(1));
        d.insert(3, Some(2));
        let l = d.layout(AREA);
        assert_eq!(neighbor(&l, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&l, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&l, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&l, 1, Direction::Left), None);
    }
}
