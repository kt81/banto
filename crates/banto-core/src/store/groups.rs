//! Session groups and their membership (banto-owned state).

use rusqlite::params;

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

    /// Adds a session to a group. Idempotent.
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
}
