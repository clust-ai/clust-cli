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
| `-s` | `--stop [ID]` | Without a value: stop the hub daemon and all agents. With a 6-char ID: stop that specific agent. When agents in worktrees are stopped, prompts with an interactive selector to keep, discard the worktree, or discard the worktree and branch. |
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
| `-B` | `--batch <ID_OR_TITLE>` | Filter agents by batch ID or title (case-insensitive substring match on title). |

Output (no filter — grouped by hub):

```
  default_hub
  ID       AGENT        REPO             BRANCH               STATUS     STARTED        ATTACHED   BATCH
  a3f8c1   claude       my-project       main                 running    14:32          1 terminal
  b7e2d9   claude       my-project       feature/auth         running    14:17          0 terminals refactor-batch

  my_feature
  ID       AGENT        REPO             BRANCH               STATUS     STARTED        ATTACHED   BATCH
  c4d5e6   aider        other-repo       develop              running    15:01          1 terminal
```

Output (with `-H` filter — flat list):

```
  ID       AGENT        REPO             BRANCH               STATUS     STARTED        ATTACHED   BATCH
  c4d5e6   aider        other-repo       develop              running    15:01          1 terminal
```

Output (with `-B` filter — agents in matching batch only):

```
  ID       AGENT        REPO             BRANCH               STATUS     STARTED        ATTACHED   BATCH
  b7e2d9   claude       my-project       feature/auth         running    14:17          0 terminals refactor-batch
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

Show detailed information about a specific worktree, including path, dirty status, active agents, and their attached terminal counts.

```
clust wt info <NAME>
```

| Argument | Description |
|----------|-------------|
| `NAME` | Branch name of the worktree to inspect. |

### `clust bypass`

Toggle bypass-permissions mode for all new agents. When enabled, agents that support it (e.g., Claude) are started with `--dangerously-skip-permissions`. The setting is persisted in SQLite.

```
clust bypass [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `--on` | Enable bypass permissions. |
| `--off` | Disable bypass permissions. |

With no flags, displays the current bypass-permissions state.

`--on` and `--off` are mutually exclusive.

```bash
# Enable bypass permissions
clust bypass --on

# Disable bypass permissions
clust bypass --off

# Show current state
clust bypass
```

### `clust repo`

Repository management.

```
clust repo [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-a` | `--add` | Register the current directory's git repository for tracking in the TUI. |
| `-r` | `--remove` | Remove a repository from clust tracking. Stops all agents first. Prompts for confirmation. |
| `-s` | `--stop` | Stop all agents running on the current repository (keeps repo tracked). Prompts for worktree cleanup if stopped agents were in worktrees. |

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

# List only agents belonging to a batch (by ID or title substring)
clust ls -B refactor
clust ls --batch abc123

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

# Enable bypass permissions (all new agents skip permission checks)
clust bypass --on

# Disable bypass permissions
clust bypass --off

# Show current bypass permissions state
clust bypass

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
| `q` | Quit the UI (hub keeps running) |
| `Esc×2` (double-tap) | Quit the UI (hub keeps running) |
| `Q` | Quit the UI and stop the hub (prompts for worktree cleanup) |
| `Tab` | Switch to next tab |
| `Shift+Tab` | Switch to previous tab |
| `?` | Toggle keyboard shortcut overlay |
| `F2` | Toggle mouse capture (allows text selection and link clicking when off) |
| `Opt+M` (macOS) / `Alt+M` | Temporarily disable mouse capture for 5 seconds (mouse passthrough) |
| `Opt+E` (macOS) / `Alt+E` | Open the create-agent modal (multi-step builder for creating agents on worktrees) |
| `Opt+D` (macOS) / `Alt+D` | Open the detached agent modal (start agent in any directory) |
| `Opt+F` (macOS) / `Alt+F` | Open the search-agent modal (only when agents are running) |
| `Opt+B` (macOS) / `Alt+B` | Toggle bypass permissions (global, persisted) |
| `Opt+N` (macOS) / `Alt+N` | Open the add-repository modal |
| `Opt+V` (macOS) / `Alt+V` | Open in editor (see Editor Integration in terminal-ui.md) |
| `Cmd+1` | Switch to Repositories tab (dismisses context menus, exits focus mode) |
| `Cmd+2` | Switch to Overview tab (dismisses context menus, exits focus mode) |

**Repositories tab:**

| Shortcut | Action |
|----------|--------|
| `↑` / `↓` | Move selection within current level |
| `Shift+↑` / `Shift+↓` | Jump to previous/next repository header (skips categories and branches) |
| `→` | Descend into selected item (navigate tree) |
| `←` | Ascend to parent level (navigate tree) |
| `Enter` | Left panel: on repo opens repo context menu; on local branch opens local branch context menu; on remote branch opens remote branch context menu. Right panel: enter focus mode for selected agent. |
| `Space` | Left panel: toggle collapse/expand on repo or category level |
| `Shift+←` / `Shift+→` | Switch focus between left and right panels |
| `v` | Toggle agent grouping between by-hub and by-repo (right panel) |

**Overview tab (Options Bar focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↓` | Enter terminal focus |
| `Shift+←` / `Shift+→` | Scroll viewport left/right |
| `←` / `→` | Navigate repo groups |
| `Enter` / `Space` | Toggle collapse/expand of selected repo group |

**Overview tab (Terminal focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` (single) | Forward Esc to agent process |
| `Esc×2` (double-tap) | Deselect terminal, return to options bar |
| `Shift+↑` | Exit terminal, return to options bar |
| `Shift+↓` | Enter focus mode for the focused agent |
| `Shift+←` / `Shift+→` | Switch to previous/next agent panel |
| `PageUp` / `PageDown` | Scroll focused panel through scrollback history |
| `Shift+PageUp` / `Shift+PageDown` | Scroll focused panel through scrollback history |
| All other keys | Forwarded to the focused agent's PTY |

**Focus mode (right panel focused):**

| Shortcut | Action |
|----------|--------|
| `Esc` | Forward Esc to agent process |
| `Shift+↑` | Exit focus mode, return to originating tab |
| `Shift+←` | Switch focus to left panel (only when agent has a repo) |
| `Shift+PageUp` | Scroll up through scrollback history |
| `Shift+PageDown` | Scroll down through scrollback history |
| All other keys | Forwarded to the focused agent's PTY |

**Focus mode (left panel focused):**

| Shortcut | Action |
|----------|--------|
| `Shift+↑` | Exit focus mode, return to originating tab |
| `↑` / `↓` | Scroll diff up/down |
| `Shift+→` | Switch focus to right panel |
| `Tab` | Cycle to next left panel tab |

**Focus mode — Terminal tab (Navigate sub-mode, default):**

The Terminal tab supports multiple shells per agent shown as a label strip (`[1] [2*] [3]    [+]`). Default sub-mode is Navigate so keys are TUI commands, not shell input.

| Shortcut | Action |
|----------|--------|
| `Ctrl+\` | Toggle Type ↔ Navigate sub-mode |
| `Enter` | Enter Type sub-mode (shortcut into typing) |
| `]` | Switch to next terminal |
| `[` | Switch to previous terminal |
| `n` | Open a new terminal (and enter Type mode on it) |
| `x` | Close the current terminal (kills its PTY) |
| `Tab` | Cycle to next left panel tab |
| `Shift+PgUp` / `Shift+PgDn` | Scroll the active terminal's scrollback |
| Mouse: click `[N]` | Switch to terminal N |
| Mouse: click `[+]` | Spawn new terminal, enter Type mode |
| Mouse: click terminal area | Enter Type mode |
| Mouse: scroll over terminal | Scroll active terminal scrollback |
| Mouse: scroll over label strip | Cycle terminals |

**Focus mode — Terminal tab (Type sub-mode):**

| Shortcut | Action |
|----------|--------|
| `Ctrl+\` | Stop typing, return to Navigate sub-mode |
| All other keys | Forwarded to the active terminal's shell PTY |

All Terminal-tab shells are killed automatically when the owning agent exits, its worktree is removed, or its branch is deleted — no orphan processes left behind.
