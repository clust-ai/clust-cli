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

/// Set to `false` to disable terminal output previews in task boxes.
pub const SHOW_TERMINAL_PREVIEW: bool = true;

/// Number of terminal output lines shown in active task preview.
pub const TASK_TERMINAL_PREVIEW_LINES: usize = 4;

/// Height of a task box without terminal preview (separator + header + prompt).
const TASK_BOX_BASE_HEIGHT: u16 = 3;

/// Extra height added for terminal preview in active task boxes.
const TASK_BOX_PREVIEW_HEIGHT: u16 = TASK_TERMINAL_PREVIEW_LINES as u16;

/// Pre-extracted terminal output lines for active tasks, keyed by agent_id.
pub type TerminalPreviewMap = HashMap<String, Vec<Line<'static>>>;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum BatchStatus {
    Idle,
    Active,
    /// Queued for scheduled execution. Contains the RFC 3339 `scheduled_at` timestamp
    /// and the hub-side batch ID.
    Queued { scheduled_at: String, batch_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LaunchMode {
    Auto,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum TaskStatus {
    Idle,
    Active,
    Done,
}

/// A single task within a batch.
pub struct TaskEntry {
    pub branch_name: String,
    pub prompt: String,
    pub status: TaskStatus,
    /// Agent ID linking this task to its AgentPanel in OverviewState (set when started).
    pub agent_id: Option<String>,
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
    pub launch_mode: LaunchMode,
    pub prompt_prefix: Option<String>,
    pub prompt_suffix: Option<String>,
    pub tasks: Vec<TaskEntry>,
    pub status: BatchStatus,
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub created_at: Instant,
}

impl BatchInfo {
    /// Builds the full prompt for a task by combining the batch prefix,
    /// the task-specific prompt, and the batch suffix.
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

/// Info returned when a batch transitions to Active, describing which tasks to start.
pub struct BatchStartInfo {
    pub batch_id: usize,
    pub repo_path: String,
    pub target_branch: String,
    pub tasks_to_start: Vec<(usize, String, String)>, // (task_index, branch_name, prompt)
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
    pub focused_task: Option<usize>,
    pub scroll_offset: usize,
    next_id: usize,
    next_auto_name: usize,
}

impl TasksState {
    pub fn new() -> Self {
        Self {
            batches: Vec::new(),
            focus: TasksFocus::BatchList,
            focused_task: None,
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
            launch_mode: output.launch_mode,
            prompt_prefix: None,
            prompt_suffix: None,
            tasks: Vec::new(),
            status: BatchStatus::Idle,
            plan_mode: false,
            allow_bypass: false,
            created_at: Instant::now(),
        });
        self.next_id += 1;
    }

    pub fn add_task(&mut self, batch_idx: usize, branch_name: String, prompt: String) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.tasks.push(TaskEntry {
                branch_name,
                prompt,
                status: TaskStatus::Idle,
                agent_id: None,
            });
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

    /// Start a single task by index within a manual-mode batch.
    pub fn start_single_task(&mut self, batch_idx: usize, task_idx: usize) -> Option<BatchStartInfo> {
        let batch = self.batches.get(batch_idx)?;
        if batch.launch_mode != LaunchMode::Manual {
            return None;
        }
        let task = batch.tasks.get(task_idx)?;
        if task.status != TaskStatus::Idle {
            return None;
        }
        Some(BatchStartInfo {
            batch_id: batch.id,
            repo_path: batch.repo_path.clone(),
            target_branch: batch.branch_name.clone(),
            tasks_to_start: vec![(task_idx, task.branch_name.clone(), task.prompt.clone())],
        })
    }

    pub fn toggle_plan_mode(&mut self, batch_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.plan_mode = !batch.plan_mode;
        }
    }

    pub fn toggle_allow_bypass(&mut self, batch_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.allow_bypass = !batch.allow_bypass;
        }
    }

    /// Toggle the focused batch status. If transitioning to Active, returns
    /// the info needed to start agents for idle tasks (up to max_concurrent).
    /// Returns None for manual-mode batches (use start_single_task instead).
    pub fn toggle_batch_status(&mut self, batch_idx: usize) -> Option<BatchStartInfo> {
        let batch = self.batches.get_mut(batch_idx)?;
        if batch.launch_mode == LaunchMode::Manual {
            return None;
        }
        match &batch.status {
            BatchStatus::Active => {
                batch.status = BatchStatus::Idle;
                None
            }
            BatchStatus::Queued { .. } => {
                // Cancel queued status — revert to idle (hub cancellation handled by caller)
                batch.status = BatchStatus::Idle;
                None
            }
            BatchStatus::Idle => {
                batch.status = BatchStatus::Active;
                let active_count = batch
                    .tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Active)
                    .count();
                let max = batch.max_concurrent.unwrap_or(usize::MAX);
                let slots = max.saturating_sub(active_count);
                let tasks_to_start: Vec<_> = batch
                    .tasks
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.status == TaskStatus::Idle)
                    .take(slots)
                    .map(|(i, t)| (i, t.branch_name.clone(), t.prompt.clone()))
                    .collect();
                if tasks_to_start.is_empty() {
                    return None;
                }
                Some(BatchStartInfo {
                    batch_id: batch.id,
                    repo_path: batch.repo_path.clone(),
                    target_branch: batch.branch_name.clone(),
                    tasks_to_start,
                })
            }
        }
    }

    /// Find a batch by its unique id.
    pub fn batch_by_id_mut(&mut self, id: usize) -> Option<&mut BatchInfo> {
        self.batches.iter_mut().find(|b| b.id == id)
    }

    /// Mark the task associated with the given agent_id as Done.
    /// If the batch is still Active and has Idle tasks remaining, returns
    /// a `BatchStartInfo` describing the next task(s) to start.
    pub fn mark_agent_done(&mut self, agent_id: &str) -> Option<BatchStartInfo> {
        let batch = self.batches.iter_mut().find(|b| {
            b.tasks.iter().any(|t| t.agent_id.as_deref() == Some(agent_id))
        })?;

        if let Some(task) = batch.tasks.iter_mut().find(|t| t.agent_id.as_deref() == Some(agent_id)) {
            task.status = TaskStatus::Done;
        }

        if !matches!(batch.status, BatchStatus::Active) {
            return None;
        }

        let active_count = batch.tasks.iter().filter(|t| t.status == TaskStatus::Active).count();
        let max = batch.max_concurrent.unwrap_or(usize::MAX);
        let slots = max.saturating_sub(active_count);
        let tasks_to_start: Vec<_> = batch
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status == TaskStatus::Idle)
            .take(slots)
            .map(|(i, t)| (i, t.branch_name.clone(), t.prompt.clone()))
            .collect();

        if tasks_to_start.is_empty() {
            return None;
        }

        Some(BatchStartInfo {
            batch_id: batch.id,
            repo_path: batch.repo_path.clone(),
            target_branch: batch.branch_name.clone(),
            tasks_to_start,
        })
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

    pub fn remove_done_tasks(&mut self, batch_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.tasks.retain(|t| t.status != TaskStatus::Done);
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
            self.focused_task = None;
            if idx > 0 {
                self.focus = TasksFocus::BatchCard(idx - 1);
            } else {
                self.focus = TasksFocus::BatchList;
            }
        }
    }

    pub fn focus_task_down(&mut self) {
        if let TasksFocus::BatchCard(idx) = self.focus {
            let task_count = self.batches.get(idx).map_or(0, |b| b.tasks.len());
            if task_count == 0 {
                return;
            }
            self.focused_task = Some(match self.focused_task {
                None => 0,
                Some(i) if i + 1 < task_count => i + 1,
                Some(i) => i,
            });
        }
    }

    pub fn focus_task_up(&mut self) {
        match self.focused_task {
            Some(0) => self.focused_task = None,
            Some(i) => self.focused_task = Some(i - 1),
            None => {}
        }
    }

    #[allow(dead_code)]
    pub fn clear_focused_task(&mut self) {
        self.focused_task = None;
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
    terminal_previews: &TerminalPreviewMap,
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

        let ft = if is_focused { state.focused_task } else { None };
        render_batch_card(frame, card_areas[i], batch, is_focused, repo_color, ft, terminal_previews);

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
            format!("  {mod_key}+T create batch  Space toggle status  T timer  {mod_key}+S start task  M mode  B bypass  d clear done"),
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
    focused_task: Option<usize>,
    terminal_previews: &TerminalPreviewMap,
) {
    let border_color = match (focused, repo_color) {
        (true, Some(c)) => c,
        (false, Some(c)) => theme::dim_color(c),
        (true, None) => theme::R_ACCENT_BRIGHT,
        (false, None) => theme::R_TEXT_TERTIARY,
    };

    let title_color = match (focused, repo_color) {
        (true, Some(c)) => c,
        (false, Some(c)) => theme::dim_color(c),
        (true, None) => theme::R_ACCENT_BRIGHT,
        (false, None) => theme::R_ACCENT,
    };
    let title_style = if focused {
        Style::default()
            .fg(title_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(title_color)
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

    let status_span = if batch.launch_mode == LaunchMode::Manual {
        Span::styled("Manual", Style::default().fg(theme::R_INFO))
    } else {
        match &batch.status {
            BatchStatus::Idle => {
                Span::styled("Idle", Style::default().fg(theme::R_TEXT_DISABLED))
            }
            BatchStatus::Active => Span::styled(
                "Active",
                Style::default()
                    .fg(theme::R_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            BatchStatus::Queued { scheduled_at, .. } => {
                let countdown = crate::timer_modal::format_countdown(scheduled_at);
                Span::styled(
                    format!("Queued {}", countdown),
                    Style::default()
                        .fg(theme::R_INFO)
                        .add_modifier(Modifier::BOLD),
                )
            }
        }
    };

    let metadata_lines = vec![
        Line::from(vec![
            Span::styled("Repo      ", label_style),
            Span::styled(&batch.repo_name, Style::default().fg(repo_color_val)),
        ]),
        Line::from(vec![
            Span::styled("Branch    ", label_style),
            Span::styled(&batch.branch_name, value_style),
        ]),
        if batch.launch_mode == LaunchMode::Auto {
            Line::from(vec![
                Span::styled("Workers   ", label_style),
                Span::styled(concurrency_text, value_style),
            ])
        } else {
            Line::from(vec![
                Span::styled("Mode      ", label_style),
                Span::styled("Manual", Style::default().fg(theme::R_INFO)),
            ])
        },
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
            Span::styled("Mode      ", label_style),
            if batch.plan_mode {
                Span::styled(
                    "Plan",
                    Style::default()
                        .fg(theme::R_WARNING)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("Normal", Style::default().fg(theme::R_TEXT_DISABLED))
            },
        ]),
        Line::from(vec![
            Span::styled("Bypass    ", label_style),
            if batch.allow_bypass {
                Span::styled(
                    "Allowed",
                    Style::default()
                        .fg(theme::R_WARNING)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("Off", Style::default().fg(theme::R_TEXT_DISABLED))
            },
        ]),
        Line::from(vec![
            Span::styled("Status    ", label_style),
            status_span,
        ]),
    ];

    // Split inner area: metadata on top, task boxes below
    let metadata_height = metadata_lines.len() as u16 + 1; // +1 for blank separator
    let [metadata_area, tasks_area] = Layout::vertical([
        Constraint::Length(metadata_height),
        Constraint::Min(0),
    ])
    .areas(inner);

    frame.render_widget(Paragraph::new(metadata_lines), metadata_area);

    if !batch.tasks.is_empty() {
        render_task_boxes(frame, tasks_area, batch, focused_task, terminal_previews);
    }
}

// ---------------------------------------------------------------------------
// Task box rendering
// ---------------------------------------------------------------------------

/// Height of a single task box based on its status and available preview data.
fn task_box_height(task: &TaskEntry, terminal_previews: &TerminalPreviewMap) -> u16 {
    if task.status == TaskStatus::Active && SHOW_TERMINAL_PREVIEW {
        let has_preview = task
            .agent_id
            .as_ref()
            .and_then(|id| terminal_previews.get(id))
            .is_some_and(|lines| !lines.is_empty());
        if has_preview {
            return TASK_BOX_BASE_HEIGHT + TASK_BOX_PREVIEW_HEIGHT;
        }
    }
    TASK_BOX_BASE_HEIGHT
}

/// Render task boxes vertically within the given area, sorted by status.
fn render_task_boxes(
    frame: &mut Frame,
    area: Rect,
    batch: &BatchInfo,
    focused_task: Option<usize>,
    terminal_previews: &TerminalPreviewMap,
) {
    if area.height < 2 || batch.tasks.is_empty() {
        return;
    }

    // Sort: Active first, then Idle, then Done
    let mut sorted_indices: Vec<usize> = (0..batch.tasks.len()).collect();
    sorted_indices.sort_by_key(|&i| match batch.tasks[i].status {
        TaskStatus::Active => 0,
        TaskStatus::Idle => 1,
        TaskStatus::Done => 2,
    });

    // Calculate how many task boxes fit in the available height
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut total_height: u16 = 0;
    let mut visible_count = 0;

    for &idx in &sorted_indices {
        let task = &batch.tasks[idx];
        let h = task_box_height(task, terminal_previews);
        if total_height + h > area.height {
            break;
        }
        constraints.push(Constraint::Length(h));
        total_height += h;
        visible_count += 1;
    }

    if visible_count == 0 {
        return;
    }

    // Flexible spacer pushes boxes to top
    if total_height < area.height {
        constraints.push(Constraint::Min(0));
    }

    let box_areas = Layout::vertical(constraints).split(area);

    for (vi, &idx) in sorted_indices.iter().take(visible_count).enumerate() {
        let task = &batch.tasks[idx];
        let is_focused = focused_task == Some(idx);
        render_single_task_box(frame, box_areas[vi], task, idx, is_focused, terminal_previews);
    }
}

/// Render a single task as a box with separator, header, prompt, and optional terminal preview.
fn render_single_task_box(
    frame: &mut Frame,
    area: Rect,
    task: &TaskEntry,
    original_index: usize,
    is_focused: bool,
    terminal_previews: &TerminalPreviewMap,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let label_style = Style::default().fg(theme::R_TEXT_TERTIARY);
    let value_style = Style::default().fg(theme::R_TEXT_SECONDARY);

    // Status styling
    let (status_text, status_style) = match task.status {
        TaskStatus::Idle => ("Idle", Style::default().fg(theme::R_TEXT_DISABLED)),
        TaskStatus::Active => (
            "Active",
            Style::default()
                .fg(theme::R_SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        TaskStatus::Done => ("Done", Style::default().fg(theme::R_WARNING)),
    };

    // Separator color based on status (accent when focused)
    let sep_color = if is_focused {
        theme::R_ACCENT_BRIGHT
    } else {
        match task.status {
            TaskStatus::Active => theme::R_SUCCESS,
            TaskStatus::Idle => theme::R_TEXT_DISABLED,
            TaskStatus::Done => theme::R_TEXT_TERTIARY,
        }
    };

    // Top separator line
    let separator = Line::from(Span::styled(
        "\u{2500}".repeat(area.width as usize),
        Style::default().fg(sep_color),
    ));
    frame.render_widget(
        Paragraph::new(separator),
        Rect {
            height: 1,
            ..area
        },
    );

    let content_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    };

    if content_area.height < 1 || content_area.width < 4 {
        return;
    }

    // Line 1: focus indicator + task number + status + branch name
    let indicator = if is_focused { "> " } else { "  " };
    let indicator_style = if is_focused {
        Style::default()
            .fg(theme::R_ACCENT_BRIGHT)
            .add_modifier(Modifier::BOLD)
    } else {
        label_style
    };
    let branch_style = if is_focused {
        Style::default()
            .fg(theme::R_TEXT_PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else {
        value_style
    };

    let max_branch = (content_area.width as usize).saturating_sub(14);
    let branch_display = if task.branch_name.len() > max_branch {
        format!(
            "{}\u{2026}",
            &task.branch_name[..max_branch.saturating_sub(1)]
        )
    } else {
        task.branch_name.clone()
    };

    let header_line = Line::from(vec![
        Span::styled(indicator, indicator_style),
        Span::styled(format!("{}.", original_index + 1), label_style),
        Span::raw(" "),
        Span::styled(status_text, status_style),
        Span::raw(" "),
        Span::styled(branch_display, branch_style),
    ]);

    let mut lines = vec![header_line];

    // Line 2: truncated prompt
    let max_prompt = content_area.width as usize;
    let prompt_first_line = task.prompt.lines().next().unwrap_or("");
    let prompt_display = if prompt_first_line.len() > max_prompt {
        format!(
            "{}\u{2026}",
            &prompt_first_line[..max_prompt.saturating_sub(1)]
        )
    } else {
        prompt_first_line.to_string()
    };
    lines.push(Line::from(Span::styled(
        prompt_display,
        Style::default().fg(theme::R_TEXT_TERTIARY),
    )));

    // Terminal preview for active tasks
    if task.status == TaskStatus::Active && SHOW_TERMINAL_PREVIEW {
        if let Some(preview_lines) = task
            .agent_id
            .as_ref()
            .and_then(|id| terminal_previews.get(id))
        {
            if !preview_lines.is_empty() {
                for line in preview_lines {
                    lines.push(line.clone());
                }
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), content_area);
}
