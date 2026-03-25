use std::io;

use tao::event_loop::EventLoopProxy;
use tokio::net::UnixListener;

use clust_ipc::{CliMessage, PoolMessage};

use crate::PoolEvent;

/// Run the IPC server, listening for CLI connections on the Unix domain socket.
/// Runs inside a tokio runtime on a background thread.
pub async fn run_ipc_server(shutdown_proxy: EventLoopProxy<PoolEvent>) {
    if let Err(e) = run(shutdown_proxy).await {
        eprintln!("ipc server error: {e}");
    }
}

async fn run(shutdown_proxy: EventLoopProxy<PoolEvent>) -> io::Result<()> {
    let dir = clust_ipc::clust_dir();
    tokio::fs::create_dir_all(&dir).await?;

    let sock_path = clust_ipc::socket_path();

    // Remove stale socket file if it exists (crash recovery per docs/pool.md)
    let _ = tokio::fs::remove_file(&sock_path).await;

    let listener = UnixListener::bind(&sock_path)?;

    loop {
        let (mut stream, _addr) = listener.accept().await?;

        let proxy = shutdown_proxy.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(&mut stream, proxy).await {
                eprintln!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: &mut tokio::net::UnixStream,
    shutdown_proxy: EventLoopProxy<PoolEvent>,
) -> io::Result<()> {
    let msg: CliMessage = clust_ipc::recv_message(stream).await?;

    match msg {
        CliMessage::StopPool => {
            clust_ipc::send_message(stream, &PoolMessage::Ok).await?;

            // Clean up socket file before signaling shutdown
            let _ = tokio::fs::remove_file(clust_ipc::socket_path()).await;

            let _ = shutdown_proxy.send_event(PoolEvent::Shutdown);
        }
        _ => {
            clust_ipc::send_message(
                stream,
                &PoolMessage::Error {
                    message: "not yet implemented".into(),
                },
            )
            .await?;
        }
    }

    Ok(())
}
