use std::collections::HashMap;
use std::time::Instant;

use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};

use crate::create_batch_modal::BatchModalOutput;
use crate::theme;
use crate::ui::ClickMap;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MIN_CARD_WIDTH: u16 = 40;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single task within a batch.
#[allow(dead_code)]
pub struct TaskEntry {
    pub branch_name: String,
    pub prompt: String,
}

/// A single batch definition (UI-only, no execution).
#[allow(dead_code)]
pub struct BatchInfo {
    pub id: usize,
    pub title: String,
    pub repo_path: String,
    pub repo_name: String,
    pub branch_name: String,
    pub max_concurrent: Option<usize>,
    pub prompt_prefix: Option<String>,
    pub prompt_suffix: Option<String>,
    pub tasks: Vec<TaskEntry>,
    pub created_at: Instant,
}

impl BatchInfo {
    /// Builds the full prompt for a task by combining the batch prefix,
    /// the task-specific prompt, and the batch suffix.
    #[allow(dead_code)]
    pub fn build_prompt(&self, task_prompt: &str) -> String {
        let mut parts = Vec::new();
        if let Some(ref prefix) = self.prompt_prefix {
            parts.push(prefix.as_str());
        }
        parts.push(task_prompt);
        if let Some(ref suffix) = self.prompt_suffix {
            parts.push(suffix.as_str());
        }
        parts.join("\n\n")
    }
}

/// Focus state within the Tasks tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TasksFocus {
    BatchList,
    BatchCard(usize),
}

/// Top-level Tasks tab state.
pub struct TasksState {
    pub batches: Vec<BatchInfo>,
    pub focus: TasksFocus,
    pub scroll_offset: usize,
    next_id: usize,
    next_auto_name: usize,
}

impl TasksState {
    pub fn new() -> Self {
        Self {
            batches: Vec::new(),
            focus: TasksFocus::BatchList,
            scroll_offset: 0,
            next_id: 1,
            next_auto_name: 1,
        }
    }

    pub fn add_batch(&mut self, output: BatchModalOutput) {
        let title = output.title.unwrap_or_else(|| {
            let name = format!("Batch {}", self.next_auto_name);
            self.next_auto_name += 1;
            name
        });
        self.batches.push(BatchInfo {
            id: self.next_id,
            title,
            repo_path: output.repo_path,
            repo_name: output.repo_name,
            branch_name: output.branch_name,
            max_concurrent: output.max_concurrent,
            prompt_prefix: None,
            prompt_suffix: None,
            tasks: Vec::new(),
            created_at: Instant::now(),
        });
        self.next_id += 1;
    }

    pub fn add_task(&mut self, batch_idx: usize, branch_name: String, prompt: String) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.tasks.push(TaskEntry { branch_name, prompt });
        }
    }

    pub fn set_prompt_prefix(&mut self, batch_idx: usize, value: String) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.prompt_prefix = if value.is_empty() { None } else { Some(value) };
        }
    }

    pub fn set_prompt_suffix(&mut self, batch_idx: usize, value: String) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.prompt_suffix = if value.is_empty() { None } else { Some(value) };
        }
    }

    pub fn remove_batch(&mut self, idx: usize) {
        if idx < self.batches.len() {
            self.batches.remove(idx);
            // Fix focus if it points beyond the list
            if let TasksFocus::BatchCard(i) = self.focus {
                if self.batches.is_empty() {
                    self.focus = TasksFocus::BatchList;
                } else if i >= self.batches.len() {
                    self.focus = TasksFocus::BatchCard(self.batches.len() - 1);
                }
            }
        }
    }

    pub fn visible_batch_count(&self, width: u16) -> usize {
        if width == 0 || self.batches.is_empty() {
            return 0;
        }
        (width / MIN_CARD_WIDTH).max(1) as usize
    }

    pub fn scroll_left(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    pub fn scroll_right(&mut self, width: u16) {
        let visible = self.visible_batch_count(width);
        if visible > 0 && self.scroll_offset + visible < self.batches.len() {
            self.scroll_offset += 1;
        }
    }

    pub fn focus_first_card(&mut self) {
        if !self.batches.is_empty() {
            self.focus = TasksFocus::BatchCard(self.scroll_offset);
        }
    }

    pub fn focus_next_card(&mut self) {
        if let TasksFocus::BatchCard(idx) = self.focus {
            if idx + 1 < self.batches.len() {
                self.focus = TasksFocus::BatchCard(idx + 1);
            }
        }
    }

    pub fn focus_prev_card(&mut self) {
        if let TasksFocus::BatchCard(idx) = self.focus {
            if idx > 0 {
                self.focus = TasksFocus::BatchCard(idx - 1);
            } else {
                self.focus = TasksFocus::BatchList;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_tasks(
    frame: &mut Frame,
    area: Rect,
    state: &mut TasksState,
    click_map: &mut ClickMap,
    repo_colors: &HashMap<String, String>,
) {
    // Split into options bar (1 row) + cards area
    let [options_area, cards_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    render_options_bar(frame, options_area, state);

    if state.batches.is_empty() {
        render_empty_state(frame, cards_area);
        return;
    }

    let visible_count = state.visible_batch_count(cards_area.width);
    if visible_count == 0 {
        return;
    }

    // Clamp scroll
    if state.scroll_offset + visible_count > state.batches.len() {
        state.scroll_offset = state.batches.len().saturating_sub(visible_count);
    }

    let end = (state.scroll_offset + visible_count).min(state.batches.len());
    let actual_visible = end - state.scroll_offset;

    // Distribute cards horizontally (at least 2 slots so a single card doesn't fill the screen)
    let slots = (actual_visible as u32).max(2);
    let constraints: Vec<Constraint> = (0..actual_visible)
        .map(|_| Constraint::Ratio(1, slots))
        .collect();
    let card_areas = Layout::horizontal(constraints).split(cards_area);

    for (i, batch_idx) in (state.scroll_offset..end).enumerate() {
        let batch = &state.batches[batch_idx];
        let is_focused = matches!(state.focus, TasksFocus::BatchCard(idx) if idx == batch_idx);

        let repo_color = repo_colors
            .get(batch.repo_path.as_str())
            .map(|c| theme::repo_color(c));

        render_batch_card(frame, card_areas[i], batch, is_focused, repo_color);

        click_map.tasks_batch_cards.push((card_areas[i], batch_idx));
    }
}

fn render_options_bar(frame: &mut Frame, area: Rect, state: &TasksState) {
    let count = state.batches.len();
    let count_text = if count == 0 {
        "No batches".to_string()
    } else if count == 1 {
        "1 batch".to_string()
    } else {
        format!("{} batches", count)
    };

    let mod_key = if cfg!(target_os = "macos") { "Opt" } else { "Alt" };

    let line = Line::from(vec![
        Span::styled(" ", Style::default().bg(theme::R_BG_RAISED)),
        Span::styled(
            count_text,
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            format!("  {mod_key}+T create batch"),
            Style::default()
                .fg(theme::R_TEXT_TERTIARY)
                .bg(theme::R_BG_RAISED),
        ),
    ]);

    // Fill remaining width
    let content_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let remaining = (area.width as usize).saturating_sub(content_width);
    let mut spans: Vec<Span> = line.spans.into_iter().collect();
    if remaining > 0 {
        spans.push(Span::styled(
            " ".repeat(remaining),
            Style::default().bg(theme::R_BG_RAISED),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_empty_state(frame: &mut Frame, area: Rect) {
    let mod_key = if cfg!(target_os = "macos") { "Opt" } else { "Alt" };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("No batches defined \u{2014} press {mod_key}+T to create one"),
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )))
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme::R_BG_BASE)),
        area,
    );
}

fn render_batch_card(
    frame: &mut Frame,
    area: Rect,
    batch: &BatchInfo,
    focused: bool,
    repo_color: Option<ratatui::style::Color>,
) {
    let border_color = match (focused, repo_color) {
        (true, Some(c)) => c,
        (false, Some(c)) => theme::dim_color(c),
        (true, None) => theme::R_ACCENT_BRIGHT,
        (false, None) => theme::R_TEXT_TERTIARY,
    };

    let title_style = if focused {
        Style::default()
            .fg(theme::R_ACCENT_BRIGHT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::R_ACCENT)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(format!(" {} ", batch.title), title_style))
        .style(Style::default().bg(if focused {
            theme::R_BG_SURFACE
        } else {
            theme::R_BG_BASE
        }))
        .padding(Padding::new(1, 1, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let repo_color_val = repo_color.unwrap_or(theme::R_TEXT_SECONDARY);
    let concurrency_text = match batch.max_concurrent {
        Some(v) => v.to_string(),
        None => "\u{221E}".to_string(),
    };

    let label_style = Style::default().fg(theme::R_TEXT_TERTIARY);
    let value_style = Style::default().fg(theme::R_TEXT_SECONDARY);

    let lines = vec![
        Line::from(vec![
            Span::styled("Repo      ", label_style),
            Span::styled(&batch.repo_name, Style::default().fg(repo_color_val)),
        ]),
        Line::from(vec![
            Span::styled("Branch    ", label_style),
            Span::styled(&batch.branch_name, value_style),
        ]),
        Line::from(vec![
            Span::styled("Workers   ", label_style),
            Span::styled(concurrency_text, value_style),
        ]),
        Line::from(vec![
            Span::styled("Tasks     ", label_style),
            Span::styled(batch.tasks.len().to_string(), value_style),
        ]),
        Line::from(vec![
            Span::styled("Prefix    ", label_style),
            Span::styled(
                batch.prompt_prefix.as_deref().unwrap_or("(none)"),
                if batch.prompt_prefix.is_some() { value_style } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("Suffix    ", label_style),
            Span::styled(
                batch.prompt_suffix.as_deref().unwrap_or("(none)"),
                if batch.prompt_suffix.is_some() { value_style } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("Status    ", label_style),
            Span::styled("Not started", Style::default().fg(theme::R_TEXT_DISABLED)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}
