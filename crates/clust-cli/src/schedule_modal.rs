//! Opt+S "schedule task" modal.
//!
//! Mirrors [`crate::create_agent_modal`] for repo/branch/prompt selection,
//! then branches by schedule kind:
//!
//! - **Schedule** — final step is a free-text time/duration entry parsed by
//!   [`parse_time`].
//! - **Depend** — final step is a multi-select list of every existing
//!   scheduled task across all repos. Inactive tasks that will create a new
//!   branch surface as **FUTURE** so a chain can be wired before the upstream
//!   branch physically exists.
//! - **Unscheduled** — submits immediately after the prompt step.
//!
//! Under **Depend**, the SelectBranch step also offers *future* branches —
//! branches that don't exist yet but will be created by an Inactive scheduled
//! task in the same repo. Picking one records the upstream task id and
//! auto-injects it into `depends_on_ids` at submission so the new task waits
//! for its base to actually exist before firing.
//!
//! Differs from Opt+E in two ways: the prompt cannot be empty, and the modal
//! exposes an `Auto Exit` toggle (Alt+X) in addition to plan-mode (Alt+P).

use chrono::{Duration as ChronoDuration, Local, NaiveTime, TimeZone, Utc};
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

use clust_ipc::{
    AgentInfo, BranchInfo, RepoInfo, ScheduleKind, ScheduledTaskInfo, ScheduledTaskStatus,
};

use crate::theme;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScheduleModalStep {
    SelectRepo,
    SelectScheduleKind,
    SelectBranch,
    NewBranch,
    EnterPrompt,
    EnterStartTime, // only for Schedule kind
    SelectDependencies, // only for Depend kind
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleKindChoice {
    Schedule,
    Depend,
    Unscheduled,
}

impl ScheduleKindChoice {
    fn label(self) -> &'static str {
        match self {
            Self::Schedule => "Schedule (start at time/duration)"
            ,
            Self::Depend => "Depend (start when other tasks complete)",
            Self::Unscheduled => "Unscheduled (start manually)",
        }
    }
}

#[derive(Debug)]
pub enum ScheduleModalResult {
    Pending,
    Cancelled,
    Completed(ScheduleModalOutput),
}

#[derive(Debug, Clone)]
pub struct ScheduleModalOutput {
    pub repo_path: String,
    pub base_branch: Option<String>,
    pub new_branch: Option<String>,
    pub prompt: String,
    pub plan_mode: bool,
    pub auto_exit: bool,
    pub schedule: ScheduleKind,
    /// Running worktree-agent IDs picked alongside scheduled-task deps in the
    /// dependency step. The hub promotes each one to a shadow scheduled-task
    /// row before persisting the new task.
    pub extra_agent_deps: Vec<String>,
    /// Set when the modal was opened in reschedule mode — the dispatcher
    /// sends `RescheduleScheduledTask { id }` instead of `CreateScheduledTask`.
    pub reschedule_task_id: Option<String>,
}

/// What the modal was opened to do. `Create` is the original Opt+S flow that
/// walks the user through repo/branch/prompt selection; `Reschedule` keeps the
/// original task's repo, branch, and prompt and lets the user only pick a new
/// trigger.
#[derive(Debug, Clone)]
enum ScheduleModalMode {
    Create,
    /// `task_id` is sent back in the output so the dispatcher can target the
    /// existing row.
    Reschedule { task_id: String },
}

/// One row in the dep picker — either an existing scheduled task or a
/// currently-running Opt+E worktree agent that hasn't been promoted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepKind {
    Task,
    Agent,
}

/// Rank used to sort the dep picker so the most chain-relevant entries float
/// to the top. Lower = earlier in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DepRank {
    Future,  // Inactive task that will create a new branch
    Pending, // Inactive task reusing an existing branch
    Running, // Active scheduled task
    Agent,   // Running Opt+E worktree agent (not yet promoted)
    Done,    // Complete task
    Aborted, // Aborted task
}

/// One-word status label for a scheduled task, surfacing "FUTURE" for the
/// Inactive+new_branch case so the dep picker reads as a chain of future
/// branches rather than a list of completed records.
fn task_status_label(task: &ScheduledTaskInfo) -> &'static str {
    match (task.status, task.new_branch.is_some()) {
        (ScheduledTaskStatus::Inactive, true) => "FUTURE",
        (ScheduledTaskStatus::Inactive, false) => "PENDING",
        (ScheduledTaskStatus::Active, _) => "RUNNING",
        (ScheduledTaskStatus::Complete, _) => "DONE",
        (ScheduledTaskStatus::Aborted, _) => "ABORTED",
    }
}

fn task_dep_rank(task: &ScheduledTaskInfo) -> DepRank {
    match (task.status, task.new_branch.is_some()) {
        (ScheduledTaskStatus::Inactive, true) => DepRank::Future,
        (ScheduledTaskStatus::Inactive, false) => DepRank::Pending,
        (ScheduledTaskStatus::Active, _) => DepRank::Running,
        (ScheduledTaskStatus::Complete, _) => DepRank::Done,
        (ScheduledTaskStatus::Aborted, _) => DepRank::Aborted,
    }
}

/// Tagged ID stored in `selected_deps` so submission can split the picks back
/// into scheduled-task IDs and agent IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DepSelection {
    kind: DepKind,
    id: String,
}

/// One choice in the SelectBranch step: either an existing local branch in the
/// repo, or a *future* branch that will be created when an upstream Inactive
/// scheduled task fires. Picking a future branch records the upstream task id
/// so it can be auto-injected into `depends_on_ids` at submission, ensuring
/// the new task waits for its base to actually exist.
#[derive(Debug, Clone)]
enum BranchChoice {
    Existing(BranchInfo),
    Future {
        branch_name: String,
        upstream_task_id: String,
    },
}

impl BranchChoice {
    fn name(&self) -> &str {
        match self {
            Self::Existing(b) => b.name.as_str(),
            Self::Future { branch_name, .. } => branch_name.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct ScheduleTaskModal {
    step: ScheduleModalStep,
    input: String,
    cursor_pos: usize,
    selected_idx: usize,

    repos: Vec<RepoInfo>,
    /// Choices shown in the SelectBranch step. Populated after the schedule
    /// kind is chosen so future branches can be conditionally included only
    /// when the kind is Depend (where the upstream task can be auto-added as
    /// a dep to make the chain actually wait for the branch to exist).
    branches: Vec<BranchChoice>,
    /// Snapshot of every existing scheduled task, used by the Depend step.
    /// Filtered fuzzily by `input` while on that step.
    all_tasks: Vec<ScheduledTaskInfo>,
    /// Snapshot of running Opt+E worktree agents that don't yet have a shadow
    /// scheduled-task row. Surfaced alongside `all_tasks` in the dep picker
    /// so a manually spawned agent can be selected as a dependency.
    candidate_agents: Vec<AgentInfo>,
    /// Picked dependencies (tagged with kind so submission can split them)
    /// accumulated by Space-toggling.
    selected_deps: Vec<DepSelection>,

    selected_repo: Option<RepoInfo>,
    schedule_kind: Option<ScheduleKindChoice>,
    target_branch: Option<String>,
    /// Set when the user picked a future branch as the base. The upstream
    /// task id is auto-included in `depends_on_ids` at submission so the new
    /// task waits for the branch to be created before firing.
    implicit_upstream_task_id: Option<String>,
    new_branch_name: Option<String>,
    new_branch_required: bool,
    /// Last time-parse error message; cleared whenever input changes.
    time_error: Option<String>,

    plan_mode: bool,
    auto_exit: bool,
    /// Prompt captured at the EnterPrompt step. Stored separately so the
    /// EnterStartTime / SelectDependencies steps can reuse the input field for
    /// their own value without losing the user's prompt.
    prompt_value: String,
    /// Create vs reschedule. In reschedule mode the modal jumps straight into
    /// the SelectScheduleKind step with repo/branch/prompt pre-populated from
    /// the existing task and never visits the repo/branch/prompt steps.
    mode: ScheduleModalMode,
    matcher: SkimMatcherV2,
}

impl ScheduleTaskModal {
    pub fn new(
        repos: Vec<RepoInfo>,
        all_tasks: Vec<ScheduledTaskInfo>,
        running_agents: Vec<AgentInfo>,
    ) -> Self {
        // Only worktree agents with a known repo + branch are useful as deps:
        // we need those fields to materialize a shadow scheduled-task row, and
        // the dep picker labels rows as `repo / branch`.
        // Drop any agent that already has an `active` shadow row to avoid
        // listing it twice (once as Task, once as Agent).
        let already_promoted: std::collections::HashSet<&str> = all_tasks
            .iter()
            .filter(|t| t.status == ScheduledTaskStatus::Active)
            .filter_map(|t| t.agent_id.as_deref())
            .collect();
        let candidate_agents: Vec<AgentInfo> = running_agents
            .into_iter()
            .filter(|a| {
                a.is_worktree
                    && a.repo_path.is_some()
                    && a.branch_name.is_some()
                    && !already_promoted.contains(a.id.as_str())
            })
            .collect();
        Self {
            step: ScheduleModalStep::SelectRepo,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            all_tasks,
            candidate_agents,
            selected_deps: Vec::new(),
            selected_repo: None,
            schedule_kind: None,
            target_branch: None,
            implicit_upstream_task_id: None,
            new_branch_name: None,
            new_branch_required: false,
            time_error: None,
            plan_mode: false,
            auto_exit: false,
            prompt_value: String::new(),
            mode: ScheduleModalMode::Create,
            matcher: SkimMatcherV2::default(),
        }
    }

    /// Open the modal in reschedule mode. The repo, branch, prompt, and the
    /// plan/auto-exit flags are taken from the existing task; the user can
    /// only change the schedule kind and its associated start_at / dep set.
    /// The first step shown is [`ScheduleModalStep::SelectScheduleKind`].
    pub fn new_reschedule(
        task: &ScheduledTaskInfo,
        repos: Vec<RepoInfo>,
        all_tasks: Vec<ScheduledTaskInfo>,
        running_agents: Vec<AgentInfo>,
    ) -> Self {
        let already_promoted: std::collections::HashSet<&str> = all_tasks
            .iter()
            .filter(|t| t.status == ScheduledTaskStatus::Active)
            .filter_map(|t| t.agent_id.as_deref())
            .collect();
        let candidate_agents: Vec<AgentInfo> = running_agents
            .into_iter()
            .filter(|a| {
                a.is_worktree
                    && a.repo_path.is_some()
                    && a.branch_name.is_some()
                    && !already_promoted.contains(a.id.as_str())
            })
            .collect();
        // Hide the rescheduled task itself from the dep picker — a Depend on
        // self would deadlock.
        let mut all_tasks = all_tasks;
        all_tasks.retain(|t| t.id != task.id);

        let selected_repo = repos.iter().find(|r| r.path == task.repo_path).cloned();
        Self {
            step: ScheduleModalStep::SelectScheduleKind,
            input: String::new(),
            cursor_pos: 0,
            selected_idx: 0,
            repos,
            branches: Vec::new(),
            all_tasks,
            candidate_agents,
            selected_deps: Vec::new(),
            selected_repo,
            schedule_kind: None,
            target_branch: task.base_branch.clone(),
            new_branch_name: task.new_branch.clone(),
            new_branch_required: false,
            time_error: None,
            plan_mode: task.plan_mode,
            auto_exit: task.auto_exit,
            prompt_value: task.prompt.clone(),
            mode: ScheduleModalMode::Reschedule {
                task_id: task.id.clone(),
            },
            implicit_upstream_task_id: None,
            matcher: SkimMatcherV2::default(),
        }
    }

    fn is_reschedule(&self) -> bool {
        matches!(self.mode, ScheduleModalMode::Reschedule { .. })
    }

    #[allow(dead_code)]
    pub fn step(&self) -> ScheduleModalStep {
        self.step
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ScheduleModalResult {
        match key.code {
            KeyCode::Esc => self.go_back(),
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Up => {
                if self.selected_idx > 0 {
                    self.selected_idx -= 1;
                }
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Down => {
                let max = self.filtered_count().saturating_sub(1);
                if self.selected_idx < max {
                    self.selected_idx += 1;
                }
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Char(' ') if self.step == ScheduleModalStep::SelectDependencies => {
                self.toggle_dep_at_cursor();
                ScheduleModalResult::Pending
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
                self.time_error = None;
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos = self.input[..self.cursor_pos]
                        .char_indices()
                        .next_back()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
                ScheduleModalResult::Pending
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += self.input[self.cursor_pos..]
                        .chars()
                        .next()
                        .unwrap()
                        .len_utf8();
                }
                ScheduleModalResult::Pending
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    // Reschedule keeps the original task's plan/auto-exit
                    // (those have their own dedicated keybinds in the panel),
                    // so the toggles here are no-ops in that mode.
                    if !self.is_reschedule() {
                        if c == 'p' || c == 'P' {
                            self.plan_mode = !self.plan_mode;
                        } else if c == 'x' || c == 'X' {
                            self.auto_exit = !self.auto_exit;
                        }
                    }
                    return ScheduleModalResult::Pending;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return ScheduleModalResult::Pending;
                }
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.selected_idx = 0;
                self.time_error = None;
                self.clamp_selected_idx();
                ScheduleModalResult::Pending
            }
            _ => ScheduleModalResult::Pending,
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
        self.time_error = None;
        self.clamp_selected_idx();
    }

    fn go_back(&mut self) -> ScheduleModalResult {
        match self.step {
            ScheduleModalStep::SelectRepo => return ScheduleModalResult::Cancelled,
            ScheduleModalStep::SelectScheduleKind => {
                if self.is_reschedule() {
                    // Reschedule starts here — Esc cancels straight back.
                    return ScheduleModalResult::Cancelled;
                }
                self.step = ScheduleModalStep::SelectRepo;
                self.selected_repo = None;
                self.branches.clear();
            }
            ScheduleModalStep::SelectBranch => {
                self.step = ScheduleModalStep::SelectScheduleKind;
                self.target_branch = None;
                self.implicit_upstream_task_id = None;
            }
            ScheduleModalStep::NewBranch => {
                if self.new_branch_required {
                    self.step = ScheduleModalStep::SelectScheduleKind;
                } else {
                    self.step = ScheduleModalStep::SelectBranch;
                    self.target_branch = None;
                    self.implicit_upstream_task_id = None;
                }
            }
            ScheduleModalStep::EnterPrompt => {
                self.step = ScheduleModalStep::NewBranch;
                self.new_branch_name = None;
            }
            ScheduleModalStep::EnterStartTime | ScheduleModalStep::SelectDependencies => {
                // In reschedule mode the kind step is the previous one
                // (we never visited the prompt step); in create mode we
                // captured the prompt before advancing here.
                self.step = if self.is_reschedule() {
                    ScheduleModalStep::SelectScheduleKind
                } else {
                    ScheduleModalStep::EnterPrompt
                };
            }
        }
        self.reset_input();
        self.time_error = None;
        ScheduleModalResult::Pending
    }

    /// Build the branch picker for the selected repo and chosen schedule kind.
    /// Existing local branches always show; *future* branches (from Inactive
    /// scheduled tasks with `new_branch` set, in the same repo) only show when
    /// `kind == Depend` because that is the only kind where we can wire an
    /// auto-dep on the upstream creator task.
    fn populate_branches(&mut self, kind: ScheduleKindChoice) {
        let mut out: Vec<BranchChoice> = Vec::new();
        let Some(repo) = &self.selected_repo else {
            self.branches = out;
            return;
        };
        for b in &repo.local_branches {
            out.push(BranchChoice::Existing(b.clone()));
        }
        if matches!(kind, ScheduleKindChoice::Depend) {
            let existing: std::collections::HashSet<&str> = repo
                .local_branches
                .iter()
                .map(|b| b.name.as_str())
                .collect();
            // Track future branches we've already added so two Inactive tasks
            // racing to create the same branch name don't both surface.
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for t in &self.all_tasks {
                if t.repo_path != repo.path {
                    continue;
                }
                if t.status != ScheduledTaskStatus::Inactive {
                    continue;
                }
                if t.new_branch.is_none() {
                    continue;
                }
                if existing.contains(t.branch_name.as_str()) {
                    continue;
                }
                if !seen.insert(t.branch_name.clone()) {
                    continue;
                }
                out.push(BranchChoice::Future {
                    branch_name: t.branch_name.clone(),
                    upstream_task_id: t.id.clone(),
                });
            }
        }
        self.branches = out;
    }

    fn handle_enter(&mut self) -> ScheduleModalResult {
        match self.step {
            ScheduleModalStep::SelectRepo => {
                let filtered = self.filtered_repos();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let repo = self.repos[idx].clone();
                    self.selected_repo = Some(repo);
                    self.step = ScheduleModalStep::SelectScheduleKind;
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::SelectScheduleKind => {
                let choices = self.kind_choices();
                if let Some(choice) = choices.get(self.selected_idx).copied() {
                    self.schedule_kind = Some(choice);
                    if self.is_reschedule() {
                        // Repo / branch / prompt are all carried over from the
                        // existing task — jump straight to the kind-specific
                        // detail step, or submit immediately for Unscheduled.
                        match choice {
                            ScheduleKindChoice::Schedule => {
                                self.step = ScheduleModalStep::EnterStartTime;
                                self.reset_input();
                                return ScheduleModalResult::Pending;
                            }
                            ScheduleKindChoice::Depend => {
                                self.step = ScheduleModalStep::SelectDependencies;
                                self.reset_input();
                                return ScheduleModalResult::Pending;
                            }
                            ScheduleKindChoice::Unscheduled => {
                                let prompt = self.recover_prompt();
                                return self.complete_with(prompt, ScheduleKind::Unscheduled);
                            }
                        }
                    }
                    self.populate_branches(choice);
                    if self.branches.is_empty() {
                        self.new_branch_required = true;
                        self.step = ScheduleModalStep::NewBranch;
                    } else {
                        self.step = ScheduleModalStep::SelectBranch;
                    }
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::SelectBranch => {
                let filtered = self.filtered_branches();
                if let Some(&(idx, _)) = filtered.get(self.selected_idx) {
                    let choice = self.branches[idx].clone();
                    self.target_branch = Some(choice.name().to_string());
                    self.implicit_upstream_task_id = match choice {
                        BranchChoice::Future { upstream_task_id, .. } => Some(upstream_task_id),
                        BranchChoice::Existing(_) => None,
                    };
                    self.step = ScheduleModalStep::NewBranch;
                    self.reset_input();
                }
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::NewBranch => {
                let sanitized = clust_ipc::branch::sanitize_branch_name(&self.input);
                if self.new_branch_required && sanitized.is_empty() {
                    return ScheduleModalResult::Pending;
                }
                self.new_branch_name = if self.input.trim().is_empty() {
                    None
                } else {
                    Some(sanitized)
                };
                self.step = ScheduleModalStep::EnterPrompt;
                self.reset_input();
                ScheduleModalResult::Pending
            }
            ScheduleModalStep::EnterPrompt => {
                if self.input.trim().is_empty() {
                    // Reject empty prompt — only behavioural delta vs Opt+E.
                    return ScheduleModalResult::Pending;
                }
                self.prompt_value = self.input.clone();
                let prompt = self.prompt_value.clone();
                match self.schedule_kind.unwrap_or(ScheduleKindChoice::Unscheduled) {
                    ScheduleKindChoice::Schedule => {
                        self.step = ScheduleModalStep::EnterStartTime;
                        self.reset_input();
                        ScheduleModalResult::Pending
                    }
                    ScheduleKindChoice::Depend => {
                        self.step = ScheduleModalStep::SelectDependencies;
                        // If the user picked a future branch as the base,
                        // pre-check the upstream task so the chain is visible
                        // in the picker and the user can extend (but not
                        // accidentally drop) it.
                        if let Some(ref upstream_id) = self.implicit_upstream_task_id {
                            let sel = DepSelection {
                                kind: DepKind::Task,
                                id: upstream_id.clone(),
                            };
                            if !self.selected_deps.contains(&sel) {
                                self.selected_deps.push(sel);
                            }
                        }
                        self.reset_input();
                        ScheduleModalResult::Pending
                    }
                    ScheduleKindChoice::Unscheduled => {
                        self.complete_with(prompt, ScheduleKind::Unscheduled)
                    }
                }
            }
            ScheduleModalStep::EnterStartTime => match parse_time(&self.input, Utc::now()) {
                Ok(start_at) => {
                    let prompt = self.recover_prompt();
                    self.complete_with(
                        prompt,
                        ScheduleKind::Time {
                            start_at: start_at.to_rfc3339(),
                        },
                    )
                }
                Err(e) => {
                    self.time_error = Some(e);
                    ScheduleModalResult::Pending
                }
            },
            ScheduleModalStep::SelectDependencies => {
                if self.selected_deps.is_empty() {
                    // Require at least one dep — otherwise the user should
                    // have picked Unscheduled.
                    return ScheduleModalResult::Pending;
                }
                let prompt = self.recover_prompt();
                let mut depends_on_ids: Vec<String> = Vec::new();
                let mut extra_agent_deps: Vec<String> = Vec::new();
                for sel in &self.selected_deps {
                    match sel.kind {
                        DepKind::Task => depends_on_ids.push(sel.id.clone()),
                        DepKind::Agent => extra_agent_deps.push(sel.id.clone()),
                    }
                }
                self.complete_with_deps(
                    prompt,
                    ScheduleKind::Depend { depends_on_ids },
                    extra_agent_deps,
                )
            }
        }
    }

    fn recover_prompt(&self) -> String {
        self.prompt_value.clone()
    }

    fn complete_with(&mut self, prompt: String, schedule: ScheduleKind) -> ScheduleModalResult {
        self.complete_with_deps(prompt, schedule, Vec::new())
    }

    fn complete_with_deps(
        &mut self,
        prompt: String,
        schedule: ScheduleKind,
        extra_agent_deps: Vec<String>,
    ) -> ScheduleModalResult {
        let repo = self
            .selected_repo
            .as_ref()
            .expect("repo set before completion");
        let reschedule_task_id = match &self.mode {
            ScheduleModalMode::Reschedule { task_id } => Some(task_id.clone()),
            ScheduleModalMode::Create => None,
        };
        // When the user picked a future branch as the base, the upstream task
        // MUST end up in `depends_on_ids` regardless of what they did in the
        // dep picker — otherwise the new task could fire before the branch
        // exists. The picker pre-selects it but the user can untoggle, so we
        // also re-inject here as a safety net.
        let schedule = match (&self.implicit_upstream_task_id, schedule) {
            (Some(upstream_id), ScheduleKind::Depend { mut depends_on_ids }) => {
                if !depends_on_ids.iter().any(|id| id == upstream_id) {
                    depends_on_ids.push(upstream_id.clone());
                }
                ScheduleKind::Depend { depends_on_ids }
            }
            (_, schedule) => schedule,
        };
        ScheduleModalResult::Completed(ScheduleModalOutput {
            repo_path: repo.path.clone(),
            base_branch: self.target_branch.clone(),
            new_branch: self.new_branch_name.clone(),
            prompt,
            plan_mode: self.plan_mode,
            auto_exit: self.auto_exit,
            schedule,
            extra_agent_deps,
            reschedule_task_id,
        })
    }

    fn toggle_dep_at_cursor(&mut self) {
        let filtered = self.filtered_deps();
        if let Some(&(kind, idx, _)) = filtered.get(self.selected_idx) {
            let id = match kind {
                DepKind::Task => self.all_tasks[idx].id.clone(),
                DepKind::Agent => self.candidate_agents[idx].id.clone(),
            };
            let sel = DepSelection { kind, id };
            if let Some(pos) = self.selected_deps.iter().position(|x| *x == sel) {
                self.selected_deps.remove(pos);
            } else {
                self.selected_deps.push(sel);
            }
        }
    }

    fn clamp_selected_idx(&mut self) {
        let len = self.filtered_count();
        if len == 0 {
            self.selected_idx = 0;
        } else {
            self.selected_idx = self.selected_idx.min(len - 1);
        }
    }

    fn reset_input(&mut self) {
        self.input.clear();
        self.cursor_pos = 0;
        self.selected_idx = 0;
    }

    fn kind_choices(&self) -> Vec<ScheduleKindChoice> {
        // Hide Depend only when there's nothing at all to depend on. A
        // running Opt+E worktree agent is just as valid a dependency target
        // as an existing scheduled task.
        if self.all_tasks.is_empty() && self.candidate_agents.is_empty() {
            vec![ScheduleKindChoice::Schedule, ScheduleKindChoice::Unscheduled]
        } else {
            vec![
                ScheduleKindChoice::Schedule,
                ScheduleKindChoice::Depend,
                ScheduleKindChoice::Unscheduled,
            ]
        }
    }

    // -- Filtering --

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
                    .fuzzy_match(branch.name(), &self.input)
                    .map(|score| (i, score))
            })
            .collect();
        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    /// Unified dep-picker view: scheduled tasks and running Opt+E agents.
    ///
    /// When the input is empty, entries are bucketed by `DepRank` so the most
    /// chain-relevant rows (Future first, then Pending, Running, Agents, Done,
    /// Aborted) lead the list. When filtering, fuzzy match score wins — a
    /// matching aborted row should still be reachable.
    fn filtered_deps(&self) -> Vec<(DepKind, usize, i64)> {
        if self.input.is_empty() {
            let mut entries: Vec<(DepRank, DepKind, usize)> = Vec::new();
            for (i, t) in self.all_tasks.iter().enumerate() {
                entries.push((task_dep_rank(t), DepKind::Task, i));
            }
            for (i, _) in self.candidate_agents.iter().enumerate() {
                entries.push((DepRank::Agent, DepKind::Agent, i));
            }
            // Stable sort preserves DB-insert order within a rank bucket.
            entries.sort_by_key(|e| e.0);
            return entries
                .into_iter()
                .map(|(_, kind, idx)| (kind, idx, 0))
                .collect();
        }
        let mut results: Vec<(DepKind, usize, i64)> = Vec::new();
        for (i, t) in self.all_tasks.iter().enumerate() {
            let label = format!("{} {}", t.repo_name, t.branch_name);
            if let Some(score) = self.matcher.fuzzy_match(&label, &self.input) {
                results.push((DepKind::Task, i, score));
            }
        }
        for (i, a) in self.candidate_agents.iter().enumerate() {
            let repo_name = a
                .repo_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let branch = a.branch_name.as_deref().unwrap_or("");
            let label = format!("{} {}", repo_name, branch);
            if let Some(score) = self.matcher.fuzzy_match(&label, &self.input) {
                results.push((DepKind::Agent, i, score));
            }
        }
        results.sort_by_key(|b| std::cmp::Reverse(b.2));
        results
    }

    fn filtered_count(&self) -> usize {
        match self.step {
            ScheduleModalStep::SelectRepo => self.filtered_repos().len(),
            ScheduleModalStep::SelectScheduleKind => self.kind_choices().len(),
            ScheduleModalStep::SelectBranch => self.filtered_branches().len(),
            ScheduleModalStep::SelectDependencies => self.filtered_deps().len(),
            _ => 0,
        }
    }

    // -- Rendering --

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let modal_width = 70u16.min(area.width.saturating_sub(4));
        let modal_height = (area.height * 70 / 100)
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

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::R_ACCENT_DIM))
            .title(Span::styled(
                format!(" {} ", self.step_title()),
                Style::default()
                    .fg(theme::R_ACCENT_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(theme::R_BG_OVERLAY))
            .padding(Padding::new(1, 1, 0, 0));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let is_prompt = self.step == ScheduleModalStep::EnterPrompt;
        let [hint, input, _gap, list, _spacer, status] = Layout::vertical([
            Constraint::Length(1),
            if is_prompt {
                Constraint::Min(3)
            } else {
                Constraint::Length(1)
            },
            if is_prompt {
                Constraint::Length(0)
            } else {
                Constraint::Length(1)
            },
            if is_prompt {
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
            hint,
        );
        self.render_input(frame, input);
        match self.step {
            ScheduleModalStep::SelectRepo => self.render_repo_list(frame, list),
            ScheduleModalStep::SelectScheduleKind => self.render_kind_list(frame, list),
            ScheduleModalStep::SelectBranch => self.render_branch_list(frame, list),
            ScheduleModalStep::SelectDependencies => self.render_deps_list(frame, list),
            ScheduleModalStep::NewBranch
            | ScheduleModalStep::EnterPrompt
            | ScheduleModalStep::EnterStartTime => {}
        }
        self.render_status_bar(frame, status);
    }

    fn step_title(&self) -> String {
        let prefix = if self.is_reschedule() {
            "Reschedule task"
        } else {
            "Schedule task"
        };
        match self.step {
            ScheduleModalStep::SelectRepo => format!("{prefix} — select repository"),
            ScheduleModalStep::SelectScheduleKind => format!("{prefix} — pick when to start"),
            ScheduleModalStep::SelectBranch => {
                if matches!(self.schedule_kind, Some(ScheduleKindChoice::Depend))
                    && self
                        .branches
                        .iter()
                        .any(|b| matches!(b, BranchChoice::Future { .. }))
                {
                    format!("{prefix} — select branch (existing or future)")
                } else {
                    format!("{prefix} — select branch")
                }
            }
            ScheduleModalStep::NewBranch => format!("{prefix} — new branch (Enter to skip)"),
            ScheduleModalStep::EnterPrompt => format!("{prefix} — prompt (required)"),
            ScheduleModalStep::EnterStartTime => format!("{prefix} — start time"),
            ScheduleModalStep::SelectDependencies => {
                format!("{prefix} — pick dependencies (incl. future branches)")
            }
        }
    }

    fn step_hint(&self) -> String {
        match self.step {
            ScheduleModalStep::SelectRepo => "↑/↓ select · Enter confirm · Esc cancel".into(),
            ScheduleModalStep::SelectScheduleKind => "↑/↓ select · Enter confirm".into(),
            ScheduleModalStep::SelectBranch => {
                if matches!(self.schedule_kind, Some(ScheduleKindChoice::Depend))
                    && self
                        .branches
                        .iter()
                        .any(|b| matches!(b, BranchChoice::Future { .. }))
                {
                    "type to filter, Enter pick existing OR future — leave empty to create one"
                        .into()
                } else {
                    "type to filter, Enter pick existing — leave empty to create one".into()
                }
            }
            ScheduleModalStep::NewBranch => {
                if self.new_branch_required {
                    "type the new branch name (required, no existing branches)".into()
                } else {
                    "type to create a new branch — Enter to skip and reuse selected".into()
                }
            }
            ScheduleModalStep::EnterPrompt => "type prompt — must be non-empty".into(),
            ScheduleModalStep::EnterStartTime => match &self.time_error {
                Some(e) => e.clone(),
                None => "examples: 5m · 2h · 1d · 30s · 20:00".into(),
            },
            ScheduleModalStep::SelectDependencies => {
                "Space toggles · Enter confirms (≥1) · FUTURE = will be created by another task"
                    .into()
            }
        }
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let before = &self.input[..self.cursor_pos];
        let (cursor_char, after) = if self.cursor_pos < self.input.len() {
            let len = self.input[self.cursor_pos..]
                .chars()
                .next()
                .unwrap()
                .len_utf8();
            (
                &self.input[self.cursor_pos..self.cursor_pos + len],
                &self.input[self.cursor_pos + len..],
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
            Span::styled(before, Style::default().fg(theme::R_TEXT_PRIMARY)),
            Span::styled(
                cursor_char,
                Style::default()
                    .fg(theme::R_BG_BASE)
                    .bg(theme::R_TEXT_PRIMARY),
            ),
            Span::styled(after, Style::default().fg(theme::R_TEXT_PRIMARY)),
        ]);
        let p = Paragraph::new(line)
            .style(Style::default().bg(theme::R_BG_INPUT))
            .wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    }

    fn render_repo_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_repos();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(idx, _))| {
                let r = &self.repos[idx];
                self.render_simple_item(&r.name, Some(&r.path), i == self.selected_idx)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_kind_list(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = self
            .kind_choices()
            .iter()
            .enumerate()
            .map(|(i, c)| self.render_simple_item(c.label(), None, i == self.selected_idx))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_branch_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_branches();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(idx, _))| {
                let b = &self.branches[idx];
                let secondary = match b {
                    BranchChoice::Existing(_) => None,
                    BranchChoice::Future { .. } => Some("future — created by scheduled task"),
                };
                self.render_simple_item(b.name(), secondary, i == self.selected_idx)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_deps_list(&self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_deps();
        let lines: Vec<Line> = filtered
            .iter()
            .take(area.height as usize)
            .enumerate()
            .map(|(i, &(kind, idx, _))| {
                let cursor = i == self.selected_idx;
                let style = if cursor {
                    Style::default()
                        .fg(theme::R_BG_BASE)
                        .bg(theme::R_ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::R_TEXT_PRIMARY)
                };
                let label = match kind {
                    DepKind::Task => {
                        let task = &self.all_tasks[idx];
                        let checked = self.selected_deps.iter().any(|x| {
                            x.kind == DepKind::Task && x.id == task.id
                        });
                        let mark = if checked { "[x] " } else { "[ ] " };
                        // Surface "future branch" semantics for tasks that
                        // haven't fired yet: an Inactive task with new_branch
                        // set will create that branch when it eventually
                        // fires, so it is a *future* branch to the user even
                        // though no worktree exists yet on disk.
                        let status = task_status_label(task);
                        format!(
                            "{}{} / {} [{}]",
                            mark, task.repo_name, task.branch_name, status
                        )
                    }
                    DepKind::Agent => {
                        let agent = &self.candidate_agents[idx];
                        let checked = self.selected_deps.iter().any(|x| {
                            x.kind == DepKind::Agent && x.id == agent.id
                        });
                        let mark = if checked { "[x] " } else { "[ ] " };
                        let repo_name = agent
                            .repo_path
                            .as_deref()
                            .and_then(|p| std::path::Path::new(p).file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "?".to_string());
                        let branch = agent.branch_name.as_deref().unwrap_or("?");
                        let auto = if agent.auto_exit {
                            " AUTO-EXIT"
                        } else {
                            " no-auto-exit"
                        };
                        format!("{}{} / {} [AGENT{}]", mark, repo_name, branch, auto)
                    }
                };
                Line::from(Span::styled(label, style))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_simple_item(&self, primary: &str, secondary: Option<&str>, selected: bool) -> Line<'_> {
        let primary_style = if selected {
            Style::default()
                .fg(theme::R_BG_BASE)
                .bg(theme::R_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::R_TEXT_PRIMARY)
        };
        let mut spans = vec![Span::styled(format!("  {primary}  "), primary_style)];
        if let Some(sec) = secondary {
            spans.push(Span::styled(
                format!("  {sec}"),
                Style::default().fg(theme::R_TEXT_TERTIARY),
            ));
        }
        Line::from(spans)
    }

    fn render_status_bar(&self, frame: &mut Frame, area: Rect) {
        let mod_key = if cfg!(target_os = "macos") {
            "Opt"
        } else {
            "Alt"
        };
        let mut spans: Vec<Span> = Vec::new();
        spans.push(if self.plan_mode {
            Span::styled(
                "PLAN",
                Style::default()
                    .fg(theme::R_WARNING)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("Plan", Style::default().fg(theme::R_TEXT_DISABLED))
        });
        spans.push(Span::styled("  ", Style::default()));
        spans.push(if self.auto_exit {
            Span::styled(
                "AUTO-EXIT",
                Style::default()
                    .fg(theme::R_INFO)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("Auto-Exit", Style::default().fg(theme::R_TEXT_DISABLED))
        });
        if self.is_reschedule() {
            // Reschedule keeps prompt + plan + auto-exit untouched; surface
            // that explicitly so the user knows the toggle keybinds are
            // intentionally disabled here.
            spans.push(Span::styled(
                "    reschedule (keeps prompt · plan · auto-exit)",
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        } else {
            spans.push(Span::styled(
                format!("    {mod_key}+P plan · {mod_key}+X auto-exit"),
                Style::default().fg(theme::R_TEXT_DISABLED),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

// ---------------------------------------------------------------------------
// Time parsing
// ---------------------------------------------------------------------------

/// Parse a user-typed time/duration string into an absolute UTC timestamp.
///
/// Accepted forms (case-insensitive, surrounding whitespace tolerated):
/// - `Ns` / `Nm` / `Nh` / `Nd` — relative to `now`. `N >= 1`.
/// - `HH:MM` (24-hour, local time) — today if still in the future, otherwise
///   tomorrow. The result is converted to UTC for storage.
///
/// Anything else returns `Err(<short message>)` so the modal can show the
/// error inline.
pub fn parse_time(input: &str, now: chrono::DateTime<Utc>) -> Result<chrono::DateTime<Utc>, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("enter a duration (e.g. 5m, 2h) or a wall-clock time (HH:MM)".into());
    }

    // Wall-clock: HH:MM
    if let Some((h, m)) = s.split_once(':') {
        let h: u32 = h.trim().parse().map_err(|_| invalid_msg())?;
        let m: u32 = m.trim().parse().map_err(|_| invalid_msg())?;
        if h > 23 || m > 59 {
            return Err("HH:MM out of range".into());
        }
        let local_now = Local::now();
        let today = local_now.date_naive();
        let target_naive_today = today
            .and_time(NaiveTime::from_hms_opt(h, m, 0).ok_or_else(invalid_msg)?);
        let target_local_today = Local
            .from_local_datetime(&target_naive_today)
            .single()
            .ok_or_else(|| "ambiguous local time (DST)".to_string())?;
        let target_local = if target_local_today > local_now {
            target_local_today
        } else {
            // Add a day, re-localize.
            let tomorrow = today
                .succ_opt()
                .ok_or_else(|| "calendar overflow".to_string())?;
            let tomorrow_at = tomorrow
                .and_time(NaiveTime::from_hms_opt(h, m, 0).ok_or_else(invalid_msg)?);
            Local
                .from_local_datetime(&tomorrow_at)
                .single()
                .ok_or_else(|| "ambiguous local time (DST)".to_string())?
        };
        return Ok(target_local.with_timezone(&Utc));
    }

    // Duration: trailing unit
    let last = s.chars().last().ok_or_else(invalid_msg)?;
    let unit = match last {
        's' | 'm' | 'h' | 'd' => last,
        _ => return Err(invalid_msg()),
    };
    let num_part = &s[..s.len() - 1];
    let n: i64 = num_part.trim().parse().map_err(|_| invalid_msg())?;
    if n < 1 {
        return Err("duration must be ≥ 1".into());
    }
    let delta = match unit {
        's' => ChronoDuration::seconds(n),
        'm' => ChronoDuration::minutes(n),
        'h' => ChronoDuration::hours(n),
        'd' => ChronoDuration::days(n),
        _ => unreachable!(),
    };
    Ok(now + delta)
}

fn invalid_msg() -> String {
    "expected Ns/Nm/Nh/Nd or HH:MM".into()
}


#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 6, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_minutes() {
        let t = parse_time("5m", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::minutes(5));
    }

    #[test]
    fn parses_seconds() {
        let t = parse_time("30s", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::seconds(30));
    }

    #[test]
    fn parses_hours() {
        let t = parse_time("2h", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::hours(2));
    }

    #[test]
    fn parses_days() {
        let t = parse_time("1d", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::days(1));
    }

    #[test]
    fn case_insensitive_and_trims() {
        let t = parse_time("  2H  ", fixed_now()).unwrap();
        assert_eq!(t - fixed_now(), ChronoDuration::hours(2));
    }

    #[test]
    fn rejects_zero() {
        assert!(parse_time("0m", fixed_now()).is_err());
    }

    #[test]
    fn rejects_negative() {
        assert!(parse_time("-5m", fixed_now()).is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse_time("5x", fixed_now()).is_err());
    }

    #[test]
    fn rejects_no_unit() {
        assert!(parse_time("5", fixed_now()).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_time("   ", fixed_now()).is_err());
    }

    #[test]
    fn parses_wall_clock_today() {
        // Pick an HH:MM well after fixed_now (12:00 UTC). Use 23:00 local to
        // avoid DST edge cases.
        let t = parse_time("23:00", fixed_now()).unwrap();
        // The result should be in the future relative to "now".
        assert!(t > fixed_now());
    }

    #[test]
    fn parses_wall_clock_tomorrow_when_already_past() {
        // Use a time guaranteed to be in the past for any fixed_now we pick.
        // We can't depend on "current local time", but we can verify that the
        // returned timestamp is at most ~24 hours after `now`.
        let t = parse_time("00:00", fixed_now()).unwrap();
        let delta = t - fixed_now();
        assert!(
            delta.num_seconds() >= 0 && delta.num_seconds() <= 25 * 3600,
            "expected within next 25h, got {} s",
            delta.num_seconds()
        );
    }

    #[test]
    fn rejects_bad_hour() {
        assert!(parse_time("25:00", fixed_now()).is_err());
    }

    #[test]
    fn rejects_bad_minute() {
        assert!(parse_time("12:99", fixed_now()).is_err());
    }

    // ── Reschedule mode ─────────────────────────────────────────────

    fn sample_repo() -> RepoInfo {
        RepoInfo {
            path: "/repo".into(),
            name: "repo".into(),
            color: None,
            editor: None,
            local_branches: vec![BranchInfo {
                name: "main".into(),
                is_head: false,
                active_agent_count: 0,
                is_worktree: false,
                is_remote: false,
            }],
            remote_branches: vec![],
        }
    }

    fn sample_existing_task() -> ScheduledTaskInfo {
        ScheduledTaskInfo {
            id: "t1".into(),
            repo_path: "/repo".into(),
            repo_name: "repo".into(),
            branch_name: "main".into(),
            base_branch: Some("main".into()),
            new_branch: None,
            prompt: "do x".into(),
            plan_mode: true,
            auto_exit: false,
            agent_binary: "claude".into(),
            schedule: ScheduleKind::Time {
                start_at: "2026-05-06T13:00:00Z".into(),
            },
            status: ScheduledTaskStatus::Inactive,
            agent_id: None,
            created_at: "2026-05-06T09:00:00Z".into(),
            completed_at: None,
        }
    }

    // ----- Future-branch picker -----

    fn make_task(
        id: &str,
        repo_path: &str,
        repo_name: &str,
        branch_name: &str,
        new_branch: Option<&str>,
        status: ScheduledTaskStatus,
    ) -> ScheduledTaskInfo {
        ScheduledTaskInfo {
            id: id.to_string(),
            repo_path: repo_path.to_string(),
            repo_name: repo_name.to_string(),
            branch_name: branch_name.to_string(),
            base_branch: Some("main".to_string()),
            new_branch: new_branch.map(String::from),
            prompt: "do thing".to_string(),
            plan_mode: false,
            auto_exit: false,
            agent_binary: "claude".to_string(),
            schedule: ScheduleKind::Unscheduled,
            status,
            agent_id: None,
            created_at: "2026-05-07T00:00:00Z".to_string(),
            completed_at: None,
        }
    }

    #[test]
    fn reschedule_starts_at_kind_step() {
        let task = sample_existing_task();
        let modal = ScheduleTaskModal::new_reschedule(
            &task,
            vec![sample_repo()],
            vec![task.clone()],
            Vec::new(),
        );
        assert_eq!(modal.step(), ScheduleModalStep::SelectScheduleKind);
        assert!(modal.is_reschedule());
    }

    #[test]
    fn reschedule_unscheduled_submits_immediately() {
        let task = sample_existing_task();
        let mut modal = ScheduleTaskModal::new_reschedule(
            &task,
            vec![sample_repo()],
            vec![task.clone()],
            Vec::new(),
        );
        // Move down past Schedule + Depend to land on Unscheduled.
        modal.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        modal.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let result = modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match result {
            ScheduleModalResult::Completed(out) => {
                assert_eq!(out.reschedule_task_id.as_deref(), Some("t1"));
                assert!(matches!(out.schedule, ScheduleKind::Unscheduled));
                // Prompt + plan + auto-exit must be carried over from the
                // existing task — reschedule never asks for them.
                assert_eq!(out.prompt, "do x");
                assert!(out.plan_mode);
                assert!(!out.auto_exit);
            }
            other => panic!("expected Completed, got {:?}", other),
        }
    }

    fn make_repo(path: &str, name: &str, branches: &[&str]) -> RepoInfo {
        RepoInfo {
            path: path.to_string(),
            name: name.to_string(),
            color: None,
            editor: None,
            local_branches: branches
                .iter()
                .map(|n| BranchInfo {
                    name: (*n).to_string(),
                    is_head: false,
                    active_agent_count: 0,
                    is_worktree: false,
                    is_remote: false,
                })
                .collect(),
            remote_branches: Vec::new(),
        }
    }

    #[test]
    fn status_label_distinguishes_future_from_pending() {
        let future = make_task(
            "1",
            "/r",
            "r",
            "feature/foo",
            Some("feature/foo"),
            ScheduledTaskStatus::Inactive,
        );
        let pending = make_task(
            "2",
            "/r",
            "r",
            "main",
            None,
            ScheduledTaskStatus::Inactive,
        );
        let running = make_task(
            "3",
            "/r",
            "r",
            "main",
            None,
            ScheduledTaskStatus::Active,
        );
        let done = make_task("4", "/r", "r", "main", None, ScheduledTaskStatus::Complete);
        let aborted = make_task("5", "/r", "r", "main", None, ScheduledTaskStatus::Aborted);
        assert_eq!(task_status_label(&future), "FUTURE");
        assert_eq!(task_status_label(&pending), "PENDING");
        assert_eq!(task_status_label(&running), "RUNNING");
        assert_eq!(task_status_label(&done), "DONE");
        assert_eq!(task_status_label(&aborted), "ABORTED");
    }

    #[test]
    fn dep_rank_orders_future_first() {
        let future = make_task(
            "1",
            "/r",
            "r",
            "f",
            Some("f"),
            ScheduledTaskStatus::Inactive,
        );
        let pending = make_task("2", "/r", "r", "p", None, ScheduledTaskStatus::Inactive);
        let running = make_task("3", "/r", "r", "x", None, ScheduledTaskStatus::Active);
        let done = make_task("4", "/r", "r", "y", None, ScheduledTaskStatus::Complete);
        let aborted = make_task("5", "/r", "r", "z", None, ScheduledTaskStatus::Aborted);
        let mut ranks = [
            task_dep_rank(&aborted),
            task_dep_rank(&done),
            task_dep_rank(&running),
            task_dep_rank(&pending),
            task_dep_rank(&future),
        ];
        ranks.sort();
        assert_eq!(
            ranks,
            [
                DepRank::Future,
                DepRank::Pending,
                DepRank::Running,
                DepRank::Done,
                DepRank::Aborted,
            ]
        );
    }

    #[test]
    fn populate_branches_includes_future_when_depend() {
        let repo = make_repo("/r", "r", &["main", "develop"]);
        let mut modal = ScheduleTaskModal::new(
            vec![repo.clone()],
            vec![make_task(
                "t1",
                "/r",
                "r",
                "feature/foo",
                Some("feature/foo"),
                ScheduledTaskStatus::Inactive,
            )],
            Vec::new(),
        );
        modal.selected_repo = Some(repo);
        modal.populate_branches(ScheduleKindChoice::Depend);
        assert!(modal
            .branches
            .iter()
            .any(|b| matches!(b, BranchChoice::Future { branch_name, .. } if branch_name == "feature/foo")));
        // Existing branches still present.
        assert!(modal
            .branches
            .iter()
            .any(|b| matches!(b, BranchChoice::Existing(bi) if bi.name == "main")));
    }

    #[test]
    fn populate_branches_hides_future_when_not_depend() {
        let repo = make_repo("/r", "r", &["main"]);
        let mut modal = ScheduleTaskModal::new(
            vec![repo.clone()],
            vec![make_task(
                "t1",
                "/r",
                "r",
                "feature/foo",
                Some("feature/foo"),
                ScheduledTaskStatus::Inactive,
            )],
            Vec::new(),
        );
        modal.selected_repo = Some(repo);
        modal.populate_branches(ScheduleKindChoice::Schedule);
        assert!(modal
            .branches
            .iter()
            .all(|b| !matches!(b, BranchChoice::Future { .. })));
        modal.populate_branches(ScheduleKindChoice::Unscheduled);
        assert!(modal
            .branches
            .iter()
            .all(|b| !matches!(b, BranchChoice::Future { .. })));
    }

    #[test]
    fn populate_branches_excludes_future_already_existing_locally() {
        // If a local branch named "foo" already exists, an Inactive task that
        // would re-create "foo" should not surface as a future-branch option.
        let repo = make_repo("/r", "r", &["main", "foo"]);
        let mut modal = ScheduleTaskModal::new(
            vec![repo.clone()],
            vec![make_task(
                "t1",
                "/r",
                "r",
                "foo",
                Some("foo"),
                ScheduledTaskStatus::Inactive,
            )],
            Vec::new(),
        );
        modal.selected_repo = Some(repo);
        modal.populate_branches(ScheduleKindChoice::Depend);
        let future_count = modal
            .branches
            .iter()
            .filter(|b| matches!(b, BranchChoice::Future { .. }))
            .count();
        assert_eq!(future_count, 0);
    }

    #[test]
    fn populate_branches_dedupes_future_with_same_name() {
        // Two Inactive tasks racing for the same future branch name should
        // only surface once in the branch picker.
        let repo = make_repo("/r", "r", &["main"]);
        let mut modal = ScheduleTaskModal::new(
            vec![repo.clone()],
            vec![
                make_task(
                    "t1",
                    "/r",
                    "r",
                    "foo",
                    Some("foo"),
                    ScheduledTaskStatus::Inactive,
                ),
                make_task(
                    "t2",
                    "/r",
                    "r",
                    "foo",
                    Some("foo"),
                    ScheduledTaskStatus::Inactive,
                ),
            ],
            Vec::new(),
        );
        modal.selected_repo = Some(repo);
        modal.populate_branches(ScheduleKindChoice::Depend);
        let future_count = modal
            .branches
            .iter()
            .filter(|b| matches!(b, BranchChoice::Future { .. }))
            .count();
        assert_eq!(future_count, 1);
    }

    #[test]
    fn future_branch_pick_records_implicit_upstream_and_injects_into_deps() {
        let repo = make_repo("/r", "r", &["main"]);
        let upstream = make_task(
            "upstream-1",
            "/r",
            "r",
            "feature/foo",
            Some("feature/foo"),
            ScheduledTaskStatus::Inactive,
        );
        let mut modal = ScheduleTaskModal::new(
            vec![repo.clone()],
            vec![upstream.clone()],
            Vec::new(),
        );
        // Walk through: SelectRepo → SelectScheduleKind=Depend → SelectBranch
        modal.selected_repo = Some(repo);
        modal.step = ScheduleModalStep::SelectScheduleKind;
        modal.schedule_kind = Some(ScheduleKindChoice::Depend);
        modal.populate_branches(ScheduleKindChoice::Depend);
        modal.step = ScheduleModalStep::SelectBranch;
        // Pick the Future branch (last entry; existing "main" is index 0).
        let future_idx = modal
            .branches
            .iter()
            .position(|b| matches!(b, BranchChoice::Future { .. }))
            .expect("future branch present");
        modal.selected_idx = future_idx;
        let _ = modal.handle_enter();
        assert_eq!(modal.target_branch.as_deref(), Some("feature/foo"));
        assert_eq!(modal.implicit_upstream_task_id.as_deref(), Some("upstream-1"));

        // Now drive submission with empty depends — the upstream must still
        // end up in `depends_on_ids` to keep the chain intact.
        modal.new_branch_name = Some("feature/foo-extension".to_string());
        modal.prompt_value = "do work".to_string();
        modal.step = ScheduleModalStep::SelectDependencies;
        // Manually clear selected_deps as if user untoggled — re-injection
        // must still happen on submit.
        modal.selected_deps.clear();
        let result = modal.complete_with_deps(
            "do work".to_string(),
            ScheduleKind::Depend {
                depends_on_ids: Vec::new(),
            },
            Vec::new(),
        );
        match result {
            ScheduleModalResult::Completed(out) => match out.schedule {
                ScheduleKind::Depend { depends_on_ids } => {
                    assert!(depends_on_ids.iter().any(|id| id == "upstream-1"));
                }
                other => panic!("expected Depend, got {other:?}"),
            },
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn reschedule_schedule_kind_advances_to_start_time() {
        let task = sample_existing_task();
        let mut modal = ScheduleTaskModal::new_reschedule(
            &task,
            vec![sample_repo()],
            vec![task.clone()],
            Vec::new(),
        );
        // First option is Schedule.
        modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(modal.step(), ScheduleModalStep::EnterStartTime);
    }

    #[test]
    fn reschedule_esc_at_kind_step_cancels() {
        let task = sample_existing_task();
        let mut modal = ScheduleTaskModal::new_reschedule(
            &task,
            vec![sample_repo()],
            vec![task.clone()],
            Vec::new(),
        );
        let r = modal.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(r, ScheduleModalResult::Cancelled));
    }

    #[test]
    fn reschedule_hides_self_from_dep_picker() {
        let task = sample_existing_task();
        // The picked-up task itself plus another inactive task to depend on.
        let other = ScheduledTaskInfo {
            id: "t2".into(),
            ..sample_existing_task()
        };
        let modal = ScheduleTaskModal::new_reschedule(
            &task,
            vec![sample_repo()],
            vec![task.clone(), other],
            Vec::new(),
        );
        assert_eq!(modal.all_tasks.len(), 1);
        assert_eq!(modal.all_tasks[0].id, "t2");
    }

    #[test]
    fn create_mode_keeps_reschedule_id_none() {
        // Sanity check: regular Opt+S flow must not accidentally tag itself
        // as a reschedule.
        let mut modal = ScheduleTaskModal::new(vec![sample_repo()], Vec::new(), Vec::new());
        // Walk the modal: pick repo → pick Unscheduled → no branches →
        // NewBranch (empty input rejected because new_branch_required) →
        // give it a name → enter prompt → submit.
        modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // repo
        // Repo step picks `repo`; since `local_branches` has one entry,
        // the kind step won't force `new_branch_required`. Pick the kind:
        modal.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // Unscheduled
        // Now branch step. Pick `main` (only entry).
        modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // NewBranch — Enter to skip.
        modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // EnterPrompt — type something.
        for c in "p".chars() {
            modal.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let r = modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match r {
            ScheduleModalResult::Completed(out) => {
                assert!(out.reschedule_task_id.is_none());
            }
            other => panic!("expected Completed, got {:?}", other),
        }
    }

    #[test]
    fn dep_picker_sorts_future_first_when_input_empty() {
        let repo = make_repo("/r", "r", &["main"]);
        let aborted = make_task("a", "/r", "r", "ab", None, ScheduledTaskStatus::Aborted);
        let done = make_task("d", "/r", "r", "do", None, ScheduledTaskStatus::Complete);
        let running = make_task("r", "/r", "r", "ru", None, ScheduledTaskStatus::Active);
        let pending = make_task("p", "/r", "r", "pe", None, ScheduledTaskStatus::Inactive);
        let future = make_task(
            "f",
            "/r",
            "r",
            "fu",
            Some("fu"),
            ScheduledTaskStatus::Inactive,
        );
        // Note the deliberately scrambled insertion order.
        let modal = ScheduleTaskModal::new(
            vec![repo],
            vec![aborted, done, running, pending, future],
            Vec::new(),
        );
        let filtered = modal.filtered_deps();
        let ids: Vec<&str> = filtered
            .iter()
            .filter_map(|(kind, idx, _)| match kind {
                DepKind::Task => Some(modal.all_tasks[*idx].id.as_str()),
                DepKind::Agent => None,
            })
            .collect();
        assert_eq!(ids, vec!["f", "p", "r", "d", "a"]);
    }
}
