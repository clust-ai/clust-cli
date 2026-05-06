//! Inline modal for editing a scheduled task's prompt.
//!
//! Pre-populates with the current prompt; Enter saves a non-empty value via
//! [`UpdateScheduledTaskPrompt`](clust_ipc::CliMessage::UpdateScheduledTaskPrompt);
//! Esc cancels.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

use crate::theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPromptResult {
    Pending,
    Cancelled,
    Submitted { task_id: String, prompt: String },
}

pub struct EditPromptModal {
    task_id: String,
    branch_name: String,
    input: String,
    cursor_pos: usize,
}

impl EditPromptModal {
    pub fn new(task_id: String, branch_name: String, current_prompt: String) -> Self {
        let cursor_pos = current_prompt.len();
        Self {
            task_id,
            branch_name,
            input: current_prompt,
            cursor_pos,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> EditPromptResult {
        match key.code {
            KeyCode::Esc => EditPromptResult::Cancelled,
            KeyCode::Enter => {
                if self.input.trim().is_empty() {
                    // Reject empty submissions — same rule as creation.
                    EditPromptResult::Pending
                } else {
                    EditPromptResult::Submitted {
                        task_id: self.task_id.clone(),
                        prompt: self.input.clone(),
                    }
                }
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                }
                EditPromptResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                EditPromptResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                EditPromptResult::Pending
            }
            KeyCode::Char(c) => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER)
                {
                    return EditPromptResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                EditPromptResult::Pending
            }
            _ => EditPromptResult::Pending,
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

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 70u16.min(area.width.saturating_sub(4));
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
                format!(" Edit prompt — {} ", self.branch_name),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [hint_area, input_area, _gap, status_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(Span::styled(
                "Update the prompt — Enter to save, Esc to cancel",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Input box (cursor as inverse cell on the current char).
        let before = &self.input[..self.cursor_pos];
        let (cursor_char, after) = if self.cursor_pos < self.input.len() {
            let len = self.input[self.cursor_pos..]
                .chars()
                .next()
                .unwrap()
                .len_utf8();
            (
                &self.input[self.cursor_pos..self.cursor_pos + len],
                &self.input[self.cursor_pos + len..],
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
            Span::styled(before, Style::default().fg(theme::R_TEXT_PRIMARY)),
            Span::styled(
                cursor_char,
                Style::default()
                    .fg(theme::R_BG_BASE)
                    .bg(theme::R_TEXT_PRIMARY),
            ),
            Span::styled(after, Style::default().fg(theme::R_TEXT_PRIMARY)),
        ]);
        frame.render_widget(
            Paragraph::new(line)
                .style(Style::default().bg(theme::R_BG_INPUT))
                .wrap(Wrap { trim: false }),
            input_area,
        );

        let warn = if self.input.trim().is_empty() {
            Span::styled(
                "prompt cannot be empty",
                Style::default().fg(theme::R_ERROR),
            )
        } else {
            Span::styled(
                format!("{} chars", self.input.chars().count()),
                Style::default().fg(theme::R_TEXT_DISABLED),
            )
        };
        frame.render_widget(Paragraph::new(Line::from(warn)), status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modal() -> EditPromptModal {
        EditPromptModal::new("t1".into(), "br".into(), "old".into())
    }

    #[test]
    fn enter_with_text_submits() {
        let mut m = modal();
        let r = m.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match r {
            EditPromptResult::Submitted { task_id, prompt } => {
                assert_eq!(task_id, "t1");
                assert_eq!(prompt, "old");
            }
            other => panic!("expected Submitted, got {:?}", other),
        }
    }

    #[test]
    fn enter_with_blank_input_stays_pending() {
        let mut m = EditPromptModal::new("t1".into(), "br".into(), "".into());
        let r = m.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(r, EditPromptResult::Pending);
    }

    #[test]
    fn esc_cancels() {
        let mut m = modal();
        let r = m.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(r, EditPromptResult::Cancelled);
    }

    #[test]
    fn typing_appends() {
        let mut m = modal();
        m.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE));
        let r = m.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match r {
            EditPromptResult::Submitted { prompt, .. } => assert_eq!(prompt, "old!"),
            _ => panic!("expected Submitted"),
        }
    }

    #[test]
    fn backspace_removes() {
        let mut m = modal();
        m.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        let r = m.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match r {
            EditPromptResult::Submitted { prompt, .. } => assert_eq!(prompt, "ol"),
            _ => panic!("expected Submitted"),
        }
    }
}
