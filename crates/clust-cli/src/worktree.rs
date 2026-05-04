use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};

use crate::ipc;
use crate::theme;

/// Info about a worktree that may need cleanup after agent stop.
pub struct WorktreeCleanup {
    pub repo_path: String,
    pub branch_name: String,
}

/// Result of the worktree cleanup selector.
enum CleanupChoice {
    Keep,
    DiscardWorktree,
    DiscardWorktreeAndBranch,
}

// ── Collection ─────────────────────────────────────────────────────

/// Extract unique worktree entries from an agent list.
///
/// Only includes a worktree if **all** agents in it are being stopped
/// (i.e., no agents in `all_agents` remain outside of `stopped_agents`
/// for that worktree).
pub fn collect_worktree_cleanups(
    stopped_agents: &[clust_ipc::AgentInfo],
    all_agents: &[clust_ipc::AgentInfo],
) -> Vec<WorktreeCleanup> {
    // Collect unique worktrees from stopped agents
    let mut seen = HashSet::new();
    let mut candidates: Vec<(String, String)> = Vec::new();

    for agent in stopped_agents {
        if !agent.is_worktree {
            continue;
        }
        if let (Some(repo), Some(branch)) = (&agent.repo_path, &agent.branch_name) {
            let key = (repo.clone(), branch.clone());
            if seen.insert(key.clone()) {
                candidates.push(key);
            }
        }
    }

    // Build set of stopped agent IDs for fast lookup
    let stopped_ids: HashSet<&str> = stopped_agents.iter().map(|a| a.id.as_str()).collect();

    // Filter: only keep worktrees where no agents remain running
    candidates
        .into_iter()
        .filter(|(repo, branch)| {
            !all_agents.iter().any(|a| {
                a.repo_path.as_deref() == Some(repo.as_str())
                    && a.branch_name.as_deref() == Some(branch.as_str())
                    && !stopped_ids.contains(a.id.as_str())
            })
        })
        .map(|(repo_path, branch_name)| WorktreeCleanup {
            repo_path,
            branch_name,
        })
        .collect()
}

/// Query the hub for the current agent list and check whether the given
/// worktree still has other agents running. Returns a single-element vec
/// if the worktree is eligible for cleanup, empty otherwise.
pub async fn query_and_collect_worktree_cleanups_for_agent(
    repo_path: &str,
    branch_name: &str,
) -> Vec<WorktreeCleanup> {
    let agents = match fetch_agent_list().await {
        Some(agents) => agents,
        None => {
            // Hub unreachable — the agent was the last one, safe to offer cleanup
            return vec![WorktreeCleanup {
                repo_path: repo_path.to_string(),
                branch_name: branch_name.to_string(),
            }];
        }
    };

    // If any agent still runs in this worktree, skip
    let still_running = agents.iter().any(|a| {
        a.repo_path.as_deref() == Some(repo_path) && a.branch_name.as_deref() == Some(branch_name)
    });

    if still_running {
        vec![]
    } else {
        vec![WorktreeCleanup {
            repo_path: repo_path.to_string(),
            branch_name: branch_name.to_string(),
        }]
    }
}

/// Fetch the agent list from the hub. Returns None if hub is unreachable.
async fn fetch_agent_list() -> Option<Vec<clust_ipc::AgentInfo>> {
    let agents = ipc::fetch_agent_list().await;
    if agents.is_empty() {
        // Could be genuinely empty or hub unreachable — check connectivity
        if ipc::try_connect().await.is_err() {
            return None;
        }
    }
    Some(agents)
}

// ── Prompting ──────────────────────────────────────────────────────

/// Prompt the user about each worktree and execute the chosen action.
pub fn prompt_worktree_cleanup(worktrees: &[WorktreeCleanup]) {
    if worktrees.is_empty() {
        return;
    }

    println!();

    for wt in worktrees {
        let dirty = is_worktree_dirty(&wt.repo_path, &wt.branch_name);
        let choice = run_worktree_selector(&wt.branch_name, dirty);

        match choice {
            CleanupChoice::Keep => {}
            CleanupChoice::DiscardWorktree => {
                match remove_worktree_local(&wt.repo_path, &wt.branch_name, false) {
                    Ok(()) => {
                        println!(
                            "  {}✔{} {}worktree discarded{}\n",
                            theme::SUCCESS,
                            theme::RESET,
                            theme::TEXT_PRIMARY,
                            theme::RESET,
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "  {}✘{} {}failed to discard worktree: {e}{}\n",
                            theme::ERROR,
                            theme::RESET,
                            theme::TEXT_PRIMARY,
                            theme::RESET,
                        );
                    }
                }
            }
            CleanupChoice::DiscardWorktreeAndBranch => {
                match remove_worktree_local(&wt.repo_path, &wt.branch_name, true) {
                    Ok(()) => {
                        println!(
                            "  {}✔{} {}worktree and branch discarded{}\n",
                            theme::SUCCESS,
                            theme::RESET,
                            theme::TEXT_PRIMARY,
                            theme::RESET,
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "  {}✘{} {}failed to discard worktree: {e}{}\n",
                            theme::ERROR,
                            theme::RESET,
                            theme::TEXT_PRIMARY,
                            theme::RESET,
                        );
                    }
                }
            }
        }
    }
}

// ── Selector UI ────────────────────────────────────────────────────

const OPTIONS: [&str; 3] = ["keep", "discard worktree", "discard worktree + branch"];

fn run_worktree_selector(branch_name: &str, dirty: bool) -> CleanupChoice {
    let mut stdout = io::stdout();

    // Header
    writeln!(
        stdout,
        "  {}worktree '{}'{}{}",
        theme::TEXT_SECONDARY,
        branch_name,
        if dirty {
            format!(
                " {}⚠ has uncommitted changes{}",
                theme::WARNING,
                theme::RESET
            )
        } else {
            String::new()
        },
        theme::RESET,
    )
    .unwrap();
    stdout.flush().unwrap();

    let _guard = RawModeGuard::new();
    let mut selected: usize = 0;

    render_worktree_selector(&mut stdout, selected);

    loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => continue,
        };
        let Event::Key(key) = ev else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if selected < OPTIONS.len() - 1 {
                    selected += 1;
                }
            }
            KeyCode::Enter => break,
            KeyCode::Esc | KeyCode::Char('q') => {
                selected = 0; // keep
                break;
            }
            _ => continue,
        }

        // Move cursor up to re-render
        write!(stdout, "\x1b[{}A", OPTIONS.len()).unwrap();
        render_worktree_selector(&mut stdout, selected);
    }

    // Erase the selector lines + header
    let total_lines = OPTIONS.len() + 1;
    write!(stdout, "\x1b[{}A", OPTIONS.len()).unwrap();
    for _ in 0..total_lines {
        write!(stdout, "\x1b[2K\x1b[1A").unwrap();
    }
    write!(stdout, "\x1b[2K").unwrap();
    stdout.flush().unwrap();

    // _guard drops here, restoring terminal

    match selected {
        1 => CleanupChoice::DiscardWorktree,
        2 => CleanupChoice::DiscardWorktreeAndBranch,
        _ => CleanupChoice::Keep,
    }
}

fn render_worktree_selector(stdout: &mut io::Stdout, selected: usize) {
    for (i, label) in OPTIONS.iter().enumerate() {
        let is_selected = i == selected;
        let prefix = if is_selected {
            format!("  {}▸{} ", theme::ACCENT, theme::RESET)
        } else {
            "    ".to_string()
        };
        let color = if is_selected {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_TERTIARY
        };
        write!(
            stdout,
            "\x1b[2K{}{}{}{}\r\n",
            prefix,
            color,
            label,
            theme::RESET
        )
        .unwrap();
    }
    stdout.flush().unwrap();
}

/// Ensures raw mode and cursor visibility are restored on drop.
struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Self {
        crossterm::terminal::enable_raw_mode().unwrap();
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[?25l").unwrap(); // hide cursor
        stdout.flush().unwrap();
        RawModeGuard
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b[?25h"); // show cursor
        let _ = stdout.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

// ── Git operations ─────────────────────────────────────────────────

/// Check if a worktree has uncommitted changes.
pub fn is_worktree_dirty(repo_path: &str, branch: &str) -> bool {
    let serialized = branch.replace('/', "__");
    let wt_path = Path::new(repo_path)
        .join(".clust")
        .join("worktrees")
        .join(&serialized);

    if !wt_path.exists() {
        return false;
    }

    let output = std::process::Command::new("git")
        .current_dir(&wt_path)
        .args(["status", "--porcelain"])
        .output();

    match output {
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => false,
    }
}

/// Remove a worktree locally using git commands.
fn remove_worktree_local(repo_path: &str, branch: &str, delete_branch: bool) -> Result<(), String> {
    let serialized = branch.replace('/', "__");
    let wt_path = Path::new(repo_path)
        .join(".clust")
        .join("worktrees")
        .join(&serialized);

    if !wt_path.exists() {
        return Err(format!("worktree for branch '{branch}' not found"));
    }

    let output = std::process::Command::new("git")
        .current_dir(repo_path)
        .args(["worktree", "remove", "--force"])
        .arg(wt_path.to_str().unwrap())
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree remove failed: {}", stderr.trim()));
    }

    if delete_branch {
        let output = std::process::Command::new("git")
            .current_dir(repo_path)
            .args(["branch", "-D", branch])
            .output()
            .map_err(|e| format!("failed to run git branch -D: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "  {}⚠{} {}branch deletion failed: {}{}",
                theme::WARNING,
                theme::RESET,
                theme::TEXT_SECONDARY,
                stderr.trim(),
                theme::RESET,
            );
        }
    }

    Ok(())
}
