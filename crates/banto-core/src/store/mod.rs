//! Persistent store: sqlite (rusqlite, bundled) under `dirs::data_local_dir()/banto`.
//!
//! Responsibilities (docs/REQUIREMENTS.md "Module layout"):
//! - session index cache (metadata mirror of provider discovery) + FTS5 text index
//! - groups / pins (banto-owned; never written to Claude's files)
//! - session <-> pane mapping for the opener (phase 2 consumes this)
//!
//! Paths are persisted lossily via `to_string_lossy`: non-UTF-8 path
//! components round-trip as U+FFFD replacement characters. This is acceptable
//! because the store is a display/search cache; the provider remains the
//! source of truth for real paths.

mod groups;
mod migrations;
mod panes;
mod pins;
mod sessions;

pub use groups::{Group, GroupId};
pub use panes::PaneRecord;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

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

    use crate::model::{SessionId, SessionMeta};

    /// Builds a synthetic `SessionMeta` fixture (never real session data).
    /// The mtime is aligned to whole milliseconds so it round-trips exactly.
    pub fn meta(id: &str, title: Option<&str>, cwd: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            provider: "claude-code".to_string(),
            title: title.map(str::to_string),
            cwd: cwd.map(PathBuf::from),
            source_path: PathBuf::from(format!("C:/synthetic-fixtures/{id}.jsonl")),
            mtime: UNIX_EPOCH + Duration::from_millis(1_750_000_000_000),
            size: 42,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
