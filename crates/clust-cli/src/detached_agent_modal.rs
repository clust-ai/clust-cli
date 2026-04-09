use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
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
pub enum DetachedModalStep {
    SelectDirectory,
    EnterPrompt,
}

pub enum DetachedModalResult {
    Pending,
    Cancelled,
    Completed(DetachedModalOutput),
}

pub struct DetachedModalOutput {
    pub working_dir: String,
    pub prompt: Option<String>,
    pub plan_mode: bool,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct DetachedAgentModal {
    step: DetachedModalStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    /// Resolved directory currently being browsed (always ends with `/`).
    base_path: String,
    /// Cached subdirectory names under `base_path`.
    dir_entries: Vec<String>,

    /// Confirmed working directory (set when transitioning to EnterPrompt).
    working_dir: String,

    plan_mode: bool,

    matcher: SkimMatcherV2,
}

impl DetachedAgentModal {
    pub fn new() -> Self {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        let base_path = if home.ends_with('/') {
            home
        } else {
            format!("{home}/")
        };
        let mut modal = Self {
            step: DetachedModalStep::SelectDirectory,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            base_path: base_path.clone(),
            dir_entries: Vec::new(),
            working_dir: base_path,
            plan_mode: false,
            matcher: SkimMatcherV2::default(),
        };
        modal.refresh_entries();
        modal
    }

    pub fn set_plan_mode(&mut self, val: bool) {
        self.plan_mode = val;
    }

    // -----------------------------------------------------------------------
    // Directory browsing
    // -----------------------------------------------------------------------

    fn refresh_entries(&mut self) {
        self.dir_entries.clear();
        if let Ok(read) = std::fs::read_dir(&self.base_path) {
            for entry in read.flatten() {
                let ok = entry
                    .file_type()
                    .map(|ft| ft.is_dir())
                    .unwrap_or(false);
                if !ok {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    if !name.starts_with('.') {
                        self.dir_entries.push(name.to_string());
                    }
                }
            }
        }
        self.dir_entries.sort_unstable();
        self.selected_idx = 0;
    }

    fn filtered_dirs(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .dir_entries
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .dir_entries
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                self.matcher
                    .fuzzy_match(name, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by(|a, b| b.1.cmp(&a.1));
        results
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> DetachedModalResult {
        match key.code {
            KeyCode::Esc => match self.step {
                DetachedModalStep::SelectDirectory => DetachedModalResult::Cancelled,
                DetachedModalStep::EnterPrompt => {
                    self.step = DetachedModalStep::SelectDirectory;
                    self.base_path = if self.working_dir.ends_with('/') {
                        self.working_dir.clone()
                    } else {
                        format!("{}/", self.working_dir)
                    };
                    self.reset_input();
                    self.refresh_entries();
                    DetachedModalResult::Pending
                }
            },
            KeyCode::Tab => {
                if self.step == DetachedModalStep::SelectDirectory {
                    self.autocomplete_selected();
                }
                DetachedModalResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                DetachedModalResult::Pending
            }
            KeyCode::Down => {
                let max = match self.step {
                    DetachedModalStep::SelectDirectory => {
                        self.filtered_dirs().len().saturating_sub(1)
                    }
                    DetachedModalStep::EnterPrompt => 0,
                };
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                DetachedModalResult::Pending
            }
            KeyCode::Backspace => {
                if self.step == DetachedModalStep::SelectDirectory
                    && self.input.is_empty()
                    && self.base_path != "/"
                {
                    // Navigate up one directory level
                    let trimmed = self.base_path.trim_end_matches('/');
                    if let Some(pos) = trimmed.rfind('/') {
                        self.base_path = format!("{}/", &trimmed[..pos]);
                        if self.base_path == "/" || self.base_path.is_empty() {
                            self.base_path = "/".to_string();
                        }
                        self.refresh_entries();
                    }
                } else if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                    self.selected_idx = 0;
                }
                DetachedModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                DetachedModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos +=
                        self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
                }
                DetachedModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    if c == 'p' {
                        self.plan_mode = !self.plan_mode;
                    }
                    return DetachedModalResult::Pending;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return DetachedModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                DetachedModalResult::Pending
            }
            _ => DetachedModalResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> DetachedModalResult {
        match self.step {
            DetachedModalStep::SelectDirectory => {
                // If there is filter text that matches a directory, append it first
                if !self.input.is_empty() {
                    let filtered = self.filtered_dirs();
                    if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                        let name = self.dir_entries[idx].clone();
                        self.base_path = format!("{}{}/", self.base_path, name);
                        self.reset_input();
                        self.refresh_entries();
                    }
                }
                // Confirm current base_path as working directory
                let wd = self.base_path.trim_end_matches('/').to_string();
                self.working_dir = if wd.is_empty() {
                    "/".to_string()
                } else {
                    wd
                };
                self.step = DetachedModalStep::EnterPrompt;
                self.reset_input();
                DetachedModalResult::Pending
            }
            DetachedModalStep::EnterPrompt => {
                let prompt = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(self.input.clone())
                };
                DetachedModalResult::Completed(DetachedModalOutput {
                    working_dir: self.working_dir.clone(),
                    prompt,
                    plan_mode: self.plan_mode,
                })
            }
        }
    }

    fn autocomplete_selected(&mut self) {
        let filtered = self.filtered_dirs();
        if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
            let name = self.dir_entries[idx].clone();
            self.base_path = format!("{}{}/", self.base_path, name);
            self.reset_input();
            self.refresh_entries();
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
        self.selected_idx = 0;
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 60u16.min(area.width.saturating_sub(4));
        let modal_height = (area.height * 60 / 100)
            .max(10)
            .min(area.height.saturating_sub(2));

        // Center horizontally
        let [_, modal_h_area, _] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(modal_width),
            Constraint::Fill(1),
        ])
        .areas(area);

        // Center vertically
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

        let is_prompt_step = self.step == DetachedModalStep::EnterPrompt;
        let [hint_area, path_area, input_area, _gap, list_area, _spacer, status_area] = Layout::vertical([
            Constraint::Length(1),
            if is_prompt_step {
                Constraint::Length(0)
            } else {
                Constraint::Length(1)
            },
            if is_prompt_step {
                Constraint::Min(3)
            } else {
                Constraint::Length(1)
            },
            if is_prompt_step {
                Constraint::Length(0)
            } else {
                Constraint::Length(1)
            },
            if is_prompt_step {
                Constraint::Length(0)
            } else {
                Constraint::Min(0)
            },
            Constraint::Length(0),
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

        // Path display (directory step only)
        if !is_prompt_step {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    &self.base_path,
                    Style::default().fg(theme::R_ACCENT_TEXT),
                )),
                path_area,
            );
        }

        // Input field
        self.render_input(frame, input_area);

        // Directory list (directory step only)
        if !is_prompt_step {
            self.render_dir_list(frame, list_area);
        }

        // Status bar: plan mode indicator
        self.render_status_bar(frame, status_area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") { "Opt" } else { "Alt" };
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

        spans.push(Span::styled(
            format!("  {mod_key}+P toggle plan mode"),
            Style::default().fg(theme::R_TEXT_DISABLED),
        ));

        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let before_cursor = &self.input[..self.cursor_pos];
        let (cursor_char, after_cursor) = if self.cursor_pos < self.input.len() {
            let ch_len = self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
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
        let scroll = if width > 0 {
            let char_pos = self.input[..self.cursor_pos].chars().count();
            let cursor_line = (2 + char_pos) / width;
            let visible = area.height as usize;
            if cursor_line >= visible {
                (cursor_line - visible + 1) as u16
            } else {
                0
            }
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

    fn render_dir_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_dirs();
        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vis_idx, &(orig_idx, _))| {
                let name = &self.dir_entries[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
                Line::from(self.list_item_spans(name, is_selected))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn list_item_spans<'a>(&self, name: &'a str, selected: bool) -> Vec<Span<'a>> {
        if selected {
            vec![
                Span::styled(
                    "  > ",
                    Style::default()
                        .fg(theme::R_ACCENT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    name,
                    Style::default()
                        .fg(theme::R_TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]
        } else {
            vec![
                Span::styled("    ", Style::default()),
                Span::styled(name, Style::default().fg(theme::R_TEXT_SECONDARY)),
            ]
        }
    }

    fn compute_scroll(&self, total: usize, visible: usize) -> usize {
        if total <= visible || self.selected_idx < visible / 2 {
            0
        } else if self.selected_idx > total.saturating_sub(visible / 2) {
            total.saturating_sub(visible)
        } else {
            self.selected_idx.saturating_sub(visible / 2)
        }
    }

    fn step_title(&self) -> &'static str {
        match self.step {
            DetachedModalStep::SelectDirectory => "Step 1/2 \u{2014} Select directory",
            DetachedModalStep::EnterPrompt => "Step 2/2 \u{2014} Enter prompt",
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            DetachedModalStep::SelectDirectory => {
                "Type to filter, Tab to enter dir, Enter to confirm, Esc to cancel"
            }
            DetachedModalStep::EnterPrompt => "Type a prompt for the agent, Enter to start",
        }
    }
}
