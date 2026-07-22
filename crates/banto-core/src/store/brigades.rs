//! Brigades: an internal operational cell of one Director session and one or
//! more Worker sessions, hosted together as tiled panes in the emporium mode.
//!
//! A brigade is a *separate* concept from a group: a group is the user's own
//! filing (project / phase), while a brigade is a live operational unit — a
//! Director commanding Worker(s). It is an internal term, never surfaced to the
//! user.
//!
//! `brigade_members` is a plain join table. Two policies are layered here in
//! code rather than enforced by the schema, mirroring how single-group
//! membership is layered on `group_members`:
//! - a session belongs to at most one brigade (see [`Store::set_brigade_member`],
//!   which clears any prior membership first);
//! - a brigade has exactly one Director — that rule lives in the formation
//!   layer (the emporium), not this table.

use std::time::SystemTime;

use rusqlite::{OptionalExtension, params};

use crate::model::SessionId;

use super::{Store, StoreError, system_time_to_unix_ms};

/// Row id of a brigade (sqlite AUTOINCREMENT primary key).
pub type BrigadeId = i64;

/// A member's role within a brigade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrigadeRole {
    /// Commands the brigade; the user's only interface into it.
    Director,
    /// Carries out the Director's instructions.
    Worker,
}

impl BrigadeRole {
    /// The token persisted in the `role` column.
    fn as_token(self) -> &'static str {
        match self {
            BrigadeRole::Director => "director",
            BrigadeRole::Worker => "worker",
        }
    }

    /// Parse a persisted `role` token leniently: anything other than
    /// `"director"` is treated as a Worker.
    fn from_token(token: &str) -> BrigadeRole {
        if token == "director" {
            BrigadeRole::Director
        } else {
            BrigadeRole::Worker
        }
    }
}

/// A brigade row. Its live membership is loaded separately via
/// [`Store::brigade_members`], mirroring groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brigade {
    pub id: BrigadeId,
    pub name: String,
}

/// One session's membership in a brigade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeMember {
    pub session_id: SessionId,
    pub role: BrigadeRole,
}

impl Store {
    /// Creates an (empty) brigade and returns its id.
    pub fn create_brigade(&self, name: &str) -> Result<BrigadeId, StoreError> {
        self.conn.execute(
            "INSERT INTO brigades (name, created_at_ms) VALUES (?1, ?2)",
            params![name, system_time_to_unix_ms(SystemTime::now())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Deletes a brigade together with its membership rows.
    pub fn delete_brigade(&mut self, id: BrigadeId) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM brigade_members WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigades WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Returns all brigades, oldest first (creation order).
    pub fn list_brigades(&self) -> Result<Vec<Brigade>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM brigades ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Brigade {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Assigns a session to exactly one brigade with `role`: transactionally
    /// removes any existing brigade membership for that session (a session
    /// belongs to at most one brigade), then adds it to `brigade_id`.
    /// Idempotent when the session is already only in `brigade_id`.
    pub fn set_brigade_member(
        &mut self,
        brigade_id: BrigadeId,
        session_id: &SessionId,
        role: BrigadeRole,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM brigade_members WHERE session_id = ?1",
            [&session_id.0],
        )?;
        tx.execute(
            "INSERT INTO brigade_members (brigade_id, session_id, role) VALUES (?1, ?2, ?3)",
            params![brigade_id, session_id.0, role.as_token()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes a session from a brigade. Removing a non-member is a no-op.
    pub fn remove_brigade_member(
        &self,
        brigade_id: BrigadeId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM brigade_members WHERE brigade_id = ?1 AND session_id = ?2",
            params![brigade_id, session_id.0],
        )?;
        Ok(())
    }

    /// Returns a brigade's members: the Director first, then Workers ordered by
    /// session id.
    pub fn brigade_members(&self, brigade_id: BrigadeId) -> Result<Vec<BrigadeMember>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, role FROM brigade_members WHERE brigade_id = ?1
             ORDER BY CASE role WHEN 'director' THEN 0 ELSE 1 END, session_id",
        )?;
        let rows = stmt.query_map([brigade_id], |row| {
            let session_id: String = row.get(0)?;
            let role: String = row.get(1)?;
            Ok(BrigadeMember {
                session_id: SessionId(session_id),
                role: BrigadeRole::from_token(&role),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns the brigade a session belongs to and its role there, if any. If
    /// the session is (unusually) in more than one brigade, returns the lowest
    /// brigade id, deterministically.
    pub fn brigade_of_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(BrigadeId, BrigadeRole)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT brigade_id, role FROM brigade_members WHERE session_id = ?1
                 ORDER BY brigade_id LIMIT 1",
                [&session_id.0],
                |row| {
                    let id: BrigadeId = row.get(0)?;
                    let role: String = row.get(1)?;
                    Ok((id, BrigadeRole::from_token(&role)))
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn create_list_delete() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("cell-a").unwrap();
        let b = store.create_brigade("cell-b").unwrap();
        assert_ne!(a, b);

        let names: Vec<String> = store
            .list_brigades()
            .unwrap()
            .into_iter()
            .map(|br| br.name)
            .collect();
        assert_eq!(names, ["cell-a", "cell-b"]); // creation order (by id)

        store.delete_brigade(a).unwrap();
        let names: Vec<String> = store
            .list_brigades()
            .unwrap()
            .into_iter()
            .map(|br| br.name)
            .collect();
        assert_eq!(names, ["cell-b"]);
    }

    #[test]
    fn members_list_director_first_then_workers_by_id() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        store
            .set_brigade_member(br, &sid("w2"), BrigadeRole::Worker)
            .unwrap();
        store
            .set_brigade_member(br, &sid("dir"), BrigadeRole::Director)
            .unwrap();
        store
            .set_brigade_member(br, &sid("w1"), BrigadeRole::Worker)
            .unwrap();

        let members = store.brigade_members(br).unwrap();
        assert_eq!(
            members,
            [
                BrigadeMember {
                    session_id: sid("dir"),
                    role: BrigadeRole::Director,
                },
                BrigadeMember {
                    session_id: sid("w1"),
                    role: BrigadeRole::Worker,
                },
                BrigadeMember {
                    session_id: sid("w2"),
                    role: BrigadeRole::Worker,
                },
            ]
        );
    }

    #[test]
    fn brigade_of_session_reports_membership_and_role() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        assert_eq!(store.brigade_of_session(&sid("dir")).unwrap(), None);

        store
            .set_brigade_member(br, &sid("dir"), BrigadeRole::Director)
            .unwrap();
        assert_eq!(
            store.brigade_of_session(&sid("dir")).unwrap(),
            Some((br, BrigadeRole::Director))
        );
    }

    #[test]
    fn set_member_moves_a_session_between_brigades() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("a").unwrap();
        let b = store.create_brigade("b").unwrap();

        store
            .set_brigade_member(a, &sid("w"), BrigadeRole::Worker)
            .unwrap();
        // Re-assigning to another brigade moves it (single-brigade invariant).
        store
            .set_brigade_member(b, &sid("w"), BrigadeRole::Director)
            .unwrap();

        assert!(store.brigade_members(a).unwrap().is_empty());
        assert_eq!(
            store.brigade_of_session(&sid("w")).unwrap(),
            Some((b, BrigadeRole::Director))
        );
    }

    #[test]
    fn set_member_is_idempotent_within_the_same_brigade() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        store
            .set_brigade_member(br, &sid("w"), BrigadeRole::Worker)
            .unwrap();
        store
            .set_brigade_member(br, &sid("w"), BrigadeRole::Worker)
            .unwrap();

        assert_eq!(
            store.brigade_members(br).unwrap(),
            [BrigadeMember {
                session_id: sid("w"),
                role: BrigadeRole::Worker,
            }]
        );
    }

    #[test]
    fn remove_member_and_delete_clear_membership() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .set_brigade_member(br, &sid("dir"), BrigadeRole::Director)
            .unwrap();
        store
            .set_brigade_member(br, &sid("w"), BrigadeRole::Worker)
            .unwrap();

        store.remove_brigade_member(br, &sid("w")).unwrap();
        assert_eq!(
            store.brigade_members(br).unwrap(),
            [BrigadeMember {
                session_id: sid("dir"),
                role: BrigadeRole::Director,
            }]
        );
        // Removing a non-member is a no-op.
        store.remove_brigade_member(br, &sid("nobody")).unwrap();

        store.delete_brigade(br).unwrap();
        assert!(store.brigade_members(br).unwrap().is_empty());
        assert_eq!(store.brigade_of_session(&sid("dir")).unwrap(), None);
    }
}
