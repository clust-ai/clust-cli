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
use serde::Deserialize;

use crate::tasks::LaunchMode;
use crate::theme;

// ---------------------------------------------------------------------------
// Batch JSON schema
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BatchJson {
    /// Optional batch title. Falls back to auto-naming ("Batch N") if omitted.
    pub title: Option<String>,
    /// Optional prompt prefix prepended to every task prompt.
    pub prefix: Option<String>,
    /// Optional prompt suffix appended to every task prompt.
    pub suffix: Option<String>,
    /// Launch mode: "auto" (default) or "manual".
    pub launch_mode: Option<String>,
    /// Max concurrent agents (auto mode only). Null/omitted = unlimited.
    pub max_concurrent: Option<usize>,
    /// Whether agents start in plan mode.
    #[serde(default)]
    pub plan_mode: bool,
    /// Whether agents can bypass permission prompts.
    #[serde(default)]
    pub allow_bypass: bool,
    /// The tasks to create in this batch.
    pub tasks: Vec<TaskJson>,
    /// Optional list of batch titles this batch depends on.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Deserialize)]
pub struct TaskJson {
    /// Branch name for the worktree.
    pub branch: String,
    /// The prompt for the agent.
    pub prompt: String,
    /// Whether the batch prompt prefix is applied to this task. Defaults to `true`.
    #[serde(default = "default_true")]
    pub use_prefix: bool,
    /// Whether the batch prompt suffix is applied to this task. Defaults to `true`.
    #[serde(default = "default_true")]
    pub use_suffix: bool,
    /// Whether this task starts in plan mode. Defaults to batch-level plan_mode if omitted.
    #[serde(default)]
    pub plan_mode: bool,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub enum ImportBatchResult {
    Pending,
    Cancelled,
    Completed(Box<ImportBatchOutput>),
}

pub struct ImportBatchOutput {
    pub batches: Vec<BatchJson>,
    /// The repo path selected by the user.
    pub repo_path: String,
    /// The repo name selected by the user.
    pub repo_name: String,
    /// The branch name selected by the user.
    pub branch_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    File,
    Repo,
    Branch,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct ImportBatchModal {
    step: Step,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    // File browser state
    base_path: String,
    file_entries: Vec<FileEntry>,

    // Parsed batches (list format)
    parsed_batches: Option<Vec<BatchJson>>,
    parse_error: Option<String>,

    // Repo/branch selection (reused from create_batch_modal pattern)
    repos: Vec<clust_ipc::RepoInfo>,
    branches: Vec<clust_ipc::BranchInfo>,
    selected_repo: Option<clust_ipc::RepoInfo>,

    matcher: SkimMatcherV2,
}

#[derive(Clone)]
struct FileEntry {
    name: String,
    is_dir: bool,
}

impl ImportBatchModal {
    pub fn new(repos: Vec<clust_ipc::RepoInfo>) -> Self {
        let downloads = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());

        let base_path = if downloads.ends_with('/') {
            downloads
        } else {
            format!("{downloads}/")
        };

        let mut modal = Self {
            step: Step::File,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            base_path,
            file_entries: Vec::new(),
            parsed_batches: None,
            parse_error: None,
            repos,
            branches: Vec::new(),
            selected_repo: None,
            matcher: SkimMatcherV2::default(),
        };
        modal.refresh_entries();
        modal
    }

    // -----------------------------------------------------------------------
    // File browser
    // -----------------------------------------------------------------------

    fn refresh_entries(&mut self) {
        self.file_entries.clear();
        if let Ok(read) = std::fs::read_dir(&self.base_path) {
            for entry in read.flatten() {
                let is_dir = entry
                    .file_type()
                    .map(|ft| ft.is_dir())
                    .unwrap_or(false);
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with('.') {
                        continue;
                    }
                    if is_dir {
                        self.file_entries.push(FileEntry {
                            name: name.to_string(),
                            is_dir: true,
                        });
                    } else if name.ends_with(".json") {
                        self.file_entries.push(FileEntry {
                            name: name.to_string(),
                            is_dir: false,
                        });
                    }
                }
            }
        }
        // Sort: directories first, then files, both alphabetically
        self.file_entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name))
        });
        self.selected_idx = 0;
    }

    fn filtered_entries(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .file_entries
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .file_entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                self.matcher
                    .fuzzy_match(&entry.name, &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by(|a, b| b.1.cmp(&a.1));
        results
    }

    fn try_parse_file(&mut self, path: &str) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                // Try list format first, then single-object format (backward compat)
                let batches = serde_json::from_str::<Vec<BatchJson>>(&contents)
                    .or_else(|_| {
                        serde_json::from_str::<BatchJson>(&contents).map(|b| vec![b])
                    });
                match batches {
                    Ok(list) => {
                        if list.is_empty() || list.iter().all(|b| b.tasks.is_empty()) {
                            self.parse_error = Some("JSON has no tasks".to_string());
                        } else {
                            self.parsed_batches = Some(list);
                            self.parse_error = None;
                            self.step = Step::Repo;
                            self.reset_input();
                        }
                    }
                    Err(e) => {
                        self.parse_error = Some(format!("Invalid JSON: {e}"));
                    }
                }
            }
            Err(e) => {
                self.parse_error = Some(format!("Cannot read file: {e}"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Repo/branch filtering (same as create_batch_modal)
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
            Step::File => self.filtered_entries().len(),
            Step::Repo => self.filtered_repos().len(),
            Step::Branch => self.filtered_branches().len(),
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> ImportBatchResult {
        match key.code {
            KeyCode::Esc => self.handle_esc(),
            KeyCode::Tab => {
                if self.step == Step::File {
                    // Tab enters a directory
                    let filtered = self.filtered_entries();
                    if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                        let entry = &self.file_entries[idx];
                        if entry.is_dir {
                            self.base_path = format!("{}{}/", self.base_path, entry.name);
                            self.reset_input();
                            self.refresh_entries();
                        }
                    }
                }
                ImportBatchResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                ImportBatchResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                ImportBatchResult::Pending
            }
            KeyCode::Backspace => {
                if self.step == Step::File
                    && self.input.is_empty()
                    && self.base_path != "/"
                {
                    // Navigate up
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
                    self.parse_error = None;
                }
                ImportBatchResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                ImportBatchResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos +=
                        self.input[self.cursor_pos..].chars().next().unwrap().len_utf8();
                }
                ImportBatchResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return ImportBatchResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                self.parse_error = None;
                ImportBatchResult::Pending
            }
            _ => ImportBatchResult::Pending,
        }
    }

    fn handle_esc(&mut self) -> ImportBatchResult {
        match self.step {
            Step::File => ImportBatchResult::Cancelled,
            Step::Repo => {
                self.step = Step::File;
                self.parsed_batches = None;
                self.reset_input();
                ImportBatchResult::Pending
            }
            Step::Branch => {
                self.step = Step::Repo;
                self.selected_repo = None;
                self.branches.clear();
                self.reset_input();
                ImportBatchResult::Pending
            }
        }
    }

    fn handle_enter(&mut self) -> ImportBatchResult {
        match self.step {
            Step::File => {
                let filtered = self.filtered_entries();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let entry = self.file_entries[idx].clone();
                    if entry.is_dir {
                        self.base_path = format!("{}{}/", self.base_path, entry.name);
                        self.reset_input();
                        self.refresh_entries();
                    } else {
                        let path = format!("{}{}", self.base_path, entry.name);
                        self.try_parse_file(&path);
                    }
                }
                ImportBatchResult::Pending
            }
            Step::Repo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.branches = repo.local_branches.clone();
                    self.selected_repo = Some(repo);
                    if self.branches.is_empty() {
                        self.selected_repo = None;
                        return ImportBatchResult::Pending;
                    }
                    self.step = Step::Branch;
                    self.reset_input();
                }
                ImportBatchResult::Pending
            }
            Step::Branch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let branch_name = self.branches[idx].name.clone();
                    let repo = self.selected_repo.as_ref().unwrap();
                    if let Some(batches) = self.parsed_batches.take() {
                        return ImportBatchResult::Completed(Box::new(ImportBatchOutput {
                            batches,
                            repo_path: repo.path.clone(),
                            repo_name: repo.name.clone(),
                            branch_name,
                        }));
                    }
                }
                ImportBatchResult::Pending
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
        self.parse_error = None;
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
        self.parse_error = None;
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

        match self.step {
            Step::File => self.render_file_browser(frame, inner),
            Step::Repo => self.render_repo_list(frame, inner),
            Step::Branch => self.render_branch_list(frame, inner),
        }
    }

    fn render_file_browser(&self, frame: &mut Frame, area: Rect) {
        let [hint_area, path_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Show current path (with error if present)
        if let Some(ref err) = self.parse_error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    err.as_str(),
                    Style::default().fg(theme::R_ERROR),
                )),
                path_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    &self.base_path,
                    Style::default().fg(theme::R_ACCENT_TEXT),
                )),
                path_area,
            );
        }

        self.render_input(frame, input_area);
        self.render_file_list(frame, list_area);
    }

    fn render_repo_list(&self, frame: &mut Frame, area: Rect) {
        let [hint_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        self.render_input(frame, input_area);

        let filtered = self.filtered_repos();
        let max_visible = list_area.height as usize;
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
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    fn render_branch_list(&self, frame: &mut Frame, area: Rect) {
        let [hint_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                self.step_hint(),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        self.render_input(frame, input_area);

        let filtered = self.filtered_branches();
        let max_visible = list_area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vis_idx, &(orig_idx, _))| {
                let branch = &self.branches[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
                let mut spans = self.list_item_spans(&branch.name, is_selected);
                if branch.is_head {
                    spans.push(Span::styled(
                        " HEAD",
                        Style::default().fg(theme::R_SUCCESS),
                    ));
                }
                if branch.is_worktree {
                    spans.push(Span::styled(
                        " [worktree]",
                        Style::default().fg(theme::R_INFO),
                    ));
                }
                if branch.active_agent_count > 0 {
                    spans.push(Span::styled(
                        format!(
                            " ({} agent{})",
                            branch.active_agent_count,
                            if branch.active_agent_count == 1 { "" } else { "s" }
                        ),
                        Style::default().fg(theme::R_WARNING),
                    ));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    fn render_file_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_entries();
        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vis_idx, &(orig_idx, _))| {
                let entry = &self.file_entries[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
                if entry.is_dir {
                    let mut spans = self.list_item_spans(&entry.name, is_selected);
                    spans.push(Span::styled(
                        "/",
                        Style::default().fg(theme::R_ACCENT),
                    ));
                    Line::from(spans)
                } else {
                    Line::from(self.list_item_spans(&entry.name, is_selected))
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
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

    fn step_title(&self) -> String {
        match self.step {
            Step::File => "Step 1/3 \u{2014} Select batch JSON file".to_string(),
            Step::Repo => "Step 2/3 \u{2014} Select repository".to_string(),
            Step::Branch => "Step 3/3 \u{2014} Select branch".to_string(),
        }
    }

    fn step_hint(&self) -> &'static str {
        match self.step {
            Step::File => "Type to filter, Tab to enter dir, Backspace to go up, Esc to cancel",
            Step::Repo => "Type to filter, Enter to select, Esc to go back",
            Step::Branch => "Type to filter, Enter to import, Esc to go back",
        }
    }
}

/// Parse the launch_mode string from JSON into the internal enum.
pub fn parse_launch_mode(s: Option<&str>) -> LaunchMode {
    match s.map(|s| s.to_lowercase()).as_deref() {
        Some("manual") => LaunchMode::Manual,
        _ => LaunchMode::Auto,
    }
}
