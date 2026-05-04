use std::path::PathBuf;

use rusqlite::Connection;

/// A row from the repos table: (path, name, color, editor).
pub type RepoRow = (String, String, Option<String>, Option<String>);

/// Returns the database path: `~/.clust/clust.db`.
fn db_path() -> PathBuf {
    clust_ipc::clust_dir().join("clust.db")
}

/// Open (or create) the SQLite database and run any pending migrations.
pub fn open_or_create() -> Result<Connection, String> {
    let path = db_path();
    let conn = Connection::open(&path).map_err(|e| format!("failed to open database: {e}"))?;

    // Enable WAL mode for better concurrent read performance
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("failed to set journal mode: {e}"))?;

    // Ensure schema_version table exists
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY
        );",
    )
    .map_err(|e| format!("failed to create schema_version table: {e}"))?;

    run_migrations(&conn)?;
    Ok(conn)
}

/// Check current schema version and apply pending migrations.
fn run_migrations(conn: &Connection) -> Result<(), String> {
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("failed to read schema version: {e}"))?;

    if current_version < 1 {
        migrate_v1(conn)?;
    }
    if current_version < 2 {
        migrate_v2(conn)?;
    }
    if current_version < 3 {
        migrate_v3(conn)?;
    }
    if current_version < 4 {
        migrate_v4(conn)?;
    }
    if current_version < 5 {
        migrate_v5(conn)?;
    }
    if current_version < 6 {
        migrate_v6(conn)?;
    }
    if current_version < 7 {
        migrate_v7(conn)?;
    }
    if current_version < 8 {
        migrate_v8(conn)?;
    }
    if current_version < 9 {
        migrate_v9(conn)?;
    }
    if current_version < 10 {
        migrate_v10(conn)?;
    }
    if current_version < 11 {
        migrate_v11(conn)?;
    }

    Ok(())
}

/// Migration v1: create the config table.
fn migrate_v1(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS config (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO schema_version (version) VALUES (1);",
    )
    .map_err(|e| format!("migration v1 failed: {e}"))
}

/// Migration v2: create the repos table.
fn migrate_v2(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repos (
            path           TEXT PRIMARY KEY,
            name           TEXT NOT NULL,
            registered_at  TEXT NOT NULL
        );
        INSERT INTO schema_version (version) VALUES (2);",
    )
    .map_err(|e| format!("migration v2 failed: {e}"))
}

/// Migration v3: add color column to repos table.
fn migrate_v3(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE repos ADD COLUMN color TEXT;
         INSERT INTO schema_version (version) VALUES (3);",
    )
    .map_err(|e| format!("migration v3 failed: {e}"))?;

    // Backfill existing repos with cycling colors
    let mut stmt = conn
        .prepare("SELECT path FROM repos ORDER BY name")
        .map_err(|e| format!("migration v3 backfill prepare failed: {e}"))?;
    let paths: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("migration v3 backfill query failed: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    for (i, path) in paths.iter().enumerate() {
        let color = REPO_COLORS[i % REPO_COLORS.len()];
        conn.execute(
            "UPDATE repos SET color = ?1 WHERE path = ?2",
            rusqlite::params![color, path],
        )
        .map_err(|e| format!("migration v3 backfill update failed: {e}"))?;
    }
    Ok(())
}

/// Migration v4: add editor column to repos table.
fn migrate_v4(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE repos ADD COLUMN editor TEXT;
         INSERT INTO schema_version (version) VALUES (4);",
    )
    .map_err(|e| format!("migration v4 failed: {e}"))?;
    Ok(())
}

/// Migration v5: create queued_batches and queued_batch_tasks tables.
fn migrate_v5(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS queued_batches (
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
            scheduled_at    TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'scheduled',
            created_at      TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS queued_batch_tasks (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            batch_id    TEXT NOT NULL REFERENCES queued_batches(id) ON DELETE CASCADE,
            task_index  INTEGER NOT NULL,
            branch_name TEXT NOT NULL,
            prompt      TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'idle',
            agent_id    TEXT,
            UNIQUE(batch_id, task_index)
        );
        INSERT INTO schema_version (version) VALUES (5);",
    )
    .map_err(|e| format!("migration v5 failed: {e}"))
}

/// Migration v6: add per-task prefix/suffix flags to queued_batch_tasks.
fn migrate_v6(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE queued_batch_tasks ADD COLUMN use_prefix INTEGER NOT NULL DEFAULT 1;
         ALTER TABLE queued_batch_tasks ADD COLUMN use_suffix INTEGER NOT NULL DEFAULT 1;
         INSERT INTO schema_version (version) VALUES (6);",
    )
    .map_err(|e| format!("migration v6 failed: {e}"))
}

/// Migration v7: add launch_mode column and allow nullable scheduled_at for idle batches.
fn migrate_v7(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE queued_batches ADD COLUMN launch_mode TEXT NOT NULL DEFAULT 'auto';
         INSERT INTO schema_version (version) VALUES (7);",
    )
    .map_err(|e| format!("migration v7 failed: {e}"))
}

/// Migration v8: add depends_on column for batch-to-batch dependencies.
fn migrate_v8(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE queued_batches ADD COLUMN depends_on TEXT NOT NULL DEFAULT '[]';
         INSERT INTO schema_version (version) VALUES (8);",
    )
    .map_err(|e| format!("migration v8 failed: {e}"))
}

/// Migration v9: add per-task plan_mode flag to queued_batch_tasks.
fn migrate_v9(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE queued_batch_tasks ADD COLUMN plan_mode INTEGER NOT NULL DEFAULT 0;
         INSERT INTO schema_version (version) VALUES (9);",
    )
    .map_err(|e| format!("migration v9 failed: {e}"))
}

/// Migration v10: add is_manager flag to queued_batch_tasks. Manager tasks are
/// auto-injected by the orchestrator import flow and do not consume a
/// `max_concurrent` slot.
fn migrate_v10(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "ALTER TABLE queued_batch_tasks ADD COLUMN is_manager INTEGER NOT NULL DEFAULT 0;
         INSERT INTO schema_version (version) VALUES (10);",
    )
    .map_err(|e| format!("migration v10 failed: {e}"))
}

/// Migration v11: add `orchestrator_id` and `orchestrator_batch_file` columns
/// to `queued_batches`, and make `scheduled_at` nullable (idle/orchestrator
/// batches don't have a scheduled time). Dropping NOT NULL requires table
/// recreation in SQLite; foreign keys are off in this connection, so the
/// `queued_batch_tasks` FK survives the drop+rename.
fn migrate_v11(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE queued_batches_new (
             id                      TEXT PRIMARY KEY,
             title                   TEXT NOT NULL,
             repo_path               TEXT NOT NULL,
             target_branch           TEXT NOT NULL,
             max_concurrent          INTEGER,
             prompt_prefix           TEXT,
             prompt_suffix           TEXT,
             plan_mode               INTEGER NOT NULL DEFAULT 0,
             allow_bypass            INTEGER NOT NULL DEFAULT 0,
             agent_binary            TEXT,
             hub                     TEXT NOT NULL,
             scheduled_at            TEXT,
             status                  TEXT NOT NULL DEFAULT 'scheduled',
             created_at              TEXT NOT NULL,
             launch_mode             TEXT NOT NULL DEFAULT 'auto',
             depends_on              TEXT NOT NULL DEFAULT '[]',
             orchestrator_id         TEXT,
             orchestrator_batch_file TEXT
         );
         INSERT INTO queued_batches_new (id, title, repo_path, target_branch,
             max_concurrent, prompt_prefix, prompt_suffix, plan_mode, allow_bypass,
             agent_binary, hub, scheduled_at, status, created_at, launch_mode,
             depends_on, orchestrator_id, orchestrator_batch_file)
         SELECT id, title, repo_path, target_branch,
             max_concurrent, prompt_prefix, prompt_suffix, plan_mode, allow_bypass,
             agent_binary, hub, scheduled_at, status, created_at, launch_mode,
             depends_on, NULL, NULL
         FROM queued_batches;
         DROP TABLE queued_batches;
         ALTER TABLE queued_batches_new RENAME TO queued_batches;
         INSERT INTO schema_version (version) VALUES (11);
         COMMIT;",
    )
    .map_err(|e| format!("migration v11 failed: {e}"))
}

// ---------------------------------------------------------------------------
// Queued batch CRUD
// ---------------------------------------------------------------------------

/// A row from the queued_batches table.
pub struct QueuedBatchRow {
    pub id: String,
    pub title: String,
    pub repo_path: String,
    pub target_branch: String,
    pub max_concurrent: Option<usize>,
    pub prompt_prefix: Option<String>,
    pub prompt_suffix: Option<String>,
    pub plan_mode: bool,
    pub allow_bypass: bool,
    pub agent_binary: Option<String>,
    pub hub: String,
    pub scheduled_at: Option<String>,
    pub status: String,
    pub launch_mode: String,
    pub depends_on: String,
    /// Orchestrator id that produced this batch (NULL for non-orchestrator batches).
    pub orchestrator_id: Option<String>,
    /// JSON filename inside the orchestrator inbox (e.g. "batch-001.json").
    pub orchestrator_batch_file: Option<String>,
}

/// A row from the queued_batch_tasks table.
pub struct QueuedBatchTaskRow {
    pub task_index: usize,
    pub branch_name: String,
    pub prompt: String,
    pub status: String,
    pub agent_id: Option<String>,
    pub use_prefix: bool,
    pub use_suffix: bool,
    pub plan_mode: bool,
    pub is_manager: bool,
}

/// One task to insert: (branch_name, prompt, use_prefix, use_suffix, plan_mode, is_manager).
pub type InsertTaskRow = (String, String, bool, bool, bool, bool);

/// Insert a new queued batch and its tasks.
pub fn insert_queued_batch(
    conn: &Connection,
    batch: &QueuedBatchRow,
    tasks: &[InsertTaskRow],
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO queued_batches (id, title, repo_path, target_branch, max_concurrent,
         prompt_prefix, prompt_suffix, plan_mode, allow_bypass, agent_binary, hub,
         scheduled_at, status, created_at, launch_mode, depends_on,
         orchestrator_id, orchestrator_batch_file)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        rusqlite::params![
            batch.id,
            batch.title,
            batch.repo_path,
            batch.target_branch,
            batch.max_concurrent.map(|v| v as i64),
            batch.prompt_prefix,
            batch.prompt_suffix,
            batch.plan_mode as i32,
            batch.allow_bypass as i32,
            batch.agent_binary,
            batch.hub,
            batch.scheduled_at,
            batch.status,
            chrono::Utc::now().to_rfc3339(),
            batch.launch_mode,
            batch.depends_on,
            batch.orchestrator_id,
            batch.orchestrator_batch_file,
        ],
    )
    .map_err(|e| format!("failed to insert queued batch: {e}"))?;

    for (i, (branch_name, prompt, use_prefix, use_suffix, plan_mode, is_manager)) in
        tasks.iter().enumerate()
    {
        conn.execute(
            "INSERT INTO queued_batch_tasks (batch_id, task_index, branch_name, prompt, status, use_prefix, use_suffix, plan_mode, is_manager)
             VALUES (?1, ?2, ?3, ?4, 'idle', ?5, ?6, ?7, ?8)",
            rusqlite::params![batch.id, i as i64, branch_name, prompt, *use_prefix as i32, *use_suffix as i32, *plan_mode as i32, *is_manager as i32],
        )
        .map_err(|e| format!("failed to insert queued batch task: {e}"))?;
    }
    Ok(())
}

/// Load all non-completed queued batches with their tasks.
pub fn load_queued_batches(
    conn: &Connection,
) -> Result<Vec<(QueuedBatchRow, Vec<QueuedBatchTaskRow>)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, title, repo_path, target_branch, max_concurrent,
                    prompt_prefix, prompt_suffix, plan_mode, allow_bypass,
                    agent_binary, hub, scheduled_at, status, launch_mode, depends_on,
                    orchestrator_id, orchestrator_batch_file
             FROM queued_batches
             WHERE status IN ('idle', 'scheduled', 'running')
             ORDER BY rowid",
        )
        .map_err(|e| format!("failed to prepare queued batch query: {e}"))?;

    let batches: Vec<QueuedBatchRow> = stmt
        .query_map([], |row| {
            Ok(QueuedBatchRow {
                id: row.get(0)?,
                title: row.get(1)?,
                repo_path: row.get(2)?,
                target_branch: row.get(3)?,
                max_concurrent: row.get::<_, Option<i64>>(4)?.map(|v| v as usize),
                prompt_prefix: row.get(5)?,
                prompt_suffix: row.get(6)?,
                plan_mode: row.get::<_, i32>(7)? != 0,
                allow_bypass: row.get::<_, i32>(8)? != 0,
                agent_binary: row.get(9)?,
                hub: row.get(10)?,
                scheduled_at: row.get(11)?,
                status: row.get(12)?,
                launch_mode: row
                    .get::<_, Option<String>>(13)?
                    .unwrap_or_else(|| "auto".to_string()),
                depends_on: row
                    .get::<_, Option<String>>(14)?
                    .unwrap_or_else(|| "[]".to_string()),
                orchestrator_id: row.get(15)?,
                orchestrator_batch_file: row.get(16)?,
            })
        })
        .map_err(|e| format!("failed to query queued batches: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to collect queued batches: {e}"))?;

    let mut result = Vec::new();
    for batch in batches {
        let tasks = load_batch_tasks(conn, &batch.id)?;
        result.push((batch, tasks));
    }
    Ok(result)
}

/// Load tasks for a specific batch.
fn load_batch_tasks(conn: &Connection, batch_id: &str) -> Result<Vec<QueuedBatchTaskRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT task_index, branch_name, prompt, status, agent_id, use_prefix, use_suffix, plan_mode, is_manager
             FROM queued_batch_tasks
             WHERE batch_id = ?1
             ORDER BY task_index",
        )
        .map_err(|e| format!("failed to prepare task query: {e}"))?;

    let rows: Vec<QueuedBatchTaskRow> = stmt
        .query_map([batch_id], |row| {
            Ok(QueuedBatchTaskRow {
                task_index: row.get::<_, i64>(0)? as usize,
                branch_name: row.get(1)?,
                prompt: row.get(2)?,
                status: row.get(3)?,
                agent_id: row.get(4)?,
                use_prefix: row.get::<_, i32>(5)? != 0,
                use_suffix: row.get::<_, i32>(6)? != 0,
                plan_mode: row.get::<_, i32>(7)? != 0,
                is_manager: row.get::<_, i32>(8)? != 0,
            })
        })
        .map_err(|e| format!("failed to query tasks: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to collect tasks: {e}"))?;
    Ok(rows)
}

/// Update the status of a queued batch.
pub fn update_batch_status(conn: &Connection, id: &str, status: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE queued_batches SET status = ?1 WHERE id = ?2",
        rusqlite::params![status, id],
    )
    .map_err(|e| format!("failed to update batch status: {e}"))?;
    Ok(())
}

/// Update a task's status and agent_id within a queued batch.
pub fn update_task_status(
    conn: &Connection,
    batch_id: &str,
    task_index: usize,
    status: &str,
    agent_id: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE queued_batch_tasks SET status = ?1, agent_id = ?2
         WHERE batch_id = ?3 AND task_index = ?4",
        rusqlite::params![status, agent_id, batch_id, task_index as i64],
    )
    .map_err(|e| format!("failed to update task status: {e}"))?;
    Ok(())
}

/// Add a task to an existing batch. Returns the assigned task_index.
pub fn add_batch_task(
    conn: &Connection,
    batch_id: &str,
    branch_name: &str,
    prompt: &str,
) -> Result<usize, String> {
    let max_index: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(task_index), -1) FROM queued_batch_tasks WHERE batch_id = ?1",
            [batch_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("failed to query max task index: {e}"))?;
    let new_index = (max_index + 1) as usize;
    conn.execute(
        "INSERT INTO queued_batch_tasks (batch_id, task_index, branch_name, prompt, status)
         VALUES (?1, ?2, ?3, ?4, 'idle')",
        rusqlite::params![batch_id, new_index as i64, branch_name, prompt],
    )
    .map_err(|e| format!("failed to insert batch task: {e}"))?;
    Ok(new_index)
}

/// Update batch configuration fields.
pub fn update_batch_config(
    conn: &Connection,
    id: &str,
    prompt_prefix: Option<&str>,
    prompt_suffix: Option<&str>,
    plan_mode: bool,
    allow_bypass: bool,
) -> Result<(), String> {
    conn.execute(
        "UPDATE queued_batches SET prompt_prefix = ?1, prompt_suffix = ?2,
         plan_mode = ?3, allow_bypass = ?4 WHERE id = ?5",
        rusqlite::params![
            prompt_prefix,
            prompt_suffix,
            plan_mode as i32,
            allow_bypass as i32,
            id
        ],
    )
    .map_err(|e| format!("failed to update batch config: {e}"))?;
    Ok(())
}

/// Update the depends_on list of a batch.
pub fn update_batch_depends_on(
    conn: &Connection,
    id: &str,
    depends_on: &[String],
) -> Result<(), String> {
    let json = serde_json::to_string(depends_on)
        .map_err(|e| format!("failed to serialize depends_on: {e}"))?;
    conn.execute(
        "UPDATE queued_batches SET depends_on = ?1 WHERE id = ?2",
        rusqlite::params![json, id],
    )
    .map_err(|e| format!("failed to update batch depends_on: {e}"))?;
    Ok(())
}

/// Remove done tasks from a batch, re-index remaining tasks.
pub fn remove_done_batch_tasks(conn: &Connection, batch_id: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM queued_batch_tasks WHERE batch_id = ?1 AND status = 'done'",
        [batch_id],
    )
    .map_err(|e| format!("failed to remove done tasks: {e}"))?;
    // Re-index remaining tasks
    let mut stmt = conn
        .prepare("SELECT id FROM queued_batch_tasks WHERE batch_id = ?1 ORDER BY task_index")
        .map_err(|e| format!("failed to prepare re-index query: {e}"))?;
    let ids: Vec<i64> = stmt
        .query_map([batch_id], |row| row.get(0))
        .map_err(|e| format!("re-index query failed: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    for (new_idx, row_id) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE queued_batch_tasks SET task_index = ?1 WHERE id = ?2",
            rusqlite::params![new_idx as i64, row_id],
        )
        .map_err(|e| format!("re-index update failed: {e}"))?;
    }
    Ok(())
}

/// Delete a queued batch and its tasks (cascade).
pub fn delete_queued_batch(conn: &Connection, id: &str) -> Result<(), String> {
    // Delete tasks first (SQLite foreign key cascade may not be enabled)
    conn.execute("DELETE FROM queued_batch_tasks WHERE batch_id = ?1", [id])
        .map_err(|e| format!("failed to delete batch tasks: {e}"))?;
    conn.execute("DELETE FROM queued_batches WHERE id = ?1", [id])
        .map_err(|e| format!("failed to delete batch: {e}"))?;
    Ok(())
}

/// Titles + source filenames of batches already imported under the given
/// orchestrator id. Used by the inbox watcher to dedup on re-import after a
/// mid-import crash, and to keep the validator from flagging "title collides
/// with existing hub batch" against the orchestrator's own earlier inserts.
pub fn imported_batches_for_orch(
    conn: &Connection,
    orch_id: &str,
) -> Result<ImportedBatches, String> {
    let mut stmt = conn
        .prepare(
            "SELECT title, orchestrator_batch_file FROM queued_batches \
             WHERE orchestrator_id = ?1",
        )
        .map_err(|e| format!("prepare imported_batches_for_orch: {e}"))?;
    let mut titles = std::collections::HashSet::new();
    let mut files = std::collections::HashSet::new();
    let rows = stmt
        .query_map([orch_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| format!("query imported_batches_for_orch: {e}"))?;
    for r in rows.flatten() {
        titles.insert(r.0);
        if let Some(f) = r.1 {
            files.insert(f);
        }
    }
    Ok(ImportedBatches { titles, files })
}

/// Output of `imported_batches_for_orch`: existing titles + source filenames.
#[derive(Default)]
pub struct ImportedBatches {
    pub titles: std::collections::HashSet<String>,
    pub files: std::collections::HashSet<String>,
}

/// Look up the orchestrator-source pair for a batch: `(orchestrator_id, batch_file)`.
/// Returns `Ok(None)` if the batch has no orchestrator origin or doesn't exist.
pub fn get_batch_source(
    conn: &Connection,
    batch_id: &str,
) -> Result<Option<(String, String)>, String> {
    let row: rusqlite::Result<(Option<String>, Option<String>)> = conn.query_row(
        "SELECT orchestrator_id, orchestrator_batch_file FROM queued_batches WHERE id = ?1",
        [batch_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );
    match row {
        Ok((Some(id), Some(file))) => Ok(Some((id, file))),
        Ok(_) => Ok(None),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(format!("failed to query batch source: {e}")),
    }
}

/// Available colors for repository identification.
pub const REPO_COLORS: &[&str] = &[
    "red", "orange", "yellow", "lime", "green", "teal", "blue", "purple", "pink", "coral",
];

/// Register a repository path with a color. Silently ignores duplicates.
pub fn register_repo(conn: &Connection, path: &str, name: &str, color: &str) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO repos (path, name, registered_at, color) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![path, name, now, color],
    )
    .map_err(|e| format!("failed to register repo: {e}"))?;
    Ok(())
}

/// List all registered repositories, ordered by name. Returns (path, name, color, editor).
pub fn list_repos(conn: &Connection) -> Result<Vec<RepoRow>, String> {
    let mut stmt = conn
        .prepare("SELECT path, name, color, editor FROM repos ORDER BY name")
        .map_err(|e| format!("failed to prepare repo query: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|e| format!("failed to query repos: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to collect repos: {e}"))
}

/// Check if a repository path is already registered.
pub fn is_repo_registered(conn: &Connection, path: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM repos WHERE path = ?1",
        [path],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

/// Remove a repository registration.
pub fn unregister_repo(conn: &Connection, path: &str) -> Result<(), String> {
    conn.execute("DELETE FROM repos WHERE path = ?1", [path])
        .map_err(|e| format!("failed to unregister repo: {e}"))?;
    Ok(())
}

/// Pick the next color for a new repo (cycles through the palette).
pub fn next_repo_color(conn: &Connection) -> &'static str {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))
        .unwrap_or(0);
    REPO_COLORS[count as usize % REPO_COLORS.len()]
}

/// Update the color of an existing repository.
pub fn set_repo_color(conn: &Connection, path: &str, color: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE repos SET color = ?1 WHERE path = ?2",
        rusqlite::params![color, path],
    )
    .map_err(|e| format!("failed to set repo color: {e}"))?;
    Ok(())
}

/// Update the preferred editor for an existing repository.
pub fn set_repo_editor(conn: &Connection, path: &str, editor: &str) -> Result<(), String> {
    conn.execute(
        "UPDATE repos SET editor = ?1 WHERE path = ?2",
        rusqlite::params![editor, path],
    )
    .map_err(|e| format!("failed to set repo editor: {e}"))?;
    Ok(())
}

/// Set the global default editor in the config table.
pub fn set_default_editor(conn: &Connection, editor: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES ('default_editor', ?1)",
        [editor],
    )
    .map_err(|e| format!("failed to set default editor: {e}"))?;
    Ok(())
}

/// Read the global default editor from the config table.
pub fn get_default_editor(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM config WHERE key = 'default_editor'",
        [],
        |row| row.get(0),
    )
    .ok()
}

/// Find a registered repository by name. Returns an error if multiple repos share the name.
pub fn find_repo_by_name(conn: &Connection, name: &str) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare("SELECT path FROM repos WHERE name = ?1")
        .map_err(|e| format!("failed to prepare repo query: {e}"))?;
    let paths: Vec<String> = stmt
        .query_map([name], |row| row.get::<_, String>(0))
        .map_err(|e| format!("failed to query repo: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to read repo: {e}"))?;

    match paths.len() {
        0 => Ok(None),
        1 => Ok(Some(paths.into_iter().next().unwrap())),
        _ => Err(format!(
            "multiple repos named '{name}'; use the full path instead"
        )),
    }
}

/// Read the default agent from the config table. Returns `None` if not set.
pub fn get_default_agent(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM config WHERE key = 'default_agent'",
        [],
        |row| row.get(0),
    )
    .ok()
}

/// Read the bypass-permissions flag from the config table. Defaults to `false`.
pub fn get_bypass_permissions(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT value FROM config WHERE key = 'bypass_permissions'",
        [],
        |row| row.get::<_, String>(0),
    )
    .map(|v| v == "true")
    .unwrap_or(false)
}

/// Set (or update) the bypass-permissions flag in the config table.
pub fn set_bypass_permissions(conn: &Connection, enabled: bool) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES ('bypass_permissions', ?1)",
        [if enabled { "true" } else { "false" }],
    )
    .map_err(|e| format!("failed to set bypass permissions: {e}"))?;
    Ok(())
}

/// Set (or update) the default agent in the config table.
pub fn set_default_agent(conn: &Connection, binary: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO config (key, value) VALUES ('default_agent', ?1)",
        [binary],
    )
    .map_err(|e| format!("failed to set default agent: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );",
        )
        .unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn creates_tables() {
        let conn = in_memory_db();
        // Verify config table exists by querying it
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM config", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn fresh_db_returns_none() {
        let conn = in_memory_db();
        assert_eq!(get_default_agent(&conn), None);
    }

    #[test]
    fn fresh_db_bypass_permissions_is_false() {
        let conn = in_memory_db();
        assert!(!get_bypass_permissions(&conn));
    }

    #[test]
    fn set_and_get_bypass_permissions() {
        let conn = in_memory_db();
        set_bypass_permissions(&conn, true).unwrap();
        assert!(get_bypass_permissions(&conn));
    }

    #[test]
    fn set_bypass_permissions_toggles() {
        let conn = in_memory_db();
        set_bypass_permissions(&conn, true).unwrap();
        assert!(get_bypass_permissions(&conn));
        set_bypass_permissions(&conn, false).unwrap();
        assert!(!get_bypass_permissions(&conn));
    }

    #[test]
    fn set_and_get_default() {
        let conn = in_memory_db();
        set_default_agent(&conn, "claude").unwrap();
        assert_eq!(get_default_agent(&conn), Some("claude".to_string()));
    }

    #[test]
    fn set_overwrites() {
        let conn = in_memory_db();
        set_default_agent(&conn, "claude").unwrap();
        set_default_agent(&conn, "opencode").unwrap();
        assert_eq!(get_default_agent(&conn), Some("opencode".to_string()));
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = in_memory_db();
        // Running migrations again should not error
        run_migrations(&conn).unwrap();
        // Still works
        set_default_agent(&conn, "aider").unwrap();
        assert_eq!(get_default_agent(&conn), Some("aider".to_string()));
    }

    #[test]
    fn set_default_persists_across_reads() {
        let conn = in_memory_db();
        set_default_agent(&conn, "opencode").unwrap();
        // Multiple reads should return the same value
        assert_eq!(get_default_agent(&conn), Some("opencode".to_string()));
        assert_eq!(get_default_agent(&conn), Some("opencode".to_string()));
    }

    #[test]
    fn set_default_overwrites_previous() {
        let conn = in_memory_db();
        set_default_agent(&conn, "claude").unwrap();
        assert_eq!(get_default_agent(&conn), Some("claude".to_string()));
        set_default_agent(&conn, "aider").unwrap();
        assert_eq!(get_default_agent(&conn), Some("aider".to_string()));
        // Old value is gone
        assert_ne!(get_default_agent(&conn), Some("claude".to_string()));
    }

    // ── Repo CRUD tests ──────────────────────────────────────────

    #[test]
    fn creates_repos_table() {
        let conn = in_memory_db();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn register_and_list_repo() {
        let conn = in_memory_db();
        register_repo(&conn, "/home/user/project", "project", "blue").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].0, "/home/user/project");
        assert_eq!(repos[0].1, "project");
    }

    #[test]
    fn register_duplicate_is_noop() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo", "green").unwrap();
        register_repo(&conn, "/tmp/repo", "repo", "green").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn is_repo_registered_true_false() {
        let conn = in_memory_db();
        assert!(!is_repo_registered(&conn, "/tmp/repo"));
        register_repo(&conn, "/tmp/repo", "repo", "green").unwrap();
        assert!(is_repo_registered(&conn, "/tmp/repo"));
    }

    #[test]
    fn unregister_repo_removes_entry() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo", "green").unwrap();
        assert!(is_repo_registered(&conn, "/tmp/repo"));
        unregister_repo(&conn, "/tmp/repo").unwrap();
        assert!(!is_repo_registered(&conn, "/tmp/repo"));
        assert!(list_repos(&conn).unwrap().is_empty());
    }

    #[test]
    fn list_repos_ordered_by_name() {
        let conn = in_memory_db();
        register_repo(&conn, "/z/zebra", "zebra", "purple").unwrap();
        register_repo(&conn, "/a/alpha", "alpha", "blue").unwrap();
        register_repo(&conn, "/m/mid", "mid", "green").unwrap();
        let repos = list_repos(&conn).unwrap();
        let names: Vec<&str> = repos.iter().map(|(_, n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn migration_v2_is_idempotent() {
        let conn = in_memory_db();
        run_migrations(&conn).unwrap();
        register_repo(&conn, "/tmp/repo", "repo", "green").unwrap();
        assert_eq!(list_repos(&conn).unwrap().len(), 1);
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let conn = in_memory_db();
        // Should not error when path doesn't exist
        unregister_repo(&conn, "/does/not/exist").unwrap();
        assert!(list_repos(&conn).unwrap().is_empty());
    }

    // ── find_repo_by_name tests ────────────────────────────────

    #[test]
    fn find_repo_by_name_no_match() {
        let conn = in_memory_db();
        assert_eq!(find_repo_by_name(&conn, "nonexistent").unwrap(), None);
    }

    #[test]
    fn find_repo_by_name_single_match() {
        let conn = in_memory_db();
        register_repo(&conn, "/home/user/project", "project", "purple").unwrap();
        assert_eq!(
            find_repo_by_name(&conn, "project").unwrap(),
            Some("/home/user/project".to_string())
        );
    }

    #[test]
    fn find_repo_by_name_multiple_matches_errors() {
        let conn = in_memory_db();
        register_repo(&conn, "/home/user/project", "project", "purple").unwrap();
        register_repo(&conn, "/tmp/project", "project", "blue").unwrap();
        let result = find_repo_by_name(&conn, "project");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("multiple repos"));
    }

    #[test]
    fn migrate_v2_only_when_v1_already_applied() {
        // Simulate a database that already has v1 applied but not v2
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );",
        )
        .unwrap();
        // Apply only v1
        migrate_v1(&conn).unwrap();

        // Verify v1 is applied but repos table doesn't exist yet
        let version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 1);
        assert!(conn
            .query_row("SELECT COUNT(*) FROM repos", [], |row| row.get::<_, i64>(0))
            .is_err());

        // Now run all migrations — should apply v2 only
        run_migrations(&conn).unwrap();

        // repos table should now exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // v1 data should still be intact
        set_default_agent(&conn, "claude").unwrap();
        assert_eq!(get_default_agent(&conn), Some("claude".to_string()));
    }

    // ── Repo color tests ────────────────────────────────────────

    #[test]
    fn register_repo_stores_color() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo", "purple").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].2, Some("purple".to_string()));
    }

    #[test]
    fn set_repo_color_updates() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo", "blue").unwrap();
        set_repo_color(&conn, "/tmp/repo", "teal").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos[0].2, Some("teal".to_string()));
    }

    #[test]
    fn next_repo_color_cycles() {
        let conn = in_memory_db();
        // Empty DB → first color
        assert_eq!(next_repo_color(&conn), REPO_COLORS[0]);
        // Add one repo → second color
        register_repo(&conn, "/a", "a", "purple").unwrap();
        assert_eq!(next_repo_color(&conn), REPO_COLORS[1]);
        // Add enough to wrap around
        for (i, color) in REPO_COLORS.iter().enumerate().skip(1) {
            register_repo(&conn, &format!("/r{i}"), &format!("r{i}"), color).unwrap();
        }
        assert_eq!(next_repo_color(&conn), REPO_COLORS[0]);
    }

    #[test]
    fn migrate_v11_adds_orchestrator_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
        )
        .unwrap();
        // Apply v1..v10 first; v11 should be the only change after this point.
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        migrate_v3(&conn).unwrap();
        migrate_v4(&conn).unwrap();
        migrate_v5(&conn).unwrap();
        migrate_v6(&conn).unwrap();
        migrate_v7(&conn).unwrap();
        migrate_v8(&conn).unwrap();
        migrate_v9(&conn).unwrap();
        migrate_v10(&conn).unwrap();
        // The new columns should not exist yet.
        assert!(conn
            .query_row(
                "SELECT orchestrator_id FROM queued_batches LIMIT 1",
                [],
                |r| r.get::<_, Option<String>>(0)
            )
            .is_err());
        migrate_v11(&conn).unwrap();
        // Inserting a row with both columns NULL must succeed.
        conn.execute(
            "INSERT INTO queued_batches (id, title, repo_path, target_branch, max_concurrent,
             prompt_prefix, prompt_suffix, plan_mode, allow_bypass, agent_binary, hub,
             scheduled_at, status, created_at, launch_mode, depends_on)
             VALUES ('b1', 't', '/r', 'main', 1, NULL, NULL, 0, 0, NULL, 'clust',
             NULL, 'idle', '2025-01-01T00:00:00Z', 'auto', '[]')",
            [],
        )
        .unwrap();
        let (orch_id, file): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT orchestrator_id, orchestrator_batch_file FROM queued_batches WHERE id = 'b1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(orch_id, None);
        assert_eq!(file, None);
    }

    #[test]
    fn insert_and_get_batch_source() {
        let conn = in_memory_db();
        let row = QueuedBatchRow {
            id: "b00abc".to_string(),
            title: "Models".to_string(),
            repo_path: "/r".to_string(),
            target_branch: "main".to_string(),
            max_concurrent: Some(2),
            prompt_prefix: None,
            prompt_suffix: None,
            plan_mode: false,
            allow_bypass: false,
            agent_binary: None,
            hub: "clust".to_string(),
            scheduled_at: None,
            status: "idle".to_string(),
            launch_mode: "auto".to_string(),
            depends_on: "[]".to_string(),
            orchestrator_id: Some("oabcdef123456".to_string()),
            orchestrator_batch_file: Some("batch-001.json".to_string()),
        };
        insert_queued_batch(&conn, &row, &[]).unwrap();
        let pair = get_batch_source(&conn, "b00abc").unwrap().unwrap();
        assert_eq!(pair.0, "oabcdef123456");
        assert_eq!(pair.1, "batch-001.json");

        // Non-orchestrator batch returns None.
        let mut row2 = row;
        row2.id = "b00def".to_string();
        row2.title = "Plain".to_string();
        row2.orchestrator_id = None;
        row2.orchestrator_batch_file = None;
        insert_queued_batch(&conn, &row2, &[]).unwrap();
        assert!(get_batch_source(&conn, "b00def").unwrap().is_none());

        // Unknown id is also None, not an error.
        assert!(get_batch_source(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn imported_batches_for_orch_returns_titles_and_files() {
        let conn = in_memory_db();
        let mk = |id: &str, title: &str, oid: Option<&str>, file: Option<&str>| QueuedBatchRow {
            id: id.to_string(),
            title: title.to_string(),
            repo_path: "/r".to_string(),
            target_branch: "main".to_string(),
            max_concurrent: None,
            prompt_prefix: None,
            prompt_suffix: None,
            plan_mode: false,
            allow_bypass: false,
            agent_binary: None,
            hub: "clust".to_string(),
            scheduled_at: None,
            status: "idle".to_string(),
            launch_mode: "auto".to_string(),
            depends_on: "[]".to_string(),
            orchestrator_id: oid.map(str::to_string),
            orchestrator_batch_file: file.map(str::to_string),
        };
        insert_queued_batch(&conn, &mk("b1", "A", Some("ox"), Some("a.json")), &[]).unwrap();
        insert_queued_batch(&conn, &mk("b2", "B", Some("ox"), Some("b.json")), &[]).unwrap();
        insert_queued_batch(&conn, &mk("b3", "C", Some("oy"), Some("a.json")), &[]).unwrap();
        insert_queued_batch(&conn, &mk("b4", "D", None, None), &[]).unwrap();

        let result = imported_batches_for_orch(&conn, "ox").unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains("a.json"));
        assert!(result.files.contains("b.json"));
        assert!(result.titles.contains("A"));
        assert!(result.titles.contains("B"));
        assert!(!result.titles.contains("C"));

        let other = imported_batches_for_orch(&conn, "nope").unwrap();
        assert!(other.titles.is_empty());
        assert!(other.files.is_empty());
    }

    #[test]
    fn migration_v3_backfills_colors() {
        // Simulate v2 database with repos but no color column
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
        )
        .unwrap();
        migrate_v1(&conn).unwrap();
        migrate_v2(&conn).unwrap();
        // Insert repos without color (v2 schema has no color column)
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO repos (path, name, registered_at) VALUES (?1, ?2, ?3)",
            rusqlite::params!["/a/alpha", "alpha", now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO repos (path, name, registered_at) VALUES (?1, ?2, ?3)",
            rusqlite::params!["/b/beta", "beta", now],
        )
        .unwrap();
        // Run migrations v3 and v4
        migrate_v3(&conn).unwrap();
        migrate_v4(&conn).unwrap();
        // Repos should now have colors (ordered by name: alpha, beta)
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos[0].2, Some(REPO_COLORS[0].to_string())); // alpha
        assert_eq!(repos[1].2, Some(REPO_COLORS[1].to_string())); // beta
    }
}
