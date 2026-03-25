# CLI Commands Reference

## Naming Conventions

All flags follow POSIX/GNU conventions:

- **Short flags**: single dash, single letter (`-b`, `-s`)
- **Long flags**: double dash, kebab-case (`--background`, `--stop`)
- **Subcommands**: no dash, lowercase (`ls`, `ui`)
- **Flag values**: space-separated (`-a a3f8c1`, `--attach a3f8c1`)
- **Positional args**: no prefix (`clust "say hi"`)

Short and long forms are always provided together, except for destructive global flags (e.g. `--stop-pool`) which are long-only to prevent accidental use.

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
| `-s` | `--stop <ID>` | Stop a specific agent by its 6-char ID. |
| | `--stop-pool` | Stop the pool daemon and all running agents. |
| `-d` | `--default` | Interactive picker to set the global default agent binary. Persisted in SQLite. |
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

Output columns:

```
ID       AGENT    STATUS    STARTED       ATTACHED
a3f8c1   claude   running   2 min ago     1 terminal
b7e2d9   claude   running   15 min ago    0 terminals
```

### `clust ui`

Open the Clust terminal UI. (Future — not in v0.1)

```
clust ui
```

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
clust --stop-pool

# Show help
clust -h
```

---

## Keyboard Shortcuts (While Attached)

These shortcuts are active when the CLI is attached to an agent session. They are displayed in the bottom status bar.

| Shortcut | Action |
|----------|--------|
| `Ctrl+Q` | Detach from agent (agent keeps running in pool) |

Additional shortcuts TBD as features are added.
