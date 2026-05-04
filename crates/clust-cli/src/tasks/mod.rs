use std::collections::HashMap;
use std::io::Write;

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
pub const SHOW_TERMINAL_PREVIEW: bool = false;

/// Number of terminal output lines shown in active task preview.
pub const TASK_TERMINAL_PREVIEW_LINES: usize = 4;

/// Maximum number of wrapped prompt lines shown in a task box.
const MAX_PROMPT_LINES: usize = 3;

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
    Queued {
        scheduled_at: String,
        batch_id: String,
    },
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
    /// Whether the batch prompt prefix should be applied to this task.
    pub use_prefix: bool,
    /// Whether the batch prompt suffix should be applied to this task.
    pub use_suffix: bool,
    /// Whether this task should run in plan mode (overrides batch default).
    pub plan_mode: bool,
}

/// Batch membership info for an agent displayed in the overview.
pub struct BatchAgentInfo {
    pub batch_title: String,
    pub batch_id: usize,
    pub task_index: usize,
    pub task_count: usize,
}

/// A single batch definition.
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
    /// Hub-assigned persistent batch ID (set after registration).
    pub hub_batch_id: Option<String>,
    /// Hub batch IDs of batches this batch depends on.
    pub depends_on: Vec<String>,
}

impl BatchInfo {
    /// Builds the full prompt for a task by combining the batch prefix,
    /// the task-specific prompt, and the batch suffix, respecting per-task flags.
    pub fn build_prompt(&self, task_prompt: &str, use_prefix: bool, use_suffix: bool) -> String {
        let mut parts = Vec::new();
        if use_prefix {
            if let Some(ref prefix) = self.prompt_prefix {
                parts.push(prefix.as_str());
            }
        }
        parts.push(task_prompt);
        if use_suffix {
            if let Some(ref suffix) = self.prompt_suffix {
                parts.push(suffix.as_str());
            }
        }
        parts.join("\n\n")
    }

    /// Serialize this batch to a JSON string matching the import schema.
    /// `depends_on_titles` should contain resolved batch titles (not hub IDs).
    pub fn to_batch_json(&self, depends_on_titles: &[String]) -> String {
        let tasks: Vec<serde_json::Value> = self
            .tasks
            .iter()
            .map(|t| {
                let mut task_obj = serde_json::json!({
                    "branch": t.branch_name,
                    "prompt": t.prompt,
                });
                if t.plan_mode {
                    task_obj["plan_mode"] = serde_json::json!(true);
                }
                if !t.use_prefix {
                    task_obj["use_prefix"] = serde_json::json!(false);
                }
                if !t.use_suffix {
                    task_obj["use_suffix"] = serde_json::json!(false);
                }
                task_obj
            })
            .collect();

        let mut obj = serde_json::Map::new();
        obj.insert("title".into(), serde_json::json!(self.title));
        if let Some(ref prefix) = self.prompt_prefix {
            obj.insert("prefix".into(), serde_json::json!(prefix));
        }
        if let Some(ref suffix) = self.prompt_suffix {
            obj.insert("suffix".into(), serde_json::json!(suffix));
        }
        let mode = match self.launch_mode {
            LaunchMode::Auto => "auto",
            LaunchMode::Manual => "manual",
        };
        obj.insert("launch_mode".into(), serde_json::json!(mode));
        if let Some(mc) = self.max_concurrent {
            obj.insert("max_concurrent".into(), serde_json::json!(mc));
        }
        if self.plan_mode {
            obj.insert("plan_mode".into(), serde_json::json!(true));
        }
        if self.allow_bypass {
            obj.insert("allow_bypass".into(), serde_json::json!(true));
        }
        if !depends_on_titles.is_empty() {
            obj.insert("depends_on".into(), serde_json::json!(depends_on_titles));
        }
        obj.insert("tasks".into(), serde_json::json!(tasks));

        serde_json::to_string_pretty(&obj).unwrap_or_default()
    }
}

/// Copy text to the system clipboard. Returns Ok on success.
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to run pbcopy: {e}"))?;
        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("Failed to write to pbcopy: {e}"))?;
        }
        child.wait().map_err(|e| format!("pbcopy failed: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Try xclip first, then xsel
        let tool = if which::which("xclip").is_ok() {
            "xclip"
        } else if which::which("xsel").is_ok() {
            "xsel"
        } else {
            return Err("No clipboard tool found (install xclip or xsel)".to_string());
        };

        let args: &[&str] = if tool == "xclip" {
            &["-selection", "clipboard"]
        } else {
            &["--clipboard", "--input"]
        };

        let mut child = std::process::Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to run {tool}: {e}"))?;
        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| format!("Failed to write to {tool}: {e}"))?;
        }
        child.wait().map_err(|e| format!("{tool} failed: {e}"))?;
        Ok(())
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
    /// Vertical scroll offset for task boxes within the focused batch card.
    pub task_scroll_offset: usize,
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
            task_scroll_offset: 0,
            next_id: 1,
            next_auto_name: 1,
        }
    }

    /// Build a mapping from agent_id to batch membership info.
    pub fn batch_agent_map(&self) -> HashMap<String, BatchAgentInfo> {
        let mut map = HashMap::new();
        for batch in &self.batches {
            let task_count = batch.tasks.len();
            for (i, task) in batch.tasks.iter().enumerate() {
                if let Some(ref agent_id) = task.agent_id {
                    map.insert(
                        agent_id.clone(),
                        BatchAgentInfo {
                            batch_title: batch.title.clone(),
                            batch_id: batch.id,
                            task_index: i,
                            task_count,
                        },
                    );
                }
            }
        }
        map
    }

    pub fn add_batch(&mut self, output: BatchModalOutput) -> usize {
        let title = output.title.unwrap_or_else(|| {
            let name = format!("Batch {}", self.next_auto_name);
            self.next_auto_name += 1;
            name
        });
        let idx = self.batches.len();
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
            hub_batch_id: None,
            depends_on: Vec::new(),
        });
        self.next_id += 1;
        idx
    }

    pub fn add_task(
        &mut self,
        batch_idx: usize,
        branch_name: String,
        prompt: String,
        use_prefix: bool,
        use_suffix: bool,
        plan_mode: bool,
    ) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            batch.tasks.push(TaskEntry {
                branch_name,
                prompt,
                status: TaskStatus::Idle,
                agent_id: None,
                use_prefix,
                use_suffix,
                plan_mode,
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
    pub fn start_single_task(
        &mut self,
        batch_idx: usize,
        task_idx: usize,
    ) -> Option<BatchStartInfo> {
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

    pub fn toggle_task_use_prefix(&mut self, batch_idx: usize, task_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            if let Some(task) = batch.tasks.get_mut(task_idx) {
                task.use_prefix = !task.use_prefix;
            }
        }
    }

    pub fn toggle_task_use_suffix(&mut self, batch_idx: usize, task_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            if let Some(task) = batch.tasks.get_mut(task_idx) {
                task.use_suffix = !task.use_suffix;
            }
        }
    }

    pub fn toggle_task_plan_mode(&mut self, batch_idx: usize, task_idx: usize) {
        if let Some(batch) = self.batches.get_mut(batch_idx) {
            if let Some(task) = batch.tasks.get_mut(task_idx) {
                task.plan_mode = !task.plan_mode;
            }
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
            b.tasks
                .iter()
                .any(|t| t.agent_id.as_deref() == Some(agent_id))
        })?;

        if let Some(task) = batch
            .tasks
            .iter_mut()
            .find(|t| t.agent_id.as_deref() == Some(agent_id))
        {
            task.status = TaskStatus::Done;
        }

        if !matches!(batch.status, BatchStatus::Active) {
            return None;
        }

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

    /// Find the index of a batch by its hub-assigned ID.
    #[allow(dead_code)]
    pub fn batch_idx_by_hub_id(&self, hub_id: &str) -> Option<usize> {
        self.batches
            .iter()
            .position(|b| b.hub_batch_id.as_deref() == Some(hub_id))
    }

    /// Load batches from hub info, replacing current state.
    /// Called on CLI startup to restore persisted batches.
    pub fn load_from_hub(&mut self, hub_batches: Vec<clust_ipc::QueuedBatchInfo>) {
        self.batches.clear();
        self.next_id = 1;
        self.next_auto_name = 1;
        self.focus = TasksFocus::BatchList;
        self.focused_task = None;
        self.scroll_offset = 0;

        for info in hub_batches {
            let launch_mode = if info.launch_mode == "manual" {
                LaunchMode::Manual
            } else {
                LaunchMode::Auto
            };

            let status = match info.status.as_str() {
                "running" => BatchStatus::Active,
                "scheduled" => BatchStatus::Queued {
                    scheduled_at: info.scheduled_at.clone().unwrap_or_default(),
                    batch_id: info.batch_id.clone(),
                },
                _ => BatchStatus::Idle,
            };

            let tasks: Vec<TaskEntry> = info
                .tasks
                .iter()
                .map(|t| TaskEntry {
                    branch_name: t.branch_name.clone(),
                    prompt: t.prompt.clone(),
                    status: match t.status.as_str() {
                        "active" => TaskStatus::Active,
                        "done" => TaskStatus::Done,
                        _ => TaskStatus::Idle,
                    },
                    agent_id: t.agent_id.clone(),
                    use_prefix: t.use_prefix,
                    use_suffix: t.use_suffix,
                    plan_mode: t.plan_mode,
                })
                .collect();

            // Derive repo_name from repo_path
            let repo_name = std::path::Path::new(&info.repo_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| info.repo_path.clone());

            // Track auto-name counter
            if info.title.starts_with("Batch ") {
                if let Ok(n) = info.title[6..].parse::<usize>() {
                    if n >= self.next_auto_name {
                        self.next_auto_name = n + 1;
                    }
                }
            }

            self.batches.push(BatchInfo {
                id: self.next_id,
                title: info.title,
                repo_path: info.repo_path,
                repo_name,
                branch_name: info.target_branch,
                max_concurrent: info.max_concurrent,
                launch_mode,
                prompt_prefix: info.prompt_prefix,
                prompt_suffix: info.prompt_suffix,
                tasks,
                status,
                plan_mode: info.plan_mode,
                allow_bypass: info.allow_bypass,
                hub_batch_id: Some(info.batch_id),
                depends_on: info.depends_on,
            });
            self.next_id += 1;
        }
    }

    /// Sync task statuses and agent_ids from hub data for all batches.
    pub fn sync_from_hub(&mut self, hub_batches: &[clust_ipc::QueuedBatchInfo]) {
        for hub_info in hub_batches {
            let batch = match self
                .batches
                .iter_mut()
                .find(|b| b.hub_batch_id.as_deref() == Some(&hub_info.batch_id))
            {
                Some(b) => b,
                None => continue,
            };

            // Update batch status
            match hub_info.status.as_str() {
                "running" if !matches!(batch.status, BatchStatus::Active) => {
                    batch.status = BatchStatus::Active;
                }
                // Keep the Queued state with its scheduled_at
                "scheduled" if !matches!(batch.status, BatchStatus::Queued { .. }) => {
                    batch.status = BatchStatus::Queued {
                        scheduled_at: hub_info.scheduled_at.clone().unwrap_or_default(),
                        batch_id: hub_info.batch_id.clone(),
                    };
                }
                "idle" if !matches!(batch.status, BatchStatus::Idle) => {
                    batch.status = BatchStatus::Idle;
                }
                _ => {}
            }

            // Sync per-task status and agent_id
            for (i, hub_task) in hub_info.tasks.iter().enumerate() {
                if let Some(task) = batch.tasks.get_mut(i) {
                    let new_status = match hub_task.status.as_str() {
                        "active" => TaskStatus::Active,
                        "done" => TaskStatus::Done,
                        _ => TaskStatus::Idle,
                    };
                    task.status = new_status;
                    if hub_task.agent_id.is_some() {
                        task.agent_id = hub_task.agent_id.clone();
                    }
                }
            }

            // Add any new tasks from hub that we don't have locally
            if hub_info.tasks.len() > batch.tasks.len() {
                for hub_task in &hub_info.tasks[batch.tasks.len()..] {
                    batch.tasks.push(TaskEntry {
                        branch_name: hub_task.branch_name.clone(),
                        prompt: hub_task.prompt.clone(),
                        status: match hub_task.status.as_str() {
                            "active" => TaskStatus::Active,
                            "done" => TaskStatus::Done,
                            _ => TaskStatus::Idle,
                        },
                        agent_id: hub_task.agent_id.clone(),
                        use_prefix: hub_task.use_prefix,
                        use_suffix: hub_task.use_suffix,
                        plan_mode: hub_task.plan_mode,
                    });
                }
            }

            // Update config
            batch.prompt_prefix = hub_info.prompt_prefix.clone();
            batch.prompt_suffix = hub_info.prompt_suffix.clone();
            batch.plan_mode = hub_info.plan_mode;
            batch.allow_bypass = hub_info.allow_bypass;
            batch.depends_on = hub_info.depends_on.clone();
        }

        // Remove batches that are no longer in the hub (completed/deleted)
        self.batches.retain(|b| {
            match &b.hub_batch_id {
                Some(id) => hub_batches.iter().any(|h| &h.batch_id == id),
                None => true, // Keep unregistered batches
            }
        });
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
            self.task_scroll_offset = 0;
        }
    }

    pub fn focus_next_card(&mut self) {
        if let TasksFocus::BatchCard(idx) = self.focus {
            if idx + 1 < self.batches.len() {
                self.focus = TasksFocus::BatchCard(idx + 1);
                self.task_scroll_offset = 0;
            }
        }
    }

    pub fn focus_prev_card(&mut self) {
        if let TasksFocus::BatchCard(idx) = self.focus {
            self.focused_task = None;
            self.task_scroll_offset = 0;
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

    /// Returns (batch_idx, task_idx, agent_id, batch_title) for the currently
    /// focused task if it is Active and has an agent_id.
    pub fn focused_active_agent(&self) -> Option<(usize, usize, &str, &str)> {
        let batch_idx = match self.focus {
            TasksFocus::BatchCard(idx) => idx,
            _ => return None,
        };
        let task_idx = self.focused_task?;
        let batch = self.batches.get(batch_idx)?;
        let task = batch.tasks.get(task_idx)?;
        if task.status != TaskStatus::Active {
            return None;
        }
        let agent_id = task.agent_id.as_deref()?;
        Some((batch_idx, task_idx, agent_id, &batch.title))
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
    let [options_area, cards_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

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
        let scroll = if is_focused {
            &mut state.task_scroll_offset
        } else {
            &mut 0
        };
        // Resolve dependency hub IDs to titles
        let dep_titles: Vec<&str> = batch
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                state
                    .batches
                    .iter()
                    .find(|b| b.hub_batch_id.as_deref() == Some(dep_id))
                    .map(|b| b.title.as_str())
            })
            .collect();
        render_batch_card(
            frame,
            card_areas[i],
            batch,
            is_focused,
            repo_color,
            ft,
            terminal_previews,
            scroll,
            &dep_titles,
        );

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

    let mod_key = if cfg!(target_os = "macos") {
        "Opt"
    } else {
        "Alt"
    };

    let line = Line::from(vec![
        Span::styled(" ", Style::default().bg(theme::R_BG_RAISED)),
        Span::styled(
            count_text,
            Style::default()
                .fg(theme::R_TEXT_SECONDARY)
                .bg(theme::R_BG_RAISED),
        ),
        Span::styled(
            format!("  {mod_key}+T create  {mod_key}+I import  Space toggle  T timer  {mod_key}+S start  M mode  B bypass  D deps  d clear done"),
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
    let mod_key = if cfg!(target_os = "macos") {
        "Opt"
    } else {
        "Alt"
    };
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

#[allow(clippy::too_many_arguments)]
fn render_batch_card(
    frame: &mut Frame,
    area: Rect,
    batch: &BatchInfo,
    focused: bool,
    repo_color: Option<ratatui::style::Color>,
    focused_task: Option<usize>,
    terminal_previews: &TerminalPreviewMap,
    task_scroll_offset: &mut usize,
    dep_titles: &[&str],
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

    // The batch top part is "selected" when the card is focused but no task is highlighted
    let batch_top_selected = focused && focused_task.is_none();

    let label_style = Style::default().fg(if batch_top_selected {
        theme::R_TEXT_SECONDARY
    } else {
        theme::R_TEXT_TERTIARY
    });
    let value_style = Style::default().fg(if batch_top_selected {
        theme::R_TEXT_PRIMARY
    } else {
        theme::R_TEXT_SECONDARY
    });

    // Selection indicator prefix: "> " on first line, "  " on subsequent lines
    let indicator_span = if batch_top_selected {
        Span::styled(
            "> ",
            Style::default()
                .fg(theme::R_ACCENT_BRIGHT)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  ", Style::default())
    };
    let pad_span = Span::styled("  ", Style::default());

    let status_span = if batch.launch_mode == LaunchMode::Manual {
        Span::styled("Manual", Style::default().fg(theme::R_INFO))
    } else {
        match &batch.status {
            BatchStatus::Idle => Span::styled("Idle", Style::default().fg(theme::R_TEXT_DISABLED)),
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
            indicator_span.clone(),
            Span::styled("Repo      ", label_style),
            Span::styled(&batch.repo_name, Style::default().fg(repo_color_val)),
        ]),
        Line::from(vec![
            pad_span.clone(),
            Span::styled("Branch    ", label_style),
            Span::styled(&batch.branch_name, value_style),
        ]),
        if batch.launch_mode == LaunchMode::Auto {
            Line::from(vec![
                pad_span.clone(),
                Span::styled("Workers   ", label_style),
                Span::styled(concurrency_text, value_style),
            ])
        } else {
            Line::from(vec![
                pad_span.clone(),
                Span::styled("Mode      ", label_style),
                Span::styled("Manual", Style::default().fg(theme::R_INFO)),
            ])
        },
        Line::from(vec![
            pad_span.clone(),
            Span::styled("Tasks     ", label_style),
            Span::styled(batch.tasks.len().to_string(), value_style),
        ]),
        Line::from(vec![
            pad_span.clone(),
            Span::styled("Prefix    ", label_style),
            Span::styled(
                batch.prompt_prefix.as_deref().unwrap_or("(none)"),
                if batch.prompt_prefix.is_some() {
                    value_style
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ),
        ]),
        Line::from(vec![
            pad_span.clone(),
            Span::styled("Suffix    ", label_style),
            Span::styled(
                batch.prompt_suffix.as_deref().unwrap_or("(none)"),
                if batch.prompt_suffix.is_some() {
                    value_style
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ),
        ]),
        Line::from(vec![
            pad_span.clone(),
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
            pad_span.clone(),
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
            pad_span,
            Span::styled("Status    ", label_style),
            status_span,
        ]),
        Line::from(vec![
            Span::styled("Depends   ", label_style),
            if dep_titles.is_empty() {
                Span::styled("(none)", Style::default().fg(theme::R_TEXT_DISABLED))
            } else {
                Span::styled(dep_titles.join(", "), Style::default().fg(theme::R_INFO))
            },
        ]),
    ];

    // Split inner area: metadata on top, task boxes below
    let metadata_height = metadata_lines.len() as u16 + 1; // +1 for blank separator
    let [metadata_area, tasks_area] =
        Layout::vertical([Constraint::Length(metadata_height), Constraint::Min(0)]).areas(inner);

    frame.render_widget(Paragraph::new(metadata_lines), metadata_area);

    if !batch.tasks.is_empty() {
        render_task_boxes(
            frame,
            tasks_area,
            batch,
            focused_task,
            terminal_previews,
            task_scroll_offset,
        );
    }
}

// ---------------------------------------------------------------------------
// Task box rendering
// ---------------------------------------------------------------------------

/// Count how many visual lines a prompt occupies when wrapped to `width` chars,
/// capped at `MAX_PROMPT_LINES`.
fn wrapped_prompt_line_count(prompt: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut count = 0usize;
    for line in prompt.lines() {
        let len = line.chars().count();
        if len == 0 {
            count += 1;
        } else {
            count += len.div_ceil(width);
        }
        if count >= MAX_PROMPT_LINES {
            return MAX_PROMPT_LINES;
        }
    }
    count.max(1)
}

/// Build the wrapped prompt lines for display, capped at `MAX_PROMPT_LINES`.
/// Adds an ellipsis to the last line if the prompt was truncated.
fn wrap_prompt_text(prompt: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    let mut exhausted = true;

    'outer: for line in prompt.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            result.push(String::new());
            if result.len() >= MAX_PROMPT_LINES {
                exhausted = false;
                break;
            }
            continue;
        }
        for chunk in chars.chunks(width) {
            result.push(chunk.iter().collect());
            if result.len() >= MAX_PROMPT_LINES {
                // Check if there's more text after this chunk
                exhausted = false;
                break 'outer;
            }
        }
    }

    // Check if we consumed everything
    if exhausted {
        // Verify no remaining lines
        let total_chars: usize = prompt
            .lines()
            .map(|l| l.chars().count().max(1))
            .sum::<usize>();
        let rendered_chars: usize = result
            .iter()
            .map(|l| l.chars().count().max(1))
            .sum::<usize>();
        if rendered_chars < total_chars {
            exhausted = false;
        }
    }

    if !exhausted {
        // Add ellipsis to the last line
        if let Some(last) = result.last_mut() {
            let last_chars: Vec<char> = last.chars().collect();
            if last_chars.len() >= width {
                // Replace last char with ellipsis
                let truncated: String = last_chars[..width - 1].iter().collect();
                *last = format!("{truncated}\u{2026}");
            } else {
                last.push('\u{2026}');
            }
        }
    }

    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Height of a single task box based on its status, prompt length, and available preview data.
fn task_box_height(task: &TaskEntry, terminal_previews: &TerminalPreviewMap, width: u16) -> u16 {
    // content_width = area.width - 2 (1px padding each side)
    let content_width = width.saturating_sub(2) as usize;
    let prompt_lines = wrapped_prompt_line_count(&task.prompt, content_width) as u16;
    // separator(1) + header(1) + prompt_lines + status(1)
    let base = 2 + prompt_lines + 1;

    if task.status == TaskStatus::Active && SHOW_TERMINAL_PREVIEW {
        let has_preview = task
            .agent_id
            .as_ref()
            .and_then(|id| terminal_previews.get(id))
            .is_some_and(|lines| !lines.is_empty());
        if has_preview {
            return base + TASK_BOX_PREVIEW_HEIGHT;
        }
    }
    base
}

/// Render task boxes vertically within the given area, sorted by status,
/// with vertical scrolling when tasks overflow.
fn render_task_boxes(
    frame: &mut Frame,
    area: Rect,
    batch: &BatchInfo,
    focused_task: Option<usize>,
    terminal_previews: &TerminalPreviewMap,
    task_scroll_offset: &mut usize,
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

    // Compute heights for all sorted tasks
    let heights: Vec<u16> = sorted_indices
        .iter()
        .map(|&idx| task_box_height(&batch.tasks[idx], terminal_previews, area.width))
        .collect();

    // Auto-scroll to keep focused task visible
    if let Some(ft) = focused_task {
        if let Some(focused_sorted_pos) = sorted_indices.iter().position(|&idx| idx == ft) {
            // Scroll up if focused task is above viewport
            if focused_sorted_pos < *task_scroll_offset {
                *task_scroll_offset = focused_sorted_pos;
            }

            // Scroll down if focused task is below viewport
            let mut running = 0u16;
            // Find last visible from current scroll
            let mut last_visible = *task_scroll_offset;
            for (i, &h) in heights.iter().enumerate().skip(*task_scroll_offset) {
                if running + h > area.height {
                    break;
                }
                running += h;
                last_visible = i;
            }
            if focused_sorted_pos > last_visible {
                // Scroll so focused task is the last visible
                let mut h = 0u16;
                let mut new_start = focused_sorted_pos;
                for i in (0..=focused_sorted_pos).rev() {
                    if h + heights[i] > area.height {
                        break;
                    }
                    h += heights[i];
                    new_start = i;
                }
                *task_scroll_offset = new_start;
            }
        }
    }

    // Clamp scroll offset
    *task_scroll_offset = (*task_scroll_offset).min(sorted_indices.len().saturating_sub(1));

    // Calculate which task boxes fit starting from scroll offset
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut total_height: u16 = 0;
    let mut visible_count = 0;

    for &h in heights.iter().skip(*task_scroll_offset) {
        if total_height + h > area.height {
            break;
        }
        constraints.push(Constraint::Length(h));
        total_height += h;
        visible_count += 1;
    }

    if visible_count == 0 {
        // Even one task doesn't fit — show it clipped
        if !sorted_indices.is_empty() {
            constraints.push(Constraint::Length(area.height));
            visible_count = 1;
        } else {
            return;
        }
    }

    // Flexible spacer pushes boxes to top
    if total_height < area.height {
        constraints.push(Constraint::Min(0));
    }

    let box_areas = Layout::vertical(constraints).split(area);

    let has_prefix = batch.prompt_prefix.is_some();
    let has_suffix = batch.prompt_suffix.is_some();

    let visible_slice = &sorted_indices[*task_scroll_offset..*task_scroll_offset + visible_count];
    for (vi, &idx) in visible_slice.iter().enumerate() {
        let task = &batch.tasks[idx];
        let is_focused = focused_task == Some(idx);
        render_single_task_box(
            frame,
            box_areas[vi],
            task,
            idx,
            is_focused,
            has_prefix,
            has_suffix,
            terminal_previews,
        );
    }

    // Scroll indicators
    let has_above = *task_scroll_offset > 0;
    let has_below = *task_scroll_offset + visible_count < sorted_indices.len();
    if has_above {
        let indicator = Span::styled(
            format!(" \u{25b2} {} more above ", *task_scroll_offset),
            Style::default().fg(theme::R_TEXT_TERTIARY),
        );
        frame.render_widget(
            Paragraph::new(Line::from(indicator)).alignment(Alignment::Center),
            Rect { height: 1, ..area },
        );
    }
    if has_below {
        let below_count = sorted_indices.len() - *task_scroll_offset - visible_count;
        let indicator = Span::styled(
            format!(" \u{25bc} {} more below ", below_count),
            Style::default().fg(theme::R_TEXT_TERTIARY),
        );
        let y = area.y + area.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Line::from(indicator)).alignment(Alignment::Center),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// Render a single task as a box with separator, header, prompt, status bar, and optional terminal preview.
#[allow(clippy::too_many_arguments)]
fn render_single_task_box(
    frame: &mut Frame,
    area: Rect,
    task: &TaskEntry,
    original_index: usize,
    is_focused: bool,
    has_prefix: bool,
    has_suffix: bool,
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
    frame.render_widget(Paragraph::new(separator), Rect { height: 1, ..area });

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

    let mut header_spans = vec![
        Span::styled(indicator, indicator_style),
        Span::styled(format!("{}.", original_index + 1), label_style),
        Span::raw(" "),
        Span::styled(status_text, status_style),
        Span::raw(" "),
        Span::styled(branch_display, branch_style),
    ];

    // Show focus-mode hint on focused active tasks
    if is_focused && task.status == TaskStatus::Active && task.agent_id.is_some() {
        header_spans.push(Span::styled(
            "  Shift+\u{2193} focus",
            Style::default().fg(theme::R_ACCENT),
        ));
    }

    let header_line = Line::from(header_spans);

    let mut lines = vec![header_line];

    // Wrapped prompt lines
    let prompt_lines = wrap_prompt_text(&task.prompt, content_area.width as usize);
    for pl in prompt_lines {
        lines.push(Line::from(Span::styled(
            pl,
            Style::default().fg(theme::R_TEXT_TERTIARY),
        )));
    }

    // Status bar: plan mode + prefix/suffix applied indicators
    {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let mut status_spans: Vec<Span> = Vec::new();

        if task.plan_mode {
            status_spans.push(Span::styled(
                "PLAN",
                Style::default()
                    .fg(theme::R_WARNING)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            status_spans.push(Span::styled(
                "Normal",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        status_spans.push(Span::styled("  ", Style::default()));

        if task.use_prefix {
            status_spans.push(Span::styled(
                "\u{2713} Pfx",
                if has_prefix {
                    Style::default().fg(theme::R_SUCCESS)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ));
        } else {
            status_spans.push(Span::styled(
                "\u{2717} Pfx",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        status_spans.push(Span::styled("  ", Style::default()));

        if task.use_suffix {
            status_spans.push(Span::styled(
                "\u{2713} Sfx",
                if has_suffix {
                    Style::default().fg(theme::R_SUCCESS)
                } else {
                    Style::default().fg(theme::R_TEXT_DISABLED)
                },
            ));
        } else {
            status_spans.push(Span::styled(
                "\u{2717} Sfx",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        if is_focused {
            status_spans.push(Span::styled(
                format!("  {mod_key}+P plan  {mod_key}+A/S pfx/sfx"),
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }

        lines.push(Line::from(status_spans));
    }

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
