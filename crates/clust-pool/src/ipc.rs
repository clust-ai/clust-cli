use std::io;
use std::sync::atomic::Ordering;

use tao::event_loop::EventLoopProxy;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixListener;

use clust_ipc::{CliMessage, PoolMessage};

use crate::agent::{self, AgentEvent, SharedPoolState};
use crate::PoolEvent;

/// Run the IPC server, listening for CLI connections on the Unix domain socket.
/// Runs inside a tokio runtime on a background thread.
pub async fn run_ipc_server(
    shutdown_proxy: EventLoopProxy<PoolEvent>,
    state: SharedPoolState,
) {
    if let Err(e) = run(shutdown_proxy, state).await {
        eprintln!("ipc server error: {e}");
    }
}

async fn run(
    shutdown_proxy: EventLoopProxy<PoolEvent>,
    state: SharedPoolState,
) -> io::Result<()> {
    let dir = clust_ipc::clust_dir();
    tokio::fs::create_dir_all(&dir).await?;

    let sock_path = clust_ipc::socket_path();

    // Remove stale socket file if it exists (crash recovery per docs/pool.md)
    let _ = tokio::fs::remove_file(&sock_path).await;

    let listener = UnixListener::bind(&sock_path)?;

    loop {
        let (stream, _addr) = listener.accept().await?;

        let proxy = shutdown_proxy.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, proxy, state).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    shutdown_proxy: EventLoopProxy<PoolEvent>,
    state: SharedPoolState,
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
        } => {
            let result = {
                let mut pool = state.lock().await;
                agent::spawn_agent(
                    &mut pool,
                    prompt,
                    agent_binary,
                    working_dir,
                    cols,
                    rows,
                    state.clone(),
                )
            };
            match result {
                Ok((id, binary)) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &PoolMessage::AgentStarted {
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
                        &PoolMessage::Error { message: e },
                    )
                    .await?;
                }
            }
        }
        CliMessage::AttachAgent { id } => {
            let agent_info = {
                let pool = state.lock().await;
                pool.agents.get(&id).map(|e| e.agent_binary.clone())
            };
            match agent_info {
                Some(binary) => {
                    clust_ipc::send_message_write(
                        &mut writer,
                        &PoolMessage::AgentAttached {
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
                        &PoolMessage::Error {
                            message: format!("agent {id} not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        CliMessage::ListAgents => {
            let agents = {
                let pool = state.lock().await;
                pool.agents
                    .values()
                    .map(|e| clust_ipc::AgentInfo {
                        id: e.id.clone(),
                        agent_binary: e.agent_binary.clone(),
                        started_at: e.started_at.clone(),
                        attached_clients: e
                            .attached_count
                            .load(Ordering::Relaxed),
                    })
                    .collect()
            };
            clust_ipc::send_message_write(&mut writer, &PoolMessage::AgentList { agents })
                .await?;
        }
        CliMessage::StopPool => {
            clust_ipc::send_message_write(&mut writer, &PoolMessage::Ok).await?;

            // Terminate all running agents (SIGTERM → 3s → SIGKILL)
            agent::shutdown_agents(&state).await;

            // Clean up socket file before signaling shutdown
            let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;

            let _ = shutdown_proxy.send_event(PoolEvent::Shutdown);
        }
        _ => {
            clust_ipc::send_message_write(
                &mut writer,
                &PoolMessage::Error {
                    message: "not yet implemented".into(),
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
    state: SharedPoolState,
) -> io::Result<()> {
    // Subscribe to agent output broadcast and assign a client ID
    let (mut output_rx, client_id) = {
        let pool = state.lock().await;
        let entry = pool.agents.get(agent_id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "agent not found")
        })?;
        entry.attached_count.fetch_add(1, Ordering::Relaxed);
        let cid = entry.next_client_id();
        (entry.output_tx.subscribe(), cid)
    };

    let agent_id_owned = agent_id.to_string();
    let state_for_cleanup = state.clone();
    let agent_id_for_cleanup = agent_id_owned.clone();

    // Task 1: Read from broadcast channel, send PoolMessages to CLI
    let agent_id_for_output = agent_id_owned.clone();
    let output_task = tokio::spawn(async move {
        loop {
            match output_rx.recv().await {
                Ok(AgentEvent::Output(data)) => {
                    if clust_ipc::send_message_write(
                        &mut writer,
                        &PoolMessage::AgentOutput {
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
                        &PoolMessage::AgentExited {
                            id: agent_id_for_output.clone(),
                            exit_code: code,
                        },
                    )
                    .await;
                    break;
                }
                Ok(AgentEvent::PoolShutdown) => {
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &PoolMessage::PoolShutdown,
                    )
                    .await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Dropped frames — OK for terminal output, client catches up
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // Agent removed from state (already exited)
                    let _ = clust_ipc::send_message_write(
                        &mut writer,
                        &PoolMessage::AgentExited {
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
                    let mut pool = state_for_input.lock().await;
                    if let Some(entry) = pool.agents.get_mut(&agent_id_for_input) {
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
                    let mut pool = state_for_input.lock().await;
                    if let Some(entry) = pool.agents.get_mut(&agent_id_for_input) {
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
    let mut pool = state_for_cleanup.lock().await;
    if let Some(entry) = pool.agents.get_mut(&agent_id_for_cleanup) {
        entry.attached_count.fetch_sub(1, Ordering::Relaxed);
        entry.client_sizes.remove(&client_id);
        if entry.active_client_id == Some(client_id) {
            entry.active_client_id = None;
        }
    }

    Ok(())
}
