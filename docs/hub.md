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

1. Create `~/.clust/` directory if it doesn't exist
2. Open/create SQLite database at `~/.clust/clust.db`
3. Ensure `.clust/worktrees` is in the global git exclude file (see Git Exclusion below)
4. Check for stale socket file at `~/.clust/clust.sock` → remove if exists
5. Create and bind Unix domain socket
6. Enter main event loop (accept connections, manage agents)

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
7. Return `AgentStarted { id, agent_binary, is_worktree, repo_path, branch_name }` to the requesting CLI

### Agent Exit

When the hub detects an agent process has exited:

1. Capture exit code
2. Notify all attached CLI clients with `AgentExited { id, exit_code }`
3. Close PTY
4. Remove agent from the in-memory agent map

#### Worktree Cleanup on Agent Stop

When agents running in git worktrees are stopped or exit, the CLI prompts the user with an interactive arrow-key selector offering three options:

- **keep** — Leave the worktree and branch as-is
- **discard worktree** — Remove the worktree directory (`git worktree remove --force`)
- **discard worktree + branch** — Remove the worktree and delete the local branch (`git branch -D`)

The prompt includes a dirty-state warning when the worktree has uncommitted changes. This flow is triggered in all stop paths:

- `clust -s` (stop hub): queries all agents before stopping, prompts after hub shutdown
- `clust -s <id>` (stop agent): queries agents, prompts only if the stopped agent was the last one in its worktree
- `clust repo -s` (stop repo agents): queries repo agents before stopping, prompts after
- Session exit (start/attach): prompts when the agent exits (not on detach), checking if other agents remain in the worktree
- TUI `Q` (stop hub): collects worktree info from in-memory agents, prompts after TUI cleanup
- TUI context menus: "Stop All Agents" (repo menu), "Stop Agents" (branch menu) trigger a worktree cleanup dialog within the TUI
- TUI focus mode: cleanup dialog appears immediately when an agent exits (if it was a worktree agent)
- TUI overview mode: cleanup dialog appears when exiting a terminal (double-Esc) if the agent has exited and was a worktree agent

The worktree removal is performed locally using git commands, independent of the hub. This ensures cleanup works even after the hub has shut down.

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

The hub handles Git worktree operations on behalf of CLI clients. Worktrees are stored in a `.clust/worktrees/` directory at the repository root. Each worktree directory is named using a serialized form of the branch name (slashes are replaced with double underscores, e.g., `feature/auth` becomes `feature__auth`).

### Operations

- **List**: Enumerates all worktrees (including main) via `git2`, checks dirty status, and matches active agents to each worktree by working directory.
- **Add**: Creates a new worktree with either a new branch or an existing branch (`--checkout`). If the worktree already exists on disk (detected by the presence of a `.git` file in the worktree path), the existing worktree is reused and returned immediately without running `git worktree add`. New branch names are sanitized via `sanitize_branch_name()` (existing branches checked out with `--checkout` are not sanitized). When checking out an existing branch that is currently HEAD in the main worktree, HEAD is automatically detached first so the branch can be moved to the new worktree. If worktree creation fails because the branch is already checked out (e.g., in another worktree), the raw git error is replaced with a user-friendly message suggesting to use "Start Agent (in place)" or create a new branch. Optionally launches an agent in the new worktree.
- **Remove**: Prunes the worktree from git and removes its directory. Stops any agents running in the worktree. Refuses to remove dirty worktrees unless `--force` is specified. Optionally deletes the local branch (`--local`).
- **Info**: Returns detailed information for a single worktree including path, dirty status, and active agents.

### Create Worktree Agent

When the hub receives a `CreateWorktreeAgent` message (sent from the TUI create-agent modal):

1. Sanitize the branch name via `clust_ipc::branch::sanitize_branch_name()` as defense-in-depth (the CLI also sanitizes before sending). Only new branch names are sanitized; existing branch names (from `target_branch`) are already valid git refs.
2. Create or check out a worktree using the existing `add_worktree()` logic:
   - If `new_branch` is provided, create a new worktree with that branch (using `target_branch` as the base branch if specified).
   - If `new_branch` is not provided, check out the `target_branch` as a worktree.
   - If worktree creation fails because the branch is already checked out, the raw git error is replaced with a user-friendly message suggesting to use "Start Agent (in place)" from the context menu or to create a new branch.
2. Spawn an agent in the new worktree directory (same logic as `StartAgent`).
3. Return `WorktreeAgentStarted { id, agent_binary, working_dir, repo_path, branch_name }` to the CLI.

This combines worktree creation and agent spawning into a single atomic operation, used by the `Opt+R` / `Alt+R` modal in the TUI.

### Repository Resolution

When the CLI sends a worktree command, the hub resolves the target repository via one of two methods:

1. **By name** (`-r/--repo`): Looks up the registered repo by name in SQLite. Errors if zero or multiple matches are found.
2. **By working directory**: Uses the `working_dir` from the CLI request and resolves the git root via `git2`.

The hub also ensures the `.clust/` directory is added to `.git/info/exclude` so it does not pollute the repository.

### Git Exclusion

On startup, the hub adds `.clust/worktrees` to the global git exclude file so that worktree directories are ignored across all repositories without needing per-repo `.gitignore` entries. The global exclude file is located via `core.excludesFile` in the git config, falling back to `$XDG_CONFIG_HOME/git/ignore` (or `~/.config/git/ignore`). This operation is idempotent -- it checks whether the entry already exists before appending.

Additionally, when a repository is first used (worktree creation, branch detection), the hub adds `.clust/` to that repository's `.git/info/exclude` to keep the per-repo `.clust/` directory hidden from git status.

## Branch Management

The hub handles branch deletion operations on behalf of CLI clients.

### Delete Local Branch

When the hub receives a `DeleteLocalBranch` message:

1. Resolve the target repository (by working directory or repo name).
2. If the branch is checked out as a worktree, remove the worktree first (force removal) and stop any agents running in it.
3. Delete the local branch using `git branch -D` (force) or `git branch -d` (non-force).
4. Return `LocalBranchDeleted { branch_name, stopped_agents }`.

### Delete Remote Branch

When the hub receives a `DeleteRemoteBranch` message:

1. Resolve the target repository.
2. Parse the remote name and branch from the full ref (e.g., `origin/feature` -> remote `origin`, branch `feature`).
3. Delete the remote branch using `git push <remote> --delete <branch>`.
4. Return `RemoteBranchDeleted { branch_name }`.

### Checkout Remote Branch

When the hub receives a `CheckoutRemoteBranch` message:

1. Resolve the target repository (by working directory or repo name).
2. Parse the remote name and local branch name from the full ref (e.g., `origin/feature` -> local branch `feature`).
3. Check out the remote branch as a local tracking branch using `git checkout --track <remote/branch>`.
4. Return `RemoteBranchCheckedOut { branch_name }` with the local branch name.

### Pull Branch

When the hub receives a `PullBranch` message:

1. Resolve the target repository from `repo_path`.
2. Determine how the branch is checked out:
   - **Repo HEAD:** Run `git pull` in the repo root directory.
   - **Worktree:** Run `git pull` in the worktree directory.
   - **Not checked out anywhere:** Run `git fetch origin <branch>:<branch>` for a fast-forward-only update.
3. Return `BranchPulled { branch_name, summary }` with the git output, or `Error` on failure.

## Repository Creation

### Create Repository

When the hub receives a `CreateRepo` message:

1. Validate the parent directory exists.
2. Run `git init` in a new subdirectory named by `name` within `parent_dir`.
3. Register the new repository in the SQLite database with an auto-assigned color.
4. Return `RepoCreated { path, name }` on success, or `Error` on failure.

### Clone Repository

When the hub receives a `CloneRepo` message:

1. Validate the parent directory exists.
2. Determine the repository name: use the provided `name` if given, otherwise extract it from the URL via `repo_name_from_url()` (handles both HTTPS and SSH URL formats).
3. Spawn `git clone --progress` as a child process with stderr piped for progress output.
4. Send an initial `CloneProgress { step }` message to the client.
5. Read stderr line-by-line in a `spawn_blocking` task, bridged to the async event loop via an unbounded channel. Each non-empty line is forwarded to the client as a `CloneProgress { step }` message, enabling real-time progress display.
6. On successful completion, register the cloned repository in the SQLite database with an auto-assigned color.
7. Return `RepoCloned { path, name }` on success, or `Error` on failure at any stage.

## Repository Maintenance

### Clean Stale Refs

When the hub receives a `CleanStaleRefs` message:

1. Resolve the target repository.
2. List all remotes via `git remote`.
3. Run `git remote prune <remote>` for each remote to remove stale remote tracking refs.
4. Return `StaleRefsCleaned { path }`.

### Purge Repository

When the hub receives a `PurgeRepo` message:

1. Detect the git root from the provided path.
2. Stop all agents associated with the repository. Sends `PurgeProgress { step: "Stopping agents..." }` and awaits agent process termination before proceeding.
3. Remove all non-main worktrees using `git worktree remove --force`. Sends `PurgeProgress { step: "Removing worktrees..." }`. Also removes the `.clust/worktrees/` directory entirely to catch any leftover files.
4. Delete all non-HEAD local branches using `git branch -D`. Sends `PurgeProgress { step: "Deleting branches..." }`.
5. Clean stale remote refs (same as `CleanStaleRefs`). Sends `PurgeProgress { step: "Cleaning stale refs..." }`.
6. Return `RepoPurged { path, stopped_agents, removed_worktrees, deleted_branches }`.

Each phase sends a `PurgeProgress` IPC message to the client before execution, allowing the TUI to display real-time progress. The hub awaits agent stops before proceeding to worktree removal to prevent race conditions.

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
