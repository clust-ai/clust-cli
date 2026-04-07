pub mod agents;
pub mod branch;

use std::io;
use std::path::PathBuf;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// Default hub name for agents not assigned to a specific hub.
pub const DEFAULT_HUB: &str = "default_hub";

/// Protocol version for IPC compatibility checks.
/// Bump this whenever `CliMessage` or `HubMessage` enum shapes change.
pub const PROTOCOL_VERSION: u32 = 4;

/// Messages sent from CLI to Hub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CliMessage {
    StartAgent {
        prompt: Option<String>,
        agent_binary: Option<String>,
        working_dir: String,
        cols: u16,
        rows: u16,
        accept_edits: bool,
        plan_mode: bool,
        allow_bypass: bool,
        hub: String,
    },
    AttachAgent { id: String },
    DetachAgent { id: String },
    AgentInput { id: String, data: Vec<u8> },
    ResizeAgent { id: String, cols: u16, rows: u16 },
    ListAgents { hub: Option<String> },
    StopHub,
    StopAgent { id: String },
    SetDefault { agent_binary: String },
    GetDefault,
    RegisterRepo { path: String },
    UnregisterRepo { path: String },
    StopRepoAgents { path: String },
    SetRepoColor { path: String, color: String },
    ListRepos,
    ListWorktrees {
        working_dir: Option<String>,
        repo_name: Option<String>,
    },
    AddWorktree {
        working_dir: Option<String>,
        repo_name: Option<String>,
        branch_name: String,
        base_branch: Option<String>,
        checkout_existing: bool,
    },
    RemoveWorktree {
        working_dir: Option<String>,
        repo_name: Option<String>,
        branch_name: String,
        delete_local_branch: bool,
        force: bool,
    },
    GetWorktreeInfo {
        working_dir: Option<String>,
        repo_name: Option<String>,
        branch_name: String,
    },
    CreateWorktreeAgent {
        repo_path: String,
        target_branch: Option<String>,
        new_branch: Option<String>,
        prompt: Option<String>,
        agent_binary: Option<String>,
        cols: u16,
        rows: u16,
        accept_edits: bool,
        plan_mode: bool,
        allow_bypass: bool,
        hub: String,
    },
    DeleteLocalBranch {
        working_dir: Option<String>,
        repo_name: Option<String>,
        branch_name: String,
        force: bool,
    },
    DeleteRemoteBranch {
        working_dir: Option<String>,
        repo_name: Option<String>,
        branch_name: String,
    },
    PurgeRepo {
        path: String,
    },
    CleanStaleRefs {
        working_dir: Option<String>,
        repo_name: Option<String>,
    },
    PullBranch {
        repo_path: String,
        branch_name: String,
    },
    CheckoutRemoteBranch {
        working_dir: Option<String>,
        repo_name: Option<String>,
        remote_branch: String,
    },
    CreateRepo {
        parent_dir: String,
        name: String,
    },
    CloneRepo {
        url: String,
        parent_dir: String,
        name: Option<String>,
    },
    Ping {
        protocol_version: u32,
    },
    SetRepoEditor {
        path: String,
        editor: String,
    },
    SetDefaultEditor {
        editor: String,
    },
    SetBypassPermissions {
        enabled: bool,
    },
    GetBypassPermissions,
    // Terminal session management
    StartTerminal {
        working_dir: String,
        cols: u16,
        rows: u16,
    },
    AttachTerminal { id: String },
    DetachTerminal { id: String },
    TerminalInput { id: String, data: Vec<u8> },
    ResizeTerminal { id: String, cols: u16, rows: u16 },
    StopTerminal { id: String },
}

/// Info about a running agent, returned in AgentList.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: String,
    pub agent_binary: String,
    pub started_at: String,
    pub attached_clients: usize,
    pub hub: String,
    pub working_dir: String,
    pub repo_path: Option<String>,
    pub branch_name: Option<String>,
    pub is_worktree: bool,
}

/// Info about a registered repository, returned in RepoList.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: String,
    pub name: String,
    pub color: Option<String>,
    pub editor: Option<String>,
    pub local_branches: Vec<BranchInfo>,
    pub remote_branches: Vec<BranchInfo>,
}

/// Info about a single branch within a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_head: bool,
    pub active_agent_count: usize,
    pub is_worktree: bool,
}

/// Info about a single worktree in a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub branch_name: String,
    pub path: String,
    pub is_main: bool,
    pub is_dirty: bool,
    pub active_agents: Vec<AgentInfo>,
}

/// Messages sent from Hub to CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HubMessage {
    Ok,
    AgentStarted {
        id: String,
        agent_binary: String,
        is_worktree: bool,
        repo_path: Option<String>,
        branch_name: Option<String>,
    },
    AgentAttached {
        id: String,
        agent_binary: String,
        is_worktree: bool,
        repo_path: Option<String>,
        branch_name: Option<String>,
    },
    AgentOutput { id: String, data: Vec<u8> },
    AgentExited { id: String, exit_code: i32 },
    AgentList { agents: Vec<AgentInfo> },
    DefaultAgent { agent_binary: Option<String> },
    AgentStopped { id: String },
    HubShutdown,
    Error { message: String },
    RepoRegistered { path: String, name: String },
    RepoUnregistered { path: String, name: String, stopped_agents: usize },
    RepoAgentsStopped { path: String, stopped_count: usize },
    RepoColorSet { path: String, color: String },
    RepoEditorSet { path: String, editor: String },
    DefaultEditorSet,
    RepoList { repos: Vec<RepoInfo> },
    AgentReplayComplete { id: String },
    WorktreeList {
        repo_name: String,
        repo_path: String,
        worktrees: Vec<WorktreeEntry>,
    },
    WorktreeAdded {
        branch_name: String,
        path: String,
    },
    WorktreeRemoved {
        branch_name: String,
        stopped_agents: usize,
    },
    WorktreeInfoResult {
        info: WorktreeEntry,
    },
    WorktreeAgentStarted {
        id: String,
        agent_binary: String,
        working_dir: String,
        repo_path: Option<String>,
        branch_name: Option<String>,
    },
    LocalBranchDeleted {
        branch_name: String,
        stopped_agents: usize,
    },
    RemoteBranchDeleted {
        branch_name: String,
    },
    RepoPurged {
        path: String,
        stopped_agents: usize,
        removed_worktrees: usize,
        deleted_branches: usize,
    },
    PurgeProgress {
        step: String,
    },
    StaleRefsCleaned {
        path: String,
    },
    BranchPulled {
        branch_name: String,
        summary: String,
    },
    RemoteBranchCheckedOut {
        branch_name: String,
    },
    RepoCreated {
        path: String,
        name: String,
    },
    RepoCloned {
        path: String,
        name: String,
    },
    CloneProgress {
        step: String,
    },
    Pong {
        protocol_version: u32,
    },
    BypassPermissions {
        enabled: bool,
    },
    // Terminal session messages
    TerminalStarted { id: String },
    TerminalAttached { id: String },
    TerminalOutput { id: String, data: Vec<u8> },
    TerminalExited { id: String, exit_code: i32 },
    TerminalReplayComplete { id: String },
    TerminalStopped { id: String },
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

/// Returns the hub log path: `~/.clust/hub.log`.
pub fn log_path() -> PathBuf {
    clust_dir().join("hub.log")
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

    async fn assert_hub_round_trip(msg: HubMessage) {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        send_message(&mut a, &msg).await.unwrap();
        let received: HubMessage = recv_message(&mut b).await.unwrap();
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
            plan_mode: false,
            allow_bypass: false,
            hub: DEFAULT_HUB.into(),
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
            plan_mode: false,
            allow_bypass: false,
            hub: DEFAULT_HUB.into(),
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
            plan_mode: false,
            allow_bypass: false,
            hub: DEFAULT_HUB.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_start_agent_custom_hub() {
        assert_cli_round_trip(CliMessage::StartAgent {
            prompt: None,
            agent_binary: Some("claude".into()),
            working_dir: "/tmp".into(),
            cols: 80,
            rows: 24,
            accept_edits: false,
            plan_mode: false,
            allow_bypass: false,
            hub: "my_feature".into(),
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
        assert_cli_round_trip(CliMessage::ListAgents { hub: None }).await;
    }

    #[tokio::test]
    async fn cli_list_agents_with_hub_filter() {
        assert_cli_round_trip(CliMessage::ListAgents {
            hub: Some("my_feature".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_stop_hub() {
        assert_cli_round_trip(CliMessage::StopHub).await;
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

    // ── HubMessage round-trips ─────────────────────────────────────

    #[tokio::test]
    async fn hub_ok() {
        assert_hub_round_trip(HubMessage::Ok).await;
    }

    #[tokio::test]
    async fn hub_agent_started() {
        assert_hub_round_trip(HubMessage::AgentStarted {
            id: "a1b2c3".into(),
            agent_binary: "claude".into(),
            is_worktree: false,
            repo_path: None,
            branch_name: None,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_attached() {
        assert_hub_round_trip(HubMessage::AgentAttached {
            id: "a1b2c3".into(),
            agent_binary: "claude".into(),
            is_worktree: false,
            repo_path: None,
            branch_name: None,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_output() {
        assert_hub_round_trip(HubMessage::AgentOutput {
            id: "a1b2c3".into(),
            data: vec![0x00, 0xFF, 0x42],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_exited() {
        assert_hub_round_trip(HubMessage::AgentExited {
            id: "a1b2c3".into(),
            exit_code: 42,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_list_populated() {
        assert_hub_round_trip(HubMessage::AgentList {
            agents: vec![
                AgentInfo {
                    id: "aaa111".into(),
                    agent_binary: "claude".into(),
                    started_at: "2026-03-25T10:00:00Z".into(),
                    attached_clients: 2,
                    hub: DEFAULT_HUB.into(),
                    working_dir: "/tmp/project".into(),
                    repo_path: Some("/tmp/project".into()),
                    branch_name: Some("main".into()),
                    is_worktree: false,
                },
                AgentInfo {
                    id: "bbb222".into(),
                    agent_binary: "aider".into(),
                    started_at: "2026-03-25T11:00:00Z".into(),
                    attached_clients: 0,
                    hub: "my_feature".into(),
                    working_dir: "/home/user/code".into(),
                    repo_path: None,
                    branch_name: None,
                    is_worktree: false,
                },
            ],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_list_empty() {
        assert_hub_round_trip(HubMessage::AgentList { agents: vec![] }).await;
    }

    #[tokio::test]
    async fn hub_default_agent_some() {
        assert_hub_round_trip(HubMessage::DefaultAgent {
            agent_binary: Some("claude".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_default_agent_none() {
        assert_hub_round_trip(HubMessage::DefaultAgent {
            agent_binary: None,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_agent_stopped() {
        assert_hub_round_trip(HubMessage::AgentStopped {
            id: "abc123".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_shutdown() {
        assert_hub_round_trip(HubMessage::HubShutdown).await;
    }

    #[tokio::test]
    async fn hub_error() {
        assert_hub_round_trip(HubMessage::Error {
            message: "something went wrong".into(),
        })
        .await;
    }

    // ── Framing / edge cases ───────────────────────────────────────

    #[tokio::test]
    async fn multiple_messages_on_one_stream() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        let msgs = vec![
            CliMessage::ListAgents { hub: None },
            CliMessage::StopHub,
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
        let msg = HubMessage::AgentOutput {
            id: "big".into(),
            data,
        };
        let expected = msg.clone();
        // Must read concurrently — socket buffer is smaller than 1MB
        let writer = tokio::spawn(async move { send_message(&mut a, &msg).await.unwrap() });
        let received: HubMessage = recv_message(&mut b).await.unwrap();
        writer.await.unwrap();
        assert_eq!(expected, received);
    }

    #[tokio::test]
    async fn empty_data_payload() {
        assert_hub_round_trip(HubMessage::AgentOutput {
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

        let msg = HubMessage::AgentOutput {
            id: "split".into(),
            data: vec![1, 2, 3],
        };
        send_message_write(&mut a_write, &msg).await.unwrap();
        let received: HubMessage = recv_message_read(&mut b_read).await.unwrap();
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
    async fn cli_unregister_repo() {
        assert_cli_round_trip(CliMessage::UnregisterRepo {
            path: "/home/user/project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_stop_repo_agents() {
        assert_cli_round_trip(CliMessage::StopRepoAgents {
            path: "/home/user/project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_list_repos() {
        assert_cli_round_trip(CliMessage::ListRepos).await;
    }

    #[tokio::test]
    async fn hub_repo_registered() {
        assert_hub_round_trip(HubMessage::RepoRegistered {
            path: "/home/user/project".into(),
            name: "project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_unregistered() {
        assert_hub_round_trip(HubMessage::RepoUnregistered {
            path: "/home/user/project".into(),
            name: "project".into(),
            stopped_agents: 2,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_unregistered_no_agents() {
        assert_hub_round_trip(HubMessage::RepoUnregistered {
            path: "/home/user/project".into(),
            name: "project".into(),
            stopped_agents: 0,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_agents_stopped() {
        assert_hub_round_trip(HubMessage::RepoAgentsStopped {
            path: "/home/user/project".into(),
            stopped_count: 3,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_agents_stopped_zero() {
        assert_hub_round_trip(HubMessage::RepoAgentsStopped {
            path: "/home/user/project".into(),
            stopped_count: 0,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_list_populated() {
        assert_hub_round_trip(HubMessage::RepoList {
            repos: vec![RepoInfo {
                path: "/home/user/project".into(),
                name: "project".into(),
                color: Some("blue".into()),
                editor: None,
                local_branches: vec![
                    BranchInfo {
                        name: "main".into(),
                        is_head: true,
                        active_agent_count: 1,
                        is_worktree: false,
                    },
                    BranchInfo {
                        name: "feature/foo".into(),
                        is_head: false,
                        active_agent_count: 0,
                        is_worktree: true,
                    },
                ],
                remote_branches: vec![BranchInfo {
                    name: "origin/main".into(),
                    is_head: false,
                    active_agent_count: 0,
                    is_worktree: false,
                }],
            }],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_list_empty() {
        assert_hub_round_trip(HubMessage::RepoList { repos: vec![] }).await;
    }

    #[tokio::test]
    async fn cli_set_repo_color() {
        assert_cli_round_trip(CliMessage::SetRepoColor {
            path: "/home/user/project".into(),
            color: "purple".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_color_set() {
        assert_hub_round_trip(HubMessage::RepoColorSet {
            path: "/home/user/project".into(),
            color: "teal".into(),
        })
        .await;
    }

    // ── Worktree message round-trips ─────────────────────────────

    #[tokio::test]
    async fn cli_list_worktrees_with_working_dir() {
        assert_cli_round_trip(CliMessage::ListWorktrees {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_list_worktrees_with_repo_name() {
        assert_cli_round_trip(CliMessage::ListWorktrees {
            working_dir: None,
            repo_name: Some("my-project".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_add_worktree_new_branch() {
        assert_cli_round_trip(CliMessage::AddWorktree {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            branch_name: "feature/auth".into(),
            base_branch: Some("main".into()),
            checkout_existing: false,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_add_worktree_checkout_existing() {
        assert_cli_round_trip(CliMessage::AddWorktree {
            working_dir: None,
            repo_name: Some("my-project".into()),
            branch_name: "existing-branch".into(),
            base_branch: None,
            checkout_existing: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_remove_worktree() {
        assert_cli_round_trip(CliMessage::RemoveWorktree {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            branch_name: "feature/auth".into(),
            delete_local_branch: true,
            force: false,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_remove_worktree_force() {
        assert_cli_round_trip(CliMessage::RemoveWorktree {
            working_dir: None,
            repo_name: Some("my-project".into()),
            branch_name: "dirty-branch".into(),
            delete_local_branch: false,
            force: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_local_branch() {
        assert_cli_round_trip(CliMessage::DeleteLocalBranch {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            branch_name: "feature/auth".into(),
            force: false,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_local_branch_force() {
        assert_cli_round_trip(CliMessage::DeleteLocalBranch {
            working_dir: None,
            repo_name: Some("my-project".into()),
            branch_name: "dirty-branch".into(),
            force: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_remote_branch() {
        assert_cli_round_trip(CliMessage::DeleteRemoteBranch {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            branch_name: "origin/feature/auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_checkout_remote_branch() {
        assert_cli_round_trip(CliMessage::CheckoutRemoteBranch {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            remote_branch: "origin/feature/auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_purge_repo() {
        assert_cli_round_trip(CliMessage::PurgeRepo {
            path: "/home/user/project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_clean_stale_refs() {
        assert_cli_round_trip(CliMessage::CleanStaleRefs {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_get_worktree_info() {
        assert_cli_round_trip(CliMessage::GetWorktreeInfo {
            working_dir: Some("/home/user/project".into()),
            repo_name: None,
            branch_name: "feature/auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_list_populated() {
        assert_hub_round_trip(HubMessage::WorktreeList {
            repo_name: "project".into(),
            repo_path: "/home/user/project".into(),
            worktrees: vec![
                WorktreeEntry {
                    branch_name: "main".into(),
                    path: "/home/user/project".into(),
                    is_main: true,
                    is_dirty: false,
                    active_agents: vec![],
                },
                WorktreeEntry {
                    branch_name: "feature/auth".into(),
                    path: "/home/user/project/.clust/worktrees/feature__auth".into(),
                    is_main: false,
                    is_dirty: true,
                    active_agents: vec![AgentInfo {
                        id: "abc123".into(),
                        agent_binary: "claude".into(),
                        started_at: "2026-03-25T10:00:00Z".into(),
                        attached_clients: 1,
                        hub: DEFAULT_HUB.into(),
                        working_dir: "/home/user/project/.clust/worktrees/feature__auth".into(),
                        repo_path: Some("/home/user/project".into()),
                        branch_name: Some("feature/auth".into()),
                        is_worktree: true,
                    }],
                },
            ],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_list_empty() {
        assert_hub_round_trip(HubMessage::WorktreeList {
            repo_name: "project".into(),
            repo_path: "/home/user/project".into(),
            worktrees: vec![],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_added() {
        assert_hub_round_trip(HubMessage::WorktreeAdded {
            branch_name: "feature/auth".into(),
            path: "/home/user/project/.clust/worktrees/feature__auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_removed() {
        assert_hub_round_trip(HubMessage::WorktreeRemoved {
            branch_name: "feature/auth".into(),
            stopped_agents: 2,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_removed_no_agents() {
        assert_hub_round_trip(HubMessage::WorktreeRemoved {
            branch_name: "clean-branch".into(),
            stopped_agents: 0,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_local_branch_deleted() {
        assert_hub_round_trip(HubMessage::LocalBranchDeleted {
            branch_name: "feature/auth".into(),
            stopped_agents: 2,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_remote_branch_deleted() {
        assert_hub_round_trip(HubMessage::RemoteBranchDeleted {
            branch_name: "origin/feature/auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_purged() {
        assert_hub_round_trip(HubMessage::RepoPurged {
            path: "/home/user/project".into(),
            stopped_agents: 3,
            removed_worktrees: 2,
            deleted_branches: 5,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_stale_refs_cleaned() {
        assert_hub_round_trip(HubMessage::StaleRefsCleaned {
            path: "/home/user/project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_info_result() {
        assert_hub_round_trip(HubMessage::WorktreeInfoResult {
            info: WorktreeEntry {
                branch_name: "feature/auth".into(),
                path: "/home/user/project/.clust/worktrees/feature__auth".into(),
                is_main: false,
                is_dirty: false,
                active_agents: vec![],
            },
        })
        .await;
    }

    // ── Create worktree agent round-trips ──────────────────────

    #[tokio::test]
    async fn cli_create_worktree_agent_all_fields() {
        assert_cli_round_trip(CliMessage::CreateWorktreeAgent {
            repo_path: "/home/user/project".into(),
            target_branch: Some("main".into()),
            new_branch: Some("feature/foo".into()),
            prompt: Some("fix the tests".into()),
            agent_binary: Some("claude".into()),
            cols: 120,
            rows: 40,
            accept_edits: true,
            plan_mode: false,
            allow_bypass: false,
            hub: DEFAULT_HUB.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_create_worktree_agent_minimal() {
        assert_cli_round_trip(CliMessage::CreateWorktreeAgent {
            repo_path: "/tmp/repo".into(),
            target_branch: None,
            new_branch: Some("first-branch".into()),
            prompt: None,
            agent_binary: None,
            cols: 80,
            rows: 24,
            accept_edits: false,
            plan_mode: false,
            allow_bypass: false,
            hub: DEFAULT_HUB.into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_worktree_agent_started() {
        assert_hub_round_trip(HubMessage::WorktreeAgentStarted {
            id: "abc123".into(),
            agent_binary: "claude".into(),
            working_dir: "/home/user/project/.clust/worktrees/feature__foo".into(),
            repo_path: Some("/home/user/project".into()),
            branch_name: Some("feature/foo".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_remote_branch_checked_out() {
        assert_hub_round_trip(HubMessage::RemoteBranchCheckedOut {
            branch_name: "feature/auth".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_create_repo() {
        assert_cli_round_trip(CliMessage::CreateRepo {
            parent_dir: "/home/user".into(),
            name: "new-project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_clone_repo() {
        assert_cli_round_trip(CliMessage::CloneRepo {
            url: "https://github.com/user/repo.git".into(),
            parent_dir: "/home/user".into(),
            name: None,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_clone_repo_with_name() {
        assert_cli_round_trip(CliMessage::CloneRepo {
            url: "git@github.com:user/repo.git".into(),
            parent_dir: "/tmp".into(),
            name: Some("custom-name".into()),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_ping() {
        assert_cli_round_trip(CliMessage::Ping {
            protocol_version: PROTOCOL_VERSION,
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_created() {
        assert_hub_round_trip(HubMessage::RepoCreated {
            path: "/home/user/new-project".into(),
            name: "new-project".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_repo_cloned() {
        assert_hub_round_trip(HubMessage::RepoCloned {
            path: "/home/user/repo".into(),
            name: "repo".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_clone_progress() {
        assert_hub_round_trip(HubMessage::CloneProgress {
            step: "Receiving objects: 45%".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_pong() {
        assert_hub_round_trip(HubMessage::Pong {
            protocol_version: PROTOCOL_VERSION,
        })
        .await;
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

    #[test]
    fn log_path_ends_with_hub_log() {
        let p = log_path();
        assert!(p.ends_with("hub.log"));
    }

    #[test]
    fn log_path_is_inside_clust_dir() {
        let dir = clust_dir();
        let log = log_path();
        assert_eq!(log.parent().unwrap(), dir);
    }
}
