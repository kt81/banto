//! Session groups and their membership (banto-owned state).
//!
//! `group_members` is a plain many-to-many join table (a session could in
//! principle join more than one group via [`add_group_member`]). The
//! group-join UX wants a session in exactly one group at a time; that's
//! layered on top via [`set_session_group`]/[`group_for_session`]/
//! [`clear_session_group`], which move a session by clearing its existing
//! memberships first, rather than a schema-level constraint.

use rusqlite::{OptionalExtension, params};

use crate::model::SessionId;

use super::{Store, StoreError};

/// Row id of a group (sqlite AUTOINCREMENT primary key).
pub type GroupId = i64;

/// A user-defined session group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    /// Manual ordering key; lower sorts first. Defaults to 0 on creation.
    pub sort_order: i64,
}

impl Store {
    /// Creates a group and returns its id. Group names are unique; creating a
    /// duplicate name fails with a constraint error.
    pub fn create_group(&self, name: &str) -> Result<GroupId, StoreError> {
        self.conn
            .execute("INSERT INTO groups (name) VALUES (?1)", [name])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Renames a group. Renaming a nonexistent group is a no-op; renaming to
    /// an existing name fails with a constraint error.
    pub fn rename_group(&self, id: GroupId, new_name: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE groups SET name = ?1 WHERE id = ?2",
            params![new_name, id],
        )?;
        Ok(())
    }

    /// Deletes a group together with its membership rows.
    pub fn delete_group(&mut self, id: GroupId) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM group_members WHERE group_id = ?1", [id])?;
        tx.execute("DELETE FROM groups WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Returns all groups ordered by `sort_order`, then name.
    pub fn list_groups(&self) -> Result<Vec<Group>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, sort_order FROM groups ORDER BY sort_order, name")?;
        let rows = stmt.query_map([], |row| {
            Ok(Group {
                id: row.get(0)?,
                name: row.get(1)?,
                sort_order: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Adds a session to a group. Idempotent. A session can belong to more
    /// than one group through this primitive; see the module docs for the
    /// single-membership layer built on top of it.
    pub fn add_group_member(
        &self,
        group_id: GroupId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO group_members (group_id, session_id) VALUES (?1, ?2)",
            params![group_id, session_id.0],
        )?;
        Ok(())
    }

    /// Removes a session from a group. Removing a non-member is a no-op.
    pub fn remove_group_member(
        &self,
        group_id: GroupId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1 AND session_id = ?2",
            params![group_id, session_id.0],
        )?;
        Ok(())
    }

    /// Returns the session ids in a group, ordered by id.
    pub fn group_members(&self, group_id: GroupId) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM group_members WHERE group_id = ?1 ORDER BY session_id",
        )?;
        let rows = stmt.query_map([group_id], |row| row.get::<_, String>(0).map(SessionId))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Assigns a session to exactly one group: transactionally removes any
    /// existing memberships for that session (there may be more than one if
    /// [`add_group_member`] was used directly), then adds it to `group_id`.
    /// Idempotent when the session is already only in `group_id`.
    pub fn set_session_group(
        &mut self,
        session_id: &SessionId,
        group_id: GroupId,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM group_members WHERE session_id = ?1",
            [&session_id.0],
        )?;
        tx.execute(
            "INSERT INTO group_members (group_id, session_id) VALUES (?1, ?2)",
            params![group_id, session_id.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns the group a session currently belongs to, if any. If the
    /// session is (unusually) a member of more than one group via
    /// [`add_group_member`], returns the lowest group id, deterministically.
    pub fn group_for_session(&self, session_id: &SessionId) -> Result<Option<GroupId>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT group_id FROM group_members WHERE session_id = ?1
                 ORDER BY group_id LIMIT 1",
                [&session_id.0],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Removes a session from every group it belongs to. A no-op if it isn't
    /// in any.
    pub fn clear_session_group(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM group_members WHERE session_id = ?1",
            [&session_id.0],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn create_list_rename_delete() {
        let mut store = Store::open_in_memory().unwrap();
        let work = store.create_group("work").unwrap();
        let play = store.create_group("play").unwrap();
        assert_ne!(work, play);

        let names: Vec<String> = store
            .list_groups()
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["play", "work"]); // same sort_order, name order

        store.rename_group(play, "hobby").unwrap();
        let names: Vec<String> = store
            .list_groups()
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["hobby", "work"]);

        store.delete_group(work).unwrap();
        let names: Vec<String> = store
            .list_groups()
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["hobby"]);
    }

    #[test]
    fn duplicate_group_name_errors() {
        let store = Store::open_in_memory().unwrap();
        store.create_group("work").unwrap();
        assert!(store.create_group("work").is_err());
    }

    #[test]
    fn membership_add_remove_list() {
        let store = Store::open_in_memory().unwrap();
        let g = store.create_group("work").unwrap();

        store.add_group_member(g, &sid("b")).unwrap();
        store.add_group_member(g, &sid("a")).unwrap();
        // Idempotent add.
        store.add_group_member(g, &sid("a")).unwrap();
        assert_eq!(store.group_members(g).unwrap(), [sid("a"), sid("b")]);

        store.remove_group_member(g, &sid("a")).unwrap();
        assert_eq!(store.group_members(g).unwrap(), [sid("b")]);

        // Removing a non-member is a no-op.
        store.remove_group_member(g, &sid("zzz")).unwrap();
        assert_eq!(store.group_members(g).unwrap(), [sid("b")]);
    }

    #[test]
    fn delete_group_removes_members() {
        let mut store = Store::open_in_memory().unwrap();
        let g = store.create_group("work").unwrap();
        store.add_group_member(g, &sid("a")).unwrap();

        store.delete_group(g).unwrap();
        assert!(store.group_members(g).unwrap().is_empty());
        assert!(store.list_groups().unwrap().is_empty());
    }

    #[test]
    fn set_get_clear_session_group() {
        let mut store = Store::open_in_memory().unwrap();
        let work = store.create_group("work").unwrap();

        assert_eq!(store.group_for_session(&sid("a")).unwrap(), None);

        store.set_session_group(&sid("a"), work).unwrap();
        assert_eq!(store.group_for_session(&sid("a")).unwrap(), Some(work));
        assert_eq!(store.group_members(work).unwrap(), [sid("a")]);

        // Idempotent when re-set to the same group.
        store.set_session_group(&sid("a"), work).unwrap();
        assert_eq!(store.group_members(work).unwrap(), [sid("a")]);

        store.clear_session_group(&sid("a")).unwrap();
        assert_eq!(store.group_for_session(&sid("a")).unwrap(), None);
        assert!(store.group_members(work).unwrap().is_empty());

        // Clearing a session that isn't in any group is a no-op.
        store.clear_session_group(&sid("never-assigned")).unwrap();
    }

    #[test]
    fn set_session_group_moves_between_groups() {
        let mut store = Store::open_in_memory().unwrap();
        let work = store.create_group("work").unwrap();
        let play = store.create_group("play").unwrap();

        store.set_session_group(&sid("a"), work).unwrap();
        store.set_session_group(&sid("a"), play).unwrap();

        assert_eq!(store.group_for_session(&sid("a")).unwrap(), Some(play));
        assert!(store.group_members(work).unwrap().is_empty());
        assert_eq!(store.group_members(play).unwrap(), [sid("a")]);
    }

    /// Proves `set_session_group`'s "remove all existing memberships" step
    /// really is transactional over every prior membership, including a
    /// session that ended up in more than one group via the lower-level
    /// `add_group_member` primitive (which `set_session_group` itself never
    /// creates, but doesn't assume away either).
    #[test]
    fn set_session_group_clears_prior_multi_group_membership() {
        let mut store = Store::open_in_memory().unwrap();
        let work = store.create_group("work").unwrap();
        let play = store.create_group("play").unwrap();
        let home = store.create_group("home").unwrap();

        store.add_group_member(work, &sid("a")).unwrap();
        store.add_group_member(play, &sid("a")).unwrap();

        store.set_session_group(&sid("a"), home).unwrap();

        assert_eq!(store.group_for_session(&sid("a")).unwrap(), Some(home));
        assert!(store.group_members(work).unwrap().is_empty());
        assert!(store.group_members(play).unwrap().is_empty());
        assert_eq!(store.group_members(home).unwrap(), [sid("a")]);
    }
}
