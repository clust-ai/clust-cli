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

use crate::tasks::LaunchMode;
use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BatchModalStep {
    SelectRepo,
    SelectBranch,
    EnterTitle,
    SelectLaunchMode,
    SetConcurrency,
}

pub enum BatchModalResult {
    Pending,
    Cancelled,
    Completed(BatchModalOutput),
}

pub struct BatchModalOutput {
    pub repo_path: String,
    pub repo_name: String,
    pub branch_name: String,
    pub title: Option<String>,
    pub max_concurrent: Option<usize>,
    pub launch_mode: LaunchMode,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct CreateBatchModal {
    step: BatchModalStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    // Data
    repos: Vec<RepoInfo>,
    branches: Vec<BranchInfo>,

    // Accumulated selections
    selected_repo: Option<RepoInfo>,
    selected_branch: Option<String>,
    batch_title: Option<String>,
    launch_mode: LaunchMode,
    max_concurrent: Option<usize>,

    matcher: SkimMatcherV2,
}

impl CreateBatchModal {
    pub fn new(repos: Vec<RepoInfo>) -> Self {
        Self {
            step: BatchModalStep::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            selected_repo: None,
            selected_branch: None,
            batch_title: None,
            launch_mode: LaunchMode::Auto,
            max_concurrent: None,
            matcher: SkimMatcherV2::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> BatchModalResult {
        // Launch mode step has its own key handling
        if self.step == BatchModalStep::SelectLaunchMode {
            return self.handle_launch_mode_key(key);
        }

        // Concurrency step has its own key handling
        if self.step == BatchModalStep::SetConcurrency {
            return self.handle_concurrency_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                match self.step {
                    BatchModalStep::SelectRepo => return BatchModalResult::Cancelled,
                    BatchModalStep::SelectBranch => {
                        self.step = BatchModalStep::SelectRepo;
                        self.selected_repo = None;
                        self.branches.clear();
                    }
                    BatchModalStep::EnterTitle => {
                        self.step = BatchModalStep::SelectBranch;
                        self.selected_branch = None;
                    }
                    BatchModalStep::SelectLaunchMode | BatchModalStep::SetConcurrency => unreachable!(),
                }
                self.reset_input();
                BatchModalResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                BatchModalResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                BatchModalResult::Pending
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
                BatchModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                BatchModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos +=
                        self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
                }
                BatchModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return BatchModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                BatchModalResult::Pending
            }
            _ => BatchModalResult::Pending,
        }
    }

    fn handle_launch_mode_key(&mut self, key: KeyEvent) -> BatchModalResult {
        match key.code {
            KeyCode::Esc => {
                self.step = BatchModalStep::EnterTitle;
                self.reset_input();
                if let Some(ref title) = self.batch_title {
                    self.input = title.clone();
                    self.cursor_pos = self.input.len();
                }
                BatchModalResult::Pending
            }
            KeyCode::Up | KeyCode::Down => {
                self.launch_mode = match self.launch_mode {
                    LaunchMode::Auto => LaunchMode::Manual,
                    LaunchMode::Manual => LaunchMode::Auto,
                };
                self.selected_idx = match self.launch_mode {
                    LaunchMode::Auto => 0,
                    LaunchMode::Manual => 1,
                };
                BatchModalResult::Pending
            }
            KeyCode::Enter => {
                match self.launch_mode {
                    LaunchMode::Auto => {
                        self.step = BatchModalStep::SetConcurrency;
                        self.reset_input();
                        BatchModalResult::Pending
                    }
                    LaunchMode::Manual => {
                        let repo = self.selected_repo.as_ref().unwrap();
                        BatchModalResult::Completed(BatchModalOutput {
                            repo_path: repo.path.clone(),
                            repo_name: repo.name.clone(),
                            branch_name: self.selected_branch.clone().unwrap(),
                            title: self.batch_title.clone(),
                            max_concurrent: None,
                            launch_mode: LaunchMode::Manual,
                        })
                    }
                }
            }
            _ => BatchModalResult::Pending,
        }
    }

    fn handle_concurrency_key(&mut self, key: KeyEvent) -> BatchModalResult {
        match key.code {
            KeyCode::Esc => {
                self.step = BatchModalStep::SelectLaunchMode;
                self.reset_input();
                self.selected_idx = 0; // Auto is selected
                BatchModalResult::Pending
            }
            KeyCode::Enter => {
                let repo = self.selected_repo.as_ref().unwrap();
                BatchModalResult::Completed(BatchModalOutput {
                    repo_path: repo.path.clone(),
                    repo_name: repo.name.clone(),
                    branch_name: self.selected_branch.clone().unwrap(),
                    title: self.batch_title.clone(),
                    max_concurrent: self.max_concurrent,
                    launch_mode: LaunchMode::Auto,
                })
            }
            KeyCode::Up | KeyCode::Right => {
                self.max_concurrent = Some(self.max_concurrent.map_or(1, |v| v.saturating_add(1)));
                BatchModalResult::Pending
            }
            KeyCode::Down | KeyCode::Left => {
                self.max_concurrent = match self.max_concurrent {
                    Some(v) if v > 1 => Some(v - 1),
                    _ => None,
                };
                BatchModalResult::Pending
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let digit = c.to_digit(10).unwrap() as usize;
                self.max_concurrent = Some(match self.max_concurrent {
                    Some(v) if v < 1000 => v * 10 + digit,
                    _ => digit,
                });
                if self.max_concurrent == Some(0) {
                    self.max_concurrent = None;
                }
                BatchModalResult::Pending
            }
            KeyCode::Backspace => {
                self.max_concurrent = match self.max_concurrent {
                    Some(v) if v >= 10 => Some(v / 10),
                    _ => None,
                };
                BatchModalResult::Pending
            }
            _ => BatchModalResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> BatchModalResult {
        match self.step {
            BatchModalStep::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.branches = repo.local_branches.clone();
                    self.selected_repo = Some(repo);

                    if self.branches.is_empty() {
                        // No branches — can't proceed
                        self.selected_repo = None;
                        return BatchModalResult::Pending;
                    }
                    self.step = BatchModalStep::SelectBranch;
                    self.reset_input();
                }
                BatchModalResult::Pending
            }
            BatchModalStep::SelectBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.selected_branch = Some(self.branches[idx].name.clone());
                    self.step = BatchModalStep::EnterTitle;
                    self.reset_input();
                }
                BatchModalResult::Pending
            }
            BatchModalStep::EnterTitle => {
                self.batch_title = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(self.input.trim().to_string())
                };
                self.step = BatchModalStep::SelectLaunchMode;
                self.reset_input();
                BatchModalResult::Pending
            }
            BatchModalStep::SelectLaunchMode | BatchModalStep::SetConcurrency => unreachable!(),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.step == BatchModalStep::SelectLaunchMode {
            return; // Paste is meaningless for launch mode selection
        }
        if self.step == BatchModalStep::SetConcurrency {
            // Only accept digits in concurrency step
            for c in text.chars() {
                if c.is_ascii_digit() {
                    let digit = c.to_digit(10).unwrap() as usize;
                    self.max_concurrent = Some(match self.max_concurrent {
                        Some(v) if v < 1000 => v * 10 + digit,
                        _ => digit,
                    });
                }
            }
            if self.max_concurrent == Some(0) {
                self.max_concurrent = None;
            }
            return;
        }
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
        results.sort_by(|a, b| b.1.cmp(&a.1));
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
        results.sort_by(|a, b| b.1.cmp(&a.1));
        results
    }

    fn filtered_count(&self) -> usize {
        match self.step {
            BatchModalStep::SelectRepo => self.filtered_repos().len(),
            BatchModalStep::SelectBranch => self.filtered_branches().len(),
            BatchModalStep::EnterTitle | BatchModalStep::SetConcurrency => 0,
            BatchModalStep::SelectLaunchMode => 2,
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

        let is_concurrency_step = self.step == BatchModalStep::SetConcurrency;
        let is_title_step = self.step == BatchModalStep::EnterTitle;
        let is_launch_mode_step = self.step == BatchModalStep::SelectLaunchMode;
        let show_list = !is_concurrency_step && !is_title_step;

        let [hint_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            if show_list { Constraint::Length(1) } else { Constraint::Length(0) },
            Constraint::Min(0),
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
        if is_launch_mode_step {
            // No text input for launch mode step
        } else if is_concurrency_step {
            self.render_concurrency_input(frame, input_area);
        } else {
            self.render_input(frame, input_area);
        }

        // List or hints
        match self.step {
            BatchModalStep::SelectRepo => self.render_repo_list(frame, list_area),
            BatchModalStep::SelectBranch => self.render_branch_list(frame, list_area),
            BatchModalStep::EnterTitle => self.render_title_hint(frame, list_area),
            BatchModalStep::SelectLaunchMode => self.render_launch_mode_list(frame, list_area),
            BatchModalStep::SetConcurrency => self.render_concurrency_hint(frame, list_area),
        }
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

    fn render_concurrency_input(&self, frame: &mut Frame, area: Rect) {
        let value_text = match self.max_concurrent {
            Some(v) => v.to_string(),
            None => "\u{221E}".to_string(), // ∞
        };

        let line = Line::from(vec![
            Span::styled(
                "> ",
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                &value_text,
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ",
                Style::default()
                    .fg(theme::R_BG_BASE)
                    .bg(theme::R_TEXT_PRIMARY),
            ),
        ]);

        frame.render_widget(
            Paragraph::new(line)
                .style(Style::default().bg(theme::R_BG_INPUT)),
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
            .map(|(vis_idx, &(orig_idx, _))| {
                let repo = &self.repos[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
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
            .map(|(vis_idx, &(orig_idx, _))| {
                let branch = &self.branches[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
                let mut suffix_spans = Vec::new();
                if branch.is_head {
                    suffix_spans.push(Span::styled(
                        " HEAD",
                        Style::default().fg(theme::R_SUCCESS),
                    ));
                }
                if branch.is_worktree {
                    suffix_spans.push(Span::styled(
                        " [worktree]",
                        Style::default().fg(theme::R_INFO),
                    ));
                }
                if branch.active_agent_count > 0 {
                    suffix_spans.push(Span::styled(
                        format!(
                            " ({} agent{})",
                            branch.active_agent_count,
                            if branch.active_agent_count == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(theme::R_WARNING),
                    ));
                }
                let mut spans = self.list_item_spans(&branch.name, is_selected);
                spans.extend(suffix_spans);
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_title_hint(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Press Enter with empty input to auto-name",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_launch_mode_list(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") { "Opt" } else { "Alt" };
        let options = [
            ("Auto", "Set max concurrency, agents auto-start"),
            ("Manual", &format!("Start individual tasks with {mod_key}+S")),
        ];
        let lines: Vec<Line> = options
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                let is_selected = self.selected_idx == i;
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

    fn render_concurrency_hint(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "\u{2191}/\u{2193} adjust  \u{00b7}  type a number  \u{00b7}  0 or \u{2193} from 1 = unlimited",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), area);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn render_list_item<'a>(&self, name: &'a str, detail: Option<&'a str>, selected: bool) -> Line<'a> {
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

    fn total_steps(&self) -> usize {
        match self.launch_mode {
            LaunchMode::Auto => 5,
            LaunchMode::Manual => 4,
        }
    }

    fn step_title(&self) -> String {
        let total = self.total_steps();
        match self.step {
            BatchModalStep::SelectRepo => format!("Step 1/{total} \u{2014} Select repository"),
            BatchModalStep::SelectBranch => format!("Step 2/{total} \u{2014} Select branch"),
            BatchModalStep::EnterTitle => format!("Step 3/{total} \u{2014} Batch name"),
            BatchModalStep::SelectLaunchMode => format!("Step 4/{total} \u{2014} Launch mode"),
            BatchModalStep::SetConcurrency => format!("Step 5/{total} \u{2014} Max concurrent agents"),
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            BatchModalStep::SelectRepo => "Type to filter, Enter to select, Esc to cancel",
            BatchModalStep::SelectBranch => "Type to filter, Enter to select, Esc to go back",
            BatchModalStep::EnterTitle => "Name this batch (Enter for auto-name), Esc to go back",
            BatchModalStep::SelectLaunchMode => "\u{2191}/\u{2193} select mode, Enter to confirm, Esc to go back",
            BatchModalStep::SetConcurrency => "Set max concurrent agents, Enter to confirm, Esc to go back",
        }
    }
}
