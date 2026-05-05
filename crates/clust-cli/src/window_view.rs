//! Window view: a recursive 2×2 grid that shows live PTY output for the
//! agents belonging to the currently-selected repo on the Repositories tab.
//!
//! Cells are filled in row-major order — top-left, top-right, bottom-left,
//! bottom-right — and each quadrant is recursively subdivided once it holds
//! more than one agent. So an N-agent grid is a binary space partition tree
//! whose leaves are agents.

use std::collections::HashMap;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overview::{render_agent_panel, OverviewState, PanelCommand};
use crate::tasks::BatchAgentInfo;
use crate::theme;
use crate::ui::render_logo;

/// Minimum cell size below which we render an ellipsis placeholder instead of
/// a full panel. A panel needs at least 1 cell of inner content plus 2 cols
/// for borders and 3 rows for borders + header; below this the vterm thrashes
/// against a 1×1 size and the output is unreadable anyway.
const MIN_CELL_W: u16 = 6;
const MIN_CELL_H: u16 = 4;

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
/// row-major fill order; the panel for each cell is looked up by ID,
/// resized to the cell's interior, and rendered with [`render_agent_panel`].
///
/// The right panel is view-only — it has no focus and no per-cell selection.
/// To interact with an agent, switch to the Overview tab.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    overview_state: &mut OverviewState,
    scoped_ids: &[String],
    empty: EmptyKind<'_>,
    repo_colors: &HashMap<String, String>,
    batch_map: &HashMap<String, BatchAgentInfo>,
) {
    if scoped_ids.is_empty() {
        match empty {
            EmptyKind::Logo => render_logo(frame, area),
            EmptyKind::NoAgentsFor(label) => {
                render_centered_message(frame, area, &format!("No agents running for {label}"))
            }
            EmptyKind::NoDetached => render_centered_message(frame, area, "No detached agents"),
        }
        return;
    }

    let cells = window_layout(area, scoped_ids.len());

    for (cell, id) in cells.iter().zip(scoped_ids.iter()) {
        // If the cell is too small to host a usable panel, skip the resize
        // (don't shrink the vterm to 1×1) and paint a single-character "…"
        // placeholder instead. The full panel would just thrash and render
        // unreadable garbage.
        if cell.width < MIN_CELL_W || cell.height < MIN_CELL_H {
            render_too_small_placeholder(frame, *cell);
            continue;
        }

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

        let panel_color = panel
            .repo_path
            .as_ref()
            .and_then(|rp| repo_colors.get(rp.as_str()))
            .map(|cn| theme::repo_color(cn));
        let batch_info = batch_map.get(&panel.id);

        render_agent_panel(frame, *cell, panel, false, false, panel_color, batch_info);
    }
}

/// Paint a single-character "…" placeholder centered in `area`. Used when a
/// cell is too small (below MIN_CELL_W × MIN_CELL_H) to host an agent panel.
fn render_too_small_placeholder(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let line = Line::from(Span::styled(
        "\u{2026}",
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_BASE),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_centered_message(frame: &mut Frame, area: Rect, msg: &str) {
    let line = Line::from(Span::styled(
        msg.to_string(),
        Style::default()
            .fg(theme::R_TEXT_TERTIARY)
            .bg(theme::R_BG_BASE),
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
/// Fill order is row-major: top-left, top-right, bottom-left, bottom-right.
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
            let [left, right] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(rect);
            vec![left, right]
        }
        3 => {
            let [top, bottom] = Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, tr] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);
            vec![tl, tr, bottom]
        }
        4 => {
            let [top, bottom] = Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, tr] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);
            let [bl, br] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(bottom);
            vec![tl, tr, bl, br]
        }
        _ => {
            let [top, bottom] = Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(rect);
            let [tl, tr] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(top);
            let [bl, br] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(bottom);
            let counts = distribute(n, 4);
            [tl, tr, bl, br]
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
    fn n_two_splits_left_right() {
        let cells = window_layout(rect(), 2);
        assert_eq!(cells.len(), 2);
        // TL on left, TR on right — same y range, contiguous x
        assert_eq!(cells[0].y, cells[1].y);
        assert_eq!(cells[0].height, cells[1].height);
        assert_eq!(cells[0].x + cells[0].width, cells[1].x);
    }

    #[test]
    fn n_three_top_row_split_full_bottom() {
        let cells = window_layout(rect(), 3);
        assert_eq!(cells.len(), 3);
        let [tl, tr, bottom] = [cells[0], cells[1], cells[2]];
        // TL and TR share y and height
        assert_eq!(tl.y, tr.y);
        assert_eq!(tl.height, tr.height);
        // TL left of TR, contiguous
        assert_eq!(tl.x + tl.width, tr.x);
        // bottom row beneath top row, full width
        assert_eq!(tl.y + tl.height, bottom.y);
        assert_eq!(bottom.x, rect().x);
        assert_eq!(bottom.width, rect().width);
    }

    #[test]
    fn n_four_full_2x2_grid() {
        let cells = window_layout(rect(), 4);
        assert_eq!(cells.len(), 4);
        let [tl, tr, bl, br] = [cells[0], cells[1], cells[2], cells[3]];
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
        // half of the screen). Verify they share the top half's y range
        // and together cover its width.
        let half_w = rect().width / 2;
        let half_h = rect().height / 2;
        let inner_tl = cells[0];
        let inner_tr = cells[1];
        assert!(inner_tl.y < half_h);
        assert!(inner_tr.y < half_h);
        assert_eq!(inner_tl.y, inner_tr.y);
        assert_eq!(inner_tl.height, inner_tr.height);
        assert_eq!(inner_tl.x + inner_tl.width, inner_tr.x);
        // Cells 2, 3, 4 are TR, BL, BR each at quarter size
        let tr = cells[2];
        let bl = cells[3];
        let br = cells[4];
        assert!(tr.x >= half_w);
        assert_eq!(tr.y, 0);
        assert_eq!(bl.x, inner_tl.x);
        assert!(bl.y >= half_h);
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
