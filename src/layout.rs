//! Pane layout: a binary split tree with focus navigation and border drawing.
//!
//! Geometry conventions:
//! - `Rect`s are absolute terminal cells.
//! - When a window has more than one pane (and enough room), an outer rounded
//!   frame plus 1-cell dividers separate panes, and every pane gets a one-line
//!   header. A single pane uses the entire window area with no chrome.
//! - The layout solver never panics for tiny terminals; it degrades to
//!   headerless, borderless panes when space is tight.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn inset(&self, n: u16) -> Rect {
        if self.w <= n * 2 || self.h <= n * 2 {
            return Rect::new(
                self.x.saturating_add(1).min(self.right().saturating_sub(1)),
                self.y
                    .saturating_add(1)
                    .min(self.bottom().saturating_sub(1)),
                0,
                0,
            );
        }
        Rect::new(self.x + n, self.y + n, self.w - n * 2, self.h - n * 2)
    }

    pub fn intersect(&self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r <= x || b <= y {
            Rect::new(x, y, 0, 0)
        } else {
            Rect::new(x, y, r - x, b - y)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

pub type PaneId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Panes side by side, separated by a vertical line.
    SideBySide,
    /// Panes stacked, separated by a horizontal line.
    Stacked,
}

impl Dir {
    pub fn opposite_axis(self) -> Dir {
        match self {
            Dir::SideBySide => Dir::Stacked,
            Dir::Stacked => Dir::SideBySide,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(PaneId),
    Split {
        dir: Dir,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Debug)]
pub struct Layout {
    root: Option<Node>,
}

pub const MIN_PANE_W: u16 = 16;
pub const MIN_PANE_H: u16 = 5;

#[derive(Clone, Debug)]
pub struct PanePlacement {
    pub pane: PaneId,
    /// Full allocation, including the one-line header when `header` is true.
    pub rect: Rect,
    /// Area available to the terminal emulator.
    pub content: Rect,
    /// Whether a pane header is drawn at `rect.y`.
    pub header: bool,
}

#[derive(Clone, Debug)]
pub struct SolvedLayout {
    pub panes: Vec<PanePlacement>,
    /// One bool per cell of the area; true where a border glyph belongs.
    pub border: Vec<bool>,
    pub border_w: u16,
    pub border_h: u16,
    /// Whether an outer frame is drawn.
    pub framed: bool,
}

impl Layout {
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: Some(Node::Leaf(pane)),
        }
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            collect_leaves(root, &mut out);
        }
        out
    }

    pub fn leaf_count(&self) -> usize {
        self.leaves().len()
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.leaves().contains(&pane)
    }

    /// Split `target` and return the new leaf. Returns `None` if the target is
    /// not part of this layout.
    pub fn split(&mut self, target: PaneId, new_pane: PaneId, dir: Dir) -> Option<()> {
        fn rec(node: &mut Node, target: PaneId, new_pane: PaneId, dir: Dir) -> bool {
            match node {
                Node::Leaf(id) if *id == target => {
                    let old = Node::Leaf(*id);
                    *node = Node::Split {
                        dir,
                        ratio: 0.5,
                        first: Box::new(old),
                        second: Box::new(Node::Leaf(new_pane)),
                    };
                    true
                }
                Node::Leaf(_) => false,
                Node::Split { first, second, .. } => {
                    rec(first, target, new_pane, dir) || rec(second, target, new_pane, dir)
                }
            }
        }
        let root = self.root.as_mut()?;
        rec(root, target, new_pane, dir).then_some(())
    }

    /// Remove a pane. Returns true if the pane was found. Removing the last
    /// pane leaves the layout empty.
    pub fn close(&mut self, pane: PaneId) -> bool {
        if !self.contains(pane) {
            return false;
        }
        if let Some(root) = self.root.take() {
            self.root = remove_pane(root, pane);
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Cycle focus in tree order.
    pub fn next_pane(&self, current: PaneId, forward: bool) -> PaneId {
        let leaves = self.leaves();
        if leaves.is_empty() {
            return current;
        }
        let idx = leaves.iter().position(|p| *p == current).unwrap_or(0);
        if forward {
            leaves[(idx + 1) % leaves.len()]
        } else {
            leaves[(idx + leaves.len() - 1) % leaves.len()]
        }
    }

    /// Adjust the nearest split on `dir`'s axis so the divider moves toward
    /// `movement`. `delta` is in ratio units (0.02 = 2%).
    pub fn resize(&mut self, pane: PaneId, movement: Dir, delta: f32) -> bool {
        fn rec(node: &mut Node, pane: PaneId, movement: Dir, delta: f32) -> bool {
            match node {
                Node::Leaf(_) => false,
                Node::Split {
                    dir,
                    ratio,
                    first,
                    second,
                } => {
                    // Prefer the nearest matching ancestor.
                    if *dir == movement {
                        let in_first = contains(first, pane);
                        let in_second = contains(second, pane);
                        if in_first || in_second {
                            let d = match (*dir, movement) {
                                (Dir::SideBySide, Dir::SideBySide) => {
                                    if in_first {
                                        delta
                                    } else {
                                        -delta
                                    }
                                }
                                (Dir::Stacked, Dir::Stacked) => {
                                    if in_first {
                                        delta
                                    } else {
                                        -delta
                                    }
                                }
                                _ => 0.0,
                            };
                            *ratio = (*ratio + d).clamp(0.1, 0.9);
                            return true;
                        }
                    }
                    rec(first, pane, movement, delta) || rec(second, pane, movement, delta)
                }
            }
        }
        match self.root.as_mut() {
            Some(root) => rec(root, pane, movement, delta),
            None => false,
        }
    }

    /// Solve the tree into absolute rectangles for `area`.
    pub fn solve(&self, area: Rect, framed: bool) -> SolvedLayout {
        let leaves = self.leaf_count();
        let Some(root) = &self.root else {
            return SolvedLayout {
                panes: Vec::new(),
                border: vec![false; area.w as usize * area.h as usize],
                border_w: area.w,
                border_h: area.h,
                framed: false,
            };
        };
        let inner = if framed && area.w > 2 && area.h > 2 {
            area.inset(1)
        } else {
            area
        };
        let mut placements = Vec::new();
        let header = framed && leaves > 1 && inner.h >= 2;
        solve_node(root, inner, header, &mut placements);
        let mut solved = SolvedLayout {
            panes: placements,
            border: vec![false; area.w as usize * area.h as usize],
            border_w: area.w,
            border_h: area.h,
            framed: framed && area.w > 2 && area.h > 2,
        };
        if solved.framed {
            mark_frame(&mut solved);
        }
        if leaves > 1 {
            // Draw separators even when the terminal is too small for the
            // outer frame and pane headers.
            solve_borders(root, inner, (area.x, area.y), &mut solved);
        }
        solved
    }
}

fn remove_pane(node: Node, pane: PaneId) -> Option<Node> {
    match node {
        Node::Leaf(id) if id == pane => None,
        Node::Leaf(_) => Some(node),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let first = remove_pane(*first, pane);
            let second = remove_pane(*second, pane);
            match (first, second) {
                (None, None) => None,
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (Some(a), Some(b)) => Some(Node::Split {
                    dir,
                    ratio,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
            }
        }
    }
}

fn collect_leaves(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { first, second, .. } => {
            collect_leaves(first, out);
            collect_leaves(second, out);
        }
    }
}

fn contains(node: &Node, pane: PaneId) -> bool {
    match node {
        Node::Leaf(id) => *id == pane,
        Node::Split { first, second, .. } => contains(first, pane) || contains(second, pane),
    }
}

fn split_sizes(total: u16, ratio: f32) -> (u16, u16) {
    if total < 2 {
        return (total, 0);
    }
    let avail = total - 1; // one cell for the divider
    if avail == 0 {
        return (0, 0);
    }
    if avail == 1 {
        return (1, 0);
    }
    let first = ((avail as f32 * ratio).round() as u16).clamp(1, avail - 1);
    (first, avail - first)
}

fn solve_node(node: &Node, area: Rect, header: bool, out: &mut Vec<PanePlacement>) {
    match node {
        Node::Leaf(id) => {
            if area.w == 0 || area.h == 0 {
                return;
            }
            let (header_h, content) = if header && area.h >= 2 {
                (1, Rect::new(area.x, area.y + 1, area.w, area.h - 1))
            } else {
                (0, area)
            };
            out.push(PanePlacement {
                pane: *id,
                rect: Rect::new(area.x, area.y, area.w, area.h),
                content,
                header: header_h > 0,
            });
        }
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => match dir {
            Dir::SideBySide => {
                if area.w < 2 {
                    solve_node(first, area, header, out);
                    return;
                }
                let (a, b) = split_sizes(area.w, *ratio);
                let first_rect = Rect::new(area.x, area.y, a, area.h);
                let second_rect = Rect::new(area.x + a + 1, area.y, b, area.h);
                solve_node(first, first_rect, header, out);
                solve_node(second, second_rect, header, out);
            }
            Dir::Stacked => {
                if area.h < 2 {
                    solve_node(first, area, header, out);
                    return;
                }
                let (a, b) = split_sizes(area.h, *ratio);
                let first_rect = Rect::new(area.x, area.y, area.w, a);
                let second_rect = Rect::new(area.x, area.y + a + 1, area.w, b);
                solve_node(first, first_rect, header, out);
                solve_node(second, second_rect, header, out);
            }
        },
    }
}

fn solve_borders(node: &Node, area: Rect, origin: (u16, u16), solved: &mut SolvedLayout) {
    let Node::Split {
        dir,
        ratio,
        first,
        second,
    } = node
    else {
        return;
    };
    match dir {
        Dir::SideBySide => {
            if area.w < 2 {
                return;
            }
            let (a, _b) = split_sizes(area.w, *ratio);
            let x = area.x + a;
            for y in area.y..area.bottom() {
                set_border(solved, x - origin.0, y - origin.1);
            }
            solve_borders(first, Rect::new(area.x, area.y, a, area.h), origin, solved);
            solve_borders(
                second,
                Rect::new(area.x + a + 1, area.y, area.w - a - 1, area.h),
                origin,
                solved,
            );
        }
        Dir::Stacked => {
            if area.h < 2 {
                return;
            }
            let (a, _b) = split_sizes(area.h, *ratio);
            let y = area.y + a;
            for x in area.x..area.right() {
                set_border(solved, x - origin.0, y - origin.1);
            }
            solve_borders(first, Rect::new(area.x, area.y, area.w, a), origin, solved);
            solve_borders(
                second,
                Rect::new(area.x, area.y + a + 1, area.w, area.h - a - 1),
                origin,
                solved,
            );
        }
    }
}

fn set_border(solved: &mut SolvedLayout, x: u16, y: u16) {
    if x < solved.border_w && y < solved.border_h {
        solved.border[y as usize * solved.border_w as usize + x as usize] = true;
    }
}

fn mark_frame(solved: &mut SolvedLayout) {
    let w = solved.border_w;
    let h = solved.border_h;
    if w < 2 || h < 2 {
        return;
    }
    for x in 0..w {
        set_border(solved, x, 0);
        set_border(solved, x, h - 1);
    }
    for y in 0..h {
        set_border(solved, 0, y);
        set_border(solved, w - 1, y);
    }
}

/// Pick a box-drawing glyph for a border cell based on its neighbours.
pub fn border_glyph(solved: &SolvedLayout, x: u16, y: u16) -> char {
    let on = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        let (x, y) = (x as u16, y as u16);
        if x >= solved.border_w || y >= solved.border_h {
            return false;
        }
        solved.border[y as usize * solved.border_w as usize + x as usize]
    };
    let u = on(x as i32, y as i32 - 1);
    let d = on(x as i32, y as i32 + 1);
    let l = on(x as i32 - 1, y as i32);
    let r = on(x as i32 + 1, y as i32);
    match (u, d, l, r) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, true, false, false) => '│',
        (false, false, true, true) => '─',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, false, false, true) => '╰',
        (true, false, true, false) => '╯',
        (false, true, false, true) => '╭',
        (false, true, true, false) => '╮',
        (true, false, false, false) => '╵',
        (false, true, false, false) => '╷',
        (false, false, true, false) => '╴',
        (false, false, false, true) => '╶',
        _ => '·',
    }
}

/// Join lines of a rectangle into a copyable text block.
pub fn rect_text(rect: Rect, rows: impl Iterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows {
        let mut chars: Vec<char> = row.chars().collect();
        let start = rect.x as usize;
        let end = (rect.x + rect.w) as usize;
        if chars.len() < end {
            chars.resize(end, ' ');
        }
        out.push(chars[start..end].iter().collect());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn single_pane_uses_full_area() {
        let layout = Layout::new(1);
        let solved = layout.solve(area(), false);
        assert_eq!(solved.panes.len(), 1);
        assert_eq!(solved.panes[0].content, area());
        assert!(!solved.panes[0].header);
        assert!(!solved.framed);
    }

    #[test]
    fn split_side_by_side_is_inside_frame() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::SideBySide).unwrap();
        let solved = layout.solve(area(), true);
        assert!(solved.framed);
        assert_eq!(solved.panes.len(), 2);
        let a = &solved.panes[0];
        let b = &solved.panes[1];
        assert!(a.content.x >= 1);
        assert!(a.content.right() < b.content.x);
        assert_eq!(a.content.w + b.content.w, 80 - 2 - 1);
        assert!(a.header && b.header);
        assert_eq!(a.content.h, 24 - 2 - 1);
    }

    #[test]
    fn split_stacked_shares_width() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::Stacked).unwrap();
        let solved = layout.solve(area(), true);
        let a = &solved.panes[0];
        let b = &solved.panes[1];
        assert_eq!(a.content.w, b.content.w);
        assert!(a.content.bottom() < b.content.y);
        assert_eq!(
            a.content.h + b.content.h,
            24 - 2 - 1 - 2 // frame + divider + two headers
        );
    }

    #[test]
    fn offset_area_dividers_are_local() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::Stacked).unwrap();
        let solved = layout.solve(Rect::new(10, 20, 30, 10), true);
        let internal: Vec<usize> = (1..9)
            .filter(|y| {
                solved.border[y * 30..(y + 1) * 30]
                    .iter()
                    .filter(|b| **b)
                    .count()
                    > 2
            })
            .collect();
        assert!(!internal.is_empty());
        assert!(solved.border[internal[0] * 30 + 15]);
    }

    #[test]
    fn tiny_area_does_not_panic() {
        for w in 0..6 {
            for h in 0..6 {
                let mut layout = Layout::new(1);
                layout.split(1, 2, Dir::SideBySide).unwrap();
                layout.split(2, 3, Dir::Stacked).unwrap();
                let _ = layout.solve(Rect::new(0, 0, w, h), true);
            }
        }
    }

    #[test]
    fn close_collapses_tree() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::SideBySide).unwrap();
        layout.split(2, 3, Dir::Stacked).unwrap();
        assert_eq!(layout.leaves(), vec![1, 2, 3]);
        layout.close(2);
        assert_eq!(layout.leaves(), vec![1, 3]);
        layout.close(1);
        assert_eq!(layout.leaves(), vec![3]);
        assert!(layout.close(3));
        assert!(layout.is_empty());
    }

    #[test]
    fn next_pane_cycles() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::SideBySide).unwrap();
        layout.split(2, 3, Dir::Stacked).unwrap();
        assert_eq!(layout.next_pane(1, true), 2);
        assert_eq!(layout.next_pane(3, true), 1);
        assert_eq!(layout.next_pane(1, false), 3);
    }

    #[test]
    fn resize_moves_ratio() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::SideBySide).unwrap();
        let before = match layout.root.as_ref().unwrap() {
            Node::Split { ratio, .. } => *ratio,
            _ => 0.0,
        };
        layout.resize(1, Dir::SideBySide, 0.05);
        let after = match layout.root.as_ref().unwrap() {
            Node::Split { ratio, .. } => *ratio,
            _ => 0.0,
        };
        assert!((after - before - 0.05).abs() < 1e-6);
    }

    #[test]
    fn border_glyphs_connect() {
        let mut layout = Layout::new(1);
        layout.split(1, 2, Dir::SideBySide).unwrap();
        layout.split(1, 3, Dir::Stacked).unwrap();
        let solved = layout.solve(area(), true);
        // Top-left outer corner is rounded.
        assert_eq!(border_glyph(&solved, 0, 0), '╭');
        // A junction exists somewhere on the vertical center divider.
        let mut found = false;
        for y in 0..24 {
            let g = border_glyph(&solved, 40, y);
            if g == '┤' || g == '├' || g == '┼' {
                found = true;
            }
        }
        assert!(found);
    }
}
