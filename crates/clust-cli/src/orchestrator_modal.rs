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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OrchestratorStep {
    SelectRepo,
    SelectSourceBranch,
    EnterNewBranch,
    EnterPrompt,
}

pub enum OrchestratorResult {
    Pending,
    Cancelled,
    Completed(OrchestratorOutput),
}

pub struct OrchestratorOutput {
    pub repo_path: String,
    pub source_branch: String,
    pub new_branch: String,
    pub prompt: String,
}

pub struct OrchestratorModal {
    step: OrchestratorStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    repos: Vec<RepoInfo>,
    branches: Vec<BranchInfo>,

    selected_repo: Option<RepoInfo>,
    source_branch: Option<String>,
    new_branch_name: Option<String>,
    /// Whether to show "prompt is required" feedback.
    prompt_error: bool,

    matcher: SkimMatcherV2,
}

impl OrchestratorModal {
    pub fn new(repos: Vec<RepoInfo>) -> Self {
        Self {
            step: OrchestratorStep::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            selected_repo: None,
            source_branch: None,
            new_branch_name: None,
            prompt_error: false,
            matcher: SkimMatcherV2::default(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> OrchestratorResult {
        match key.code {
            KeyCode::Esc => {
                match self.step {
                    OrchestratorStep::SelectRepo => return OrchestratorResult::Cancelled,
                    OrchestratorStep::SelectSourceBranch => {
                        self.step = OrchestratorStep::SelectRepo;
                        self.selected_repo = None;
                        self.branches.clear();
                    }
                    OrchestratorStep::EnterNewBranch => {
                        self.step = OrchestratorStep::SelectSourceBranch;
                        self.source_branch = None;
                    }
                    OrchestratorStep::EnterPrompt => {
                        self.step = OrchestratorStep::EnterNewBranch;
                        self.new_branch_name = None;
                        self.prompt_error = false;
                    }
                }
                self.reset_input();
                OrchestratorResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                OrchestratorResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                OrchestratorResult::Pending
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
                    self.prompt_error = false;
                }
                OrchestratorResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                OrchestratorResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                OrchestratorResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return OrchestratorResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                self.prompt_error = false;
                OrchestratorResult::Pending
            }
            _ => OrchestratorResult::Pending,
        }
    }

    fn handle_enter(&mut self) -> OrchestratorResult {
        match self.step {
            OrchestratorStep::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.branches = repo.local_branches.clone();
                    self.selected_repo = Some(repo);
                    if !self.branches.is_empty() {
                        self.step = OrchestratorStep::SelectSourceBranch;
                    } else {
                        self.step = OrchestratorStep::EnterNewBranch;
                    }
                    self.reset_input();
                }
                OrchestratorResult::Pending
            }
            OrchestratorStep::SelectSourceBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    self.source_branch = Some(self.branches[idx].name.clone());
                    self.step = OrchestratorStep::EnterNewBranch;
                    self.reset_input();
                }
                OrchestratorResult::Pending
            }
            OrchestratorStep::EnterNewBranch => {
                let sanitized = clust_ipc::branch::sanitize_branch_name(&self.input);
                if sanitized.is_empty() {
                    return OrchestratorResult::Pending;
                }
                if self.source_branch.as_deref() == Some(sanitized.as_str()) {
                    return OrchestratorResult::Pending;
                }
                self.new_branch_name = Some(sanitized);
                self.step = OrchestratorStep::EnterPrompt;
                self.reset_input();
                OrchestratorResult::Pending
            }
            OrchestratorStep::EnterPrompt => {
                let trimmed = self.input.trim();
                if trimmed.is_empty() {
                    self.prompt_error = true;
                    return OrchestratorResult::Pending;
                }
                let repo = self.selected_repo.as_ref().unwrap();
                let source = self
                    .source_branch
                    .clone()
                    .unwrap_or_else(|| "main".to_string());
                OrchestratorResult::Completed(OrchestratorOutput {
                    repo_path: repo.path.clone(),
                    source_branch: source,
                    new_branch: self.new_branch_name.clone().unwrap_or_default(),
                    prompt: self.input.clone(),
                })
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                if self.step == OrchestratorStep::EnterPrompt {
                    self.input.insert(self.cursor_pos, '\n');
                    self.cursor_pos += '\n'.len_utf8();
                }
                continue;
            }
            self.input.insert(self.cursor_pos, c);
            self.cursor_pos += c.len_utf8();
        }
        self.selected_idx = 0;
        self.prompt_error = false;
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

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

    fn filtered_count(&self) -> usize {
        match self.step {
            OrchestratorStep::SelectRepo => self.filtered_repos().len(),
            OrchestratorStep::SelectSourceBranch => self.filtered_branches().len(),
            OrchestratorStep::EnterNewBranch | OrchestratorStep::EnterPrompt => 0,
        }
    }

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

        let is_prompt_step = self.step == OrchestratorStep::EnterPrompt;
        let [hint_area, input_area, _gap, list_area, _spacer, status_area] = Layout::vertical([
            Constraint::Length(1),
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

        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        self.render_input(frame, input_area);

        match self.step {
            OrchestratorStep::SelectRepo => self.render_repo_list(frame, list_area),
            OrchestratorStep::SelectSourceBranch => self.render_branch_list(frame, list_area),
            OrchestratorStep::EnterNewBranch => self.render_branch_hint(frame, list_area),
            OrchestratorStep::EnterPrompt => {}
        }

        self.render_status_bar(frame, status_area);
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        if self.prompt_error {
            spans.push(Span::styled(
                "Prompt is required",
                Style::default()
                    .fg(theme::R_ERROR)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                "Orchestrator agent",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }
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
        frame.render_widget(
            Paragraph::new(line)
                .style(Style::default().bg(theme::R_BG_INPUT))
                .wrap(Wrap { trim: false }),
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
                self.render_list_item(&branch.name, None, is_selected)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_branch_hint(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![Line::from(Span::styled(
            "Type a new branch name to clone toward (required)",
            Style::default().fg(theme::R_TEXT_TERTIARY),
        ))];
        if let Some(ref source) = self.source_branch {
            lines.push(Line::from(vec![
                Span::styled(
                    "  Cloning from: ",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ),
                Span::styled(
                    source.clone(),
                    Style::default()
                        .fg(theme::R_ACCENT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_list_item<'a>(
        &self,
        name: &'a str,
        detail: Option<&'a str>,
        selected: bool,
    ) -> Line<'a> {
        let mut spans = if selected {
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
        };
        if let Some(d) = detail {
            spans.push(Span::styled("  ", Style::default()));
            let detail_style = if selected {
                Style::default().fg(theme::R_TEXT_TERTIARY)
            } else {
                Style::default().fg(theme::R_TEXT_DISABLED)
            };
            spans.push(Span::styled(d, detail_style));
        }
        Line::from(spans)
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
            OrchestratorStep::SelectRepo => {
                "Orchestrator \u{2014} Step 1/4 \u{2014} Select repository"
            }
            OrchestratorStep::SelectSourceBranch => {
                "Orchestrator \u{2014} Step 2/4 \u{2014} Clone from"
            }
            OrchestratorStep::EnterNewBranch => {
                "Orchestrator \u{2014} Step 3/4 \u{2014} Clone toward"
            }
            OrchestratorStep::EnterPrompt => "Orchestrator \u{2014} Step 4/4 \u{2014} Enter prompt",
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            OrchestratorStep::SelectRepo => "Type to filter, Enter to select, Esc to cancel",
            OrchestratorStep::SelectSourceBranch => {
                "Pick the branch to clone from. Enter to select, Esc to go back"
            }
            OrchestratorStep::EnterNewBranch => {
                "Type the new integration branch name. Enter to continue, Esc to go back"
            }
            OrchestratorStep::EnterPrompt => {
                "Describe what to plan. PROMPT IS REQUIRED. Enter to launch, Esc to go back"
            }
        }
    }
}
