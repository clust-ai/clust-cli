//! Window view: a recursive 2×2 grid that shows live PTY output for the
//! agents belonging to the currently-selected repo on the Repositories tab.
//!
//! Cells are filled in column-major order — top-left, bottom-left, top-right,
//! bottom-right — and each quadrant is recursively subdivided once it holds
//! more than one agent. So an N-agent grid is a binary space partition tree
//! whose leaves are agents.

use std::collections::HashMap;

use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overview::{render_agent_panel, OverviewState, PanelCommand};
use crate::tasks::BatchAgentInfo;
use crate::theme;
use crate::ui::render_logo;

/// Persistent state for the Window view across frames.
#[derive(Default, Clone)]
pub struct WindowGridState {
    /// Index of the currently-selected cell within the scoped agent list.
    pub selected_idx: usize,
}

impl WindowGridState {
    /// Clamp `selected_idx` into the valid range for the current cell count.
    pub fn clamp(&mut self, cell_count: usize) {
        if cell_count == 0 {
            self.selected_idx = 0;
        } else if self.selected_idx >= cell_count {
            self.selected_idx = cell_count - 1;
        }
    }
}

/// What to show when there are no agents to render in the grid.
#[derive(Clone, Copy)]
pub enum EmptyKind<'a> {
    /// Show the centered logo (used for the "Add Repository" sentinel).
    Logo,
    /// Show a centered "no agents" message scoped to the given label.
    NoAgentsFor(&'a str),
    /// Show a centered "no detached agents" message.
    NoDetached,
}

/// Render the Window view for the Repositories tab right panel.
///
/// `scoped_ids` is the ordered list of agent IDs that belong to the
/// currently-selected repo on the left panel. Each ID must exist in
/// `overview_state.panels`. Cells are laid out via [`window_layout`] in
/// column-major fill order; the panel for each cell is looked up by ID,
/// resized to the cell's interior, and rendered with [`render_agent_panel`].
///
/// Populates `click_map.window_cells` so the mouse handler can map
/// click positions back to agent IDs.
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    grid: &mut WindowGridState,
    overview_state: &mut OverviewState,
    scoped_ids: &[String],
    empty: EmptyKind<'_>,
    focused: bool,
    repo_colors: &HashMap<String, String>,
    batch_map: &HashMap<String, BatchAgentInfo>,
    click_map: &mut ClickMapWindow<'_>,
) {
    grid.clamp(scoped_ids.len());

    if scoped_ids.is_empty() {
        match empty {
            EmptyKind::Logo => render_logo(frame, area),
            EmptyKind::NoAgentsFor(label) => render_centered_message(
                frame,
                area,
                &format!("No agents running for {label}"),
            ),
            EmptyKind::NoDetached => render_centered_message(frame, area, "No detached agents"),
        }
        // Focus indicator in the top-right corner.
        render_focus_dot(frame, area, focused);
        return;
    }

    let cells = window_layout(area, scoped_ids.len());

    for (idx, (cell, id)) in cells.iter().zip(scoped_ids.iter()).enumerate() {
        // Find the panel by id. If a sync hasn't caught up yet, skip the cell.
        let panel_idx = match overview_state.panels.iter().position(|p| &p.id == id) {
            Some(i) => i,
            None => continue,
        };
        let panel = &mut overview_state.panels[panel_idx];

        // Resize this panel's vterm to the cell interior (border + header eat
        // 3 rows + 2 cols). Send a SIGWINCH first; only update local vterm if
        // the send succeeds, mirroring `OverviewState::resize_panels_to`.
        let inner_w = cell.width.saturating_sub(2);
        let inner_h = cell.height.saturating_sub(3);
        let target_cols = inner_w.max(1);
        let target_rows = inner_h.max(1);
        if (panel.vterm.cols() != target_cols as usize
            || panel.vterm.rows() != target_rows as usize)
            && panel
                .command_tx
                .try_send(PanelCommand::Resize {
                    cols: target_cols,
                    rows: target_rows,
                })
                .is_ok()
        {
            panel
                .vterm
                .resize(target_cols as usize, target_rows as usize);
        }

        let is_focused = focused && idx == grid.selected_idx;
        let panel_color = panel
            .repo_path
            .as_ref()
            .and_then(|rp| repo_colors.get(rp.as_str()))
            .map(|cn| theme::repo_color(cn));
        let batch_info = batch_map.get(&panel.id);

        render_agent_panel(
            frame,
            *cell,
            panel,
            is_focused,
            false,
            panel_color,
            batch_info,
        );

        click_map.window_cells.push((*cell, id.clone()));
    }

    render_focus_dot(frame, area, focused);
}

/// Tiny click-map view used by [`render`] so the module doesn't depend on
/// every field of the parent crate's `ClickMap`.
pub struct ClickMapWindow<'a> {
    pub window_cells: &'a mut Vec<(Rect, String)>,
}

fn render_focus_dot(frame: &mut Frame, area: Rect, focused: bool) {
    let color = if focused {
        theme::R_ACCENT_BRIGHT
    } else {
        theme::R_TEXT_TERTIARY
    };
    let indicator = Paragraph::new(Span::styled(
        "●",
        Style::default().fg(color).bg(theme::R_BG_BASE),
    ))
    .alignment(Alignment::Right);
    let strip = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    frame.render_widget(indicator, strip);
}

fn render_centered_message(frame: &mut Frame, area: Rect, msg: &str) {
    let line = Line::from(Span::styled(
        msg.to_string(),
        Style::default().fg(theme::R_TEXT_TERTIARY).bg(theme::R_BG_BASE),
    ));
    let [vert] = Layout::vertical([Constraint::Length(1)])
        .flex(Flex::Center)
        .areas(area);
    let [horiz] = Layout::horizontal([Constraint::Length(msg.chars().count() as u16)])
        .flex(Flex::Center)
        .areas(vert);
    frame.render_widget(Paragraph::new(line), horiz);
}


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

/// Direction for spatial neighbor lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Return the index of the cell to navigate to from `current` when the user
/// presses an arrow key in direction `dir`. If no candidate exists in that
/// direction, return `current` (no-op — wrap-around is jarring in a recursive
/// grid).
///
/// Scoring favors cells whose center lies in the requested half-plane and
/// minimizes Manhattan distance with a 2× penalty on the perpendicular axis,
/// so that "Right" prefers a cell in the same row over one diagonally placed.
pub fn neighbor(cells: &[Rect], current: usize, dir: Direction) -> usize {
    if cells.is_empty() || current >= cells.len() {
        return current;
    }
    let cur = center(cells[current]);
    let mut best: Option<(usize, i64)> = None;
    for (idx, cell) in cells.iter().enumerate() {
        if idx == current {
            continue;
        }
        let c = center(*cell);
        let in_half_plane = match dir {
            Direction::Up => c.1 < cur.1,
            Direction::Down => c.1 > cur.1,
            Direction::Left => c.0 < cur.0,
            Direction::Right => c.0 > cur.0,
        };
        if !in_half_plane {
            continue;
        }
        let dx = (c.0 - cur.0).abs();
        let dy = (c.1 - cur.1).abs();
        let score = match dir {
            Direction::Up | Direction::Down => dy + 2 * dx,
            Direction::Left | Direction::Right => dx + 2 * dy,
        };
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((idx, score));
        }
    }
    best.map_or(current, |(idx, _)| idx)
}

fn center(r: Rect) -> (i64, i64) {
    (
        i64::from(r.x) + i64::from(r.width) / 2,
        i64::from(r.y) + i64::from(r.height) / 2,
    )
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

    // -------- neighbor() tests --------

    #[test]
    fn neighbor_in_2x2_grid() {
        // 2x2 grid: cells[0]=TL, cells[1]=BL, cells[2]=TR, cells[3]=BR
        let cells = window_layout(rect(), 4);
        // From TL: Right → TR, Down → BL, Up/Left → no move
        assert_eq!(neighbor(&cells, 0, Direction::Right), 2);
        assert_eq!(neighbor(&cells, 0, Direction::Down), 1);
        assert_eq!(neighbor(&cells, 0, Direction::Up), 0);
        assert_eq!(neighbor(&cells, 0, Direction::Left), 0);
        // From BR: Up → TR, Left → BL
        assert_eq!(neighbor(&cells, 3, Direction::Up), 2);
        assert_eq!(neighbor(&cells, 3, Direction::Left), 1);
        // From TR: Left → TL, Down → BR
        assert_eq!(neighbor(&cells, 2, Direction::Left), 0);
        assert_eq!(neighbor(&cells, 2, Direction::Down), 3);
    }

    #[test]
    fn neighbor_prefers_same_row_or_column() {
        // 5-cell layout: TL/BL inside TL-quadrant, then BL/TR/BR full
        // cells[0], [1] inside top-left quadrant (small)
        // cells[2] = bottom-left full, cells[3] = top-right full,
        // cells[4] = bottom-right full
        let cells = window_layout(rect(), 5);
        // From cells[3] (TR full), Down should pick BR (cells[4]),
        // not the smaller top-left subdivisions.
        assert_eq!(neighbor(&cells, 3, Direction::Down), 4);
        // From cells[4] (BR full), Up → TR.
        assert_eq!(neighbor(&cells, 4, Direction::Up), 3);
    }

    #[test]
    fn neighbor_no_movement_returns_current() {
        let cells = window_layout(rect(), 1);
        assert_eq!(neighbor(&cells, 0, Direction::Right), 0);
        assert_eq!(neighbor(&cells, 0, Direction::Up), 0);
    }

    #[test]
    fn neighbor_empty_cells_returns_current() {
        let cells: Vec<Rect> = Vec::new();
        assert_eq!(neighbor(&cells, 0, Direction::Right), 0);
    }

    #[test]
    fn neighbor_out_of_bounds_index_returns_current() {
        let cells = window_layout(rect(), 2);
        assert_eq!(neighbor(&cells, 99, Direction::Right), 99);
    }

    #[test]
    fn neighbor_in_three_cell_layout() {
        // n=3: cells[0]=TL (top-left quarter), cells[1]=BL (bottom-left
        // quarter), cells[2]=full right column.
        let cells = window_layout(rect(), 3);
        // From TL: Right → right column.
        assert_eq!(neighbor(&cells, 0, Direction::Right), 2);
        // From BL: Right → right column.
        assert_eq!(neighbor(&cells, 1, Direction::Right), 2);
        // From right column: Left lands on the left column. Both TL and BL
        // are equidistant from the right column's vertical midpoint, so
        // first-match wins (TL = idx 0).
        assert_eq!(neighbor(&cells, 2, Direction::Left), 0);
        // From TL: Down → BL.
        assert_eq!(neighbor(&cells, 0, Direction::Down), 1);
        // From BL: Up → TL.
        assert_eq!(neighbor(&cells, 1, Direction::Up), 0);
    }
}
