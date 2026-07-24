//! Brigades: an internal operational cell of one Director session and one or
//! more Worker sessions, hosted together as tiled panes in the emporium mode.
//!
//! A brigade is a *separate* concept from a group: a group is the user's own
//! filing (project / phase), while a brigade is a live operational unit — a
//! Director commanding Worker(s). It is an internal term, never surfaced to the
//! user.
//!
//! `brigade_members` is keyed by a banto-owned `member_token` ('director',
//! 'worker-1', 'worker-2', ...) rather than the Claude session id itself: a
//! Worker is formed *before* Claude assigns it a session id (banto
//! auto-spawns a fresh `claude` process and only later discovers its id —
//! see `crate::embedded::emporium`'s `PendingNew` flow), so the id has to be
//! a nullable column (`claude_session_id`) filled in after the fact, not the
//! primary identity. Two policies are layered here in code rather than
//! enforced by the schema:
//! - a brigade has exactly one Director — that rule lives in the formation
//!   layer (the emporium), not this table;
//! - a Claude session belongs to at most one brigade at a time — also the
//!   formation layer's job (it only ever forms a brigade from a session with
//!   no existing membership, and Workers are always freshly spawned).

use std::time::SystemTime;

use rusqlite::{OptionalExtension, params};

use banto_core::model::{
    Brigade, BrigadeId, BrigadeMember, BrigadeMessage, BrigadeRole, MemberToken, SessionId,
};

use super::{Store, StoreError, system_time_to_unix_ms};

impl Store {
    /// Creates an (empty) brigade and returns its id.
    pub fn create_brigade(&self, name: &str) -> Result<BrigadeId, StoreError> {
        self.conn.execute(
            "INSERT INTO brigades (name, created_at_ms) VALUES (?1, ?2)",
            params![name, system_time_to_unix_ms(SystemTime::now())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Deletes a brigade together with its membership, queued messages, and
    /// read cursors — nothing is left orphaned for a deleted brigade id to
    /// accumulate under. (Pruning *read* messages out of a still-live brigade
    /// is a separate concern, left for later: this only clears a brigade that
    /// is gone entirely.)
    pub fn delete_brigade(&mut self, id: BrigadeId) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM brigade_members WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigade_messages WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigade_cursors WHERE brigade_id = ?1", [id])?;
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

    /// Adds `token` to `brigade_id` with `role`, optionally already knowing
    /// its Claude session id (the Director always does, at formation time;
    /// a freshly-spawned Worker starts with `None`, filled in later via
    /// [`Self::set_member_claude_session`] once Claude assigns one). Also
    /// seeds the member's cursor to "now" (the current max message id), so
    /// it sees only messages enqueued *after* joining.
    pub fn add_brigade_member(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        role: BrigadeRole,
        claude_session_id: Option<&SessionId>,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO brigade_members (brigade_id, member_token, role, claude_session_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                brigade_id,
                token,
                role.as_token(),
                claude_session_id.map(|s| s.0.as_str())
            ],
        )?;
        tx.execute(
            "INSERT INTO brigade_cursors (brigade_id, member_token, last_seen_id)
             VALUES (?1, ?2, COALESCE((SELECT MAX(id) FROM brigade_messages), 0))",
            params![brigade_id, token],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the Claude session id Claude assigned to a member banto
    /// spawned ahead of time (a Worker's `claude_session_id`, initially
    /// `None`). A no-op if `(brigade_id, token)` doesn't exist.
    pub fn set_member_claude_session(
        &self,
        brigade_id: BrigadeId,
        token: &str,
        claude_session_id: &SessionId,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE brigade_members SET claude_session_id = ?1
             WHERE brigade_id = ?2 AND member_token = ?3",
            params![claude_session_id.0, brigade_id, token],
        )?;
        Ok(())
    }

    /// Removes a member from a brigade together with its cursor. Removing a
    /// non-member is a no-op.
    pub fn remove_brigade_member(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM brigade_members WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, token],
        )?;
        tx.execute(
            "DELETE FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, token],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns a brigade's members: the Director first, then Workers ordered
    /// by token (`worker-1`, `worker-2`, ... — lexicographic ordering is only
    /// numerically correct up to single-digit tokens, which the emporium's
    /// worker-count clamp of 1..=8 guarantees).
    pub fn brigade_members(&self, brigade_id: BrigadeId) -> Result<Vec<BrigadeMember>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT member_token, role, claude_session_id FROM brigade_members
             WHERE brigade_id = ?1
             ORDER BY CASE role WHEN 'director' THEN 0 ELSE 1 END, member_token",
        )?;
        let rows = stmt.query_map([brigade_id], |row| {
            let token: String = row.get(0)?;
            let role: String = row.get(1)?;
            let claude_session_id: Option<String> = row.get(2)?;
            Ok(BrigadeMember {
                token,
                role: BrigadeRole::from_token(&role),
                claude_session_id: claude_session_id.map(SessionId),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Looks up one member of `brigade_id` by its token.
    pub fn brigade_member(
        &self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<Option<BrigadeMember>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT member_token, role, claude_session_id FROM brigade_members
                 WHERE brigade_id = ?1 AND member_token = ?2",
                params![brigade_id, token],
                |row| {
                    let token: String = row.get(0)?;
                    let role: String = row.get(1)?;
                    let claude_session_id: Option<String> = row.get(2)?;
                    Ok(BrigadeMember {
                        token,
                        role: BrigadeRole::from_token(&role),
                        claude_session_id: claude_session_id.map(SessionId),
                    })
                },
            )
            .optional()?)
    }

    /// Returns the brigade a Claude session belongs to, its token, and its
    /// role there, if any. If the session is (unusually) linked from more
    /// than one member, returns the lowest brigade id, deterministically.
    pub fn brigade_of_claude_session(
        &self,
        claude_session_id: &SessionId,
    ) -> Result<Option<(BrigadeId, MemberToken, BrigadeRole)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT brigade_id, member_token, role FROM brigade_members
                 WHERE claude_session_id = ?1
                 ORDER BY brigade_id LIMIT 1",
                [&claude_session_id.0],
                |row| {
                    let id: BrigadeId = row.get(0)?;
                    let token: String = row.get(1)?;
                    let role: String = row.get(2)?;
                    Ok((id, token, BrigadeRole::from_token(&role)))
                },
            )
            .optional()?)
    }

    /// Every known Worker's Claude session id, across every brigade, that has
    /// been assigned one so far (a Worker banto is still waiting on Claude to
    /// assign one has no entry). Used to hide Workers from the session list.
    pub fn brigade_worker_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT claude_session_id FROM brigade_members
             WHERE role = 'worker' AND claude_session_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok(SessionId(row.get(0)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every known Director's Claude session id, across every brigade. Used
    /// to mark Directors in the session list.
    pub fn brigade_director_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT claude_session_id FROM brigade_members
             WHERE role = 'director' AND claude_session_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok(SessionId(row.get(0)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Enqueue a message in `brigade_id` from `from_token`, addressed to
    /// `to_role` and, within that role, either every member (`to_member =
    /// None`, a broadcast — the original semantics) or one specific member
    /// (`to_member = Some(token)`) — so with 2+ Workers, an instruction meant
    /// for `worker-1` doesn't also nudge and occupy `worker-2`. Returns the
    /// new message's queue id.
    pub fn enqueue_brigade_message(
        &self,
        brigade_id: BrigadeId,
        from_token: &str,
        to_role: BrigadeRole,
        to_member: Option<&str>,
        body: &str,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO brigade_messages (brigade_id, from_session, to_role, to_member, body, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                brigade_id,
                from_token,
                to_role.as_token(),
                to_member,
                body,
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Pull the messages in `brigade_id` addressed to `recipient_role` (and,
    /// within that role, either broadcast or specifically to `member_token` —
    /// see [`Self::enqueue_brigade_message`]) that `member_token` has not
    /// seen yet (id past its cursor), oldest first, and advance that
    /// member's cursor past them — so a later call returns only what has
    /// arrived since. The cursor is scoped to `(brigade_id, member_token)`,
    /// so it doesn't carry over if the token's underlying Claude session
    /// later changes, and per-member cursors mean each recipient of a
    /// broadcast sees it independently.
    pub fn fetch_brigade_messages(
        &mut self,
        brigade_id: BrigadeId,
        member_token: &str,
        recipient_role: BrigadeRole,
    ) -> Result<Vec<BrigadeMessage>, StoreError> {
        let tx = self.conn.transaction()?;
        let cursor: i64 = tx
            .query_row(
                "SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
                params![brigade_id, member_token],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let messages = {
            let mut stmt = tx.prepare(
                "SELECT id, from_session, body, to_member FROM brigade_messages
                 WHERE brigade_id = ?1 AND to_role = ?2 AND id > ?3
                   AND (to_member IS NULL OR to_member = ?4)
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(
                params![brigade_id, recipient_role.as_token(), cursor, member_token],
                |row| {
                    Ok(BrigadeMessage {
                        id: row.get(0)?,
                        from_token: row.get(1)?,
                        body: row.get(2)?,
                        to_member: row.get(3)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if let Some(max_id) = messages.last().map(|message| message.id) {
            tx.execute(
                "INSERT INTO brigade_cursors (brigade_id, member_token, last_seen_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(brigade_id, member_token) DO UPDATE SET last_seen_id = excluded.last_seen_id",
                params![brigade_id, member_token, max_id],
            )?;
        }
        tx.commit()?;
        Ok(messages)
    }

    /// Janitor: deletes every brigade with zero `brigade_members` rows —
    /// legacy data from before disband purged properly, or residue from a
    /// crash between [`Self::create_brigade`] and its first
    /// [`Self::add_brigade_member`] — together with any stray
    /// `brigade_messages`/`brigade_cursors` rows left under those brigade
    /// ids. A populated brigade, and all its rows, is left untouched.
    /// Returns how many brigades were removed.
    pub fn delete_empty_brigades(&mut self) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        let empty_ids: Vec<BrigadeId> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM brigades
                 WHERE id NOT IN (SELECT DISTINCT brigade_id FROM brigade_members)",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in &empty_ids {
            tx.execute("DELETE FROM brigade_messages WHERE brigade_id = ?1", [id])?;
            tx.execute("DELETE FROM brigade_cursors WHERE brigade_id = ?1", [id])?;
            tx.execute("DELETE FROM brigades WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(empty_ids.len())
    }

    /// Whether `member_token` in `brigade_id` has any message addressed to
    /// `recipient_role` — and, within that role, either broadcast or
    /// specifically to `member_token` — past its read cursor. Same
    /// visibility rule as [`Self::fetch_brigade_messages`] (this is
    /// load-bearing, not cosmetic: without the `to_member` condition the
    /// relay engine would nudge e.g. `worker-2` forever over a message
    /// addressed to `worker-1` that `worker-2` can never actually pull),
    /// but EXISTS-only and, critically, never advances the cursor. The
    /// emporium's relay engine polls this roughly once a second to decide
    /// whether to nudge an idle member; consuming the cursor here would make
    /// that polling itself the thing that swallows messages before the
    /// member ever pulls them.
    pub fn has_unseen_brigade_messages(
        &self,
        brigade_id: BrigadeId,
        member_token: &str,
        recipient_role: BrigadeRole,
    ) -> Result<bool, StoreError> {
        let cursor: i64 = self
            .conn
            .query_row(
                "SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
                params![brigade_id, member_token],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM brigade_messages
             WHERE brigade_id = ?1 AND to_role = ?2 AND id > ?3
               AND (to_member IS NULL OR to_member = ?4))",
            params![brigade_id, recipient_role.as_token(), cursor, member_token],
            |row| row.get(0),
        )?)
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
    fn members_list_director_first_then_workers_by_token() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, Some(&sid("w2")))
            .unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();

        let members = store.brigade_members(br).unwrap();
        assert_eq!(
            members,
            [
                BrigadeMember {
                    token: "director".to_string(),
                    role: BrigadeRole::Director,
                    claude_session_id: Some(sid("dir")),
                },
                BrigadeMember {
                    token: "worker-1".to_string(),
                    role: BrigadeRole::Worker,
                    claude_session_id: Some(sid("w1")),
                },
                BrigadeMember {
                    token: "worker-2".to_string(),
                    role: BrigadeRole::Worker,
                    claude_session_id: Some(sid("w2")),
                },
            ]
        );
    }

    #[test]
    fn worker_can_be_added_with_no_claude_session_yet() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.claude_session_id, None);

        store
            .set_member_claude_session(br, "worker-1", &sid("w1"))
            .unwrap();
        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.claude_session_id, Some(sid("w1")));
    }

    #[test]
    fn brigade_of_claude_session_reports_membership_token_and_role() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        assert_eq!(store.brigade_of_claude_session(&sid("dir")).unwrap(), None);

        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        assert_eq!(
            store.brigade_of_claude_session(&sid("dir")).unwrap(),
            Some((br, "director".to_string(), BrigadeRole::Director))
        );
    }

    #[test]
    fn brigade_of_claude_session_is_none_for_a_worker_awaiting_discovery() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        assert_eq!(store.brigade_of_claude_session(&sid("w1")).unwrap(), None);
    }

    #[test]
    fn worker_and_director_session_id_sets_are_reported_separately() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();
        // Still awaiting discovery: contributes no id to either set.
        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, None)
            .unwrap();

        assert_eq!(store.brigade_worker_session_ids().unwrap(), [sid("w1")]);
        assert_eq!(store.brigade_director_session_ids().unwrap(), [sid("dir")]);
    }

    #[test]
    fn director_message_reaches_the_worker_then_the_cursor_advances() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        // Director sends to the Worker role.
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Worker,
                None,
                "please run the tests",
            )
            .unwrap();

        // The Worker pulls it once.
        let got = store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from_token, "director");
        assert_eq!(got[0].body, "please run the tests");
        assert_eq!(got[0].to_member, None);

        // A second pull returns nothing (its cursor advanced past the message).
        assert!(
            store
                .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
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
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "cross-process")
            .unwrap();

        let mut receiver = Store::open(&db).unwrap();
        let got = receiver
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "cross-process");
    }

    #[test]
    fn a_broadcast_reaches_every_worker_independently() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "stand by")
            .unwrap();

        // Both workers see it — per-member cursors, not a shared delivered flag.
        assert_eq!(
            store
                .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .fetch_brigade_messages(br, "worker-2", BrigadeRole::Worker)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_member_addressed_message_is_invisible_to_a_different_member_of_the_same_role() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Worker,
                Some("worker-1"),
                "only for worker-1",
            )
            .unwrap();

        let for_worker_1 = store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(for_worker_1.len(), 1);
        assert_eq!(for_worker_1[0].body, "only for worker-1");
        assert_eq!(for_worker_1[0].to_member.as_deref(), Some("worker-1"));

        // worker-2 shares the role but is not the addressee: invisible to it,
        // now and forever (its cursor never advances past this message, but
        // the (to_role, to_member) filter excludes it regardless of cursor).
        assert!(
            store
                .fetch_brigade_messages(br, "worker-2", BrigadeRole::Worker)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn has_unseen_respects_member_addressing_not_just_role() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Worker,
                Some("worker-1"),
                "only for worker-1",
            )
            .unwrap();

        // Without this, the relay engine would nudge worker-2 forever over a
        // message it can never pull.
        assert!(
            store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
        assert!(
            !store
                .has_unseen_brigade_messages(br, "worker-2", BrigadeRole::Worker)
                .unwrap()
        );
    }

    #[test]
    fn messages_are_addressed_by_role_and_scoped_to_the_brigade() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        let other = store.create_brigade("other").unwrap();

        // Worker -> Director in this brigade.
        store
            .enqueue_brigade_message(
                br,
                "worker-1",
                BrigadeRole::Director,
                None,
                "done, deviated because X",
            )
            .unwrap();
        // Noise addressed to the Director of a *different* brigade.
        store
            .enqueue_brigade_message(other, "worker-1", BrigadeRole::Director, None, "unrelated")
            .unwrap();

        // The Director pulling its own brigade sees only its message...
        let got = store
            .fetch_brigade_messages(br, "director", BrigadeRole::Director)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "done, deviated because X");

        // ...and a Worker pulling the same brigade sees none (wrong role).
        assert!(
            store
                .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remove_member_and_delete_clear_membership() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w")))
            .unwrap();

        store.remove_brigade_member(br, "worker-1").unwrap();
        assert_eq!(
            store.brigade_members(br).unwrap(),
            [BrigadeMember {
                token: "director".to_string(),
                role: BrigadeRole::Director,
                claude_session_id: Some(sid("dir")),
            }]
        );
        // Removing a non-member is a no-op.
        store.remove_brigade_member(br, "nobody").unwrap();

        store.delete_brigade(br).unwrap();
        assert!(store.brigade_members(br).unwrap().is_empty());
        assert_eq!(store.brigade_of_claude_session(&sid("dir")).unwrap(), None);
    }

    #[test]
    fn joining_a_brigade_only_sees_messages_enqueued_after_joining() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        // Enqueued before "worker-1" joins...
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "before joining")
            .unwrap();

        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w")))
            .unwrap();

        // ...enqueued after it joins.
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "after joining")
            .unwrap();

        // Only the post-join message is delivered.
        let got = store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "after joining");
    }

    #[test]
    fn remove_brigade_member_deletes_its_cursor_too() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w")))
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "seen")
            .unwrap();
        store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();

        store.remove_brigade_member(br, "worker-1").unwrap();

        let cursor: Option<i64> = store
            .conn
            .query_row(
                "SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
                params![br, "worker-1"],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(cursor, None);
    }

    #[test]
    fn delete_empty_brigades_purges_membership_free_brigades_and_their_stray_rows() {
        let mut store = Store::open_in_memory().unwrap();
        let empty = store.create_brigade("empty").unwrap();
        let populated = store.create_brigade("populated").unwrap();
        store
            .add_brigade_member(
                populated,
                "director",
                BrigadeRole::Director,
                Some(&sid("dir")),
            )
            .unwrap();

        // Stray rows under the empty brigade (e.g. a crash mid-formation, or
        // legacy pre-v7 data) that a naive `DELETE FROM brigades` would leave
        // orphaned.
        store
            .enqueue_brigade_message(empty, "director", BrigadeRole::Worker, None, "orphaned")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO brigade_cursors (brigade_id, member_token, last_seen_id)
                 VALUES (?1, ?2, ?3)",
                params![empty, "stray", 0],
            )
            .unwrap();

        let removed = store.delete_empty_brigades().unwrap();
        assert_eq!(removed, 1);

        assert_eq!(
            store.list_brigades().unwrap(),
            [Brigade {
                id: populated,
                name: "populated".to_string(),
            }]
        );
        let messages: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_messages WHERE brigade_id = ?1",
                [empty],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(messages, 0);
        let cursors: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_cursors WHERE brigade_id = ?1",
                [empty],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursors, 0);

        // The populated brigade and its rows are untouched.
        assert_eq!(store.brigade_members(populated).unwrap().len(), 1);
    }

    #[test]
    fn delete_empty_brigades_returns_zero_when_none_are_empty() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();

        assert_eq!(store.delete_empty_brigades().unwrap(), 0);
        assert_eq!(store.list_brigades().unwrap().len(), 1);
    }

    #[test]
    fn has_unseen_brigade_messages_true_after_enqueue_false_after_fetch() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();

        assert!(
            !store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );

        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();
        assert!(
            store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );

        store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert!(
            !store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
    }

    #[test]
    fn has_unseen_brigade_messages_never_advances_the_cursor() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();

        // Polling repeatedly (as the relay engine's ~1/s tick does) must not
        // consume the message — a later real fetch must still see it.
        for _ in 0..3 {
            assert!(
                store
                    .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                    .unwrap()
            );
        }
        let got = store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn has_unseen_brigade_messages_is_scoped_by_brigade_and_role() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("a").unwrap();
        let b = store.create_brigade("b").unwrap();
        store
            .add_brigade_member(a, "worker-1", BrigadeRole::Worker, Some(&sid("wa")))
            .unwrap();
        store
            .add_brigade_member(b, "worker-1", BrigadeRole::Worker, Some(&sid("wb")))
            .unwrap();
        store
            .add_brigade_member(a, "director", BrigadeRole::Director, Some(&sid("da")))
            .unwrap();

        store
            .enqueue_brigade_message(a, "director", BrigadeRole::Worker, None, "for a's worker")
            .unwrap();

        assert!(
            store
                .has_unseen_brigade_messages(a, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
        // Same token, different brigade: unaffected.
        assert!(
            !store
                .has_unseen_brigade_messages(b, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
        // Same brigade, wrong role: unaffected.
        assert!(
            !store
                .has_unseen_brigade_messages(a, "director", BrigadeRole::Director)
                .unwrap()
        );
    }

    #[test]
    fn delete_brigade_purges_its_messages_and_cursors_but_not_another_brigades() {
        let mut store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("a").unwrap();
        let b = store.create_brigade("b").unwrap();

        store
            .add_brigade_member(a, "worker-1", BrigadeRole::Worker, Some(&sid("w-a")))
            .unwrap();
        store
            .add_brigade_member(b, "worker-1", BrigadeRole::Worker, Some(&sid("w-b")))
            .unwrap();
        store
            .enqueue_brigade_message(a, "director", BrigadeRole::Worker, None, "in A")
            .unwrap();
        store
            .enqueue_brigade_message(b, "director", BrigadeRole::Worker, None, "in B")
            .unwrap();
        store
            .fetch_brigade_messages(a, "worker-1", BrigadeRole::Worker)
            .unwrap();
        store
            .fetch_brigade_messages(b, "worker-1", BrigadeRole::Worker)
            .unwrap();

        store.delete_brigade(a).unwrap();

        let messages_a: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_messages WHERE brigade_id = ?1",
                [a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(messages_a, 0);
        let cursors_a: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_cursors WHERE brigade_id = ?1",
                [a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursors_a, 0);

        // Brigade B's rows are untouched.
        let messages_b: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_messages WHERE brigade_id = ?1",
                [b],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(messages_b, 1);
        let cursors_b: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_cursors WHERE brigade_id = ?1",
                [b],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursors_b, 1);
    }
}
