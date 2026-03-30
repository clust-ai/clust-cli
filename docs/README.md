# Clust — Agent Manager CLI

Clust is a CLI tool for managing AI agents (Claude Code, etc.) with session multiplexing, background execution, and a persistent agent hub.

## Crates

| Crate | Type | Description |
|-------|------|-------------|
| `clust-cli` | Binary | The CLI users interact with. Installed as `clust`. |
| `clust-hub` | Binary | Background daemon. Manages agent lifecycles, PTYs, and IPC. Installed alongside `clust-cli`. |
| `clust-ipc` | Library | Shared IPC types (message enums, framing, socket path helpers). Used by both binaries. |

## Documentation

- [Architecture](./architecture.md) — System design, crate responsibilities, IPC
- [Commands](./commands.md) — CLI reference with all flags and subcommands
- [Hub](./hub.md) — Daemon lifecycle, agent management, PTY handling
- [Storage](./storage.md) — SQLite schema, config, file layout (`~/.clust/`)
- [Terminal UI](./terminal-ui.md) — Rendering, bottom bar, attach/detach behavior
- [Terminal Multiplexing](./terminal-multiplexing.md) — I/O forwarding, filter chain, input conventions

## Design Principles

1. **Agents are hub-managed** — Agents always run inside `clust-hub`, never in the user's terminal process. The CLI attaches/detaches to agent PTY output.
2. **One hub, many agents** — A single `clust-hub` daemon per machine. Multiple agents run inside it concurrently.
3. **Ephemeral hub, persistent config** — Hub state lives in memory and dies with the daemon. Configuration and defaults persist in SQLite at `~/.clust/`.
4. **Cross-platform path** — macOS and Linux first, Windows support planned. Architecture choices (IPC, PTY) account for this.
5. **Minimal footprint** — The hub is a lightweight background process. No unnecessary resource usage when idle.
