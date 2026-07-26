//! Persistent store: sqlite (rusqlite, bundled) under `dirs::data_local_dir()/banto`.
//!
//! Responsibilities (docs/REQUIREMENTS.md "Module layout"):
//! - groups / pins / archived sessions (banto-owned; never written to
//!   Claude's files)
//! - session <-> pane mapping for the opener (phase 2 consumes this)
//!
//! No session index cache or filesystem path ever lives here (see
//! `migrations::MIGRATIONS`'s v11 for why) — every row is keyed by session
//! id and holds no paths at all, so nothing in the store can go stale
//! against `~/.claude`. Rows deliberately keep no foreign key to a session,
//! which is what lets a pin or a group survive a source that is temporarily
//! unavailable.

mod archive;
mod brigades;
mod groups;
mod lineage;
mod migrations;
mod panes;
mod pins;

pub use banto_core::model::{
    Brigade, BrigadeId, BrigadeMember, BrigadeMessage, BrigadeRole, MemberToken,
};
pub use groups::{Group, GroupId};
pub use panes::PaneRecord;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// How long a connection waits for a lock held by another connection before
/// giving up with `SQLITE_BUSY`. The TUI process and the `banto _wrap`
/// process(es) it launches all open the same sqlite file, so without this a
/// pane-map registration or cleanup could fail silently under contention
/// instead of just waiting the other side out.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors returned by [`Store`] operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Underlying sqlite failure (including constraint violations such as
    /// creating two groups with the same name).
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The parent directory of the database file could not be created.
    #[error("failed to create database directory {path:?}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The database file reports a `PRAGMA user_version` this build does not
    /// know, i.e. it was written by a newer banto. Refusing to touch it avoids
    /// corrupting a newer schema.
    #[error("unsupported database schema version {0}")]
    UnsupportedSchemaVersion(i64),
}

/// Handle to banto's own sqlite database.
///
/// All banto-owned persistent state lives here; nothing is ever written to
/// Claude's files.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (or creates) the database at `path`, creating the parent
    /// directory if missing, and applies pending migrations.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens a fresh in-memory database (for tests).
    pub fn open_in_memory() -> Result<Store, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Store, StoreError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        migrations::apply(&conn)?;
        Ok(Store { conn })
    }
}

/// Converts a `SystemTime` to signed milliseconds since the unix epoch
/// (negative for pre-epoch times), saturating at the `i64` range.
fn system_time_to_unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(err) => {
            let before = err.duration();
            i64::try_from(before.as_millis())
                .map(|ms| ms.saturating_neg())
                .unwrap_or(i64::MIN)
        }
    }
}

/// Inverse of [`system_time_to_unix_ms`].
fn unix_ms_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use banto_core::model::{AgentKind, SessionId, SessionMeta};

    /// Builds a synthetic `SessionMeta` fixture (never real session data).
    /// The mtime is aligned to whole milliseconds so it round-trips exactly.
    pub fn meta(id: &str, title: Option<&str>, cwd: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            agent: AgentKind::ClaudeCode,
            title: title.map(str::to_string),
            cwd: cwd.map(PathBuf::from),
            source_path: PathBuf::from(format!("C:/synthetic-fixtures/{id}.jsonl")),
            mtime: UNIX_EPOCH + Duration::from_millis(1_750_000_000_000),
            size: 42,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::SessionId;

    #[test]
    fn open_sets_the_configured_busy_timeout() {
        let store = Store::open_in_memory().unwrap();
        let ms: i64 = store
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(ms, BUSY_TIMEOUT.as_millis() as i64);
    }

    #[test]
    fn busy_timeout_waits_out_a_concurrent_writer_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        let store = Store::open(&db).unwrap();

        // A second, independent connection holds the write lock for a short
        // while, simulating `banto _wrap` touching the pane map at the same
        // time as the TUI. `pin` is a single-statement, implicit-transaction
        // write (no held read lock of its own), so it only ever contends on
        // the lock the blocker is holding.
        let blocker_db = db.clone();
        let blocker = std::thread::spawn(move || {
            let conn = Connection::open(&blocker_db).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            std::thread::sleep(Duration::from_millis(200));
            conn.execute_batch("COMMIT;").unwrap();
        });
        // Give the blocker a head start so it acquires the lock first.
        std::thread::sleep(Duration::from_millis(50));

        // Without a busy_timeout this would fail immediately with
        // SQLITE_BUSY; with it configured, the write waits for the blocker
        // to commit (well under the 5s timeout) and succeeds.
        let result = store.pin(&SessionId("s1".to_string()));
        blocker.join().unwrap();
        assert!(result.is_ok(), "expected the write to wait, got {result:?}");
    }

    #[test]
    fn unix_ms_roundtrip_positive() {
        let t = UNIX_EPOCH + Duration::from_millis(1_750_000_000_123);
        assert_eq!(unix_ms_to_system_time(system_time_to_unix_ms(t)), t);
    }

    #[test]
    fn unix_ms_roundtrip_pre_epoch() {
        let t = UNIX_EPOCH - Duration::from_millis(12_345);
        let ms = system_time_to_unix_ms(t);
        assert_eq!(ms, -12_345);
        assert_eq!(unix_ms_to_system_time(ms), t);
    }
}
