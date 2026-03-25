# Storage

## File Layout

```
~/.clust/
├── clust.db        # SQLite database (config, defaults)
├── clust.sock      # Unix domain socket (IPC, runtime only)
└── logs/           # Optional: daemon logs (future)
```

The `~/.clust/` directory is created on first run if it doesn't exist.

## SQLite Database

**Path**: `~/.clust/clust.db`

Both `clust-cli` and `clust-pool` can read from this database. Only `clust-pool` writes to it (the CLI sends commands to the pool, which performs the writes).

### Schema

#### `config`

General key-value configuration store.

```sql
CREATE TABLE config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

**Known keys:**

| Key | Default | Description |
|-----|---------|-------------|
| `default_agent` | *(none)* | The agent binary to use when none is specified. Set via `clust -d` or first-run prompt. |

#### `agent_history` *(deferred — migration v2)*

Log of past agent sessions. Written when an agent exits. Useful for future features (UI history, analytics). Not created in migration v1; will be added as migration v2.

```sql
CREATE TABLE agent_history (
    id            TEXT PRIMARY KEY,  -- 6-char hex ID
    agent_binary  TEXT NOT NULL,
    prompt        TEXT,              -- initial prompt, if any
    working_dir   TEXT NOT NULL,
    started_at    TEXT NOT NULL,     -- ISO 8601
    ended_at      TEXT NOT NULL,     -- ISO 8601
    exit_code     INTEGER NOT NULL
);
```

### Migrations

Schema changes are managed with a simple version table:

```sql
CREATE TABLE schema_version (
    version INTEGER PRIMARY KEY
);
```

On startup, the pool checks `schema_version` and applies any pending migrations sequentially. This keeps the database forward-compatible as features are added.

## What Goes Where

| Data | Location | Lifetime |
|------|----------|----------|
| Default agent binary | SQLite `config` table | Persistent (survives restarts) |
| Agent session history | SQLite `agent_history` table | Persistent |
| Running agent state | Pool in-memory | Ephemeral (dies with pool) |
| IPC socket | `~/.clust/clust.sock` | Runtime only |
