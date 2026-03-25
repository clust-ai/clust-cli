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

Triggered by `clust -s` / `clust --stop`.

**Shutdown sequence:**

1. Pool receives `StopPool` message
2. Send SIGTERM to all running agent processes
3. Wait briefly for graceful exit (e.g., 3s timeout)
4. SIGKILL any remaining agents
5. Notify all attached CLI clients that the pool is shutting down
6. Close all IPC connections
7. Remove socket file
8. Exit

### Crash Recovery

Since the pool is ephemeral (no state survives restart):

- If the pool crashes, the socket file may be stale
- On next startup, the pool removes any existing socket file before binding
- Running agents are lost on pool crash (they were children of the pool process)

## Agent Management

### Spawning an Agent

When the pool receives a `StartAgent` message:

1. Determine agent binary: use `agent_binary` from the message, or fall back to the stored default (from SQLite), or fall back to `claude`
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
    default_agent: String, // loaded from SQLite on startup
}

struct AgentEntry {
    id: String,               // 6-char hex
    agent_binary: String,     // e.g., "claude"
    pid: u32,
    pty_master: PtyMaster,
    started_at: Instant,
    working_dir: PathBuf,
    attached_clients: Vec<ClientConnection>,
}
```

This state is entirely in-memory. Nothing here survives a pool restart.
