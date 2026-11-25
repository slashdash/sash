// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! Split-pane geometry: the layout tree that divides the panel above libghostty.
//!
//! The tree only tracks which pane index sits where. Terminal state and the
//! pane list itself live in the caller, in a parallel `Vec`.

/// The space in points kept between two split children.
pub const GAP: f32 = 1.0;

/// The axis a split divides along.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Side by side, with the first child on the left.
    Right,
    /// Stacked, with the first child on top.
    Down,
}

/// A rectangle in points, measured from the area's top-left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Distance from the area's left edge.
    pub x: f32,
    /// Distance from the area's top edge.
    pub y: f32,
    /// Width in points.
    pub w: f32,
    /// Height in points.
    pub h: f32,
}

/// One node of the layout tree: a pane or a split of two children.
#[derive(Clone, Debug)]
enum Node {
    Leaf(usize),
    Split {
        dir: Dir,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// The arrangement of panes in the panel.
///
/// Leaves are indices into the caller's parallel pane list. A pane can be
/// split again, and removing a pane hands its parent's space to its sibling.
#[derive(Clone, Debug)]
pub struct Tree {
    root: Node,
}

impl Tree {
    /// A tree holding a single pane.
    pub fn new(leaf: usize) -> Self {
        Self {
            root: Node::Leaf(leaf),
        }
    }

    /// Pane indices in layout order: depth-first, left or top child first.
    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.len());
        collect_leaves(&self.root, &mut out);
        out
    }

    /// How many panes the tree holds.
    pub fn len(&self) -> usize {
        count(&self.root)
    }

    /// Whether the tree is empty. A valid tree always holds one pane.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `leaf` is in the tree.
    pub fn contains(&self, leaf: usize) -> bool {
        holds(&self.root, leaf)
    }

    /// The pane after `current` in layout order, wrapping at both ends.
    /// An unknown `current` gives the first pane.
    pub fn focus_next(&self, current: usize, forward: bool) -> usize {
        let leaves = self.leaves();
        let Some(at) = leaves.iter().position(|&leaf| leaf == current) else {
            return leaves.first().copied().unwrap_or(current);
        };
        if leaves.len() <= 1 {
            return leaves[0];
        }
        let step = if forward { 1 } else { leaves.len() - 1 };
        leaves[(at + step) % leaves.len()]
    }

    /// Splits `target` in two, putting `new_leaf` after it at an even ratio.
    /// `false` when `target` is unknown or `new_leaf` is already a pane.
    pub fn split(&mut self, target: usize, new_leaf: usize, dir: Dir) -> bool {
        if self.contains(new_leaf) {
            return false;
        }
        split_at(&mut self.root, target, new_leaf, dir)
    }

    /// Removes a pane, letting its sibling take the parent's space. `false`
    /// when `leaf` is unknown or is the only pane.
    pub fn remove(&mut self, leaf: usize) -> bool {
        if self.len() <= 1 || !self.contains(leaf) {
            return false;
        }
        remove_at(&mut self.root, leaf)
    }

    /// Shifts every pane index above `removed` down by one. Call this after
    /// dropping the removed pane from the caller's parallel list.
    pub fn remap_after_remove(&mut self, removed: usize) {
        remap(&mut self.root, removed);
    }

    /// Every pane and its rectangle, in layout order. `gap` is the space
    /// between split children; `area` is clamped so the result is finite.
    pub fn layout(&self, area: Rect, gap: f32) -> Vec<(usize, Rect)> {
        let mut out = Vec::with_capacity(self.len());
        layout_at(&self.root, clean(area), gap, &mut out);
        out
    }
}

fn count(node: &Node) -> usize {
    match node {
        Node::Leaf(_) => 1,
        Node::Split { first, second, .. } => count(first) + count(second),
    }
}

fn collect_leaves(node: &Node, out: &mut Vec<usize>) {
    match node {
        Node::Leaf(leaf) => out.push(*leaf),
        Node::Split { first, second, .. } => {
            collect_leaves(first, out);
            collect_leaves(second, out);
        }
    }
}

fn holds(node: &Node, leaf: usize) -> bool {
    match node {
        Node::Leaf(at) => *at == leaf,
        Node::Split { first, second, .. } => holds(first, leaf) || holds(second, leaf),
    }
}

fn split_at(node: &mut Node, target: usize, new_leaf: usize, dir: Dir) -> bool {
    match node {
        Node::Leaf(leaf) => {
            if *leaf != target {
                return false;
            }
            let first = Box::new(Node::Leaf(*leaf));
            *node = Node::Split {
                dir,
                ratio: 0.5,
                first,
                second: Box::new(Node::Leaf(new_leaf)),
            };
            true
        }
        Node::Split { first, second, .. } => {
            split_at(first, target, new_leaf, dir) || split_at(second, target, new_leaf, dir)
        }
    }
}

fn remove_at(node: &mut Node, leaf: usize) -> bool {
    let Node::Split { first, second, .. } = node else {
        return false;
    };
    // A leaf can only be removed through its parent split. Taking the sibling
    // out leaves a placeholder behind, and the whole split is dropped with it.
    if matches!(**first, Node::Leaf(at) if at == leaf) {
        let sibling = std::mem::replace(second, Box::new(Node::Leaf(0)));
        *node = *sibling;
        return true;
    }
    if matches!(**second, Node::Leaf(at) if at == leaf) {
        let sibling = std::mem::replace(first, Box::new(Node::Leaf(0)));
        *node = *sibling;
        return true;
    }
    remove_at(first, leaf) || remove_at(second, leaf)
}

fn remap(node: &mut Node, removed: usize) {
    match node {
        Node::Leaf(leaf) => {
            if *leaf > removed {
                *leaf -= 1;
            }
        }
        Node::Split { first, second, .. } => {
            remap(first, removed);
            remap(second, removed);
        }
    }
}

fn layout_at(node: &Node, area: Rect, gap: f32, out: &mut Vec<(usize, Rect)>) {
    match node {
        Node::Leaf(leaf) => out.push((*leaf, area)),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *dir, *ratio, gap);
            layout_at(first, a, gap, out);
            layout_at(second, b, gap, out);
        }
    }
}

/// Breaks an area into its two children, keeping `gap` between them.
fn split_rect(area: Rect, dir: Dir, ratio: f32, gap: f32) -> (Rect, Rect) {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    };
    let gap = finite(gap).max(0.0);
    match dir {
        Dir::Right => {
            let usable = area.w - gap;
            if !usable.is_finite() || usable <= 0.0 {
                // Too thin to divide: the first pane keeps the whole strip.
                let first = Rect { w: area.w, ..area };
                let second = Rect {
                    x: area.x + area.w,
                    w: 0.0,
                    ..area
                };
                (first, second)
            } else {
                let first_w = usable * ratio;
                (
                    Rect { w: first_w, ..area },
                    Rect {
                        x: area.x + first_w + gap,
                        w: usable - first_w,
                        ..area
                    },
                )
            }
        }
        Dir::Down => {
            let usable = area.h - gap;
            if !usable.is_finite() || usable <= 0.0 {
                let first = Rect { h: area.h, ..area };
                let second = Rect {
                    y: area.y + area.h,
                    h: 0.0,
                    ..area
                };
                (first, second)
            } else {
                let first_h = usable * ratio;
                (
                    Rect { h: first_h, ..area },
                    Rect {
                        y: area.y + first_h + gap,
                        h: usable - first_h,
                        ..area
                    },
                )
            }
        }
    }
}

/// Clamps an area to finite, non-negative sizes.
fn clean(area: Rect) -> Rect {
    Rect {
        x: finite(area.x),
        y: finite(area.y),
        w: finite(area.w).max(0.0),
        h: finite(area.h).max(0.0),
    }
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 10.0,
            y: 20.0,
            w: 400.0,
            h: 300.0,
        }
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.001
    }

    fn assert_rect(got: Rect, want: Rect) {
        assert!(near(got.x, want.x), "x: {got:?} != {want:?}");
        assert!(near(got.y, want.y), "y: {got:?} != {want:?}");
        assert!(near(got.w, want.w), "w: {got:?} != {want:?}");
        assert!(near(got.h, want.h), "h: {got:?} != {want:?}");
    }

    fn rect_of(layout: &[(usize, Rect)], leaf: usize) -> Rect {
        layout
            .iter()
            .find(|(at, _)| *at == leaf)
            .expect("leaf in layout")
            .1
    }

    #[test]
    fn single_leaf_fills_the_area() {
        let tree = Tree::new(7);
        assert_eq!(tree.len(), 1);
        assert!(!tree.is_empty());
        assert_eq!(tree.leaves(), vec![7]);
        let layout = tree.layout(area(), GAP);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].0, 7);
        assert_rect(layout[0].1, area());
    }

    #[test]
    fn split_right_keeps_one_gap_between_panes() {
        let mut tree = Tree::new(0);
        assert!(tree.split(0, 1, Dir::Right));
        assert_eq!(tree.leaves(), vec![0, 1]);
        let layout = tree.layout(area(), GAP);
        let left = rect_of(&layout, 0);
        let right = rect_of(&layout, 1);
        assert!(near(right.x - (left.x + left.w), GAP));
        assert!(near(left.w, right.w));
        assert!(near(left.w, 199.5));
        assert!(near(left.w + GAP + right.w, area().w));
        assert!(near(left.y, area().y) && near(left.h, area().h));
        assert!(near(right.y, area().y) && near(right.h, area().h));
    }

    #[test]
    fn split_down_stacks_panes() {
        let mut tree = Tree::new(0);
        assert!(tree.split(0, 1, Dir::Down));
        assert_eq!(tree.leaves(), vec![0, 1]);
        let layout = tree.layout(area(), GAP);
        let top = rect_of(&layout, 0);
        let bottom = rect_of(&layout, 1);
        assert!(near(bottom.y - (top.y + top.h), GAP));
        assert!(near(top.h, bottom.h));
        assert!(near(top.h, 149.5));
        assert!(near(top.h + GAP + bottom.h, area().h));
        assert!(near(top.x, area().x) && near(top.w, area().w));
        assert!(near(bottom.x, area().x) && near(bottom.w, area().w));
    }

    #[test]
    fn nested_split_lays_out_depth_first() {
        let mut tree = Tree::new(0);
        assert!(tree.split(0, 1, Dir::Right));
        assert!(tree.split(1, 2, Dir::Down));
        assert_eq!(tree.leaves(), vec![0, 1, 2]);
        let layout = tree.layout(area(), GAP);
        let order: Vec<usize> = layout.iter().map(|(leaf, _)| *leaf).collect();
        assert_eq!(order, vec![0, 1, 2]);

        let left = rect_of(&layout, 0);
        let top = rect_of(&layout, 1);
        let bottom = rect_of(&layout, 2);
        // The right pane splits down: both halves share a column.
        assert!(near(top.x, bottom.x) && near(top.w, bottom.w));
        assert!(near(bottom.y - (top.y + top.h), GAP));
        assert!(near(top.h, bottom.h));
        // The left pane keeps the full height beside the column.
        assert!(near(left.h, area().h));
        assert!(near(top.x - (left.x + left.w), GAP));
    }

    #[test]
    fn removing_a_leaf_collapses_its_parent() {
        let mut tree = Tree::new(0);
        tree.split(0, 1, Dir::Right);
        tree.split(1, 2, Dir::Down);
        assert_eq!(tree.leaves(), vec![0, 1, 2]);

        assert!(tree.remove(2));
        assert_eq!(tree.leaves(), vec![0, 1]);
        assert!(!tree.contains(2));

        // The survivor takes over the space the split had.
        let layout = tree.layout(area(), GAP);
        let left = rect_of(&layout, 0);
        let right = rect_of(&layout, 1);
        assert!(near(right.y, area().y));
        assert!(near(right.h, area().h));
        assert!(near(right.x - (left.x + left.w), GAP));
    }

    #[test]
    fn remap_shifts_higher_leaves_down() {
        let mut tree = Tree::new(0);
        tree.split(0, 1, Dir::Right);
        tree.split(1, 2, Dir::Down);
        tree.split(2, 3, Dir::Down);
        assert_eq!(tree.leaves(), vec![0, 1, 2, 3]);

        // Close pane 2 in the tree and in the parallel pane list, then shift
        // the surviving indices down so they still point at their panes.
        assert!(tree.remove(2));
        tree.remap_after_remove(2);
        assert_eq!(tree.leaves(), vec![0, 1, 2]);
        assert!(tree.contains(2));
        assert!(!tree.contains(3));
    }

    #[test]
    fn focus_next_wraps_and_falls_back() {
        let mut tree = Tree::new(0);
        tree.split(0, 1, Dir::Right);
        tree.split(1, 2, Dir::Down);
        assert_eq!(tree.focus_next(0, true), 1);
        assert_eq!(tree.focus_next(1, true), 2);
        assert_eq!(tree.focus_next(2, true), 0);
        assert_eq!(tree.focus_next(0, false), 2);
        assert_eq!(tree.focus_next(1, false), 0);
        assert_eq!(tree.focus_next(2, false), 1);
        assert_eq!(tree.focus_next(99, true), 0);
        assert_eq!(tree.focus_next(99, false), 0);
    }

    #[test]
    fn impossible_edits_are_rejected() {
        let mut tree = Tree::new(0);
        assert!(!tree.remove(0));
        assert!(!tree.remove(9));
        assert!(!tree.split(9, 1, Dir::Right));

        assert!(tree.split(0, 1, Dir::Right));
        assert!(!tree.split(0, 1, Dir::Right));
        assert!(!tree.split(1, 1, Dir::Right));
        assert_eq!(tree.leaves(), vec![0, 1]);
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn thin_and_broken_areas_stay_finite() {
        let mut tree = Tree::new(0);
        tree.split(0, 1, Dir::Right);

        for area in [
            Rect::default(),
            Rect {
                w: -5.0,
                h: -5.0,
                ..area()
            },
            Rect {
                w: f32::NAN,
                h: f32::INFINITY,
                ..area()
            },
            Rect {
                x: f32::NAN,
                y: f32::INFINITY,
                ..area()
            },
        ] {
            for (_, r) in tree.layout(area, GAP) {
                for v in [r.x, r.y, r.w, r.h] {
                    assert!(v.is_finite() && v >= 0.0, "{r:?} from {area:?}");
                }
            }
        }

        // Too thin to divide: the first pane keeps everything.
        let flat = tree.layout(Rect::default(), GAP);
        assert!(near(rect_of(&flat, 0).w, 0.0));
        assert!(near(rect_of(&flat, 1).w, 0.0));
        assert!(near(rect_of(&flat, 1).x, 0.0));
    }
}
