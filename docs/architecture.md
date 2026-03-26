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
│              │   clust-pool   │  (single background daemon)  │
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
- Ensure `clust-pool` is running (auto-start if not)
- Send commands to pool over IPC
- Render agent output to the terminal (raw byte forwarding with output filter chain)
- Draw the bottom status bar (agent ID, shortcuts)
- Handle attach/detach lifecycle
- TUI dashboard (`clust ui`) with repo tree and agent cards via `ratatui`
- Default agent picker with known agent detection
- Homebrew update check

The CLI is a thin client. It does NOT manage agent processes directly.

### clust-pool

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

- Define IPC message types (`CliMessage` and `PoolMessage` enums)
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

Messages between CLI and Pool use a length-prefixed binary format:

```
[4 bytes: message length (u32 big-endian)] [N bytes: MessagePack payload]
```

Serialization uses **MessagePack** via `rmp-serde` (compact, fast, schema-friendly).

> **Note:** `serde_json` is also used in the CLI crate for parsing output from external tools (e.g., `brew info --json=v2` for update checks). It is not used for IPC.

### Message Types

```
CLI -> Pool:
  StartAgent { prompt: Option<String>, agent_binary: Option<String>, working_dir: String, cols: u16, rows: u16, accept_edits: bool, pool: String }
  AttachAgent { id: String }
  DetachAgent { id: String }
  AgentInput { id: String, data: Vec<u8> }
  ResizeAgent { id: String, cols: u16, rows: u16 }
  ListAgents { pool: Option<String> }
  StopPool
  StopAgent { id: String }
  SetDefault { agent_binary: String }
  GetDefault
  RegisterRepo { path: String }
  ListRepos

Pool -> CLI:
  Ok
  AgentStarted { id: String, agent_binary: String }
  AgentAttached { id: String, agent_binary: String }
  AgentOutput { id: String, data: Vec<u8> }
  AgentExited { id: String, exit_code: i32 }
  AgentList { agents: Vec<AgentInfo> }
  AgentStopped { id: String }  // Sent when stop is initiated (agent may still be terminating)
  DefaultAgent { agent_binary: Option<String> }
  PoolShutdown
  Error { message: String }
  RepoRegistered { path: String, name: String }
  RepoList { repos: Vec<RepoInfo> }
```

### Connection Lifecycle

1. CLI opens connection to `~/.clust/clust.sock`
2. If connection fails → CLI spawns `clust-pool` as a background process, retries
3. CLI sends command message
4. For attach: connection stays open, bidirectional streaming (output down, input up)
5. For one-shot commands (ls, stop): pool responds, connection closes

## Agent Lifecycle

```
clust "do something"
        │
        ▼
  CLI connects to Pool
        │
        ▼
  Pool spawns agent PTY
  (e.g., `claude "do something"`)
        │
        ▼
  Pool assigns ID (e.g., a3f8c1)
  Pool sends AgentStarted { id }
        │
        ▼
  CLI enters attached mode:
    - Pool streams PTY output → CLI renders in terminal
    - CLI forwards keyboard input → Pool → agent PTY
    - CLI draws bottom status bar
        │
        ▼
  User detaches (Ctrl+D or shortcut)
    - CLI disconnects, agent keeps running in pool
        │
        ▼
  Agent process exits
    - Pool removes agent from pool
    - Pool notifies any attached CLIs → they exit gracefully
```

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing |
| `tokio` | Async runtime (pool daemon, IPC) |
| `portable-pty` | Cross-platform PTY allocation |
| `rusqlite` | SQLite access (with bundled feature) |
| `rmp-serde` | MessagePack serialization (IPC framing) |
| `ratatui` | Terminal UI rendering (TUI dashboard, status bar) |
| `crossterm` | Terminal manipulation (raw mode, input) |
| `tao` | Native event loop (macOS tray icon support) |
| `tray-icon` | System tray icon and menu |
| `git2` | Git repository/branch/worktree detection |
| `serde_json` | Parse JSON output from external tools (e.g., Homebrew) |
| `which` | Locate agent binaries on PATH (default agent discovery) |
