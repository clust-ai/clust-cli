pub mod agents;

use std::io;
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// Default pool name for agents not assigned to a specific pool.
pub const DEFAULT_POOL: &str = "default_pool";

/// Messages sent from CLI to Pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CliMessage {
    StartAgent {
        prompt: Option<String>,
        agent_binary: Option<String>,
        working_dir: String,
        cols: u16,
        rows: u16,
        accept_edits: bool,
        pool: String,
    },
    AttachAgent { id: String },
    DetachAgent { id: String },
    AgentInput { id: String, data: Vec<u8> },
    ResizeAgent { id: String, cols: u16, rows: u16 },
    ListAgents { pool: Option<String> },
    StopPool,
    StopAgent { id: String },
    SetDefault { agent_binary: String },
    GetDefault,
    RegisterRepo { path: String },
    ListRepos,
}

/// Info about a running agent, returned in AgentList.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub attached_clients: usize,
    pub pool: String,
    pub working_dir: String,
}

/// Info about a registered repository, returned in RepoList.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: String,
    pub name: String,
    pub local_branches: Vec<BranchInfo>,
    pub remote_branches: Vec<BranchInfo>,
}

/// Info about a single branch within a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub active_agent_id: Option<String>,
    pub is_worktree: bool,
}

/// Messages sent from Pool to CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PoolMessage {
    Ok,
    AgentStarted { id: String, agent_binary: String },
    AgentAttached { id: String, agent_binary: String },
    AgentOutput { id: String, data: Vec<u8> },
    AgentExited { id: String, exit_code: i32 },
    AgentList { agents: Vec<AgentInfo> },
    DefaultAgent { agent_binary: Option<String> },
    AgentStopped { id: String },
    PoolShutdown,
    Error { message: String },
    RepoRegistered { path: String, name: String },
    RepoList { repos: Vec<RepoInfo> },
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

/// Send a length-prefixed MessagePack message over a split write half.
/// Used for bidirectional streaming sessions where read and write happen concurrently.
pub async fn send_message_write<T: Serialize>(
    writer: &mut OwnedWriteHalf,
    msg: &T,
) -> io::Result<()> {
    let payload =
        rmp_serde::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Receive a length-prefixed MessagePack message from a split read half.
/// Used for bidirectional streaming sessions where read and write happen concurrently.
pub async fn recv_message_read<T: DeserializeOwned>(
    reader: &mut OwnedReadHalf,
) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    rmp_serde::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    // ── Round-trip helpers ──────────────────────────────────────────

    async fn assert_cli_round_trip(msg: CliMessage) {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        send_message(&mut a, &msg).await.unwrap();
        let received: CliMessage = recv_message(&mut b).await.unwrap();
        assert_eq!(msg, received);
    }

    async fn assert_pool_round_trip(msg: PoolMessage) {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        send_message(&mut a, &msg).await.unwrap();
        let received: PoolMessage = recv_message(&mut b).await.unwrap();
        assert_eq!(msg, received);
    }

    // ── CliMessage round-trips ─────────────────────────────────────

    #[tokio::test]
    async fn cli_start_agent_all_fields() {
        assert_cli_round_trip(CliMessage::StartAgent {
            prompt: Some("do something".into()),
            agent_binary: Some("claude".into()),
            working_dir: "/tmp".into(),
            cols: 120,
            rows: 40,
            accept_edits: false,
            pool: DEFAULT_POOL.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_start_agent_no_optionals() {
        assert_cli_round_trip(CliMessage::StartAgent {
            prompt: None,
            agent_binary: None,
            working_dir: "/home/user".into(),
            cols: 80,
            rows: 24,
            accept_edits: false,
            pool: DEFAULT_POOL.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_start_agent_accept_edits() {
        assert_cli_round_trip(CliMessage::StartAgent {
            prompt: Some("fix tests".into()),
            agent_binary: Some("claude".into()),
            working_dir: "/tmp".into(),
            cols: 120,
            rows: 40,
            accept_edits: true,
            pool: DEFAULT_POOL.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_start_agent_custom_pool() {
        assert_cli_round_trip(CliMessage::StartAgent {
            prompt: None,
            agent_binary: Some("claude".into()),
            working_dir: "/tmp".into(),
            cols: 80,
            rows: 24,
            accept_edits: false,
            pool: "my_feature".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_attach_agent() {
        assert_cli_round_trip(CliMessage::AttachAgent {
            id: "abc123".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_detach_agent() {
        assert_cli_round_trip(CliMessage::DetachAgent {
            id: "def456".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_list_agents_no_filter() {
        assert_cli_round_trip(CliMessage::ListAgents { pool: None }).await;
    }

    #[tokio::test]
    async fn cli_list_agents_with_pool_filter() {
        assert_cli_round_trip(CliMessage::ListAgents {
            pool: Some("my_feature".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_stop_pool() {
        assert_cli_round_trip(CliMessage::StopPool).await;
    }

    #[tokio::test]
    async fn cli_stop_agent() {
        assert_cli_round_trip(CliMessage::StopAgent {
            id: "abc123".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_set_default() {
        assert_cli_round_trip(CliMessage::SetDefault {
            agent_binary: "aider".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_get_default() {
        assert_cli_round_trip(CliMessage::GetDefault).await;
    }

    // ── PoolMessage round-trips ────────────────────────────────────

    #[tokio::test]
    async fn pool_ok() {
        assert_pool_round_trip(PoolMessage::Ok).await;
    }

    #[tokio::test]
    async fn pool_agent_started() {
        assert_pool_round_trip(PoolMessage::AgentStarted {
            id: "a1b2c3".into(),
            agent_binary: "claude".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_attached() {
        assert_pool_round_trip(PoolMessage::AgentAttached {
            id: "a1b2c3".into(),
            agent_binary: "claude".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_output() {
        assert_pool_round_trip(PoolMessage::AgentOutput {
            id: "a1b2c3".into(),
            data: vec![0x00, 0xFF, 0x42],
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_exited() {
        assert_pool_round_trip(PoolMessage::AgentExited {
            id: "a1b2c3".into(),
            exit_code: 42,
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_list_populated() {
        assert_pool_round_trip(PoolMessage::AgentList {
            agents: vec![
                AgentInfo {
                    id: "aaa111".into(),
                    agent_binary: "claude".into(),
                    started_at: "2026-03-25T10:00:00Z".into(),
                    attached_clients: 2,
                    pool: DEFAULT_POOL.into(),
                    working_dir: "/tmp/project".into(),
                },
                AgentInfo {
                    id: "bbb222".into(),
                    agent_binary: "aider".into(),
                    started_at: "2026-03-25T11:00:00Z".into(),
                    attached_clients: 0,
                    pool: "my_feature".into(),
                    working_dir: "/home/user/code".into(),
                },
            ],
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_list_empty() {
        assert_pool_round_trip(PoolMessage::AgentList { agents: vec![] }).await;
    }

    #[tokio::test]
    async fn pool_default_agent_some() {
        assert_pool_round_trip(PoolMessage::DefaultAgent {
            agent_binary: Some("claude".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn pool_default_agent_none() {
        assert_pool_round_trip(PoolMessage::DefaultAgent {
            agent_binary: None,
        })
        .await;
    }

    #[tokio::test]
    async fn pool_agent_stopped() {
        assert_pool_round_trip(PoolMessage::AgentStopped {
            id: "abc123".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn pool_shutdown() {
        assert_pool_round_trip(PoolMessage::PoolShutdown).await;
    }

    #[tokio::test]
    async fn pool_error() {
        assert_pool_round_trip(PoolMessage::Error {
            message: "something went wrong".into(),
        })
        .await;
    }

    // ── Framing / edge cases ───────────────────────────────────────

    #[tokio::test]
    async fn multiple_messages_on_one_stream() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let msgs = vec![
            CliMessage::ListAgents { pool: None },
            CliMessage::StopPool,
            CliMessage::SetDefault {
                agent_binary: "claude".into(),
            },
        ];

        for msg in &msgs {
            send_message(&mut a, msg).await.unwrap();
        }

        for expected in &msgs {
            let received: CliMessage = recv_message(&mut b).await.unwrap();
            assert_eq!(expected, &received);
        }
    }

    #[tokio::test]
    async fn large_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let data = vec![0xAB; 1_000_000]; // 1 MB
        let msg = PoolMessage::AgentOutput {
            id: "big".into(),
            data,
        };
        let expected = msg.clone();
        // Must read concurrently — socket buffer is smaller than 1MB
        let writer = tokio::spawn(async move { send_message(&mut a, &msg).await.unwrap() });
        let received: PoolMessage = recv_message(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(expected, received);
    }

    #[tokio::test]
    async fn empty_data_payload() {
        assert_pool_round_trip(PoolMessage::AgentOutput {
            id: "empty".into(),
            data: vec![],
        })
        .await;
    }

    // ── Error handling ─────────────────────────────────────────────

    #[tokio::test]
    async fn recv_on_closed_stream() {
        let (a, mut b) = UnixStream::pair().unwrap();
        drop(a); // close the writer
        let result: io::Result<CliMessage> = recv_message(&mut b).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn malformed_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        // Write a valid length prefix but garbage payload
        let garbage = b"not valid msgpack";
        let len = garbage.len() as u32;
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(garbage).await.unwrap();
        a.flush().await.unwrap();

        let result: io::Result<CliMessage> = recv_message(&mut b).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn truncated_payload() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        // Claim 100 bytes but only send 10, then drop
        let len: u32 = 100;
        a.write_all(&len.to_be_bytes()).await.unwrap();
        a.write_all(&[0u8; 10]).await.unwrap();
        drop(a);

        let result: io::Result<CliMessage> = recv_message(&mut b).await;
        assert!(result.is_err());
    }

    // ── New message variant round-trips ────────────────────────

    #[tokio::test]
    async fn cli_agent_input() {
        assert_cli_round_trip(CliMessage::AgentInput {
            id: "abc123".into(),
            data: vec![0x68, 0x65, 0x6c, 0x6c, 0x6f],
        })
        .await;
    }

    #[tokio::test]
    async fn cli_resize_agent() {
        assert_cli_round_trip(CliMessage::ResizeAgent {
            id: "abc123".into(),
            cols: 120,
            rows: 40,
        })
        .await;
    }

    // ── Split-stream round-trips ─────────────────────────────

    #[tokio::test]
    async fn split_stream_round_trip() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_, mut a_write) = a.into_split();
        let (mut b_read, _) = b.into_split();

        let msg = PoolMessage::AgentOutput {
            id: "split".into(),
            data: vec![1, 2, 3],
        };
        send_message_write(&mut a_write, &msg).await.unwrap();
        let received: PoolMessage = recv_message_read(&mut b_read).await.unwrap();
        assert_eq!(msg, received);
    }

    #[tokio::test]
    async fn split_stream_multiple_messages() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_, mut a_write) = a.into_split();
        let (mut b_read, _) = b.into_split();

        let msgs = vec![
            CliMessage::AgentInput {
                id: "x".into(),
                data: vec![0x41],
            },
            CliMessage::ResizeAgent {
                id: "x".into(),
                cols: 80,
                rows: 24,
            },
            CliMessage::DetachAgent { id: "x".into() },
        ];

        for msg in &msgs {
            send_message_write(&mut a_write, msg).await.unwrap();
        }

        for expected in &msgs {
            let received: CliMessage = recv_message_read(&mut b_read).await.unwrap();
            assert_eq!(expected, &received);
        }
    }

    // ── Repo message round-trips ────────────────────────────────

    #[tokio::test]
    async fn cli_register_repo() {
        assert_cli_round_trip(CliMessage::RegisterRepo {
            path: "/home/user/project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_list_repos() {
        assert_cli_round_trip(CliMessage::ListRepos).await;
    }

    #[tokio::test]
    async fn pool_repo_registered() {
        assert_pool_round_trip(PoolMessage::RepoRegistered {
            path: "/home/user/project".into(),
            name: "project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn pool_repo_list_populated() {
        assert_pool_round_trip(PoolMessage::RepoList {
            repos: vec![RepoInfo {
                path: "/home/user/project".into(),
                name: "project".into(),
                local_branches: vec![
                    BranchInfo {
                        name: "main".into(),
                        is_head: true,
                        active_agent_id: Some("abc123".into()),
                        is_worktree: false,
                    },
                    BranchInfo {
                        name: "feature/foo".into(),
                        is_head: false,
                        active_agent_id: None,
                        is_worktree: true,
                    },
                ],
                remote_branches: vec![BranchInfo {
                    name: "origin/main".into(),
                    is_head: false,
                    active_agent_id: None,
                    is_worktree: false,
                }],
            }],
        })
        .await;
    }

    #[tokio::test]
    async fn pool_repo_list_empty() {
        assert_pool_round_trip(PoolMessage::RepoList { repos: vec![] }).await;
    }

    // ── Path helpers ───────────────────────────────────────────────

    #[test]
    fn clust_dir_ends_with_dot_clust() {
        let p = clust_dir();
        assert!(p.ends_with(".clust"));
    }

    #[test]
    fn socket_path_ends_with_clust_sock() {
        let p = socket_path();
        assert!(p.ends_with("clust.sock"));
    }

    #[test]
    fn socket_path_is_inside_clust_dir() {
        let dir = clust_dir();
        let sock = socket_path();
        assert_eq!(sock.parent().unwrap(), dir);
    }
}
