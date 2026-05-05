use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::sleep;

use clust_ipc::{CliMessage, HubMessage};

use crate::hub_launcher;

/// How long to wait for the old hub's socket file to disappear after StopHub.
const SOCKET_REMOVAL_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for a freshly spawned hub to bind the socket.
const HUB_BIND_TIMEOUT: Duration = Duration::from_secs(2);
/// Polling cadence for both wait loops.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Initial settle delay after spawning the hub before the first connect attempt.
const SPAWN_INITIAL_DELAY: Duration = Duration::from_millis(50);

/// Wait for the socket file at `sock` to disappear.
///
/// Polls every `POLL_INTERVAL` for up to `SOCKET_REMOVAL_TIMEOUT`. Returns Ok
/// once the socket is gone, or a TimedOut error if it persists.
async fn wait_for_socket_removal(sock: &Path) -> io::Result<()> {
    let mut elapsed = Duration::ZERO;
    while elapsed < SOCKET_REMOVAL_TIMEOUT {
        if !sock.exists() {
            return Ok(());
        }
        sleep(POLL_INTERVAL).await;
        elapsed += POLL_INTERVAL;
    }
    if !sock.exists() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for stale hub to release socket at {}",
            sock.display()
        ),
    ))
}

/// Wait for a freshly spawned hub to bind the socket and accept a connection.
///
/// Adds a brief initial delay so we don't race the hub before it has a chance
/// to bind, then polls every `POLL_INTERVAL` for up to `HUB_BIND_TIMEOUT`.
async fn wait_for_hub_bind(sock: &Path) -> io::Result<UnixStream> {
    // Give the hub a brief head start so its initial bind has a chance to land
    // before our first connect attempt.
    sleep(SPAWN_INITIAL_DELAY).await;

    let mut elapsed = Duration::ZERO;
    while elapsed < HUB_BIND_TIMEOUT {
        if sock.exists() {
            if let Ok(stream) = UnixStream::connect(sock).await {
                return Ok(stream);
            }
        }
        sleep(POLL_INTERVAL).await;
        elapsed += POLL_INTERVAL;
    }
    // Final attempt before giving up
    if sock.exists() {
        if let Ok(stream) = UnixStream::connect(sock).await {
            return Ok(stream);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "hub spawned but did not bind socket within {}s — check {} for errors",
            HUB_BIND_TIMEOUT.as_secs(),
            clust_ipc::log_path().display()
        ),
    ))
}

/// Connect to the hub, auto-spawning it if not running.
/// Detects stale hubs via protocol version check and restarts them.
/// Polls for socket existence with a 2 second ceiling around each phase.
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
                // Stale hub — stop it, then poll for socket removal before
                // spawning a new one.
                if let Ok(mut stream) = UnixStream::connect(&sock).await {
                    let _ = send_stop(&mut stream).await;
                }
                wait_for_socket_removal(&sock).await?;
            }
        }
    }

    // Hub not running (or we just stopped a stale one) — spawn it
    hub_launcher::spawn_hub()?;

    // Wait for the hub to bind, with an initial settle delay.
    wait_for_hub_bind(&sock).await
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
        HubMessage::Error { message } => Err(io::Error::other(message)),
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
        &CliMessage::UnregisterRepo {
            path: path.to_string(),
        },
    )
    .await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::RepoUnregistered {
            name,
            stopped_agents,
            ..
        } => Ok((name, stopped_agents)),
        HubMessage::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected response")),
    }
}

/// Send a StopRepoAgents message and return the stopped count on success.
pub async fn send_stop_repo_agents(stream: &mut UnixStream, path: &str) -> io::Result<usize> {
    clust_ipc::send_message(
        stream,
        &CliMessage::StopRepoAgents {
            path: path.to_string(),
        },
    )
    .await?;
    let response: HubMessage = clust_ipc::recv_message(stream).await?;

    match response {
        HubMessage::RepoAgentsStopped { stopped_count, .. } => Ok(stopped_count),
        HubMessage::Error { message } => Err(io::Error::other(message)),
        _ => Err(io::Error::other("unexpected response")),
    }
}

/// Fetch the full agent list from the hub.
///
/// Returns `Ok(agents)` on success (possibly empty), or `Err` if the hub is
/// unreachable or returned an unexpected response. Callers must distinguish
/// between "no agents running" (legitimately empty) and "could not query hub"
/// to avoid silently skipping cleanup prompts.
pub async fn try_fetch_agent_list() -> io::Result<Vec<clust_ipc::AgentInfo>> {
    let mut stream = try_connect().await?;
    clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { hub: None }).await?;
    match clust_ipc::recv_message::<HubMessage>(&mut stream).await? {
        HubMessage::AgentList { agents } => Ok(agents),
        _ => Err(io::Error::other(
            "unexpected response from hub while listing agents",
        )),
    }
}

/// Fetch the full agent list from the hub. Returns empty vec if hub is unreachable.
///
/// Prefer [`try_fetch_agent_list`] when the caller needs to distinguish between
/// "empty" and "hub unreachable" (e.g. to avoid silently skipping cleanup prompts).
pub async fn fetch_agent_list() -> Vec<clust_ipc::AgentInfo> {
    try_fetch_agent_list().await.unwrap_or_default()
}
