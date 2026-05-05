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

use clust_ipc::{BranchInfo, RepoInfo};

use crate::theme;
use crate::timer_modal::{format_duration_short, parse_duration, parse_time_of_day};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    SelectRepo,
    SelectKind,
    SelectBranch,
    SelectParent,
    NewBranch,
    EnterPrompt,
    EnterTime,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScheduleKind {
    Scheduled,
    Dependent,
    Unscheduled,
}

pub enum ModalResult {
    Pending,
    Cancelled,
    Completed(ModalOutput),
}

/// A potential parent task the user can select for Dependent mode.
#[derive(Clone, Debug)]
pub struct ParentChoice {
    /// Hub-assigned batch id of the parent (used for `depends_on`).
    pub batch_id: String,
    /// Display title of the parent task.
    pub title: String,
    /// Branch the parent task creates — becomes the dependent task's `target_branch`.
    pub branch_name: String,
    pub repo_path: String,
    pub repo_name: String,
}

/// Output emitted when the modal completes.
pub struct ModalOutput {
    pub repo_path: String,
    pub repo_name: String,
    /// Branch the new task is based on (parent's branch when Dependent).
    pub target_branch: String,
    /// New branch name the task creates.
    pub new_branch: String,
    pub prompt: String,
    pub plan_mode: bool,
    pub kind: KindOutput,
}

pub enum KindOutput {
    /// RFC 3339 timestamp.
    Scheduled { scheduled_at: String },
    /// Hub-assigned batch id of the parent.
    Dependent { parent_batch_id: String },
    Unscheduled,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct ScheduleTaskModal {
    step: Step,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    // Data
    repos: Vec<RepoInfo>,
    branches: Vec<BranchInfo>,
    parents: Vec<ParentChoice>,

    // Accumulated selections
    selected_repo: Option<RepoInfo>,
    selected_kind: ScheduleKind,
    selected_branch: Option<String>,
    selected_parent: Option<ParentChoice>,
    new_branch_name: Option<String>,
    prompt_text: Option<String>,
    scheduled_at: Option<String>,

    // EnterTime preview/error
    time_preview: Option<String>,
    time_error: Option<String>,

    plan_mode: bool,

    matcher: SkimMatcherV2,
}

impl ScheduleTaskModal {
    pub fn new(repos: Vec<RepoInfo>, parents: Vec<ParentChoice>) -> Self {
        Self {
            step: Step::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            parents,
            selected_repo: None,
            selected_kind: ScheduleKind::Scheduled,
            selected_branch: None,
            selected_parent: None,
            new_branch_name: None,
            prompt_text: None,
            scheduled_at: None,
            time_preview: None,
            time_error: None,
            plan_mode: false,
            matcher: SkimMatcherV2::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> ModalResult {
        match self.step {
            Step::SelectKind => self.handle_kind_key(key),
            Step::EnterTime => self.handle_time_key(key),
            _ => self.handle_text_step_key(key),
        }
    }

    fn handle_text_step_key(&mut self, key: KeyEvent) -> ModalResult {
        match key.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                ModalResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                ModalResult::Pending
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                    self.selected_idx = 0;
                }
                ModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                ModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                ModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    if c == 'p' {
                        self.plan_mode = !self.plan_mode;
                    }
                    return ModalResult::Pending;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return ModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                ModalResult::Pending
            }
            _ => ModalResult::Pending,
        }
    }

    fn handle_kind_key(&mut self, key: KeyEvent) -> ModalResult {
        match key.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Up => {
                self.selected_kind = match self.selected_kind {
                    ScheduleKind::Scheduled => ScheduleKind::Unscheduled,
                    ScheduleKind::Dependent => ScheduleKind::Scheduled,
                    ScheduleKind::Unscheduled => ScheduleKind::Dependent,
                };
                ModalResult::Pending
            }
            KeyCode::Down => {
                self.selected_kind = match self.selected_kind {
                    ScheduleKind::Scheduled => ScheduleKind::Dependent,
                    ScheduleKind::Dependent => ScheduleKind::Unscheduled,
                    ScheduleKind::Unscheduled => ScheduleKind::Scheduled,
                };
                ModalResult::Pending
            }
            KeyCode::Enter => {
                match self.selected_kind {
                    ScheduleKind::Scheduled | ScheduleKind::Unscheduled => {
                        // Move to branch selection
                        if let Some(ref repo) = self.selected_repo {
                            self.branches = repo.local_branches.clone();
                        }
                        if self.branches.is_empty() {
                            // No branches — skip directly to NewBranch
                            self.step = Step::NewBranch;
                        } else {
                            self.step = Step::SelectBranch;
                        }
                    }
                    ScheduleKind::Dependent => {
                        // Filter parents to current repo only
                        let repo_path = self
                            .selected_repo
                            .as_ref()
                            .map(|r| r.path.clone())
                            .unwrap_or_default();
                        self.parents
                            .retain(|p| p.repo_path == repo_path || repo_path.is_empty());
                        if self.parents.is_empty() {
                            // No parents available — show empty list, user can Esc back
                        }
                        self.step = Step::SelectParent;
                    }
                }
                self.reset_input();
                ModalResult::Pending
            }
            _ => ModalResult::Pending,
        }
    }

    fn handle_time_key(&mut self, key: KeyEvent) -> ModalResult {
        match key.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Enter => match self.try_parse_time() {
                Ok((rfc, _)) => {
                    self.scheduled_at = Some(rfc.clone());
                    self.complete()
                }
                Err(msg) => {
                    self.time_error = Some(msg);
                    ModalResult::Pending
                }
            },
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    self.input.remove(self.cursor_pos);
                    self.update_time_preview();
                }
                ModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                ModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                ModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return ModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.update_time_preview();
                ModalResult::Pending
            }
            _ => ModalResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> ModalResult {
        match self.step {
            Step::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.selected_repo = Some(self.repos[idx].clone());
                    self.step = Step::SelectKind;
                    self.reset_input();
                }
                ModalResult::Pending
            }
            Step::SelectBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.selected_branch = Some(self.branches[idx].name.clone());
                    self.step = Step::NewBranch;
                    self.reset_input();
                }
                ModalResult::Pending
            }
            Step::SelectParent => {
                let filtered = self.filtered_parents();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let parent = self.parents[idx].clone();
                    // The dependent task bases its branch off the parent's branch.
                    self.selected_branch = Some(parent.branch_name.clone());
                    self.selected_parent = Some(parent);
                    self.step = Step::NewBranch;
                    self.reset_input();
                }
                ModalResult::Pending
            }
            Step::NewBranch => {
                let sanitized = clust_ipc::branch::sanitize_branch_name(&self.input);
                if sanitized.is_empty() {
                    return ModalResult::Pending;
                }
                self.new_branch_name = Some(sanitized);
                self.step = Step::EnterPrompt;
                self.reset_input();
                ModalResult::Pending
            }
            Step::EnterPrompt => {
                if self.input.trim().is_empty() {
                    return ModalResult::Pending;
                }
                self.prompt_text = Some(self.input.clone());
                if self.selected_kind == ScheduleKind::Scheduled {
                    self.step = Step::EnterTime;
                    self.reset_input();
                    ModalResult::Pending
                } else {
                    self.complete()
                }
            }
            _ => ModalResult::Pending,
        }
    }

    fn complete(&self) -> ModalResult {
        let repo = self.selected_repo.as_ref().unwrap();
        let target_branch = self.selected_branch.clone().unwrap_or_default();
        let new_branch = self.new_branch_name.clone().unwrap_or_default();
        let prompt = self.prompt_text.clone().unwrap_or_default();

        let kind = match self.selected_kind {
            ScheduleKind::Scheduled => KindOutput::Scheduled {
                scheduled_at: self.scheduled_at.clone().unwrap_or_default(),
            },
            ScheduleKind::Dependent => KindOutput::Dependent {
                parent_batch_id: self
                    .selected_parent
                    .as_ref()
                    .map(|p| p.batch_id.clone())
                    .unwrap_or_default(),
            },
            ScheduleKind::Unscheduled => KindOutput::Unscheduled,
        };

        ModalResult::Completed(ModalOutput {
            repo_path: repo.path.clone(),
            repo_name: repo.name.clone(),
            target_branch,
            new_branch,
            prompt,
            plan_mode: self.plan_mode,
            kind,
        })
    }

    fn go_back(&mut self) -> ModalResult {
        match self.step {
            Step::SelectRepo => return ModalResult::Cancelled,
            Step::SelectKind => {
                self.step = Step::SelectRepo;
                self.selected_repo = None;
            }
            Step::SelectBranch => {
                self.step = Step::SelectKind;
                self.selected_branch = None;
            }
            Step::SelectParent => {
                self.step = Step::SelectKind;
                self.selected_branch = None;
                self.selected_parent = None;
            }
            Step::NewBranch => {
                if matches!(self.selected_kind, ScheduleKind::Dependent) {
                    self.step = Step::SelectParent;
                    self.selected_branch = None;
                    self.selected_parent = None;
                } else if self.branches.is_empty() {
                    self.step = Step::SelectKind;
                } else {
                    self.step = Step::SelectBranch;
                    self.selected_branch = None;
                }
                self.new_branch_name = None;
            }
            Step::EnterPrompt => {
                self.step = Step::NewBranch;
                self.prompt_text = None;
                if let Some(ref name) = self.new_branch_name {
                    self.input = name.clone();
                    self.cursor_pos = self.input.len();
                    return ModalResult::Pending;
                }
            }
            Step::EnterTime => {
                self.step = Step::EnterPrompt;
                self.scheduled_at = None;
                self.time_preview = None;
                self.time_error = None;
                if let Some(ref p) = self.prompt_text {
                    self.input = p.clone();
                    self.cursor_pos = self.input.len();
                    return ModalResult::Pending;
                }
            }
        }
        self.reset_input();
        ModalResult::Pending
    }

    pub fn handle_paste(&mut self, text: &str) {
        if matches!(self.step, Step::SelectKind) {
            return;
        }
        let allow_newlines = matches!(self.step, Step::EnterPrompt);
        for c in text.chars() {
            if (c == '\n' || c == '\r') && !allow_newlines {
                continue;
            }
            self.input.insert(self.cursor_pos, c);
            self.cursor_pos += c.len_utf8();
        }
        self.selected_idx = 0;
        if matches!(self.step, Step::EnterTime) {
            self.update_time_preview();
        }
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

    // -----------------------------------------------------------------------
    // Time parsing
    // -----------------------------------------------------------------------

    fn try_parse_time(&self) -> Result<(String, String), String> {
        let input = self.input.trim();
        if input.is_empty() {
            return Err("Enter a duration (2h, 30m) or time (16:00)".to_string());
        }
        if let Some(dur) = parse_duration(input) {
            if dur.as_secs() == 0 {
                return Err("Duration must be greater than zero".to_string());
            }
            let start = chrono::Utc::now() + dur;
            let local = start.with_timezone(&chrono::Local);
            let rfc = start.to_rfc3339();
            let preview = format!(
                "Starts at {} (in {})",
                local.format("%H:%M"),
                format_duration_short(dur),
            );
            return Ok((rfc, preview));
        }
        if let Some(start) = parse_time_of_day(input) {
            let now = chrono::Utc::now();
            let remaining = start.signed_duration_since(now);
            let dur = if remaining.num_seconds() > 0 {
                std::time::Duration::from_secs(remaining.num_seconds() as u64)
            } else {
                std::time::Duration::from_secs(0)
            };
            let local = start.with_timezone(&chrono::Local);
            let rfc = start.to_rfc3339();
            let preview = if dur.as_secs() > 0 {
                format!(
                    "Starts at {} (in {})",
                    local.format("%H:%M"),
                    format_duration_short(dur),
                )
            } else {
                format!("Starts at {} (immediately)", local.format("%H:%M"))
            };
            return Ok((rfc, preview));
        }
        Err("Invalid format. Use duration (2h, 30m) or time (16:00)".to_string())
    }

    fn update_time_preview(&mut self) {
        match self.try_parse_time() {
            Ok((_, preview)) => {
                self.time_preview = Some(preview);
                self.time_error = None;
            }
            Err(msg) => {
                self.time_preview = None;
                self.time_error = Some(msg);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Fuzzy filtering
    // -----------------------------------------------------------------------

    fn filtered_repos(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self.repos.iter().enumerate().map(|(i, _)| (i, 0)).collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .repos
            .iter()
            .enumerate()
            .filter_map(|(i, repo)| {
                self.matcher
                    .fuzzy_match(&repo.name, &self.input)
                    .or_else(|| self.matcher.fuzzy_match(&repo.path, &self.input))
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    fn filtered_branches(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .branches
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .branches
            .iter()
            .enumerate()
            .filter_map(|(i, branch)| {
                self.matcher
                    .fuzzy_match(&branch.name, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    fn filtered_parents(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .parents
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .parents
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let combined = format!("{} {}", p.title, p.branch_name);
                self.matcher
                    .fuzzy_match(&combined, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    fn filtered_count(&self) -> usize {
        match self.step {
            Step::SelectRepo => self.filtered_repos().len(),
            Step::SelectBranch => self.filtered_branches().len(),
            Step::SelectParent => self.filtered_parents().len(),
            Step::SelectKind => 3,
            Step::NewBranch | Step::EnterPrompt | Step::EnterTime => 0,
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 64u16.min(area.width.saturating_sub(4));
        let modal_height = (area.height * 65 / 100)
            .max(12)
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
                format!(" {} ", self.step_title()),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let is_prompt_step = self.step == Step::EnterPrompt;
        let is_kind_step = self.step == Step::SelectKind;
        let is_time_step = self.step == Step::EnterTime;
        let show_input = !is_kind_step;

        let [hint_area, input_area, _gap, list_area, _spacer, status_area] = Layout::vertical([
            Constraint::Length(1),
            if is_prompt_step {
                Constraint::Min(3)
            } else if show_input {
                Constraint::Length(1)
            } else {
                Constraint::Length(0)
            },
            if is_prompt_step || is_time_step {
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

        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        if show_input {
            self.render_input(frame, input_area);
        }

        match self.step {
            Step::SelectRepo => self.render_repo_list(frame, list_area),
            Step::SelectBranch => self.render_branch_list(frame, list_area),
            Step::SelectParent => self.render_parent_list(frame, list_area),
            Step::SelectKind => self.render_kind_list(frame, list_area),
            Step::NewBranch => self.render_new_branch_hint(frame, list_area),
            Step::EnterPrompt => {}
            Step::EnterTime => self.render_time_preview(frame, list_area),
        }

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
        spans.push(Span::styled(
            format!("  {mod_key}+P toggle plan mode"),
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
        let cursor_line = (2 + char_pos).checked_div(width.max(1)).unwrap_or(0);
        let visible = area.height as usize;
        let scroll: u16 = if cursor_line >= visible {
            (cursor_line - visible + 1) as u16
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

    fn render_repo_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_repos();
        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vi, &(orig_idx, _))| {
                let repo = &self.repos[orig_idx];
                let is_selected = vi + scroll == self.selected_idx;
                self.render_list_item(&repo.name, Some(&repo.path), is_selected)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_branch_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_branches();
        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vi, &(orig_idx, _))| {
                let branch = &self.branches[orig_idx];
                let is_selected = vi + scroll == self.selected_idx;
                let mut suffix = Vec::new();
                if branch.is_head {
                    suffix.push(Span::styled(" HEAD", Style::default().fg(theme::R_SUCCESS)));
                }
                if branch.is_worktree {
                    suffix.push(Span::styled(
                        " [worktree]",
                        Style::default().fg(theme::R_INFO),
                    ));
                }
                let mut spans = self.list_item_spans(&branch.name, is_selected);
                spans.extend(suffix);
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_parent_list(&self, frame: &mut Frame, area: Rect) {
        if self.parents.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "No tasks to depend on. Press Esc to choose a different option.",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ))),
                area,
            );
            return;
        }
        let filtered = self.filtered_parents();
        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vi, &(orig_idx, _))| {
                let parent = &self.parents[orig_idx];
                let is_selected = vi + scroll == self.selected_idx;
                let label = format!("{}  ({})", parent.title, parent.branch_name);
                let mut spans: Vec<Span<'static>> = if is_selected {
                    vec![
                        Span::styled(
                            "  > ",
                            Style::default()
                                .fg(theme::R_ACCENT_BRIGHT)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            label,
                            Style::default()
                                .fg(theme::R_TEXT_PRIMARY)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]
                } else {
                    vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(label, Style::default().fg(theme::R_TEXT_SECONDARY)),
                    ]
                };
                let detail_style = if is_selected {
                    Style::default().fg(theme::R_TEXT_TERTIARY)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                };
                spans.push(Span::styled("  ", Style::default()));
                spans.push(Span::styled(parent.repo_name.clone(), detail_style));
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_kind_list(&self, frame: &mut Frame, area: Rect) {
        let kinds = [
            (
                ScheduleKind::Scheduled,
                "Schedule",
                "Run at a specific time (e.g. 2h or 20:00)",
            ),
            (
                ScheduleKind::Dependent,
                "Dependent",
                "Run after another task completes (branches off it)",
            ),
            (
                ScheduleKind::Unscheduled,
                "Unscheduled",
                "Don't auto-start; you'll start it manually",
            ),
        ];
        let lines: Vec<Line> = kinds
            .iter()
            .map(|(k, name, desc)| {
                let is_selected = *k == self.selected_kind;
                let mut spans = self.list_item_spans(name, is_selected);
                let detail_style = if is_selected {
                    Style::default().fg(theme::R_TEXT_TERTIARY)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                };
                spans.push(Span::styled("  ", Style::default()));
                spans.push(Span::styled(*desc, detail_style));
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_new_branch_hint(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(Span::styled(
            "Enter a branch name for the new task (required)",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        ))];
        if let Some(ref target) = self.selected_branch {
            let label = if matches!(self.selected_kind, ScheduleKind::Dependent) {
                "Based on parent's branch"
            } else {
                "Based on"
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {label}: "),
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ),
                Span::styled(
                    target.clone(),
                    Style::default()
                        .fg(theme::R_ACCENT_TEXT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_time_preview(&self, frame: &mut Frame, area: Rect) {
        let line = if let Some(ref preview) = self.time_preview {
            Line::from(Span::styled(
                preview.as_str(),
                Style::default().fg(theme::R_SUCCESS),
            ))
        } else if let Some(ref err) = self.time_error {
            Line::from(Span::styled(
                err.as_str(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ))
        } else {
            Line::from(Span::styled(
                "Examples: 2h, 30m, 1h30m, 16:00",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ))
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_list_item<'a>(
        &self,
        name: &'a str,
        detail: Option<&'a str>,
        selected: bool,
    ) -> Line<'a> {
        let mut all = self.list_item_spans(name, selected);
        if let Some(d) = detail {
            all.push(Span::styled("  ", Style::default()));
            let detail_style = if selected {
                Style::default().fg(theme::R_TEXT_TERTIARY)
            } else {
                Style::default().fg(theme::R_TEXT_DISABLED)
            };
            all.push(Span::styled(d, detail_style));
        }
        Line::from(all)
    }

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

    fn step_title(&self) -> String {
        let total = self.total_steps();
        let n = self.step_number();
        let label = match self.step {
            Step::SelectRepo => "Select repository",
            Step::SelectKind => "Choose schedule",
            Step::SelectBranch => "Select source branch",
            Step::SelectParent => "Pick parent task",
            Step::NewBranch => "Branch name",
            Step::EnterPrompt => "Enter prompt",
            Step::EnterTime => "Enter time",
        };
        format!("Step {n}/{total} \u{2014} {label}")
    }

    fn step_number(&self) -> usize {
        match self.step {
            Step::SelectRepo => 1,
            Step::SelectKind => 2,
            Step::SelectBranch | Step::SelectParent => 3,
            Step::NewBranch => 4,
            Step::EnterPrompt => 5,
            Step::EnterTime => 6,
        }
    }

    fn total_steps(&self) -> usize {
        match self.selected_kind {
            ScheduleKind::Scheduled => 6,
            _ => 5,
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            Step::SelectRepo => "Type to filter, Enter to select, Esc to cancel",
            Step::SelectKind => "\u{2191}/\u{2193} pick schedule, Enter to confirm, Esc to go back",
            Step::SelectBranch => "Type to filter, Enter to select, Esc to go back",
            Step::SelectParent => "Type to filter, Enter to pick a parent, Esc to go back",
            Step::NewBranch => "Type a new branch name and press Enter, Esc to go back",
            Step::EnterPrompt => "Type the prompt for the agent, Enter to confirm, Esc to go back",
            Step::EnterTime => "Duration (2h, 30m) or time (16:00). Enter to schedule, Esc to go back",
        }
    }
}
