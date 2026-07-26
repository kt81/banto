//! Archived sessions.
//!
//! Archiving hides a session from the default list without deleting
//! anything or touching pins/groups/panes; `unarchive_session` reverses it.
//! Mirrors the `pins` table/API shape: a loose reference (no foreign key),
//! so an archived id survives a source that is temporarily unavailable.

use std::time::SystemTime;

use rusqlite::params;

use banto_core::model::SessionId;

use super::{Store, StoreError, system_time_to_unix_ms};

impl Store {
    /// Archives a session. Idempotent: archiving an already-archived session
    /// keeps the original archive time.
    pub fn archive_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO archived (session_id, archived_at_ms) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO NOTHING",
            params![session_id.0, system_time_to_unix_ms(SystemTime::now())],
        )?;
        Ok(())
    }

    /// Unarchives a session. Unarchiving a session that isn't archived is a
    /// no-op.
    pub fn unarchive_session(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM archived WHERE session_id = ?1",
            [&session_id.0],
        )?;
        Ok(())
    }

    /// Returns archived session ids, oldest archive first (id as
    /// tie-breaker).
    pub fn archived_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id FROM archived ORDER BY archived_at_ms, session_id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0).map(SessionId))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn archive_unarchive_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.archived_ids().unwrap().is_empty());

        store.archive_session(&sid("a")).unwrap();
        store.archive_session(&sid("b")).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [sid("a"), sid("b")]);

        store.unarchive_session(&sid("a")).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [sid("b")]);

        // Unarchiving something not archived is a no-op.
        store.unarchive_session(&sid("never-archived")).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [sid("b")]);
    }

    #[test]
    fn archive_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.archive_session(&sid("a")).unwrap();
        store.archive_session(&sid("a")).unwrap();
        assert_eq!(store.archived_ids().unwrap(), [sid("a")]);
    }
}
