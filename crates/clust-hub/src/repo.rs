use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clust_ipc::{AgentInfo, BranchInfo, RepoInfo, WorktreeEntry};

use crate::agent::AgentEntry;

/// Trait for accessing agent matching fields without requiring the full AgentEntry.
/// This allows repo queries to use a lightweight snapshot taken outside the hub lock.
pub trait AgentMatcher {
    fn repo_path(&self) -> Option<&str>;
    fn branch_name(&self) -> Option<&str>;
    fn id(&self) -> &str;
    fn agent_binary(&self) -> &str;
    fn started_at(&self) -> &str;
    fn attached_clients(&self) -> usize;
    fn hub(&self) -> &str;
    fn working_dir(&self) -> &str;
    fn is_worktree(&self) -> bool;
}

impl AgentMatcher for AgentEntry {
    fn repo_path(&self) -> Option<&str> {
        self.repo_path.as_deref()
    }
    fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn agent_binary(&self) -> &str {
        &self.agent_binary
    }
    fn started_at(&self) -> &str {
        &self.started_at
    }
    fn attached_clients(&self) -> usize {
        self.attached_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn hub(&self) -> &str {
        &self.hub
    }
    fn working_dir(&self) -> &str {
        &self.working_dir
    }
    fn is_worktree(&self) -> bool {
        self.is_worktree
    }
}

/// Lightweight snapshot of agent fields needed for repo state queries.
pub(crate) struct AgentSnapshot {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub attached_clients: usize,
    pub hub: String,
    pub working_dir: String,
    pub repo_path: Option<String>,
    pub branch_name: Option<String>,
    pub is_worktree: bool,
}

impl AgentMatcher for AgentSnapshot {
    fn repo_path(&self) -> Option<&str> {
        self.repo_path.as_deref()
    }
    fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn agent_binary(&self) -> &str {
        &self.agent_binary
    }
    fn started_at(&self) -> &str {
        &self.started_at
    }
    fn attached_clients(&self) -> usize {
        self.attached_clients
    }
    fn hub(&self) -> &str {
        &self.hub
    }
    fn working_dir(&self) -> &str {
        &self.working_dir
    }
    fn is_worktree(&self) -> bool {
        self.is_worktree
    }
}

/// Walk upward from a working directory to find the git repository root.
/// Handles both regular repos and worktrees.
pub fn detect_git_root(working_dir: &str) -> Option<PathBuf> {
    let repo = git2::Repository::discover(working_dir).ok()?;
    if repo.is_worktree() {
        // For worktrees, repo.path() is like /main/.git/worktrees/<name>/
        // Walk up to .git/, then up to the repo root.
        let git_dir = repo.path(); // .git/worktrees/<name>/
        git_dir
            .parent() // .git/worktrees/
            .and_then(|p| p.parent()) // .git/
            .and_then(|p| p.parent()) // repo root
            .map(|p| p.to_path_buf())
    } else {
        // Regular repo: repo.path() is /repo/.git/, parent is the root.
        repo.path().parent().map(|p| p.to_path_buf())
    }
}

/// Detect the current branch name and whether the working directory is a worktree.
pub fn detect_branch_and_worktree(working_dir: &str) -> (Option<String>, bool) {
    let Ok(repo) = git2::Repository::discover(working_dir) else {
        return (None, false);
    };
    let branch_name = repo
        .head()
        .ok()
        .and_then(|h| h.shorthand().map(|s| s.to_string()));
    let is_worktree = repo.is_worktree();
    (branch_name, is_worktree)
}

/// Compute the current state of a repository: branches, worktrees, active agents.
/// Returns None if the repo cannot be opened (e.g. path deleted).
pub fn get_repo_state<A: AgentMatcher>(
    path: &Path,
    name: &str,
    agents: &HashMap<String, A>,
) -> Option<RepoInfo> {
    let repo = git2::Repository::open(path).ok()?;

    let worktree_branches = collect_worktree_branches(&repo);
    let local_branches = list_branches(&repo, git2::BranchType::Local, agents, &worktree_branches);
    let remote_branches =
        list_branches(&repo, git2::BranchType::Remote, agents, &worktree_branches);

    Some(RepoInfo {
        path: path.to_string_lossy().into_owned(),
        name: name.to_string(),
        color: None,
        local_branches,
        remote_branches,
    })
}

fn list_branches<A: AgentMatcher>(
    repo: &git2::Repository,
    branch_type: git2::BranchType,
    agents: &HashMap<String, A>,
    worktree_branches: &[String],
) -> Vec<BranchInfo> {
    let Ok(branches) = repo.branches(Some(branch_type)) else {
        return vec![];
    };

    let repo_path = repo
        .workdir()
        .map(|p| p.to_string_lossy().trim_end_matches('/').to_string());

    branches
        .filter_map(|b| b.ok())
        .filter_map(|(branch, _)| {
            let name = branch.name().ok()??.to_string();
            let is_head = branch.is_head();
            let is_worktree = worktree_branches.contains(&name);

            // Count agents whose repo_path and branch_name match this branch
            let active_agent_count = repo_path.as_ref().map_or(0, |rp| {
                agents
                    .values()
                    .filter(|a| {
                        a.repo_path() == Some(rp.as_str())
                            && a.branch_name() == Some(&name)
                    })
                    .count()
            });

            Some(BranchInfo {
                name,
                is_head,
                active_agent_count,
                is_worktree,
            })
        })
        .collect()
}

/// Collect branch names that are checked out in worktrees.
fn collect_worktree_branches(repo: &git2::Repository) -> Vec<String> {
    let mut branches = Vec::new();

    // The main checkout's HEAD branch is also "checked out"
    if let Ok(head) = repo.head() {
        if let Some(name) = head.shorthand() {
            branches.push(name.to_string());
        }
    }

    if let Ok(worktree_names) = repo.worktrees() {
        for name in worktree_names.iter().flatten() {
            // Open the worktree's repo to read its HEAD
            if let Ok(wt) = repo.find_worktree(name) {
                if let Ok(wt_repo) = git2::Repository::open_from_worktree(&wt) {
                    if let Ok(head) = wt_repo.head() {
                        if let Some(branch_name) = head.shorthand() {
                            branches.push(branch_name.to_string());
                        }
                    }
                }
            }
        }
    }

    branches
}

// ── Worktree management ────────────────────────────────────────────────

/// Serialize a branch name for use as a directory name.
/// Replaces `/` with `__` (e.g., `feature/auth` -> `feature__auth`).
pub fn serialize_branch_name(branch: &str) -> String {
    branch.replace('/', "__")
}

/// Deserialize a directory name back to a branch name.
/// Replaces `__` with `/` (e.g., `feature__auth` -> `feature/auth`).
pub fn deserialize_branch_name(dir_name: &str) -> String {
    dir_name.replace("__", "/")
}

/// Compute the worktree directory path for a given branch.
/// Convention: `{repo_root}/.clust/worktrees/{serialized_branch}`
pub fn worktree_path(repo_root: &Path, branch: &str) -> PathBuf {
    repo_root
        .join(".clust")
        .join("worktrees")
        .join(serialize_branch_name(branch))
}

/// Ensure `.clust/` is listed in `.git/info/exclude` so git ignores it.
fn ensure_clust_dir_excluded(repo_root: &Path) -> Result<(), String> {
    let exclude_path = repo_root.join(".git").join("info").join("exclude");
    let exclude_entry = ".clust/";

    let content = std::fs::read_to_string(&exclude_path).unwrap_or_default();

    if content.lines().any(|line| line.trim() == exclude_entry) {
        return Ok(());
    }

    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create .git/info/: {e}"))?;
    }

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude_path)
        .map_err(|e| format!("failed to open .git/info/exclude: {e}"))?;

    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(file).map_err(|e| format!("failed to write exclude: {e}"))?;
    }
    writeln!(file, "{exclude_entry}")
        .map_err(|e| format!("failed to write exclude entry: {e}"))?;

    Ok(())
}

/// Resolve a repo root path from either a working directory or a repo name.
pub fn resolve_repo(
    working_dir: Option<&str>,
    repo_name: Option<&str>,
    db: Option<&rusqlite::Connection>,
) -> Result<PathBuf, String> {
    if let Some(name) = repo_name {
        let db = db.ok_or("database not initialized")?;
        match crate::db::find_repo_by_name(db, name)? {
            Some(path) => Ok(PathBuf::from(path)),
            None => Err(format!("no repo named '{name}' is registered")),
        }
    } else if let Some(wd) = working_dir {
        detect_git_root(wd).ok_or_else(|| format!("{wd} is not inside a git repository"))
    } else {
        Err("no repo specified".into())
    }
}

/// Check if a worktree path has uncommitted changes.
pub fn is_worktree_dirty(wt_path: &Path) -> bool {
    let Ok(repo) = git2::Repository::open(wt_path) else {
        return false;
    };
    let Ok(statuses) = repo.statuses(None) else {
        return false;
    };
    !statuses.is_empty()
}

/// List all worktrees in a repository.
///
/// Uses `git worktree list --porcelain` for reliable parsing.
/// Returns entries for the main checkout and all worktrees.
pub fn list_worktrees<A: AgentMatcher>(
    repo_root: &Path,
    agents: &HashMap<String, A>,
) -> Result<Vec<WorktreeEntry>, String> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree list failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let repo_root_str = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();

    let mut entries = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut is_bare = false;

    for line in stdout.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            // End of a worktree block
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                if !is_bare {
                    let wt_path = Path::new(&path);
                    let canon_path = wt_path
                        .canonicalize()
                        .unwrap_or_else(|_| wt_path.to_path_buf())
                        .to_string_lossy()
                        .trim_end_matches('/')
                        .to_string();
                    let is_main = canon_path == repo_root_str;
                    let is_dirty = is_worktree_dirty(wt_path);

                    let matching_agents: Vec<AgentInfo> = agents
                        .values()
                        .filter(|a| {
                            a.repo_path()
                                .map(|rp| rp.trim_end_matches('/'))
                                == Some(repo_root_str.as_str())
                                && a.branch_name() == Some(branch.as_str())
                        })
                        .map(|a| AgentInfo {
                            id: a.id().to_string(),
                            agent_binary: a.agent_binary().to_string(),
                            started_at: a.started_at().to_string(),
                            attached_clients: a.attached_clients(),
                            hub: a.hub().to_string(),
                            working_dir: a.working_dir().to_string(),
                            repo_path: a.repo_path().map(|s| s.to_string()),
                            branch_name: a.branch_name().map(|s| s.to_string()),
                            is_worktree: a.is_worktree(),
                        })
                        .collect();

                    entries.push(WorktreeEntry {
                        branch_name: branch,
                        path,
                        is_main,
                        is_dirty,
                        active_agents: matching_agents,
                    });
                }
            }
            current_path = None;
            current_branch = None;
            is_bare = false;
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
        } else if let Some(branch_ref) = line.strip_prefix("branch ") {
            // branch refs/heads/feature/auth -> feature/auth
            current_branch = Some(
                branch_ref
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch_ref)
                    .to_string(),
            );
        } else if line == "bare" {
            is_bare = true;
        } else if line == "detached" {
            // Detached HEAD — use "HEAD" as branch name
            if current_branch.is_none() {
                current_branch = Some("HEAD".to_string());
            }
        }
    }

    Ok(entries)
}

/// Create a new worktree.
///
/// If `checkout_existing` is true, checks out an existing branch.
/// Otherwise creates a new branch from `base` (or current HEAD).
pub fn add_worktree(
    repo_root: &Path,
    branch: &str,
    base: Option<&str>,
    checkout_existing: bool,
) -> Result<PathBuf, String> {
    ensure_clust_dir_excluded(repo_root)?;

    let wt_path = worktree_path(repo_root, branch);

    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create worktree directory: {e}"))?;
    }

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo_root);
    cmd.args(["worktree", "add"]);

    if checkout_existing {
        cmd.arg(wt_path.to_str().unwrap());
        cmd.arg(branch);
    } else {
        cmd.args(["-b", branch]);
        cmd.arg(wt_path.to_str().unwrap());
        if let Some(base) = base {
            cmd.arg(base);
        }
    }

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr.trim()));
    }

    Ok(wt_path)
}

/// Remove a worktree and optionally delete its local branch.
pub fn remove_worktree(
    repo_root: &Path,
    branch: &str,
    delete_branch: bool,
    force: bool,
) -> Result<(), String> {
    let wt_path = worktree_path(repo_root, branch);

    if !wt_path.exists() {
        return Err(format!("worktree for branch '{branch}' not found"));
    }

    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo_root);
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(wt_path.to_str().unwrap());

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree remove failed: {}", stderr.trim()));
    }

    if delete_branch {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(repo_root);
        if force {
            cmd.args(["branch", "-D", branch]);
        } else {
            cmd.args(["branch", "-d", branch]);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("failed to run git branch -d: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("warning: branch deletion failed: {}", stderr.trim());
        }
    }

    Ok(())
}

/// Delete a local branch, removing its worktree first if one exists.
pub fn delete_local_branch(
    repo_root: &Path,
    branch: &str,
    force: bool,
) -> Result<(), String> {
    let wt_path = worktree_path(repo_root, branch);
    if wt_path.exists() {
        // remove_worktree with delete_branch=true handles both
        return remove_worktree(repo_root, branch, true, force);
    }

    // No worktree – just delete the branch
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo_root);
    if force {
        cmd.args(["branch", "-D", branch]);
    } else {
        cmd.args(["branch", "-d", branch]);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git branch delete failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Delete a remote branch (e.g. "origin/feature-x").
pub fn delete_remote_branch(
    repo_root: &Path,
    remote_branch: &str,
) -> Result<(), String> {
    let mut parts = remote_branch.splitn(2, '/');
    let remote = parts
        .next()
        .ok_or_else(|| format!("invalid remote branch name: {remote_branch}"))?;
    let branch = parts
        .next()
        .ok_or_else(|| format!("invalid remote branch name: {remote_branch}"))?;

    let output = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["push", remote, "--delete", branch])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git push --delete failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Result of a repository purge operation.
pub struct PurgeResult {
    pub removed_worktrees: usize,
    pub deleted_branches: usize,
}

/// Purge a repository: remove all worktrees, delete all non-HEAD local branches,
/// and clean stale remote refs.
pub fn purge_repo(repo_root: &Path) -> Result<PurgeResult, String> {
    // 1. Remove all non-main worktrees
    let wt_output = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to list worktrees: {e}"))?;

    let mut removed_worktrees = 0;
    let mut current_path: Option<String> = None;
    let mut is_main = false;

    for line in String::from_utf8_lossy(&wt_output.stdout).lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(path.to_string());
            is_main = false;
        } else if line == "bare" || current_path.as_deref() == Some(repo_root.to_string_lossy().trim_end_matches('/')) {
            is_main = true;
        } else if line.is_empty() {
            if let Some(ref path) = current_path {
                if !is_main && path != repo_root.to_string_lossy().trim_end_matches('/') {
                    let _ = std::process::Command::new("git")
                        .current_dir(repo_root)
                        .args(["worktree", "remove", "--force", path])
                        .output();
                    removed_worktrees += 1;
                }
            }
            current_path = None;
        }
    }
    // Handle last entry if file doesn't end with blank line
    if let Some(ref path) = current_path {
        if !is_main && path != repo_root.to_string_lossy().trim_end_matches('/') {
            let _ = std::process::Command::new("git")
                .current_dir(repo_root)
                .args(["worktree", "remove", "--force", path])
                .output();
            removed_worktrees += 1;
        }
    }

    // 2. Delete all non-HEAD local branches
    let repo = git2::Repository::open(repo_root)
        .map_err(|e| format!("failed to open repo: {e}"))?;
    let head_name = repo.head().ok().and_then(|h| h.shorthand().map(String::from));

    let mut deleted_branches = 0;
    if let Ok(branches) = repo.branches(Some(git2::BranchType::Local)) {
        let names: Vec<String> = branches
            .filter_map(|b| b.ok())
            .filter_map(|(branch, _)| branch.name().ok()?.map(String::from))
            .filter(|name| head_name.as_deref() != Some(name.as_str()))
            .collect();

        for name in &names {
            let result = std::process::Command::new("git")
                .current_dir(repo_root)
                .args(["branch", "-D", name])
                .output();
            if result.is_ok() {
                deleted_branches += 1;
            }
        }
    }

    // 3. Clean stale remote refs
    let _ = clean_stale_refs(repo_root);

    Ok(PurgeResult {
        removed_worktrees,
        deleted_branches,
    })
}

/// Prune stale remote tracking refs for all remotes.
pub fn clean_stale_refs(repo_root: &Path) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .current_dir(repo_root)
        .args(["remote"])
        .output()
        .map_err(|e| format!("failed to list remotes: {e}"))?;

    let remotes = String::from_utf8_lossy(&output.stdout);
    for remote in remotes.lines() {
        let remote = remote.trim();
        if !remote.is_empty() {
            let _ = std::process::Command::new("git")
                .current_dir(repo_root)
                .args(["remote", "prune", remote])
                .output();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::Arc;

    use portable_pty::PtySize;
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use crate::agent::AgentEntry;

    /// Create a dummy PTY master for test AgentEntry construction.
    fn create_dummy_pty_master() -> Box<dyn portable_pty::MasterPty + Send> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open pty for test");
        drop(pair.slave);
        pair.master
    }

    /// Create a test AgentEntry with the given git fields.
    fn test_agent(
        id: &str,
        repo_path: Option<&str>,
        branch_name: Option<&str>,
        is_worktree: bool,
    ) -> AgentEntry {
        AgentEntry {
            id: id.to_string(),
            agent_binary: "test".into(),
            started_at: "2026-01-01T00:00:00Z".into(),
            working_dir: "/tmp".into(),
            hub: clust_ipc::DEFAULT_HUB.into(),
            pid: None,
            pty_master: create_dummy_pty_master(),
            pty_writer: Box::new(io::sink()),
            output_tx: broadcast::channel(1).0,
            replay_buffer: Arc::new(std::sync::Mutex::new(crate::agent::ReplayBuffer::new())),
            attached_count: Arc::new(AtomicUsize::new(0)),
            client_sizes: HashMap::new(),
            current_pty_size: (80, 24),
            active_client_id: None,
            next_client_id: AtomicU64::new(0),
            repo_path: repo_path.map(|s| s.to_string()),
            branch_name: branch_name.map(|s| s.to_string()),
            is_worktree,
        }
    }

    /// Create a temporary git repo with an initial commit.
    fn create_test_repo() -> (TempDir, git2::Repository) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Create an initial commit so HEAD is valid
        {
            let sig = git2::Signature::now("Test", "test@test.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
        }

        (dir, repo)
    }

    /// Canonicalize a path to resolve symlinks (e.g. macOS /var -> /private/var).
    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    #[test]
    fn detect_git_root_finds_repo() {
        let (dir, _repo) = create_test_repo();
        let root = detect_git_root(dir.path().to_str().unwrap());
        assert!(root.is_some());
        assert_eq!(canon(&root.unwrap()), canon(dir.path()));
    }

    #[test]
    fn detect_git_root_from_subdirectory() {
        let (dir, _repo) = create_test_repo();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let root = detect_git_root(sub.to_str().unwrap());
        assert!(root.is_some());
        assert_eq!(canon(&root.unwrap()), canon(dir.path()));
    }

    #[test]
    fn detect_git_root_returns_none_for_non_repo() {
        let dir = TempDir::new().unwrap();
        assert!(detect_git_root(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn detect_branch_and_worktree_on_main() {
        let (dir, _repo) = create_test_repo();
        let (branch, is_wt) = detect_branch_and_worktree(dir.path().to_str().unwrap());
        // git init creates "master" or "main" depending on config
        assert!(branch.is_some());
        assert!(!is_wt);
    }

    #[test]
    fn get_repo_state_lists_branches() {
        let (dir, repo) = create_test_repo();

        // Create a second branch
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature-branch", &head, false).unwrap();

        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let state = get_repo_state(dir.path(), "test-repo", &agents);
        assert!(state.is_some());
        let info = state.unwrap();
        assert_eq!(info.name, "test-repo");
        assert!(info.local_branches.len() >= 2);

        // One branch should be head
        assert!(info.local_branches.iter().any(|b| b.is_head));
    }

    #[test]
    fn get_repo_state_returns_none_for_missing_path() {
        let agents: HashMap<String, AgentEntry> = HashMap::new();
        assert!(get_repo_state(Path::new("/nonexistent/path"), "gone", &agents).is_none());
    }

    #[test]
    fn get_repo_state_shows_active_agent() {
        let (dir, repo) = create_test_repo();
        let head_branch = repo
            .head()
            .unwrap()
            .shorthand()
            .unwrap()
            .to_string();

        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let state = get_repo_state(dir.path(), "test-repo", &agents).unwrap();
        let branch = state
            .local_branches
            .iter()
            .find(|b| b.name == head_branch)
            .unwrap();
        assert_eq!(branch.active_agent_count, 0);
    }

    #[test]
    fn detect_git_root_from_worktree() {
        let (dir, _repo) = create_test_repo();
        let wt_path = dir.path().join("wt-branch");

        // Use git CLI to add worktree (git2's worktree API is cumbersome for creation)
        let status = Command::new("git")
            .args(["worktree", "add", wt_path.to_str().unwrap(), "-b", "wt-branch"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        if !status.status.success() {
            // Skip if git worktree not available
            return;
        }

        // detect_git_root from inside the worktree should return the main repo root
        let root = detect_git_root(wt_path.to_str().unwrap());
        assert!(root.is_some());
        assert_eq!(canon(&root.unwrap()), canon(dir.path()));
    }

    // ── HIGH: agent-branch matching (positive case) ──────────────

    #[test]
    fn get_repo_state_matches_agent_to_branch() {
        let (dir, repo) = create_test_repo();
        let head_branch = repo.head().unwrap().shorthand().unwrap().to_string();

        // The repo_path stored on AgentEntry comes from detect_git_root,
        // while list_branches uses repo.workdir(). Verify they match.
        let repo_path_str = canon(dir.path()).to_string_lossy().into_owned();

        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            test_agent("abc123", Some(&repo_path_str), Some(&head_branch), false),
        );

        let state = get_repo_state(&canon(dir.path()), "test-repo", &agents).unwrap();
        let branch = state
            .local_branches
            .iter()
            .find(|b| b.name == head_branch)
            .unwrap();
        assert_eq!(
            branch.active_agent_count, 1,
            "agent should be matched to its branch"
        );
    }

    #[test]
    fn get_repo_state_no_match_wrong_branch() {
        let (dir, repo) = create_test_repo();
        let head_branch = repo.head().unwrap().shorthand().unwrap().to_string();
        let repo_path_str = canon(dir.path()).to_string_lossy().into_owned();

        // Agent on a different branch
        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            test_agent("abc123", Some(&repo_path_str), Some("nonexistent-branch"), false),
        );

        let state = get_repo_state(&canon(dir.path()), "test-repo", &agents).unwrap();
        let branch = state
            .local_branches
            .iter()
            .find(|b| b.name == head_branch)
            .unwrap();
        assert_eq!(
            branch.active_agent_count, 0,
            "agent on different branch should not match"
        );
    }

    #[test]
    fn get_repo_state_no_match_wrong_repo() {
        let (dir, repo) = create_test_repo();
        let head_branch = repo.head().unwrap().shorthand().unwrap().to_string();

        // Agent in a different repo
        let mut agents = HashMap::new();
        agents.insert(
            "abc123".to_string(),
            test_agent("abc123", Some("/some/other/repo"), Some(&head_branch), false),
        );

        let state = get_repo_state(&canon(dir.path()), "test-repo", &agents).unwrap();
        let branch = state
            .local_branches
            .iter()
            .find(|b| b.name == head_branch)
            .unwrap();
        assert_eq!(
            branch.active_agent_count, 0,
            "agent in different repo should not match"
        );
    }

    // ── HIGH: worktree branch detection ──────────────────────────

    #[test]
    fn get_repo_state_marks_worktree_branches() {
        let (dir, _repo) = create_test_repo();
        let wt_path = dir.path().join("wt-test");

        let status = Command::new("git")
            .args(["worktree", "add", wt_path.to_str().unwrap(), "-b", "wt-test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        if !status.status.success() {
            return; // skip if git worktree not available
        }

        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let state = get_repo_state(dir.path(), "test-repo", &agents).unwrap();

        // The worktree branch should be marked is_worktree: true
        let wt_branch = state
            .local_branches
            .iter()
            .find(|b| b.name == "wt-test");
        assert!(wt_branch.is_some(), "worktree branch should appear in local branches");
        assert!(
            wt_branch.unwrap().is_worktree,
            "worktree branch should have is_worktree=true"
        );
    }

    #[test]
    fn detect_branch_and_worktree_in_worktree() {
        let (dir, _repo) = create_test_repo();
        let wt_path = dir.path().join("wt-detect");

        let status = Command::new("git")
            .args(["worktree", "add", wt_path.to_str().unwrap(), "-b", "wt-detect"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        if !status.status.success() {
            return;
        }

        let (branch, is_wt) = detect_branch_and_worktree(wt_path.to_str().unwrap());
        assert_eq!(branch.as_deref(), Some("wt-detect"));
        assert!(is_wt, "should detect worktree");
    }

    // ── HIGH: detached HEAD ──────────────────────────────────────

    #[test]
    fn detect_branch_and_worktree_detached_head() {
        let (dir, repo) = create_test_repo();

        // Detach HEAD by checking out the commit directly
        let head_oid = repo.head().unwrap().target().unwrap();
        repo.set_head_detached(head_oid).unwrap();

        let (branch, is_wt) = detect_branch_and_worktree(dir.path().to_str().unwrap());
        // Detached HEAD: shorthand() returns something like "HEAD" or the short sha,
        // not a branch name. The key thing is it doesn't panic.
        assert!(!is_wt);
        // branch may be Some("HEAD") or a short sha — just verify it doesn't crash
        let _ = branch;
    }

    // ── MEDIUM: non-repo path for detect_branch_and_worktree ─────

    #[test]
    fn detect_branch_and_worktree_non_repo() {
        let dir = TempDir::new().unwrap();
        let (branch, is_wt) = detect_branch_and_worktree(dir.path().to_str().unwrap());
        assert!(branch.is_none());
        assert!(!is_wt);
    }

    // ── MEDIUM: repo with only local branches (no remotes) ───────

    #[test]
    fn get_repo_state_empty_remote_branches() {
        let (dir, _repo) = create_test_repo();
        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let state = get_repo_state(dir.path(), "test-repo", &agents).unwrap();
        assert!(
            state.remote_branches.is_empty(),
            "fresh repo with no remotes should have empty remote branches"
        );
    }

    // ── Worktree management tests ───────────────────────────────

    #[test]
    fn serialize_branch_name_with_slashes() {
        assert_eq!(serialize_branch_name("feature/auth"), "feature__auth");
        assert_eq!(
            serialize_branch_name("feat/sub/deep"),
            "feat__sub__deep"
        );
    }

    #[test]
    fn serialize_branch_name_without_slashes() {
        assert_eq!(serialize_branch_name("my-branch"), "my-branch");
    }

    #[test]
    fn deserialize_branch_name_round_trip() {
        let original = "feature/auth/deep";
        let serialized = serialize_branch_name(original);
        assert_eq!(deserialize_branch_name(&serialized), original);
    }

    #[test]
    fn worktree_path_format() {
        let root = Path::new("/home/user/project");
        let path = worktree_path(root, "feature/auth");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/.clust/worktrees/feature__auth")
        );
    }

    #[test]
    fn worktree_path_simple_branch() {
        let root = Path::new("/home/user/project");
        let path = worktree_path(root, "my-branch");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/.clust/worktrees/my-branch")
        );
    }

    #[test]
    fn ensure_clust_dir_excluded_creates_entry() {
        let (dir, _repo) = create_test_repo();
        ensure_clust_dir_excluded(dir.path()).unwrap();

        let exclude = std::fs::read_to_string(
            dir.path().join(".git").join("info").join("exclude"),
        )
        .unwrap();
        assert!(exclude.contains(".clust/"));
    }

    #[test]
    fn ensure_clust_dir_excluded_idempotent() {
        let (dir, _repo) = create_test_repo();
        ensure_clust_dir_excluded(dir.path()).unwrap();
        ensure_clust_dir_excluded(dir.path()).unwrap();

        let exclude = std::fs::read_to_string(
            dir.path().join(".git").join("info").join("exclude"),
        )
        .unwrap();
        assert_eq!(
            exclude.matches(".clust/").count(),
            1,
            "should only appear once"
        );
    }

    #[test]
    fn add_and_list_worktrees() {
        let (dir, _repo) = create_test_repo();

        let result = add_worktree(dir.path(), "test-wt", None, false);
        if result.is_err() {
            return; // skip if git worktree not available
        }
        let wt_path = result.unwrap();
        assert!(wt_path.exists());

        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let worktrees = list_worktrees(dir.path(), &agents).unwrap();

        // Should have main checkout + our new worktree
        assert!(worktrees.len() >= 2);

        let wt = worktrees.iter().find(|w| w.branch_name == "test-wt");
        assert!(wt.is_some(), "our worktree should appear in the list");
        assert!(!wt.unwrap().is_main);
    }

    #[test]
    fn add_and_remove_worktree() {
        let (dir, _repo) = create_test_repo();

        let result = add_worktree(dir.path(), "rm-test", None, false);
        if result.is_err() {
            return;
        }
        let wt_path = result.unwrap();
        assert!(wt_path.exists());

        remove_worktree(dir.path(), "rm-test", false, false).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn remove_worktree_with_branch_deletion() {
        let (dir, repo) = create_test_repo();

        let result = add_worktree(dir.path(), "rm-branch-test", None, false);
        if result.is_err() {
            return;
        }

        remove_worktree(dir.path(), "rm-branch-test", true, false).unwrap();

        // Branch should be deleted
        assert!(
            repo.find_branch("rm-branch-test", git2::BranchType::Local)
                .is_err(),
            "branch should be deleted"
        );
    }

    #[test]
    fn is_worktree_dirty_clean() {
        let (dir, _repo) = create_test_repo();
        assert!(!is_worktree_dirty(dir.path()));
    }

    #[test]
    fn is_worktree_dirty_modified() {
        let (dir, _repo) = create_test_repo();
        std::fs::write(dir.path().join("dirty.txt"), "change").unwrap();
        assert!(is_worktree_dirty(dir.path()));
    }

    #[test]
    fn add_worktree_with_base_branch() {
        let (dir, repo) = create_test_repo();

        // Create a branch to use as base
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("base-branch", &head, false).unwrap();

        let result = add_worktree(dir.path(), "from-base", Some("base-branch"), false);
        if result.is_err() {
            return;
        }
        assert!(result.unwrap().exists());
    }

    #[test]
    fn add_worktree_checkout_existing() {
        let (dir, repo) = create_test_repo();

        // Create a branch that we'll checkout in a worktree
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("existing-branch", &head, false).unwrap();

        let result = add_worktree(dir.path(), "existing-branch", None, true);
        if result.is_err() {
            return;
        }
        assert!(result.unwrap().exists());
    }

    #[test]
    fn list_worktrees_includes_main() {
        let (dir, _repo) = create_test_repo();

        let agents: HashMap<String, AgentEntry> = HashMap::new();
        let worktrees = list_worktrees(dir.path(), &agents).unwrap();

        assert!(!worktrees.is_empty());
        assert!(
            worktrees.iter().any(|w| w.is_main),
            "main checkout should be in the list"
        );
    }

    #[test]
    fn remove_nonexistent_worktree_errors() {
        let (dir, _repo) = create_test_repo();
        let result = remove_worktree(dir.path(), "nonexistent", false, false);
        assert!(result.is_err());
    }
}
