use std::io;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::sleep;

use clust_ipc::{CliMessage, HubMessage};

use crate::hub_launcher;

/// Connect to the hub, auto-spawning it if not running.
/// Detects stale hubs via protocol version check and restarts them.
/// Retries every 50ms for up to 2 seconds after spawning.
pub async fn connect_to_hub() -> io::Result<UnixStream> {
    let sock = clust_ipc::socket_path();

    // Try connecting first without spawning
    if UnixStream::connect(&sock).await.is_ok() {
        // Hub is running — verify protocol compatibility
        match check_hub_protocol().await {
            Ok(()) => {
                // Compatible — return a fresh connection
                return UnixStream::connect(&sock).await;
            }
            Err(_) => {
                // Stale hub — stop it, then fall through to spawn a new one
                if let Ok(mut stream) = UnixStream::connect(&sock).await {
                    let _ = send_stop(&mut stream).await;
                }
                // Give old hub time to release the socket
                sleep(Duration::from_millis(200)).await;
            }
        }
    }

    // Hub not running (or we just stopped a stale one) — spawn it
    hub_launcher::spawn_hub()?;

    // Retry with backoff
    let max_wait = Duration::from_secs(2);
    let interval = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;

    while elapsed < max_wait {
        sleep(interval).await;
        elapsed += interval;

        if let Ok(stream) = UnixStream::connect(&sock).await {
            return Ok(stream);
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for hub to start (check {} for errors)",
            clust_ipc::log_path().display()
        ),
    ))
}

/// Check that the running hub speaks the same protocol version.
async fn check_hub_protocol() -> io::Result<()> {
    let mut stream = try_connect().await?;
    clust_ipc::send_message(
        &mut stream,
        &CliMessage::Ping {
            protocol_version: clust_ipc::PROTOCOL_VERSION,
        },
    )
    .await?;
    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::Pong { protocol_version })
            if protocol_version == clust_ipc::PROTOCOL_VERSION =>
        {
            Ok(())
        }
        Ok(HubMessage::Pong { protocol_version }) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "hub protocol mismatch: hub={protocol_version}, cli={}",
                clust_ipc::PROTOCOL_VERSION
            ),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hub did not respond to ping (likely outdated)",
        )),
    }
}

/// Try to connect to an existing hub without spawning one.
pub async fn try_connect() -> io::Result<UnixStream> {
    UnixStream::connect(clust_ipc::socket_path()).await
}

/// Count unique hubs by querying the agent list. Returns 1 on failure.
pub async fn count_hubs() -> usize {
    let Ok(mut stream) = try_connect().await else {
        return 1;
    };
    if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: None })
        .await
        .is_err()
    {
        return 1;
    }
    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::AgentList { agents }) => {
            let mut names: Vec<&str> = agents.iter().map(|a| a.hub.as_str()).collect();
            names.sort();
            names.dedup();
            names.len().max(1)
        }
        _ => 1,
    }
}

/// Send a StopHub message and print the result.
pub async fn send_stop(stream: &mut UnixStream) -> io::Result<()> {
    clust_ipc::send_message(stream, &CliMessage::StopHub).await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::Ok => {}
        HubMessage::Error { message } => eprintln!("error stopping hub: {message}"),
        _ => {}
    }

    Ok(())
}

/// Send a StopAgent message and return the result.
pub async fn send_stop_agent(stream: &mut UnixStream, id: &str) -> io::Result<()> {
    clust_ipc::send_message(stream, &CliMessage::StopAgent { id: id.to_string() }).await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::AgentStopped { .. } => Ok(()),
        HubMessage::Error { message } => {
            Err(io::Error::other(message))
        }
        _ => Ok(()),
    }
}

/// Send an UnregisterRepo message and return (name, stopped_agents) on success.
pub async fn send_unregister_repo(
    stream: &mut UnixStream,
    path: &str,
) -> io::Result<(String, usize)> {
    clust_ipc::send_message(
        stream,
        &CliMessage::UnregisterRepo { path: path.to_string() },
    )
    .await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::RepoUnregistered { name, stopped_agents, .. } => Ok((name, stopped_agents)),
        HubMessage::Error { message } => {
            Err(io::Error::other(message))
        }
        _ => Err(io::Error::other("unexpected response")),
    }
}

/// Send a StopRepoAgents message and return the stopped count on success.
pub async fn send_stop_repo_agents(
    stream: &mut UnixStream,
    path: &str,
) -> io::Result<usize> {
    clust_ipc::send_message(
        stream,
        &CliMessage::StopRepoAgents { path: path.to_string() },
    )
    .await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::RepoAgentsStopped { stopped_count, .. } => Ok(stopped_count),
        HubMessage::Error { message } => {
            Err(io::Error::other(message))
        }
        _ => Err(io::Error::other("unexpected response")),
    }
}

/// Fetch the full agent list from the hub. Returns empty vec if hub is unreachable.
pub async fn fetch_agent_list() -> Vec<clust_ipc::AgentInfo> {
    let Ok(mut stream) = try_connect().await else {
        return vec![];
    };
    if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: None })
        .await
        .is_err()
    {
        return vec![];
    }
    match clust_ipc::recv_message::<HubMessage>(&mut stream).await {
        Ok(HubMessage::AgentList { agents }) => agents,
        _ => vec![],
    }
}
