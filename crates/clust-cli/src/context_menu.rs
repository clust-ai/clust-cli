use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Result of processing a key event in the context menu.
pub enum MenuResult {
    /// Key was handled but no action taken yet.
    None,
    /// User selected an item (index).
    Selected(usize),
    /// User dismissed the menu.
    Dismissed,
}

/// A single item in the context menu.
pub struct ContextMenuItem {
    pub label: String,
    /// Optional colored indicator dot shown before the label.
    pub color: Option<Color>,
}

/// Reusable modal context menu with numbered items, arrow navigation, and enter/esc.
pub struct ContextMenu {
    pub title: String,
    /// Optional body text shown between the title and the numbered items.
    pub description: Option<String>,
    pub items: Vec<ContextMenuItem>,
    pub selected_idx: usize,
}

impl ContextMenu {
    /// Create a new context menu with string labels (no color indicators).
    pub fn new(title: &str, labels: Vec<String>) -> Self {
        Self {
            title: title.to_string(),
            description: None,
            items: labels
                .into_iter()
                .map(|label| ContextMenuItem { label, color: None })
                .collect(),
            selected_idx: 0,
        }
    }

    /// Create a new context menu with colored items.
    pub fn with_colors(title: &str, items: Vec<ContextMenuItem>) -> Self {
        Self {
            title: title.to_string(),
            description: None,
            items,
            selected_idx: 0,
        }
    }

    /// Set an optional description shown between the title and items.
    pub fn with_description(mut self, desc: String) -> Self {
        self.description = Some(desc);
        self
    }

    /// Process a key event. Returns the menu result.
    pub fn handle_key(&mut self, code: KeyCode) -> MenuResult {
        match code {
            KeyCode::Up => {
                self.selected_idx = self.selected_idx.saturating_sub(1);
                MenuResult::None
            }
            KeyCode::Down => {
                if !self.items.is_empty() {
                    self.selected_idx = (self.selected_idx + 1).min(self.items.len() - 1);
                }
                MenuResult::None
            }
            KeyCode::Enter => {
                if self.items.is_empty() {
                    MenuResult::Dismissed
                } else {
                    MenuResult::Selected(self.selected_idx)
                }
            }
            KeyCode::Esc => MenuResult::Dismissed,
            KeyCode::Char(c) => {
                if let Some(idx) = char_to_index(c) {
                    if idx < self.items.len() {
                        return MenuResult::Selected(idx);
                    }
                }
                MenuResult::None
            }
            _ => MenuResult::None,
        }
    }

    /// Render the context menu as a centered modal overlay.
    /// Returns `(modal_rect, inner_rect)` for mouse hit-testing.
    pub fn render(&self, frame: &mut Frame, area: Rect) -> (Rect, Rect) {
        // Re-clamp selection in case the items list shrank between the last
        // key event and now (e.g. dynamic items rebuilt on tick). If items is
        // empty we do not render anything at all. We clamp into a local copy
        // because `&self` cannot mutate; `handle_key` keeps `self.selected_idx`
        // bounded for any future input.
        if self.items.is_empty() {
            let modal_rect = Rect {
                x: area.x,
                y: area.y,
                width: 0,
                height: 0,
            };
            return (modal_rect, modal_rect);
        }
        let render_selected_idx = self.selected_idx.min(self.items.len() - 1);

        // Calculate dimensions. Use display width (UnicodeWidthStr::width)
        // for alignment math since ratatui paints in cells, not chars; emoji
        // and CJK characters render at width 2.
        let label_max: usize = self
            .items
            .iter()
            .map(|i| UnicodeWidthStr::width(i.label.as_str()))
            .max()
            .unwrap_or(0);
        let title_len = UnicodeWidthStr::width(self.title.as_str());
        // "  N  ● label  " => 2 + 1 + 2 + 2 + label + 2
        let item_width = 5 + 2 + label_max + 2; // number prefix + optional dot + label + padding
        let desc_lines: Vec<&str> = self
            .description
            .as_deref()
            .map(|d| d.lines().collect())
            .unwrap_or_default();
        let desc_max_width: usize = desc_lines
            .iter()
            .map(|l| UnicodeWidthStr::width(*l))
            .max()
            .unwrap_or(0);
        let content_width = item_width.max(title_len + 4).max(desc_max_width + 2);
        let modal_width = (content_width + 2) as u16; // +2 for borders
        let desc_height = if desc_lines.is_empty() {
            0
        } else {
            desc_lines.len() + 1
        }; // +1 blank line after
        let modal_height = (self.items.len() + desc_height + 3) as u16; // +2 borders, +1 title line

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
                    self.title.clone(),
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

        // Render description lines if present
        for line_text in &desc_lines {
            lines.push(Line::from(Span::styled(
                format!(" {line_text}"),
                Style::default()
                    .fg(theme::R_TEXT_SECONDARY)
                    .bg(theme::R_BG_OVERLAY),
            )));
        }
        if !desc_lines.is_empty() {
            lines.push(Line::from(""));
        }

        for (i, item) in self.items.iter().enumerate() {
            let is_selected = i == render_selected_idx;
            let num = index_to_char(i);
            let bg = if is_selected {
                theme::R_BG_HOVER
            } else {
                theme::R_BG_OVERLAY
            };

            let mut spans = vec![Span::styled(
                format!(" {num} "),
                Style::default().fg(theme::R_TEXT_TERTIARY).bg(bg),
            )];

            if let Some(color) = item.color {
                spans.push(Span::styled("● ", Style::default().fg(color).bg(bg)));
            }

            let label_fg = if is_selected {
                theme::R_TEXT_PRIMARY
            } else {
                theme::R_TEXT_SECONDARY
            };
            spans.push(Span::styled(
                item.label.clone(),
                Style::default().fg(label_fg).bg(bg),
            ));

            // Pad to fill width. Use display width since ratatui paints in
            // terminal cells; chars().count() over-counts narrow chars and
            // under-counts wide chars (CJK, emoji), producing misalignment.
            let content_len: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let remaining = (inner.width as usize).saturating_sub(content_len);
            if remaining > 0 {
                spans.push(Span::styled(" ".repeat(remaining), Style::default().bg(bg)));
            }

            lines.push(Line::from(spans));
        }

        frame.render_widget(Paragraph::new(lines), inner);

        // Return an items-only rect for mouse hit-testing (offset past description)
        let items_rect = Rect {
            x: inner.x,
            y: inner.y + desc_height as u16,
            width: inner.width,
            height: inner.height.saturating_sub(desc_height as u16),
        };

        (modal_rect, items_rect)
    }
}

/// Map a number character to a 0-based item index.
/// '1' → 0, '2' → 1, ..., '9' → 8, '0' → 9
fn char_to_index(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some((c as usize) - ('1' as usize)),
        '0' => Some(9),
        _ => None,
    }
}

/// Map a 0-based index to the display number character.
fn index_to_char(idx: usize) -> char {
    match idx {
        0..=8 => (b'1' + idx as u8) as char,
        9 => '0',
        _ => '?',
    }
}
