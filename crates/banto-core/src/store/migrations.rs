//! Schema migrations, keyed by `PRAGMA user_version`.
//!
//! `MIGRATIONS[i]` upgrades a database at `user_version == i` to `i + 1`.
//! Each script runs inside one transaction together with the version bump, so
//! a crash mid-migration leaves the previous version intact and reopening the
//! same file is idempotent.

use rusqlite::Connection;

use super::StoreError;

const MIGRATIONS: &[&str] = &[
    // v1: initial schema.
    //
    // `sessions` is a cache mirror of provider discovery; `sessions_fts` is a
    // contentless-sync FTS5 index over (title, cwd) kept in sync manually on
    // upsert/delete. `pins`, `groups`/`group_members`, and `panes` are
    // banto-owned state referencing sessions loosely (no foreign keys), so
    // user data survives a source that is temporarily unavailable.
    "CREATE TABLE sessions (
        id          TEXT PRIMARY KEY,
        provider    TEXT NOT NULL,
        title       TEXT,
        cwd         TEXT,
        source_path TEXT NOT NULL,
        mtime_ms    INTEGER NOT NULL,
        size        INTEGER NOT NULL
    );
    CREATE VIRTUAL TABLE sessions_fts USING fts5(title, cwd, id UNINDEXED);
    CREATE TABLE pins (
        session_id   TEXT PRIMARY KEY,
        pinned_at_ms INTEGER NOT NULL
    );
    CREATE TABLE groups (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        name       TEXT NOT NULL UNIQUE,
        sort_order INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE group_members (
        group_id   INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        PRIMARY KEY (group_id, session_id)
    );
    CREATE TABLE panes (
        session_id   TEXT PRIMARY KEY,
        backend      TEXT NOT NULL,
        target       TEXT NOT NULL,
        pid          INTEGER,
        opened_at_ms INTEGER NOT NULL
    );",
];

/// Applies all pending migrations. A `user_version` outside the known range
/// (negative or newer than this build) is rejected.
pub(super) fn apply(conn: &Connection) -> Result<(), StoreError> {
    loop {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 0 || version > MIGRATIONS.len() as i64 {
            return Err(StoreError::UnsupportedSchemaVersion(version));
        }
        let idx = version as usize;
        if idx == MIGRATIONS.len() {
            return Ok(());
        }
        let script = format!(
            "BEGIN;\n{}\nPRAGMA user_version = {};\nCOMMIT;",
            MIGRATIONS[idx],
            idx + 1
        );
        conn.execute_batch(&script)?;
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::meta;
    use super::super::{Store, StoreError};

    /// Proves the bundled rusqlite build ships the FTS5 module.
    #[test]
    fn fts5_module_available() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x);")
            .unwrap();
        conn.execute("INSERT INTO t (x) VALUES ('hello world')", [])
            .unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'hello'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn migrations_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        // Nested path also exercises parent-directory creation in `open`.
        let db = dir.path().join("nested").join("banto.db");

        {
            let mut store = Store::open(&db).unwrap();
            store
                .sync_sessions(&[meta("s1", Some("first"), None)])
                .unwrap();
        }

        // Reopening must not re-run migrations or lose data.
        let store = Store::open(&db).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn future_schema_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);

        let Err(err) = Store::open(&db) else {
            panic!("open should reject a future schema version");
        };
        assert!(matches!(err, StoreError::UnsupportedSchemaVersion(99)));
    }
}
