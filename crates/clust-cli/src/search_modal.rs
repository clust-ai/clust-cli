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

use clust_ipc::AgentInfo;

use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub enum SearchResult {
    Pending,
    Cancelled,
    Selected(Box<AgentInfo>),
}

// ---------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------

pub struct SearchModal {
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    agents: Vec<AgentInfo>,
    matcher: SkimMatcherV2,
}

impl SearchModal {
    pub fn new(agents: Vec<AgentInfo>) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            agents,
            matcher: SkimMatcherV2::default(),
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> SearchResult {
        match key.code {
            KeyCode::Esc => SearchResult::Cancelled,
            KeyCode::Enter => {
                let filtered = self.filtered_agents();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    SearchResult::Selected(Box::new(self.agents[idx].clone()))
                } else {
                    SearchResult::Pending
                }
            }
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                SearchResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_agents().len().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                SearchResult::Pending
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
                SearchResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                SearchResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                SearchResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return SearchResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                SearchResult::Pending
            }
            _ => SearchResult::Pending,
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

    // -----------------------------------------------------------------------
    // Fuzzy filtering
    // -----------------------------------------------------------------------

    fn filtered_agents(&self) -> Vec<(usize, i64)> {
        if self.input.is_empty() {
            return self
                .agents
                .iter()
                .enumerate()
                .map(|(i, _)| (i, 0))
                .collect();
        }
        let mut results: Vec<(usize, i64)> = self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(i, agent)| {
                // Match against all searchable fields, take the best score
                let fields: Vec<&str> = [
                    Some(agent.id.as_str()),
                    Some(agent.agent_binary.as_str()),
                    agent.branch_name.as_deref(),
                    agent.repo_path.as_deref(),
                    Some(agent.working_dir.as_str()),
                    Some(agent.hub.as_str()),
                ]
                .into_iter()
                .flatten()
                .collect();

                // Also match against repo name (last path component)
                let repo_name = agent
                    .repo_path
                    .as_deref()
                    .and_then(|p| p.rsplit('/').next());

                let best = fields
                    .iter()
                    .filter_map(|f| self.matcher.fuzzy_match(f, &self.input))
                    .chain(repo_name.and_then(|n| self.matcher.fuzzy_match(n, &self.input)))
                    .max();

                best.map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 70u16.min(area.width.saturating_sub(4));
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

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_ACCENT_DIM))
            .title(Span::styled(
                " Search Agents ",
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [hint_area, input_area, _gap, list_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        // Hint
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Type to filter, Enter to select, Esc to cancel",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )),
            hint_area,
        );

        // Input field
        self.render_input(frame, input_area);

        // Agent list
        self.render_agent_list(frame, list_area);
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

    fn render_agent_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_agents();
        if filtered.is_empty() {
            let msg = if self.input.is_empty() {
                "No running agents"
            } else {
                "No matching agents"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    msg,
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                )),
                area,
            );
            return;
        }

        let max_visible = area.height as usize;
        let scroll = self.compute_scroll(filtered.len(), max_visible);
        let lines: Vec<Line> = filtered
            .iter()
            .skip(scroll)
            .take(max_visible)
            .enumerate()
            .map(|(vis_idx, &(orig_idx, _))| {
                let agent = &self.agents[orig_idx];
                let is_selected = vis_idx + scroll == self.selected_idx;
                self.render_agent_line(agent, is_selected)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_agent_line<'a>(&self, agent: &'a AgentInfo, selected: bool) -> Line<'a> {
        let (prefix, name_style, detail_style) = if selected {
            (
                Span::styled(
                    "  > ",
                    Style::default()
                        .fg(theme::R_ACCENT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            )
        } else {
            (
                Span::styled("    ", Style::default()),
                Style::default().fg(theme::R_TEXT_SECONDARY),
                Style::default().fg(theme::R_TEXT_DISABLED),
            )
        };

        let mut spans = vec![prefix];

        // Agent binary name
        spans.push(Span::styled(agent.agent_binary.as_str(), name_style));

        // Branch name if present
        if let Some(ref branch) = agent.branch_name {
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(
                branch.as_str(),
                if selected {
                    Style::default().fg(theme::R_INFO)
                } else {
                    Style::default().fg(theme::R_TEXT_TERTIARY)
                },
            ));
        }

        // Repo name (last path component)
        if let Some(ref repo_path) = agent.repo_path {
            let repo_name = repo_path.rsplit('/').next().unwrap_or(repo_path);
            spans.push(Span::styled("  ", Style::default()));
            spans.push(Span::styled(repo_name.to_string(), detail_style));
        }

        // Short ID suffix
        let short_id = if agent.id.len() > 8 {
            &agent.id[..8]
        } else {
            &agent.id
        };
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            short_id.to_string(),
            Style::default().fg(theme::R_TEXT_DISABLED),
        ));

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
}
