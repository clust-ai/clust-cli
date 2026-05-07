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
///
/// Both the CLI and the hub MUST verify the peer's reported version on the
/// `Ping`/`Pong` round-trip and refuse to interoperate on mismatch. Hub-side
/// enforcement is informational today (it logs the mismatch but still answers)
/// because the CLI bounces an outdated hub via `connect_to_hub`. See
/// `validate_client_version` for a helper hubs may use to reject mismatched
/// clients explicitly.
pub const PROTOCOL_VERSION: u32 = 9;

/// Maximum size of a single IPC message payload.
///
/// The wire format is `[u32 BE length][payload]`. Without an upper bound a
/// peer could send an arbitrarily large length prefix and force the receiver
/// to allocate gigabytes before the read fails. Cap at 64 MiB which is
/// comfortably above legitimate traffic (largest legitimate frames are
/// terminal output bursts, well under 1 MiB).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Verify that a client's reported protocol version matches the hub's.
///
/// Returns `Ok(())` on match and a static error string on mismatch suitable
/// for logging or relaying back to the client. Hubs MAY use this to reject
/// connections from incompatible clients; the canonical enforcement path is
/// the `Ping`/`Pong` round-trip on the CLI side, which already aborts when
/// the hub's reported version differs from the CLI's compiled-in
/// `PROTOCOL_VERSION`.
pub fn validate_client_version(client: u32) -> Result<(), &'static str> {
    if client == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err("protocol version mismatch")
    }
}

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
    AttachAgent {
        id: String,
    },
    DetachAgent {
        id: String,
    },
    AgentInput {
        id: String,
        data: Vec<u8>,
    },
    ResizeAgent {
        id: String,
        cols: u16,
        rows: u16,
    },
    ListAgents {
        hub: Option<String>,
    },
    StopHub,
    StopAgent {
        id: String,
    },
    SetDefault {
        agent_binary: String,
    },
    GetDefault,
    RegisterRepo {
        path: String,
    },
    UnregisterRepo {
        path: String,
    },
    DeleteRepo {
        path: String,
    },
    StopRepoAgents {
        path: String,
    },
    SetRepoColor {
        path: String,
        color: String,
    },
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
        /// When true and the resolved agent supports a Stop hook, the spawned
        /// agent terminates itself at its first natural stopping point. Mirrors
        /// the per-task `auto_exit` flag on scheduled tasks so a manually
        /// spawned agent can also act as a dependency in a scheduled chain.
        #[serde(default)]
        auto_exit: bool,
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
    DetachHead {
        repo_path: String,
    },
    CheckoutLocalBranch {
        repo_path: String,
        branch_name: String,
    },
    // Terminal session management
    StartTerminal {
        working_dir: String,
        cols: u16,
        rows: u16,
        agent_id: Option<String>,
    },
    AttachTerminal {
        id: String,
    },
    DetachTerminal {
        id: String,
    },
    TerminalInput {
        id: String,
        data: Vec<u8>,
    },
    ResizeTerminal {
        id: String,
        cols: u16,
        rows: u16,
    },
    StopTerminal {
        id: String,
    },
    // Scheduled task management
    CreateScheduledTask {
        repo_path: String,
        base_branch: Option<String>,
        new_branch: Option<String>,
        prompt: String,
        plan_mode: bool,
        auto_exit: bool,
        agent_binary: Option<String>,
        schedule: ScheduleKind,
        /// Running worktree-agent IDs the new task should also depend on.
        /// The hub promotes each one to a shadow `scheduled_tasks` row
        /// (status=`active`, `agent_id` set) before persisting the new task,
        /// so the existing dep edges still reference task IDs.
        #[serde(default)]
        extra_agent_deps: Vec<String>,
    },
    ListScheduledTasks,
    UpdateScheduledTaskPrompt {
        id: String,
        prompt: String,
    },
    SetScheduledTaskPlanMode {
        id: String,
        plan_mode: bool,
    },
    SetScheduledTaskAutoExit {
        id: String,
        auto_exit: bool,
    },
    DeleteScheduledTask {
        id: String,
    },
    DeleteScheduledTasksByStatus {
        status: ScheduledTaskStatus,
    },
    StartScheduledTaskNow {
        id: String,
    },
    RestartScheduledTask {
        id: String,
        clean: bool,
    },
    /// Replace an existing task's schedule kind (and start_at / dep edges) in
    /// place. Used by the Schedule tab's reschedule modal: the repo, branch,
    /// and prompt stay put, only the trigger changes. An aborted task is
    /// flipped back to `inactive` so the new schedule can take effect.
    RescheduleScheduledTask {
        id: String,
        schedule: ScheduleKind,
        /// Same shadow-promotion semantics as `CreateScheduledTask`: any
        /// running Opt+E worktree agents picked in the dep step get a shadow
        /// `scheduled_tasks` row before the dep edges are written.
        #[serde(default)]
        extra_agent_deps: Vec<String>,
    },
}

/// What triggers a scheduled task to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleKind {
    /// Auto-start at the given absolute time (RFC 3339 UTC).
    Time { start_at: String },
    /// Auto-start once every listed task is `Complete`. `Aborted` blocks.
    Depend { depends_on_ids: Vec<String> },
    /// Never auto-start; user must trigger manually.
    Unscheduled,
}

/// Lifecycle state of a scheduled task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduledTaskStatus {
    Inactive,
    Active,
    Complete,
    Aborted,
}

impl ScheduledTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Aborted => "aborted",
        }
    }

    /// Parse the lowercase storage form back into the enum. Named with the
    /// `parse_` prefix to avoid colliding with `std::str::FromStr::from_str`.
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "inactive" => Some(Self::Inactive),
            "active" => Some(Self::Active),
            "complete" => Some(Self::Complete),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// Wire info for a single scheduled task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledTaskInfo {
    pub id: String,
    pub repo_path: String,
    pub repo_name: String,
    /// Resolved branch the task's worktree lives on. For "create new" tasks
    /// this is the sanitized form of `new_branch`; for "use existing" tasks
    /// it equals `base_branch`.
    pub branch_name: String,
    /// Original base ref the user picked (e.g. `main`, `origin/foo`). Needed
    /// at fire time so a "create new" task can re-run `git worktree add -b
    /// <new_branch> <wt> <base_branch>`. `None` for shadow tasks promoted
    /// from a running agent.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Original new-branch name the user requested, if any. `None` when the
    /// task reuses an existing branch.
    #[serde(default)]
    pub new_branch: Option<String>,
    pub prompt: String,
    pub plan_mode: bool,
    pub auto_exit: bool,
    pub agent_binary: String,
    pub schedule: ScheduleKind,
    pub status: ScheduledTaskStatus,
    pub agent_id: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
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
    /// Spawn-time flags surfaced so the schedule modal's dep picker can render
    /// status pills and the hub can populate a shadow scheduled-task row when
    /// the agent is selected as a dependency.
    #[serde(default)]
    pub auto_exit: bool,
    #[serde(default)]
    pub plan_mode: bool,
    #[serde(default)]
    pub prompt: Option<String>,
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
    #[serde(default)]
    pub is_remote: bool,
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
    AgentOutput {
        id: String,
        data: Vec<u8>,
    },
    AgentExited {
        id: String,
        exit_code: i32,
    },
    AgentList {
        agents: Vec<AgentInfo>,
    },
    DefaultAgent {
        agent_binary: Option<String>,
    },
    AgentStopped {
        id: String,
    },
    HubShutdown,
    Error {
        message: String,
    },
    RepoRegistered {
        path: String,
        name: String,
    },
    RepoUnregistered {
        path: String,
        name: String,
        stopped_agents: usize,
    },
    RepoDeleted {
        path: String,
        name: String,
        stopped_agents: usize,
    },
    RepoAgentsStopped {
        path: String,
        stopped_count: usize,
    },
    RepoColorSet {
        path: String,
        color: String,
    },
    RepoEditorSet {
        path: String,
        editor: String,
    },
    DefaultEditorSet,
    RepoList {
        repos: Vec<RepoInfo>,
    },
    AgentReplayComplete {
        id: String,
    },
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
    HeadDetached,
    LocalBranchCheckedOut {
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
    TerminalStarted {
        id: String,
    },
    TerminalAttached {
        id: String,
    },
    TerminalOutput {
        id: String,
        data: Vec<u8>,
    },
    TerminalExited {
        id: String,
        exit_code: i32,
    },
    TerminalReplayComplete {
        id: String,
    },
    TerminalStopped {
        id: String,
    },
    // Scheduled task notifications
    ScheduledTaskCreated {
        info: ScheduledTaskInfo,
    },
    ScheduledTaskList {
        tasks: Vec<ScheduledTaskInfo>,
    },
    ScheduledTaskUpdated {
        info: ScheduledTaskInfo,
    },
    ScheduledTaskDeleted {
        id: String,
    },
    ScheduledTasksCleared {
        count: usize,
    },
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
    let payload =
        rmp_serde::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive a length-prefixed MessagePack message from a Unix stream.
///
/// Rejects messages whose declared length exceeds [`MAX_MESSAGE_BYTES`] so a
/// hostile or buggy peer cannot force an unbounded allocation.
pub async fn recv_message<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len} exceeds maximum of {MAX_MESSAGE_BYTES} bytes"),
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    rmp_serde::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
///
/// Rejects messages whose declared length exceeds [`MAX_MESSAGE_BYTES`] so a
/// hostile or buggy peer cannot force an unbounded allocation.
pub async fn recv_message_read<T: DeserializeOwned>(reader: &mut OwnedReadHalf) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message length {len} exceeds maximum of {MAX_MESSAGE_BYTES} bytes"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    rmp_serde::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
                    auto_exit: true,
                    plan_mode: false,
                    prompt: Some("hello".into()),
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
                    auto_exit: false,
                    plan_mode: true,
                    prompt: None,
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
        assert_hub_round_trip(HubMessage::DefaultAgent { agent_binary: None }).await;
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

    #[tokio::test]
    async fn rejects_oversized_length_prefix() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        // Declare a length one byte beyond the cap. We do not need to send a
        // payload — the receiver should reject the prefix before allocating.
        let oversized = (MAX_MESSAGE_BYTES as u32).wrapping_add(1);
        a.write_all(&oversized.to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        let result: io::Result<CliMessage> = recv_message(&mut b).await;
        let err = result.expect_err("expected length cap rejection");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[tokio::test]
    async fn rejects_max_u32_length_prefix() {
        let (mut a, mut b) = UnixStream::pair().unwrap();

        // u32::MAX would otherwise allocate 4 GiB.
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        a.flush().await.unwrap();
        drop(a);

        let result: io::Result<CliMessage> = recv_message(&mut b).await;
        let err = result.expect_err("expected length cap rejection");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn split_stream_rejects_oversized_length_prefix() {
        let (a, b) = UnixStream::pair().unwrap();
        let (_, mut a_write) = a.into_split();
        let (mut b_read, _) = b.into_split();

        let oversized = (MAX_MESSAGE_BYTES as u32).wrapping_add(1);
        a_write.write_all(&oversized.to_be_bytes()).await.unwrap();
        a_write.flush().await.unwrap();
        drop(a_write);

        let result: io::Result<CliMessage> = recv_message_read(&mut b_read).await;
        let err = result.expect_err("expected length cap rejection on split read half");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn validate_client_version_matches() {
        assert!(validate_client_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn validate_client_version_rejects_mismatch() {
        let bad = PROTOCOL_VERSION.wrapping_add(1);
        assert!(validate_client_version(bad).is_err());
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
                        is_remote: false,
                    },
                    BranchInfo {
                        name: "feature/foo".into(),
                        is_head: false,
                        active_agent_count: 0,
                        is_worktree: true,
                        is_remote: false,
                    },
                ],
                remote_branches: vec![BranchInfo {
                    name: "origin/main".into(),
                    is_head: false,
                    active_agent_count: 0,
                    is_worktree: false,
                    is_remote: true,
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
                        auto_exit: false,
                        plan_mode: false,
                        prompt: None,
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
            auto_exit: true,
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
            auto_exit: false,
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

    // ── Scheduled task round-trips ────────────────────────────────

    fn sample_task() -> ScheduledTaskInfo {
        ScheduledTaskInfo {
            id: "abcd1234".into(),
            repo_path: "/home/user/project".into(),
            repo_name: "project".into(),
            branch_name: "feature/x".into(),
            base_branch: Some("main".into()),
            new_branch: Some("feature/x".into()),
            prompt: "do the thing".into(),
            plan_mode: false,
            auto_exit: true,
            agent_binary: "claude".into(),
            schedule: ScheduleKind::Time {
                start_at: "2026-05-06T10:00:00Z".into(),
            },
            status: ScheduledTaskStatus::Inactive,
            agent_id: None,
            created_at: "2026-05-06T09:00:00Z".into(),
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn cli_create_scheduled_task_time() {
        assert_cli_round_trip(CliMessage::CreateScheduledTask {
            repo_path: "/repo".into(),
            base_branch: Some("main".into()),
            new_branch: Some("feature/x".into()),
            prompt: "do something".into(),
            plan_mode: false,
            auto_exit: true,
            agent_binary: Some("claude".into()),
            schedule: ScheduleKind::Time {
                start_at: "2026-05-06T10:00:00Z".into(),
            },
            extra_agent_deps: Vec::new(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_create_scheduled_task_depend() {
        assert_cli_round_trip(CliMessage::CreateScheduledTask {
            repo_path: "/repo".into(),
            base_branch: None,
            new_branch: Some("after-foo".into()),
            prompt: "after foo".into(),
            plan_mode: true,
            auto_exit: false,
            agent_binary: None,
            schedule: ScheduleKind::Depend {
                depends_on_ids: vec!["aaa111".into(), "bbb222".into()],
            },
            extra_agent_deps: vec!["ag0001".into()],
        })
        .await;
    }

    #[tokio::test]
    async fn cli_create_scheduled_task_unscheduled() {
        assert_cli_round_trip(CliMessage::CreateScheduledTask {
            repo_path: "/repo".into(),
            base_branch: Some("main".into()),
            new_branch: None,
            prompt: "manual".into(),
            plan_mode: false,
            auto_exit: false,
            agent_binary: None,
            schedule: ScheduleKind::Unscheduled,
            extra_agent_deps: Vec::new(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_list_scheduled_tasks() {
        assert_cli_round_trip(CliMessage::ListScheduledTasks).await;
    }

    #[tokio::test]
    async fn cli_update_scheduled_task_prompt() {
        assert_cli_round_trip(CliMessage::UpdateScheduledTaskPrompt {
            id: "abcd1234".into(),
            prompt: "new prompt".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_set_scheduled_task_plan_mode() {
        assert_cli_round_trip(CliMessage::SetScheduledTaskPlanMode {
            id: "abcd1234".into(),
            plan_mode: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_set_scheduled_task_auto_exit() {
        assert_cli_round_trip(CliMessage::SetScheduledTaskAutoExit {
            id: "abcd1234".into(),
            auto_exit: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_scheduled_task() {
        assert_cli_round_trip(CliMessage::DeleteScheduledTask {
            id: "abcd1234".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_scheduled_tasks_by_status_complete() {
        assert_cli_round_trip(CliMessage::DeleteScheduledTasksByStatus {
            status: ScheduledTaskStatus::Complete,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_delete_scheduled_tasks_by_status_aborted() {
        assert_cli_round_trip(CliMessage::DeleteScheduledTasksByStatus {
            status: ScheduledTaskStatus::Aborted,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_start_scheduled_task_now() {
        assert_cli_round_trip(CliMessage::StartScheduledTaskNow {
            id: "abcd1234".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_restart_scheduled_task_clean() {
        assert_cli_round_trip(CliMessage::RestartScheduledTask {
            id: "abcd1234".into(),
            clean: true,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_restart_scheduled_task_in_place() {
        assert_cli_round_trip(CliMessage::RestartScheduledTask {
            id: "abcd1234".into(),
            clean: false,
        })
        .await;
    }

    #[tokio::test]
    async fn cli_reschedule_scheduled_task_time() {
        assert_cli_round_trip(CliMessage::RescheduleScheduledTask {
            id: "abcd1234".into(),
            schedule: ScheduleKind::Time {
                start_at: "2026-05-06T10:00:00Z".into(),
            },
            extra_agent_deps: Vec::new(),
        })
        .await;
    }

    #[tokio::test]
    async fn cli_reschedule_scheduled_task_depend() {
        assert_cli_round_trip(CliMessage::RescheduleScheduledTask {
            id: "abcd1234".into(),
            schedule: ScheduleKind::Depend {
                depends_on_ids: vec!["aaa111".into()],
            },
            extra_agent_deps: vec!["ag0001".into()],
        })
        .await;
    }

    #[tokio::test]
    async fn cli_reschedule_scheduled_task_unscheduled() {
        assert_cli_round_trip(CliMessage::RescheduleScheduledTask {
            id: "abcd1234".into(),
            schedule: ScheduleKind::Unscheduled,
            extra_agent_deps: Vec::new(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_scheduled_task_created() {
        assert_hub_round_trip(HubMessage::ScheduledTaskCreated { info: sample_task() }).await;
    }

    #[tokio::test]
    async fn hub_scheduled_task_list_populated() {
        let mut t2 = sample_task();
        t2.id = "efef5678".into();
        t2.schedule = ScheduleKind::Unscheduled;
        t2.status = ScheduledTaskStatus::Active;
        t2.agent_id = Some("aabbcc".into());
        assert_hub_round_trip(HubMessage::ScheduledTaskList {
            tasks: vec![sample_task(), t2],
        })
        .await;
    }

    #[tokio::test]
    async fn hub_scheduled_task_list_empty() {
        assert_hub_round_trip(HubMessage::ScheduledTaskList { tasks: vec![] }).await;
    }

    #[tokio::test]
    async fn hub_scheduled_task_updated_complete() {
        let mut t = sample_task();
        t.status = ScheduledTaskStatus::Complete;
        t.completed_at = Some("2026-05-06T10:30:00Z".into());
        t.agent_id = Some("aabbcc".into());
        assert_hub_round_trip(HubMessage::ScheduledTaskUpdated { info: t }).await;
    }

    #[tokio::test]
    async fn hub_scheduled_task_deleted() {
        assert_hub_round_trip(HubMessage::ScheduledTaskDeleted {
            id: "abcd1234".into(),
        })
        .await;
    }

    #[tokio::test]
    async fn hub_scheduled_tasks_cleared() {
        assert_hub_round_trip(HubMessage::ScheduledTasksCleared { count: 3 }).await;
    }

    #[test]
    fn status_str_round_trip() {
        for s in [
            ScheduledTaskStatus::Inactive,
            ScheduledTaskStatus::Active,
            ScheduledTaskStatus::Complete,
            ScheduledTaskStatus::Aborted,
        ] {
            assert_eq!(ScheduledTaskStatus::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(ScheduledTaskStatus::parse_str("garbage"), None);
    }
}
