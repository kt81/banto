//! Session <-> pane mapping for the opener (phase 2).
//!
//! One pane per session, mirroring the "no double resume" invariant: before
//! resuming, the opener checks this table and focuses the existing pane
//! instead of spawning a second one.

use std::time::SystemTime;

use rusqlite::{OptionalExtension, params};

use banto_core::model::SessionId;

use super::{Store, StoreError, system_time_to_unix_ms, unix_ms_to_system_time};

/// A pane/tab a session was resumed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRecord {
    pub session_id: SessionId,
    /// Opener backend name, e.g. "psmux" or "windows-terminal".
    pub backend: String,
    /// Backend-specific target id (e.g. a psmux pane id like "%8", or a
    /// window handle for Windows Terminal window mode).
    pub target: String,
    /// PID of the `banto _wrap` process, when known.
    pub pid: Option<u32>,
    /// When the pane was opened.
    pub opened_at: SystemTime,
}

impl Store {
    /// Records (or replaces) the pane a session is resumed in.
    pub fn set_pane(&self, pane: &PaneRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO panes (session_id, backend, target, pid, opened_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id) DO UPDATE SET
                 backend      = excluded.backend,
                 target       = excluded.target,
                 pid          = excluded.pid,
                 opened_at_ms = excluded.opened_at_ms",
            params![
                pane.session_id.0,
                pane.backend,
                pane.target,
                pane.pid.map(i64::from),
                system_time_to_unix_ms(pane.opened_at),
            ],
        )?;
        Ok(())
    }

    /// Looks up the pane a session is currently mapped to, if any.
    pub fn get_pane(&self, session_id: &SessionId) -> Result<Option<PaneRecord>, StoreError> {
        let record = self
            .conn
            .query_row(
                "SELECT session_id, backend, target, pid, opened_at_ms
                 FROM panes WHERE session_id = ?1",
                [&session_id.0],
                pane_from_row,
            )
            .optional()?;
        Ok(record)
    }

    /// Removes the pane mapping for a session (e.g. after the wrapper exits).
    /// Removing a session without a pane is a no-op.
    pub fn remove_pane(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM panes WHERE session_id = ?1", [&session_id.0])?;
        Ok(())
    }

    /// Returns all recorded panes, most recently opened first.
    pub fn list_panes(&self) -> Result<Vec<PaneRecord>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, backend, target, pid, opened_at_ms
             FROM panes ORDER BY opened_at_ms DESC, session_id",
        )?;
        let rows = stmt.query_map([], pane_from_row)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn pane_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaneRecord> {
    Ok(PaneRecord {
        session_id: SessionId(row.get(0)?),
        backend: row.get(1)?,
        target: row.get(2)?,
        pid: row
            .get::<_, Option<i64>>(3)?
            .and_then(|pid| u32::try_from(pid).ok()),
        opened_at: unix_ms_to_system_time(row.get(4)?),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn pane(session: &str, target: &str, pid: Option<u32>, ms: u64) -> PaneRecord {
        PaneRecord {
            session_id: SessionId(session.to_string()),
            backend: "psmux".to_string(),
            target: target.to_string(),
            pid,
            opened_at: UNIX_EPOCH + Duration::from_millis(ms),
        }
    }

    #[test]
    fn set_get_remove_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let p = pane("s1", "%8", Some(4321), 1_750_000_000_000);

        assert_eq!(store.get_pane(&p.session_id).unwrap(), None);
        store.set_pane(&p).unwrap();
        assert_eq!(store.get_pane(&p.session_id).unwrap(), Some(p.clone()));

        store.remove_pane(&p.session_id).unwrap();
        assert_eq!(store.get_pane(&p.session_id).unwrap(), None);
        // Removing again is a no-op.
        store.remove_pane(&p.session_id).unwrap();
    }

    #[test]
    fn set_pane_replaces_existing() {
        let store = Store::open_in_memory().unwrap();
        store.set_pane(&pane("s1", "%1", Some(1), 1_000)).unwrap();
        store.set_pane(&pane("s1", "%2", None, 2_000)).unwrap();

        let got = store
            .get_pane(&SessionId("s1".to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(got.target, "%2");
        assert_eq!(got.pid, None);
    }

    #[test]
    fn list_panes_most_recent_first() {
        let store = Store::open_in_memory().unwrap();
        store.set_pane(&pane("old", "%1", None, 1_000)).unwrap();
        store.set_pane(&pane("new", "%2", None, 2_000)).unwrap();

        let targets: Vec<String> = store
            .list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.target)
            .collect();
        assert_eq!(targets, ["%2", "%1"]);
    }
}
