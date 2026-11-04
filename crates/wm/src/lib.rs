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
        let target = target.filter(|t| self.contains(*t)).or(self.last.filter(|t| self.contains(*t)));
        self.last = Some(w);
        let Some(root) = self.root.take() else {
            self.root = Some(Node::Leaf(w));
            return;
        };
        fn split<T: Copy + PartialEq>(n: Node<T>, target: Option<T>, w: T, done: &mut bool) -> Node<T> {
            match n {
                Node::Leaf(t) if !*done && (target.is_none() || target == Some(t)) => {
                    *done = true;
                    Node::Split { ratio: 0.5, a: Box::new(Node::Leaf(t)), b: Box::new(Node::Leaf(w)) }
                }
                Node::Leaf(t) => Node::Leaf(t),
                Node::Split { ratio, a, b } => {
                    let a = split(*a, target, w, done);
                    let b = split(*b, target, w, done);
                    Node::Split { ratio, a: Box::new(a), b: Box::new(b) }
                }
            }
        }
        let mut done = false;
        self.root = Some(split(root, target, w, &mut done));
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
