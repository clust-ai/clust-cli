use std::path::PathBuf;

use rusqlite::Connection;

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

/// Register a repository path. Silently ignores duplicates.
pub fn register_repo(conn: &Connection, path: &str, name: &str) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO repos (path, name, registered_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![path, name, now],
    )
    .map_err(|e| format!("failed to register repo: {e}"))?;
    Ok(())
}

/// List all registered repositories, ordered by name.
pub fn list_repos(conn: &Connection) -> Result<Vec<(String, String)>, String> {
    let mut stmt = conn
        .prepare("SELECT path, name FROM repos ORDER BY name")
        .map_err(|e| format!("failed to prepare repo query: {e}"))?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
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

/// Read the default agent from the config table. Returns `None` if not set.
pub fn get_default_agent(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM config WHERE key = 'default_agent'",
        [],
        |row| row.get(0),
    )
    .ok()
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
        register_repo(&conn, "/home/user/project", "project").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].0, "/home/user/project");
        assert_eq!(repos[0].1, "project");
    }

    #[test]
    fn register_duplicate_is_noop() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo").unwrap();
        register_repo(&conn, "/tmp/repo", "repo").unwrap();
        let repos = list_repos(&conn).unwrap();
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn is_repo_registered_true_false() {
        let conn = in_memory_db();
        assert!(!is_repo_registered(&conn, "/tmp/repo"));
        register_repo(&conn, "/tmp/repo", "repo").unwrap();
        assert!(is_repo_registered(&conn, "/tmp/repo"));
    }

    #[test]
    fn unregister_repo_removes_entry() {
        let conn = in_memory_db();
        register_repo(&conn, "/tmp/repo", "repo").unwrap();
        assert!(is_repo_registered(&conn, "/tmp/repo"));
        unregister_repo(&conn, "/tmp/repo").unwrap();
        assert!(!is_repo_registered(&conn, "/tmp/repo"));
        assert!(list_repos(&conn).unwrap().is_empty());
    }

    #[test]
    fn list_repos_ordered_by_name() {
        let conn = in_memory_db();
        register_repo(&conn, "/z/zebra", "zebra").unwrap();
        register_repo(&conn, "/a/alpha", "alpha").unwrap();
        register_repo(&conn, "/m/mid", "mid").unwrap();
        let repos = list_repos(&conn).unwrap();
        let names: Vec<&str> = repos.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn migration_v2_is_idempotent() {
        let conn = in_memory_db();
        run_migrations(&conn).unwrap();
        register_repo(&conn, "/tmp/repo", "repo").unwrap();
        assert_eq!(list_repos(&conn).unwrap().len(), 1);
    }

    #[test]
    fn unregister_nonexistent_is_noop() {
        let conn = in_memory_db();
        // Should not error when path doesn't exist
        unregister_repo(&conn, "/does/not/exist").unwrap();
        assert!(list_repos(&conn).unwrap().is_empty());
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
}
