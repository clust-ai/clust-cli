# Architecture

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                        User Machine                         │
│                                                             │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                  │
│  │ Terminal  │  │ Terminal  │  │ Terminal  │   (any number)  │
│  │ clust-cli │  │ clust-cli │  │ clust-cli │                │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                  │
│       │              │              │                        │
│       └──────────────┼──────────────┘                        │
│                      │  IPC (Unix Domain Socket)             │
│                      │                                       │
│              ┌───────┴────────┐                              │
│              │   clust-hub    │  (single background daemon)  │
│              │                │                               │
│              │  ┌──────────┐  │                               │
│              │  │ Agent PTY │  │  ID: a3f8c1                  │
│              │  │ (claude)  │  │                               │
│              │  └──────────┘  │                               │
│              │                │                               │
│              │  ┌──────────┐  │                               │
│              │  │ Agent PTY │  │  ID: b7e2d9                  │
│              │  │ (claude)  │  │                               │
│              │  └──────────┘  │                               │
│              │                │                               │
│              └────────────────┘                               │
│                      │                                       │
│              ┌───────┴────────┐                              │
│              │  ~/.clust/     │                               │
│              │  clust.db      │  (SQLite — config/defaults)  │
│              │  clust.sock    │  (IPC socket)                │
│              └────────────────┘                               │
└─────────────────────────────────────────────────────────────┘
```

## Crate Responsibilities

### clust-cli

- Parse CLI arguments (via `clap`)
- Ensure `clust-hub` is running (auto-start if not)
- Send commands to hub over IPC
- Render agent output to the terminal (raw byte forwarding with output filter chain)
- Draw the bottom status bar (agent ID, agent binary, repo/branch context, shortcuts)
- Handle attach/detach lifecycle
- TUI dashboard (`clust ui`) with repo tree, agent cards, and multi-agent overview via `ratatui`
- Default agent picker with known agent detection
- Editor detection and "open in editor" integration (scanning PATH for known editors via `which`)
- Version update check (via Git)
- Worktree cleanup prompts when agents in worktrees are stopped or exit

The CLI is a thin client. It does NOT manage agent processes directly.

### clust-hub

- Run as a background daemon (no UI, no terminal)
- Manage agent lifecycles: spawn, track, clean up on exit
- Allocate PTYs for each agent (via `portable-pty`)
- Multiplex PTY output to all attached CLI clients
- Route input from any attached CLI client to the agent PTY
- Accept IPC commands (start agent, attach, list, stop, etc.)
- Generate agent IDs (6-char hex hash)
- Auto-start when first `clust` command is run
- Shut down on `clust -s` / `clust --stop` (graceful SIGTERM + SIGKILL)
- Manage SQLite database (config, repo registrations)
- Git repository/branch/worktree detection (via `git2`)
- macOS tray icon (via `tao` + `tray-icon`, hidden from dock)

### clust-ipc

- Define IPC message types (`CliMessage` and `HubMessage` enums)
- Length-prefixed MessagePack framing (`send_message` / `recv_message`)
- Protocol version constant (`PROTOCOL_VERSION`) for detecting stale hubs after rebuilds
- Split-stream variants for bidirectional sessions
- Socket path and clust directory helpers
- Known agent registry (`KNOWN_AGENTS`) with accept-edits, bypass-permissions, plan-mode, allow-bypass, and stop-hook metadata (`supports_stop_hook` indicates whether the agent honors the per-task "Exit when done" flag, currently `claude` only)
- Branch name sanitization (`sanitize_branch_name`) for converting user input into valid git branch names. NFC-normalises input, strips control characters, rejects ref-style prefixes (`refs/heads/`, `refs/remotes/`), reflog syntax (`@{`), and leading dots so the result is always a valid git branch name.
- Maximum frame guard (`MAX_MESSAGE_BYTES = 64 MiB`) and `validate_client_version` helper

## IPC Design

### Protocol: Unix Domain Socket

- **Socket path**: `~/.clust/clust.sock`
- **Why**: Fast, secure (filesystem permissions), no network exposure
- **Implementation**: Uses `tokio::net::UnixStream` directly. Cross-platform abstraction (e.g., named pipes for Windows) deferred to later.

### Message Format

Messages between CLI and Hub use a length-prefixed binary format:

```
[4 bytes: message length (u32 big-endian)] [N bytes: MessagePack payload]
```

Serialization uses **MessagePack** via `rmp-serde` (compact, fast, schema-friendly).

The receive path enforces an upper bound (`MAX_MESSAGE_BYTES`, currently 64 MiB) on the length prefix before allocating a buffer. This guards against a peer sending an arbitrarily large length and forcing the receiver to allocate gigabytes before failing the read. Legitimate traffic is well under 1 MiB per frame (largest frames are terminal output bursts).

### Message Types

```
CLI -> Hub:
  StartAgent { prompt: Option<String>, agent_binary: Option<String>, working_dir: String, cols: u16, rows: u16, accept_edits: bool, plan_mode: bool, allow_bypass: bool, hub: String }
  AttachAgent { id: String }
  DetachAgent { id: String }
  AgentInput { id: String, data: Vec<u8> }
  ResizeAgent { id: String, cols: u16, rows: u16 }
  ListAgents { hub: Option<String> }
  StopHub
  StopAgent { id: String }
  SetDefault { agent_binary: String }
  GetDefault
  RegisterRepo { path: String }
  UnregisterRepo { path: String }
  DeleteRepo { path: String }
  StopRepoAgents { path: String }
  SetRepoColor { path: String, color: String }
  SetRepoEditor { path: String, editor: String }
  SetDefaultEditor { editor: String }
  ListRepos
  ListWorktrees { working_dir: Option<String>, repo_name: Option<String> }
  AddWorktree { working_dir: Option<String>, repo_name: Option<String>, branch_name: String, base_branch: Option<String>, checkout_existing: bool }
  RemoveWorktree { working_dir: Option<String>, repo_name: Option<String>, branch_name: String, delete_local_branch: bool, force: bool }
  GetWorktreeInfo { working_dir: Option<String>, repo_name: Option<String>, branch_name: String }
  CreateWorktreeAgent { repo_path: String, target_branch: Option<String>, new_branch: Option<String>, prompt: Option<String>, agent_binary: Option<String>, cols: u16, rows: u16, accept_edits: bool, plan_mode: bool, allow_bypass: bool, hub: String }
  DeleteLocalBranch { working_dir: Option<String>, repo_name: Option<String>, branch_name: String, force: bool }
  DeleteRemoteBranch { working_dir: Option<String>, repo_name: Option<String>, branch_name: String }
  CheckoutRemoteBranch { working_dir: Option<String>, repo_name: Option<String>, remote_branch: String }
  PurgeRepo { path: String }
  CleanStaleRefs { working_dir: Option<String>, repo_name: Option<String> }
  PullBranch { repo_path: String, branch_name: String }
  CreateRepo { parent_dir: String, name: String }
  CloneRepo { url: String, parent_dir: String, name: Option<String> }
  SetBypassPermissions { enabled: bool }
  GetBypassPermissions
  DetachHead { repo_path: String }
  CheckoutLocalBranch { repo_path: String, branch_name: String }
  StartTerminal { working_dir: String, cols: u16, rows: u16, agent_id: Option<String> }
  AttachTerminal { id: String }
  DetachTerminal { id: String }
  TerminalInput { id: String, data: Vec<u8> }
  ResizeTerminal { id: String, cols: u16, rows: u16 }
  StopTerminal { id: String }
  Ping { protocol_version: u32 }

Hub -> CLI:
  Ok
  AgentStarted { id: String, agent_binary: String, is_worktree: bool, repo_path: Option<String>, branch_name: Option<String> }
  AgentAttached { id: String, agent_binary: String, is_worktree: bool, repo_path: Option<String>, branch_name: Option<String> }
  AgentOutput { id: String, data: Vec<u8> }
  AgentExited { id: String, exit_code: i32 }
  AgentList { agents: Vec<AgentInfo> }
  AgentReplayComplete { id: String }  // Marks end of replay buffer data on attach
  AgentStopped { id: String }  // Sent when stop is initiated (agent may still be terminating)
  DefaultAgent { agent_binary: Option<String> }
  HubShutdown
  Error { message: String }
  RepoRegistered { path: String, name: String }
  RepoUnregistered { path: String, name: String, stopped_agents: usize }
  RepoDeleted { path: String, name: String, stopped_agents: usize }
  RepoAgentsStopped { path: String, stopped_count: usize }
  RepoColorSet { path: String, color: String }
  RepoEditorSet { path: String, editor: String }
  DefaultEditorSet
  RepoList { repos: Vec<RepoInfo> }
  WorktreeList { repo_name: String, repo_path: String, worktrees: Vec<WorktreeEntry> }
  WorktreeAdded { branch_name: String, path: String }
  WorktreeRemoved { branch_name: String, stopped_agents: usize }
  WorktreeInfoResult { info: WorktreeEntry }
  WorktreeAgentStarted { id: String, agent_binary: String, working_dir: String, repo_path: Option<String>, branch_name: Option<String> }
  LocalBranchDeleted { branch_name: String, stopped_agents: usize }
  RemoteBranchDeleted { branch_name: String }
  RemoteBranchCheckedOut { branch_name: String }
  HeadDetached
  LocalBranchCheckedOut { branch_name: String }
  RepoPurged { path: String, stopped_agents: usize, removed_worktrees: usize, deleted_branches: usize }
  PurgeProgress { step: String }
  StaleRefsCleaned { path: String }
  BranchPulled { branch_name: String, summary: String }
  RepoCreated { path: String, name: String }
  RepoCloned { path: String, name: String }
  CloneProgress { step: String }
  BypassPermissions { enabled: bool }
  TerminalStarted { id: String }
  TerminalAttached { id: String }
  TerminalOutput { id: String, data: Vec<u8> }
  TerminalExited { id: String, exit_code: i32 }
  TerminalReplayComplete { id: String }
  TerminalStopped { id: String }
  Pong { protocol_version: u32 }
```

### Protocol Versioning

The IPC protocol includes a version check to detect stale hubs. `clust-ipc` exports a `PROTOCOL_VERSION` constant (currently `8`) that must be bumped whenever the `CliMessage` or `HubMessage` enum shapes change (since `rmp-serde` uses numeric enum indices).

On connection, the CLI sends a `Ping { protocol_version }` message. The hub replies with `Pong { protocol_version }` carrying its own version. If versions mismatch, the CLI stops the stale hub and spawns a fresh one before proceeding.

The crate also exports a `validate_client_version(client: u32)` helper that returns a static error string on mismatch. Hubs MAY use it to reject incompatible clients explicitly; the canonical enforcement path remains the CLI-side `Ping`/`Pong` check, which gracefully bounces an outdated hub before the user's command runs.

### Connection Lifecycle

1. CLI opens connection to `~/.clust/clust.sock`
2. If connection fails → CLI spawns `clust-hub` as a background process, retries
3. If connection succeeds → CLI sends `Ping` to verify protocol compatibility
4. If protocol mismatch → CLI sends `StopHub`, waits for socket release, spawns a new hub
5. CLI sends command message
6. For attach: connection stays open, bidirectional streaming (output down, input up)
7. For one-shot commands (ls, stop): hub responds, connection closes

## Agent Lifecycle

```
clust "do something"
        │
        ▼
  CLI connects to Hub
        │
        ▼
  Hub spawns agent PTY
  (e.g., `claude "do something"`)
        │
        ▼
  Hub assigns ID (e.g., a3f8c1)
  Hub sends AgentStarted { id }
        │
        ▼
  CLI enters attached mode:
    - Hub streams PTY output → CLI renders in terminal
    - CLI forwards keyboard input → Hub → agent PTY
    - CLI draws bottom status bar
        │
        ▼
  User detaches (Ctrl+Q)
    - CLI disconnects, agent keeps running in hub
        │
        ▼
  Agent process exits
    - Hub removes agent from hub
    - Hub notifies any attached CLIs → they exit gracefully
```

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing |
| `tokio` | Async runtime (hub daemon, IPC) |
| `portable-pty` | Cross-platform PTY allocation |
| `rusqlite` | SQLite access (with bundled feature) |
| `rmp-serde` | MessagePack serialization (IPC framing) |
| `ratatui` | Terminal UI rendering (TUI dashboard) |
| `crossterm` | Terminal manipulation (raw mode, input) |
| `tao` | Native event loop (macOS tray icon support) |
| `tray-icon` | System tray icon and menu |
| `git2` | Git repository/branch/worktree detection |
| `vt100` | Terminal emulator (overview panels, focus mode, attached scrollback) |
| `fuzzy-matcher` | Fuzzy string matching (create-agent and search-agent modal filtering) |
| `which` | Locate agent binaries on PATH (default agent discovery) |
| `syntect` | Syntax highlighting for diff viewer (TextMate grammar based) |
