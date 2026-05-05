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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AddTaskStep {
    EnterBranch,
    EnterPrompt,
}

pub enum AddTaskResult {
    Pending,
    Cancelled,
    Completed(AddTaskOutput),
}

pub struct AddTaskOutput {
    pub batch_idx: usize,
    pub branch_name: String,
    pub prompt: String,
    pub use_prefix: bool,
    pub use_suffix: bool,
    pub plan_mode: bool,
    pub exit_when_done: bool,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct AddTaskModal {
    step: AddTaskStep,
    input: String,
    cursor_pos: usize,

    batch_idx: usize,
    batch_title: String,
    branch_name: String,
    use_prefix: bool,
    use_suffix: bool,
    has_prefix: bool,
    has_suffix: bool,
    plan_mode: bool,
    exit_when_done: bool,
    /// Whether the batch's agent binary supports the auto-exit Stop hook.
    /// When `false`, the toggle is hidden and the flag stays at `false`.
    supports_exit_when_done: bool,
}

impl AddTaskModal {
    pub fn new(
        batch_idx: usize,
        batch_title: String,
        has_prefix: bool,
        has_suffix: bool,
        batch_plan_mode: bool,
        supports_exit_when_done: bool,
    ) -> Self {
        Self {
            step: AddTaskStep::EnterBranch,
            input: String::new(),
            cursor_pos: 0,
            batch_idx,
            batch_title,
            branch_name: String::new(),
            use_prefix: true,
            use_suffix: true,
            has_prefix,
            has_suffix,
            plan_mode: batch_plan_mode,
            exit_when_done: false,
            supports_exit_when_done,
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> AddTaskResult {
        match key.code {
            KeyCode::Esc => {
                match self.step {
                    AddTaskStep::EnterBranch => return AddTaskResult::Cancelled,
                    AddTaskStep::EnterPrompt => {
                        self.step = AddTaskStep::EnterBranch;
                        self.input = self.branch_name.clone();
                        self.cursor_pos = self.input.len();
                    }
                }
                AddTaskResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                }
                AddTaskResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                AddTaskResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                AddTaskResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    match c {
                        'p' => self.plan_mode = !self.plan_mode,
                        'a' => self.use_prefix = !self.use_prefix,
                        's' => self.use_suffix = !self.use_suffix,
                        'x' if self.supports_exit_when_done => {
                            self.exit_when_done = !self.exit_when_done;
                        }
                        _ => {}
                    }
                    return AddTaskResult::Pending;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return AddTaskResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                AddTaskResult::Pending
            }
            _ => AddTaskResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> AddTaskResult {
        match self.step {
            AddTaskStep::EnterBranch => {
                let trimmed = self.input.trim().to_string();
                if trimmed.is_empty() {
                    return AddTaskResult::Pending;
                }
                self.branch_name = trimmed;
                self.step = AddTaskStep::EnterPrompt;
                self.input.clear();
                self.cursor_pos = 0;
                AddTaskResult::Pending
            }
            AddTaskStep::EnterPrompt => {
                let trimmed = self.input.trim().to_string();
                if trimmed.is_empty() {
                    return AddTaskResult::Pending;
                }
                AddTaskResult::Completed(AddTaskOutput {
                    batch_idx: self.batch_idx,
                    branch_name: self.branch_name.clone(),
                    prompt: trimmed,
                    use_prefix: self.use_prefix,
                    use_suffix: self.use_suffix,
                    plan_mode: self.plan_mode,
                    exit_when_done: self.exit_when_done,
                })
            }
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

        let title = self.step_title();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_ACCENT_DIM))
            .title(Span::styled(
                format!(" {} ", title),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let is_prompt_step = self.step == AddTaskStep::EnterPrompt;
        let input_height = if is_prompt_step {
            Constraint::Min(3)
        } else {
            Constraint::Length(1)
        };
        // In the prompt step the info row is just one "Branch: name" line, so
        // pin it to a fixed height and let the input swallow the remainder.
        let info_height = if is_prompt_step {
            Constraint::Length(1)
        } else {
            Constraint::Min(0)
        };

        let [hint_area, input_area, _gap, info_area, _spacer, status_area] = Layout::vertical([
            Constraint::Length(1),
            input_height,
            Constraint::Length(1),
            info_height,
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        // Step hint
        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Input field
        self.render_input(frame, input_area);

        // Context info below input
        match self.step {
            AddTaskStep::EnterBranch => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "Branch name is required",
                        Style::default().fg(theme::R_TEXT_TERTIARY),
                    )),
                    info_area,
                );
            }
            AddTaskStep::EnterPrompt => {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("Branch: ", Style::default().fg(theme::R_TEXT_TERTIARY)),
                        Span::styled(&self.branch_name, Style::default().fg(theme::R_ACCENT)),
                    ])),
                    info_area,
                );
            }
        }

        // Status bar: prefix/suffix toggle indicators
        self.render_status_bar(frame, status_area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let mut spans: Vec<Span> = Vec::new();

        if self.plan_mode {
            spans.push(Span::styled(
                "PLAN",
                Style::default()
                    .fg(theme::R_WARNING)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                "Normal",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        spans.push(Span::styled("  ", Style::default()));

        if self.use_prefix {
            spans.push(Span::styled(
                "\u{2713} Pfx",
                if self.has_prefix {
                    Style::default().fg(theme::R_SUCCESS)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ));
        } else {
            spans.push(Span::styled(
                "\u{2717} Pfx",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        spans.push(Span::styled("  ", Style::default()));

        if self.use_suffix {
            spans.push(Span::styled(
                "\u{2713} Sfx",
                if self.has_suffix {
                    Style::default().fg(theme::R_SUCCESS)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ));
        } else {
            spans.push(Span::styled(
                "\u{2717} Sfx",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        if self.supports_exit_when_done {
            spans.push(Span::styled("  ", Style::default()));
            if self.exit_when_done {
                spans.push(Span::styled(
                    "\u{2713} Exit",
                    Style::default().fg(theme::R_SUCCESS),
                ));
            } else {
                spans.push(Span::styled(
                    "\u{2717} Exit",
                    Style::default().fg(theme::R_TEXT_DISABLED),
                ));
            }
        }

        let hint = if self.supports_exit_when_done {
            format!("  {mod_key}+P plan  {mod_key}+A/S pfx/sfx  {mod_key}+X exit")
        } else {
            format!("  {mod_key}+P plan  {mod_key}+A/S pfx/sfx")
        };
        spans.push(Span::styled(
            hint,
            Style::default().fg(theme::R_TEXT_DISABLED),
        ));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn step_title(&self) -> String {
        match self.step {
            AddTaskStep::EnterBranch => {
                format!("Step 1/2 \u{2014} Branch name ({})", self.batch_title)
            }
            AddTaskStep::EnterPrompt => {
                format!("Step 2/2 \u{2014} Task prompt ({})", self.batch_title)
            }
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            AddTaskStep::EnterBranch => "Enter branch name, Enter to continue, Esc to cancel",
            AddTaskStep::EnterPrompt => {
                "Enter prompt for the agent, Enter to add task, Esc to go back"
            }
        }
    }
}
