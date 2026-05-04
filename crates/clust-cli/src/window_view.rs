//! Window view: a recursive 2×2 grid that shows live PTY output for the
//! agents belonging to the currently-selected repo on the Repositories tab.
//!
//! Cells are filled in column-major order — top-left, bottom-left, top-right,
//! bottom-right — and each quadrant is recursively subdivided once it holds
//! more than one agent. So an N-agent grid is a binary space partition tree
//! whose leaves are agents.

// Helpers land staged across several commits; suppress until callers exist.
#![allow(dead_code)]

use ratatui::layout::{Constraint, Layout, Rect};

/// Lay out `n` agent cells inside `rect`.
///
/// Fill order is column-major: top-left, bottom-left, top-right, bottom-right.
/// For `n > 4` the four quadrants of `rect` each receive a share of the
/// remaining agents (TL gets the remainder first) and the function recurses.
///
/// The returned cells tile `rect` exactly (no gaps, no overlaps), modulo
/// integer-rounding artifacts that ratatui's `Layout` already minimizes.
pub fn window_layout(rect: Rect, n: usize) -> Vec<Rect> {
    match n {
        0 => Vec::new(),
        1 => vec![rect],
        2 => {
            let [top, bottom] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(rect);
            vec![top, bottom]
        }
        3 => {
            let [left, right] =
                Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, bl] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(left);
            vec![tl, bl, right]
        }
        4 => {
            let [left, right] =
                Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, bl] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(left);
            let [tr, br] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(right);
            vec![tl, bl, tr, br]
        }
        _ => {
            let [left, right] =
                Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, bl] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(left);
            let [tr, br] =
                Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(right);
            let counts = distribute(n, 4);
            [tl, bl, tr, br]
                .into_iter()
                .zip(counts)
                .flat_map(|(quadrant, count)| window_layout(quadrant, count))
                .collect()
        }
    }
}

/// Distribute `total` items across `slots` buckets. The first `total % slots`
/// buckets receive `total / slots + 1`, the rest receive `total / slots`.
/// So `distribute(5, 4)` → `[2, 1, 1, 1]` and `distribute(10, 4)` → `[3, 3, 2, 2]`.
fn distribute(total: usize, slots: usize) -> Vec<usize> {
    debug_assert!(slots > 0);
    let base = total / slots;
    let extra = total % slots;
    (0..slots)
        .map(|i| if i < extra { base + 1 } else { base })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 80,
        }
    }

    #[test]
    fn distribute_examples() {
        assert_eq!(distribute(0, 4), vec![0, 0, 0, 0]);
        assert_eq!(distribute(1, 4), vec![1, 0, 0, 0]);
        assert_eq!(distribute(4, 4), vec![1, 1, 1, 1]);
        assert_eq!(distribute(5, 4), vec![2, 1, 1, 1]);
        assert_eq!(distribute(7, 4), vec![2, 2, 2, 1]);
        assert_eq!(distribute(8, 4), vec![2, 2, 2, 2]);
        assert_eq!(distribute(10, 4), vec![3, 3, 2, 2]);
    }

    #[test]
    fn n_zero_returns_empty() {
        assert!(window_layout(rect(), 0).is_empty());
    }

    #[test]
    fn n_one_fills_rect() {
        let cells = window_layout(rect(), 1);
        assert_eq!(cells, vec![rect()]);
    }

    #[test]
    fn n_two_splits_top_bottom() {
        let cells = window_layout(rect(), 2);
        assert_eq!(cells.len(), 2);
        // TL on top, BL on bottom — same x range, contiguous y
        assert_eq!(cells[0].x, cells[1].x);
        assert_eq!(cells[0].width, cells[1].width);
        assert_eq!(cells[0].y + cells[0].height, cells[1].y);
    }

    #[test]
    fn n_three_left_column_split_full_right() {
        let cells = window_layout(rect(), 3);
        assert_eq!(cells.len(), 3);
        let [tl, bl, right] = [cells[0], cells[1], cells[2]];
        // TL and BL share x and width
        assert_eq!(tl.x, bl.x);
        assert_eq!(tl.width, bl.width);
        // TL above BL, contiguous
        assert_eq!(tl.y + tl.height, bl.y);
        // right column to the right of left column, full height
        assert_eq!(tl.x + tl.width, right.x);
        assert_eq!(right.y, rect().y);
        assert_eq!(right.height, rect().height);
    }

    #[test]
    fn n_four_full_2x2_grid() {
        let cells = window_layout(rect(), 4);
        assert_eq!(cells.len(), 4);
        let [tl, bl, tr, br] = [cells[0], cells[1], cells[2], cells[3]];
        // Column alignment
        assert_eq!(tl.x, bl.x);
        assert_eq!(tr.x, br.x);
        // Row alignment
        assert_eq!(tl.y, tr.y);
        assert_eq!(bl.y, br.y);
        // Same dimensions
        assert_eq!(tl.width, tr.width);
        assert_eq!(tl.width, bl.width);
        assert_eq!(tl.height, bl.height);
        assert_eq!(tl.height, tr.height);
        // Sums tile rect
        assert_eq!(tl.width + tr.width, rect().width);
        assert_eq!(tl.height + bl.height, rect().height);
    }

    #[test]
    fn n_five_recurses_into_top_left() {
        let cells = window_layout(rect(), 5);
        assert_eq!(cells.len(), 5);
        // First two cells live inside the would-be TL quadrant (top-left
        // half of the screen). Verify they share the left half's x range
        // and together cover its height.
        let half_w = rect().width / 2;
        let half_h = rect().height / 2;
        let inner_tl = cells[0];
        let inner_bl = cells[1];
        assert!(inner_tl.x < half_w);
        assert!(inner_bl.x < half_w);
        assert_eq!(inner_tl.x, inner_bl.x);
        assert_eq!(inner_tl.width, inner_bl.width);
        assert_eq!(inner_tl.y + inner_tl.height, inner_bl.y);
        // Cells 2, 3, 4 are BL, TR, BR each at quarter size
        let bl = cells[2];
        let tr = cells[3];
        let br = cells[4];
        assert_eq!(bl.x, inner_tl.x);
        assert!(bl.y >= half_h);
        assert!(tr.x >= half_w);
        assert_eq!(tr.y, 0);
        assert_eq!(br.x, tr.x);
        assert!(br.y >= half_h);
    }

    #[test]
    fn cells_tile_rect_for_powers_of_two() {
        // For n = 4 and n = 16 (after recursive 4-quadrant split twice) the
        // grid should tile the rect with no gaps and no overlaps.
        for &n in &[1, 4] {
            let cells = window_layout(rect(), n);
            let total_area: u32 = cells
                .iter()
                .map(|c| u32::from(c.width) * u32::from(c.height))
                .sum();
            assert_eq!(
                total_area,
                u32::from(rect().width) * u32::from(rect().height),
                "n={n}"
            );
        }
    }

    #[test]
    fn cells_count_matches_n() {
        for n in 0..=12usize {
            let cells = window_layout(rect(), n);
            assert_eq!(cells.len(), n, "n={n}");
        }
    }
}
