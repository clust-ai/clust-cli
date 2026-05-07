use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;

use clust_ipc::{CliMessage, HubMessage};

use crate::agent::{self, AgentEvent, SharedHubState};
use crate::ShutdownSignal;

/// Run the IPC server, listening for CLI connections on the Unix domain socket.
/// Runs inside a tokio runtime on a background thread.
pub async fn run_ipc_server(shutdown_signal: Arc<dyn ShutdownSignal>, state: SharedHubState) {
    if let Err(e) = run(shutdown_signal, state).await {
        eprintln!("ipc server error: {e}");
    }
}

async fn run(shutdown_signal: Arc<dyn ShutdownSignal>, state: SharedHubState) -> io::Result<()> {
    let dir = clust_ipc::clust_dir();
    tokio::fs::create_dir_all(&dir).await?;

    // Initialize SQLite database (creates tables on first run). Once the DB
    // is open, run scheduler-recovery so any task that was `active` when the
    // hub last died is rewritten to `aborted` (its agent process is gone).
    {
        let mut hub = state.lock().await;
        if let Err(e) = hub.init_db() {
            eprintln!("database init failed: {e}");
        }
        if let Some(ref conn) = hub.db {
            match crate::db::recover_active_scheduled_tasks(conn) {
                Ok(0) => {}
                Ok(n) => eprintln!("[hub] recovered {n} active scheduled task(s) → aborted"),
                Err(e) => eprintln!("[hub] failed to recover active scheduled tasks: {e}"),
            }
        }
    }

    // Spawn the per-task scheduler. Lives for the rest of the hub process and
    // is the sole writer that flips Inactive → Active for time- and dep-driven
    // tasks. Manual `StartScheduledTaskNow` / `RestartScheduledTask` paths use
    // the same helper but bypass the polling delay.
    {
        let scheduler_state = state.clone();
        tokio::spawn(crate::scheduler::run_scheduler(scheduler_state));
    }

    // Ensure .clust/worktrees is in the global git exclude file
    if let Err(e) = crate::repo::ensure_global_worktree_exclude() {
        eprintln!("global git exclude setup failed: {e}");
    }

    let sock_path = clust_ipc::socket_path();

    // Remove stale socket file if it exists (crash recovery per docs/hub.md)
    let _ = tokio::fs::remove_file(&sock_path).await;

    let listener = UnixListener::bind(&sock_path)?;

    loop {
        let (stream, _addr) = listener.accept().await?;

        let signal = shutdown_signal.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, signal, state).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

/// Compare two canonical paths component-by-component. Returns `true` iff
/// `ancestor` is `path` or a strict ancestor of `path`.
///
/// Unlike `Path::starts_with`, which compares the underlying byte string,
/// this only matches whole path components — so `/tmp/foo` is NOT considered
/// an ancestor of `/tmp/foo-bar`.
fn is_canonical_ancestor(ancestor: &std::path::Path, path: &std::path::Path) -> bool {
    let mut a = ancestor.components();
    let mut p = path.components();
    loop {
        match (a.next(), p.next()) {
            (None, _) => return true,        // ancestor exhausted → match
            (Some(_), None) => return false, // path exhausted before ancestor
            (Some(ac), Some(pc)) if ac == pc => continue,
            _ => return false,
        }
    }
}

/// Canonicalize a path that should already exist. Returns an error rather
/// than falling back to the non-canonical path; callers must know the exact
/// path on disk before performing a destructive operation.
fn canonicalize_existing(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !path.exists() {
        return Err(format!("path does not exist: {}", path.display()));
    }
    std::fs::canonicalize(path).map_err(|e| format!("could not resolve {}: {e}", path.display()))
}

/// Verify that `target` is safe to recursively delete. Returns the canonical
/// path on success.
///
/// Refuses the filesystem root, the user's home directory, the clust state
/// directory, and any ancestor of either. Comparisons are component-aware
/// so prefix tricks like `/tmp/foo` vs `/tmp/foo-bar` are rejected.
fn check_safe_to_delete(target: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let canonical = canonicalize_existing(target)?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    if canonical.parent().is_none() {
        return Err("refusing to delete the filesystem root".into());
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_path = std::path::PathBuf::from(home);
        if home_path.exists() {
            let home_canonical = canonicalize_existing(&home_path)?;
            if canonical == home_canonical {
                return Err("refusing to delete the home directory".into());
            }
            if is_canonical_ancestor(&canonical, &home_canonical) {
                return Err(
                    "refusing to delete a directory that contains the home directory".into(),
                );
            }
        }
    }
    let clust_dir = clust_ipc::clust_dir();
    if clust_dir.exists() {
        let clust_canonical = canonicalize_existing(&clust_dir)?;
        if canonical == clust_canonical || is_canonical_ancestor(&canonical, &clust_canonical) {
            return Err("refusing to delete the clust state directory".into());
        }
    }
    Ok(canonical)
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    shutdown_signal: Arc<dyn ShutdownSignal>,
    state: SharedHubState,
) -> io::Result<()> {
    // Split for bidirectional streaming; first message determines the mode.
    let (mut reader, mut writer) = stream.into_split();
    let msg: CliMessage = clust_ipc::recv_message_read(&mut reader).await?;

    match msg {
        CliMessage::StartAgent {
            prompt,
            agent_binary,
            working_dir,
            cols,
            rows,
            accept_edits,
            plan_mode,
            allow_bypass,
            hub,
        } => {
            // Detect git info BEFORE acquiring the lock (avoid holding lock during I/O)
            let working_dir_for_register = working_dir.clone();
            let (repo_path, branch_name, is_worktree) =
                match crate::repo::detect_git_root(&working_dir) {
                    Some(root) => {
                        let rp = root.to_string_lossy().into_owned();
                        let (bn, iw) = crate::repo::detect_branch_and_worktree(&working_dir);
                        (Some(rp), bn, iw)
                    }
                    None => (None, None, false),
                };
            let result = {
                let mut hub_state = state.lock().await;
                agent::spawn_agent(
                    &mut hub_state,
                    agent::SpawnAgentParams {
                        prompt,
                        agent_binary,
                        working_dir,
                        cols,
                        rows,
                        accept_edits,
                        plan_mode,
                        allow_bypass,
                        hub,
                        repo_path,
                        branch_name,
                        is_worktree,
                        exit_when_done: false,
                    },
                    state.clone(),
                )
            };
            match result {
                Ok((id, binary)) => {
                    // Auto-register repo from working_dir
                    {
                        let hub_state = state.lock().await;
                        if let Some(ref db) = hub_state.db {
                            if let Some(root) =
                                crate::repo::detect_git_root(&working_dir_for_register)
                            {
                                let root_str = root.to_string_lossy().into_owned();
                                let name = root
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| root_str.clone());
                                let color = crate::db::next_repo_color(db);
                                let _ = crate::db::register_repo(db, &root_str, &name, color);
                            }
                        }
                    }
                    let (ag_is_worktree, ag_repo_path, ag_branch_name) = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .get(&id)
                            .map(|e| (e.is_worktree, e.repo_path.clone(), e.branch_name.clone()))
                            .unwrap_or((false, None, None))
                    };
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentStarted {
                            id: id.clone(),
                            agent_binary: binary,
                            is_worktree: ag_is_worktree,
                            repo_path: ag_repo_path,
                            branch_name: ag_branch_name,
                        },
                    )
                    .await?;
                    handle_attached_session(&id, reader, writer, state).await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::AttachAgent { id } => {
            let agent_info = {
                let hub = state.lock().await;
                hub.agents.get(&id).map(|e| {
                    (
                        e.agent_binary.clone(),
                        e.is_worktree,
                        e.repo_path.clone(),
                        e.branch_name.clone(),
                    )
                })
            };
            match agent_info {
                Some((binary, ag_is_worktree, ag_repo_path, ag_branch_name)) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentAttached {
                            id: id.clone(),
                            agent_binary: binary,
                            is_worktree: ag_is_worktree,
                            repo_path: ag_repo_path,
                            branch_name: ag_branch_name,
                        },
                    )
                    .await?;
                    handle_attached_session(&id, reader, writer, state).await?;
                }
                None => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error {
                            message: format!("agent {id} not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        CliMessage::ListAgents { hub: filter } => {
            let agents = {
                let hub_state = state.lock().await;

                hub_state
                    .agents
                    .values()
                    .filter(|e| filter.as_ref().is_none_or(|f| &e.hub == f))
                    .map(|e| clust_ipc::AgentInfo {
                        id: e.id.clone(),
                        agent_binary: e.agent_binary.clone(),
                        started_at: e.started_at.clone(),
                        attached_clients: e.attached_count.load(Ordering::Relaxed),
                        hub: e.hub.clone(),
                        working_dir: e.working_dir.clone(),
                        repo_path: e.repo_path.clone(),
                        branch_name: e.branch_name.clone(),
                        is_worktree: e.is_worktree,
                        auto_exit: e.auto_exit,
                        plan_mode: e.plan_mode,
                        prompt: e.prompt.clone(),
                    })
                    .collect()
            };
            clust_ipc::send_message_write(&mut writer, &HubMessage::AgentList { agents }).await?;
        }
        CliMessage::StopHub => {
            clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;

            // Terminate all running agents (SIGTERM → 3s → SIGKILL)
            agent::shutdown_agents(&state).await;

            // Clean up socket file before signaling shutdown
            let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;

            shutdown_signal.signal_shutdown();
        }
        CliMessage::StopAgent { id } => {
            let exists = {
                let hub = state.lock().await;
                hub.agents.contains_key(&id)
            };
            if exists {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::AgentStopped { id: id.clone() },
                )
                .await?;
                // Spawn so the 3s grace period doesn't block the connection handler
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = agent::stop_agent(&state, &id).await;
                });
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("agent {id} not found"),
                    },
                )
                .await?;
            }
        }
        CliMessage::SetDefault { agent_binary } => {
            let result = {
                let hub = state.lock().await;
                if let Some(ref db) = hub.db {
                    crate::db::set_default_agent(db, &agent_binary)
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok(()) => {
                    let mut hub = state.lock().await;
                    hub.default_agent = Some(agent_binary);
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::GetDefault => {
            let default = {
                let hub = state.lock().await;
                hub.default_agent.clone()
            };
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::DefaultAgent {
                    agent_binary: default,
                },
            )
            .await?;
        }
        CliMessage::SetBypassPermissions { enabled } => {
            let result = {
                let hub = state.lock().await;
                if let Some(ref db) = hub.db {
                    crate::db::set_bypass_permissions(db, enabled)
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok(()) => {
                    let mut hub = state.lock().await;
                    hub.bypass_permissions = enabled;
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::GetBypassPermissions => {
            let enabled = {
                let hub = state.lock().await;
                hub.bypass_permissions
            };
            clust_ipc::send_message_write(&mut writer, &HubMessage::BypassPermissions { enabled })
                .await?;
        }
        CliMessage::RegisterRepo { path } => {
            // Detect git root BEFORE acquiring the lock (avoid holding lock during I/O)
            let git_root = crate::repo::detect_git_root(&path);
            let result = {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    match git_root {
                        Some(root) => {
                            let root_str = root.to_string_lossy().into_owned();
                            let name = root
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| root_str.clone());
                            let color = crate::db::next_repo_color(db);
                            crate::db::register_repo(db, &root_str, &name, color)
                                .map(|_| (root_str, name))
                        }
                        None => Err(format!("{path} is not inside a git repository")),
                    }
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok((path, name)) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoRegistered { path, name },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::ListRepos => {
            // Collect repo list and agent snapshot under lock, then release
            let (repo_list, agent_snapshots) = {
                let hub_state = state.lock().await;
                let list = if let Some(ref db) = hub_state.db {
                    crate::db::list_repos(db).unwrap_or_default()
                } else {
                    vec![]
                };
                let snapshots: std::collections::HashMap<String, crate::repo::AgentSnapshot> =
                    hub_state
                        .agents
                        .iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                crate::repo::AgentSnapshot {
                                    id: v.id.clone(),
                                    agent_binary: v.agent_binary.clone(),
                                    started_at: v.started_at.clone(),
                                    attached_clients: v
                                        .attached_count
                                        .load(std::sync::atomic::Ordering::Relaxed),
                                    hub: v.hub.clone(),
                                    working_dir: v.working_dir.clone(),
                                    repo_path: v.repo_path.clone(),
                                    branch_name: v.branch_name.clone(),
                                    is_worktree: v.is_worktree,
                                    auto_exit: v.auto_exit,
                                    plan_mode: v.plan_mode,
                                    prompt: v.prompt.clone(),
                                },
                            )
                        })
                        .collect();
                (list, snapshots)
            };
            // Do git I/O outside the lock
            let mut valid_repos = Vec::new();
            let mut stale_paths = Vec::new();
            for (path, name, color, editor) in repo_list {
                match crate::repo::get_repo_state(
                    std::path::Path::new(&path),
                    &name,
                    &agent_snapshots,
                ) {
                    Some(mut info) => {
                        info.color = color;
                        info.editor = editor;
                        valid_repos.push(info);
                    }
                    None => stale_paths.push(path),
                }
            }
            // Clean up stale repos under lock
            if !stale_paths.is_empty() {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    for path in &stale_paths {
                        let _ = crate::db::unregister_repo(db, path);
                    }
                }
            }
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::RepoList { repos: valid_repos },
            )
            .await?;
        }
        CliMessage::SetRepoColor { path, color } => {
            let result = {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    crate::db::set_repo_color(db, &path, &color)
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok(()) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoColorSet { path, color },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::SetRepoEditor { path, editor } => {
            let result = {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    crate::db::set_repo_editor(db, &path, &editor)
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok(()) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoEditorSet { path, editor },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::SetDefaultEditor { editor } => {
            let result = {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    crate::db::set_default_editor(db, &editor)
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok(()) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::DefaultEditorSet)
                        .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::UnregisterRepo { path } => {
            let git_root = crate::repo::detect_git_root(&path);
            let result = {
                let hub_state = state.lock().await;
                if let Some(ref db) = hub_state.db {
                    match git_root {
                        Some(root) => {
                            let root_str = root.to_string_lossy().into_owned();
                            if !crate::db::is_repo_registered(db, &root_str) {
                                Err("repository is not registered".into())
                            } else {
                                let name = root
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| root_str.clone());
                                // Collect agent IDs matching this repo
                                let agent_ids: Vec<String> = hub_state
                                    .agents
                                    .values()
                                    .filter(|e| e.repo_path.as_deref() == Some(root_str.as_str()))
                                    .map(|e| e.id.clone())
                                    .collect();
                                let count = agent_ids.len();
                                crate::db::unregister_repo(db, &root_str)
                                    .map(|_| (root_str, name, agent_ids, count))
                            }
                        }
                        None => Err(format!("{path} is not inside a git repository")),
                    }
                } else {
                    Err("database not initialized".into())
                }
            };
            match result {
                Ok((path, name, agent_ids, count)) => {
                    // Stop agents outside the lock
                    for id in agent_ids {
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = agent::stop_agent(&state, &id).await;
                        });
                    }
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoUnregistered {
                            path,
                            name,
                            stopped_agents: count,
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::DeleteRepo { path } => {
            let git_root = crate::repo::detect_git_root(&path);
            // Stage 1: validate, run safety checks, collect agent IDs.
            // We do NOT touch the DB or filesystem yet — if anything below
            // fails we want to leave the repo registered.
            let prepared = {
                let hub_state = state.lock().await;
                if let Some(db) = hub_state.db.as_ref() {
                    match git_root {
                        Some(root) => {
                            let root_str = root.to_string_lossy().into_owned();
                            if !crate::db::is_repo_registered(db, &root_str) {
                                Err("repository is not registered".into())
                            } else {
                                match check_safe_to_delete(&root) {
                                    Err(e) => Err(e),
                                    Ok(canonical) => {
                                        let name = root
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| root_str.clone());
                                        let agent_ids: Vec<String> = hub_state
                                            .agents
                                            .values()
                                            .filter(|e| {
                                                e.repo_path.as_deref() == Some(root_str.as_str())
                                            })
                                            .map(|e| e.id.clone())
                                            .collect();
                                        let count = agent_ids.len();
                                        Ok((root_str, name, canonical, agent_ids, count))
                                    }
                                }
                            }
                        }
                        None => Err(format!("{path} is not inside a git repository")),
                    }
                } else {
                    Err("database not initialized".into())
                }
            };
            match prepared {
                Ok((root_str, name, canonical, agent_ids, count)) => {
                    // Stop agents outside the lock so their tasks can drain.
                    for id in agent_ids {
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = agent::stop_agent(&state, &id).await;
                        });
                    }
                    // Unregister from the DB first. If the filesystem delete
                    // then fails the DB row is already gone — that is the
                    // safer state: an orphaned directory can be removed by
                    // hand, but an orphaned DB row would surface as a dead
                    // repo in the TUI. If the unregister itself fails we do
                    // NOT touch the filesystem.
                    let db_result = {
                        let hub_state = state.lock().await;
                        hub_state
                            .db
                            .as_ref()
                            .map(|db| crate::db::unregister_repo(db, &root_str))
                    };
                    match db_result {
                        None => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error {
                                    message: "database not initialized".into(),
                                },
                            )
                            .await?;
                        }
                        Some(Err(e)) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error {
                                    message: format!(
                                        "Failed to unregister repo (folder NOT deleted): {e}"
                                    ),
                                },
                            )
                            .await?;
                        }
                        Some(Ok(_)) => {
                            if let Err(e) = std::fs::remove_dir_all(&canonical) {
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::Error {
                                        message: format!(
                                            "Repo unregistered but failed to delete {}: {e}. \
                                             Remove the directory manually.",
                                            canonical.display()
                                        ),
                                    },
                                )
                                .await?;
                            } else {
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::RepoDeleted {
                                        path: root_str,
                                        name,
                                        stopped_agents: count,
                                    },
                                )
                                .await?;
                            }
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::StopRepoAgents { path } => {
            let git_root = crate::repo::detect_git_root(&path);
            let result = {
                let hub_state = state.lock().await;
                match git_root {
                    Some(root) => {
                        let root_str = root.to_string_lossy().into_owned();
                        let agent_ids: Vec<String> = hub_state
                            .agents
                            .values()
                            .filter(|e| e.repo_path.as_deref() == Some(root_str.as_str()))
                            .map(|e| e.id.clone())
                            .collect();
                        let count = agent_ids.len();
                        Ok((root_str, agent_ids, count))
                    }
                    None => Err(format!("{path} is not inside a git repository")),
                }
            };
            match result {
                Ok((path, agent_ids, count)) => {
                    for id in agent_ids {
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = agent::stop_agent(&state, &id).await;
                        });
                    }
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoAgentsStopped {
                            path,
                            stopped_count: count,
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::ListWorktrees {
            working_dir,
            repo_name,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    let agent_snapshots = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    crate::repo::AgentSnapshot {
                                        id: v.id.clone(),
                                        agent_binary: v.agent_binary.clone(),
                                        started_at: v.started_at.clone(),
                                        attached_clients: v.attached_count.load(Ordering::Relaxed),
                                        hub: v.hub.clone(),
                                        working_dir: v.working_dir.clone(),
                                        repo_path: v.repo_path.clone(),
                                        branch_name: v.branch_name.clone(),
                                        is_worktree: v.is_worktree,
                                        auto_exit: v.auto_exit,
                                        plan_mode: v.plan_mode,
                                        prompt: v.prompt.clone(),
                                    },
                                )
                            })
                            .collect::<std::collections::HashMap<_, _>>()
                    };
                    let name = root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match crate::repo::list_worktrees(&root, &agent_snapshots).await {
                        Ok(worktrees) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::WorktreeList {
                                    repo_name: name,
                                    repo_path: root.to_string_lossy().into_owned(),
                                    worktrees,
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message: e },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::AddWorktree {
            working_dir,
            repo_name,
            branch_name,
            base_branch,
            checkout_existing,
        } => {
            // Sanitize new branch names; existing branches are already valid.
            let branch_name = if checkout_existing {
                branch_name
            } else {
                clust_ipc::branch::sanitize_branch_name(&branch_name)
            };
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    match crate::repo::add_worktree(
                        &root,
                        &branch_name,
                        base_branch.as_deref(),
                        checkout_existing,
                    )
                    .await
                    {
                        Ok(wt_path) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::WorktreeAdded {
                                    branch_name,
                                    path: wt_path.to_string_lossy().into_owned(),
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            let message = if e.contains("already checked out") {
                                format!(
                                    "Branch '{}' is already checked out and cannot be used as a worktree. \
                                     Use 'Start Agent (in place)' from the context menu, or create a new branch.",
                                    branch_name
                                )
                            } else {
                                e
                            };
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::RemoveWorktree {
            working_dir,
            repo_name,
            branch_name,
            delete_local_branch,
            force,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    let wt_path = crate::repo::worktree_path(&root, &branch_name);

                    // Check dirty state unless --force
                    if !force && crate::repo::is_worktree_dirty(&wt_path) {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error {
                                message:
                                    "worktree has uncommitted changes (use --force to override)"
                                        .into(),
                            },
                        )
                        .await?;
                        return Ok(());
                    }

                    // Stop agents running in this worktree, then wait for the
                    // stop tasks to drain BEFORE invoking `git worktree remove`.
                    // Otherwise an agent process can still be holding files in
                    // the worktree directory and git refuses to delete it
                    // (or worse, removes a half-occupied tree).
                    let root_str = root.to_string_lossy().into_owned();
                    let agent_ids = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .values()
                            .filter(|e| {
                                e.repo_path.as_deref() == Some(root_str.as_str())
                                    && e.branch_name.as_deref() == Some(branch_name.as_str())
                            })
                            .map(|e| e.id.clone())
                            .collect::<Vec<_>>()
                    };
                    let stopped_count = agent_ids.len();

                    let mut stop_handles = Vec::with_capacity(agent_ids.len());
                    for id in &agent_ids {
                        let state = state.clone();
                        let id = id.clone();
                        stop_handles.push(tokio::spawn(async move {
                            agent::stop_agent(&state, &id).await
                        }));
                    }
                    for handle in stop_handles {
                        match handle.await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                eprintln!("stop_agent failed during worktree removal: {e}");
                            }
                            Err(e) => {
                                eprintln!("stop_agent task panicked during worktree removal: {e}");
                            }
                        }
                    }

                    match crate::repo::remove_worktree(
                        &root,
                        &branch_name,
                        delete_local_branch,
                        force,
                    )
                    .await
                    {
                        Ok(()) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::WorktreeRemoved {
                                    branch_name,
                                    stopped_agents: stopped_count,
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message: e },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::GetWorktreeInfo {
            working_dir,
            repo_name,
            branch_name,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    let agent_snapshots = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    crate::repo::AgentSnapshot {
                                        id: v.id.clone(),
                                        agent_binary: v.agent_binary.clone(),
                                        started_at: v.started_at.clone(),
                                        attached_clients: v.attached_count.load(Ordering::Relaxed),
                                        hub: v.hub.clone(),
                                        working_dir: v.working_dir.clone(),
                                        repo_path: v.repo_path.clone(),
                                        branch_name: v.branch_name.clone(),
                                        is_worktree: v.is_worktree,
                                        auto_exit: v.auto_exit,
                                        plan_mode: v.plan_mode,
                                        prompt: v.prompt.clone(),
                                    },
                                )
                            })
                            .collect::<std::collections::HashMap<_, _>>()
                    };
                    match crate::repo::list_worktrees(&root, &agent_snapshots).await {
                        Ok(worktrees) => {
                            match worktrees.into_iter().find(|w| w.branch_name == branch_name) {
                                Some(info) => {
                                    clust_ipc::send_message_write(
                                        &mut writer,
                                        &HubMessage::WorktreeInfoResult { info },
                                    )
                                    .await?;
                                }
                                None => {
                                    clust_ipc::send_message_write(
                                        &mut writer,
                                        &HubMessage::Error {
                                            message: format!(
                                                "no worktree found for branch '{branch_name}'"
                                            ),
                                        },
                                    )
                                    .await?;
                                }
                            }
                        }
                        Err(e) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message: e },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::CreateWorktreeAgent {
            repo_path,
            target_branch,
            new_branch,
            prompt,
            agent_binary,
            cols,
            rows,
            accept_edits: _,
            plan_mode,
            allow_bypass,
            hub,
            auto_exit,
        } => {
            match crate::agent::create_worktree_and_spawn_agent(
                crate::agent::CreateWorktreeParams {
                    state: &state,
                    repo_path: &repo_path,
                    target_branch: target_branch.as_deref(),
                    new_branch: new_branch.as_deref(),
                    prompt,
                    agent_binary,
                    plan_mode,
                    allow_bypass,
                    hub: &hub,
                    cols,
                    rows,
                    exit_when_done: auto_exit,
                    scheduled_task_id: None,
                },
            )
            .await
            {
                Ok((id, binary, working_dir)) => {
                    // Read git info for the response
                    let (response_repo_path, response_branch_name) = {
                        let hub_state = state.lock().await;
                        if let Some(entry) = hub_state.agents.get(&id) {
                            (entry.repo_path.clone(), entry.branch_name.clone())
                        } else {
                            (None, None)
                        }
                    };
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::WorktreeAgentStarted {
                            id,
                            agent_binary: binary,
                            working_dir,
                            repo_path: response_repo_path,
                            branch_name: response_branch_name,
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::DeleteLocalBranch {
            working_dir,
            repo_name,
            branch_name,
            force,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    // Stop agents running on this branch
                    let root_str = root.to_string_lossy().into_owned();
                    let agent_ids = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .values()
                            .filter(|e| {
                                e.repo_path.as_deref() == Some(root_str.as_str())
                                    && e.branch_name.as_deref() == Some(branch_name.as_str())
                            })
                            .map(|e| e.id.clone())
                            .collect::<Vec<_>>()
                    };
                    let stopped_count = agent_ids.len();

                    for id in &agent_ids {
                        let state = state.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let _ = agent::stop_agent(&state, &id).await;
                        });
                    }

                    match crate::repo::delete_local_branch(&root, &branch_name, force).await {
                        Ok(()) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::LocalBranchDeleted {
                                    branch_name,
                                    stopped_agents: stopped_count,
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message: e },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::DeleteRemoteBranch {
            working_dir,
            repo_name,
            branch_name,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => match crate::repo::delete_remote_branch(&root, &branch_name).await {
                    Ok(()) => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::RemoteBranchDeleted { branch_name },
                        )
                        .await?;
                    }
                    Err(e) => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error { message: e },
                        )
                        .await?;
                    }
                },
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::CheckoutRemoteBranch {
            working_dir,
            repo_name,
            remote_branch,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => {
                    match crate::repo::checkout_remote_branch(&root, &remote_branch).await {
                        Ok(branch_name) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::RemoteBranchCheckedOut { branch_name },
                            )
                            .await?;
                        }
                        Err(e) => {
                            clust_ipc::send_message_write(
                                &mut writer,
                                &HubMessage::Error { message: e },
                            )
                            .await?;
                        }
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::DetachHead { repo_path } => {
            let repo_root = std::path::Path::new(&repo_path);
            match crate::repo::detach_head(repo_root).await {
                Ok(()) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::HeadDetached).await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::CheckoutLocalBranch {
            repo_path,
            branch_name,
        } => {
            let repo_root = std::path::Path::new(&repo_path);
            match crate::repo::checkout_local_branch(repo_root, &branch_name).await {
                Ok(()) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::LocalBranchCheckedOut { branch_name },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::PurgeRepo { path } => {
            let git_root = crate::repo::detect_git_root(&path);
            match git_root {
                Some(root) => {
                    let root_str = root.to_string_lossy().into_owned();

                    // Phase 1: Stop all repo agents
                    let agent_ids = {
                        let hub_state = state.lock().await;
                        hub_state
                            .agents
                            .values()
                            .filter(|e| e.repo_path.as_deref() == Some(root_str.as_str()))
                            .map(|e| e.id.clone())
                            .collect::<Vec<_>>()
                    };
                    let stopped_agents = agent_ids.len();

                    if stopped_agents > 0 {
                        let label = if stopped_agents == 1 {
                            "agent"
                        } else {
                            "agents"
                        };
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::PurgeProgress {
                                step: format!("Stopping {stopped_agents} {label}"),
                            },
                        )
                        .await?;

                        let mut handles = Vec::new();
                        for id in &agent_ids {
                            let state = state.clone();
                            let id = id.clone();
                            handles.push(tokio::spawn(async move {
                                let _ = agent::stop_agent(&state, &id).await;
                            }));
                        }
                        for handle in handles {
                            let _ = handle.await;
                        }
                    }

                    // Purge is a best-effort operation. Collect each phase's
                    // failure (if any) as a warning instead of swallowing it,
                    // and surface warnings to the user as PurgeProgress
                    // messages before the final RepoPurged. The success
                    // counts for each phase are reported regardless.
                    let mut warnings: Vec<String> = Vec::new();

                    // Phase 2: Remove worktrees
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Removing worktrees".to_string(),
                        },
                    )
                    .await?;
                    let removed_worktrees = match crate::repo::purge_worktrees(&root).await {
                        Ok(n) => n,
                        Err(e) => {
                            warnings.push(format!("purge_worktrees: {e}"));
                            0
                        }
                    };

                    // Phase 3: Delete local branches
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Deleting local branches".to_string(),
                        },
                    )
                    .await?;
                    let deleted_branches = match crate::repo::purge_branches(&root).await {
                        Ok(n) => n,
                        Err(e) => {
                            warnings.push(format!("purge_branches: {e}"));
                            0
                        }
                    };

                    // Phase 4: Clean stale refs
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Cleaning stale refs".to_string(),
                        },
                    )
                    .await?;
                    if let Err(e) = crate::repo::clean_stale_refs(&root).await {
                        warnings.push(format!("clean_stale_refs: {e}"));
                    }

                    // Surface warnings as progress entries before the final
                    // result so the user sees partial-failure context.
                    for w in &warnings {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::PurgeProgress {
                                step: format!("\u{26a0} {w}"),
                            },
                        )
                        .await?;
                    }

                    // Done
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoPurged {
                            path: root_str,
                            stopped_agents,
                            removed_worktrees,
                            deleted_branches,
                        },
                    )
                    .await?;
                }
                None => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error {
                            message: format!("{path} is not inside a git repository"),
                        },
                    )
                    .await?;
                }
            }
        }
        CliMessage::CleanStaleRefs {
            working_dir,
            repo_name,
        } => {
            let repo_root = {
                let hub_state = state.lock().await;
                crate::repo::resolve_repo(
                    working_dir.as_deref(),
                    repo_name.as_deref(),
                    hub_state.db.as_ref(),
                )
            };
            match repo_root {
                Ok(root) => match crate::repo::clean_stale_refs(&root).await {
                    Ok(()) => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::StaleRefsCleaned {
                                path: root.to_string_lossy().into_owned(),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error { message: e },
                        )
                        .await?;
                    }
                },
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::PullBranch {
            repo_path,
            branch_name,
        } => {
            let repo_root = std::path::Path::new(&repo_path);
            match crate::repo::pull_branch(repo_root, &branch_name).await {
                Ok(summary) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::BranchPulled {
                            branch_name,
                            summary,
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::CreateRepo { parent_dir, name } => {
            // git init runs outside the lock
            match crate::repo::init_repo(std::path::Path::new(&parent_dir), &name).await {
                Ok(repo_path) => {
                    let path_str = repo_path.to_string_lossy().into_owned();
                    // Register in DB under lock
                    {
                        let hub = state.lock().await;
                        if let Some(ref db) = hub.db {
                            let color = crate::db::next_repo_color(db);
                            let _ = crate::db::register_repo(db, &path_str, &name, color);
                        }
                    }
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::RepoCreated {
                            path: path_str,
                            name,
                        },
                    )
                    .await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }

        CliMessage::CloneRepo {
            url,
            parent_dir,
            name,
        } => {
            // Spawn child process outside the lock
            match crate::repo::start_clone(&url, std::path::Path::new(&parent_dir), name.as_deref())
                .await
            {
                Ok((mut child, repo_path)) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::CloneProgress {
                            step: format!("Cloning {url}..."),
                        },
                    )
                    .await?;

                    // Read stderr progress in a blocking task, bridge via channel
                    let (tx, mut rx) =
                        tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
                    let stderr = child.stderr.take().unwrap();
                    tokio::task::spawn_blocking(move || {
                        use std::io::BufRead;
                        let reader = std::io::BufReader::new(stderr);
                        for line in reader.lines() {
                            match line {
                                #[allow(clippy::collapsible_match)]
                                Ok(l) if !l.is_empty() => {
                                    if tx.send(Ok(l)).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(e.to_string()));
                                    break;
                                }
                                _ => {}
                            }
                        }
                        match child.wait() {
                            Ok(status) if status.success() => {
                                let _ = tx.send(Ok("__CLONE_DONE__".into()));
                            }
                            Ok(status) => {
                                let _ =
                                    tx.send(Err(format!("git clone exited with status {status}")));
                            }
                            Err(e) => {
                                let _ = tx.send(Err(format!("failed to wait for git: {e}")));
                            }
                        }
                    });

                    // Forward progress, then handle completion
                    let mut clone_ok = false;
                    while let Some(msg) = rx.recv().await {
                        match msg {
                            Ok(ref s) if s == "__CLONE_DONE__" => {
                                clone_ok = true;
                                break;
                            }
                            Ok(line) => {
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::CloneProgress { step: line },
                                )
                                .await?;
                            }
                            Err(e) => {
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::Error { message: e },
                                )
                                .await?;
                                return Ok(());
                            }
                        }
                    }

                    if clone_ok {
                        crate::repo::ensure_main_branch(&repo_path);
                        let path_str = repo_path.to_string_lossy().into_owned();
                        let repo_name = repo_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path_str.clone());
                        // Register the new repo in the DB. If registration
                        // fails the clone is in a half-state (on disk but
                        // unknown to the hub); best-effort remove the
                        // freshly cloned directory and surface the error so
                        // the user can retry.
                        let register_result: Result<(), String> = {
                            let hub = state.lock().await;
                            match hub.db.as_ref() {
                                Some(db) => {
                                    let color = crate::db::next_repo_color(db);
                                    crate::db::register_repo(db, &path_str, &repo_name, color)
                                }
                                None => Err("database not initialized".into()),
                            }
                        };
                        match register_result {
                            Ok(()) => {
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::RepoCloned {
                                        path: path_str,
                                        name: repo_name,
                                    },
                                )
                                .await?;
                            }
                            Err(reg_err) => {
                                let cleanup_note = match std::fs::remove_dir_all(&repo_path) {
                                    Ok(()) => "cloned directory removed".to_string(),
                                    Err(rm_err) => format!(
                                        "could not clean up clone at {}: {rm_err}",
                                        repo_path.display()
                                    ),
                                };
                                clust_ipc::send_message_write(
                                    &mut writer,
                                    &HubMessage::Error {
                                        message: format!(
                                            "clone succeeded but registration failed: {reg_err} ({cleanup_note})"
                                        ),
                                    },
                                )
                                .await?;
                            }
                        }
                    } else {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error {
                                message: "Clone failed unexpectedly".into(),
                            },
                        )
                        .await?;
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }

        CliMessage::Ping { protocol_version } => {
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::Pong {
                    protocol_version: clust_ipc::PROTOCOL_VERSION,
                },
            )
            .await?;
            if protocol_version != clust_ipc::PROTOCOL_VERSION {
                eprintln!(
                    "protocol version mismatch: hub={}, client={protocol_version}",
                    clust_ipc::PROTOCOL_VERSION
                );
            }
        }

        // Terminal session management
        CliMessage::StartTerminal {
            working_dir,
            cols,
            rows,
            agent_id,
        } => {
            let result = {
                let mut hub_state = state.lock().await;
                agent::spawn_terminal(
                    &mut hub_state,
                    working_dir,
                    cols,
                    rows,
                    agent_id,
                    state.clone(),
                )
            };
            match result {
                Ok(id) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::TerminalStarted { id: id.clone() },
                    )
                    .await?;
                    handle_attached_terminal_session(&id, reader, writer, state).await?;
                }
                Err(e) => {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Error { message: e })
                        .await?;
                }
            }
        }
        CliMessage::AttachTerminal { id } => {
            let exists = {
                let hub = state.lock().await;
                hub.terminals.contains_key(&id)
            };
            if exists {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::TerminalAttached { id: id.clone() },
                )
                .await?;
                handle_attached_terminal_session(&id, reader, writer, state).await?;
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("terminal {id} not found"),
                    },
                )
                .await?;
            }
        }
        CliMessage::StopTerminal { id } => {
            let exists = {
                let hub = state.lock().await;
                hub.terminals.contains_key(&id)
            };
            if exists {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::TerminalStopped { id: id.clone() },
                )
                .await?;
                let state = state.clone();
                tokio::spawn(async move {
                    let _ = agent::stop_terminal(&state, &id).await;
                });
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("terminal {id} not found"),
                    },
                )
                .await?;
            }
        }

        // -----------------------------------------------------------------
        // Scheduled task handlers
        // -----------------------------------------------------------------
        CliMessage::CreateScheduledTask {
            repo_path,
            base_branch,
            new_branch,
            prompt,
            plan_mode,
            auto_exit,
            agent_binary,
            schedule,
            extra_agent_deps,
        } => {
            // Resolve which agent binary the spawn will use, falling back to
            // the hub's configured default. We persist the resolution rather
            // than the user's None, so a later config change can't accidentally
            // re-target an old task.
            let resolved_binary = {
                let hub = state.lock().await;
                match agent::resolve_agent_binary(agent_binary, &hub.default_agent) {
                    Ok(b) => b,
                    Err(e) => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error { message: e },
                        )
                        .await?;
                        return Ok(());
                    }
                }
            };
            // Reject empty prompts at the IPC boundary as well as the modal,
            // so a hand-crafted client can't bypass it.
            if prompt.trim().is_empty() {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "scheduled task prompt must not be empty".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            // Compute the resolved branch_name (sanitized when creating new).
            let branch_name = match new_branch.as_deref() {
                Some(name) => clust_ipc::branch::sanitize_branch_name(name),
                None => match base_branch.as_deref() {
                    Some(name) => name.to_string(),
                    None => {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::Error {
                                message: "either base_branch or new_branch must be provided"
                                    .into(),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                },
            };

            let mut hub = state.lock().await;
            let conn_present = hub.db.is_some();
            if !conn_present {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "database unavailable".into(),
                    },
                )
                .await?;
                return Ok(());
            }

            // Promote each running-agent dep to a shadow scheduled-task row
            // (or reuse the existing one), so the new task's persisted deps
            // can stay as plain task IDs. We do this BEFORE inserting the
            // new task so the FK-style edge in `scheduled_task_deps` exists.
            let mut promoted_dep_ids: Vec<String> = Vec::new();
            let mut promotion_error: Option<String> = None;
            for agent_id in &extra_agent_deps {
                let entry_info = hub.agents.get(agent_id).map(|e| {
                    (
                        e.repo_path.clone(),
                        e.branch_name.clone(),
                        e.prompt.clone(),
                        e.plan_mode,
                        e.auto_exit,
                        e.agent_binary.clone(),
                    )
                });
                let Some((rp, bn, p, pm, ae, bin)) = entry_info else {
                    promotion_error = Some(format!(
                        "agent {agent_id} is no longer running and cannot be used as a dependency"
                    ));
                    break;
                };
                let conn = hub.db.as_ref().unwrap();
                let existing = match crate::db::find_scheduled_task_by_agent_id(conn, agent_id) {
                    Ok(x) => x,
                    Err(e) => {
                        promotion_error = Some(e);
                        break;
                    }
                };
                if let Some(task_id) = existing {
                    promoted_dep_ids.push(task_id);
                    continue;
                }
                let Some(rp) = rp else {
                    promotion_error = Some(format!(
                        "agent {agent_id} has no repo path and cannot be used as a dependency"
                    ));
                    break;
                };
                let Some(bn) = bn else {
                    promotion_error = Some(format!(
                        "agent {agent_id} has no branch name and cannot be used as a dependency"
                    ));
                    break;
                };
                let spec = crate::db::NewScheduledTask {
                    repo_path: rp,
                    base_branch: None,
                    new_branch: None,
                    branch_name: bn,
                    prompt: p.unwrap_or_default(),
                    plan_mode: pm,
                    auto_exit: ae,
                    agent_binary: bin,
                    schedule: clust_ipc::ScheduleKind::Unscheduled,
                };
                match crate::db::insert_active_shadow_task(conn, spec, agent_id) {
                    Ok(id) => promoted_dep_ids.push(id),
                    Err(e) => {
                        promotion_error = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = promotion_error {
                drop(hub);
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error { message: e },
                )
                .await?;
                return Ok(());
            }

            // Merge promoted shadow-task IDs into the dep list. This only
            // takes effect for `Depend` schedules; for the other variants
            // the picker step never runs and `extra_agent_deps` is empty.
            let schedule = if promoted_dep_ids.is_empty() {
                schedule
            } else {
                match schedule {
                    clust_ipc::ScheduleKind::Depend { mut depends_on_ids } => {
                        for id in promoted_dep_ids {
                            if !depends_on_ids.contains(&id) {
                                depends_on_ids.push(id);
                            }
                        }
                        clust_ipc::ScheduleKind::Depend { depends_on_ids }
                    }
                    other => other,
                }
            };

            let conn = hub.db.as_mut().unwrap();
            let result = crate::db::insert_scheduled_task(
                conn,
                crate::db::NewScheduledTask {
                    repo_path: repo_path.clone(),
                    base_branch: base_branch.clone(),
                    new_branch: new_branch.clone(),
                    branch_name,
                    prompt,
                    plan_mode,
                    auto_exit,
                    agent_binary: resolved_binary,
                    schedule,
                },
            );
            match result {
                Ok(id) => {
                    let lookup = repo_name_lookup_from(hub.db.as_ref());
                    let info = crate::db::get_scheduled_task(
                        hub.db.as_ref().unwrap(),
                        &id,
                        &lookup,
                    )
                    .ok()
                    .flatten();
                    drop(hub);
                    if let Some(info) = info {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::ScheduledTaskCreated { info },
                        )
                        .await?;
                    } else {
                        clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                    }
                }
                Err(e) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }
        CliMessage::ListScheduledTasks => {
            let hub = state.lock().await;
            let tasks = if let Some(ref conn) = hub.db {
                let lookup = repo_name_lookup_from(Some(conn));
                crate::db::list_scheduled_tasks(conn, &lookup).unwrap_or_default()
            } else {
                Vec::new()
            };
            drop(hub);
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::ScheduledTaskList { tasks },
            )
            .await?;
        }
        CliMessage::UpdateScheduledTaskPrompt { id, prompt } => {
            // Reject empty prompts here too — the modal also enforces this but
            // a stale or hand-crafted client could otherwise blank a row.
            if prompt.trim().is_empty() {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "prompt must not be empty".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            let hub = state.lock().await;
            if let Some(ref conn) = hub.db {
                if let Err(e) = crate::db::update_scheduled_task_prompt(conn, &id, &prompt) {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                    return Ok(());
                }
                let lookup = repo_name_lookup_from(Some(conn));
                let info = crate::db::get_scheduled_task(conn, &id, &lookup)
                    .ok()
                    .flatten();
                drop(hub);
                if let Some(info) = info {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::ScheduledTaskUpdated { info },
                    )
                    .await?;
                } else {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                }
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "database unavailable".into(),
                    },
                )
                .await?;
            }
        }
        CliMessage::SetScheduledTaskPlanMode { id, plan_mode } => {
            let hub = state.lock().await;
            if let Some(ref conn) = hub.db {
                if let Err(e) = crate::db::update_scheduled_task_plan_mode(conn, &id, plan_mode) {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                    return Ok(());
                }
                let lookup = repo_name_lookup_from(Some(conn));
                let info = crate::db::get_scheduled_task(conn, &id, &lookup)
                    .ok()
                    .flatten();
                drop(hub);
                if let Some(info) = info {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::ScheduledTaskUpdated { info },
                    )
                    .await?;
                } else {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                }
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "database unavailable".into(),
                    },
                )
                .await?;
            }
        }
        CliMessage::SetScheduledTaskAutoExit { id, auto_exit } => {
            let hub = state.lock().await;
            if let Some(ref conn) = hub.db {
                if let Err(e) = crate::db::update_scheduled_task_auto_exit(conn, &id, auto_exit) {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                    return Ok(());
                }
                let lookup = repo_name_lookup_from(Some(conn));
                let info = crate::db::get_scheduled_task(conn, &id, &lookup)
                    .ok()
                    .flatten();
                drop(hub);
                if let Some(info) = info {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::ScheduledTaskUpdated { info },
                    )
                    .await?;
                } else {
                    clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                }
            } else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "database unavailable".into(),
                    },
                )
                .await?;
            }
        }
        CliMessage::DeleteScheduledTask { id } => {
            // If the task is currently Active, also stop the agent. Otherwise
            // a "delete" leaves a zombie agent attached to a row that no
            // longer exists.
            let agent_to_stop: Option<String> = {
                let hub = state.lock().await;
                hub.db.as_ref().and_then(|conn| {
                    let lookup = repo_name_lookup_from(Some(conn));
                    crate::db::get_scheduled_task(conn, &id, &lookup)
                        .ok()
                        .flatten()
                        .filter(|t| t.status == clust_ipc::ScheduledTaskStatus::Active)
                        .and_then(|t| t.agent_id)
                })
            };
            if let Some(aid) = agent_to_stop {
                let _ = agent::stop_agent(&state, &aid).await;
            }
            let hub = state.lock().await;
            if let Some(ref conn) = hub.db {
                if let Err(e) = crate::db::delete_scheduled_task(conn, &id) {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                    return Ok(());
                }
            }
            drop(hub);
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::ScheduledTaskDeleted { id },
            )
            .await?;
        }
        CliMessage::DeleteScheduledTasksByStatus { status } => {
            let hub = state.lock().await;
            let count = if let Some(ref conn) = hub.db {
                crate::db::delete_scheduled_tasks_with_status(conn, status).unwrap_or(0)
            } else {
                0
            };
            drop(hub);
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::ScheduledTasksCleared { count },
            )
            .await?;
        }
        CliMessage::StartScheduledTaskNow { id } => {
            // Same code path as the auto-trigger so manually started tasks
            // reach Active through identical bookkeeping.
            let task = {
                let hub = state.lock().await;
                hub.db.as_ref().and_then(|conn| {
                    let lookup = repo_name_lookup_from(Some(conn));
                    crate::db::get_scheduled_task(conn, &id, &lookup)
                        .ok()
                        .flatten()
                })
            };
            let Some(task) = task else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("scheduled task {id} not found"),
                    },
                )
                .await?;
                return Ok(());
            };
            if task.status != clust_ipc::ScheduledTaskStatus::Inactive {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!(
                            "task {id} is {:?}, not Inactive",
                            task.status
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
            match crate::scheduler::fire_scheduled_task(&state, &task).await {
                Ok(_agent_id) => {
                    let info = {
                        let hub = state.lock().await;
                        hub.db.as_ref().and_then(|conn| {
                            let lookup = repo_name_lookup_from(Some(conn));
                            crate::db::get_scheduled_task(conn, &id, &lookup)
                                .ok()
                                .flatten()
                        })
                    };
                    if let Some(info) = info {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::ScheduledTaskUpdated { info },
                        )
                        .await?;
                    } else {
                        clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                    }
                }
                Err(e) => {
                    let _ = {
                        let hub = state.lock().await;
                        hub.db
                            .as_ref()
                            .map(|conn| crate::db::mark_scheduled_task_aborted(conn, &id))
                    };
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }
        CliMessage::RestartScheduledTask { id, clean } => {
            let task = {
                let hub = state.lock().await;
                hub.db.as_ref().and_then(|conn| {
                    let lookup = repo_name_lookup_from(Some(conn));
                    crate::db::get_scheduled_task(conn, &id, &lookup)
                        .ok()
                        .flatten()
                })
            };
            let Some(task) = task else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("scheduled task {id} not found"),
                    },
                )
                .await?;
                return Ok(());
            };
            if task.status != clust_ipc::ScheduledTaskStatus::Aborted {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "restart only valid for Aborted tasks".into(),
                    },
                )
                .await?;
                return Ok(());
            }
            if clean {
                if let Err(e) = crate::scheduler::clean_worktree_for_task(&task).await {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error {
                            message: format!("clean failed: {e}"),
                        },
                    )
                    .await?;
                    return Ok(());
                }
            }
            // Reset to Inactive so fire_scheduled_task can take it; this also
            // preserves existing agent_id history for diagnostics until the
            // new spawn succeeds.
            {
                let hub = state.lock().await;
                if let Some(ref conn) = hub.db {
                    let _ = conn.execute(
                        "UPDATE scheduled_tasks SET status='inactive', agent_id=NULL WHERE id=?1",
                        [&id],
                    );
                }
            }
            // Re-read the task in its now-Inactive state before firing, so the
            // helper sees the right status.
            let refreshed = {
                let hub = state.lock().await;
                hub.db.as_ref().and_then(|conn| {
                    let lookup = repo_name_lookup_from(Some(conn));
                    crate::db::get_scheduled_task(conn, &id, &lookup)
                        .ok()
                        .flatten()
                })
            };
            let Some(refreshed) = refreshed else {
                clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                return Ok(());
            };
            match crate::scheduler::fire_scheduled_task(&state, &refreshed).await {
                Ok(_agent_id) => {
                    let info = {
                        let hub = state.lock().await;
                        hub.db.as_ref().and_then(|conn| {
                            let lookup = repo_name_lookup_from(Some(conn));
                            crate::db::get_scheduled_task(conn, &id, &lookup)
                                .ok()
                                .flatten()
                        })
                    };
                    if let Some(info) = info {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::ScheduledTaskUpdated { info },
                        )
                        .await?;
                    } else {
                        clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                    }
                }
                Err(e) => {
                    let _ = {
                        let hub = state.lock().await;
                        hub.db
                            .as_ref()
                            .map(|conn| crate::db::mark_scheduled_task_aborted(conn, &id))
                    };
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }
        CliMessage::RescheduleScheduledTask {
            id,
            schedule,
            extra_agent_deps,
        } => {
            // Look the task up first so we can validate its current status —
            // only Inactive and Aborted tasks can be rescheduled. Active
            // worktrees are running and Complete tasks already finished.
            let task = {
                let hub = state.lock().await;
                hub.db.as_ref().and_then(|conn| {
                    let lookup = repo_name_lookup_from(Some(conn));
                    crate::db::get_scheduled_task(conn, &id, &lookup)
                        .ok()
                        .flatten()
                })
            };
            let Some(task) = task else {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: format!("scheduled task {id} not found"),
                    },
                )
                .await?;
                return Ok(());
            };
            if !matches!(
                task.status,
                clust_ipc::ScheduledTaskStatus::Inactive
                    | clust_ipc::ScheduledTaskStatus::Aborted
            ) {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "reschedule only valid for Inactive or Aborted tasks"
                            .into(),
                    },
                )
                .await?;
                return Ok(());
            }

            let mut hub = state.lock().await;
            if hub.db.is_none() {
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error {
                        message: "database unavailable".into(),
                    },
                )
                .await?;
                return Ok(());
            }

            // Promote any picked Opt+E worktree agent IDs to shadow scheduled
            // tasks (mirrors the create path) before the dep edges are
            // rewritten, so the new edges all reference task IDs.
            let mut promoted_dep_ids: Vec<String> = Vec::new();
            let mut promotion_error: Option<String> = None;
            for agent_id in &extra_agent_deps {
                let entry_info = hub.agents.get(agent_id).map(|e| {
                    (
                        e.repo_path.clone(),
                        e.branch_name.clone(),
                        e.prompt.clone(),
                        e.plan_mode,
                        e.auto_exit,
                        e.agent_binary.clone(),
                    )
                });
                let Some((rp, bn, p, pm, ae, bin)) = entry_info else {
                    promotion_error = Some(format!(
                        "agent {agent_id} is no longer running and cannot be used as a dependency"
                    ));
                    break;
                };
                let conn = hub.db.as_ref().unwrap();
                let existing = match crate::db::find_scheduled_task_by_agent_id(conn, agent_id)
                {
                    Ok(x) => x,
                    Err(e) => {
                        promotion_error = Some(e);
                        break;
                    }
                };
                if let Some(task_id) = existing {
                    promoted_dep_ids.push(task_id);
                    continue;
                }
                let Some(rp) = rp else {
                    promotion_error = Some(format!(
                        "agent {agent_id} has no repo path and cannot be used as a dependency"
                    ));
                    break;
                };
                let Some(bn) = bn else {
                    promotion_error = Some(format!(
                        "agent {agent_id} has no branch name and cannot be used as a dependency"
                    ));
                    break;
                };
                let spec = crate::db::NewScheduledTask {
                    repo_path: rp,
                    base_branch: None,
                    new_branch: None,
                    branch_name: bn,
                    prompt: p.unwrap_or_default(),
                    plan_mode: pm,
                    auto_exit: ae,
                    agent_binary: bin,
                    schedule: clust_ipc::ScheduleKind::Unscheduled,
                };
                match crate::db::insert_active_shadow_task(conn, spec, agent_id) {
                    Ok(id) => promoted_dep_ids.push(id),
                    Err(e) => {
                        promotion_error = Some(e);
                        break;
                    }
                }
            }
            if let Some(e) = promotion_error {
                drop(hub);
                clust_ipc::send_message_write(
                    &mut writer,
                    &HubMessage::Error { message: e },
                )
                .await?;
                return Ok(());
            }

            let schedule = if promoted_dep_ids.is_empty() {
                schedule
            } else {
                match schedule {
                    clust_ipc::ScheduleKind::Depend { mut depends_on_ids } => {
                        for id in promoted_dep_ids {
                            if !depends_on_ids.contains(&id) {
                                depends_on_ids.push(id);
                            }
                        }
                        clust_ipc::ScheduleKind::Depend { depends_on_ids }
                    }
                    other => other,
                }
            };

            let conn = hub.db.as_mut().unwrap();
            let result = crate::db::reschedule_scheduled_task(conn, &id, &schedule);
            match result {
                Ok(()) => {
                    let lookup = repo_name_lookup_from(hub.db.as_ref());
                    let info = crate::db::get_scheduled_task(
                        hub.db.as_ref().unwrap(),
                        &id,
                        &lookup,
                    )
                    .ok()
                    .flatten();
                    drop(hub);
                    if let Some(info) = info {
                        clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::ScheduledTaskUpdated { info },
                        )
                        .await?;
                    } else {
                        clust_ipc::send_message_write(&mut writer, &HubMessage::Ok).await?;
                    }
                }
                Err(e) => {
                    drop(hub);
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }

        _ => {
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::Error {
                    message: "unknown message".into(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

/// Build a repo-path → display-name closure from the registered-repos table.
/// Falls back to the directory's basename for unregistered paths so the UI
/// always has *something* to render.
fn repo_name_lookup_from(
    conn: Option<&rusqlite::Connection>,
) -> impl Fn(&str) -> String {
    use std::collections::HashMap;
    let mut map: HashMap<String, String> = HashMap::new();
    if let Some(conn) = conn {
        if let Ok(repos) = crate::db::list_repos(conn) {
            for (path, name, _, _) in repos {
                map.insert(path, name);
            }
        }
    }
    move |path: &str| {
        map.get(path).cloned().unwrap_or_else(|| {
            std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string())
        })
    }
}

/// Handle a bidirectional streaming session for an attached client.
///
/// Spawns two tasks:
/// - Output task: subscribes to the agent's broadcast channel and sends
///   AgentOutput/AgentExited messages to the CLI.
/// - Input task: reads AgentInput/ResizeAgent/DetachAgent messages from the CLI
///   and routes them to the agent's PTY.
async fn handle_attached_session(
    agent_id: &str,
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    state: SharedHubState,
) -> io::Result<()> {
    // Subscribe to agent output broadcast and assign a client ID.
    // Also grab a handle to the replay buffer for replay-on-attach and lag recovery.
    //
    // We clone the `Arc<AtomicUsize>` for `attached_count` and keep it outside
    // the lock so the cleanup decrement at the end of this function runs even
    // if the agent has been removed from `state.agents` before we get there
    // (e.g. the PTY reader observed exit and dropped the entry). The atomic
    // outlives the entry; orphan decrements are harmless because the count is
    // only read off live entries via `entry.attached_count.load(...)`.
    let (mut output_rx, client_id, replay_buf, attached_count) = {
        let hub = state.lock().await;
        let entry = hub
            .agents
            .get(agent_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "agent not found"))?;
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        let cid = entry.next_client_id();
        (
            entry.output_tx.subscribe(),
            cid,
            entry.replay_buffer.clone(),
            entry.attached_count.clone(),
        )
    };

    let agent_id_owned = agent_id.to_string();

    // Replay buffered output before starting the live stream.
    // Sent as regular AgentOutput chunks so both terminal and overview
    // consumers process them through their existing pipelines.
    {
        let replay_data = replay_buf.lock().unwrap().snapshot();
        const REPLAY_CHUNK_SIZE: usize = 32 * 1024;
        for chunk in replay_data.chunks(REPLAY_CHUNK_SIZE) {
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::AgentOutput {
                    id: agent_id_owned.clone(),
                    data: chunk.to_vec(),
                },
            )
            .await?;
        }
        clust_ipc::send_message_write(
            &mut writer,
            &HubMessage::AgentReplayComplete {
                id: agent_id_owned.clone(),
            },
        )
        .await?;
    }

    let state_for_cleanup = state.clone();
    let agent_id_for_cleanup = agent_id_owned.clone();

    // Task 1: Read from broadcast channel, send HubMessages to CLI
    let agent_id_for_output = agent_id_owned.clone();
    let replay_buf_for_output = replay_buf.clone();
    let output_task = tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(AgentEvent::Output(data)) => {
                    if clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentOutput {
                            id: agent_id_for_output.clone(),
                            data,
                        },
                    )
                    .await
                    .is_err()
                    {
                        break; // Client disconnected
                    }
                }
                Ok(AgentEvent::Exited(code)) => {
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentExited {
                            id: agent_id_for_output.clone(),
                            exit_code: code,
                        },
                    )
                    .await;
                    break;
                }
                Ok(AgentEvent::HubShutdown) => {
                    let _ =
                        clust_ipc::send_message_write(&mut writer, &HubMessage::HubShutdown).await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Lag recovery: resend replay buffer to resync the client
                    let replay_data = replay_buf_for_output.lock().unwrap().snapshot();
                    const REPLAY_CHUNK_SIZE: usize = 32 * 1024;
                    for chunk in replay_data.chunks(REPLAY_CHUNK_SIZE) {
                        if clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::AgentOutput {
                                id: agent_id_for_output.clone(),
                                data: chunk.to_vec(),
                            },
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Agent removed from state (already exited)
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentExited {
                            id: agent_id_for_output.clone(),
                            exit_code: -1,
                        },
                    )
                    .await;
                    break;
                }
            }
        }
    });

    // Task 2: Read CliMessages from CLI, route input to PTY
    let agent_id_for_input = agent_id_owned.clone();
    let state_for_input = state.clone();
    let input_task = tokio::spawn(async move {
        loop {
            match clust_ipc::recv_message_read::<CliMessage>(&mut reader).await {
                Ok(CliMessage::AgentInput { data, .. }) => {
                    let mut hub = state_for_input.lock().await;
                    if let Some(entry) = hub.agents.get_mut(&agent_id_for_input) {
                        // Input-fallback: if this client isn't active and we
                        // know its size, resize the PTY to match before
                        // forwarding input (handles terminals without focus
                        // event support).
                        if entry.active_client_id != Some(client_id) {
                            if let Some(&(cols, rows)) = entry.client_sizes.get(&client_id) {
                                entry.resize_pty_if_needed(cols, rows);
                            }
                            entry.active_client_id = Some(client_id);
                        }
                        let _ = entry.pty_writer.write_all(&data);
                    }
                }
                Ok(CliMessage::ResizeAgent { cols, rows, .. }) => {
                    let mut hub = state_for_input.lock().await;
                    if let Some(entry) = hub.agents.get_mut(&agent_id_for_input) {
                        entry.client_sizes.insert(client_id, (cols, rows));
                        entry.active_client_id = Some(client_id);
                        entry.resize_pty_if_needed(cols, rows);
                    }
                }
                Ok(CliMessage::DetachAgent { .. }) => {
                    break; // Client detaching
                }
                Ok(_) => {
                    // Unexpected message during attached session — ignore
                }
                Err(_) => {
                    break; // Client disconnected
                }
            }
        }
    });

    // Wait for either task to finish, then cancel the other
    tokio::select! {
        _ = output_task => {}
        _ = input_task => {}
    }

    // Decrement attached count via the cloned Arc — runs even if the agent
    // entry has already been removed (e.g. the agent exited mid-session).
    attached_count.fetch_sub(1, Ordering::Relaxed);

    // Best-effort: clear per-client size tracking and active-client marker if
    // the entry is still around. If it's gone there is nothing to clean up.
    let mut hub = state_for_cleanup.lock().await;
    if let Some(entry) = hub.agents.get_mut(&agent_id_for_cleanup) {
        entry.client_sizes.remove(&client_id);
        if entry.active_client_id == Some(client_id) {
            entry.active_client_id = None;
        }
    }

    Ok(())
}

/// Handle a bidirectional streaming session for an attached terminal client.
///
/// Same pattern as `handle_attached_session` but uses Terminal message variants
/// and looks up `state.terminals` instead of `state.agents`.
async fn handle_attached_terminal_session(
    terminal_id: &str,
    mut reader: OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    state: SharedHubState,
) -> io::Result<()> {
    // Same invariant as `handle_attached_session`: clone the `Arc<AtomicUsize>`
    // so the cleanup decrement always runs even if the terminal entry has been
    // removed by the PTY reader before we reach the end of this function.
    let (mut output_rx, client_id, replay_buf, attached_count) = {
        let hub = state.lock().await;
        let entry = hub
            .terminals
            .get(terminal_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "terminal not found"))?;
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        let cid = entry.next_client_id();
        (
            entry.output_tx.subscribe(),
            cid,
            entry.replay_buffer.clone(),
            entry.attached_count.clone(),
        )
    };

    let terminal_id_owned = terminal_id.to_string();

    // Replay buffered output before starting the live stream.
    {
        let replay_data = replay_buf.lock().unwrap().snapshot();
        const REPLAY_CHUNK_SIZE: usize = 32 * 1024;
        for chunk in replay_data.chunks(REPLAY_CHUNK_SIZE) {
            clust_ipc::send_message_write(
                &mut writer,
                &HubMessage::TerminalOutput {
                    id: terminal_id_owned.clone(),
                    data: chunk.to_vec(),
                },
            )
            .await?;
        }
        clust_ipc::send_message_write(
            &mut writer,
            &HubMessage::TerminalReplayComplete {
                id: terminal_id_owned.clone(),
            },
        )
        .await?;
    }

    let state_for_cleanup = state.clone();
    let terminal_id_for_cleanup = terminal_id_owned.clone();

    // Task 1: Read from broadcast channel, send HubMessages to CLI
    let terminal_id_for_output = terminal_id_owned.clone();
    let replay_buf_for_output = replay_buf.clone();
    let output_task = tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(AgentEvent::Output(data)) => {
                    if clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::TerminalOutput {
                            id: terminal_id_for_output.clone(),
                            data,
                        },
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Ok(AgentEvent::Exited(code)) => {
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::TerminalExited {
                            id: terminal_id_for_output.clone(),
                            exit_code: code,
                        },
                    )
                    .await;
                    break;
                }
                Ok(AgentEvent::HubShutdown) => {
                    let _ =
                        clust_ipc::send_message_write(&mut writer, &HubMessage::HubShutdown).await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let replay_data = replay_buf_for_output.lock().unwrap().snapshot();
                    const REPLAY_CHUNK_SIZE: usize = 32 * 1024;
                    for chunk in replay_data.chunks(REPLAY_CHUNK_SIZE) {
                        if clust_ipc::send_message_write(
                            &mut writer,
                            &HubMessage::TerminalOutput {
                                id: terminal_id_for_output.clone(),
                                data: chunk.to_vec(),
                            },
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::TerminalExited {
                            id: terminal_id_for_output.clone(),
                            exit_code: -1,
                        },
                    )
                    .await;
                    break;
                }
            }
        }
    });

    // Task 2: Read CliMessages from CLI, route input to terminal PTY
    let terminal_id_for_input = terminal_id_owned.clone();
    let state_for_input = state.clone();
    let input_task = tokio::spawn(async move {
        loop {
            match clust_ipc::recv_message_read::<CliMessage>(&mut reader).await {
                Ok(CliMessage::TerminalInput { data, .. }) => {
                    let mut hub = state_for_input.lock().await;
                    if let Some(entry) = hub.terminals.get_mut(&terminal_id_for_input) {
                        if entry.active_client_id != Some(client_id) {
                            if let Some(&(cols, rows)) = entry.client_sizes.get(&client_id) {
                                entry.resize_pty_if_needed(cols, rows);
                            }
                            entry.active_client_id = Some(client_id);
                        }
                        let _ = entry.pty_writer.write_all(&data);
                    }
                }
                Ok(CliMessage::ResizeTerminal { cols, rows, .. }) => {
                    let mut hub = state_for_input.lock().await;
                    if let Some(entry) = hub.terminals.get_mut(&terminal_id_for_input) {
                        entry.client_sizes.insert(client_id, (cols, rows));
                        entry.active_client_id = Some(client_id);
                        entry.resize_pty_if_needed(cols, rows);
                    }
                }
                Ok(CliMessage::DetachTerminal { .. }) => {
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = output_task => {}
        _ = input_task => {}
    }

    // Always decrement the attached count via the cloned Arc, even if the
    // terminal entry is gone.
    attached_count.fetch_sub(1, Ordering::Relaxed);

    let mut hub = state_for_cleanup.lock().await;
    if let Some(entry) = hub.terminals.get_mut(&terminal_id_for_cleanup) {
        entry.client_sizes.remove(&client_id);
        if entry.active_client_id == Some(client_id) {
            entry.active_client_id = None;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_canonical_ancestor` should match only on whole path components,
    /// so `/tmp/foo` is NOT an ancestor of `/tmp/foo-bar`.
    #[test]
    fn canonical_ancestor_rejects_prefix_trick() {
        let a = std::path::Path::new("/tmp/foo");
        let p = std::path::Path::new("/tmp/foo-bar");
        assert!(
            !is_canonical_ancestor(a, p),
            "/tmp/foo must not be considered an ancestor of /tmp/foo-bar"
        );
    }

    #[test]
    fn canonical_ancestor_accepts_equal() {
        let a = std::path::Path::new("/a/b/c");
        let p = std::path::Path::new("/a/b/c");
        assert!(is_canonical_ancestor(a, p));
    }

    #[test]
    fn canonical_ancestor_accepts_strict_ancestor() {
        let a = std::path::Path::new("/a/b");
        let p = std::path::Path::new("/a/b/c");
        assert!(is_canonical_ancestor(a, p));
    }

    #[test]
    fn canonical_ancestor_rejects_unrelated() {
        let a = std::path::Path::new("/a/b");
        let p = std::path::Path::new("/x/y/z");
        assert!(!is_canonical_ancestor(a, p));
    }

    /// Asking to delete a path that doesn't exist must return an error
    /// rather than silently falling back to the non-canonical path.
    #[test]
    fn check_safe_to_delete_errors_on_missing_path() {
        let p = std::path::Path::new("/this/path/should/never/exist/abc123-clust-test");
        assert!(check_safe_to_delete(p).is_err());
    }

    /// `check_safe_to_delete` must reject the prefix-trick case where the
    /// home dir shares a string prefix with the target but is not a real
    /// path-component ancestor.
    ///
    /// Strategy: create two sibling dirs `home_dir` and `home_dir-extra`
    /// inside a tempdir, point `HOME` at `home_dir`, then ask whether
    /// `home_dir-extra` is safe to delete. The new code uses canonical-
    /// component comparison so this case is allowed (whereas a buggy
    /// `Path::starts_with` based check would mis-classify).
    #[test]
    fn check_safe_to_delete_canonical_components_not_byte_prefix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home_dir = tmp.path().join("home_dir");
        let extra_dir = tmp.path().join("home_dir-extra");
        std::fs::create_dir(&home_dir).unwrap();
        std::fs::create_dir(&extra_dir).unwrap();

        // Save and restore HOME so other tests aren't affected.
        let prev_home = std::env::var_os("HOME");
        // SAFETY: cargo runs tests within a process; mutating env may race
        // with parallel tests that read HOME. This test does not assume
        // exclusivity, so we restore HOME before asserting.
        unsafe {
            std::env::set_var("HOME", &home_dir);
        }
        let result = check_safe_to_delete(&extra_dir);
        match prev_home {
            Some(h) => unsafe {
                std::env::set_var("HOME", h);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }

        assert!(
            result.is_ok(),
            "deleting {:?} (sibling of HOME={:?}) should be allowed, got: {:?}",
            extra_dir,
            home_dir,
            result
        );
    }
}
