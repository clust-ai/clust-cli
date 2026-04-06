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

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModalStep {
    SelectRepo,
    SelectBranch,
    NewBranch,
    EnterPrompt,
}

pub enum ModalResult {
    Pending,
    Cancelled,
    Completed(ModalOutput),
}

pub struct ModalOutput {
    pub repo_path: String,
    pub target_branch: Option<String>,
    pub new_branch: Option<String>,
    pub prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct CreateAgentModal {
    step: ModalStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    // Data
    repos: Vec<RepoInfo>,
    branches: Vec<BranchInfo>,

    // Accumulated selections
    selected_repo: Option<RepoInfo>,
    target_branch: Option<String>,
    new_branch_name: Option<String>,

    // Whether new branch input is required (no branches exist)
    new_branch_required: bool,
    // Whether the modal was opened with repo+branch pre-selected (e.g. "Base Worktree Off")
    pre_selected: bool,

    matcher: SkimMatcherV2,
}

impl CreateAgentModal {
    pub fn new(repos: Vec<RepoInfo>) -> Self {
        Self {
            step: ModalStep::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            selected_repo: None,
            target_branch: None,
            new_branch_name: None,
            new_branch_required: false,
            pre_selected: false,
            matcher: SkimMatcherV2::default(),
        }
    }

    pub fn new_with_branch(repos: Vec<RepoInfo>, repo: RepoInfo, branch_name: String) -> Self {
        Self {
            step: ModalStep::NewBranch,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            selected_repo: Some(repo),
            target_branch: Some(branch_name),
            new_branch_name: None,
            new_branch_required: true,
            pre_selected: true,
            matcher: SkimMatcherV2::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> ModalResult {
        match key.code {
            KeyCode::Esc => {
                match self.step {
                    ModalStep::SelectRepo => return ModalResult::Cancelled,
                    ModalStep::SelectBranch => {
                        self.step = ModalStep::SelectRepo;
                        self.selected_repo = None;
                        self.branches.clear();
                    }
                    ModalStep::NewBranch => {
                        if self.pre_selected {
                            return ModalResult::Cancelled;
                        } else if self.new_branch_required {
                            self.step = ModalStep::SelectRepo;
                            self.selected_repo = None;
                        } else {
                            self.step = ModalStep::SelectBranch;
                            self.target_branch = None;
                        }
                    }
                    ModalStep::EnterPrompt => {
                        self.step = ModalStep::NewBranch;
                        self.new_branch_name = None;
                    }
                }
                self.reset_input();
                ModalResult::Pending
            }
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
                    self.cursor_pos +=
                        self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
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
                self.selected_idx = 0;
                ModalResult::Pending
            }
            _ => ModalResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> ModalResult {
        match self.step {
            ModalStep::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.branches = repo.local_branches.clone();
                    self.selected_repo = Some(repo);

                    if self.branches.is_empty() {
                        self.new_branch_required = true;
                        self.step = ModalStep::NewBranch;
                    } else {
                        self.step = ModalStep::SelectBranch;
                    }
                    self.reset_input();
                }
                ModalResult::Pending
            }
            ModalStep::SelectBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.target_branch = Some(self.branches[idx].name.clone());
                    self.step = ModalStep::NewBranch;
                    self.reset_input();
                }
                ModalResult::Pending
            }
            ModalStep::NewBranch => {
                let sanitized = clust_ipc::branch::sanitize_branch_name(&self.input);
                if self.new_branch_required && sanitized.is_empty() {
                    return ModalResult::Pending;
                }
                self.new_branch_name = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(sanitized)
                };
                self.step = ModalStep::EnterPrompt;
                self.reset_input();
                ModalResult::Pending
            }
            ModalStep::EnterPrompt => {
                let prompt = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(self.input.clone())
                };
                let repo = self.selected_repo.as_ref().unwrap();
                ModalResult::Completed(ModalOutput {
                    repo_path: repo.path.clone(),
                    target_branch: self.target_branch.clone(),
                    new_branch: self.new_branch_name.clone(),
                    prompt,
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
            ModalStep::SelectRepo => self.filtered_repos().len(),
            ModalStep::SelectBranch => self.filtered_branches().len(),
            ModalStep::NewBranch | ModalStep::EnterPrompt => 0,
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

        let is_prompt_step = self.step == ModalStep::EnterPrompt;
        let [hint_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            if is_prompt_step { Constraint::Min(3) } else { Constraint::Length(1) },
            if is_prompt_step { Constraint::Length(0) } else { Constraint::Length(1) },
            if is_prompt_step { Constraint::Length(0) } else { Constraint::Min(0) },
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

        // List or hints
        match self.step {
            ModalStep::SelectRepo => self.render_repo_list(frame, list_area),
            ModalStep::SelectBranch => self.render_branch_list(frame, list_area),
            ModalStep::NewBranch => self.render_new_branch_hint(frame, list_area),
            ModalStep::EnterPrompt => {}
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

    fn render_new_branch_hint(&self, frame: &mut Frame, area: Rect) {
        let hint = if self.new_branch_required {
            "Enter a branch name to create (required)"
        } else {
            "Enter a new branch name, or press Enter to use the target branch"
        };
        let mut lines = vec![Line::from(Span::styled(
            hint,
            Style::default().fg(theme::R_TEXT_TERTIARY),
        ))];
        if let Some(ref target) = self.target_branch {
            lines.push(Line::from(vec![
                Span::styled("  Target: ", Style::default().fg(theme::R_TEXT_TERTIARY)),
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

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn render_list_item<'a>(&self, name: &'a str, detail: Option<&'a str>, selected: bool) -> Line<'a> {
        let spans = self.list_item_spans(name, selected);
        let mut all = spans;
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

    fn step_title(&self) -> &'static str {
        if self.pre_selected {
            match self.step {
                ModalStep::NewBranch => "Step 1/2 \u{2014} New branch",
                ModalStep::EnterPrompt => "Step 2/2 \u{2014} Enter prompt",
                _ => unreachable!(),
            }
        } else {
            match self.step {
                ModalStep::SelectRepo => "Step 1/4 \u{2014} Select repository",
                ModalStep::SelectBranch => "Step 2/4 \u{2014} Select target branch",
                ModalStep::NewBranch => "Step 3/4 \u{2014} New branch",
                ModalStep::EnterPrompt => "Step 4/4 \u{2014} Enter prompt",
            }
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            ModalStep::SelectRepo => "Type to filter, Enter to select, Esc to cancel",
            ModalStep::SelectBranch => "Type to filter, Enter to select, Esc to go back",
            ModalStep::NewBranch => {
                if self.pre_selected {
                    "Type branch name and press Enter, Esc to cancel"
                } else {
                    "Type branch name and press Enter, Esc to go back"
                }
            }
            ModalStep::EnterPrompt => "Type a prompt for the agent, Enter to start",
        }
    }
}
