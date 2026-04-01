# Hub Daemon

## Overview

`clust-hub` is a headless background daemon that owns all agent processes. It has no terminal UI. It communicates exclusively over IPC with `clust-cli` instances.

## Lifecycle

### Startup

The hub starts automatically when any `clust` command is run and no hub is already active.

**Startup sequence:**

1. CLI tries to connect to `~/.clust/clust.sock`
2. Connection fails → no hub running
3. CLI spawns `clust-hub` as a detached background process
4. CLI retries connection with short backoff (e.g., 50ms intervals, max 2s)
5. Connection succeeds → CLI proceeds with its command

**Hub startup:**

1. Check for stale socket file at `~/.clust/clust.sock` → remove if exists
2. Create and bind Unix domain socket
3. Open/create SQLite database at `~/.clust/clust.db`
4. Enter main event loop (accept connections, manage agents)

### Shutdown

Triggered by `clust -s` / `clust --stop` (no argument).

**Shutdown sequence:**

1. Hub receives `StopHub` message over IPC
2. Hub replies `Ok` to the requesting CLI
3. Hub notifies all attached CLI clients via broadcast channels (`HubShutdown` event)
4. Hub sends SIGTERM to all agent processes, waits 3 seconds, then SIGKILL any survivors
5. Hub removes the socket file (`~/.clust/clust.sock`)
6. Hub signals the tao event loop to exit

### Crash Recovery

Since the hub is ephemeral (no state survives restart):

- If the hub crashes, the socket file may be stale
- On next startup, the hub removes any existing socket file before binding
- Running agents are lost on hub crash (they were children of the hub process)

## Hubs

Hubs are logical groupings of agents within the single `clust-hub` process. They are **not** separate daemon instances.

- **Default hub:** `default_hub` — all agents spawn here unless `-H` is specified
- **Naming convention:** snake_case (`^[a-z][a-z0-9]*(_[a-z0-9]+)*$`) — must start with a lowercase letter, no trailing or consecutive underscores
- **Lifecycle:** implicit — a hub exists as long as at least one agent references it; empty hubs disappear from listings
- **No creation command:** hubs are created on first use when an agent is assigned to one

### Usage

```bash
# Start agent in default hub
clust "fix the bug"

# Start agent in a named hub
clust -H my_feature "fix the bug"

# List all agents grouped by hub
clust ls

# List only agents in a specific hub
clust ls -H my_feature
```

The TUI (`clust ui`) shows hub names in the left sidebar and groups agent cards by hub in the main panel.

## Agent Management

### Spawning an Agent

When the hub receives a `StartAgent` message:

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

When the hub detects an agent process has exited:

1. Capture exit code
2. Notify all attached CLI clients with `AgentExited { id, exit_code }`
3. Close PTY
4. Remove agent from the in-memory agent map

### Output Multiplexing

Each agent has:
- A PTY master file descriptor
- A list of attached CLI client connections
- A replay buffer (512 KB ring buffer of recent PTY output)

The hub reads from the PTY master and fans out data to all attached clients via a broadcast channel (capacity 1024). Each client connection is independent — one slow client does not block others.

All PTY output is recorded in the agent's replay buffer as it arrives. When a client attaches, the hub sends the entire replay buffer contents before starting live streaming, followed by an `AgentReplayComplete` sentinel message. This ensures the client sees recent history without missing output that occurred before attachment. If a broadcast channel receiver lags (slow client), the hub re-sends the replay buffer to resync that client instead of silently dropping frames.

### Input Routing

Any attached client can send input. The hub writes it directly to the agent's PTY master. Multiple clients sending input simultaneously is allowed (agent sees interleaved input).

## Worktree Management

The hub handles Git worktree operations on behalf of CLI clients. Worktrees are stored in a `.zm-worktrees/` directory at the repository root. Each worktree directory is named using a serialized form of the branch name (slashes are replaced with double underscores, e.g., `feature/auth` becomes `feature__auth`).

### Operations

- **List**: Enumerates all worktrees (including main) via `git2`, checks dirty status, and matches active agents to each worktree by working directory.
- **Add**: Creates a new worktree with either a new branch or an existing branch (`--checkout`). Optionally launches an agent in the new worktree.
- **Remove**: Prunes the worktree from git and removes its directory. Stops any agents running in the worktree. Refuses to remove dirty worktrees unless `--force` is specified. Optionally deletes the local branch (`--local`).
- **Info**: Returns detailed information for a single worktree including path, dirty status, and active agents.

### Create Worktree Agent

When the hub receives a `CreateWorktreeAgent` message (sent from the TUI create-agent modal):

1. Create or check out a worktree using the existing `add_worktree()` logic:
   - If `new_branch` is provided, create a new worktree with that branch (using `target_branch` as the base branch if specified).
   - If `new_branch` is not provided, check out the `target_branch` as a worktree.
2. Spawn an agent in the new worktree directory (same logic as `StartAgent`).
3. Return `WorktreeAgentStarted { id, agent_binary, working_dir }` to the CLI.

This combines worktree creation and agent spawning into a single atomic operation, used by the `Alt+E` modal in the TUI.

### Repository Resolution

When the CLI sends a worktree command, the hub resolves the target repository via one of two methods:

1. **By name** (`-r/--repo`): Looks up the registered repo by name in SQLite. Errors if zero or multiple matches are found.
2. **By working directory**: Uses the `working_dir` from the CLI request and resolves the git root via `git2`.

The hub also ensures the `.clust/` directory is added to `.git/info/exclude` so it does not pollute the repository.

## In-Memory State

```rust
struct HubState {
    agents: HashMap<String, AgentEntry>,
    default_agent: Option<String>,         // loaded from SQLite on startup; None if unset
    db: Option<rusqlite::Connection>,      // open SQLite connection; Some after init_db()
}

struct AgentEntry {
    id: String,                            // 6-char hex
    agent_binary: String,                  // e.g., "claude"
    started_at: String,                    // RFC 3339 timestamp
    working_dir: String,
    hub: String,                           // e.g., "default_hub"
    pid: Option<u32>,                      // OS process ID (for SIGTERM/SIGKILL)
    pty_master: Box<dyn MasterPty + Send>,
    pty_writer: Box<dyn Write + Send>,
    output_tx: broadcast::Sender<AgentEvent>,
    replay_buffer: Arc<Mutex<ReplayBuffer>>, // 512 KB ring buffer of recent PTY output
    attached_count: Arc<AtomicUsize>,
    client_sizes: HashMap<u64, (u16, u16)>,// per-client terminal sizes
    current_pty_size: (u16, u16),          // current PTY dimensions (skip redundant resizes)
    active_client_id: Option<u64>,         // most recently active client
    next_client_id: AtomicU64,             // monotonic counter for client IDs
    repo_path: Option<String>,             // git repo root (None if not in a git repo)
    branch_name: Option<String>,           // current git branch
    is_worktree: bool,                     // whether working_dir is a git worktree
}
```

This state is entirely in-memory. Nothing here survives a hub restart.
