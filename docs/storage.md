# Storage

## File Layout

```
~/.clust/
├── bin/            # Installed binaries (clust, clust-hub)
├── clust.db        # SQLite database (config, defaults)
├── clust.sock      # Unix domain socket (IPC, runtime only)
└── logs/           # Optional: daemon logs (future)
```

The `~/.clust/` directory is created on first run if it doesn't exist.

## SQLite Database

**Path**: `~/.clust/clust.db`

Both `clust-cli` and `clust-hub` can read from this database. Only `clust-hub` writes to it (the CLI sends commands to the hub, which performs the writes).

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
| `default_editor` | *(none)* | The preferred editor binary for opening repositories. Set via the "For all repositories" option in the editor remember modal. |

#### `repos` *(migration v2, extended in v3 and v4)*

Registered git repositories tracked in the TUI. Branch/worktree data is ephemeral (fetched from git on each poll), only the registration, color, and editor preference are persisted.

```sql
CREATE TABLE repos (
    path           TEXT PRIMARY KEY,  -- absolute path to repo root
    name           TEXT NOT NULL,     -- directory name
    registered_at  TEXT NOT NULL,     -- ISO 8601
    color          TEXT               -- repo color name (e.g., "red", "blue"); added in migration v3
    editor         TEXT               -- preferred editor binary (e.g., "code", "cursor"); added in migration v4
);
```

Repos are registered via `clust repo -a` or auto-registered when an agent is launched inside a git repository. Stale entries (deleted repos) are cleaned up automatically when the TUI polls for repo state.

Migration v3 adds the `color` column and backfills existing repos with cycling colors from the palette: `red`, `orange`, `yellow`, `lime`, `green`, `teal`, `blue`, `purple`, `pink`, `coral`.

Migration v4 adds the `editor` column for per-repository editor preferences. When set, the TUI skips the editor picker modal and opens the repository directly in the saved editor.

#### `agent_history` *(deferred — future migration)*

Log of past agent sessions. Written when an agent exits. Useful for future features (UI history, analytics). Not yet created.

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

On startup, the hub checks `schema_version` and applies any pending migrations sequentially. This keeps the database forward-compatible as features are added.

## What Goes Where

| Data | Location | Lifetime |
|------|----------|----------|
| Default agent binary | SQLite `config` table | Persistent (survives restarts) |
| Default editor binary | SQLite `config` table | Persistent (survives restarts) |
| Registered repositories | SQLite `repos` table | Persistent (auto-cleaned if path deleted) |
| Per-repo editor preference | SQLite `repos` table (`editor` column) | Persistent (survives restarts) |
| Agent session history | SQLite `agent_history` table | Persistent *(not yet implemented)* |
| Running agent state | Hub in-memory | Ephemeral (dies with hub) |
| Repository branch/worktree data | Fetched from git on demand | Ephemeral (never persisted) |
| IPC socket | `~/.clust/clust.sock` | Runtime only |
