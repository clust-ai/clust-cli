use std::collections::HashSet;
use std::time::Duration;

use clust_ipc::QueuedTask;

use crate::agent::{self, HubState, SharedHubState, SpawnAgentParams};
use crate::db;

// ---------------------------------------------------------------------------
// Hub-side batch types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HubBatchStatus {
    Idle,
    Scheduled,
    Running,
    Completed,
}

impl HubBatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Scheduled => "scheduled",
            Self::Running => "running",
            Self::Completed => "completed",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "running" => Self::Running,
            "completed" => Self::Completed,
            _ => Self::Scheduled,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HubTaskStatus {
    Idle,
    Active,
    Done,
    /// The task's worktree or agent could not be spawned. Terminal state,
    /// distinct from Done so the UI can surface failures.
    Failed,
}

impl HubTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse_status(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Idle,
        }
    }
}

pub struct HubTaskEntry {
    pub branch_name: String,
    pub prompt: String,
    pub status: HubTaskStatus,
    pub agent_id: Option<String>,
    pub use_prefix: bool,
    pub use_suffix: bool,
    pub plan_mode: bool,
    pub exit_when_done: bool,
}

pub struct HubBatchEntry {
    pub id: String,
    pub title: String,
    pub repo_path: String,
    pub target_branch: String,
    pub max_concurrent: Option<usize>,
    pub prompt_prefix: Option<String>,
    pub prompt_suffix: Option<String>,
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub agent_binary: Option<String>,
    pub hub: String,
    pub tasks: Vec<HubTaskEntry>,
    pub scheduled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub status: HubBatchStatus,
    pub launch_mode: String,
    pub depends_on: Vec<String>,
}

impl HubBatchEntry {
    /// Build the full prompt for a task using the batch prefix/suffix,
    /// respecting per-task flags.
    pub fn build_prompt(&self, task: &HubTaskEntry) -> String {
        let mut parts = Vec::new();
        if task.use_prefix {
            if let Some(ref prefix) = self.prompt_prefix {
                parts.push(prefix.as_str());
            }
        }
        parts.push(task.prompt.as_str());
        if task.use_suffix {
            if let Some(ref suffix) = self.prompt_suffix {
                parts.push(suffix.as_str());
            }
        }
        parts.join("\n\n")
    }

    /// How many more agents can be started for this batch.
    fn available_slots(&self) -> usize {
        let active = self
            .tasks
            .iter()
            .filter(|t| t.status == HubTaskStatus::Active)
            .count();
        let max = self.max_concurrent.unwrap_or(usize::MAX);
        max.saturating_sub(active)
    }

    /// Collect the next idle tasks that can be started, up to available slots.
    fn next_tasks_to_start(&self) -> Vec<(usize, &HubTaskEntry)> {
        let slots = self.available_slots();
        self.tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status == HubTaskStatus::Idle)
            .take(slots)
            .collect()
    }

    /// Whether all tasks have reached a terminal state (Done or Failed).
    fn all_done(&self) -> bool {
        self.tasks
            .iter()
            .all(|t| matches!(t.status, HubTaskStatus::Done | HubTaskStatus::Failed))
    }
}

/// Reject batches whose task list contains two tasks that sanitize to the same
/// branch name — they would race each other on the same worktree.
pub fn validate_unique_sanitized_branches(tasks: &[QueuedTask]) -> Result<(), String> {
    let mut seen: HashSet<String> = HashSet::new();
    for t in tasks {
        let s = clust_ipc::branch::sanitize_branch_name(&t.branch_name);
        if !seen.insert(s.clone()) {
            return Err(format!(
                "duplicate sanitized branch name '{s}' in batch (raw: '{}')",
                t.branch_name
            ));
        }
    }
    Ok(())
}

/// Return true if setting `target_id`'s `depends_on` to `new_deps` would form
/// a cycle (or include a self-reference). The TUI already rejects cycles in
/// `batch_deps_modal::would_create_cycle`; this is the IPC-side guard for
/// requests that bypass the TUI.
pub fn would_create_cycle(batches: &[HubBatchEntry], target_id: &str, new_deps: &[String]) -> bool {
    if new_deps.iter().any(|d| d == target_id) {
        return true;
    }
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = new_deps.to_vec();
    while let Some(cur) = stack.pop() {
        if cur == target_id {
            return true;
        }
        if !visited.insert(cur.clone()) {
            continue;
        }
        if let Some(b) = batches.iter().find(|b| b.id == cur) {
            for d in &b.depends_on {
                stack.push(d.clone());
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Timer task
// ---------------------------------------------------------------------------

/// Spawn the background batch timer task that checks for expired timers
/// and advances running batches.
pub fn spawn_batch_timer(state: SharedHubState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            check_and_advance_batches(&state).await;
        }
    });
}

/// A DB update entry: (kind, batch_id, optional (task_index, agent_id)).
type BatchDbUpdate = (&'static str, String, Option<(usize, Option<String>)>);

/// Check all queued batches for expired timers and advance running batches.
async fn check_and_advance_batches(state: &SharedHubState) {
    let now = chrono::Utc::now();

    // Phase 1: Collect work to do (under lock)
    let work = {
        let mut hub = state.lock().await;

        // Collect agent IDs for quick lookup (avoids borrow conflicts)
        let active_agent_ids: std::collections::HashSet<String> =
            hub.agents.keys().cloned().collect();

        let mut tasks_to_spawn: Vec<TaskSpawnRequest> = Vec::new();
        let mut db_updates: Vec<BatchDbUpdate> = Vec::new();

        for batch in hub.queued_batches.iter_mut() {
            // Transition scheduled → running if timer expired
            if batch.status == HubBatchStatus::Scheduled
                && batch.scheduled_at.is_some_and(|t| t <= now)
            {
                batch.status = HubBatchStatus::Running;
                db_updates.push(("running", batch.id.clone(), None));
            }

            // For running batches: check for exited agents and start next tasks
            if batch.status == HubBatchStatus::Running {
                // Mark active tasks whose agents have exited as Done
                for (idx, task) in batch.tasks.iter_mut().enumerate() {
                    if task.status == HubTaskStatus::Active {
                        if let Some(ref aid) = task.agent_id {
                            if !active_agent_ids.contains(aid) {
                                task.status = HubTaskStatus::Done;
                                db_updates.push((
                                    "task_done",
                                    batch.id.clone(),
                                    Some((idx, task.agent_id.clone())),
                                ));
                            }
                        }
                    }
                }

                // Check if all done
                if batch.all_done() {
                    batch.status = HubBatchStatus::Completed;
                    db_updates.push(("completed", batch.id.clone(), None));
                    continue;
                }

                // Collect tasks to spawn AND pre-mark them Active under lock
                // so the next timer tick won't pick them up again, and so a
                // racing RemoveDoneBatchTasks won't shift their indices.
                let to_start: Vec<(usize, String, String, bool, bool)> = batch
                    .next_tasks_to_start()
                    .into_iter()
                    .map(|(idx, task)| {
                        (
                            idx,
                            task.branch_name.clone(),
                            batch.build_prompt(task),
                            task.plan_mode,
                            task.exit_when_done,
                        )
                    })
                    .collect();
                for (task_idx, branch_name, prompt, plan_mode, exit_when_done) in to_start {
                    if let Some(t) = batch.tasks.get_mut(task_idx) {
                        t.status = HubTaskStatus::Active;
                        t.agent_id = None;
                    }
                    db_updates.push((
                        "task_active_pending",
                        batch.id.clone(),
                        Some((task_idx, None)),
                    ));
                    tasks_to_spawn.push(TaskSpawnRequest {
                        batch_id: batch.id.clone(),
                        repo_path: batch.repo_path.clone(),
                        target_branch: batch.target_branch.clone(),
                        branch_name,
                        prompt,
                        agent_binary: batch.agent_binary.clone(),
                        plan_mode,
                        allow_bypass: batch.allow_bypass,
                        hub: batch.hub.clone(),
                        exit_when_done,
                    });
                }
            }
        }

        // Auto-start idle batches whose dependencies are all satisfied.
        // Manual-launch batches are NEVER auto-started — they require explicit
        // per-task user action regardless of dependency state.
        let completed_ids: std::collections::HashSet<String> = hub
            .queued_batches
            .iter()
            .filter(|b| b.status == HubBatchStatus::Completed)
            .map(|b| b.id.clone())
            .collect();

        let batches_to_start: Vec<String> = hub
            .queued_batches
            .iter()
            .filter(|b| {
                b.status == HubBatchStatus::Idle
                    && b.launch_mode == "auto"
                    && !b.depends_on.is_empty()
            })
            .filter(|b| {
                b.depends_on.iter().all(|dep_id| {
                    // Satisfied if completed or no longer in memory (deleted/evicted)
                    completed_ids.contains(dep_id)
                        || !hub.queued_batches.iter().any(|other| other.id == *dep_id)
                })
            })
            .map(|b| b.id.clone())
            .collect();

        for batch_id in &batches_to_start {
            if let Some(batch) = hub.queued_batches.iter_mut().find(|b| &b.id == batch_id) {
                batch.status = HubBatchStatus::Running;
                db_updates.push(("running", batch.id.clone(), None));

                let to_start: Vec<(usize, String, String, bool, bool)> = batch
                    .next_tasks_to_start()
                    .into_iter()
                    .map(|(idx, task)| {
                        (
                            idx,
                            task.branch_name.clone(),
                            batch.build_prompt(task),
                            task.plan_mode,
                            task.exit_when_done,
                        )
                    })
                    .collect();
                for (task_idx, branch_name, prompt, plan_mode, exit_when_done) in to_start {
                    if let Some(t) = batch.tasks.get_mut(task_idx) {
                        t.status = HubTaskStatus::Active;
                        t.agent_id = None;
                    }
                    db_updates.push((
                        "task_active_pending",
                        batch.id.clone(),
                        Some((task_idx, None)),
                    ));
                    tasks_to_spawn.push(TaskSpawnRequest {
                        batch_id: batch.id.clone(),
                        repo_path: batch.repo_path.clone(),
                        target_branch: batch.target_branch.clone(),
                        branch_name,
                        prompt,
                        agent_binary: batch.agent_binary.clone(),
                        plan_mode,
                        allow_bypass: batch.allow_bypass,
                        hub: batch.hub.clone(),
                        exit_when_done,
                    });
                }
            }
        }

        // Apply DB updates (now that iteration is done)
        if let Some(ref db) = hub.db {
            for (kind, batch_id, task_info) in &db_updates {
                match *kind {
                    "running" | "completed" => {
                        let _ = db::update_batch_status(db, batch_id, kind);
                    }
                    "task_done" => {
                        if let Some((idx, ref agent_id)) = task_info {
                            let _ = db::update_task_status(
                                db,
                                batch_id,
                                *idx,
                                "done",
                                agent_id.as_deref(),
                            );
                        }
                    }
                    "task_active_pending" => {
                        if let Some((idx, _)) = task_info {
                            let _ = db::update_task_status(db, batch_id, *idx, "active", None);
                        }
                    }
                    _ => {}
                }
            }
        }

        tasks_to_spawn
    };

    // Phase 2: Spawn agents outside the lock (worktree creation can be slow)
    for req in work {
        let result = create_worktree_and_spawn_agent(CreateWorktreeParams {
            state,
            repo_path: &req.repo_path,
            target_branch: Some(&req.target_branch),
            new_branch: Some(&req.branch_name),
            prompt: Some(req.prompt),
            agent_binary: req.agent_binary,
            plan_mode: req.plan_mode,
            allow_bypass: req.allow_bypass,
            hub: &req.hub,
            cols: 120,
            rows: 40,
            exit_when_done: req.exit_when_done,
        })
        .await;

        // Phase 3: Update task state with the result (under lock).
        // Lookup by branch_name (not task_index) because RemoveDoneBatchTasks
        // can re-index between phases. Branch names are unique within a batch
        // (see validate_unique_sanitized_branches).
        let orphan_to_stop: Option<String> = {
            let mut hub = state.lock().await;
            let batch = hub.queued_batches.iter_mut().find(|b| b.id == req.batch_id);
            match batch {
                None => {
                    // Batch was deleted while we were spawning. Surface the
                    // orphan agent_id so we can stop it after dropping lock.
                    match result {
                        Ok((agent_id, _, _)) => Some(agent_id),
                        Err(_) => None,
                    }
                }
                Some(batch) => {
                    // Recompute task_index for DB write (lookup before
                    // taking the mutable borrow below).
                    let task_idx = batch
                        .tasks
                        .iter()
                        .position(|t| t.branch_name == req.branch_name);
                    if let Some(task) = batch
                        .tasks
                        .iter_mut()
                        .find(|t| t.branch_name == req.branch_name)
                    {
                        match result {
                            Ok((agent_id, _, _)) => {
                                task.status = HubTaskStatus::Active;
                                task.agent_id = Some(agent_id.clone());
                                if let (Some(ref db), Some(idx)) = (&hub.db, task_idx) {
                                    let _ = db::update_task_status(
                                        db,
                                        &req.batch_id,
                                        idx,
                                        "active",
                                        Some(&agent_id),
                                    );
                                }
                            }
                            Err(_) => {
                                // Spawn failed — terminal Failed state, not Done.
                                task.status = HubTaskStatus::Failed;
                                task.agent_id = None;
                                if let (Some(ref db), Some(idx)) = (&hub.db, task_idx) {
                                    let _ = db::update_task_status(
                                        db,
                                        &req.batch_id,
                                        idx,
                                        "failed",
                                        None,
                                    );
                                }
                            }
                        }
                    }
                    None
                }
            }
        };
        if let Some(aid) = orphan_to_stop {
            let _ = agent::stop_agent(state, &aid).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Agent exit hook
// ---------------------------------------------------------------------------

/// Called from `spawn_pty_reader` when an agent exits. Marks the matching
/// queued batch task as Done. Actual task advancement happens on the next
/// timer tick to avoid doing slow worktree operations in the blocking thread.
pub fn on_agent_exited(hub: &mut HubState, agent_id: &str) {
    for batch in hub.queued_batches.iter_mut() {
        if batch.status != HubBatchStatus::Running {
            continue;
        }
        for (idx, task) in batch.tasks.iter_mut().enumerate() {
            if task.agent_id.as_deref() == Some(agent_id) {
                task.status = HubTaskStatus::Done;
                if let Some(ref db) = hub.db {
                    let _ = db::update_task_status(
                        db,
                        &batch.id,
                        idx,
                        "done",
                        task.agent_id.as_deref(),
                    );
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared worktree + agent creation helper
// ---------------------------------------------------------------------------

struct TaskSpawnRequest {
    batch_id: String,
    repo_path: String,
    target_branch: String,
    branch_name: String,
    prompt: String,
    agent_binary: Option<String>,
    plan_mode: bool,
    allow_bypass: bool,
    hub: String,
    exit_when_done: bool,
}

/// Parameters for `create_worktree_and_spawn_agent`.
pub struct CreateWorktreeParams<'a> {
    pub state: &'a SharedHubState,
    pub repo_path: &'a str,
    pub target_branch: Option<&'a str>,
    pub new_branch: Option<&'a str>,
    pub prompt: Option<String>,
    pub agent_binary: Option<String>,
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub hub: &'a str,
    pub cols: u16,
    pub rows: u16,
    pub exit_when_done: bool,
}

/// Create a git worktree and spawn an agent in it.
/// Returns `(agent_id, agent_binary, working_dir)` on success.
///
/// This extracts the logic previously inline in the `CreateWorktreeAgent` IPC
/// handler so it can be reused by both IPC and the batch timer.
pub async fn create_worktree_and_spawn_agent(
    params: CreateWorktreeParams<'_>,
) -> Result<(String, String, String), String> {
    let CreateWorktreeParams {
        state,
        repo_path,
        target_branch,
        new_branch,
        prompt,
        agent_binary,
        plan_mode,
        allow_bypass,
        hub,
        cols,
        rows,
        exit_when_done,
    } = params;
    // Determine branch name
    let sanitized_new = new_branch.map(clust_ipc::branch::sanitize_branch_name);
    let branch_name = sanitized_new
        .as_deref()
        .or(target_branch)
        .ok_or("either target_branch or new_branch must be provided")?
        .to_string();

    // Create worktree (outside lock — can be slow)
    let repo_root = std::path::Path::new(repo_path);
    let checkout_existing = new_branch.is_none();
    let base = if new_branch.is_some() {
        target_branch
    } else {
        None
    };

    let worktree_path = crate::repo::add_worktree(repo_root, &branch_name, base, checkout_existing)
        .await
        .map_err(|e| {
            if e.contains("already checked out") {
                format!(
                    "Branch '{}' is already checked out and cannot be used as a worktree.",
                    branch_name
                )
            } else {
                e
            }
        })?;

    let working_dir = worktree_path.to_string_lossy().into_owned();

    // Detect git info from the new worktree
    let (wt_repo_path, wt_branch_name, is_worktree) =
        match crate::repo::detect_git_root(&working_dir) {
            Some(root) => {
                let rp = root.to_string_lossy().into_owned();
                let (bn, iw) = crate::repo::detect_branch_and_worktree(&working_dir);
                (Some(rp), bn.or(Some(branch_name)), iw)
            }
            None => (Some(repo_path.to_string()), Some(branch_name), true),
        };

    // Spawn agent (under lock)
    let result = {
        let mut hub_state = state.lock().await;
        agent::spawn_agent(
            &mut hub_state,
            SpawnAgentParams {
                prompt,
                agent_binary,
                working_dir: working_dir.clone(),
                cols,
                rows,
                accept_edits: false,
                plan_mode,
                allow_bypass,
                hub: hub.to_string(),
                repo_path: wt_repo_path,
                branch_name: wt_branch_name,
                is_worktree,
                exit_when_done,
            },
            state.clone(),
        )
    };

    match result {
        Ok((id, binary)) => {
            // Auto-register repo
            let hub_state = state.lock().await;
            if let Some(ref db) = hub_state.db {
                let name = std::path::Path::new(repo_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| repo_path.to_string());
                let _ = crate::db::register_repo(db, repo_path, &name, "");
            }
            Ok((id, binary, working_dir))
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Loading from database
// ---------------------------------------------------------------------------

/// Convert database rows into in-memory batch entries.
pub fn load_batches_from_db(hub: &HubState) -> Vec<HubBatchEntry> {
    let conn = match hub.db {
        Some(ref c) => c,
        None => return Vec::new(),
    };

    let rows = match db::load_queued_batches(conn) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    rows.into_iter()
        .map(|(batch_row, task_rows)| {
            let parsed_at = batch_row.scheduled_at.as_ref().and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            });

            let status = HubBatchStatus::parse(&batch_row.status);

            // Recovery for corrupt rows: if a batch is persisted as
            // Scheduled but has an unparseable/missing timestamp, treat it
            // as due immediately so the next timer tick advances it instead
            // of leaving it stuck forever.
            let scheduled_at = if status == HubBatchStatus::Scheduled && parsed_at.is_none() {
                Some(chrono::Utc::now())
            } else {
                parsed_at
            };

            let tasks = task_rows
                .into_iter()
                .map(|t| HubTaskEntry {
                    branch_name: t.branch_name,
                    prompt: t.prompt,
                    status: HubTaskStatus::parse_status(&t.status),
                    agent_id: t.agent_id,
                    use_prefix: t.use_prefix,
                    use_suffix: t.use_suffix,
                    plan_mode: t.plan_mode,
                    exit_when_done: t.exit_when_done,
                })
                .collect();

            let depends_on: Vec<String> =
                serde_json::from_str(&batch_row.depends_on).unwrap_or_default();

            HubBatchEntry {
                id: batch_row.id,
                title: batch_row.title,
                repo_path: batch_row.repo_path,
                target_branch: batch_row.target_branch,
                max_concurrent: batch_row.max_concurrent,
                prompt_prefix: batch_row.prompt_prefix,
                prompt_suffix: batch_row.prompt_suffix,
                plan_mode: batch_row.plan_mode,
                allow_bypass: batch_row.allow_bypass,
                agent_binary: batch_row.agent_binary,
                hub: batch_row.hub,
                tasks,
                scheduled_at,
                status,
                launch_mode: batch_row.launch_mode,
                depends_on,
            }
        })
        .collect()
}
