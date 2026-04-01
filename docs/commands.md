# CLI Commands Reference

## Naming Conventions

All flags follow POSIX/GNU conventions:

- **Short flags**: single dash, single letter (`-b`, `-s`)
- **Long flags**: double dash, kebab-case (`--background`, `--stop`)
- **Subcommands**: no dash, lowercase (`ls`, `ui`)
- **Flag values**: space-separated (`-a a3f8c1`, `--attach a3f8c1`)
- **Positional args**: no prefix (`clust "say hi"`)

Short and long forms are always provided together.

---

## Commands

### `clust`

Start a new agent session and attach to it.

```
clust [OPTIONS] [PROMPT]
```

| Argument | Description |
|----------|-------------|
| `PROMPT` | Optional. Positional argument. Passed to the agent as its initial prompt. |

| Flag | Long | Description |
|------|------|-------------|
| `-b` | `--background` | Start agent without attaching. Returns the agent ID. |
| `-a` | `--attach <ID>` | Attach to an existing agent by its 6-char ID. |
| `-s` | `--stop [ID]` | Without a value: stop the hub daemon and all agents. With a 6-char ID: stop that specific agent. |
| `-d` | `--default` | Interactive picker to set the global default agent binary. Persisted in SQLite. |
| `-u` | `--use <AGENT>` | Use a specific agent binary for this session only (does not change the default). |
| `-e` | `--accept-edits` | Auto-accept edits. Agent-specific: for Claude, passes `--permission-mode acceptEdits`. Ignored for agents that don't support it. |
| `-H` | `--hub <NAME>` | Assign the agent to a named hub (snake_case; default: `default_hub`). Hubs are logical groupings within the single hub process. |
| `-h` | `--help` | Show help with all available options. |
| `-V` | `--version` | Show version. |

### `clust ls`

List all running agents in the hub.

```
clust ls [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-i` | `--select` | Interactive selector: navigate with arrow keys, Enter to attach or start a new agent. |
| `-H` | `--hub <NAME>` | Filter agents by hub name. Without this flag, agents are grouped by hub. |

Output (no filter — grouped by hub):

```
  default_hub
  ID       AGENT        STATUS     STARTED        ATTACHED
  a3f8c1   claude       running    14:32          1 terminal
  b7e2d9   claude       running    14:17          0 terminals

  my_feature
  ID       AGENT        STATUS     STARTED        ATTACHED
  c4d5e6   aider        running    15:01          1 terminal
```

Output (with `-H` filter — flat list):

```
  ID       AGENT        STATUS     STARTED        ATTACHED
  c4d5e6   aider        running    15:01          1 terminal
```

### `clust ui`

Open the Clust terminal UI.

```
clust ui
```

`clust .` is an alias for `clust ui`.

### `clust wt` / `clust worktree`

Git worktree management. All subcommands operate on the repository detected from the current directory, or on a registered repository specified by `-r/--repo`.

```
clust wt [OPTIONS] <COMMAND>
```

| Flag | Long | Description |
|------|------|-------------|
| `-r` | `--repo <NAME>` | Target a registered repo by name instead of using the current working directory. |

#### `clust wt ls`

List all worktrees in the repository. Shows branch name, active agent count, dirty status, and the main worktree indicator.

```
clust wt ls
clust wt ls -r my-repo
```

#### `clust wt add`

Create a new worktree. Creates a new branch by default, or checks out an existing branch with `--checkout`.

```
clust wt add <NAME> [OPTIONS]
```

| Argument | Description |
|----------|-------------|
| `NAME` | Branch name for the new worktree. |

| Flag | Long | Description |
|------|------|-------------|
| `-b` | `--branch <BASE>` | Base branch to create from (default: current HEAD). |
| | `--checkout` | Check out an existing branch instead of creating a new one. |
| `-p` | `--prompt [PROMPT]` | Start an agent in the new worktree. Optionally pass a prompt string. |

```bash
# Create worktree with new branch
clust wt add feature/auth

# Create from a specific base branch
clust wt add feature/auth -b develop

# Check out an existing branch
clust wt add feature/auth --checkout

# Create worktree and start an agent in it
clust wt add feature/auth -p "implement auth module"

# Create worktree and start agent with no prompt
clust wt add feature/auth -p
```

#### `clust wt rm`

Remove a worktree. Refuses to remove dirty worktrees unless `--force` is used.

```
clust wt rm [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-b` | `--branch <NAME>` | Target branch (default: current worktree's branch). |
| `-l` | `--local` | Also delete the local branch after removing the worktree. |
| | `--force` | Force remove even if the worktree has uncommitted changes. |

```bash
# Remove current worktree
clust wt rm

# Remove a specific worktree by branch
clust wt rm -b feature/auth

# Remove and delete the local branch
clust wt rm -l

# Force remove dirty worktree
clust wt rm --force
```

#### `clust wt info`

Show detailed information about a specific worktree, including path, dirty status, and active agents.

```
clust wt info <NAME>
```

| Argument | Description |
|----------|-------------|
| `NAME` | Branch name of the worktree to inspect. |

### `clust repo`

Repository management.

```
clust repo [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-a` | `--add` | Register the current directory's git repository for tracking in the TUI. |
| `-r` | `--remove` | Remove a repository from clust tracking. Stops all agents first. Prompts for confirmation. |
| `-s` | `--stop` | Stop all agents running on the current repository (keeps repo tracked). |

---

## Usage Examples

```bash
# Start a new agent session (default: claude), attach to it
clust

# Start an agent with a prompt
clust "refactor the auth module"

# Start an agent in the background
clust -b

# Start a background agent with a prompt
clust -b "run the test suite and fix failures"

# Start with accept-edits enabled
clust -e "refactor the auth module"

# Background agent with accept-edits
clust -e -b "run the test suite and fix failures"

# Start an agent in a named hub
clust -H my_feature "refactor auth"

# List agents grouped by hub
clust ls

# List only agents in a specific hub
clust ls -H my_feature

# Use a specific agent for this session only
clust -u opencode "refactor auth"

# Attach to a running agent
clust -a a3f8c1

# List all running agents
clust ls

# Set default agent interactively
clust -d

# Interactive agent selector
clust ls -i

# Stop a specific agent
clust -s a3f8c1

# Stop the hub and all agents
clust -s

# Register the current repo for TUI tracking
clust repo -a
clust repo --add

# Remove a repo from tracking (stops agents, prompts for confirmation)
clust repo --remove
clust repo -r

# Stop all agents in the current repo (keeps repo tracked)
clust repo --stop
clust repo -s

# Open terminal UI
clust ui
clust .

# List worktrees
clust wt ls
clust worktree ls

# List worktrees for a specific registered repo
clust wt ls -r my-repo

# Create a new worktree
clust wt add feature/auth

# Create worktree from a base branch
clust wt add feature/auth -b develop

# Check out existing branch as worktree
clust wt add feature/auth --checkout

# Create worktree and start agent with prompt
clust wt add feature/auth -p "implement auth"

# Remove current worktree
clust wt rm

# Remove worktree by branch and delete the local branch
clust wt rm -b feature/auth -l

# Force remove dirty worktree
clust wt rm --force

# Show worktree details
clust wt info feature/auth

# Show help
clust -h
```

---

## Keyboard Shortcuts (While Attached)

These shortcuts are active when the CLI is attached to an agent session. They are displayed in the bottom status bar.

| Shortcut | Action |
|----------|--------|
| `Ctrl+Q` | Detach from agent (agent keeps running in hub) |

## Keyboard Shortcuts (Terminal UI)

These shortcuts are active in the `clust ui` dashboard. They are displayed in the bottom status bar.

**Global:**

| Shortcut | Action |
|----------|--------|
| `q` / `Esc` | Quit the UI (hub keeps running) |
| `Q` | Quit the UI and stop the hub |
| `Tab` | Switch to next tab |
| `Shift+Tab` | Switch to previous tab |
| `?` | Toggle keyboard shortcut overlay |
| `Opt+E` (macOS) / `Alt+E` | Open the create-agent modal (multi-step builder for creating agents on worktrees) |

**Repositories tab:**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection within current level |
| `→` | Descend into selected item, or expand if collapsed |
| `←` | Collapse current item, or ascend to parent level |
| `Enter` | Left panel: on repo opens context menu (Change Color); on branch with 1 agent opens focus mode, with multiple agents opens agent picker; on category toggles collapse. Right panel: enter focus mode for selected agent. |
| `Shift+←` / `Shift+→` | Switch focus between left and right panels |
| `v` | Toggle agent grouping between by-hub and by-repo (right panel) |

**Overview tab (Options Bar focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↓` | Enter terminal focus |
| `Shift+←` / `Shift+→` | Scroll viewport left/right |

**Overview tab (Terminal focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↑` | Exit terminal, return to options bar |
| `Shift+↓` | Enter focus mode for the focused agent |
| `Shift+←` / `Shift+→` | Switch to previous/next agent panel |
| All other keys | Forwarded to the focused agent's PTY |

**Focus mode (right panel focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` | Exit focus mode, return to originating tab |
| `Shift+←` | Switch focus to left panel |
| `Shift+PageUp` | Scroll up through scrollback history |
| `Shift+PageDown` | Scroll down through scrollback history |
| All other keys | Forwarded to the focused agent's PTY |

**Focus mode (left panel focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` | Exit focus mode, return to originating tab |
| `↑` / `↓` | Scroll diff up/down |
| `Shift+↑` | Jump to previous file header |
| `Shift+↓` | Jump to next file header |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab |
