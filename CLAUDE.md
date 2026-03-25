# Clust

Agent manager CLI built in Rust. Two crates: `clust-cli` (CLI binary) and `clust-pool` (background daemon).

## Quick Reference

- **Docs**: See `docs/` for architecture, commands, pool design, storage schema, and terminal UI
- **Storage**: `~/.clust/` (SQLite db + Unix domain socket)
- **Target**: macOS and Linux first, Windows later

## Build & Run

```bash
cargo build
cargo run --bin clust-cli
cargo run --bin clust-pool
cargo test
```

## Conventions

- CLI flags: POSIX/GNU style (short `-b`, long `--background`, kebab-case)
- CLI parsing: `clap`
- Async runtime: `tokio`
- Serialization: MessagePack (`rmp-serde`)
- Terminal rendering: `ratatui` + `crossterm`
