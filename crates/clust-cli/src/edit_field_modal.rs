use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub enum EditFieldResult {
    Pending,
    Cancelled,
    Completed(String),
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct EditFieldModal {
    input: String,
    cursor_pos: usize,
    label: String,
    hint: String,
}

impl EditFieldModal {
    pub fn new(label: String, hint: String, initial_value: String) -> Self {
        let cursor_pos = initial_value.len();
        Self {
            input: initial_value,
            cursor_pos,
            label,
            hint,
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> EditFieldResult {
        match key.code {
            KeyCode::Esc => EditFieldResult::Cancelled,
            KeyCode::Enter => EditFieldResult::Completed(self.input.trim().to_string()),
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                }
                EditFieldResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                EditFieldResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                EditFieldResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return EditFieldResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                EditFieldResult::Pending
            }
            _ => EditFieldResult::Pending,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            self.input.insert(self.cursor_pos, c);
            self.cursor_pos += c.len_utf8();
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 60u16.min(area.width.saturating_sub(4));
        let modal_height = (area.height * 60 / 100)
            .max(10)
            .min(area.height.saturating_sub(2));

        let [_, modal_h_area, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(modal_width),
            Constraint::Fill(1),
        ])
        .areas(area);

        let [_, modal_area, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(modal_height),
            Constraint::Fill(1),
        ])
        .areas(modal_h_area);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_ACCENT_DIM))
            .title(Span::styled(
                format!(" {} ", self.label),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [hint_area, input_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(inner);

        // Hint
        frame.render_widget(
            Paragraph::new(Span::styled(
                &self.hint,
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Input field
        self.render_input(frame, input_area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let before_cursor = &self.input[..self.cursor_pos];
        let (cursor_char, after_cursor) = if self.cursor_pos < self.input.len() {
            let ch_len = self.input[self.cursor_pos..]
                .chars()
                .next()
                .unwrap()
                .len_utf8();
            (
                &self.input[self.cursor_pos..self.cursor_pos + ch_len],
                &self.input[self.cursor_pos + ch_len..],
            )
        } else {
            (" ", "")
        };

        let line = Line::from(vec![
            Span::styled(
                "> ",
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(before_cursor, Style::default().fg(theme::R_TEXT_PRIMARY)),
            Span::styled(
                cursor_char,
                Style::default()
                    .fg(theme::R_BG_BASE)
                    .bg(theme::R_TEXT_PRIMARY),
            ),
            Span::styled(after_cursor, Style::default().fg(theme::R_TEXT_PRIMARY)),
        ]);
        let width = area.width as usize;
        let char_pos = self.input[..self.cursor_pos].chars().count();
        let cursor_line = (2 + char_pos).checked_div(width).unwrap_or(0);
        let visible = area.height as usize;
        // Keep one empty line below the cursor so the prompt has breathing
        // room against the bottom of the input box.
        let max_view_line = visible.saturating_sub(2);
        let scroll: u16 = if cursor_line > max_view_line {
            (cursor_line - max_view_line) as u16
        } else {
            0
        };

        frame.render_widget(
            Paragraph::new(line)
                .style(Style::default().bg(theme::R_BG_INPUT))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            area,
        );
    }
}
