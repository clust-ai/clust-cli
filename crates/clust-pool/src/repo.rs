use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clust_ipc::{BranchInfo, RepoInfo};

use crate::agent::AgentEntry;

/// Trait for accessing agent matching fields without requiring the full AgentEntry.
/// This allows repo queries to use a lightweight snapshot taken outside the pool lock.
pub trait AgentMatcher {
    fn repo_path(&self) -> Option<&str>;
    fn branch_name(&self) -> Option<&str>;
}

impl AgentMatcher for AgentEntry {
    fn repo_path(&self) -> Option<&str> {
        self.repo_path.as_deref()
    }
    fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
    }
}

/// Lightweight snapshot of agent fields needed for repo state queries.
pub(crate) struct AgentSnapshot {
    pub repo_path: Option<String>,
    pub branch_name: Option<String>,
}

impl AgentMatcher for AgentSnapshot {
    fn repo_path(&self) -> Option<&str> {
        self.repo_path.as_deref()
    }
    fn branch_name(&self) -> Option<&str> {
        self.branch_name.as_deref()
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
        for wt_name in worktree_names.iter() {
            if let Some(name) = wt_name {
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
    }

    branches
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
            pool: clust_ipc::DEFAULT_POOL.into(),
            pid: None,
            pty_master: create_dummy_pty_master(),
            pty_writer: Box::new(io::sink()),
            output_tx: broadcast::channel(1).0,
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
}
