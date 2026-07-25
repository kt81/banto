//! Session lineage — resolved auto-compaction parent links
//! (`session_lineage`: child_id -> parent_id). Links are discovered by a
//! streaming uuid scan run elsewhere (banto_io's lineage resolution step,
//! beside the provider); this module only ever inserts and reads —
//! lineage is immutable once resolved, so a row is never updated or
//! deleted.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, params};

use banto_core::model::SessionId;

use super::{Store, StoreError};

/// Hop cap for [`Store::lineage_leaf`]'s child-following walk. Lineage is a
/// DAG by construction (a session can be the auto-compaction continuation
/// of at most one parent), so a cycle should never occur; the cap only
/// guards against banto hanging if the data is ever corrupted.
const LEAF_HOP_CAP: u32 = 16;

impl Store {
    /// Records that `child` is the auto-compaction continuation of
    /// `parent`. Idempotent: an already-known child keeps its original
    /// parent (lineage is resolved once and cached forever).
    pub fn record_lineage(&self, child: &SessionId, parent: &SessionId) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO session_lineage (child_id, parent_id) VALUES (?1, ?2)
             ON CONFLICT(child_id) DO NOTHING",
            params![child.0, parent.0],
        )?;
        Ok(())
    }

    /// True if `child` already has a resolved lineage entry. Used by the
    /// resolution step (banto_io's lineage resolution, beside the
    /// provider) to skip a child that a prior pass already resolved,
    /// without spending its scan budget on it.
    pub fn has_lineage(&self, child: &SessionId) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM session_lineage WHERE child_id = ?1",
                [&child.0],
                |_row| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Returns every session id with a known continuation, i.e. every
    /// `parent_id` recorded in the lineage table — the set of sessions a
    /// caller should treat as superseded.
    pub fn lineage_parent_ids(&self) -> Result<HashSet<SessionId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT parent_id FROM session_lineage")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0).map(SessionId))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    /// Follows child links (parent -> child, the reverse of how a link is
    /// discovered) from `id` to its newest known continuation. Returns
    /// `id` itself if it has no known continuation. Capped at
    /// [`LEAF_HOP_CAP`] hops against a cycle, returning the last id
    /// reached if the cap is hit.
    pub fn lineage_leaf(&self, id: &SessionId) -> Result<SessionId, StoreError> {
        let mut current = id.0.clone();
        for _ in 0..LEAF_HOP_CAP {
            let next: Option<String> = self
                .conn
                .query_row(
                    "SELECT child_id FROM session_lineage WHERE parent_id = ?1",
                    [&current],
                    |row| row.get(0),
                )
                .optional()?;
            match next {
                Some(child) => current = child,
                None => break,
            }
        }
        Ok(SessionId(current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    #[test]
    fn record_and_query_parent_ids() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.lineage_parent_ids().unwrap().is_empty());

        store
            .record_lineage(&sid("child-a"), &sid("parent-a"))
            .unwrap();
        store
            .record_lineage(&sid("child-b"), &sid("parent-b"))
            .unwrap();
        assert_eq!(
            store.lineage_parent_ids().unwrap(),
            HashSet::from([sid("parent-a"), sid("parent-b")])
        );
    }

    #[test]
    fn record_lineage_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_lineage(&sid("child"), &sid("parent-1"))
            .unwrap();
        // A second call for the same child must not error or overwrite —
        // lineage is resolved once and cached forever.
        store
            .record_lineage(&sid("child"), &sid("parent-2"))
            .unwrap();
        assert_eq!(store.lineage_leaf(&sid("parent-1")).unwrap(), sid("child"));
        assert_eq!(
            store.lineage_leaf(&sid("parent-2")).unwrap(),
            sid("parent-2")
        );
    }

    #[test]
    fn leaf_follows_a_two_hop_chain() {
        let store = Store::open_in_memory().unwrap();
        store.record_lineage(&sid("b"), &sid("a")).unwrap();
        store.record_lineage(&sid("c"), &sid("b")).unwrap();

        assert_eq!(store.lineage_leaf(&sid("a")).unwrap(), sid("c"));
        assert_eq!(store.lineage_leaf(&sid("b")).unwrap(), sid("c"));
        assert_eq!(store.lineage_leaf(&sid("c")).unwrap(), sid("c"));
        // An id with no lineage at all is its own leaf.
        assert_eq!(
            store.lineage_leaf(&sid("unrelated")).unwrap(),
            sid("unrelated")
        );
    }

    #[test]
    fn has_lineage_reflects_resolved_children_only() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.has_lineage(&sid("child")).unwrap());
        store.record_lineage(&sid("child"), &sid("parent")).unwrap();
        assert!(store.has_lineage(&sid("child")).unwrap());
        // A parent with no continuation of its own is not itself "resolved".
        assert!(!store.has_lineage(&sid("parent")).unwrap());
    }

    #[test]
    fn leaf_following_is_capped_against_a_cycle() {
        let store = Store::open_in_memory().unwrap();
        // A cycle should never occur in real data, but the cap must still
        // terminate rather than hang if it somehow does.
        store.record_lineage(&sid("a"), &sid("b")).unwrap();
        store.record_lineage(&sid("b"), &sid("a")).unwrap();

        let leaf = store.lineage_leaf(&sid("a")).unwrap();
        assert!(leaf == sid("a") || leaf == sid("b"));
    }
}
