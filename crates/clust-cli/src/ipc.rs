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
