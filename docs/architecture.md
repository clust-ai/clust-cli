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
- Render agent output to the terminal
- Draw the bottom status bar (agent ID, shortcuts)
- Handle attach/detach lifecycle

The CLI is a thin client. It does NOT manage agent processes directly.

### clust-pool

- Run as a background daemon (no UI, no terminal)
- Manage agent lifecycles: spawn, track, clean up on exit
- Allocate PTYs for each agent
- Multiplex PTY output to all attached CLI clients
- Route input from any attached CLI client to the agent PTY
- Accept IPC commands (start agent, attach, list, stop, etc.)
- Generate agent IDs (6-char hex hash)
- Auto-start when first `clust` command is run
- Shut down on `clust -s` / `clust --stop` (kills all running agents)

## IPC Design

### Protocol: Unix Domain Socket

- **Socket path**: `~/.clust/clust.sock`
- **Why**: Fast, secure (filesystem permissions), no network exposure
- **Cross-platform plan**: Use the `interprocess` crate which abstracts Unix domain sockets (macOS/Linux) and named pipes (Windows)

### Message Format

Messages between CLI and Pool use a length-prefixed binary format:

```
[4 bytes: message length (u32 big-endian)] [N bytes: message payload (MessagePack or JSON)]
```

Candidate serialization: **MessagePack** via `rmp-serde` (compact, fast, schema-friendly). JSON is the fallback if debugging simplicity is preferred during development.

### Message Types

```
CLI -> Pool:
  StartAgent { prompt: Option<String>, agent_binary: Option<String> }
  AttachAgent { id: String }
  DetachAgent { id: String }
  ListAgents
  StopPool
  SetDefault { agent_binary: String }
  GetDefault

Pool -> CLI:
  AgentStarted { id: String }
  AgentOutput { id: String, data: Vec<u8> }
  AgentExited { id: String, exit_code: i32 }
  AgentList { agents: Vec<AgentInfo> }
  Error { message: String }
  Ok
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

## Key Dependencies (Planned)

| Crate | Purpose |
|-------|---------|
| `clap` | CLI argument parsing |
| `tokio` | Async runtime (pool daemon, IPC) |
| `portable-pty` | Cross-platform PTY allocation |
| `interprocess` | Cross-platform IPC (Unix sockets / named pipes) |
| `rusqlite` | SQLite access |
| `rmp-serde` | MessagePack serialization |
| `ratatui` | Terminal UI rendering (bottom bar, future `clust ui`) |
| `crossterm` | Terminal manipulation (raw mode, input) |
