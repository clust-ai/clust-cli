use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::agent::{self, SharedHubState, SpawnAgentParams};
use crate::batch::HubTaskEntry;

/// Suffix appended (programmatically) to every worker task's suffix in
/// orchestrator-imported batches. Manager tasks have `use_suffix: false`
/// and never see this.
pub const COMMIT_SUFFIX: &str = "Commit when you are finished with your implementation.";

/// In-process tracking entry for a live orchestrator agent.
#[derive(Clone)]
pub struct OrchestratorEntry {
    pub id: String,
    pub agent_id: String,
    pub inbox_dir: PathBuf,
    pub repo_path: String,
    pub source_branch: String,
    pub target_branch: String,
}

impl OrchestratorEntry {
    /// Snapshot the persistent fields for the on-disk sidecar.
    pub fn sidecar(&self) -> OrchestratorSidecar {
        OrchestratorSidecar {
            id: self.id.clone(),
            agent_id: self.agent_id.clone(),
            repo_path: self.repo_path.clone(),
            source_branch: self.source_branch.clone(),
            target_branch: self.target_branch.clone(),
        }
    }

    /// Reconstruct a tracking entry from a sidecar + the on-disk inbox path.
    /// Used after a hub restart, where the live in-memory entry is gone.
    pub fn from_sidecar(s: OrchestratorSidecar, inbox_dir: PathBuf) -> Self {
        Self {
            id: s.id,
            agent_id: s.agent_id,
            inbox_dir,
            repo_path: s.repo_path,
            source_branch: s.source_branch,
            target_branch: s.target_branch,
        }
    }
}

/// Filename for the per-inbox sidecar that lets the watcher rebuild the
/// metadata it needs after a hub restart.
pub const SIDECAR_FILENAME: &str = "orch.json";

/// Persistent metadata written next to `manifest.json` so the inbox is
/// self-contained — the watcher can import the batches even after a hub
/// restart that wiped `state.orchestrators`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorSidecar {
    pub id: String,
    pub agent_id: String,
    pub repo_path: String,
    pub source_branch: String,
    pub target_branch: String,
}

/// Write the sidecar atomically (write tmp → rename) so a partial file is
/// never observed by the watcher.
pub fn write_sidecar(inbox_dir: &Path, sidecar: &OrchestratorSidecar) -> Result<(), String> {
    let final_path = inbox_dir.join(SIDECAR_FILENAME);
    let tmp_path = inbox_dir.join(format!("{SIDECAR_FILENAME}.tmp"));
    let body =
        serde_json::to_vec_pretty(sidecar).map_err(|e| format!("serialize orch sidecar: {e}"))?;
    std::fs::write(&tmp_path, body).map_err(|e| format!("write orch sidecar tmp: {e}"))?;
    std::fs::rename(&tmp_path, &final_path).map_err(|e| format!("rename orch sidecar: {e}"))?;
    Ok(())
}

/// Read the sidecar from an inbox dir.
pub fn read_sidecar(inbox_dir: &Path) -> Result<OrchestratorSidecar, String> {
    let path = inbox_dir.join(SIDECAR_FILENAME);
    let body = std::fs::read_to_string(&path)
        .map_err(|e| format!("read orch sidecar {}: {e}", path.display()))?;
    serde_json::from_str(&body).map_err(|e| format!("parse orch sidecar: {e}"))
}

/// Generate a 12-char hex orchestrator id, matching the agent-id style.
pub fn generate_orchestrator_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 6] = rng.gen();
    format!(
        "o{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

/// Path to the inbox directory for a given orchestrator id.
pub fn inbox_dir_for(orch_id: &str) -> PathBuf {
    clust_ipc::clust_dir().join("inbox").join(orch_id)
}

/// Path to the archive directory for processed orchestrator inboxes.
pub fn processed_dir() -> PathBuf {
    clust_ipc::clust_dir().join("inbox").join(".processed")
}

/// Spawn an orchestrator agent. Creates the inbox dir, opens a worktree on
/// `new_branch` branched from `source_branch`, and spawns the underlying
/// agent process inside that worktree.
///
/// Returns `(orchestrator_id, agent_id, inbox_dir, working_dir)` on success.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_orchestrator(
    state: &SharedHubState,
    repo_path: &str,
    source_branch: &str,
    new_branch: &str,
    user_prompt: &str,
    cols: u16,
    rows: u16,
    hub: &str,
) -> Result<(String, String, PathBuf, String), String> {
    let orch_id = generate_orchestrator_id();
    let inbox = inbox_dir_for(&orch_id);
    std::fs::create_dir_all(&inbox)
        .map_err(|e| format!("failed to create inbox dir {}: {e}", inbox.display()))?;

    let sanitized_new = clust_ipc::branch::sanitize_branch_name(new_branch);
    if sanitized_new.is_empty() {
        return Err("new_branch is empty after sanitization".to_string());
    }
    if sanitized_new == source_branch {
        return Err("new_branch must differ from source_branch".to_string());
    }

    let repo_root = std::path::Path::new(repo_path);
    let worktree_path =
        crate::repo::add_worktree(repo_root, &sanitized_new, Some(source_branch), false)?;
    let working_dir = worktree_path.to_string_lossy().into_owned();

    let (wt_repo_path, wt_branch_name, is_worktree) =
        match crate::repo::detect_git_root(&working_dir) {
            Some(root) => {
                let rp = root.to_string_lossy().into_owned();
                let (bn, iw) = crate::repo::detect_branch_and_worktree(&working_dir);
                (Some(rp), bn.or_else(|| Some(sanitized_new.clone())), iw)
            }
            None => (
                Some(repo_path.to_string()),
                Some(sanitized_new.clone()),
                true,
            ),
        };

    let full_prompt = build_orchestrator_prompt(
        source_branch,
        &sanitized_new,
        &inbox.to_string_lossy(),
        user_prompt,
    );

    let (agent_id, _binary) = {
        let mut hub_state = state.lock().await;
        agent::spawn_agent(
            &mut hub_state,
            SpawnAgentParams {
                prompt: Some(full_prompt),
                agent_binary: None,
                working_dir: working_dir.clone(),
                cols,
                rows,
                accept_edits: false,
                plan_mode: false,
                allow_bypass: true,
                hub: hub.to_string(),
                repo_path: wt_repo_path,
                branch_name: wt_branch_name,
                is_worktree,
            },
            state.clone(),
        )?
    };

    let entry = OrchestratorEntry {
        id: orch_id.clone(),
        agent_id: agent_id.clone(),
        inbox_dir: inbox.clone(),
        repo_path: repo_path.to_string(),
        source_branch: source_branch.to_string(),
        target_branch: sanitized_new,
    };

    // Persist a sidecar so the watcher can finish importing this orchestrator's
    // batches even if the hub is restarted before the manifest is processed.
    if let Err(e) = write_sidecar(&inbox, &entry.sidecar()) {
        return Err(format!("failed to write orchestrator sidecar: {e}"));
    }

    {
        let mut hub_state = state.lock().await;
        hub_state.orchestrators.insert(orch_id.clone(), entry);
    }

    Ok((orch_id, agent_id, inbox, working_dir))
}

/// Build the full prompt sent to the orchestrator agent on launch:
/// the standard prefix with `{source_branch}` / `{target_branch}` filled in,
/// followed by the user's prompt, followed by the literal `INBOX:` line.
pub fn build_orchestrator_prompt(
    source_branch: &str,
    target_branch: &str,
    inbox_dir: &str,
    user_prompt: &str,
) -> String {
    let prefix = ORCHESTRATOR_PREFIX
        .replace("{source_branch}", source_branch)
        .replace("{target_branch}", target_branch);
    format!("{prefix}\n\n## User request\n{user_prompt}\n\nINBOX: {inbox_dir}\n")
}

/// Slugify a string into a kebab-case identifier suitable for branch components.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("batch");
    }
    out
}

/// Build the auto-injected manager task for one batch. The `worker_branches`
/// list is the branches of every other (worker) task in the batch. The manager
/// runs in a worktree on `target_branch` and merges those branches locally as
/// they accumulate commits.
///
/// The batch_id is interpolated into the prompt so the manager can call
/// `clust ls --batch <batch_id>` to detect when sibling agents have exited.
pub fn build_manager_task(
    batch_id: &str,
    batch_title: &str,
    target_branch: &str,
    worker_branches: &[String],
) -> HubTaskEntry {
    let prompt = format!(
        r#"You are the merge manager for batch "{title}".
Your job: locally merge sibling task branches into the integration branch as their work completes.

# State
- Sibling task branches: {branches}
- Integration branch (target): {target}
- You are running in a worktree on the integration branch. Use `git merge` here only.
- Do NOT push to the remote. Local merges only.

# Loop
1. For each sibling branch, check whether it has commits not yet in HEAD using `git log HEAD..<branch> --oneline`. If empty, skip it.
2. If there are new commits, run `git merge --no-ff <branch>`. On conflict, abort with `git merge --abort` and continue — leaving the conflict for human resolution.
3. After each pass, check sibling agent status: run `clust ls --batch {batch_id}` and look at which agents are still listed. (Manager tasks ignore themselves.)
4. If every non-manager sibling has exited (no longer present in `clust ls`), do ONE final pass through the loop, then exit 0.
5. Otherwise sleep 60 seconds and go to step 1.

# Hard rules
- Never push, never force-push, never rebase, never delete branches.
- If `git merge` fails for a non-conflict reason, log the error to stderr and exit 1 — Clust will still mark you done.
"#,
        title = batch_title,
        branches = worker_branches.join(", "),
        target = target_branch,
        batch_id = batch_id,
    );

    HubTaskEntry {
        branch_name: format!("manager/{}", slug(batch_title)),
        prompt,
        status: crate::batch::HubTaskStatus::Idle,
        agent_id: None,
        use_prefix: false,
        use_suffix: false,
        plan_mode: false,
        is_manager: true,
    }
}

/// Per-(repo, target_branch) lock so two manager agents do not race to
/// check out the same branch via `git worktree`.
pub type ManagerLockMap = std::collections::HashMap<(String, String), Arc<Mutex<()>>>;

/// Get-or-create the manager lock for the given (repo, branch).
pub fn manager_lock(map: &mut ManagerLockMap, repo: &str, branch: &str) -> Arc<Mutex<()>> {
    let key = (repo.to_string(), branch.to_string());
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

// ---------------------------------------------------------------------------
// Prefix prompt
// ---------------------------------------------------------------------------

/// Prefix prepended to every orchestrator agent's user prompt. The
/// `{source_branch}` and `{target_branch}` placeholders are interpolated at
/// spawn time. The literal `INBOX:` line is appended after the user prompt.
pub const ORCHESTRATOR_PREFIX: &str = r#"You are a Clust **Orchestrator Agent**. Your only job is to design a DAG of work batches that other Claude agents will execute in parallel, then emit those batches as JSON files. You do NOT write code yourself.

# Your environment
- You are running inside a git worktree at the project root.
- Your integration branch is "{target_branch}" — branched from "{source_branch}".
- You have an INBOX directory: see the INBOX line at the bottom of this prompt. You MUST write all output JSON files there. No other location is read.
- You can run shell commands to inspect the codebase (read files, grep, ls).

# Workflow
1. **Understand the request fully before planning.** Read the user's prompt, then exhaust every uncertainty by asking the user clarifying questions in the terminal until you can produce a plan with no surprises. Examples of things to nail down: scope, files/modules involved, test strategy, naming conventions, deployment concerns, what "done" looks like for each piece. Do not skip this — a hand-wavy plan produces failing tasks.
2. **Inspect the existing codebase.** Use shell commands to understand the structure, conventions, and current state. Reuse existing patterns; do not propose new abstractions when something fits.
3. **Decompose the work into batches.** Each batch is a group of tasks that can run in parallel without stepping on each other's files. Batches that depend on earlier batches go later in the DAG via `depends_on`.
4. **Emit the JSON files** to your INBOX, then write `manifest.json` LAST.
5. After writing the manifest, your job is done. Clust will import the plan and stop your process automatically — do NOT keep working.

# Naming branches (CRITICAL — keep things orderly)
Every task gets its own branch. Each branch lives only on that task — never reuse a branch across tasks.
- Pattern: `<integration>/<batch-slug>/<task-slug>` — e.g. `feat/auth/batch-models/user-table`.
- The integration prefix is the slug of the integration branch you were given.
- Use kebab-case. No spaces, no uppercase, no `..`, no leading `-`.
- Branches are created off the integration branch. Sibling task branches in the same batch run in parallel.

# JSON file format (write each batch as its own file in your INBOX)
```json
{
  "title": "Concise unique batch title",
  "prefix": "Optional shared context applied to every task in this batch.",
  "suffix": "Optional shared instructions applied to every task in this batch.",
  "launch_mode": "auto",
  "max_concurrent": 3,
  "plan_mode": false,
  "allow_bypass": false,
  "depends_on": ["Title of an earlier batch"],
  "tasks": [
    {
      "branch": "feat/auth/batch-models/user-table",
      "prompt": "Detailed, self-contained prompt for this single agent. Include file paths, expected behavior, and acceptance criteria. The agent will not see other tasks' prompts."
    }
  ]
}
```

# DAG rules
- `depends_on` references batch **titles** (case-sensitive, must match exactly).
- The DAG must be acyclic — Clust will reject cyclic plans.
- A batch starts only after every batch listed in its `depends_on` has completed.
- Independent batches with no `depends_on` start in parallel.
- Use dependencies sparingly; only when batch B truly needs batch A's branches merged first.

# Manifest (write LAST, signals you are done)
```json
{
  "version": 1,
  "complete": true,
  "batches": ["batch-001.json", "batch-002.json"]
}
```
The `batches` array lists every JSON file you wrote. Use `batch-001.json`, `batch-002.json`, etc. for filenames.

# Hard rules
- Every task `prompt` must be non-empty and self-contained — the executing agent has no memory of your conversation with the user.
- No two tasks (across any batches) may share a branch name.
- Every batch must have at least one task and a unique non-empty `title`.
- Do NOT add a "manager" or "merge" task yourself. Clust adds one automatically per batch.
- Do NOT set `is_manager` on any task — it is reserved.
- Do NOT push branches anywhere or perform git operations beyond reading. The executing agents handle that.
- After writing `manifest.json`, stop. Do not continue working. Your process will be terminated automatically.

# Quality bar
A good plan is one where every task agent can succeed without asking questions, and the merge order is unambiguous. If you can't write a self-contained prompt for a task, the work isn't broken down enough — go back to step 1 and ask the user more.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_simple() {
        assert_eq!(slug("My Batch"), "my-batch");
        assert_eq!(slug("Models / Schema!"), "models-schema");
        assert_eq!(slug("---"), "batch");
    }

    #[test]
    fn build_prompt_interpolates() {
        let p = build_orchestrator_prompt("main", "feat/x", "/tmp/inbox", "do the thing");
        assert!(p.contains("\"feat/x\""));
        assert!(p.contains("\"main\""));
        assert!(p.contains("INBOX: /tmp/inbox"));
        assert!(p.contains("do the thing"));
    }

    #[test]
    fn manager_task_props() {
        let t = build_manager_task(
            "b00001",
            "Models",
            "feat/x",
            &["a".to_string(), "b".to_string()],
        );
        assert!(t.is_manager);
        assert!(!t.use_prefix);
        assert!(!t.use_suffix);
        assert_eq!(t.branch_name, "manager/models");
        assert!(t.prompt.contains("b00001"));
        assert!(t.prompt.contains("feat/x"));
    }

    #[test]
    fn sidecar_roundtrips_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let original = OrchestratorSidecar {
            id: "oabcdef123456".to_string(),
            agent_id: "a000111222".to_string(),
            repo_path: "/tmp/repo".to_string(),
            source_branch: "main".to_string(),
            target_branch: "feat/x".to_string(),
        };
        write_sidecar(dir.path(), &original).unwrap();
        let read_back = read_sidecar(dir.path()).unwrap();
        assert_eq!(read_back.id, original.id);
        assert_eq!(read_back.agent_id, original.agent_id);
        assert_eq!(read_back.repo_path, original.repo_path);
        assert_eq!(read_back.source_branch, original.source_branch);
        assert_eq!(read_back.target_branch, original.target_branch);
    }

    #[test]
    fn sidecar_write_is_atomic() {
        // Writing twice should overwrite cleanly without leaving a tmp file.
        let dir = tempfile::tempdir().unwrap();
        let s = OrchestratorSidecar {
            id: "o1".to_string(),
            agent_id: "a1".to_string(),
            repo_path: "/r".to_string(),
            source_branch: "s".to_string(),
            target_branch: "t".to_string(),
        };
        write_sidecar(dir.path(), &s).unwrap();
        write_sidecar(dir.path(), &s).unwrap();
        assert!(dir.path().join(SIDECAR_FILENAME).exists());
        assert!(!dir.path().join(format!("{SIDECAR_FILENAME}.tmp")).exists());
    }
}
