use std::path::PathBuf;

use rusqlite::Connection;

/// A row from the repos table: (path, name, color, editor).
pub type RepoRow = (String, String, Option<String>, Option<String>);

/// Returns the database path: `~/.clust/clust.db`.
fn db_path() -> PathBuf {
    clust_ipc::clust_dir().join("clust.db")
}

/// Open (or create) the SQLite database and run any pending migrations.
///
/// Migrations are wrapped in a `BEGIN IMMEDIATE` transaction so that a second
/// hub starting concurrently (and racing on the same DB file) blocks until
/// the first finishes. The second hub then sees the post-migration version
/// inside its own transaction and skips all migration steps.
pub fn open_or_create() -> Result<Connection, String> {
    open_or_create_at(&db_path())
}

/// Same as [`open_or_create`] but at an explicit path. Public to the crate
/// (and to integration tests) so we can exercise the concurrency-safe
/// migration logic without touching the user's real `~/.clust/clust.db`.
pub fn open_or_create_at(path: &std::path::Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("failed to open database: {e}"))?;

    // Enable WAL mode for better concurrent read performance.
    // WAL is set at the database level (not per-connection), so doing this
    // outside the migration transaction is fine.
    conn.execute_batch("PRAGMA journal_mode=WAL;")
        .map_err(|e| format!("failed to set journal mode: {e}"))?;

    // Increase the busy timeout so concurrent migrators wait on SQLite's
    // file lock rather than failing fast. Migrations are tiny (milliseconds),
    // so 5 seconds is plenty.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| format!("failed to set busy timeout: {e}"))?;

    // Ensure schema_version table exists. Using IF NOT EXISTS, this is safe
    // to run from two connections simultaneously — each will either create
    // the table or no-op.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY
        );",
    )
    .map_err(|e| format!("failed to create schema_version table: {e}"))?;

    run_migrations(&conn)?;
    Ok(conn)
}

/// Highest schema version this binary knows how to produce.
const LATEST_SCHEMA_VERSION: i64 = 11;

/// Read the current `schema_version` value (0 if no rows).
fn read_schema_version(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )
    .map_err(|e| format!("failed to read schema version: {e}"))
}

/// Check current schema version and apply pending migrations atomically.
///
/// The whole migration sequence runs inside a single `BEGIN IMMEDIATE`
/// transaction so racing hubs serialize on SQLite's reserved lock. The second
/// caller observes the post-migration version inside its own transaction and
/// becomes a no-op — no idempotent backfills run twice.
fn run_migrations(conn: &Connection) -> Result<(), String> {
    // Fast path: if we are already at the latest version, skip the
    // transaction entirely. This is the common case after the first run.
    let pre_version = read_schema_version(conn)?;
    if pre_version >= LATEST_SCHEMA_VERSION {
        return Ok(());
    }

    // BEGIN IMMEDIATE acquires a RESERVED lock immediately, so a second
    // concurrent migrator either succeeds in serialization (waiting here) or
    // fails fast with SQLITE_BUSY. We retry busy with a short backoff to
    // give the holder time to commit.
    let mut attempts = 0u32;
    loop {
        match conn.execute_batch("BEGIN IMMEDIATE;") {
            Ok(()) => break,
            Err(e) => {
                let is_busy = matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy)
                        | Some(rusqlite::ErrorCode::DatabaseLocked)
                );
                attempts += 1;
                if !is_busy || attempts > 50 {
                    return Err(format!("failed to begin migration transaction: {e}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }

    // Re-read schema_version inside the transaction. If another hub already
    // applied the migrations between our pre-check and this point, the
    // version is now caught up and we commit a no-op transaction.
    let inner_result = (|| -> Result<(), String> {
        let current_version = read_schema_version(conn)?;

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
    })();

    match inner_result {
        Ok(()) => conn
            .execute_batch("COMMIT;")
            .map_err(|e| format!("failed to commit migrations: {e}")),
        Err(err) => {
            // Best-effort rollback; surface the original error regardless.
            let _ = conn.execute_batch("ROLLBACK;");
            Err(err)
        }
    }
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

// v5-v10 originally created and evolved the batch/schedule tables. The schedule
// system has been removed; these migrations are kept as version-bump stubs so
// existing databases still advance through the version sequence and the
// `concurrent_open_or_create_runs_migrations_once` test still observes a
// `schema_version` row count equal to LATEST_SCHEMA_VERSION on a fresh DB.
// Migration v11 below drops the orphaned batch tables on existing v10 DBs.

fn migrate_v5(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (5);")
        .map_err(|e| format!("migration v5 failed: {e}"))
}

fn migrate_v6(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (6);")
        .map_err(|e| format!("migration v6 failed: {e}"))
}

fn migrate_v7(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (7);")
        .map_err(|e| format!("migration v7 failed: {e}"))
}

fn migrate_v8(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (8);")
        .map_err(|e| format!("migration v8 failed: {e}"))
}

fn migrate_v9(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (9);")
        .map_err(|e| format!("migration v9 failed: {e}"))
}

fn migrate_v10(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("INSERT INTO schema_version (version) VALUES (10);")
        .map_err(|e| format!("migration v10 failed: {e}"))
}

/// Migration v11: drop the orphaned queued_batches and queued_batch_tasks
/// tables left behind by the removed schedule system.
fn migrate_v11(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS queued_batch_tasks;
         DROP TABLE IF EXISTS queued_batches;
         INSERT INTO schema_version (version) VALUES (11);",
    )
    .map_err(|e| format!("migration v11 failed: {e}"))
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

    // ── Concurrent migration tests ──────────────────────────────

    #[test]
    fn concurrent_open_or_create_runs_migrations_once() {
        // Two hubs racing on a fresh DB file must not double-apply the v3
        // backfill (which would produce duplicate rows from idempotent UPDATEs)
        // nor leave the schema_version table with extra entries.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clust.db");

        // Pre-seed the DB with two repos *without* colors so that v3's
        // backfill logic has work to do. We do this by manually applying v1
        // and v2 only, then inserting rows.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);",
            )
            .unwrap();
            migrate_v1(&conn).unwrap();
            migrate_v2(&conn).unwrap();
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
        }

        // Now race two threads through open_or_create_at. They share the
        // same on-disk DB but each owns its own Connection.
        let path1 = path.clone();
        let path2 = path.clone();
        let h1 = std::thread::spawn(move || {
            super::open_or_create_at(&path1).expect("thread 1 open_or_create")
        });
        let h2 = std::thread::spawn(move || {
            super::open_or_create_at(&path2).expect("thread 2 open_or_create")
        });
        h1.join().expect("thread 1 joined");
        h2.join().expect("thread 2 joined");

        // Both opens must have committed the schema. Re-open and inspect.
        let conn = Connection::open(&path).unwrap();

        // Schema is at the latest version exactly once. With the bug, the
        // second migrator could `INSERT INTO schema_version (version) VALUES (3)`
        // a second time, producing two rows for version 3. The PRIMARY KEY
        // constraint would actually error — but more importantly, the v3
        // backfill is an UPDATE, not an INSERT, so without the transaction
        // guard it could run twice and overwrite colors set by the first
        // migrator with colors derived from the same ordering. The cleanest
        // assertion is: schema_version has exactly LATEST_SCHEMA_VERSION rows
        // (one per applied migration, no duplicates).
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            row_count,
            super::LATEST_SCHEMA_VERSION,
            "schema_version table should have exactly one row per migration"
        );

        let max_version: i64 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(max_version, super::LATEST_SCHEMA_VERSION);

        // Repos still exist and have non-null colors after the race.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM repos WHERE color IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "both repos should have a color after migration");

        // No duplicate repos either.
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn open_or_create_at_is_idempotent() {
        // Calling sequentially (not racing) twice should still leave the DB
        // in a known-good state with no duplicate schema_version rows.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clust.db");
        let _conn1 = super::open_or_create_at(&path).unwrap();
        let _conn2 = super::open_or_create_at(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(row_count, super::LATEST_SCHEMA_VERSION);
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
