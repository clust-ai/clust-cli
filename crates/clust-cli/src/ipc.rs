use std::io;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::sleep;

use clust_ipc::{CliMessage, PoolMessage};

use crate::pool_launcher;

/// Connect to the pool, auto-spawning it if not running.
/// Retries every 50ms for up to 2 seconds after spawning.
pub async fn connect_to_pool() -> io::Result<UnixStream> {
    let sock = clust_ipc::socket_path();

    // Try connecting first without spawning
    if let Ok(stream) = UnixStream::connect(&sock).await {
        return Ok(stream);
    }

    // Pool not running — spawn it
    pool_launcher::spawn_pool()?;

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
        "timed out waiting for pool to start",
    ))
}

/// Try to connect to an existing pool without spawning one.
pub async fn try_connect() -> io::Result<UnixStream> {
    UnixStream::connect(clust_ipc::socket_path()).await
}

/// Count unique pools by querying the agent list. Returns 1 on failure.
pub async fn count_pools() -> usize {
    let Ok(mut stream) = try_connect().await else {
        return 1;
    };
    if clust_ipc::send_message(&mut stream, &CliMessage::ListAgents { pool: None })
        .await
        .is_err()
    {
        return 1;
    }
    match clust_ipc::recv_message::<PoolMessage>(&mut stream).await {
        Ok(PoolMessage::AgentList { agents }) => {
            let mut names: Vec<&str> = agents.iter().map(|a| a.pool.as_str()).collect();
            names.sort();
            names.dedup();
            names.len().max(1)
        }
        _ => 1,
    }
}

/// Send a StopPool message and print the result.
pub async fn send_stop(stream: &mut UnixStream) -> io::Result<()> {
    clust_ipc::send_message(stream, &CliMessage::StopPool).await?;
    let response: PoolMessage = clust_ipc::recv_message(stream).await?;

    match response {
        PoolMessage::Ok => {}
        PoolMessage::Error { message } => eprintln!("error stopping pool: {message}"),
        _ => {}
    }

    Ok(())
}

/// Send a StopAgent message and return the result.
pub async fn send_stop_agent(stream: &mut UnixStream, id: &str) -> io::Result<()> {
    clust_ipc::send_message(stream, &CliMessage::StopAgent { id: id.to_string() }).await?;
    let response: PoolMessage = clust_ipc::recv_message(stream).await?;

    match response {
        PoolMessage::AgentStopped { .. } => Ok(()),
        PoolMessage::Error { message } => {
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
    let response: PoolMessage = clust_ipc::recv_message(stream).await?;

    match response {
        PoolMessage::RepoUnregistered { name, stopped_agents, .. } => Ok((name, stopped_agents)),
        PoolMessage::Error { message } => {
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
    let response: PoolMessage = clust_ipc::recv_message(stream).await?;

    match response {
        PoolMessage::RepoAgentsStopped { stopped_count, .. } => Ok(stopped_count),
        PoolMessage::Error { message } => {
            Err(io::Error::other(message))
        }
        _ => Err(io::Error::other("unexpected response")),
    }
}
