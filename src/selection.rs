//! Mouse selection over the terminal grid: drag, word, line, and copy text.

use crate::term::Grid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPos {
    pub x: u16,
    pub y: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cells,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: CellPos,
    pub head: CellPos,
    pub kind: Kind,
}

impl Selection {
    pub fn new(anchor: CellPos, kind: Kind) -> Self {
        Self {
            anchor,
            head: anchor,
            kind,
        }
    }

    pub fn extend(&mut self, head: CellPos) {
        self.head = head;
    }

    /// The selection's flat inclusive cell range: `(first, last)` row-major
    /// indices into the grid. `None` when the grid has no cells. Callers that
    /// test many cells should resolve this once, not call [`Self::contains`] per cell.
    pub fn range(&self, grid: &Grid) -> Option<(usize, usize)> {
        if grid.cols == 0 || grid.rows == 0 {
            return None;
        }
        let cols = grid.cols as usize;
        let anchor = clamp(grid, self.anchor);
        let head = clamp(grid, self.head);
        let (lo, hi) = match self.kind {
            Kind::Cells => {
                let a = anchor.y as usize * cols + anchor.x as usize;
                let h = head.y as usize * cols + head.x as usize;
                (a.min(h), a.max(h))
            }
            Kind::Word => {
                let (a_start, a_end) = word_bounds(grid, anchor);
                let (h_start, h_end) = word_bounds(grid, head);
                let starts = [
                    a_start.y as usize * cols + a_start.x as usize,
                    h_start.y as usize * cols + h_start.x as usize,
                ];
                let ends = [
                    a_end.y as usize * cols + a_end.x as usize,
                    h_end.y as usize * cols + h_end.x as usize,
                ];
                (
                    *starts.iter().min().unwrap_or(&0),
                    *ends.iter().max().unwrap_or(&0),
                )
            }
            Kind::Line => {
                let y0 = anchor.y.min(head.y) as usize;
                let y1 = anchor.y.max(head.y) as usize;
                (y0 * cols, y1 * cols + cols - 1)
            }
        };
        Some((lo, hi))
    }

    pub fn contains(&self, grid: &Grid, x: u16, y: u16) -> bool {
        if x >= grid.cols || y >= grid.rows {
            return false;
        }
        let Some((lo, hi)) = self.range(grid) else {
            return false;
        };
        let key = y as usize * grid.cols as usize + x as usize;
        key >= lo && key <= hi
    }

    /// A click that has not been dragged anywhere.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn text(&self, grid: &Grid) -> String {
        let Some((lo, hi)) = self.range(grid) else {
            return String::new();
        };
        let cols = grid.cols as usize;
        let y0 = lo / cols;
        let y1 = hi / cols;
        let mut lines: Vec<String> = Vec::new();
        for y in y0..=y1 {
            let mut line = String::new();
            for x in 0..grid.cols {
                let key = y * cols + x as usize;
                if key >= lo
                    && key <= hi
                    && let Some(cell) = grid.cell(x, y as u16)
                {
                    line.push_str(&cell.text);
                }
            }
            lines.push(line.trim_end_matches([' ', '\t']).to_string());
        }
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

pub fn word_at(grid: &Grid, at: CellPos) -> Selection {
    let at = clamp(grid, at);
    let (start, end) = word_bounds(grid, at);
    Selection {
        anchor: start,
        head: end,
        kind: Kind::Word,
    }
}

pub fn line_at(grid: &Grid, at: CellPos) -> Selection {
    let at = clamp(grid, at);
    let end = grid.cols.saturating_sub(1);
    Selection {
        anchor: CellPos { x: 0, y: at.y },
        head: CellPos { x: end, y: at.y },
        kind: Kind::Line,
    }
}

fn clamp(grid: &Grid, p: CellPos) -> CellPos {
    CellPos {
        x: p.x.min(grid.cols.saturating_sub(1)),
        y: p.y.min(grid.rows.saturating_sub(1)),
    }
}

fn is_word_cell(grid: &Grid, x: u16, y: u16) -> bool {
    grid.cell(x, y)
        .is_some_and(|cell| !cell.text.is_empty() && !cell.text.chars().all(char::is_whitespace))
}

/// Inclusive bounds of the non-space run containing the cell, or the cell
/// itself when it is blank or whitespace.
fn word_bounds(grid: &Grid, at: CellPos) -> (CellPos, CellPos) {
    let at = clamp(grid, at);
    if grid.cols == 0 || grid.rows == 0 || !is_word_cell(grid, at.x, at.y) {
        return (at, at);
    }
    let mut start = at.x;
    while start > 0 && is_word_cell(grid, start - 1, at.y) {
        start -= 1;
    }
    let mut end = at.x;
    while end + 1 < grid.cols && is_word_cell(grid, end + 1, at.y) {
        end += 1;
    }
    (CellPos { x: start, y: at.y }, CellPos { x: end, y: at.y })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{Cell, Grid};

    fn grid(rows: &[&str]) -> Grid {
        let cols = rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0);
        let mut cells = Vec::with_capacity(cols * rows.len());
        for row in rows {
            let chars: Vec<char> = row.chars().collect();
            for x in 0..cols {
                let mut cell = Cell::default();
                if let Some(ch) = chars.get(x) {
                    cell.text.push(*ch);
                }
                cells.push(cell);
            }
        }
        Grid {
            cols: cols as u16,
            rows: rows.len() as u16,
            cells,
            ..Grid::default()
        }
    }

    fn pos(x: u16, y: u16) -> CellPos {
        CellPos { x, y }
    }

    #[test]
    fn drag_forward_is_a_linear_range() {
        let grid = grid(&["abc def", "  x  ", ""]);
        let mut selection = Selection::new(pos(5, 0), Kind::Cells);
        selection.extend(pos(2, 1));
        assert!(!selection.contains(&grid, 4, 0));
        assert!(selection.contains(&grid, 5, 0));
        assert!(selection.contains(&grid, 6, 0));
        assert!(selection.contains(&grid, 0, 1));
        assert!(selection.contains(&grid, 2, 1));
        assert!(!selection.contains(&grid, 3, 1));
        assert_eq!(selection.text(&grid), "ef\n  x");
    }

    #[test]
    fn drag_backward_is_the_same_range() {
        let grid = grid(&["abc def", "  x  ", ""]);
        let mut selection = Selection::new(pos(2, 1), Kind::Cells);
        selection.extend(pos(5, 0));
        assert!(!selection.contains(&grid, 4, 0));
        assert!(selection.contains(&grid, 0, 1));
        assert!(!selection.contains(&grid, 3, 1));
        assert_eq!(selection.text(&grid), "ef\n  x");
    }

    #[test]
    fn word_at_selects_a_whole_url() {
        let grid = grid(&["see https://a.b/c now"]);
        let selection = word_at(&grid, pos(10, 0));
        assert_eq!(selection.anchor, pos(4, 0));
        assert_eq!(selection.head, pos(16, 0));
        assert_eq!(selection.text(&grid), "https://a.b/c");
        assert!(selection.contains(&grid, 4, 0));
        assert!(selection.contains(&grid, 16, 0));
        assert!(!selection.contains(&grid, 17, 0));
        assert!(!selection.contains(&grid, 3, 0));
    }

    #[test]
    fn line_at_selects_the_whole_row() {
        let grid = grid(&["abc def", "  x  ", ""]);
        let selection = line_at(&grid, pos(4, 1));
        assert_eq!(selection.anchor, pos(0, 1));
        assert_eq!(selection.head, pos(6, 1));
        assert!(selection.contains(&grid, 0, 1));
        assert!(selection.contains(&grid, 6, 1));
        assert!(!selection.contains(&grid, 0, 0));
        assert_eq!(selection.text(&grid), "  x");
    }

    #[test]
    fn text_trims_trailing_spaces_and_blank_lines() {
        let grid = grid(&["abc def", "  x  ", ""]);
        let mut selection = Selection::new(pos(0, 0), Kind::Cells);
        selection.extend(pos(6, 2));
        assert_eq!(selection.text(&grid), "abc def\n  x");
    }

    #[test]
    fn text_keeps_internal_blank_lines() {
        let grid = grid(&["a", "", "b"]);
        let mut selection = Selection::new(pos(0, 0), Kind::Cells);
        selection.extend(pos(0, 2));
        assert_eq!(selection.text(&grid), "a\n\nb");
    }

    #[test]
    fn blank_anchor_selects_only_that_cell() {
        let grid = grid(&["abc def", "  x  ", ""]);
        let selection = word_at(&grid, pos(6, 1));
        assert_eq!(selection.anchor, pos(6, 1));
        assert_eq!(selection.head, pos(6, 1));
        assert!(selection.is_empty());
        assert!(selection.contains(&grid, 6, 1));
        assert!(!selection.contains(&grid, 5, 1));
        assert_eq!(selection.text(&grid), "");
    }

    #[test]
    fn range_is_the_flat_inclusive_cell_range() {
        let grid = grid(&["abc def", "  x  ", "y"]);
        let cols = grid.cols as usize;
        let mut cells = Selection::new(pos(5, 0), Kind::Cells);
        cells.extend(pos(2, 1));
        assert_eq!(cells.range(&grid), Some((5, cols + 2)));

        let word = word_at(&grid, pos(5, 0));
        assert_eq!(word.range(&grid), Some((4, 6)));

        let line = line_at(&grid, pos(4, 1));
        assert_eq!(line.range(&grid), Some((cols, 2 * cols - 1)));

        let empty = Grid::default();
        assert_eq!(Selection::new(pos(0, 0), Kind::Cells).range(&empty), None);
    }

    #[test]
    fn positions_are_clamped_to_the_grid() {
        let grid = grid(&["ab", "cd"]);
        let mut selection = Selection::new(pos(0, 0), Kind::Cells);
        selection.extend(pos(99, 99));
        assert_eq!(selection.text(&grid), "ab\ncd");
        assert!(!selection.contains(&grid, 2, 0));
    }
}
