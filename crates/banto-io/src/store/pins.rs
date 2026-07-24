//! Pinned sessions (banto-owned state; never written to Claude's files).

use std::time::SystemTime;

use rusqlite::params;

use banto_core::model::SessionId;

use super::{Store, StoreError, system_time_to_unix_ms};

impl Store {
    /// Pins a session. Idempotent: pinning an already-pinned session keeps
    /// the original pin time.
    pub fn pin(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO pins (session_id, pinned_at_ms) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO NOTHING",
            params![session_id.0, system_time_to_unix_ms(SystemTime::now())],
        )?;
        Ok(())
    }

    /// Unpins a session. Unpinning a session that is not pinned is a no-op.
    pub fn unpin(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM pins WHERE session_id = ?1", [&session_id.0])?;
        Ok(())
    }

    /// Returns pinned session ids, oldest pin first (id as tie-breaker).
    pub fn pinned_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT session_id FROM pins ORDER BY pinned_at_ms, session_id")?;
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
    fn pin_unpin_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.pinned_ids().unwrap().is_empty());

        store.pin(&sid("a")).unwrap();
        store.pin(&sid("b")).unwrap();
        assert_eq!(store.pinned_ids().unwrap(), [sid("a"), sid("b")]);

        store.unpin(&sid("a")).unwrap();
        assert_eq!(store.pinned_ids().unwrap(), [sid("b")]);

        // Unpinning something not pinned is a no-op.
        store.unpin(&sid("never-pinned")).unwrap();
        assert_eq!(store.pinned_ids().unwrap(), [sid("b")]);
    }

    #[test]
    fn pin_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.pin(&sid("a")).unwrap();
        store.pin(&sid("a")).unwrap();
        assert_eq!(store.pinned_ids().unwrap(), [sid("a")]);
    }
}
