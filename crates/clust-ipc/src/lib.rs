use std::io;
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Messages sent from CLI to Pool.
#[derive(Debug, Serialize, Deserialize)]
pub enum CliMessage {
    StartAgent {
        prompt: Option<String>,
        agent_binary: Option<String>,
        working_dir: String,
    },
    AttachAgent { id: String },
    DetachAgent { id: String },
    ListAgents,
    StopPool,
    SetDefault { agent_binary: String },
    GetDefault,
}

/// Info about a running agent, returned in AgentList.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub attached_clients: usize,
}

/// Messages sent from Pool to CLI.
#[derive(Debug, Serialize, Deserialize)]
pub enum PoolMessage {
    Ok,
    AgentStarted { id: String },
    AgentOutput { id: String, data: Vec<u8> },
    AgentExited { id: String, exit_code: i32 },
    AgentList { agents: Vec<AgentInfo> },
    Error { message: String },
}

/// Returns the clust data directory: `~/.clust/`.
pub fn clust_dir() -> PathBuf {
    dirs::home_dir()
        .expect("could not determine home directory")
        .join(".clust")
}

/// Returns the IPC socket path: `~/.clust/clust.sock`.
pub fn socket_path() -> PathBuf {
    clust_dir().join("clust.sock")
}

/// Send a length-prefixed MessagePack message over a Unix stream.
pub async fn send_message<T: Serialize>(stream: &mut UnixStream, msg: &T) -> io::Result<()> {
    let payload = rmp_serde::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive a length-prefixed MessagePack message from a Unix stream.
pub async fn recv_message<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    rmp_serde::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
