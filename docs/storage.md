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
| `bypass_permissions` | `false` | When `true`, all new agents that support it are started with permission-bypassing args (e.g., `--dangerously-skip-permissions` for Claude). Set via `clust bypass --on/--off` or Alt+B in the TUI. |

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

#### `queued_batches` *(migration v5, extended in v6, v7, v8)*

Batches persisted by the hub daemon. Includes idle batches (registered from the CLI Tasks tab), scheduled batches (with a timer), and running batches. Persisted so batches survive hub restarts.

```sql
CREATE TABLE queued_batches (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    repo_path       TEXT NOT NULL,
    target_branch   TEXT NOT NULL,
    max_concurrent  INTEGER,
    prompt_prefix   TEXT,
    prompt_suffix   TEXT,
    plan_mode       INTEGER NOT NULL DEFAULT 0,
    allow_bypass    INTEGER NOT NULL DEFAULT 0,
    agent_binary    TEXT,
    hub             TEXT NOT NULL,
    scheduled_at    TEXT,                -- RFC 3339 timestamp; NULL for idle batches
    status          TEXT NOT NULL DEFAULT 'scheduled',  -- idle, scheduled, running, completed
    created_at      TEXT NOT NULL,       -- RFC 3339 timestamp
    launch_mode     TEXT NOT NULL DEFAULT 'auto',  -- auto or manual; added in migration v7
    depends_on      TEXT NOT NULL DEFAULT '[]'    -- JSON array of hub batch IDs; added in migration v8
);
```

Migration v7 adds the `launch_mode` column (with default `'auto'`) and makes `scheduled_at` effectively nullable for idle batches that have no scheduled start time.

Migration v8 adds the `depends_on` column which stores a JSON array of hub batch IDs that this batch depends on. When all dependency batches complete (or are no longer present), the hub's batch timer auto-starts the dependent batch by transitioning it from Idle to Running.

#### `queued_batch_tasks` *(migration v5, extended in v6)*

Individual tasks within a queued batch. Each task maps to an agent that will be spawned in a worktree.

```sql
CREATE TABLE queued_batch_tasks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id    TEXT NOT NULL REFERENCES queued_batches(id) ON DELETE CASCADE,
    task_index  INTEGER NOT NULL,
    branch_name TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'idle',  -- idle, active, done
    agent_id    TEXT,                           -- set when the task's agent is started
    use_prefix  INTEGER NOT NULL DEFAULT 1,    -- per-task prefix toggle (1 = apply, 0 = skip); added in migration v6
    use_suffix  INTEGER NOT NULL DEFAULT 1     -- per-task suffix toggle (1 = apply, 0 = skip); added in migration v6
);
```

Migration v6 adds `use_prefix` and `use_suffix` columns for per-task control over whether the batch's prompt prefix and suffix are applied when building the full prompt. Both default to 1 (true) for backward compatibility.

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
| Bypass permissions toggle | SQLite `config` table | Persistent (survives restarts) |
| Registered repositories | SQLite `repos` table | Persistent (auto-cleaned if path deleted) |
| Per-repo editor preference | SQLite `repos` table (`editor` column) | Persistent (survives restarts) |
| Agent session history | SQLite `agent_history` table | Persistent *(not yet implemented)* |
| Queued batches | SQLite `queued_batches` + `queued_batch_tasks` tables | Persistent (survives restarts, loaded on startup) |
| Running agent state | Hub in-memory | Ephemeral (dies with hub) |
| Repository branch/worktree data | Fetched from git on demand | Ephemeral (never persisted) |
| IPC socket | `~/.clust/clust.sock` | Runtime only |
