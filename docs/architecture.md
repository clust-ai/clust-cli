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
- Draw the bottom status bar (agent ID, shortcuts)
- Handle attach/detach lifecycle
- TUI dashboard (`clust ui`) with repo tree, agent cards, and multi-agent overview via `ratatui`
- Default agent picker with known agent detection
- Version update check (via Git)

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
- Split-stream variants for bidirectional sessions
- Socket path and clust directory helpers
- Known agent registry (`KNOWN_AGENTS`) with accept-edits metadata

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

### Message Types

```
CLI -> Hub:
  StartAgent { prompt: Option<String>, agent_binary: Option<String>, working_dir: String, cols: u16, rows: u16, accept_edits: bool, hub: String }
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
  StopRepoAgents { path: String }
  ListRepos

Hub -> CLI:
  Ok
  AgentStarted { id: String, agent_binary: String }
  AgentAttached { id: String, agent_binary: String }
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
  RepoAgentsStopped { path: String, stopped_count: usize }
  RepoList { repos: Vec<RepoInfo> }
```

### Connection Lifecycle

1. CLI opens connection to `~/.clust/clust.sock`
2. If connection fails → CLI spawns `clust-hub` as a background process, retries
3. CLI sends command message
4. For attach: connection stays open, bidirectional streaming (output down, input up)
5. For one-shot commands (ls, stop): hub responds, connection closes

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
| `vte` | VTE terminal escape sequence parser (overview screen emulation) |
| `which` | Locate agent binaries on PATH (default agent discovery) |
