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
                                let _ = crate::db::register_repo(db, &root_str, &name);
                            }
                        }
                    }
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentStarted {
                            id: id.clone(),
                            agent_binary: binary,
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
                hub.agents.get(&id).map(|e| e.agent_binary.clone())
            };
            match agent_info {
                Some(binary) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &HubMessage::AgentAttached {
                            id: id.clone(),
                            agent_binary: binary,
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
                            crate::db::register_repo(db, &root_str, &name)
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
                                repo_path: v.repo_path.clone(),
                                branch_name: v.branch_name.clone(),
                            },
                        )
                    })
                    .collect();
                (list, snapshots)
            };
            // Do git I/O outside the lock
            let mut valid_repos = Vec::new();
            let mut stale_paths = Vec::new();
            for (path, name) in repo_list {
                match crate::repo::get_repo_state(
                    std::path::Path::new(&path),
                    &name,
                    &agent_snapshots,
                ) {
                    Some(info) => valid_repos.push(info),
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
