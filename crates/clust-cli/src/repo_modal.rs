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
enum RepoAction {
    Create,
    Clone,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    ChooseAction,
    SelectDirectory,
    EnterUrl,
    EnterName,
}

pub enum RepoModalResult {
    Pending,
    Cancelled,
    CreateRepo(CreateRepoOutput),
    CloneRepo(CloneRepoOutput),
}

pub struct CreateRepoOutput {
    pub parent_dir: String,
    pub name: String,
}

pub struct CloneRepoOutput {
    pub url: String,
    pub parent_dir: String,
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct RepoModal {
    step: Step,
    action: RepoAction,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    // Directory browser state
    base_path: String,
    dir_entries: Vec<String>,

    // Accumulated values
    parent_dir: String,
    url: String,

    matcher: SkimMatcherV2,
}

impl RepoModal {
    pub fn new() -> Self {
        Self {
            step: Step::ChooseAction,
            action: RepoAction::Create,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            base_path: String::new(),
            dir_entries: Vec::new(),
            parent_dir: String::new(),
            url: String::new(),
            matcher: SkimMatcherV2::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Directory browsing (same pattern as DetachedAgentModal)
    // -----------------------------------------------------------------------

    fn init_directory_browser(&mut self) {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        self.base_path = if home.ends_with('/') {
            home
        } else {
            format!("{home}/")
        };
        self.refresh_entries();
    }

    fn refresh_entries(&mut self) {
        self.dir_entries.clear();
        if let Ok(read) = std::fs::read_dir(&self.base_path) {
            for entry in read.flatten() {
                let ok = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
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
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
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

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> RepoModalResult {
        match key.code {
            KeyCode::Esc => self.handle_esc(),
            KeyCode::Tab => {
                if self.step == Step::SelectDirectory {
                    self.autocomplete_selected();
                }
                RepoModalResult::Pending
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                RepoModalResult::Pending
            }
            KeyCode::Down => {
                let max = match self.step {
                    Step::ChooseAction => 1, // two items
                    Step::SelectDirectory => self.filtered_dirs().len().saturating_sub(1),
                    _ => 0,
                };
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                RepoModalResult::Pending
            }
            KeyCode::Backspace => {
                if self.step == Step::SelectDirectory
                    && self.input.is_empty()
                    && self.base_path != "/"
                {
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
                    if self.step == Step::SelectDirectory || self.step == Step::ChooseAction {
                        self.selected_idx = 0;
                    }
                }
                RepoModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                RepoModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                RepoModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return RepoModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                if self.step == Step::SelectDirectory || self.step == Step::ChooseAction {
                    self.selected_idx = 0;
                }
                RepoModalResult::Pending
            }
            _ => RepoModalResult::Pending,
        }
    }

    fn handle_esc(&mut self) -> RepoModalResult {
        match self.step {
            Step::ChooseAction => RepoModalResult::Cancelled,
            Step::SelectDirectory => {
                self.step = Step::ChooseAction;
                self.reset_input();
                RepoModalResult::Pending
            }
            Step::EnterUrl => {
                self.step = Step::SelectDirectory;
                self.reset_input();
                self.base_path = if self.parent_dir.ends_with('/') {
                    self.parent_dir.clone()
                } else {
                    format!("{}/", self.parent_dir)
                };
                self.refresh_entries();
                RepoModalResult::Pending
            }
            Step::EnterName => {
                self.reset_input();
                match self.action {
                    RepoAction::Clone => {
                        self.step = Step::EnterUrl;
                    }
                    RepoAction::Create => {
                        self.step = Step::SelectDirectory;
                        self.base_path = if self.parent_dir.ends_with('/') {
                            self.parent_dir.clone()
                        } else {
                            format!("{}/", self.parent_dir)
                        };
                        self.refresh_entries();
                    }
                }
                RepoModalResult::Pending
            }
        }
    }

    fn handle_enter(&mut self) -> RepoModalResult {
        match self.step {
            Step::ChooseAction => {
                self.action = if self.selected_idx == 0 {
                    RepoAction::Create
                } else {
                    RepoAction::Clone
                };
                self.step = Step::SelectDirectory;
                self.reset_input();
                self.init_directory_browser();
                RepoModalResult::Pending
            }
            Step::SelectDirectory => {
                // If filter text matches a directory, enter it first
                if !self.input.is_empty() {
                    let filtered = self.filtered_dirs();
                    if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                        let name = self.dir_entries[idx].clone();
                        self.base_path = format!("{}{}/", self.base_path, name);
                        self.reset_input();
                        self.refresh_entries();
                    }
                }
                // Confirm current base_path as parent directory
                let pd = self.base_path.trim_end_matches('/').to_string();
                self.parent_dir = if pd.is_empty() { "/".to_string() } else { pd };
                self.reset_input();
                match self.action {
                    RepoAction::Clone => self.step = Step::EnterUrl,
                    RepoAction::Create => self.step = Step::EnterName,
                }
                RepoModalResult::Pending
            }
            Step::EnterUrl => {
                let url = self.input.trim().to_string();
                if url.is_empty() {
                    return RepoModalResult::Pending;
                }
                self.url = url;
                self.step = Step::EnterName;
                self.reset_input();
                RepoModalResult::Pending
            }
            Step::EnterName => match self.action {
                RepoAction::Create => {
                    let name = self.input.trim().to_string();
                    if name.is_empty() {
                        return RepoModalResult::Pending;
                    }
                    RepoModalResult::CreateRepo(CreateRepoOutput {
                        parent_dir: self.parent_dir.clone(),
                        name,
                    })
                }
                RepoAction::Clone => {
                    let name = self.input.trim().to_string();
                    let name_opt = if name.is_empty() { None } else { Some(name) };
                    RepoModalResult::CloneRepo(CloneRepoOutput {
                        url: self.url.clone(),
                        parent_dir: self.parent_dir.clone(),
                        name: name_opt,
                    })
                }
            },
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
        if self.step == Step::SelectDirectory || self.step == Step::ChooseAction {
            self.selected_idx = 0;
        }
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
            Step::ChooseAction => self.render_choose_action(frame, inner),
            Step::SelectDirectory => self.render_select_directory(frame, inner),
            Step::EnterUrl => self.render_text_input(frame, inner, "Enter the git clone URL"),
            Step::EnterName => {
                let hint = match self.action {
                    RepoAction::Create => "Enter repository name",
                    RepoAction::Clone => "Enter name (optional, press Enter to use default)",
                };
                self.render_text_input(frame, inner, hint);
            }
        }
    }

    fn render_choose_action(&self, frame: &mut Frame, area: Rect) {
        let [hint_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                "Select an action, Enter to confirm, Esc to cancel",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        let items = ["Create new repository", "Clone existing repository"];
        let lines: Vec<Line> = items
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let selected = i == self.selected_idx;
                Line::from(self.list_item_spans(label, selected))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    fn render_select_directory(&self, frame: &mut Frame, area: Rect) {
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
                "Type to filter, Tab to enter dir, Enter to confirm, Esc to go back",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        frame.render_widget(
            Paragraph::new(Span::styled(
                &self.base_path,
                Style::default().fg(theme::R_ACCENT_TEXT),
            )),
            path_area,
        );

        self.render_input(frame, input_area);
        self.render_dir_list(frame, list_area);
    }

    fn render_text_input(&self, frame: &mut Frame, area: Rect, hint: &str) {
        let [hint_area, ctx_area, input_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
        ])
        .areas(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{hint}, Esc to go back"),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Show context: parent directory (and URL for name step of clone)
        let ctx = match (self.step, self.action) {
            (Step::EnterUrl, _) => format!("Directory: {}", self.parent_dir),
            (Step::EnterName, RepoAction::Clone) => {
                let default_name = repo_name_from_url(&self.url).unwrap_or_default();
                format!(
                    "Directory: {}  URL: {}  Default name: {}",
                    self.parent_dir, self.url, default_name
                )
            }
            (Step::EnterName, RepoAction::Create) => {
                format!("Directory: {}", self.parent_dir)
            }
            _ => String::new(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                ctx,
                Style::default().fg(theme::R_TEXT_SECONDARY),
            )),
            ctx_area,
        );

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
        match (self.step, self.action) {
            (Step::ChooseAction, _) => "Add Repository",
            (Step::SelectDirectory, RepoAction::Create) => {
                "Create \u{2014} Select parent directory"
            }
            (Step::SelectDirectory, RepoAction::Clone) => "Clone \u{2014} Select destination",
            (Step::EnterUrl, _) => "Clone \u{2014} Repository URL",
            (Step::EnterName, RepoAction::Create) => "Create \u{2014} Repository name",
            (Step::EnterName, RepoAction::Clone) => "Clone \u{2014} Directory name",
        }
    }
}

/// Extract a repository name from a clone URL (shared with hub, but duplicated
/// here to avoid an IPC dependency for a pure string helper).
fn repo_name_from_url(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let name = if let Some(colon_part) = s.rsplit_once(':') {
        colon_part.1.rsplit('/').next()
    } else {
        s.rsplit('/').next()
    };
    name.filter(|n| !n.is_empty()).map(|n| n.to_string())
}
