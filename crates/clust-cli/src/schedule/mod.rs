//! Schedule tab — UI state, rendering, and key handling for the per-task
//! scheduler.
//!
//! Layout mirrors the Overview tab: a horizontal grid of equal-width columns,
//! one per task, with `Shift+Left/Right` to focus a task and `Shift+Down` to
//! enter focus mode (Active tasks only). Each column's rendering varies by
//! task status:
//!
//! - **Inactive** — header pill + schedule info + plan/auto-exit pills, then
//!   the prompt body (wrapping; scrollable on Y).
//! - **Active** — embedded `TerminalEmulator` filling the entire inner area
//!   of the column. We open our own attachment so the schedule view stays
//!   live regardless of what Overview is doing. When the panel is focused,
//!   typing is forwarded straight to the agent's PTY (mirrors Overview's
//!   terminal-focus key flow).
//! - **Complete** — compact: branch name + small `✓`.
//! - **Aborted** — header pill + prompt + restart hint.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use clust_ipc::{RepoInfo, ScheduleKind, ScheduledTaskInfo, ScheduledTaskStatus};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};
use tokio::sync::mpsc;

use crate::overview::{
    agent_connection_task, AgentOutputEvent, AgentPanel, AgentTerminalCache, PanelCommand,
};
use crate::terminal_emulator::TerminalEmulator;
use crate::theme;
use crate::ui::{ClickMap, ScheduleHintKey};

/// Minimum width of a single task column. Below this, columns scroll horizontally.
const MIN_PANEL_WIDTH: u16 = 60;

fn max_panels_for_width(available_width: u16) -> usize {
    (available_width / MIN_PANEL_WIDTH).max(1) as usize
}

// ---------------------------------------------------------------------------
// Outcome of key/mouse handling — communicated up to the main loop so it can
// trigger IPC, open modals, or update status messages.
// ---------------------------------------------------------------------------

/// Side-effect requested by the Schedule tab as a result of a key press.
///
/// The Schedule UI never reaches into the IPC layer or modal stack directly;
/// instead it returns one of these and the main loop dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleAction {
    /// Nothing to do.
    Noop,
    /// Open the inline edit-prompt modal pre-populated with this task's prompt.
    EditPrompt { task_id: String, current: String },
    /// Toggle plan_mode for the focused Inactive/Aborted task.
    TogglePlanMode { task_id: String, new_value: bool },
    /// Toggle auto_exit for the focused Inactive/Aborted task.
    ToggleAutoExit { task_id: String, new_value: bool },
    /// Manually start an Inactive task right now.
    StartNow { task_id: String },
    /// Restart an Aborted task.
    Restart { task_id: String, clean: bool },
    /// Show a confirmation menu before deleting this single task.
    ConfirmDelete {
        task_id: String,
        branch_name: String,
    },
    /// Show the "clear by status" sub-menu (Complete / Aborted / both).
    OpenClearMenu,
    /// Enter focus mode on the currently-focused Active task.
    EnterFocusMode { task_id: String },
    /// Open the reschedule modal pre-populated from this task. The modal
    /// keeps the repo, branch, prompt and plan/auto-exit flags untouched —
    /// only the schedule kind (and start_at / dep set) can be overwritten.
    OpenReschedule { task_id: String },
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScheduleFocus {
    /// The top filter bar is focused — Left/Right cycles repo chips,
    /// Enter/Space toggles a chip's collapse state, Shift+Down moves into
    /// a task. This is the default state when no tasks are visible.
    OptionsBar,
    /// A task at the given index in `tasks` is focused for keybinds.
    Task(usize),
}

/// Per-task scrollback / prompt scroll bookkeeping that survives task list
/// refreshes (re-keyed by task id, not panel index).
#[derive(Default)]
struct TaskUiState {
    prompt_scroll: u16,
}

pub struct ScheduleState {
    pub tasks: Vec<ScheduledTaskInfo>,
    pub focus: ScheduleFocus,
    pub scroll_offset: usize,
    /// Repo paths that are currently filtered out of the panel grid. Mirrors
    /// the Overview tab's filter model — collapsed repos still show their
    /// chip in the bar (dimmed) but their tasks disappear from the grid.
    pub collapsed_repos: HashSet<String>,
    /// Cursor position within the bar's repo chips when the bar is focused.
    /// Indexes into the ordered list of repos that have at least one task.
    pub filter_cursor: usize,
    /// Indices into `self.tasks` for tasks whose repo is not collapsed.
    /// Recomputed whenever the task list or `collapsed_repos` changes; used
    /// by both navigation (focus_prev/next walk this list) and rendering.
    pub visible_task_indices: Vec<usize>,
    /// Live PTY connections for tasks in the Active state. Keyed by task id so
    /// we don't lose them when the underlying agent_id list re-orders.
    panels: HashMap<String, AgentPanel>,
    /// Per-task UI bookkeeping (scroll positions etc.) preserved across list
    /// refreshes.
    ui: HashMap<String, TaskUiState>,
    /// Channel that all background `agent_connection_task` futures send into.
    output_tx: mpsc::Sender<AgentOutputEvent>,
    output_rx: mpsc::Receiver<AgentOutputEvent>,
    /// Last computed panel dimensions (cached so resize-on-attach is cheap).
    panel_cols: u16,
    panel_rows: u16,
    viewport_width: u16,
}

impl Default for ScheduleState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduleState {
    pub fn new() -> Self {
        let (output_tx, output_rx) = mpsc::channel(512);
        Self {
            tasks: Vec::new(),
            focus: ScheduleFocus::OptionsBar,
            scroll_offset: 0,
            collapsed_repos: HashSet::new(),
            filter_cursor: 0,
            visible_task_indices: Vec::new(),
            panels: HashMap::new(),
            ui: HashMap::new(),
            output_tx,
            output_rx,
            panel_cols: 80,
            panel_rows: 24,
            viewport_width: 0,
        }
    }

    /// Replace the task list with a fresh snapshot from the hub. Spawns
    /// connections for newly-active tasks and aborts ones that left Active.
    /// Panels for tasks that disappeared entirely are dropped. Tasks are
    /// sorted by repo order (matching the Overview panel) so navigation walks
    /// through them in repo-grouped order.
    pub fn sync_tasks(&mut self, tasks: Vec<ScheduledTaskInfo>, repos: &[RepoInfo]) {
        // Remember the focused task by id so we can restore focus after
        // sorting (otherwise a re-sync that reshuffles indices would steal
        // focus to the task that happens to land at the same numeric index).
        let prev_focus_id = match self.focus {
            ScheduleFocus::Task(idx) => self.tasks.get(idx).map(|t| t.id.clone()),
            _ => None,
        };

        // 1. Drop panels for tasks that no longer exist OR are no longer Active.
        let alive_active_ids: HashSet<String> = tasks
            .iter()
            .filter(|t| t.status == ScheduledTaskStatus::Active)
            .map(|t| t.id.clone())
            .collect();
        let to_drop: Vec<String> = self
            .panels
            .keys()
            .filter(|id| !alive_active_ids.contains(*id))
            .cloned()
            .collect();
        for id in to_drop {
            if let Some(panel) = self.panels.remove(&id) {
                panel.task_handle.abort();
            }
        }

        // 2. Drop UI bookkeeping for tasks that disappeared entirely.
        let alive_ids: HashSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
        self.ui.retain(|id, _| alive_ids.contains(id));

        // 3. Spawn connections for newly-active tasks.
        for task in &tasks {
            if task.status == ScheduledTaskStatus::Active && !self.panels.contains_key(&task.id) {
                if let Some(agent_id) = &task.agent_id {
                    self.spawn_connection(&task.id, agent_id, &task.agent_binary, task);
                }
            }
        }

        self.tasks = tasks;

        // 4. Sort by repo order, then by created_at, then by id — same
        //    ordering Overview uses for its sorted_indices.
        let repo_order: HashMap<&str, usize> = repos
            .iter()
            .enumerate()
            .map(|(i, r)| (r.path.as_str(), i))
            .collect();
        self.tasks.sort_by(|a, b| {
            let oa = repo_order
                .get(a.repo_path.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            let ob = repo_order
                .get(b.repo_path.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            oa.cmp(&ob)
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });

        // 5. Restore focus on the same task id when possible.
        let had_prev_focus = prev_focus_id.is_some();
        if let Some(id) = prev_focus_id {
            if let Some(new_idx) = self.tasks.iter().position(|t| t.id == id) {
                self.focus = ScheduleFocus::Task(new_idx);
            }
        }

        // 6. Recompute visible indices (used by focus clamping below) before
        //    clamping focus — collapsed repos may have shrunk the visible set.
        self.recompute_visible_task_indices();

        // 7. Clamp focus to a valid index. The first time tasks appear we
        //    auto-focus the first visible task so the keybind hints become
        //    immediately useful; if no tasks are visible (all collapsed or
        //    list is empty) we fall back to the bar.
        match self.focus {
            ScheduleFocus::OptionsBar
                if !self.visible_task_indices.is_empty() && !had_prev_focus =>
            {
                let first = self.visible_task_indices[0];
                self.focus = ScheduleFocus::Task(first);
            }
            ScheduleFocus::Task(idx) if idx >= self.tasks.len() => {
                self.focus = self
                    .visible_task_indices
                    .last()
                    .copied()
                    .map(ScheduleFocus::Task)
                    .unwrap_or(ScheduleFocus::OptionsBar);
            }
            _ => {}
        }
        if self.tasks.is_empty() {
            self.focus = ScheduleFocus::OptionsBar;
        }

        // 8. Clamp filter_cursor to the new used-repos count.
        let used_count = self.used_repo_paths().len();
        if used_count == 0 {
            self.filter_cursor = 0;
        } else if self.filter_cursor >= used_count {
            self.filter_cursor = used_count - 1;
        }

        // 9. Clamp scroll.
        if self.scroll_offset >= self.tasks.len() {
            self.scroll_offset = self.tasks.len().saturating_sub(1);
        }
    }

    /// Ordered list of repo paths that have at least one task. Order matches
    /// `self.tasks` (which is sorted by `RepoInfo` order in `sync_tasks`),
    /// so this is the canonical bar-cursor traversal order.
    pub fn used_repo_paths(&self) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for task in &self.tasks {
            if seen.insert(task.repo_path.clone()) {
                result.push(task.repo_path.clone());
            }
        }
        result
    }

    /// Refresh `visible_task_indices` from `self.tasks` and `collapsed_repos`.
    /// Cheap enough to call on every render and after every state mutation
    /// that could change visibility.
    fn recompute_visible_task_indices(&mut self) {
        self.visible_task_indices = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| !self.collapsed_repos.contains(t.repo_path.as_str()))
            .map(|(i, _)| i)
            .collect();
    }

    /// Open an attached IPC connection for a task that just transitioned to
    /// Active. The background task forwards PTY output to `self.output_tx`,
    /// keyed by `agent_id`. We map back to `task_id` in `drain_output_events`.
    fn spawn_connection(
        &mut self,
        task_id: &str,
        agent_id: &str,
        agent_binary: &str,
        task: &ScheduledTaskInfo,
    ) {
        let cols = self.panel_cols;
        let rows = self.panel_rows;
        let event_tx = self.output_tx.clone();
        let (command_tx, command_rx) = mpsc::channel::<PanelCommand>(64);
        let aid = agent_id.to_string();
        let handle = tokio::task::spawn(async move {
            agent_connection_task(aid, cols, rows, event_tx, command_rx).await
        });
        self.panels.insert(
            task_id.to_string(),
            AgentPanel {
                id: agent_id.to_string(),
                agent_binary: agent_binary.to_string(),
                branch_name: Some(task.branch_name.clone()),
                repo_path: Some(task.repo_path.clone()),
                is_worktree: true,
                started_at: task.created_at.clone(),
                auto_exit: task.auto_exit,
                vterm: TerminalEmulator::new(cols as usize, rows as usize),
                command_tx,
                exited: false,
                worktree_cleanup_shown: false,
                panel_scroll_offset: 0,
                task_handle: handle,
            },
        );
    }

    /// Drain pending PTY output events into the matching task's vterm.
    pub fn drain_output_events(&mut self) {
        while let Ok(event) = self.output_rx.try_recv() {
            match event {
                AgentOutputEvent::Output { id, data } => {
                    // Find the task whose agent_id == id, then index into panels.
                    if let Some(task_id) = self.task_id_for_agent(&id) {
                        if let Some(panel) = self.panels.get_mut(&task_id) {
                            panel.vterm.process(&data);
                        }
                    }
                }
                AgentOutputEvent::Exited { id, .. } | AgentOutputEvent::ConnectionLost { id } => {
                    if let Some(task_id) = self.task_id_for_agent(&id) {
                        if let Some(panel) = self.panels.get_mut(&task_id) {
                            panel.exited = true;
                        }
                    }
                }
            }
        }
    }

    fn task_id_for_agent(&self, agent_id: &str) -> Option<String> {
        self.tasks
            .iter()
            .find(|t| t.agent_id.as_deref() == Some(agent_id))
            .map(|t| t.id.clone())
    }

    /// Resize all live vterms after the panel grid recomputes.
    pub fn resize_panels_to(&mut self, content_area: Rect) {
        let visible = self.tasks.len().max(2);
        let count_fit = max_panels_for_width(content_area.width).max(1);
        let slots = count_fit.max(2);
        let panel_w = (content_area.width / slots as u16).max(MIN_PANEL_WIDTH);
        // Active panels hand the entire inner area to the terminal (no inner
        // header / hint rows), so the usable rows = panel_h − 2 borders.
        // panel_cols/rows are only used to spawn fresh PTYs for newly-Active
        // tasks; the per-frame `render_active_body` re-resizes to the actual
        // inner rect.
        let panel_h = content_area.height;
        let inner_cols = panel_w.saturating_sub(2);
        let inner_rows = panel_h.saturating_sub(2);
        self.panel_cols = inner_cols.max(20);
        self.panel_rows = inner_rows.max(8);
        self.viewport_width = content_area.width;
        for panel in self.panels.values_mut() {
            let cols = self.panel_cols;
            let rows = self.panel_rows;
            if (panel.vterm.cols() != cols as usize || panel.vterm.rows() != rows as usize)
                && panel
                    .command_tx
                    .try_send(PanelCommand::Resize { cols, rows })
                    .is_ok()
            {
                panel.vterm.resize(cols as usize, rows as usize);
            }
        }
        let _ = visible;
    }

    // -- Navigation --

    /// Set focus to the task at `idx` and scroll it into view. No-op if the
    /// index is out of range. Used by the mouse handler when a panel or
    /// branch indicator is clicked. If the task's repo is collapsed, the
    /// repo is un-collapsed first so the panel becomes visible.
    pub fn focus_task_index(&mut self, idx: usize) {
        if let Some(task) = self.tasks.get(idx) {
            if self.collapsed_repos.contains(task.repo_path.as_str()) {
                let path = task.repo_path.clone();
                self.collapsed_repos.remove(&path);
                self.recompute_visible_task_indices();
            }
            self.focus = ScheduleFocus::Task(idx);
            self.ensure_visible(idx);
        }
    }

    /// Adjust the prompt scroll on the task at `idx` by `delta` lines (positive
    /// scrolls down, negative scrolls up). Does not change focus, so the user
    /// can scroll a panel's prompt without losing their selected task.
    pub fn scroll_prompt_at(&mut self, idx: usize, delta: i32) {
        if let Some(task) = self.tasks.get(idx) {
            let entry = self.ui.entry(task.id.clone()).or_default();
            if delta < 0 {
                entry.prompt_scroll = entry
                    .prompt_scroll
                    .saturating_sub(delta.unsigned_abs() as u16);
            } else {
                entry.prompt_scroll = entry.prompt_scroll.saturating_add(delta as u16);
            }
        }
    }

    pub fn focus_prev(&mut self) {
        if let ScheduleFocus::Task(idx) = self.focus {
            if let Some(pos) = self.visible_task_indices.iter().position(|&i| i == idx) {
                let len = self.visible_task_indices.len();
                let new_pos = if pos == 0 { len - 1 } else { pos - 1 };
                let new_idx = self.visible_task_indices[new_pos];
                self.focus = ScheduleFocus::Task(new_idx);
                self.ensure_visible(new_idx);
            }
        }
    }

    pub fn focus_next(&mut self) {
        if let ScheduleFocus::Task(idx) = self.focus {
            if let Some(pos) = self.visible_task_indices.iter().position(|&i| i == idx) {
                let len = self.visible_task_indices.len();
                let new_pos = (pos + 1) % len;
                let new_idx = self.visible_task_indices[new_pos];
                self.focus = ScheduleFocus::Task(new_idx);
                self.ensure_visible(new_idx);
            }
        }
    }

    /// Move focus from the bar onto the first visible task (or do nothing if
    /// there are no visible tasks). Used by `Shift+Down` from the bar.
    pub fn enter_task_focus(&mut self) {
        if let Some(&first) = self.visible_task_indices.first() {
            self.focus = ScheduleFocus::Task(first);
            self.ensure_visible(first);
        }
    }

    /// Return focus to the bar from a task. Used by `Shift+Up` from a task.
    pub fn exit_task_focus(&mut self) {
        self.focus = ScheduleFocus::OptionsBar;
    }

    /// Cycle the filter cursor one position to the left (saturating at 0).
    pub fn filter_cursor_left(&mut self) {
        if self.filter_cursor > 0 {
            self.filter_cursor -= 1;
        }
    }

    /// Cycle the filter cursor one position to the right (saturating at the
    /// last used repo).
    pub fn filter_cursor_right(&mut self) {
        let count = self.used_repo_paths().len();
        if count > 0 && self.filter_cursor + 1 < count {
            self.filter_cursor += 1;
        }
    }

    /// Toggle the collapsed state of the repo currently under the filter
    /// cursor. If the focused task ends up in a now-collapsed repo, jump
    /// focus to the next visible task (or back to the bar if none remain).
    pub fn toggle_filter_at_cursor(&mut self) {
        let used = self.used_repo_paths();
        let Some(repo_path) = used.get(self.filter_cursor).cloned() else {
            return;
        };
        if self.collapsed_repos.contains(&repo_path) {
            self.collapsed_repos.remove(&repo_path);
        } else {
            self.collapsed_repos.insert(repo_path);
        }
        self.recompute_visible_task_indices();
        if let ScheduleFocus::Task(idx) = self.focus {
            if !self.visible_task_indices.contains(&idx) {
                self.focus = self
                    .visible_task_indices
                    .first()
                    .copied()
                    .map(|i| {
                        self.ensure_visible(i);
                        ScheduleFocus::Task(i)
                    })
                    .unwrap_or(ScheduleFocus::OptionsBar);
            }
        }
    }

    /// Toggle collapse for the given repo path (used by mouse clicks on a
    /// chip in the bar). Mirrors `toggle_filter_at_cursor` but addressed by
    /// path rather than cursor index.
    pub fn toggle_filter_for_repo(&mut self, repo_path: &str) {
        if self.collapsed_repos.contains(repo_path) {
            self.collapsed_repos.remove(repo_path);
        } else {
            self.collapsed_repos.insert(repo_path.to_string());
        }
        // Move filter_cursor onto the clicked chip so subsequent keyboard
        // toggles operate on the chip the user just touched.
        if let Some(pos) = self.used_repo_paths().iter().position(|p| p == repo_path) {
            self.filter_cursor = pos;
        }
        self.recompute_visible_task_indices();
        if let ScheduleFocus::Task(idx) = self.focus {
            if !self.visible_task_indices.contains(&idx) {
                self.focus = self
                    .visible_task_indices
                    .first()
                    .copied()
                    .map(|i| {
                        self.ensure_visible(i);
                        ScheduleFocus::Task(i)
                    })
                    .unwrap_or(ScheduleFocus::OptionsBar);
            }
        }
    }

    /// Make sure the task at global index `idx` is visible on-screen. The
    /// `scroll_offset` indexes into `visible_task_indices` (mirrors Overview's
    /// scroll into `sorted_indices`), so we map the global idx to its
    /// position in the visible list first.
    fn ensure_visible(&mut self, idx: usize) {
        let Some(pos) = self.visible_task_indices.iter().position(|&i| i == idx) else {
            return;
        };
        if pos < self.scroll_offset {
            self.scroll_offset = pos;
        }
        if self.viewport_width > 0 {
            let fit = max_panels_for_width(self.viewport_width);
            if pos >= self.scroll_offset + fit {
                self.scroll_offset = pos + 1 - fit;
            }
        }
    }

    pub fn scroll_prompt_up(&mut self) {
        if let ScheduleFocus::Task(idx) = self.focus {
            if let Some(task) = self.tasks.get(idx) {
                let entry = self.ui.entry(task.id.clone()).or_default();
                entry.prompt_scroll = entry.prompt_scroll.saturating_sub(1);
            }
        }
    }

    pub fn scroll_prompt_down(&mut self) {
        if let ScheduleFocus::Task(idx) = self.focus {
            if let Some(task) = self.tasks.get(idx) {
                let entry = self.ui.entry(task.id.clone()).or_default();
                entry.prompt_scroll = entry.prompt_scroll.saturating_add(1);
            }
        }
    }

    /// Forward raw bytes to the focused Active task's PTY. No-op if the
    /// focused task isn't Active or has no live attachment.
    pub fn send_input(&self, data: Vec<u8>) {
        if let Some(panel) = self.focused_active_panel() {
            let _ = panel.command_tx.try_send(PanelCommand::Input(data));
        }
    }

    /// Scroll the focused Active panel's scrollback up by one page.
    pub fn panel_scroll_up(&mut self) {
        if let Some(task_id) = self.focused_active_task_id() {
            if let Some(panel) = self.panels.get_mut(&task_id) {
                let page = panel.vterm.rows();
                let max = panel.vterm.scrollback_len();
                panel.panel_scroll_offset = (panel.panel_scroll_offset + page).min(max);
            }
        }
    }

    /// Scroll the focused Active panel's scrollback down by one page.
    pub fn panel_scroll_down(&mut self) {
        if let Some(task_id) = self.focused_active_task_id() {
            if let Some(panel) = self.panels.get_mut(&task_id) {
                let page = panel.vterm.rows();
                panel.panel_scroll_offset = panel.panel_scroll_offset.saturating_sub(page);
            }
        }
    }

    fn focused_active_task_id(&self) -> Option<String> {
        match self.focus {
            ScheduleFocus::Task(idx) => self
                .tasks
                .get(idx)
                .filter(|t| {
                    t.status == ScheduledTaskStatus::Active && self.panels.contains_key(&t.id)
                })
                .map(|t| t.id.clone()),
            _ => None,
        }
    }

    fn focused_active_panel(&self) -> Option<&AgentPanel> {
        let task_id = self.focused_active_task_id()?;
        self.panels.get(&task_id)
    }

    /// Take the cached TerminalEmulator + connection for the task's active
    /// agent so focus mode can reuse them. Returns `None` if the task isn't
    /// Active. Stash with [`store_panel`] when focus mode closes.
    #[allow(dead_code)]
    pub fn take_terminal_cache(&mut self, task_id: &str) -> Option<AgentTerminalCache> {
        let panel = self.panels.remove(task_id)?;
        // We don't have any sub-panels to bundle; AgentTerminalCache exists
        // for focus-mode shells, not for the agent vterm itself. Returning
        // empty here preserves the type contract; the actual handoff for the
        // agent vterm uses the AgentPanel directly via the focus_mode_state
        // helpers in `overview`.
        // Re-insert so the caller can keep using it; they'll abort if needed.
        self.panels.insert(task_id.to_string(), panel);
        Some(AgentTerminalCache::new())
    }

    /// True if the focused task is Active and has a live attachment, i.e.
    /// `Shift+Down` is meaningful.
    pub fn focused_task_is_active(&self) -> bool {
        match self.focus {
            ScheduleFocus::Task(idx) => self
                .tasks
                .get(idx)
                .map(|t| t.status == ScheduledTaskStatus::Active && self.panels.contains_key(&t.id))
                .unwrap_or(false),
            _ => false,
        }
    }

    pub fn focused_task(&self) -> Option<&ScheduledTaskInfo> {
        match self.focus {
            ScheduleFocus::Task(idx) => self.tasks.get(idx),
            _ => None,
        }
    }

    /// Convert a key event into a [`ScheduleAction`]. The main loop is
    /// responsible for executing the action (IPC send, modal open, etc.).
    ///
    /// Two-mode handling: when the bar is focused, only filter-bar nav keys
    /// are consumed (everything else is a Noop, falling through to the rest
    /// of the UI). When a task is focused, the existing per-status keymap
    /// applies, with `Shift+Up` returning focus to the bar.
    pub fn handle_key(&mut self, key: KeyEvent) -> ScheduleAction {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // OptionsBar focused: handle filter cursor + bar→task transition.
        if self.focus == ScheduleFocus::OptionsBar {
            match key.code {
                KeyCode::Left if !shift => {
                    self.filter_cursor_left();
                }
                KeyCode::Right if !shift => {
                    self.filter_cursor_right();
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.toggle_filter_at_cursor();
                }
                KeyCode::Down if shift => {
                    self.enter_task_focus();
                }
                _ => {}
            }
            return ScheduleAction::Noop;
        }

        // Task focused: existing per-status keymap.
        let Some(task) = self.focused_task().cloned() else {
            return ScheduleAction::Noop;
        };

        match key.code {
            KeyCode::Left if shift => {
                self.focus_prev();
                ScheduleAction::Noop
            }
            KeyCode::Right if shift => {
                self.focus_next();
                ScheduleAction::Noop
            }
            KeyCode::Up if shift => {
                self.exit_task_focus();
                ScheduleAction::Noop
            }
            KeyCode::Down if shift => {
                if self.focused_task_is_active() {
                    ScheduleAction::EnterFocusMode { task_id: task.id }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Up => {
                self.scroll_prompt_up();
                ScheduleAction::Noop
            }
            KeyCode::Down => {
                self.scroll_prompt_down();
                ScheduleAction::Noop
            }
            KeyCode::Char('e') if !shift => {
                if matches!(
                    task.status,
                    ScheduledTaskStatus::Inactive | ScheduledTaskStatus::Aborted
                ) {
                    ScheduleAction::EditPrompt {
                        task_id: task.id,
                        current: task.prompt,
                    }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Char('p') if !shift => {
                if matches!(
                    task.status,
                    ScheduledTaskStatus::Inactive | ScheduledTaskStatus::Aborted
                ) {
                    ScheduleAction::TogglePlanMode {
                        task_id: task.id,
                        new_value: !task.plan_mode,
                    }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Char('x') if !shift => {
                if matches!(
                    task.status,
                    ScheduledTaskStatus::Inactive | ScheduledTaskStatus::Aborted
                ) {
                    ScheduleAction::ToggleAutoExit {
                        task_id: task.id,
                        new_value: !task.auto_exit,
                    }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Char('s') if !shift => {
                if task.status == ScheduledTaskStatus::Inactive {
                    ScheduleAction::StartNow { task_id: task.id }
                } else {
                    ScheduleAction::Noop
                }
            }
            // Lowercase r with no shift = in-place restart; uppercase R (which
            // crossterm reports when Shift+R is pressed) = clean restart.
            KeyCode::Char('r') if !shift => {
                if task.status == ScheduledTaskStatus::Aborted {
                    ScheduleAction::Restart {
                        task_id: task.id,
                        clean: false,
                    }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Char('R') => {
                if task.status == ScheduledTaskStatus::Aborted {
                    ScheduleAction::Restart {
                        task_id: task.id,
                        clean: true,
                    }
                } else {
                    ScheduleAction::Noop
                }
            }
            // Shift+S: reschedule. Only meaningful for tasks that haven't run
            // yet (Inactive) or stopped (Aborted) — Active tasks have a live
            // worktree and Complete tasks already finished.
            KeyCode::Char('S') => {
                if matches!(
                    task.status,
                    ScheduledTaskStatus::Inactive | ScheduledTaskStatus::Aborted
                ) {
                    ScheduleAction::OpenReschedule { task_id: task.id }
                } else {
                    ScheduleAction::Noop
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => ScheduleAction::ConfirmDelete {
                task_id: task.id.clone(),
                branch_name: task.branch_name.clone(),
            },
            KeyCode::Char('C') if shift => ScheduleAction::OpenClearMenu,
            _ => ScheduleAction::Noop,
        }
    }

    // -- Rendering --

    /// Render the Schedule tab into `area`. `repos` provides the canonical
    /// order + color palette for grouping; `repo_colors` is the path→color-name
    /// lookup the rest of the UI uses.
    ///
    /// Layout: a 1-row top bar (Overview-style repo chips + branch indicators),
    /// the panel grid in the middle, then a 2-row keybind hint footer at the
    /// bottom so the user always has every applicable shortcut visible. The
    /// footer is status-aware — its second line changes based on the focused
    /// task's state.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        repos: &[RepoInfo],
        repo_colors: &HashMap<String, String>,
        click_map: &mut ClickMap,
    ) {
        let hint_height: u16 = 2;
        let bar_height: u16 = 1;
        // Need room for the top bar + at least one row of panels + the hint
        // footer; if the viewport is too small, drop the hint footer first
        // (the bar is cheap and orienting, the hint is large but optional).
        let (bar_area, panels_area, hint_area) = if area.height > bar_height + hint_height + 1 {
            let [b, p, h] = Layout::vertical([
                Constraint::Length(bar_height),
                Constraint::Min(1),
                Constraint::Length(hint_height),
            ])
            .areas(area);
            (b, p, h)
        } else if area.height > bar_height + 1 {
            let [b, p] =
                Layout::vertical([Constraint::Length(bar_height), Constraint::Min(1)]).areas(area);
            (b, p, Rect::new(area.x, area.y + area.height, area.width, 0))
        } else {
            (
                Rect::new(area.x, area.y, area.width, 0),
                area,
                Rect::new(area.x, area.y + area.height, area.width, 0),
            )
        };

        // Refresh the visible-tasks list every frame so any external mutation
        // (sync_tasks, manual collapsed_repos changes) is reflected.
        self.recompute_visible_task_indices();

        if self.tasks.is_empty() {
            // Paint the bar background even when empty so the layout doesn't
            // shift the moment a task appears.
            render_empty_bar(frame, bar_area);
            self.render_empty(frame, panels_area);
            self.render_keybind_hint_bar(frame, hint_area, None, click_map);
            return;
        }
        self.resize_panels_to(panels_area);

        let count_fit = max_panels_for_width(panels_area.width).max(1);
        // Clamp scroll to the visible-tasks list so collapsing a repo doesn't
        // leave the grid scrolled past the last remaining panel.
        if self.scroll_offset >= self.visible_task_indices.len() {
            self.scroll_offset = self.visible_task_indices.len().saturating_sub(1);
        }
        let visible_count = self
            .visible_task_indices
            .len()
            .saturating_sub(self.scroll_offset)
            .min(count_fit);
        // Always reserve at least 2 column slots so a single column doesn't
        // explode to fill the whole width.
        let slots = visible_count.max(2);
        let constraints: Vec<Constraint> = (0..visible_count)
            .map(|_| Constraint::Ratio(1, slots as u32))
            .collect();

        // Indices visible on screen — used to invert-video the matching
        // branch indicators in the bar. These are global indices into
        // `self.tasks`, picked out of `visible_task_indices`.
        let visible_set: HashSet<usize> = if visible_count > 0 {
            self.visible_task_indices[self.scroll_offset..self.scroll_offset + visible_count]
                .iter()
                .copied()
                .collect()
        } else {
            HashSet::new()
        };
        let bar_focused = self.focus == ScheduleFocus::OptionsBar;
        self.render_options_bar(
            frame,
            bar_area,
            repos,
            &visible_set,
            bar_focused,
            self.filter_cursor,
            click_map,
        );

        if constraints.is_empty() {
            self.render_keybind_hint_bar(frame, hint_area, None, click_map);
            return;
        }
        let panel_areas = Layout::horizontal(constraints).split(panels_area);

        let visible_indices: Vec<usize> = self.visible_task_indices
            [self.scroll_offset..self.scroll_offset + visible_count]
            .to_vec();
        let focused_task_id = self.focused_task().map(|t| t.id.clone());
        let any_dep_warning_map: HashMap<String, bool> =
            self.dep_warning_map().into_iter().collect();
        let now = Utc::now();

        for (display_idx, &task_idx) in visible_indices.iter().enumerate() {
            let area = panel_areas[display_idx];
            click_map.schedule_panels.push((area, task_idx));
            let task_clone;
            let prompt_scroll;
            {
                let task = &self.tasks[task_idx];
                task_clone = task.clone();
                prompt_scroll = self.ui.get(&task.id).map(|s| s.prompt_scroll).unwrap_or(0);
            }
            let is_focused = focused_task_id.as_deref() == Some(task_clone.id.as_str());
            let dep_warning = *any_dep_warning_map.get(&task_clone.id).unwrap_or(&false);
            let repo_color = repo_colors
                .get(task_clone.repo_path.as_str())
                .map(|c| theme::repo_color(c));
            self.render_panel(
                frame,
                area,
                &task_clone,
                is_focused,
                prompt_scroll,
                dep_warning,
                now,
                repo_color,
            );
        }

        let focused_status = self.focused_task().map(|t| t.status);
        self.render_keybind_hint_bar(frame, hint_area, focused_status, click_map);
    }

    /// Two-row keybind hint footer so the user can see every shortcut at a
    /// glance. Top row = always-available bindings, bottom row = bindings
    /// specific to the focused task's status. Each pair is recorded into the
    /// click map so a click on a key hint fires the same action.
    fn render_keybind_hint_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        focused_status: Option<ScheduledTaskStatus>,
        click_map: &mut ClickMap,
    ) {
        if area.height == 0 {
            return;
        }
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };

        let common_pairs: Vec<(String, String, Option<ScheduleHintKey>)> = vec![
            (
                "Shift+\u{2190}".into(),
                "prev".into(),
                Some(ScheduleHintKey::PrevPanel),
            ),
            (
                "Shift+\u{2192}".into(),
                "next".into(),
                Some(ScheduleHintKey::NextPanel),
            ),
            (
                format!("{mod_key}+S"),
                "new task".into(),
                Some(ScheduleHintKey::NewTask),
            ),
            (
                "d/Del".into(),
                "delete".into(),
                Some(ScheduleHintKey::Delete),
            ),
            (
                "Shift+C".into(),
                "clear by status".into(),
                Some(ScheduleHintKey::ClearByStatus),
            ),
            ("?".into(), "help".into(), Some(ScheduleHintKey::Help)),
        ];

        // Bottom row: actions that apply only to the focused task's current state.
        let status_pairs: Vec<(String, String, Option<ScheduleHintKey>)> = match focused_status {
            Some(ScheduledTaskStatus::Inactive) => vec![
                (
                    "e".into(),
                    "edit prompt".into(),
                    Some(ScheduleHintKey::EditPrompt),
                ),
                (
                    "p".into(),
                    "toggle plan".into(),
                    Some(ScheduleHintKey::TogglePlan),
                ),
                (
                    "x".into(),
                    "toggle auto-exit".into(),
                    Some(ScheduleHintKey::ToggleAutoExit),
                ),
                (
                    "s".into(),
                    "start now".into(),
                    Some(ScheduleHintKey::StartNow),
                ),
                (
                    "Shift+S".into(),
                    "reschedule".into(),
                    Some(ScheduleHintKey::Reschedule),
                ),
                ("\u{2191}/\u{2193}".into(), "scroll prompt".into(), None),
            ],
            Some(ScheduledTaskStatus::Active) => vec![
                ("type".into(), "send to agent".into(), None),
                (
                    "Shift+\u{2193}".into(),
                    "focus mode (terminal)".into(),
                    Some(ScheduleHintKey::EnterFocusMode),
                ),
                ("PgUp/PgDn".into(), "scroll".into(), None),
            ],
            Some(ScheduledTaskStatus::Aborted) => vec![
                (
                    "e".into(),
                    "edit prompt".into(),
                    Some(ScheduleHintKey::EditPrompt),
                ),
                (
                    "p".into(),
                    "toggle plan".into(),
                    Some(ScheduleHintKey::TogglePlan),
                ),
                (
                    "x".into(),
                    "toggle auto-exit".into(),
                    Some(ScheduleHintKey::ToggleAutoExit),
                ),
                ("r".into(), "restart".into(), Some(ScheduleHintKey::Restart)),
                (
                    "Shift+R".into(),
                    "clean restart".into(),
                    Some(ScheduleHintKey::CleanRestart),
                ),
                (
                    "Shift+S".into(),
                    "reschedule".into(),
                    Some(ScheduleHintKey::Reschedule),
                ),
            ],
            Some(ScheduledTaskStatus::Complete) => vec![(
                "d/Del".into(),
                "delete completed task".into(),
                Some(ScheduleHintKey::Delete),
            )],
            None => vec![(
                format!("{mod_key}+S"),
                "schedule a task to begin".into(),
                Some(ScheduleHintKey::NewTask),
            )],
        };

        let status_label = match focused_status {
            Some(ScheduledTaskStatus::Inactive) => Some(("INACTIVE", theme::R_WARNING)),
            Some(ScheduledTaskStatus::Active) => Some(("ACTIVE", theme::R_SUCCESS)),
            Some(ScheduledTaskStatus::Aborted) => Some(("ABORTED", theme::R_ERROR)),
            Some(ScheduledTaskStatus::Complete) => Some(("COMPLETE", theme::R_TEXT_DISABLED)),
            None => None,
        };

        let chunks = if area.height >= 2 {
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)])
                .split(area)
                .to_vec()
        } else {
            vec![area]
        };

        // Top row: common keybinds, no status label.
        let (top_spans, top_hits) = build_hint_spans_with_hits(&common_pairs, None, chunks[0]);
        frame.render_widget(Paragraph::new(Line::from(top_spans)), chunks[0]);
        click_map.schedule_hint_keys.extend(top_hits);

        if chunks.len() > 1 {
            let (bot_spans, bot_hits) =
                build_hint_spans_with_hits(&status_pairs, status_label, chunks[1]);
            frame.render_widget(Paragraph::new(Line::from(bot_spans)), chunks[1]);
            click_map.schedule_hint_keys.extend(bot_hits);
        }
    }

    /// Render the schedule top bar: repo chips on the left, branch indicators
    /// on the right. Branch indicators are clickable — clicking one focuses
    /// the corresponding task and scrolls it into view. Repo chips are
    /// clickable filter toggles; when the bar is focused the cursor position
    /// gets a highlight background and Left/Right cycles through chips.
    #[allow(clippy::too_many_arguments)]
    fn render_options_bar(
        &self,
        frame: &mut Frame,
        area: Rect,
        repos: &[RepoInfo],
        visible_set: &HashSet<usize>,
        bar_focused: bool,
        filter_cursor: usize,
        click_map: &mut ClickMap,
    ) {
        // Bar background brightens slightly when focused so the user can tell
        // that keystrokes will land here. Mirrors Overview's behavior.
        let bar_bg = if bar_focused {
            theme::R_BG_OVERLAY
        } else {
            theme::R_BG_RAISED
        };

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " ".repeat(area.width as usize),
                Style::default().bg(bar_bg),
            ))),
            area,
        );

        // Only show chips for repos that actually have tasks scheduled —
        // matches what users care about on this tab and avoids a noisy bar.
        // Iterate `repos` in canonical order (matches `self.tasks` sort) so
        // the on-screen cursor index lines up with `filter_cursor`.
        let used_paths: HashSet<&str> = self.tasks.iter().map(|t| t.repo_path.as_str()).collect();
        let mut spans: Vec<Span> = Vec::new();
        let mut col_cursor: u16 = area.x;
        let mut shown_any_repo = false;
        let mut cursor_idx: usize = 0;

        let push_span =
            |span: Span<'static>, spans: &mut Vec<Span<'static>>, col_cursor: &mut u16| {
                *col_cursor = col_cursor.saturating_add(span.content.chars().count() as u16);
                spans.push(span);
            };

        for repo in repos.iter() {
            if !used_paths.contains(repo.path.as_str()) {
                continue;
            }
            let color = repo
                .color
                .as_ref()
                .map(|c| theme::repo_color(c))
                .unwrap_or(theme::R_ACCENT);

            let is_collapsed = self.collapsed_repos.contains(&repo.path);
            let is_cursor = bar_focused && cursor_idx == filter_cursor;
            let chip_bg = if is_cursor {
                theme::R_BG_ACTIVE
            } else {
                bar_bg
            };
            let (dot_color, text_color) = if is_collapsed {
                (theme::dim_color(color), theme::R_TEXT_DISABLED)
            } else {
                (color, theme::R_TEXT_PRIMARY)
            };

            // Track the chip's rect so a click toggles the filter. Width is
            // " ● " (3) + name + trailing space (1) = 4 + name.
            let chip_x = col_cursor;
            let chip_w = 4u16 + repo.name.chars().count() as u16;

            push_span(
                Span::styled(" \u{25cf} ", Style::default().fg(dot_color).bg(chip_bg)),
                &mut spans,
                &mut col_cursor,
            );
            push_span(
                Span::styled(
                    format!("{} ", repo.name),
                    Style::default().fg(text_color).bg(chip_bg),
                ),
                &mut spans,
                &mut col_cursor,
            );

            if chip_x < area.x + area.width {
                let clipped_w = (area.x + area.width).saturating_sub(chip_x).min(chip_w);
                if clipped_w > 0 {
                    let chip_rect = Rect::new(chip_x, area.y, clipped_w, area.height);
                    click_map
                        .schedule_repo_buttons
                        .push((chip_rect, repo.path.clone()));
                }
            }

            shown_any_repo = true;
            cursor_idx += 1;
        }

        if shown_any_repo && !self.tasks.is_empty() {
            push_span(
                Span::styled(
                    " \u{2502} ",
                    Style::default().fg(theme::R_TEXT_TERTIARY).bg(bar_bg),
                ),
                &mut spans,
                &mut col_cursor,
            );
        }

        // Branch indicators — tasks are already in repo-sorted order. Each
        // chip records its rect so a click can focus that task. Indicators
        // for tasks in collapsed repos are dimmed but still rendered (and
        // still clickable — clicking un-collapses the repo).
        for (idx, task) in self.tasks.iter().enumerate() {
            let repo_color = repos
                .iter()
                .find(|r| r.path == task.repo_path)
                .and_then(|r| r.color.as_ref())
                .map(|c| theme::repo_color(c))
                .unwrap_or(theme::R_ACCENT);

            let is_visible = visible_set.contains(&idx);
            let is_repo_collapsed = self.collapsed_repos.contains(task.repo_path.as_str());
            let style = if is_visible {
                Style::default().fg(theme::R_TEXT_PRIMARY).bg(repo_color)
            } else if is_repo_collapsed {
                Style::default().fg(theme::R_TEXT_DISABLED).bg(bar_bg)
            } else {
                Style::default().fg(repo_color).bg(bar_bg)
            };
            let label = format!(" {} ", task.branch_name);
            let label_w = label.chars().count() as u16;
            let chip_x = col_cursor;
            let chip_rect = Rect::new(chip_x, area.y, label_w.min(area.width), area.height);
            // Clamp to the bar's right edge in case the chip would overflow —
            // overflow chips just won't be clickable past the visible width.
            if chip_x < area.x + area.width {
                let clipped_w = (area.x + area.width).saturating_sub(chip_x);
                let safe_rect = Rect::new(
                    chip_x,
                    chip_rect.y,
                    label_w.min(clipped_w),
                    chip_rect.height,
                );
                if safe_rect.width > 0 {
                    click_map.schedule_branch_indicators.push((safe_rect, idx));
                }
            }
            push_span(Span::styled(label, style), &mut spans, &mut col_cursor);
        }

        // Pad remaining width so the bar paints edge-to-edge in the bar bg.
        let content_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let remaining = (area.width as usize).saturating_sub(content_len);
        if remaining > 0 {
            spans.push(Span::styled(
                " ".repeat(remaining),
                Style::default().bg(bar_bg),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "No scheduled tasks",
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme::R_TEXT_TERTIARY)),
                Span::styled(
                    format!("{mod_key}+S"),
                    Style::default()
                        .fg(theme::R_ACCENT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " to schedule one",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("or press ", Style::default().fg(theme::R_TEXT_TERTIARY)),
                Span::styled(
                    "?",
                    Style::default()
                        .fg(theme::R_ACCENT_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " for the full keybind reference",
                    Style::default().fg(theme::R_TEXT_TERTIARY),
                ),
            ]),
        ];
        let p = Paragraph::new(lines).alignment(Alignment::Center);
        frame.render_widget(p, area);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_panel(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        task: &ScheduledTaskInfo,
        is_focused: bool,
        prompt_scroll: u16,
        dep_warning: bool,
        now: DateTime<Utc>,
        repo_color: Option<Color>,
    ) {
        // Border tints with the repo color when known; falls back to the
        // accent. Focused = full color, unfocused = dimmed (mirrors Overview).
        let border_color = match (is_focused, repo_color) {
            (true, Some(c)) => c,
            (false, Some(c)) => theme::dim_color(c),
            (true, None) => theme::R_ACCENT_BRIGHT,
            (false, None) => theme::R_ACCENT_DIM,
        };
        let border_style = Style::default().fg(border_color);
        let title = format!(" {} ", task.branch_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(Style::default().bg(theme::R_BG_BASE))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(theme::R_TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        match task.status {
            ScheduledTaskStatus::Active => self.render_active_body(frame, inner, task),
            ScheduledTaskStatus::Complete => render_complete_body(frame, inner, task),
            ScheduledTaskStatus::Aborted => {
                render_inactive_or_aborted_body(frame, inner, task, prompt_scroll, dep_warning, now)
            }
            ScheduledTaskStatus::Inactive => {
                render_inactive_or_aborted_body(frame, inner, task, prompt_scroll, dep_warning, now)
            }
        }
    }

    fn render_active_body(&mut self, frame: &mut Frame, inner: Rect, task: &ScheduledTaskInfo) {
        // Active panels hand the entire inner area to the live PTY so the
        // terminal output gets the maximum width and height. Status pills and
        // hints would only steal rows the user wants to see; the keybind
        // footer at the bottom of the tab already documents Shift+↓.
        if let Some(panel) = self.panels.get_mut(&task.id) {
            let cols = inner.width.max(20);
            let rows = inner.height.max(4);
            if panel.vterm.cols() != cols as usize || panel.vterm.rows() != rows as usize {
                panel.vterm.resize(cols as usize, rows as usize);
                let _ = panel
                    .command_tx
                    .try_send(PanelCommand::Resize { cols, rows });
            }
            let lines = if panel.panel_scroll_offset > 0 {
                panel
                    .vterm
                    .to_ratatui_lines_scrolled(panel.panel_scroll_offset)
            } else {
                panel.vterm.to_ratatui_lines()
            };
            frame.render_widget(
                Paragraph::new(lines).style(Style::default().bg(theme::R_BG_BASE)),
                inner,
            );
        } else {
            let p = Paragraph::new(Span::styled(
                "(connecting…)",
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ))
            .style(Style::default().bg(theme::R_BG_BASE));
            frame.render_widget(p, inner);
        }
    }

    /// Compute, for each task, whether it has at least one Depend upstream
    /// whose `auto_exit` is OFF. Surfaced as a warning pill so the user knows
    /// the chain may stall waiting for a manual exit.
    fn dep_warning_map(&self) -> Vec<(String, bool)> {
        let by_id: HashMap<&str, &ScheduledTaskInfo> =
            self.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        self.tasks
            .iter()
            .map(|t| {
                let warn = match &t.schedule {
                    ScheduleKind::Depend { depends_on_ids } => depends_on_ids.iter().any(|id| {
                        by_id
                            .get(id.as_str())
                            .map(|up| !up.auto_exit && up.status != ScheduledTaskStatus::Complete)
                            .unwrap_or(false)
                    }),
                    _ => false,
                };
                (t.id.clone(), warn)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Panel-body renderers (free functions so they don't borrow `self`)
// ---------------------------------------------------------------------------

/// Render an empty top bar (just the bg fill) when there are no tasks.
fn render_empty_bar(frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(theme::R_BG_RAISED),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_complete_body(frame: &mut Frame, inner: Rect, task: &ScheduledTaskInfo) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "✓ ",
                Style::default()
                    .fg(theme::R_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                task.branch_name.clone(),
                Style::default().fg(theme::R_TEXT_DISABLED),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "completed",
            Style::default().fg(theme::R_TEXT_DISABLED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("press ", Style::default().fg(theme::R_TEXT_TERTIARY)),
            Span::styled(
                "d",
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" to remove", Style::default().fg(theme::R_TEXT_TERTIARY)),
        ]),
    ];
    let p = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(p, inner);
}

fn render_inactive_or_aborted_body(
    frame: &mut Frame,
    inner: Rect,
    task: &ScheduledTaskInfo,
    prompt_scroll: u16,
    dep_warning: bool,
    now: DateTime<Utc>,
) {
    let aborted = task.status == ScheduledTaskStatus::Aborted;
    let status_pill = if aborted {
        Span::styled(
            "ABORTED ",
            Style::default()
                .fg(theme::R_ERROR)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "INACTIVE ",
            Style::default()
                .fg(theme::R_WARNING)
                .add_modifier(Modifier::BOLD),
        )
    };

    let header = Paragraph::new(Line::from(vec![
        status_pill,
        pill("PLAN", task.plan_mode, theme::R_WARNING),
        pill("AUTO-EXIT", task.auto_exit, theme::R_INFO),
    ]));

    let sched_line = Paragraph::new(Line::from(schedule_line_spans(
        &task.schedule,
        aborted,
        now,
    )));

    let warning_line = if dep_warning {
        Some(Paragraph::new(Span::styled(
            "⚠ depends on tasks without AUTO-EXIT",
            Style::default()
                .fg(theme::R_WARNING)
                .add_modifier(Modifier::BOLD),
        )))
    } else {
        None
    };

    // Layout: header | meta | (warning) | prompt body
    let constraints: Vec<Constraint> = if warning_line.is_some() {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ]
    };
    let chunks = Layout::vertical(constraints).split(inner);

    frame.render_widget(header, chunks[0]);
    frame.render_widget(sched_line, chunks[1]);
    if let Some(w) = warning_line {
        frame.render_widget(w, chunks[2]);
        frame.render_widget(prompt_paragraph(task, prompt_scroll), chunks[3]);
    } else {
        frame.render_widget(prompt_paragraph(task, prompt_scroll), chunks[2]);
    }
}

fn prompt_paragraph(task: &ScheduledTaskInfo, scroll: u16) -> Paragraph<'static> {
    Paragraph::new(task.prompt.clone())
        .style(Style::default().fg(theme::R_TEXT_PRIMARY))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

/// Render a row of `key — description` pairs separated by middle dots,
/// optionally prefixed with a colored status pill (`[INACTIVE]` etc.).
/// Also computes a click rect for each pair tagged with a [`ScheduleHintKey`]
/// so the footer doubles as a clickable button strip.
fn build_hint_spans_with_hits(
    pairs: &[(String, String, Option<ScheduleHintKey>)],
    status_label: Option<(&str, ratatui::style::Color)>,
    row_area: Rect,
) -> (Vec<Span<'static>>, Vec<(Rect, ScheduleHintKey)>) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut hits: Vec<(Rect, ScheduleHintKey)> = Vec::new();
    let mut col: u16 = row_area.x;
    let row_y = row_area.y;
    let row_end = row_area.x + row_area.width;
    let row_h = row_area.height.max(1);

    let push = |span: Span<'static>, spans: &mut Vec<Span<'static>>, col: &mut u16| {
        let w = span.content.chars().count() as u16;
        spans.push(span);
        *col = col.saturating_add(w);
    };

    push(Span::raw(" "), &mut spans, &mut col);
    if let Some((label, color)) = status_label {
        push(
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(theme::R_BG_BASE)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            &mut spans,
            &mut col,
        );
        push(Span::raw("  "), &mut spans, &mut col);
    }
    for (i, (key, desc, hint_key)) in pairs.iter().enumerate() {
        if i > 0 {
            push(
                Span::styled("  \u{00b7}  ", Style::default().fg(theme::R_TEXT_DISABLED)),
                &mut spans,
                &mut col,
            );
        }
        let pair_start = col;
        push(
            Span::styled(
                key.clone(),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            &mut spans,
            &mut col,
        );
        push(Span::raw(" "), &mut spans, &mut col);
        push(
            Span::styled(desc.clone(), Style::default().fg(theme::R_TEXT_SECONDARY)),
            &mut spans,
            &mut col,
        );
        if let Some(hk) = hint_key {
            // The whole "key + space + description" is the click hit zone so
            // users can click sloppily and still hit the right action. Clamp
            // to the visible row width.
            if pair_start < row_end {
                let end = col.min(row_end);
                if end > pair_start {
                    hits.push((Rect::new(pair_start, row_y, end - pair_start, row_h), *hk));
                }
            }
        }
    }
    (spans, hits)
}

fn pill<'a>(label: &'a str, on: bool, on_color: ratatui::style::Color) -> Span<'a> {
    if on {
        Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(theme::R_BG_BASE)
                .bg(on_color)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!(" {label} "),
            Style::default().fg(theme::R_TEXT_DISABLED),
        )
    }
}

fn describe_schedule(kind: &ScheduleKind, now: DateTime<Utc>) -> String {
    match kind {
        ScheduleKind::Time { start_at } => match DateTime::parse_from_rfc3339(start_at) {
            Ok(dt) => {
                let target = dt.with_timezone(&Utc);
                if target <= now {
                    "Starts: any moment".into()
                } else {
                    let delta = target - now;
                    let total_secs = delta.num_seconds();
                    if total_secs < 60 {
                        format!("Starts in {}s", total_secs)
                    } else if total_secs < 3600 {
                        format!("Starts in {}m {}s", total_secs / 60, total_secs % 60)
                    } else if total_secs < 86_400 {
                        let h = total_secs / 3600;
                        let m = (total_secs % 3600) / 60;
                        format!("Starts in {h}h {m}m")
                    } else {
                        let d = total_secs / 86_400;
                        let h = (total_secs % 86_400) / 3600;
                        format!("Starts in {d}d {h}h")
                    }
                }
            }
            Err(_) => "Starts: <invalid time>".into(),
        },
        ScheduleKind::Depend { depends_on_ids } => {
            format!("Waiting on {} task(s)", depends_on_ids.len())
        }
        ScheduleKind::Unscheduled => "Unscheduled".into(),
    }
}

/// Schedule meta line — a description of when the task will run, with any
/// inline keybinds rendered in the accent color so they stand out.
fn schedule_line_spans(
    kind: &ScheduleKind,
    aborted: bool,
    now: DateTime<Utc>,
) -> Vec<Span<'static>> {
    let key_style = Style::default()
        .fg(theme::R_ACCENT_BRIGHT)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme::R_TEXT_TERTIARY);

    if aborted {
        return vec![
            Span::styled("Aborted — press ", dim),
            Span::styled("r", key_style),
            Span::styled(" to restart, ", dim),
            Span::styled("Shift+R", key_style),
            Span::styled(" for clean restart", dim),
        ];
    }
    if matches!(kind, ScheduleKind::Unscheduled) {
        return vec![
            Span::styled("Unscheduled — press ", dim),
            Span::styled("s", key_style),
            Span::styled(" to start now", dim),
        ];
    }
    vec![Span::styled(describe_schedule(kind, now), dim)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: ScheduledTaskStatus, schedule: ScheduleKind) -> ScheduledTaskInfo {
        ScheduledTaskInfo {
            id: id.into(),
            repo_path: "/repo".into(),
            repo_name: "repo".into(),
            branch_name: format!("br-{id}"),
            base_branch: None,
            new_branch: None,
            prompt: "p".into(),
            plan_mode: false,
            auto_exit: false,
            agent_binary: "claude".into(),
            schedule,
            status,
            agent_id: None,
            created_at: "2026-05-06T09:00:00Z".into(),
            completed_at: None,
        }
    }

    /// Tests don't care about repo ordering, so they pass an empty slice;
    /// tasks fall back to created_at + id ordering, which matches their
    /// declaration order in these fixtures.
    fn sync(state: &mut ScheduleState, tasks: Vec<ScheduledTaskInfo>) {
        state.sync_tasks(tasks, &[]);
    }

    #[test]
    fn focus_clamps_when_tasks_shrink() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![
                task(
                    "a",
                    ScheduledTaskStatus::Inactive,
                    ScheduleKind::Unscheduled,
                ),
                task(
                    "b",
                    ScheduledTaskStatus::Inactive,
                    ScheduleKind::Unscheduled,
                ),
                task(
                    "c",
                    ScheduledTaskStatus::Inactive,
                    ScheduleKind::Unscheduled,
                ),
            ],
        );
        state.focus = ScheduleFocus::Task(2);
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        assert_eq!(state.focus, ScheduleFocus::Task(0));
    }

    #[test]
    fn focus_becomes_bar_when_list_empty() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        state.focus = ScheduleFocus::Task(0);
        sync(&mut state, Vec::new());
        assert_eq!(state.focus, ScheduleFocus::OptionsBar);
    }

    #[test]
    fn focus_initialises_when_tasks_appear() {
        let mut state = ScheduleState::new();
        assert_eq!(state.focus, ScheduleFocus::OptionsBar);
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        assert_eq!(state.focus, ScheduleFocus::Task(0));
    }

    #[test]
    fn dep_warning_when_upstream_no_auto_exit() {
        let mut state = ScheduleState::new();
        let mut up = task(
            "u",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        up.auto_exit = false;
        let down = task(
            "d",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Depend {
                depends_on_ids: vec!["u".into()],
            },
        );
        sync(&mut state, vec![up, down]);
        let warns: HashMap<String, bool> = state.dep_warning_map().into_iter().collect();
        assert_eq!(warns.get("d"), Some(&true));
        assert_eq!(warns.get("u"), Some(&false));
    }

    #[test]
    fn dep_warning_clears_when_upstream_completes() {
        let mut state = ScheduleState::new();
        let mut up = task(
            "u",
            ScheduledTaskStatus::Complete,
            ScheduleKind::Unscheduled,
        );
        up.auto_exit = false;
        let down = task(
            "d",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Depend {
                depends_on_ids: vec!["u".into()],
            },
        );
        sync(&mut state, vec![up, down]);
        let warns: HashMap<String, bool> = state.dep_warning_map().into_iter().collect();
        assert_eq!(warns.get("d"), Some(&false));
    }

    #[test]
    fn dep_warning_off_when_upstream_auto_exits() {
        let mut state = ScheduleState::new();
        let mut up = task(
            "u",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        up.auto_exit = true;
        let down = task(
            "d",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Depend {
                depends_on_ids: vec!["u".into()],
            },
        );
        sync(&mut state, vec![up, down]);
        let warns: HashMap<String, bool> = state.dep_warning_map().into_iter().collect();
        assert_eq!(warns.get("d"), Some(&false));
    }

    #[test]
    fn handle_key_e_emits_edit_for_inactive() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        match action {
            ScheduleAction::EditPrompt { task_id, .. } => assert_eq!(task_id, "a"),
            other => panic!("expected EditPrompt, got {:?}", other),
        }
    }

    #[test]
    fn handle_key_e_noop_for_active() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Active,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        assert_eq!(action, ScheduleAction::Noop);
    }

    #[test]
    fn handle_key_s_starts_inactive() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(
            action,
            ScheduleAction::StartNow {
                task_id: "a".into()
            }
        );
    }

    #[test]
    fn handle_key_r_restarts_aborted_only() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Aborted,
                ScheduleKind::Unscheduled,
            )],
        );
        let r = state.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(
            r,
            ScheduleAction::Restart {
                task_id: "a".into(),
                clean: false
            }
        );
        let r_clean = state.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT));
        assert_eq!(
            r_clean,
            ScheduleAction::Restart {
                task_id: "a".into(),
                clean: true
            }
        );
    }

    #[test]
    fn handle_key_shift_s_reschedules_inactive() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(
            action,
            ScheduleAction::OpenReschedule {
                task_id: "a".into()
            }
        );
    }

    #[test]
    fn handle_key_shift_s_reschedules_aborted() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Aborted,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(
            action,
            ScheduleAction::OpenReschedule {
                task_id: "a".into()
            }
        );
    }

    #[test]
    fn handle_key_shift_s_noop_for_active() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Active,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(action, ScheduleAction::Noop);
    }

    #[test]
    fn handle_key_shift_s_noop_for_complete() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Complete,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT));
        assert_eq!(action, ScheduleAction::Noop);
    }

    #[test]
    fn handle_key_d_confirms_delete() {
        let mut state = ScheduleState::new();
        sync(
            &mut state,
            vec![task(
                "a",
                ScheduledTaskStatus::Inactive,
                ScheduleKind::Unscheduled,
            )],
        );
        let action = state.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        match action {
            ScheduleAction::ConfirmDelete { task_id, .. } => assert_eq!(task_id, "a"),
            other => panic!("expected ConfirmDelete, got {:?}", other),
        }
    }

    #[test]
    fn tasks_sort_by_repo_order_then_created_at() {
        // Two repos with explicit order; tasks arrive in mixed order from
        // the hub. After sync, they must appear sorted by repo first, then
        // by created_at within a repo.
        let repo_a = RepoInfo {
            path: "/repo-a".into(),
            name: "repo-a".into(),
            color: Some("red".into()),
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let repo_b = RepoInfo {
            path: "/repo-b".into(),
            name: "repo-b".into(),
            color: Some("blue".into()),
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let mut t1 = task(
            "1",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t1.repo_path = "/repo-b".into();
        t1.created_at = "2026-05-06T10:00:00Z".into();
        let mut t2 = task(
            "2",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t2.repo_path = "/repo-a".into();
        t2.created_at = "2026-05-06T11:00:00Z".into();
        let mut t3 = task(
            "3",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t3.repo_path = "/repo-a".into();
        t3.created_at = "2026-05-06T09:00:00Z".into();

        let mut state = ScheduleState::new();
        state.sync_tasks(vec![t1, t2, t3], &[repo_a, repo_b]);

        let order: Vec<&str> = state.tasks.iter().map(|t| t.id.as_str()).collect();
        // repo-a comes first; within it, t3 (09:00) before t2 (11:00). repo-b
        // comes last with t1.
        assert_eq!(order, vec!["3", "2", "1"]);
    }

    #[test]
    fn focus_follows_task_id_after_resort() {
        let repo_a = RepoInfo {
            path: "/repo-a".into(),
            name: "a".into(),
            color: None,
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let repo_b = RepoInfo {
            path: "/repo-b".into(),
            name: "b".into(),
            color: None,
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let mut t1 = task(
            "1",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t1.repo_path = "/repo-a".into();
        let mut t2 = task(
            "2",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t2.repo_path = "/repo-b".into();

        let mut state = ScheduleState::new();
        // First sync: only repo-a known; t2 falls to the end (unknown repo).
        state.sync_tasks(vec![t1.clone(), t2.clone()], std::slice::from_ref(&repo_a));
        // Focus the repo-a task (t1) at index 0.
        state.focus = ScheduleFocus::Task(0);
        // Now sync with both repos in reversed order: t2 (repo-b) sorts
        // first, t1 second. Focus must follow t1 to its new index (1).
        state.sync_tasks(vec![t1, t2], &[repo_b, repo_a]);
        assert_eq!(state.focus, ScheduleFocus::Task(1));
    }

    #[test]
    fn describe_schedule_unscheduled() {
        let s = describe_schedule(&ScheduleKind::Unscheduled, Utc::now());
        assert!(s.starts_with("Unscheduled"));
    }

    #[test]
    fn describe_schedule_in_2_hours() {
        let now = Utc::now();
        let target = (now + chrono::Duration::hours(2)).to_rfc3339();
        let s = describe_schedule(&ScheduleKind::Time { start_at: target }, now);
        assert!(s.contains("2h"), "got {s}");
    }

    /// Build two tasks across two repos. Used by the filter tests below.
    fn two_repo_state() -> (ScheduleState, RepoInfo, RepoInfo) {
        let repo_a = RepoInfo {
            path: "/repo-a".into(),
            name: "a".into(),
            color: None,
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let repo_b = RepoInfo {
            path: "/repo-b".into(),
            name: "b".into(),
            color: None,
            editor: None,
            local_branches: vec![],
            remote_branches: vec![],
        };
        let mut t1 = task(
            "1",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t1.repo_path = "/repo-a".into();
        let mut t2 = task(
            "2",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t2.repo_path = "/repo-b".into();
        let mut state = ScheduleState::new();
        state.sync_tasks(vec![t1, t2], &[repo_a.clone(), repo_b.clone()]);
        (state, repo_a, repo_b)
    }

    #[test]
    fn used_repo_paths_in_canonical_order() {
        let (state, _, _) = two_repo_state();
        let used = state.used_repo_paths();
        assert_eq!(used, vec!["/repo-a".to_string(), "/repo-b".to_string()]);
    }

    #[test]
    fn collapsing_repo_hides_its_tasks_from_visible_indices() {
        let (mut state, _, _) = two_repo_state();
        // Both tasks visible to start.
        assert_eq!(state.visible_task_indices.len(), 2);
        // Collapse repo-a; only repo-b's task should remain visible.
        state.collapsed_repos.insert("/repo-a".into());
        state.recompute_visible_task_indices();
        let visible_repos: Vec<&str> = state
            .visible_task_indices
            .iter()
            .map(|i| state.tasks[*i].repo_path.as_str())
            .collect();
        assert_eq!(visible_repos, vec!["/repo-b"]);
    }

    #[test]
    fn toggle_filter_at_cursor_moves_focus_off_collapsed_task() {
        let (mut state, _, _) = two_repo_state();
        // Focus the task in repo-a (index 0 in canonical order).
        state.focus = ScheduleFocus::Task(0);
        state.filter_cursor = 0; // repo-a chip
        state.toggle_filter_at_cursor();
        // Repo-a is now collapsed; focus must have moved to the next visible
        // task (repo-b's task) instead of being stuck on the hidden one.
        assert!(state.collapsed_repos.contains("/repo-a"));
        match state.focus {
            ScheduleFocus::Task(idx) => {
                assert_eq!(state.tasks[idx].repo_path, "/repo-b");
            }
            other => panic!("expected Task focus, got {:?}", other),
        }
    }

    #[test]
    fn toggle_filter_drops_to_bar_when_no_visible_tasks_remain() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::Task(0);
        // Collapse both repos.
        state.filter_cursor = 0;
        state.toggle_filter_at_cursor();
        state.filter_cursor = 1;
        state.toggle_filter_at_cursor();
        assert_eq!(state.focus, ScheduleFocus::OptionsBar);
    }

    #[test]
    fn filter_cursor_clamps_to_used_repo_count() {
        let (mut state, _, _) = two_repo_state();
        state.filter_cursor = 0;
        state.filter_cursor_right(); // 0 → 1
        assert_eq!(state.filter_cursor, 1);
        state.filter_cursor_right(); // saturates at last
        assert_eq!(state.filter_cursor, 1);
        state.filter_cursor_left(); // 1 → 0
        assert_eq!(state.filter_cursor, 0);
        state.filter_cursor_left(); // saturates at 0
        assert_eq!(state.filter_cursor, 0);
    }

    #[test]
    fn enter_task_focus_picks_first_visible_task() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::OptionsBar;
        // Collapse repo-a so only repo-b's task is visible.
        state.collapsed_repos.insert("/repo-a".into());
        state.recompute_visible_task_indices();
        state.enter_task_focus();
        match state.focus {
            ScheduleFocus::Task(idx) => {
                assert_eq!(state.tasks[idx].repo_path, "/repo-b");
            }
            other => panic!("expected Task focus, got {:?}", other),
        }
    }

    #[test]
    fn handle_key_on_bar_cycles_filter_cursor() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::OptionsBar;
        state.filter_cursor = 0;
        let _ = state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(state.filter_cursor, 1);
        let _ = state.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(state.filter_cursor, 0);
    }

    #[test]
    fn handle_key_on_bar_enter_toggles_filter() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::OptionsBar;
        state.filter_cursor = 0; // repo-a
        let _ = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(state.collapsed_repos.contains("/repo-a"));
        // Pressing Enter again un-collapses.
        let _ = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!state.collapsed_repos.contains("/repo-a"));
    }

    #[test]
    fn handle_key_on_bar_shift_down_enters_task_focus() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::OptionsBar;
        let _ = state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(matches!(state.focus, ScheduleFocus::Task(_)));
    }

    #[test]
    fn handle_key_shift_up_returns_focus_to_bar() {
        let (mut state, _, _) = two_repo_state();
        state.focus = ScheduleFocus::Task(0);
        let _ = state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(state.focus, ScheduleFocus::OptionsBar);
    }

    #[test]
    fn focus_task_index_uncollapses_hidden_repo() {
        let (mut state, _, _) = two_repo_state();
        state.collapsed_repos.insert("/repo-a".into());
        state.recompute_visible_task_indices();
        // Find the task in repo-a and click its branch indicator.
        let a_idx = state
            .tasks
            .iter()
            .position(|t| t.repo_path == "/repo-a")
            .unwrap();
        state.focus_task_index(a_idx);
        // Repo-a must be un-collapsed and the task focused.
        assert!(!state.collapsed_repos.contains("/repo-a"));
        assert_eq!(state.focus, ScheduleFocus::Task(a_idx));
    }

    #[test]
    fn collapsed_repos_persist_across_sync() {
        let (mut state, repo_a, repo_b) = two_repo_state();
        state.collapsed_repos.insert("/repo-a".into());
        // Re-sync with the same task set.
        let mut t1 = task(
            "1",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t1.repo_path = "/repo-a".into();
        let mut t2 = task(
            "2",
            ScheduledTaskStatus::Inactive,
            ScheduleKind::Unscheduled,
        );
        t2.repo_path = "/repo-b".into();
        state.sync_tasks(vec![t1, t2], &[repo_a, repo_b]);
        // The collapsed-repo set must be preserved across sync.
        assert!(state.collapsed_repos.contains("/repo-a"));
    }
}
