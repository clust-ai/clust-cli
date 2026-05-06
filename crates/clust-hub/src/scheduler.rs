//! Background trigger loop for persistent scheduled tasks.
//!
//! The hub spawns [`run_scheduler`] once on startup. It polls the
//! `scheduled_tasks` table every [`POLL_INTERVAL`] and fires any inactive task
//! whose trigger condition is satisfied:
//!
//! - `Time { start_at }`: `now >= start_at`
//! - `Depend { depends_on_ids }`: every upstream task has `status='complete'`
//!   (an `Aborted` upstream blocks indefinitely, by user-confirmed design)
//! - `Unscheduled`: never auto-fires; user must trigger via `StartScheduledTaskNow`
//!
//! When a task fires, [`fire_scheduled_task`] reuses the same
//! [`crate::agent::create_worktree_and_spawn_agent`] helper as the manual
//! Opt+E flow, then writes the resulting `agent_id` back into the row so the
//! agent-exit hook in [`crate::agent`] can mark the task `Complete` later.

use std::time::Duration;

use chrono::{DateTime, Utc};
use clust_ipc::{ScheduleKind, ScheduledTaskInfo, ScheduledTaskStatus, DEFAULT_HUB};
use tokio::time::{interval, sleep};

use crate::agent::{create_worktree_and_spawn_agent, CreateWorktreeParams, SharedHubState};
use crate::db;

/// How often the scheduler wakes to evaluate triggers. 5 seconds is a
/// reasonable trade-off: small enough that user-typed durations like `5m` feel
/// responsive, large enough to cost almost nothing when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Initial delay before the first tick so the hub finishes binding the socket
/// and accepting the first CLI before we start spawning agents (which compete
/// for the same lock).
const STARTUP_DELAY: Duration = Duration::from_secs(2);

/// Default PTY dimensions for scheduler-spawned agents. The CLI sends a real
/// resize the first time it attaches, so these only matter for the brief
/// window before any client is attached.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

/// Run the scheduler loop forever. Spawn this from the hub's tokio runtime.
pub async fn run_scheduler(state: SharedHubState) {
    sleep(STARTUP_DELAY).await;
    let mut tick = interval(POLL_INTERVAL);
    // Skip the immediate-fire that interval does on first tick — STARTUP_DELAY
    // already paced us. We want POLL_INTERVAL between iterations from here on.
    tick.tick().await;

    loop {
        tick.tick().await;
        if let Err(e) = run_one_pass(&state).await {
            eprintln!("[scheduler] pass failed: {e}");
        }
    }
}

/// Single evaluation pass. Public so integration tests can drive it without
/// waiting on `tokio::time::interval`.
pub async fn run_one_pass(state: &SharedHubState) -> Result<(), String> {
    let to_fire = collect_triggered_tasks(state).await?;
    for task in to_fire {
        if let Err(e) = fire_scheduled_task(state, &task).await {
            eprintln!("[scheduler] failed to fire task {}: {e}", task.id);
            mark_task_aborted(state, &task.id).await;
        }
    }
    Ok(())
}

/// Snapshot of inactive tasks whose trigger fires now. The lock is released
/// before we start spawning so per-task git operations don't serialise the
/// whole hub.
async fn collect_triggered_tasks(
    state: &SharedHubState,
) -> Result<Vec<ScheduledTaskInfo>, String> {
    let hub = state.lock().await;
    let Some(ref conn) = hub.db else {
        return Ok(Vec::new());
    };
    // Snapshot repo names (cheap clone) so the lookup closure doesn't need the
    // lock once we drop it.
    let repo_names: std::collections::HashMap<String, String> = db::list_repos(conn)
        .unwrap_or_default()
        .into_iter()
        .map(|(path, name, _, _)| (path, name))
        .collect();
    let lookup = |path: &str| {
        repo_names
            .get(path)
            .cloned()
            .unwrap_or_else(|| display_name_from_path(path))
    };
    let all = db::list_scheduled_tasks(conn, &lookup)?;
    let now = Utc::now();
    let triggered = all
        .into_iter()
        .filter(|t| t.status == ScheduledTaskStatus::Inactive)
        .filter(|t| should_fire(t, &all_status_map(conn), now))
        .collect();
    Ok(triggered)
}

/// Map of `task_id → status`, used by `should_fire` to evaluate Depend
/// upstreams without a per-task DB round-trip.
fn all_status_map(
    conn: &rusqlite::Connection,
) -> std::collections::HashMap<String, ScheduledTaskStatus> {
    let mut out = std::collections::HashMap::new();
    let mut stmt = match conn.prepare("SELECT id, status FROM scheduled_tasks") {
        Ok(s) => s,
        Err(_) => return out,
    };
    if let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) {
        for r in rows.flatten() {
            if let Some(status) = ScheduledTaskStatus::parse_str(&r.1) {
                out.insert(r.0, status);
            }
        }
    }
    out
}

/// Decide whether a single inactive task should fire right now.
///
/// `statuses` is the current snapshot of every task's status, so a Depend task
/// can check its upstreams without re-querying the DB once per dep.
pub fn should_fire(
    task: &ScheduledTaskInfo,
    statuses: &std::collections::HashMap<String, ScheduledTaskStatus>,
    now: DateTime<Utc>,
) -> bool {
    if task.status != ScheduledTaskStatus::Inactive {
        return false;
    }
    match &task.schedule {
        ScheduleKind::Time { start_at } => match DateTime::parse_from_rfc3339(start_at) {
            Ok(dt) => dt.with_timezone(&Utc) <= now,
            Err(_) => false,
        },
        ScheduleKind::Depend { depends_on_ids } => {
            if depends_on_ids.is_empty() {
                return true;
            }
            depends_on_ids
                .iter()
                .all(|id| statuses.get(id) == Some(&ScheduledTaskStatus::Complete))
        }
        ScheduleKind::Unscheduled => false,
    }
}

/// Spawn a worktree agent for `task` and mark the task `active` with the
/// resulting agent id.
///
/// Used by both the auto-trigger pass and the manual "start now" / "restart"
/// IPC handlers, so a task always reaches `active` through the same code path.
pub async fn fire_scheduled_task(
    state: &SharedHubState,
    task: &ScheduledTaskInfo,
) -> Result<String, String> {
    let (agent_id, _binary, _wd) = create_worktree_and_spawn_agent(CreateWorktreeParams {
        state,
        repo_path: &task.repo_path,
        target_branch: task.base_branch_for_spawn().as_deref(),
        new_branch: task.new_branch_for_spawn().as_deref(),
        prompt: Some(task.prompt.clone()),
        agent_binary: Some(task.agent_binary.clone()),
        plan_mode: task.plan_mode,
        allow_bypass: false,
        hub: DEFAULT_HUB,
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
        exit_when_done: task.auto_exit,
    })
    .await?;

    // Persist active + agent_id while still under the hub lock.
    let hub = state.lock().await;
    if let Some(ref conn) = hub.db {
        db::mark_scheduled_task_active(conn, &task.id, &agent_id)?;
    }
    Ok(agent_id)
}

async fn mark_task_aborted(state: &SharedHubState, id: &str) {
    let hub = state.lock().await;
    if let Some(ref conn) = hub.db {
        let _ = db::mark_scheduled_task_aborted(conn, id);
    }
}

/// Reset the worktree at `<repo>/.clust/worktrees/<sanitized_branch>` to a
/// pristine state — `git reset --hard HEAD` then `git clean -fdx`. Used by
/// `RestartScheduledTask { clean: true }` so the agent re-runs against the
/// same starting point as its first attempt.
pub async fn clean_worktree_for_task(task: &ScheduledTaskInfo) -> Result<(), String> {
    let repo_root = std::path::Path::new(&task.repo_path);
    let wt = crate::repo::worktree_path(repo_root, &task.branch_name);
    if !wt.exists() {
        return Err(format!("worktree {} does not exist", wt.display()));
    }
    let wt_str = wt.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        for args in [
            vec!["reset", "--hard", "HEAD"],
            vec!["clean", "-fdx"],
        ] {
            let out = std::process::Command::new("git")
                .current_dir(&wt_str)
                .args(&args)
                .output()
                .map_err(|e| format!("git failed: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("clean worktree blocking task panicked: {e}"))??;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort fallback when no registered repo matches `path`.
fn display_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

trait SpawnFields {
    /// Branch passed as `--target_branch` to `add_worktree`. Empty when this
    /// task created a new branch.
    fn base_branch_for_spawn(&self) -> Option<String>;
    /// Branch passed as `--new_branch` to `add_worktree`. Empty when this task
    /// reused an existing branch.
    fn new_branch_for_spawn(&self) -> Option<String>;
}

impl SpawnFields for ScheduledTaskInfo {
    fn base_branch_for_spawn(&self) -> Option<String> {
        // The wire type carries `branch_name` (the resolved/identifying name)
        // but not the original `base_branch` / `new_branch`. For respawns we
        // re-derive: if the worktree already exists at the conventional path
        // we'll reuse it via `add_worktree`'s short-circuit; otherwise we ask
        // for a checkout-existing of `branch_name`.
        None
    }

    fn new_branch_for_spawn(&self) -> Option<String> {
        // For now we always treat this as "checkout existing" once the task is
        // persisted: the worktree was created on first spawn and the same
        // branch_name is reused on restarts.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, schedule: ScheduleKind, status: ScheduledTaskStatus) -> ScheduledTaskInfo {
        ScheduledTaskInfo {
            id: id.into(),
            repo_path: "/repo".into(),
            repo_name: "repo".into(),
            branch_name: format!("br-{id}"),
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

    #[test]
    fn time_fires_when_due() {
        let now = Utc::now();
        let past = (now - chrono::Duration::seconds(10)).to_rfc3339();
        let t = task(
            "a",
            ScheduleKind::Time { start_at: past },
            ScheduledTaskStatus::Inactive,
        );
        assert!(should_fire(&t, &Default::default(), now));
    }

    #[test]
    fn time_does_not_fire_in_future() {
        let now = Utc::now();
        let future = (now + chrono::Duration::hours(1)).to_rfc3339();
        let t = task(
            "a",
            ScheduleKind::Time { start_at: future },
            ScheduledTaskStatus::Inactive,
        );
        assert!(!should_fire(&t, &Default::default(), now));
    }

    #[test]
    fn unscheduled_never_fires() {
        let t = task("a", ScheduleKind::Unscheduled, ScheduledTaskStatus::Inactive);
        assert!(!should_fire(&t, &Default::default(), Utc::now()));
    }

    #[test]
    fn depend_fires_when_upstream_complete() {
        let t = task(
            "downstream",
            ScheduleKind::Depend {
                depends_on_ids: vec!["upstream".into()],
            },
            ScheduledTaskStatus::Inactive,
        );
        let mut statuses = std::collections::HashMap::new();
        statuses.insert("upstream".into(), ScheduledTaskStatus::Complete);
        assert!(should_fire(&t, &statuses, Utc::now()));
    }

    #[test]
    fn depend_blocks_on_aborted_upstream() {
        let t = task(
            "downstream",
            ScheduleKind::Depend {
                depends_on_ids: vec!["upstream".into()],
            },
            ScheduledTaskStatus::Inactive,
        );
        let mut statuses = std::collections::HashMap::new();
        statuses.insert("upstream".into(), ScheduledTaskStatus::Aborted);
        assert!(
            !should_fire(&t, &statuses, Utc::now()),
            "Aborted upstream should block dependents per the user's design"
        );
    }

    #[test]
    fn depend_blocks_on_active_upstream() {
        let t = task(
            "downstream",
            ScheduleKind::Depend {
                depends_on_ids: vec!["upstream".into()],
            },
            ScheduledTaskStatus::Inactive,
        );
        let mut statuses = std::collections::HashMap::new();
        statuses.insert("upstream".into(), ScheduledTaskStatus::Active);
        assert!(!should_fire(&t, &statuses, Utc::now()));
    }

    #[test]
    fn depend_requires_all_upstreams_complete() {
        let t = task(
            "downstream",
            ScheduleKind::Depend {
                depends_on_ids: vec!["a".into(), "b".into()],
            },
            ScheduledTaskStatus::Inactive,
        );
        let mut statuses = std::collections::HashMap::new();
        statuses.insert("a".into(), ScheduledTaskStatus::Complete);
        statuses.insert("b".into(), ScheduledTaskStatus::Inactive);
        assert!(!should_fire(&t, &statuses, Utc::now()));
        statuses.insert("b".into(), ScheduledTaskStatus::Complete);
        assert!(should_fire(&t, &statuses, Utc::now()));
    }

    #[test]
    fn already_active_does_not_re_fire() {
        let t = task("a", ScheduleKind::Unscheduled, ScheduledTaskStatus::Active);
        assert!(!should_fire(&t, &Default::default(), Utc::now()));
    }
}
