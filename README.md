# clust

A CLI tool for managing AI coding agents. Run multiple agents concurrently in a background hub daemon, monitor them in a terminal UI, and manage git worktrees — all from one tool.

## Install

```bash
brew install clust-ai/tap/clust
```

Or build from source:

```bash
git clone https://github.com/clust-ai/clust-cli.git
cd clust-cli
cargo build --release
cp target/release/clust target/release/clust-hub ~/.clust/bin/
```

## How It Works

```
  Terminal 1        Terminal 2        Terminal 3
  (clust-cli)       (clust-cli)       (clust-cli)
       │                 │                 │
       └─────────────────┼─────────────────┘
                         │  Unix Domain Socket
                         │
                 ┌───────┴────────┐
                 │   clust-hub    │  (background daemon)
                 │                │
                 │  ┌──────────┐  │
                 │  │ Agent PTY │  │  a3f8c1
                 │  │ (claude)  │  │
                 │  └──────────┘  │
                 │                │
                 │  ┌──────────┐  │
                 │  │ Agent PTY │  │  b7e2d9
                 │  │ (claude)  │  │
                 │  └──────────┘  │
                 └────────────────┘
```

The CLI is a thin client. All agent processes live in `clust-hub`, a single background daemon that starts automatically on first use. Multiple terminals can attach to the same agent simultaneously.

## Usage

```bash
# Start a new agent session and attach
clust

# Start with a prompt
clust "refactor the auth module"

# Start in the background
clust -b "run the test suite and fix failures"

# Use a specific agent binary for this session
clust -u aider "refactor the auth module"

# Auto-accept edits (agent-specific, e.g. Claude's acceptEdits mode)
clust -e "update the config parser"

# List running agents
clust ls

# Interactive agent selector
clust ls -i

# Attach to a running agent
clust -a a3f8c1

# Open the terminal UI dashboard
clust ui   # or: clust .

# Set default agent binary
clust -d

# Stop a specific agent
clust -s a3f8c1

# Stop the hub and all agents
clust -s
```

### Worktree Management

```bash
# List worktrees in current repo
clust wt ls

# Create a worktree with a new branch
clust wt add feature-login

# Create a worktree and start an agent in it
clust wt add feature-login -p "implement login flow"

# Remove a worktree (and optionally its local branch)
clust wt rm -b feature-login -l

# Show worktree details
clust wt info feature-login
```

### Repository Tracking

```bash
# Register current repo for tracking in the TUI
clust repo -a

# Remove repo from tracking
clust repo -r

# Stop all agents on current repo
clust repo -s
```

### Flags

| Flag | Long | Description |
|------|------|-------------|
| `-b` | `--background` | Start agent without attaching. Returns the agent ID. |
| `-a` | `--attach <ID>` | Attach to an existing agent by its 6-char ID. |
| `-s` | `--stop [ID]` | Stop a specific agent, or the entire hub if no ID given. |
| `-d` | `--default` | Interactive picker to set the default agent binary. |
| `-u` | `--use <AGENT>` | Use a specific agent binary for this session. |
| `-e` | `--accept-edits` | Auto-accept edits (maps to agent-specific flags). |
| `-H` | `--hub <NAME>` | Assign agent to a named hub (default: `default_hub`). |

### Keyboard Shortcuts

**Attached mode:**

| Shortcut | Action |
|----------|--------|
| `Ctrl+Q` | Detach from agent (agent keeps running) |

**Terminal UI (`clust ui`):**

| Shortcut | Action |
|----------|--------|
| `Tab` / `Shift+Tab` | Switch tabs |
| `↑` `↓` `←` `→` | Navigate repo tree / panels |
| `Enter` | Open context menu or focus mode |
| `Space` | Toggle collapse/expand |
| `Shift+←` / `Shift+→` | Switch panel focus |
| `Shift+↓` | Enter terminal / focus mode |
| `Alt+E` | Create new agent on a worktree |
| `v` | Toggle agent grouping (by-repo / by-hub) |
| `?` | Show keyboard shortcut overlay |
| `q` / `Esc` | Quit UI |

**Focus mode** provides a two-panel view with a live git diff on the left and the agent terminal on the right, with file-jumping and scrollback support.

## Terminal UI

The TUI dashboard (`clust ui`) has three tabs:

- **Repositories** — Browse tracked repos and branches in a tree view, see which agents are running where, and jump into focus mode. Repos are color-coded for quick identification.
- **Overview** — Side-by-side terminal panels showing all running agents with full ANSI rendering, scrollback, and Cmd+click URL opening.
- **Batches** — Batch creation and task management with horizontal batch cards, scheduling, and JSON import.

Mouse support includes click navigation, scroll, and Cmd+click to open URLs.

## Batch JSON Import

Batches can be imported from JSON files using `Alt+I` in the TUI. The file browser opens in `~/Downloads` by default and filters for `.json` files.

### Structure

```json
{
  "title": "My Batch",
  "prefix": "You are working on the auth module.",
  "suffix": "Run tests before finishing.",
  "launch_mode": "auto",
  "max_concurrent": 2,
  "plan_mode": false,
  "allow_bypass": false,
  "tasks": [
    {
      "branch": "feat/login-form",
      "prompt": "Implement the login form component",
      "depends_on": []
    },
    {
      "branch": "feat/login-api",
      "prompt": "Add the login API endpoint",
      "depends_on": []
    },
    {
      "branch": "feat/login-tests",
      "prompt": "Write integration tests for the login flow",
      "depends_on": ["feat/login-form", "feat/login-api"]
    }
  ]
}
```

### Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `title` | string | No | Batch name. Auto-generates "Batch N" if omitted. |
| `prefix` | string | No | Prompt prefix prepended to every task. |
| `suffix` | string | No | Prompt suffix appended to every task. |
| `launch_mode` | string | No | `"auto"` (default) or `"manual"`. |
| `max_concurrent` | number | No | Max concurrent agents (auto mode). Null = unlimited. |
| `plan_mode` | boolean | No | Start agents in plan mode. Default `false`. |
| `allow_bypass` | boolean | No | Allow agents to bypass permission prompts. Default `false`. |
| `tasks` | array | Yes | List of task objects (at least one required). |

**Task fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `branch` | string | Yes | Branch name for the worktree. |
| `prompt` | string | Yes | The prompt for the agent. |
| `depends_on` | array | No | Branch names this task depends on (for ordering/documentation). |

After selecting a JSON file, you'll be prompted to choose a repository and branch. The batch is created with all tasks pre-populated.

## Architecture

Three crates in a Cargo workspace:

| Crate | Description |
|-------|-------------|
| `clust-cli` | CLI binary. Installed as `clust`. |
| `clust-hub` | Background daemon. Manages agent lifecycles, PTYs, and IPC. |
| `clust-ipc` | Shared IPC message types and serialization. |

See [`docs/`](docs/) for detailed design docs covering [architecture](docs/architecture.md), [commands](docs/commands.md), [hub design](docs/hub.md), [storage schema](docs/storage.md), and [terminal UI](docs/terminal-ui.md).

## License

MIT
