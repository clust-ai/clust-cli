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
}
