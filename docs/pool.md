# Pool Daemon

## Overview

`clust-pool` is a headless background daemon that owns all agent processes. It has no terminal UI. It communicates exclusively over IPC with `clust-cli` instances.

## Lifecycle

### Startup

The pool starts automatically when any `clust` command is run and no pool is already active.

**Startup sequence:**

1. CLI tries to connect to `~/.clust/clust.sock`
2. Connection fails → no pool running
3. CLI spawns `clust-pool` as a detached background process
4. CLI retries connection with short backoff (e.g., 50ms intervals, max 2s)
5. Connection succeeds → CLI proceeds with its command

**Pool startup:**

1. Check for stale socket file at `~/.clust/clust.sock` → remove if exists
2. Create and bind Unix domain socket
3. Open/create SQLite database at `~/.clust/clust.db`
4. Enter main event loop (accept connections, manage agents)

### Shutdown

Triggered by `clust -s` / `clust --stop` (no argument).

**Shutdown sequence:**

1. Pool receives `StopPool` message over IPC
2. Pool replies `Ok` to the requesting CLI
3. Pool removes the socket file (`~/.clust/clust.sock`)
4. Pool signals the main event loop to exit (all agents terminate with the process)

> **Future work:** Graceful agent termination (SIGTERM with timeout, then SIGKILL) and
> notification of attached CLI clients before shutdown are planned but not yet implemented.

### Crash Recovery

Since the pool is ephemeral (no state survives restart):

- If the pool crashes, the socket file may be stale
- On next startup, the pool removes any existing socket file before binding
- Running agents are lost on pool crash (they were children of the pool process)

## Agent Management

### Spawning an Agent

When the pool receives a `StartAgent` message:

1. Determine agent binary: use `agent_binary` from the message, or fall back to the stored default (from SQLite). If no default is configured, return an error — the CLI prompts the user to pick a default before calling `StartAgent`.
2. Generate a 6-character hex ID (from random bytes, check for collision against running agents)
3. Allocate a PTY pair (master/slave) via `portable-pty`
4. Spawn the agent process in the slave PTY
   - If `prompt` is provided, pass it as an argument to the agent binary
   - Set working directory to the directory the CLI was invoked from (passed in the StartAgent message)
5. Store agent metadata in memory:
   - ID, agent binary, PID, PTY master handle, start time, attached clients
6. Begin reading from PTY master → buffer output, forward to attached clients
7. Return `AgentStarted { id }` to the requesting CLI

### Agent Exit

When the pool detects an agent process has exited:

1. Capture exit code
2. Notify all attached CLI clients with `AgentExited { id, exit_code }`
3. Close PTY
4. Remove agent from the in-memory agent map

### Output Multiplexing

Each agent has:
- A PTY master file descriptor
- A list of attached CLI client connections

The pool reads from the PTY master and fans out data to all attached clients. Each client connection is independent — one slow client does not block others.

### Input Routing

Any attached client can send input. The pool writes it directly to the agent's PTY master. Multiple clients sending input simultaneously is allowed (agent sees interleaved input).

## In-Memory State

```rust
struct PoolState {
    agents: HashMap<String, AgentEntry>,
    default_agent: Option<String>, // loaded from SQLite on startup; None if unset
    db: Option<rusqlite::Connection>, // open SQLite connection; Some after init_db()
}

struct AgentEntry {
    id: String,               // 6-char hex
    agent_binary: String,     // e.g., "claude"
    started_at: String,       // RFC 3339 timestamp
    working_dir: String,
    pty_master: Box<dyn MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    output_tx: broadcast::Sender<AgentEvent>,
    attached_count: Arc<AtomicUsize>,
}
```

This state is entirely in-memory. Nothing here survives a pool restart.
