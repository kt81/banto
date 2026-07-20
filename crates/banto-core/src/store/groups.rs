//! Session groups and their membership (banto-owned state).
//!
//! A session belongs to at most one group (enforced by `group_members`
//! having `session_id` as its primary key, since schema v3 — see
//! `store::migrations`).

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

    /// Assigns a session to a group, moving it out of whatever group it was
    /// previously in (a session belongs to at most one group). Idempotent
    /// when the session is already in `group_id`.
    pub fn set_session_group(
        &self,
        session_id: &SessionId,
        group_id: GroupId,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO group_members (session_id, group_id) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET group_id = excluded.group_id",
            params![session_id.0, group_id],
        )?;
        Ok(())
    }

    /// Returns the group a session currently belongs to, if any.
    pub fn group_for_session(&self, session_id: &SessionId) -> Result<Option<GroupId>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT group_id FROM group_members WHERE session_id = ?1",
                [&session_id.0],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Removes a session from whatever group it belongs to. A no-op if it
    /// isn't in one.
    pub fn clear_session_group(&self, session_id: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM group_members WHERE session_id = ?1",
            [&session_id.0],
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
    fn set_get_clear_session_group() {
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
        let work = store.create_group("work").unwrap();
        let play = store.create_group("play").unwrap();

        store.set_session_group(&sid("a"), work).unwrap();
        store.set_session_group(&sid("a"), play).unwrap();

        assert_eq!(store.group_for_session(&sid("a")).unwrap(), Some(play));
        assert!(store.group_members(work).unwrap().is_empty());
        assert_eq!(store.group_members(play).unwrap(), [sid("a")]);
    }

    #[test]
    fn group_members_ordered_by_session_id() {
        let store = Store::open_in_memory().unwrap();
        let g = store.create_group("work").unwrap();
        store.set_session_group(&sid("b"), g).unwrap();
        store.set_session_group(&sid("a"), g).unwrap();
        assert_eq!(store.group_members(g).unwrap(), [sid("a"), sid("b")]);
    }

    #[test]
    fn delete_group_removes_members() {
        let mut store = Store::open_in_memory().unwrap();
        let g = store.create_group("work").unwrap();
        store.set_session_group(&sid("a"), g).unwrap();

        store.delete_group(g).unwrap();
        assert!(store.group_members(g).unwrap().is_empty());
        assert!(store.list_groups().unwrap().is_empty());
    }
}
