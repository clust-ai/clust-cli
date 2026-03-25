# clust

A CLI tool for managing AI agents with session multiplexing, background execution, and a persistent agent pool.

Run multiple AI agents (Claude Code, etc.) concurrently in a background daemon. Attach, detach, and reattach to sessions from any terminal.

## Install

```bash
brew install clust-ai/tap/clust
```

Or build from source:

```bash
git clone https://github.com/clust-ai/clust-cli.git
cd clust-cli
cargo build --release
cp target/release/clust target/release/clust-pool ~/.clust/bin/
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
                 │   clust-pool   │  (background daemon)
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

The CLI is a thin client. All agent processes live in `clust-pool`, a single background daemon that starts automatically on first use. Multiple terminals can attach to the same agent simultaneously.

## Usage

```bash
# Start a new agent session and attach
clust

# Start with a prompt
clust "refactor the auth module"

# Start in the background
clust -b "run the test suite and fix failures"

# List running agents
clust ls

# Attach to a running agent
clust -a a3f8c1

# Set default agent binary
clust -d aider

# Stop the pool and all agents
clust -s
```

### Flags

| Flag | Long | Description |
|------|------|-------------|
| `-b` | `--background` | Start agent without attaching. Returns the agent ID. |
| `-a` | `--attach <ID>` | Attach to an existing agent by its 6-char ID. |
| `-s` | `--stop` | Stop the pool daemon and all running agents. |
| `-d` | `--default <AGENT>` | Set the default agent binary (e.g., `claude`, `aider`). |
| `-h` | `--help` | Show help. |
| `-V` | `--version` | Show version. |

### Keyboard Shortcuts (while attached)

| Shortcut | Action |
|----------|--------|
| `Ctrl+Q` | Detach from agent (agent keeps running) |

## Architecture

Three crates in a Cargo workspace:

| Crate | Description |
|-------|-------------|
| `clust-cli` | CLI binary users interact with. Installed as `clust`. |
| `clust-pool` | Background daemon. Manages agent lifecycles, PTYs, and IPC. |
| `clust-ipc` | Shared IPC message types and serialization. |

See [`docs/`](docs/) for detailed design docs covering [architecture](docs/architecture.md), [commands](docs/commands.md), [pool design](docs/pool.md), [storage schema](docs/storage.md), and [terminal UI](docs/terminal-ui.md).

## License

MIT
