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
pub async fn run_ipc_server(
    shutdown_signal: Arc<dyn ShutdownSignal>,
    state: SharedHubState,
) {
    if let Err(e) = run(shutdown_signal, state).await {
        eprintln!("ipc server error: {e}");
    }
}

async fn run(
    shutdown_signal: Arc<dyn ShutdownSignal>,
    state: SharedHubState,
) -> io::Result<()> {
    let dir = clust_ipc::clust_dir();
    tokio::fs::create_dir_all(&dir).await?;

    // Initialize SQLite database (creates tables on first run)
    {
        let mut hub = state.lock().await;
        if let Err(e) = hub.init_db() {
            eprintln!("database init failed: {e}");
        }
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
                        hub,
                        repo_path,
                        branch_name,
                        is_worktree,
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
                            if let Some(root) = crate::repo::detect_git_root(&working_dir_for_register) {
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
                        hub_state.agents.get(&id).map(|e| {
                            (e.is_worktree, e.repo_path.clone(), e.branch_name.clone())
                        }).unwrap_or((false, None, None))
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }
        CliMessage::AttachAgent { id } => {
            let agent_info = {
                let hub = state.lock().await;
                hub.agents.get(&id).map(|e| {
                    (e.agent_binary.clone(), e.is_worktree, e.repo_path.clone(), e.branch_name.clone())
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
                        attached_clients: e
                            .attached_count
                            .load(Ordering::Relaxed),
                        hub: e.hub.clone(),
                        working_dir: e.working_dir.clone(),
                        repo_path: e.repo_path.clone(),
                        branch_name: e.branch_name.clone(),
                        is_worktree: e.is_worktree,
                    })
                    .collect()
            };
            clust_ipc::send_message_write(&mut writer, &HubMessage::AgentList { agents })
                .await?;
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                let snapshots: std::collections::HashMap<
                    String,
                    crate::repo::AgentSnapshot,
                > = hub_state
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
                            },
                        )
                    })
                    .collect();
                (list, snapshots)
            };
            // Do git I/O outside the lock
            let mut valid_repos = Vec::new();
            let mut stale_paths = Vec::new();
            for (path, name, color) in repo_list {
                match crate::repo::get_repo_state(
                    std::path::Path::new(&path),
                    &name,
                    &agent_snapshots,
                ) {
                    Some(mut info) => {
                        info.color = color;
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
                &HubMessage::RepoList {
                    repos: valid_repos,
                },
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                                        attached_clients: v
                                            .attached_count
                                            .load(Ordering::Relaxed),
                                        hub: v.hub.clone(),
                                        working_dir: v.working_dir.clone(),
                                        repo_path: v.repo_path.clone(),
                                        branch_name: v.branch_name.clone(),
                                        is_worktree: v.is_worktree,
                                    },
                                )
                            })
                            .collect::<std::collections::HashMap<_, _>>()
                    };
                    let name = root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match crate::repo::list_worktrees(&root, &agent_snapshots) {
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                    ) {
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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

                    // Stop agents running in this worktree
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

                    match crate::repo::remove_worktree(
                        &root,
                        &branch_name,
                        delete_local_branch,
                        force,
                    ) {
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                                        attached_clients: v
                                            .attached_count
                                            .load(Ordering::Relaxed),
                                        hub: v.hub.clone(),
                                        working_dir: v.working_dir.clone(),
                                        repo_path: v.repo_path.clone(),
                                        branch_name: v.branch_name.clone(),
                                        is_worktree: v.is_worktree,
                                    },
                                )
                            })
                            .collect::<std::collections::HashMap<_, _>>()
                    };
                    match crate::repo::list_worktrees(&root, &agent_snapshots) {
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
            accept_edits,
            hub,
        } => {
            // Determine branch name and whether to create or checkout.
            // Sanitize new_branch (user input) but not target_branch (from git).
            let sanitized_new = new_branch
                .as_deref()
                .map(clust_ipc::branch::sanitize_branch_name);
            let branch_name = sanitized_new
                .as_deref()
                .or(target_branch.as_deref())
                .ok_or("either target_branch or new_branch must be provided");

            let branch_name = match branch_name {
                Ok(b) => b.to_string(),
                Err(e) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e.to_string() },
                    )
                    .await?;
                    return Ok(());
                }
            };

            // Use the existing add_worktree which places worktrees at
            // <repo>/.clust/worktrees/<serialized_branch>/
            let repo_root = std::path::Path::new(&repo_path);
            let checkout_existing = new_branch.is_none();
            let base = if new_branch.is_some() {
                target_branch.as_deref()
            } else {
                None
            };

            let worktree_path = match crate::repo::add_worktree(
                repo_root,
                &branch_name,
                base,
                checkout_existing,
            ) {
                Ok(path) => path,
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
                    return Ok(());
                }
            };

            let working_dir = worktree_path.to_string_lossy().into_owned();

            // Detect git info from the new worktree
            let (wt_repo_path, wt_branch_name, is_worktree) =
                match crate::repo::detect_git_root(&working_dir) {
                    Some(root) => {
                        let rp = root.to_string_lossy().into_owned();
                        let (bn, iw) =
                            crate::repo::detect_branch_and_worktree(&working_dir);
                        (Some(rp), bn.or(Some(branch_name)), iw)
                    }
                    None => (Some(repo_path.clone()), Some(branch_name), true),
                };

            // Clone git info before moving into SpawnAgentParams
            let response_repo_path = wt_repo_path.clone();
            let response_branch_name = wt_branch_name.clone();

            // Spawn agent in the worktree
            let result = {
                let mut hub_state = state.lock().await;
                agent::spawn_agent(
                    &mut hub_state,
                    agent::SpawnAgentParams {
                        prompt,
                        agent_binary,
                        working_dir: working_dir.clone(),
                        cols,
                        rows,
                        accept_edits,
                        hub,
                        repo_path: wt_repo_path,
                        branch_name: wt_branch_name,
                        is_worktree,
                    },
                    state.clone(),
                )
            };

            match result {
                Ok((id, binary)) => {
                    // Auto-register repo
                    {
                        let hub_state = state.lock().await;
                        if let Some(ref db) = hub_state.db {
                            let name = std::path::Path::new(&repo_path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| repo_path.clone());
                            let _ = crate::db::register_repo(db, &repo_path, &name, "");
                        }
                    }
                    // One-shot response (no streaming — TUI attaches separately)
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                                    && e.branch_name.as_deref()
                                        == Some(branch_name.as_str())
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

                    match crate::repo::delete_local_branch(&root, &branch_name, force)
                    {
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
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::Error { message: e },
                    )
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
                Ok(root) => {
                    match crate::repo::delete_remote_branch(&root, &branch_name) {
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
                            .filter(|e| {
                                e.repo_path.as_deref() == Some(root_str.as_str())
                            })
                            .map(|e| e.id.clone())
                            .collect::<Vec<_>>()
                    };
                    let stopped_agents = agent_ids.len();

                    if stopped_agents > 0 {
                        let label = if stopped_agents == 1 { "agent" } else { "agents" };
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

                    // Phase 2: Remove worktrees
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Removing worktrees".to_string(),
                        },
                    )
                    .await?;
                    let removed_worktrees =
                        crate::repo::purge_worktrees(&root).unwrap_or(0);

                    // Phase 3: Delete local branches
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Deleting local branches".to_string(),
                        },
                    )
                    .await?;
                    let deleted_branches =
                        crate::repo::purge_branches(&root).unwrap_or(0);

                    // Phase 4: Clean stale refs
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::PurgeProgress {
                            step: "Cleaning stale refs".to_string(),
                        },
                    )
                    .await?;
                    let _ = crate::repo::clean_stale_refs(&root);

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
                Ok(root) => {
                    match crate::repo::clean_stale_refs(&root) {
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
        CliMessage::PullBranch {
            repo_path,
            branch_name,
        } => {
            let repo_root = std::path::Path::new(&repo_path);
            match crate::repo::pull_branch(repo_root, &branch_name) {
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
    let (mut output_rx, client_id, replay_buf) = {
        let hub = state.lock().await;
        let entry = hub.agents.get(agent_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "agent not found")
        })?;
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        let cid = entry.next_client_id();
        (entry.output_tx.subscribe(), cid, entry.replay_buffer.clone())
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
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::HubShutdown,
                    )
                    .await;
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

    // Decrement attached count and remove client from size tracking
    let mut hub = state_for_cleanup.lock().await;
    if let Some(entry) = hub.agents.get_mut(&agent_id_for_cleanup) {
        entry.attached_count.fetch_sub(1, Ordering::Relaxed);
        entry.client_sizes.remove(&client_id);
        if entry.active_client_id == Some(client_id) {
            entry.active_client_id = None;
        }
    }

    Ok(())
}
