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

/// A queued message from one brigade member to the peer role (see the
/// `brigade_messages` migration): what a recipient pulls via
/// [`Store::fetch_brigade_messages`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeMessage {
    /// Monotonic queue id (also the per-session read cursor).
    pub id: i64,
    /// The session that sent it (for attribution in the firewall framing).
    pub from_session: String,
    pub body: String,
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

    /// Enqueue a message in `brigade_id` from `from_session`, addressed to
    /// `to_role` (every session of that role in the brigade will pull it).
    /// Returns the new message's queue id.
    pub fn enqueue_brigade_message(
        &self,
        brigade_id: BrigadeId,
        from_session: &str,
        to_role: BrigadeRole,
        body: &str,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO brigade_messages (brigade_id, from_session, to_role, body, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                brigade_id,
                from_session,
                to_role.as_token(),
                body,
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Pull the messages in `brigade_id` addressed to `recipient_role` that
    /// `session_id` has not seen yet (id past its cursor), oldest first, and
    /// advance that session's cursor past them — so a later call returns only
    /// what has arrived since. Per-session cursors mean each recipient of a
    /// broadcast sees it independently.
    pub fn fetch_brigade_messages(
        &mut self,
        brigade_id: BrigadeId,
        session_id: &str,
        recipient_role: BrigadeRole,
    ) -> Result<Vec<BrigadeMessage>, StoreError> {
        let tx = self.conn.transaction()?;
        let cursor: i64 = tx
            .query_row(
                "SELECT last_seen_id FROM brigade_cursors WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let messages = {
            let mut stmt = tx.prepare(
                "SELECT id, from_session, body FROM brigade_messages
                 WHERE brigade_id = ?1 AND to_role = ?2 AND id > ?3 ORDER BY id",
            )?;
            let rows = stmt.query_map(
                params![brigade_id, recipient_role.as_token(), cursor],
                |row| {
                    Ok(BrigadeMessage {
                        id: row.get(0)?,
                        from_session: row.get(1)?,
                        body: row.get(2)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if let Some(max_id) = messages.last().map(|message| message.id) {
            tx.execute(
                "INSERT INTO brigade_cursors (session_id, last_seen_id) VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET last_seen_id = excluded.last_seen_id",
                params![session_id, max_id],
            )?;
        }
        tx.commit()?;
        Ok(messages)
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
    fn director_message_reaches_the_worker_then_the_cursor_advances() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        // Director sends to the Worker role.
        store
            .enqueue_brigade_message(br, "dir", BrigadeRole::Worker, "please run the tests")
            .unwrap();

        // The Worker pulls it once.
        let got = store
            .fetch_brigade_messages(br, "w1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from_session, "dir");
        assert_eq!(got[0].body, "please run the tests");

        // A second pull returns nothing (its cursor advanced past the message).
        assert!(
            store
                .fetch_brigade_messages(br, "w1", BrigadeRole::Worker)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn messages_are_visible_across_separate_connections() {
        // The Director's and Worker's `banto _mcp` servers are separate
        // processes, each with its own connection to the same sqlite file —
        // this is the medium banto mediates over, so a message one enqueues
        // must be visible to the other's connection.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("banto.db");

        let sender = Store::open(&db).unwrap();
        let br = sender.create_brigade("cell").unwrap();
        sender
            .enqueue_brigade_message(br, "dir", BrigadeRole::Worker, "cross-process")
            .unwrap();

        let mut receiver = Store::open(&db).unwrap();
        let got = receiver
            .fetch_brigade_messages(br, "w1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "cross-process");
    }

    #[test]
    fn a_broadcast_reaches_every_worker_independently() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .enqueue_brigade_message(br, "dir", BrigadeRole::Worker, "stand by")
            .unwrap();

        // Both workers see it — per-session cursors, not a shared delivered flag.
        assert_eq!(
            store
                .fetch_brigade_messages(br, "w1", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .fetch_brigade_messages(br, "w2", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn messages_are_addressed_by_role_and_scoped_to_the_brigade() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        let other = store.create_brigade("other").unwrap();

        // Worker -> Director in this brigade.
        store
            .enqueue_brigade_message(br, "w1", BrigadeRole::Director, "done, deviated because X")
            .unwrap();
        // Noise addressed to the Director of a *different* brigade.
        store
            .enqueue_brigade_message(other, "x", BrigadeRole::Director, "unrelated")
            .unwrap();

        // The Director pulling its own brigade sees only its message...
        let got = store
            .fetch_brigade_messages(br, "dir", BrigadeRole::Director)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "done, deviated because X");

        // ...and a Worker pulling the same brigade sees none (wrong role).
        assert!(
            store
                .fetch_brigade_messages(br, "w1", BrigadeRole::Worker)
                .unwrap()
                .is_empty()
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
