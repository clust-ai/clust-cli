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
| `-s` | `--stop [ID]` | Without a value: stop the pool daemon and all agents. With a 6-char ID: stop that specific agent. |
| `-d` | `--default` | Interactive picker to set the global default agent binary. Persisted in SQLite. |
| `-u` | `--use <AGENT>` | Use a specific agent binary for this session only (does not change the default). |
| `-e` | `--accept-edits` | Auto-accept edits. Agent-specific: for Claude, passes `--permission-mode acceptEdits`. Ignored for agents that don't support it. |
| `-p` | `--pool <NAME>` | Assign the agent to a named pool (snake_case; default: `default_pool`). Pools are logical groupings within the single pool process. |
| `-h` | `--help` | Show help with all available options. |
| `-V` | `--version` | Show version. |

### `clust ls`

List all running agents in the pool.

```
clust ls [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-i` | `--select` | Interactive selector: navigate with arrow keys, Enter to attach or start a new agent. |
| `-p` | `--pool <NAME>` | Filter agents by pool name. Without this flag, agents are grouped by pool. |

Output (no filter — grouped by pool):

```
  default_pool
  ID       AGENT        STATUS     STARTED        ATTACHED
  a3f8c1   claude       running    14:32          1 terminal
  b7e2d9   claude       running    14:17          0 terminals

  my_feature
  ID       AGENT        STATUS     STARTED        ATTACHED
  c4d5e6   aider        running    15:01          1 terminal
```

Output (with `-p` filter — flat list):

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

### `clust repo`

Repository management.

```
clust repo [OPTIONS]
```

| Flag | Long | Description |
|------|------|-------------|
| `-R` | `--register` | Register the current directory's git repository for tracking in the TUI. |
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

# Start an agent in a named pool
clust -p my_feature "refactor auth"

# List agents grouped by pool
clust ls

# List only agents in a specific pool
clust ls -p my_feature

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

# Stop the pool and all agents
clust -s

# Register the current repo for TUI tracking
clust repo -R
clust repo --register

# Remove a repo from tracking (stops agents, prompts for confirmation)
clust repo --remove
clust repo -r

# Stop all agents in the current repo (keeps repo tracked)
clust repo --stop
clust repo -s

# Open terminal UI
clust ui
clust .

# Show help
clust -h
```

---

## Keyboard Shortcuts (While Attached)

These shortcuts are active when the CLI is attached to an agent session. They are displayed in the bottom status bar.

| Shortcut | Action |
|----------|--------|
| `Ctrl+Q` | Detach from agent (agent keeps running in pool) |

## Keyboard Shortcuts (Terminal UI)

These shortcuts are active in the `clust ui` dashboard. They are displayed in the bottom status bar.

| Shortcut | Action |
|----------|--------|
| `q` / `Esc` | Quit the UI (pool keeps running) |
| `Q` | Quit the UI and stop the pool |
| `↑` / `↓` | Move selection within current level |
| `→` | Descend into selected item, or expand if collapsed |
| `←` | Collapse current item, or ascend to parent level |
| `Enter` | Toggle collapse/expand on repos and categories |
| `Shift+←` / `Shift+→` | Switch focus between left and right panels |
| `Tab` | Switch to next tab |
| `Shift+Tab` | Switch to previous tab |
