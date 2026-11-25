// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

//! Cell-filling glyphs for block and box-drawing characters.
//!
//! A terminal cell is taller than a font's em box: the cell height carries
//! leading so text breathes. Programs do not treat block elements and box
//! drawing as text, though; they use them as pixels and borders, expecting
//! every cell to butt against its neighbours. Fonts draw those glyphs inside
//! their em box, so the art leaves horizontal seams and a vertical border
//! breaks at every row. Sash draws them as rectangles on the cell grid.

/// One fill in cell-local points, with alpha for the shaded blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub alpha: f32,
}

impl Rect {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            x,
            y,
            w,
            h,
            alpha: 1.0,
        }
    }

    const fn shade(x: f32, y: f32, w: f32, h: f32, alpha: f32) -> Self {
        Self { x, y, w, h, alpha }
    }
}

/// Whether the renderer must draw this character itself rather than shape it.
pub fn is_cell_glyph(ch: char) -> bool {
    let cp = ch as u32;
    (0x2500..=0x259f).contains(&cp) && !matches!(ch, '╱' | '╲' | '╳')
}

/// The rectangles that make up one cell's glyph, in cell-local points.
pub fn rects(ch: char, w: f32, h: f32) -> Option<Vec<Rect>> {
    if !is_cell_glyph(ch) {
        return None;
    }
    if ch as u32 >= 0x2580 {
        let block = blocks(ch, w, h);
        return (!block.is_empty()).then_some(block);
    }
    let mut out = Vec::new();
    box_drawing(ch as u32, w, h, &mut out);
    (!out.is_empty()).then_some(out)
}

/// A solid block, quadrant or shade, exactly on the cell grid.
fn blocks(ch: char, w: f32, h: f32) -> Vec<Rect> {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let e = |n: f32| h * n / 8.0;
    match ch {
        '█' => vec![Rect::new(0.0, 0.0, w, h)],
        '▀' => vec![Rect::new(0.0, 0.0, w, hh)],
        '▄' => vec![Rect::new(0.0, hh, w, hh)],
        '▌' => vec![Rect::new(0.0, 0.0, hw, h)],
        '▐' => vec![Rect::new(hw, 0.0, hw, h)],
        '▁' => vec![Rect::new(0.0, h - e(1.0), w, e(1.0))],
        '▂' => vec![Rect::new(0.0, h - e(2.0), w, e(2.0))],
        '▃' => vec![Rect::new(0.0, h - e(3.0), w, e(3.0))],
        '▅' => vec![Rect::new(0.0, h - e(5.0), w, e(5.0))],
        '▆' => vec![Rect::new(0.0, h - e(6.0), w, e(6.0))],
        '▇' => vec![Rect::new(0.0, h - e(7.0), w, e(7.0))],
        '▉' => vec![Rect::new(0.0, 0.0, w * 7.0 / 8.0, h)],
        '▊' => vec![Rect::new(0.0, 0.0, w * 3.0 / 4.0, h)],
        '▋' => vec![Rect::new(0.0, 0.0, w * 5.0 / 8.0, h)],
        '▍' => vec![Rect::new(0.0, 0.0, w * 3.0 / 8.0, h)],
        '▎' => vec![Rect::new(0.0, 0.0, w / 4.0, h)],
        '▏' => vec![Rect::new(0.0, 0.0, w / 8.0, h)],
        '░' => vec![Rect::shade(0.0, 0.0, w, h, 0.25)],
        '▒' => vec![Rect::shade(0.0, 0.0, w, h, 0.50)],
        '▓' => vec![Rect::shade(0.0, 0.0, w, h, 0.75)],
        '▔' => vec![Rect::new(0.0, 0.0, w, e(1.0))],
        '▕' => vec![Rect::new(w - e(1.0), 0.0, e(1.0), h)],
        '▖' => vec![Rect::new(0.0, hh, hw, hh)],
        '▗' => vec![Rect::new(hw, hh, hw, hh)],
        '▘' => vec![Rect::new(0.0, 0.0, hw, hh)],
        '▝' => vec![Rect::new(hw, 0.0, hw, hh)],
        '▙' => vec![Rect::new(0.0, 0.0, hw, h), Rect::new(hw, hh, hw, hh)],
        '▚' => vec![Rect::new(0.0, 0.0, hw, hh), Rect::new(hw, hh, hw, hh)],
        '▛' => vec![Rect::new(0.0, 0.0, w, hh), Rect::new(0.0, hh, hw, hh)],
        '▜' => vec![Rect::new(0.0, 0.0, w, hh), Rect::new(hw, hh, hw, hh)],
        '▞' => vec![Rect::new(hw, 0.0, hw, hh), Rect::new(0.0, hh, hw, hh)],
        '▟' => vec![Rect::new(hw, 0.0, hw, h), Rect::new(0.0, hh, hw, hh)],
        _ => Vec::new(),
    }
}

const NONE: u8 = 0;
const LIGHT: u8 = 1;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

const LIGHT_PX: f32 = 1.0;
const HEAVY_PX: f32 = 2.0;
const DOUBLE_GAP: f32 = 1.0;

/// Left, right, up and down arms for U+2500..U+257F, two bits each.
const BOX_ARMS: [u8; 0x80] = [
    0x05, 0x0a, 0x50, 0xa0, 0x05, 0x0a, 0x50, 0xa0, // 2500
    0x05, 0x0a, 0x50, 0xa0, 0x44, 0x48, 0x84, 0x88, // 2508
    0x41, 0x42, 0x81, 0x82, 0x14, 0x18, 0x24, 0x28, // 2510
    0x11, 0x12, 0x21, 0x22, 0x54, 0x58, 0x64, 0x94, // 2518
    0xa4, 0x68, 0x98, 0xa8, 0x51, 0x52, 0x61, 0x91, // 2520
    0xa1, 0x62, 0x92, 0xa2, 0x45, 0x46, 0x49, 0x4a, // 2528
    0x85, 0x86, 0x89, 0x8a, 0x15, 0x16, 0x19, 0x1a, // 2530
    0x25, 0x26, 0x29, 0x2a, 0x55, 0x56, 0x59, 0x5a, // 2538
    0x65, 0x95, 0xa5, 0x66, 0x69, 0x96, 0x99, 0x6a, // 2540
    0x9a, 0xa6, 0xa9, 0xaa, 0x0f, 0x0f, 0xf0, 0xf0, // 2548
    0x0f, 0xf0, 0x4c, 0xc4, 0xcc, 0x43, 0xc1, 0xc3, // 2550
    0x1c, 0x34, 0x3c, 0x13, 0x31, 0x33, 0x5c, 0xf4, // 2558
    0xfc, 0x53, 0xf1, 0xf3, 0x4f, 0xc5, 0xcf, 0x1f, // 2560
    0x35, 0x3f, 0x5f, 0xf5, 0xff, 0x44, 0x41, 0x11, // 2568
    0x14, 0x00, 0x00, 0x00, 0x01, 0x10, 0x04, 0x40, // 2570
    0x02, 0x20, 0x08, 0x80, 0x09, 0x90, 0x06, 0x60, // 2578
];

fn arm(arms: u8, shift: u8) -> u8 {
    (arms >> shift) & 3
}

/// Dashed lines have no joins, so they bypass the arm walk.
fn dash(cp: u32) -> Option<(usize, bool, f32)> {
    let (count, vertical, heavy) = match cp {
        0x2504 => (3, false, false),
        0x2505 => (3, false, true),
        0x2506 => (3, true, false),
        0x2507 => (3, true, true),
        0x2508 => (4, false, false),
        0x2509 => (4, false, true),
        0x250a => (4, true, false),
        0x250b => (4, true, true),
        0x254c => (2, false, false),
        0x254d => (2, false, true),
        0x254e => (2, true, false),
        0x254f => (2, true, true),
        _ => return None,
    };
    Some((count, vertical, if heavy { HEAVY_PX } else { LIGHT_PX }))
}

fn box_drawing(cp: u32, w: f32, h: f32, out: &mut Vec<Rect>) {
    if let Some((count, vertical, thickness)) = dash(cp) {
        // End on a gap so the next cell's first dash follows a gap instead of
        // fusing with this cell's last one at the boundary.
        let parts = count * 2;
        let length = if vertical { h } else { w };
        let segment = length / parts as f32;
        for part in (0..parts).step_by(2) {
            let offset = part as f32 * segment;
            if vertical {
                out.push(Rect::new(
                    w / 2.0 - thickness / 2.0,
                    offset,
                    thickness,
                    segment,
                ));
            } else {
                out.push(Rect::new(
                    offset,
                    h / 2.0 - thickness / 2.0,
                    segment,
                    thickness,
                ));
            }
        }
        return;
    }

    let arms = BOX_ARMS[(cp - 0x2500) as usize];
    let (l, r, u, d) = (arm(arms, 0), arm(arms, 2), arm(arms, 4), arm(arms, 6));
    if l == NONE && r == NONE && u == NONE && d == NONE {
        return;
    }

    let (cx, cy) = (w / 2.0, h / 2.0);

    // A double corner is two nested Ls. Each L joins the horizontal and
    // vertical track on the same side of the centre, which is what keeps the
    // outer and inner corner apart instead of crossing both tracks.
    let h_count = (l != NONE) as u8 + (r != NONE) as u8;
    let v_count = (u != NONE) as u8 + (d != NONE) as u8;
    let all_double = [l, r, u, d].iter().all(|&a| a == NONE || a == DOUBLE);
    if h_count == 1 && v_count == 1 && all_double {
        let sx = if l != NONE { 1.0 } else { -1.0 };
        let sy = if u != NONE { 1.0 } else { -1.0 };
        for (sx, sy) in [(sx, sy), (-sx, -sy)] {
            let (x0, x1) = if r != NONE {
                (cx + sx * DOUBLE_GAP, w)
            } else {
                (0.0, cx + sx * DOUBLE_GAP)
            };
            out.push(Rect::new(x0, cy + sy * DOUBLE_GAP - 0.5, x1 - x0, 1.0));
            let (y0, y1) = if d != NONE {
                (cy + sy * DOUBLE_GAP, h)
            } else {
                (0.0, cy + sy * DOUBLE_GAP)
            };
            out.push(Rect::new(cx + sx * DOUBLE_GAP - 0.5, y0, 1.0, y1 - y0));
        }
        return;
    }

    // Mixed single and double arms have to join across the gap between the
    // double tracks. A small centre patch keeps the corner or branch closed;
    // the all-double corner above already has its own nested Ls.
    let mixed = !all_double
        && [l, r, u, d].contains(&DOUBLE)
        && ([l, r, u, d].contains(&LIGHT) || [l, r, u, d].contains(&HEAVY));
    if mixed {
        let reach = DOUBLE_GAP + 0.5;
        out.push(Rect::new(cx - reach, cy - reach, reach * 2.0, reach * 2.0));
    }

    // A straight run is one fill instead of two halves. The remaining arms
    // butt at the cell centre, which is where a corner's two strokes meet.
    if l != NONE && r != NONE && l == r {
        horizontal(out, l, 0.0, w, cy);
    } else {
        if l != NONE {
            horizontal(out, l, 0.0, cx, cy);
        }
        if r != NONE {
            horizontal(out, r, cx, w, cy);
        }
    }
    if u != NONE && d != NONE && u == d {
        vertical(out, u, cx, 0.0, h);
    } else {
        if u != NONE {
            vertical(out, u, cx, 0.0, cy);
        }
        if d != NONE {
            vertical(out, d, cx, cy, h);
        }
    }
}

fn horizontal(out: &mut Vec<Rect>, weight: u8, x0: f32, x1: f32, cy: f32) {
    let width = x1 - x0;
    match weight {
        LIGHT => out.push(Rect::new(x0, cy - LIGHT_PX / 2.0, width, LIGHT_PX)),
        HEAVY => out.push(Rect::new(x0, cy - HEAVY_PX / 2.0, width, HEAVY_PX)),
        DOUBLE => {
            out.push(Rect::new(x0, cy - DOUBLE_GAP - 0.5, width, 1.0));
            out.push(Rect::new(x0, cy + DOUBLE_GAP - 0.5, width, 1.0));
        }
        _ => {}
    }
}

fn vertical(out: &mut Vec<Rect>, weight: u8, cx: f32, y0: f32, y1: f32) {
    let height = y1 - y0;
    match weight {
        LIGHT => out.push(Rect::new(cx - LIGHT_PX / 2.0, y0, LIGHT_PX, height)),
        HEAVY => out.push(Rect::new(cx - HEAVY_PX / 2.0, y0, HEAVY_PX, height)),
        DOUBLE => {
            out.push(Rect::new(cx - DOUBLE_GAP - 0.5, y0, 1.0, height));
            out.push(Rect::new(cx + DOUBLE_GAP - 0.5, y0, 1.0, height));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(ch: char) -> Vec<Rect> {
        super::rects(ch, 8.0, 16.0).unwrap()
    }

    #[test]
    fn blocks_fill_exactly_the_cell() {
        assert_eq!(shape('█'), [Rect::new(0.0, 0.0, 8.0, 16.0)]);
        assert_eq!(shape('▀'), [Rect::new(0.0, 0.0, 8.0, 8.0)]);
        assert_eq!(shape('▄'), [Rect::new(0.0, 8.0, 8.0, 8.0)]);
        assert_eq!(shape('▌'), [Rect::new(0.0, 0.0, 4.0, 16.0)]);
        assert_eq!(shape('▐'), [Rect::new(4.0, 0.0, 4.0, 16.0)]);
    }

    #[test]
    fn vertical_blocks_meet_the_next_cell() {
        let top = shape('▀');
        let bottom = shape('▄');
        assert_eq!(top[0].y + top[0].h, bottom[0].y);
        assert_eq!(bottom[0].y + bottom[0].h, 16.0);
    }

    #[test]
    fn quadrants_are_exact_quarters() {
        assert_eq!(shape('▖'), [Rect::new(0.0, 8.0, 4.0, 8.0)]);
        assert_eq!(shape('▗'), [Rect::new(4.0, 8.0, 4.0, 8.0)]);
        assert_eq!(shape('▘'), [Rect::new(0.0, 0.0, 4.0, 8.0)]);
        assert_eq!(shape('▝'), [Rect::new(4.0, 0.0, 4.0, 8.0)]);
    }

    #[test]
    fn a_vertical_border_reaches_both_cell_edges() {
        let line = shape('┃');
        assert_eq!(line, [Rect::new(3.0, 0.0, 2.0, 16.0)]);
    }

    #[test]
    fn a_horizontal_border_reaches_both_cell_edges() {
        let line = shape('─');
        assert_eq!(line, [Rect::new(0.0, 7.5, 8.0, 1.0)]);
    }

    #[test]
    fn a_corner_joins_both_arms() {
        let corner = shape('┌');
        assert_eq!(corner.len(), 2);
        assert_eq!(corner[0], Rect::new(4.0, 7.5, 4.0, 1.0));
        assert_eq!(corner[1], Rect::new(3.5, 8.0, 1.0, 8.0));
    }

    #[test]
    fn every_box_drawing_character_has_a_shape_except_diagonals() {
        for cp in 0x2500..0x2580 {
            let ch = char::from_u32(cp).unwrap();
            if matches!(ch, '╱' | '╲' | '╳') {
                assert!(super::rects(ch, 8.0, 16.0).is_none(), "{ch}");
            } else {
                assert!(!super::rects(ch, 8.0, 16.0).unwrap().is_empty(), "{ch}");
            }
        }
    }

    #[test]
    fn mixed_single_and_double_joints_are_closed() {
        for ch in ['╒', '╓', '╞', '╥', '╪'] {
            let rects = shape(ch);
            assert!(
                rects
                    .iter()
                    .any(|r| r.x <= 4.0 && r.x + r.w >= 4.0 && r.y <= 8.0 && r.y + r.h >= 8.0),
                "{ch} must cover the cell centre"
            );
        }
    }

    #[test]
    fn dashes_end_on_a_gap_so_cells_tile() {
        let dash = shape('┄');
        assert_eq!(dash.len(), 3);
        let last = dash.last().unwrap();
        assert!(
            last.x + last.w < 8.0,
            "the last dash leaves a gap at the edge"
        );
        assert_eq!(last.y, 7.5);
        assert_eq!(last.h, 1.0);
    }

    #[test]
    fn shades_are_translucent_fills() {
        assert_eq!(shape('░')[0].alpha, 0.25);
        assert_eq!(shape('▒')[0].alpha, 0.50);
        assert_eq!(shape('▓')[0].alpha, 0.75);
    }

    #[test]
    fn box_drawing_characters_outside_the_grid_stay_text() {
        assert!(!is_cell_glyph('a'));
        assert!(!is_cell_glyph('╱'));
        assert!(is_cell_glyph('┃'));
        assert!(is_cell_glyph('█'));
    }
}
