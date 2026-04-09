use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame,
};

use crate::theme;

/// Result of processing a key event in the batch deps modal.
pub enum BatchDepsResult {
    Pending,
    Cancelled,
    /// User confirmed: returns selected hub batch IDs.
    Completed(Vec<String>),
}

/// An available batch that can be selected as a dependency.
struct DepsItem {
    hub_batch_id: String,
    title: String,
    selected: bool,
    /// This batch's own depends_on list (for cycle detection).
    depends_on: Vec<String>,
}

/// Multi-select modal for choosing batch dependencies.
pub struct BatchDepsModal {
    batch_idx: usize,
    batch_title: String,
    /// Hub batch ID of the batch being edited (for cycle detection).
    batch_hub_id: Option<String>,
    items: Vec<DepsItem>,
    selected_idx: usize,
}

impl BatchDepsModal {
    /// Create a new dependency modal for a batch.
    ///
    /// `batch_idx` — local index of the batch being edited.
    /// `batch_title` — title for display.
    /// `batch_hub_id` — hub ID of the batch (needed for cycle detection).
    /// `current_deps` — currently selected dependency hub IDs.
    /// `all_batches` — all other batches: (hub_batch_id, title, depends_on).
    pub fn new(
        batch_idx: usize,
        batch_title: String,
        batch_hub_id: Option<String>,
        current_deps: &[String],
        all_batches: Vec<(String, String, Vec<String>)>,
    ) -> Self {
        let items = all_batches
            .into_iter()
            .map(|(hub_id, title, deps)| DepsItem {
                selected: current_deps.contains(&hub_id),
                hub_batch_id: hub_id,
                title,
                depends_on: deps,
            })
            .collect();
        Self {
            batch_idx,
            batch_title,
            batch_hub_id,
            items,
            selected_idx: 0,
        }
    }

    pub fn batch_idx(&self) -> usize {
        self.batch_idx
    }

    pub fn handle_key(&mut self, code: KeyCode) -> BatchDepsResult {
        match code {
            KeyCode::Up => {
                self.selected_idx = self.selected_idx.saturating_sub(1);
                BatchDepsResult::Pending
            }
            KeyCode::Down => {
                if !self.items.is_empty() {
                    self.selected_idx = (self.selected_idx + 1).min(self.items.len() - 1);
                }
                BatchDepsResult::Pending
            }
            KeyCode::Char(' ') => {
                if let Some(item) = self.items.get(self.selected_idx) {
                    if item.selected {
                        // Always allow deselecting
                        self.items[self.selected_idx].selected = false;
                    } else if !self.would_create_cycle(&item.hub_batch_id) {
                        self.items[self.selected_idx].selected = true;
                    }
                }
                BatchDepsResult::Pending
            }
            KeyCode::Enter => {
                let selected: Vec<String> = self
                    .items
                    .iter()
                    .filter(|i| i.selected)
                    .map(|i| i.hub_batch_id.clone())
                    .collect();
                BatchDepsResult::Completed(selected)
            }
            KeyCode::Esc => BatchDepsResult::Cancelled,
            _ => BatchDepsResult::Pending,
        }
    }

    /// Check if adding `candidate_id` as a dependency would create a cycle.
    /// A cycle exists if, starting from `candidate_id`, we can reach our own batch
    /// by following the depends_on chains.
    fn would_create_cycle(&self, candidate_id: &str) -> bool {
        let our_id = match &self.batch_hub_id {
            Some(id) => id.as_str(),
            None => return false, // No hub ID yet, can't form a cycle
        };

        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![candidate_id.to_string()];

        while let Some(current) = stack.pop() {
            if current == our_id {
                return true;
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            // Find this batch's dependencies
            if let Some(item) = self.items.iter().find(|i| i.hub_batch_id == current) {
                for dep in &item.depends_on {
                    stack.push(dep.clone());
                }
            }
        }
        false
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        if self.items.is_empty() {
            self.render_empty(frame, area);
            return;
        }

        let title = format!("Dependencies \u{2014} {}", self.batch_title);
        let hint = "Space toggle  Enter confirm  Esc cancel";

        let label_max: usize = self.items.iter().map(|i| i.title.chars().count()).max().unwrap_or(0);
        // "  [x] label  " => 2 + 3 + 1 + label + 2
        let item_width = 6 + label_max + 2;
        let content_width = item_width
            .max(title.chars().count() + 4)
            .max(hint.chars().count() + 2);
        let modal_width = (content_width + 2) as u16;
        let modal_height = (self.items.len() + 4) as u16; // borders + title + hint + items

        let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
            .flex(Flex::Center)
            .areas(area);

        let modal_rect = Rect {
            x: horz_area.x,
            y: area.y + area.height.saturating_sub(modal_height) / 2,
            width: modal_width.min(area.width),
            height: modal_height.min(area.height),
        };

        frame.render_widget(Clear, modal_rect);

        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    title,
                    Style::default()
                        .fg(theme::R_TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
            ]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
            .padding(Padding::new(1, 1, 0, 0))
            .style(Style::default().bg(theme::R_BG_OVERLAY));

        let inner = block.inner(modal_rect);
        frame.render_widget(block, modal_rect);

        let mut lines: Vec<Line> = Vec::new();

        for (i, item) in self.items.iter().enumerate() {
            let is_selected = i == self.selected_idx;
            let bg = if is_selected {
                theme::R_BG_HOVER
            } else {
                theme::R_BG_OVERLAY
            };

            let check = if item.selected { "[x]" } else { "[ ]" };
            let check_fg = if item.selected {
                theme::R_SUCCESS
            } else {
                theme::R_TEXT_TERTIARY
            };

            let label_fg = if is_selected {
                theme::R_TEXT_PRIMARY
            } else {
                theme::R_TEXT_SECONDARY
            };

            let spans = vec![
                Span::styled(format!(" {check} "), Style::default().fg(check_fg).bg(bg)),
                Span::styled(
                    item.title.clone(),
                    Style::default().fg(label_fg).bg(bg),
                ),
            ];

            let content_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let remaining = (inner.width as usize).saturating_sub(content_len);
            let mut row_spans = spans;
            if remaining > 0 {
                row_spans.push(Span::styled(
                    " ".repeat(remaining),
                    Style::default().bg(bg),
                ));
            }
            lines.push(Line::from(row_spans));
        }

        // Hint line
        lines.push(Line::from(Span::styled(
            format!(" {hint}"),
            Style::default().fg(theme::R_TEXT_TERTIARY).bg(theme::R_BG_OVERLAY),
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let title = format!("Dependencies \u{2014} {}", self.batch_title);
        let msg = "No other batches available";
        let modal_width = (title.chars().count() + 6).max(msg.chars().count() + 4) as u16;
        let modal_height = 4u16;

        let [horz_area] = Layout::horizontal([Constraint::Length(modal_width)])
            .flex(Flex::Center)
            .areas(area);

        let modal_rect = Rect {
            x: horz_area.x,
            y: area.y + area.height.saturating_sub(modal_height) / 2,
            width: modal_width.min(area.width),
            height: modal_height.min(area.height),
        };

        frame.render_widget(Clear, modal_rect);

        let block = Block::default()
            .title(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    title,
                    Style::default()
                        .fg(theme::R_TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", Style::default()),
            ]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_TEXT_TERTIARY))
            .padding(Padding::new(1, 1, 0, 0))
            .style(Style::default().bg(theme::R_BG_OVERLAY));

        let inner = block.inner(modal_rect);
        frame.render_widget(block, modal_rect);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(theme::R_TEXT_TERTIARY).bg(theme::R_BG_OVERLAY),
            ))),
            inner,
        );
    }
}
