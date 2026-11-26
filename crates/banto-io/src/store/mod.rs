//! Persistent store: sqlite (rusqlite, bundled), by default `banto.db` in the data
//! directory (`dirs::data_local_dir()/banto` unless another is chosen; the
//! `db_path` key may put the file elsewhere).
//!
//! Responsibilities (docs/REQUIREMENTS.md "Module layout"):
//! - groups / pins / archived sessions (banto-owned; never written to
//!   Claude's files)
//! - brigades: their members (with display names), the message queue between
//!   them, and the emporium's arrangement preferences (`brigades.rs`,
//!   `ui_prefs.rs`)
//!
//! No session index cache or filesystem path ever lives here (see
//! `migrations::MIGRATIONS`'s v11 for why) — every row is keyed by session
//! id and holds no paths at all, so nothing in the store can go stale
//! against `~/.claude`. Rows deliberately keep no foreign key to a session,
//! which is what lets a pin or a group survive a source that is temporarily
//! unavailable.

mod archive;
mod backup;
mod brigades;
mod consultations;
mod groups;
mod migrations;
mod pins;
mod plans;
mod progress;
mod ui_prefs;

pub use banto_core::model::{
    Brigade, BrigadeId, BrigadeMember, BrigadeMessage, BrigadeRole, MemberToken,
};
pub use brigades::{
    InterruptState, MAX_WORKER_NUMBER, MemberNaming, RequestDetail, UnreadInterrupt, glance_line,
};
pub use groups::{Group, GroupId};
pub use migrations::SCHEMA_VERSION;
pub use plans::{
    ClosedPlan, Notice, OpenedPlanner, PlanApproval, PlanClosed, PlanClosing, PlanReleased,
    PlanWritten,
};

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// How long a connection waits for a lock held by another connection before
/// giving up with `SQLITE_BUSY`. The emporium and the `banto _mcp` /
/// `banto _hook` processes it launches all open the same sqlite file, so
/// without this a message or member write could fail under contention instead
/// of just waiting the other side out.
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
    /// corrupting a newer schema. The text names the way out: the newer build, or
    /// the copy the upgrade left (`backup`'s naming: `before-v<K + 1>` holds
    /// schema `K`, and this build reads schemas up to `SCHEMA_VERSION`).
    #[error(
        "unsupported database schema version {0}: it was upgraded by a newer banto; run that \
         build, or go back with this build and the newest banto.db.before-v<N> backup beside the \
         database (N at most {newest_backup})",
        newest_backup = SCHEMA_VERSION + 1
    )]
    UnsupportedSchemaVersion(i64),
    /// The copy taken before a migration could not be written, so the migration
    /// did not run and the database is as it was.
    #[error(
        "could not write the backup {} that precedes a database upgrade ({cause}); the database \
         was not migrated",
        path.display()
    )]
    Backup { path: PathBuf, cause: String },
    /// A process that must not migrate (`_mcp`, `_hook`) found a schema other
    /// than the one it was built for: the banto on disk is not the one that
    /// started it, or not the one running the UI.
    #[error(
        "banto was replaced on disk; restart banto (database schema {found}, this build \
         {SCHEMA_VERSION})"
    )]
    SchemaMismatch { found: i64 },
    /// The brigade has issued every Worker number there is
    /// (nine digits); nothing was created.
    #[error("this brigade has run out of Worker numbers")]
    WorkerNumbersExhausted,
}

/// Handle to banto's own sqlite database.
///
/// All banto-owned persistent state lives here; nothing is ever written to
/// Claude's files.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// The UI's door: opens (or creates) the database at `path`, creating the
    /// parent directory if missing, and applies pending migrations — after a copy
    /// of the database as it was (`backup`). Only the UI migrates; the processes it
    /// starts use [`Store::open_current`].
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        backup::before_migration(&conn, path)?;
        Self::from_connection(conn)
    }

    /// The door of the processes a UI starts (`_mcp`, `_hook`): opens the
    /// existing database and uses it only if it is at exactly the schema this
    /// build knows. It never creates the file, never migrates and never copies.
    ///
    /// A running UI holds the single-instance lock precisely so that nothing
    /// migrates a database under it. On Unix the banto executable can be replaced
    /// while the UI runs, and the next child it starts is then the new build: if
    /// that one migrated, the running UI would find a schema it does not know.
    /// Any other version is therefore [`StoreError::SchemaMismatch`].
    pub fn open_current(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found != SCHEMA_VERSION {
            return Err(StoreError::SchemaMismatch { found });
        }
        Ok(Store { conn })
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
        // while, simulating a `banto _mcp` process writing at the same time
        // as the emporium. `pin` is a single-statement, implicit-transaction
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

    fn set_version(path: &Path, version: i64) {
        Connection::open(path)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
    }

    fn version_of(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_child_opens_a_current_database_and_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        let child = Store::open_current(&db).unwrap();
        child.pin(&SessionId("s1".to_string())).unwrap();
    }

    #[test]
    fn a_child_never_migrates_an_older_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        set_version(&db, SCHEMA_VERSION - 1);

        let Err(err) = Store::open_current(&db) else {
            panic!("an older schema is not this build's");
        };

        assert!(matches!(err, StoreError::SchemaMismatch { found } if found == SCHEMA_VERSION - 1));
        assert!(
            err.to_string()
                .starts_with("banto was replaced on disk; restart banto")
        );
        assert_eq!(
            err.to_string(),
            format!(
                "banto was replaced on disk; restart banto (database schema {}, this build {})",
                SCHEMA_VERSION - 1,
                SCHEMA_VERSION
            )
        );
        assert_eq!(version_of(&db), SCHEMA_VERSION - 1, "untouched");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no copy");
    }

    #[test]
    fn a_child_refuses_a_newer_database_with_the_same_words() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        drop(Store::open(&db).unwrap());
        set_version(&db, SCHEMA_VERSION + 1);
        let Err(err) = Store::open_current(&db) else {
            panic!("a newer schema is not this build's");
        };
        assert!(matches!(err, StoreError::SchemaMismatch { .. }), "{err:?}");
    }

    #[test]
    fn a_child_never_creates_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        assert!(Store::open_current(&db).is_err());
        assert!(!db.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_child_reads_an_empty_file_as_a_replaced_build_and_leaves_it_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");
        std::fs::write(&db, b"").unwrap();
        let Err(err) = Store::open_current(&db) else {
            panic!("version 0 is not this build's");
        };
        assert!(
            matches!(err, StoreError::SchemaMismatch { found: 0 }),
            "{err:?}"
        );
        assert_eq!(std::fs::metadata(&db).unwrap().len(), 0);
    }
}
