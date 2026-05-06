//! Opt+S "schedule task" modal.
//!
//! Mirrors [`crate::create_agent_modal`] for repo/branch/prompt selection,
//! then branches by schedule kind:
//!
//! - **Schedule** — final step is a free-text time/duration entry parsed by
//!   [`parse_time`].
//! - **Depend** — final step is a multi-select list of every existing
//!   scheduled task across all repos.
//! - **Unscheduled** — submits immediately after the prompt step.
//!
//! Differs from Opt+E in two ways: the prompt cannot be empty, and the modal
//! exposes an `Auto Exit` toggle (Alt+X) in addition to plan-mode (Alt+P).

use chrono::{Duration as ChronoDuration, Local, NaiveTime, TimeZone, Utc};
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

use clust_ipc::{BranchInfo, RepoInfo, ScheduleKind, ScheduledTaskInfo, ScheduledTaskStatus};

use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScheduleModalStep {
    SelectRepo,
    SelectScheduleKind,
    SelectBranch,
    NewBranch,
    EnterPrompt,
    EnterStartTime, // only for Schedule kind
    SelectDependencies, // only for Depend kind
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleKindChoice {
    Schedule,
    Depend,
    Unscheduled,
}

impl ScheduleKindChoice {
    fn label(self) -> &'static str {
        match self {
            Self::Schedule => "Schedule (start at time/duration)"
            ,
            Self::Depend => "Depend (start when other tasks complete)",
            Self::Unscheduled => "Unscheduled (start manually)",
        }
    }
}

#[derive(Debug)]
pub enum ScheduleModalResult {
    Pending,
    Cancelled,
    Completed(ScheduleModalOutput),
}

#[derive(Debug, Clone)]
pub struct ScheduleModalOutput {
    pub repo_path: String,
    pub base_branch: Option<String>,
    pub new_branch: Option<String>,
    pub prompt: String,
    pub plan_mode: bool,
    pub auto_exit: bool,
    pub schedule: ScheduleKind,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct ScheduleTaskModal {
    step: ScheduleModalStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    repos: Vec<RepoInfo>,
    branches: Vec<BranchInfo>,
    /// Snapshot of every existing scheduled task, used by the Depend step.
    /// Filtered fuzzily by `input` while on that step.
    all_tasks: Vec<ScheduledTaskInfo>,
    /// Picked dependencies (task ids) accumulated by Space-toggling.
    selected_deps: Vec<String>,

    selected_repo: Option<RepoInfo>,
    schedule_kind: Option<ScheduleKindChoice>,
    target_branch: Option<String>,
    new_branch_name: Option<String>,
    new_branch_required: bool,
    /// Last time-parse error message; cleared whenever input changes.
    time_error: Option<String>,

    plan_mode: bool,
    auto_exit: bool,
    /// Prompt captured at the EnterPrompt step. Stored separately so the
    /// EnterStartTime / SelectDependencies steps can reuse the input field for
    /// their own value without losing the user's prompt.
    prompt_value: String,
    matcher: SkimMatcherV2,
}

impl ScheduleTaskModal {
    pub fn new(repos: Vec<RepoInfo>, all_tasks: Vec<ScheduledTaskInfo>) -> Self {
        Self {
            step: ScheduleModalStep::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            all_tasks,
            selected_deps: Vec::new(),
            selected_repo: None,
            schedule_kind: None,
            target_branch: None,
            new_branch_name: None,
            new_branch_required: false,
            time_error: None,
            plan_mode: false,
            auto_exit: false,
            prompt_value: String::new(),
            matcher: SkimMatcherV2::default(),
        }
    }

    #[allow(dead_code)]
    pub fn step(&self) -> ScheduleModalStep {
        self.step
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ScheduleModalResult {
        match key.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Char(' ') if self.step == ScheduleModalStep::SelectDependencies => {
                self.toggle_dep_at_cursor();
                ScheduleModalResult::Pending
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
                self.time_error = None;
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                ScheduleModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                ScheduleModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    if c == 'p' || c == 'P' {
                        self.plan_mode = !self.plan_mode;
                    } else if c == 'x' || c == 'X' {
                        self.auto_exit = !self.auto_exit;
                    }
                    return ScheduleModalResult::Pending;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return ScheduleModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                self.time_error = None;
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            _ => ScheduleModalResult::Pending,
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
        self.time_error = None;
        self.clamp_selected_idx();
    }

    fn go_back(&mut self) -> ScheduleModalResult {
        match self.step {
            ScheduleModalStep::SelectRepo => return ScheduleModalResult::Cancelled,
            ScheduleModalStep::SelectScheduleKind => {
                self.step = ScheduleModalStep::SelectRepo;
                self.selected_repo = None;
                self.branches.clear();
            }
            ScheduleModalStep::SelectBranch => {
                self.step = ScheduleModalStep::SelectScheduleKind;
                self.target_branch = None;
            }
            ScheduleModalStep::NewBranch => {
                if self.new_branch_required {
                    self.step = ScheduleModalStep::SelectScheduleKind;
                } else {
                    self.step = ScheduleModalStep::SelectBranch;
                    self.target_branch = None;
                }
            }
            ScheduleModalStep::EnterPrompt => {
                self.step = ScheduleModalStep::NewBranch;
                self.new_branch_name = None;
            }
            ScheduleModalStep::EnterStartTime | ScheduleModalStep::SelectDependencies => {
                self.step = ScheduleModalStep::EnterPrompt;
            }
        }
        self.reset_input();
        self.time_error = None;
        ScheduleModalResult::Pending
    }

    fn handle_enter(&mut self) -> ScheduleModalResult {
        match self.step {
            ScheduleModalStep::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.branches = repo.local_branches.clone();
                    self.selected_repo = Some(repo);
                    self.step = ScheduleModalStep::SelectScheduleKind;
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::SelectScheduleKind => {
                let choices = self.kind_choices();
                if let Some(choice) = choices.get(self.selected_idx).copied() {
                    self.schedule_kind = Some(choice);
                    if self.branches.is_empty() {
                        self.new_branch_required = true;
                        self.step = ScheduleModalStep::NewBranch;
                    } else {
                        self.step = ScheduleModalStep::SelectBranch;
                    }
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::SelectBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.target_branch = Some(self.branches[idx].name.clone());
                    self.step = ScheduleModalStep::NewBranch;
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::NewBranch => {
                let sanitized = clust_ipc::branch::sanitize_branch_name(&self.input);
                if self.new_branch_required && sanitized.is_empty() {
                    return ScheduleModalResult::Pending;
                }
                self.new_branch_name = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(sanitized)
                };
                self.step = ScheduleModalStep::EnterPrompt;
                self.reset_input();
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::EnterPrompt => {
                if self.input.trim().is_empty() {
                    // Reject empty prompt — only behavioural delta vs Opt+E.
                    return ScheduleModalResult::Pending;
                }
                self.prompt_value = self.input.clone();
                let prompt = self.prompt_value.clone();
                match self.schedule_kind.unwrap_or(ScheduleKindChoice::Unscheduled) {
                    ScheduleKindChoice::Schedule => {
                        self.step = ScheduleModalStep::EnterStartTime;
                        self.reset_input();
                        ScheduleModalResult::Pending
                    }
                    ScheduleKindChoice::Depend => {
                        self.step = ScheduleModalStep::SelectDependencies;
                        self.reset_input();
                        ScheduleModalResult::Pending
                    }
                    ScheduleKindChoice::Unscheduled => {
                        self.complete_with(prompt, ScheduleKind::Unscheduled)
                    }
                }
            }
            ScheduleModalStep::EnterStartTime => match parse_time(&self.input, Utc::now()) {
                Ok(start_at) => {
                    let prompt = self.recover_prompt();
                    self.complete_with(
                        prompt,
                        ScheduleKind::Time {
                            start_at: start_at.to_rfc3339(),
                        },
                    )
                }
                Err(e) => {
                    self.time_error = Some(e);
                    ScheduleModalResult::Pending
                }
            },
            ScheduleModalStep::SelectDependencies => {
                if self.selected_deps.is_empty() {
                    // Require at least one dep — otherwise the user should
                    // have picked Unscheduled.
                    return ScheduleModalResult::Pending;
                }
                let prompt = self.recover_prompt();
                let depends_on_ids = self.selected_deps.clone();
                self.complete_with(prompt, ScheduleKind::Depend { depends_on_ids })
            }
        }
    }

    fn recover_prompt(&self) -> String {
        self.prompt_value.clone()
    }

    fn complete_with(&mut self, prompt: String, schedule: ScheduleKind) -> ScheduleModalResult {
        let repo = self
            .selected_repo
            .as_ref()
            .expect("repo set before completion");
        ScheduleModalResult::Completed(ScheduleModalOutput {
            repo_path: repo.path.clone(),
            base_branch: self.target_branch.clone(),
            new_branch: self.new_branch_name.clone(),
            prompt,
            plan_mode: self.plan_mode,
            auto_exit: self.auto_exit,
            schedule,
        })
    }

    fn toggle_dep_at_cursor(&mut self) {
        let filtered = self.filtered_deps();
        if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
            let id = self.all_tasks[idx].id.clone();
            if let Some(pos) = self.selected_deps.iter().position(|x| x == &id) {
                self.selected_deps.remove(pos);
            } else {
                self.selected_deps.push(id);
            }
        }
    }

    fn clamp_selected_idx(&mut self) {
        let len = self.filtered_count();
        if len == 0 {
            self.selected_idx = 0;
        } else {
            self.selected_idx = self.selected_idx.min(len - 1);
        }
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

    fn kind_choices(&self) -> Vec<ScheduleKindChoice> {
        // Hide Depend if there are no existing tasks to depend on.
        if self.all_tasks.is_empty() {
            vec![ScheduleKindChoice::Schedule, ScheduleKindChoice::Unscheduled]
        } else {
            vec![
                ScheduleKindChoice::Schedule,
                ScheduleKindChoice::Depend,
                ScheduleKindChoice::Unscheduled,
            ]
        }
    }

    // -- Filtering --

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

    fn filtered_deps(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .all_tasks
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .all_tasks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let label = format!("{} {}", t.repo_name, t.branch_name);
                self.matcher
                    .fuzzy_match(&label, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    fn filtered_count(&self) -> usize {
        match self.step {
            ScheduleModalStep::SelectRepo => self.filtered_repos().len(),
            ScheduleModalStep::SelectScheduleKind => self.kind_choices().len(),
            ScheduleModalStep::SelectBranch => self.filtered_branches().len(),
            ScheduleModalStep::SelectDependencies => self.filtered_deps().len(),
            _ => 0,
        }
    }

    // -- Rendering --

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 70u16.min(area.width.saturating_sub(4));
        let modal_height = (area.height * 70 / 100)
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

        let is_prompt = self.step == ScheduleModalStep::EnterPrompt;
        let [hint, input, _gap, list, _spacer, status] = Layout::vertical([
            Constraint::Length(1),
            if is_prompt {
                Constraint::Min(3)
            } else {
                Constraint::Length(1)
            },
            if is_prompt {
                Constraint::Length(0)
            } else {
                Constraint::Length(1)
            },
            if is_prompt {
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
            hint,
        );
        self.render_input(frame, input);
        match self.step {
            ScheduleModalStep::SelectRepo => self.render_repo_list(frame, list),
            ScheduleModalStep::SelectScheduleKind => self.render_kind_list(frame, list),
            ScheduleModalStep::SelectBranch => self.render_branch_list(frame, list),
            ScheduleModalStep::SelectDependencies => self.render_deps_list(frame, list),
            ScheduleModalStep::NewBranch
            | ScheduleModalStep::EnterPrompt
            | ScheduleModalStep::EnterStartTime => {}
        }
        self.render_status_bar(frame, status);
    }

    fn step_title(&self) -> String {
        match self.step {
            ScheduleModalStep::SelectRepo => "Schedule task — select repository".into(),
            ScheduleModalStep::SelectScheduleKind => "Schedule task — pick when to start".into(),
            ScheduleModalStep::SelectBranch => "Schedule task — select branch".into(),
            ScheduleModalStep::NewBranch => "Schedule task — new branch (Enter to skip)".into(),
            ScheduleModalStep::EnterPrompt => "Schedule task — prompt (required)".into(),
            ScheduleModalStep::EnterStartTime => "Schedule task — start time".into(),
            ScheduleModalStep::SelectDependencies => "Schedule task — pick dependencies".into(),
        }
    }

    fn step_hint(&self) -> String {
        match self.step {
            ScheduleModalStep::SelectRepo => "↑/↓ select · Enter confirm · Esc cancel".into(),
            ScheduleModalStep::SelectScheduleKind => "↑/↓ select · Enter confirm".into(),
            ScheduleModalStep::SelectBranch => {
                "type to filter, Enter pick existing — leave empty to create one".into()
            }
            ScheduleModalStep::NewBranch => {
                if self.new_branch_required {
                    "type the new branch name (required, no existing branches)".into()
                } else {
                    "type to create a new branch — Enter to skip and reuse selected".into()
                }
            }
            ScheduleModalStep::EnterPrompt => "type prompt — must be non-empty".into(),
            ScheduleModalStep::EnterStartTime => match &self.time_error {
                Some(e) => e.clone(),
                None => "examples: 5m · 2h · 1d · 30s · 20:00".into(),
            },
            ScheduleModalStep::SelectDependencies => {
                "Space toggles, Enter confirms (≥1 required)".into()
            }
        }
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
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
        let p = Paragraph::new(line)
            .style(Style::default().bg(theme::R_BG_INPUT))
            .wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    }

    fn render_repo_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_repos();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(idx, _))| {
                let r = &self.repos[idx];
                self.render_simple_item(&r.name, Some(&r.path), i == self.selected_idx)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_kind_list(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = self
            .kind_choices()
            .iter()
            .enumerate()
            .map(|(i, c)| self.render_simple_item(c.label(), None, i == self.selected_idx))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_branch_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_branches();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(idx, _))| {
                let b = &self.branches[idx];
                self.render_simple_item(&b.name, None, i == self.selected_idx)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_deps_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_deps();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(idx, _))| {
                let task = &self.all_tasks[idx];
                let checked = self.selected_deps.iter().any(|x| x == &task.id);
                let cursor = i == self.selected_idx;
                let mark = if checked { "[x] " } else { "[ ] " };
                let status = match task.status {
                    ScheduledTaskStatus::Active => "ACTIVE",
                    ScheduledTaskStatus::Inactive => "INACTIVE",
                    ScheduledTaskStatus::Complete => "COMPLETE",
                    ScheduledTaskStatus::Aborted => "ABORTED",
                };
                let label = format!(
                    "{}{} / {} [{}]",
                    mark, task.repo_name, task.branch_name, status
                );
                let style = if cursor {
                    Style::default()
                        .fg(theme::R_BG_BASE)
                        .bg(theme::R_ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::R_TEXT_PRIMARY)
                };
                Line::from(Span::styled(label, style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_simple_item(&self, primary: &str, secondary: Option<&str>, selected: bool) -> Line<'_> {
        let primary_style = if selected {
            Style::default()
                .fg(theme::R_BG_BASE)
                .bg(theme::R_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::R_TEXT_PRIMARY)
        };
        let mut spans = vec![Span::styled(format!("  {primary}  "), primary_style)];
        if let Some(sec) = secondary {
            spans.push(Span::styled(
                format!("  {sec}"),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ));
        }
        Line::from(spans)
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let mut spans: Vec<Span> = Vec::new();
        spans.push(if self.plan_mode {
            Span::styled(
                "PLAN",
                Style::default()
                    .fg(theme::R_WARNING)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("Plan", Style::default().fg(theme::R_TEXT_DISABLED))
        });
        spans.push(Span::styled("  ", Style::default()));
        spans.push(if self.auto_exit {
            Span::styled(
                "AUTO-EXIT",
                Style::default()
                    .fg(theme::R_INFO)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("Auto-Exit", Style::default().fg(theme::R_TEXT_DISABLED))
        });
        spans.push(Span::styled(
            format!("    {mod_key}+P plan · {mod_key}+X auto-exit"),
            Style::default().fg(theme::R_TEXT_DISABLED),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// ---------------------------------------------------------------------------
// Time parsing
// ---------------------------------------------------------------------------

/// Parse a user-typed time/duration string into an absolute UTC timestamp.
///
/// Accepted forms (case-insensitive, surrounding whitespace tolerated):
/// - `Ns` / `Nm` / `Nh` / `Nd` — relative to `now`. `N >= 1`.
/// - `HH:MM` (24-hour, local time) — today if still in the future, otherwise
///   tomorrow. The result is converted to UTC for storage.
///
/// Anything else returns `Err(<short message>)` so the modal can show the
/// error inline.
pub fn parse_time(input: &str, now: chrono::DateTime<Utc>) -> Result<chrono::DateTime<Utc>, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("enter a duration (e.g. 5m, 2h) or a wall-clock time (HH:MM)".into());
    }

    // Wall-clock: HH:MM
    if let Some((h, m)) = s.split_once(':') {
        let h: u32 = h.trim().parse().map_err(|_| invalid_msg())?;
        let m: u32 = m.trim().parse().map_err(|_| invalid_msg())?;
        if h > 23 || m > 59 {
            return Err("HH:MM out of range".into());
        }
        let local_now = Local::now();
        let today = local_now.date_naive();
        let target_naive_today = today
            .and_time(NaiveTime::from_hms_opt(h, m, 0).ok_or_else(invalid_msg)?);
        let target_local_today = Local
            .from_local_datetime(&target_naive_today)
            .single()
            .ok_or_else(|| "ambiguous local time (DST)".to_string())?;
        let target_local = if target_local_today > local_now {
            target_local_today
        } else {
            // Add a day, re-localize.
            let tomorrow = today
                .succ_opt()
                .ok_or_else(|| "calendar overflow".to_string())?;
            let tomorrow_at = tomorrow
                .and_time(NaiveTime::from_hms_opt(h, m, 0).ok_or_else(invalid_msg)?);
            Local
                .from_local_datetime(&tomorrow_at)
                .single()
                .ok_or_else(|| "ambiguous local time (DST)".to_string())?
        };
        return Ok(target_local.with_timezone(&Utc));
    }

    // Duration: trailing unit
    let last = s.chars().last().ok_or_else(invalid_msg)?;
    let unit = match last {
        's' | 'm' | 'h' | 'd' => last,
        _ => return Err(invalid_msg()),
    };
    let num_part = &s[..s.len() - 1];
    let n: i64 = num_part.trim().parse().map_err(|_| invalid_msg())?;
    if n < 1 {
        return Err("duration must be ≥ 1".into());
    }
    let delta = match unit {
        's' => ChronoDuration::seconds(n),
        'm' => ChronoDuration::minutes(n),
        'h' => ChronoDuration::hours(n),
        'd' => ChronoDuration::days(n),
        _ => unreachable!(),
    };
    Ok(now + delta)
}

fn invalid_msg() -> String {
    "expected Ns/Nm/Nh/Nd or HH:MM".into()
}


#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_minutes() {
        let t = parse_time("5m", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::minutes(5));
    }

    #[test]
    fn parses_seconds() {
        let t = parse_time("30s", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::seconds(30));
    }

    #[test]
    fn parses_hours() {
        let t = parse_time("2h", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::hours(2));
    }

    #[test]
    fn parses_days() {
        let t = parse_time("1d", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::days(1));
    }

    #[test]
    fn case_insensitive_and_trims() {
        let t = parse_time("  2H  ", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::hours(2));
    }

    #[test]
    fn rejects_zero() {
        assert!(parse_time("0m", fixed_now()).is_err());
    }

    #[test]
    fn rejects_negative() {
        assert!(parse_time("-5m", fixed_now()).is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_time("5x", fixed_now()).is_err());
    }

    #[test]
    fn rejects_no_unit() {
        assert!(parse_time("5", fixed_now()).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_time("   ", fixed_now()).is_err());
    }

    #[test]
    fn parses_wall_clock_today() {
        // Pick an HH:MM well after fixed_now (12:00 UTC). Use 23:00 local to
        // avoid DST edge cases.
        let t = parse_time("23:00", fixed_now()).unwrap();
        // The result should be in the future relative to "now".
        assert!(t > fixed_now());
    }

    #[test]
    fn parses_wall_clock_tomorrow_when_already_past() {
        // Use a time guaranteed to be in the past for any fixed_now we pick.
        // We can't depend on "current local time", but we can verify that the
        // returned timestamp is at most ~24 hours after `now`.
        let t = parse_time("00:00", fixed_now()).unwrap();
        let delta = t - fixed_now();
        assert!(
            delta.num_seconds() >= 0 && delta.num_seconds() <= 25 * 3600,
            "expected within next 25h, got {} s",
            delta.num_seconds()
        );
    }

    #[test]
    fn rejects_bad_hour() {
        assert!(parse_time("25:00", fixed_now()).is_err());
    }

    #[test]
    fn rejects_bad_minute() {
        assert!(parse_time("12:99", fixed_now()).is_err());
    }
}
