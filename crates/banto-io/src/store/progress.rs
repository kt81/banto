//! A Director's Progress: one per brigade, replaced whole on every write.
//!
//! The rule for what may be written is `banto_core::progress::check_progress`;
//! this module stores what passed it and does not look again at the text. What it
//! does enforce is who writes: the caller (the `update_progress` tool) resolved
//! its role from the store a moment before, and the write asks again, in its own
//! transaction, the way [`Store::set_request`] does.

use std::time::SystemTime;

use rusqlite::{OptionalExtension, params};

use banto_core::model::{BrigadeId, BrigadeRole};
use banto_core::progress::{Progress, ProgressGlance, ProgressRecord};

use super::{Store, StoreError, system_time_to_unix_ms};

fn encode<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value)
        .map_err(|err| StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(err))))
}

fn decode<T: serde::de::DeserializeOwned>(column: usize, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(err),
        )
    })
}

impl Store {
    /// Replaces the brigade's Progress with `progress` and stamps the time. The
    /// sequence goes up by one on every write, so a write that changes nothing is
    /// still a write (the stamp says when the Director last looked at the state).
    /// Returns the new sequence, or `None` when there is no such brigade or
    /// `director_token` is not, inside this transaction, the brigade's Director:
    /// nothing is written then.
    pub fn set_progress(
        &mut self,
        brigade_id: BrigadeId,
        director_token: &str,
        progress: &Progress,
    ) -> Result<Option<i64>, StoreError> {
        let tasks = encode(&progress.tasks)?;
        let off_plan = encode(&progress.off_plan)?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let is_director: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM brigade_members \
             WHERE brigade_id = ?1 AND member_token = ?2 AND role = ?3)",
            params![brigade_id, director_token, BrigadeRole::Director.as_token()],
            |row| row.get(0),
        )?;
        if !is_director {
            return Ok(None);
        }
        // CAST: whatever a hand-edited column holds counts on as an integer.
        tx.execute(
            "INSERT INTO brigade_progress (brigade_id, stage, tasks, off_plan, seq, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5) \
             ON CONFLICT(brigade_id) DO UPDATE SET \
             stage = excluded.stage, tasks = excluded.tasks, off_plan = excluded.off_plan, \
             seq = CAST(brigade_progress.seq AS INTEGER) + 1, \
             updated_at_ms = excluded.updated_at_ms",
            params![
                brigade_id,
                progress.stage,
                tasks,
                off_plan,
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        let seq: i64 = tx.query_row(
            "SELECT CAST(seq AS INTEGER) FROM brigade_progress WHERE brigade_id = ?1",
            [brigade_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(seq))
    }

    /// The brigade's Progress with its sequence and time; `None` when none has been
    /// written. Read by the reading screen only, so an unreadable body (a hand
    /// edit) is an error here and not a missing Progress: the screen says it could
    /// not read it rather than that nothing was written.
    pub fn brigade_progress(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Option<ProgressRecord>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT stage, tasks, off_plan, CAST(seq AS INTEGER), \
                 CAST(updated_at_ms AS INTEGER) \
                 FROM brigade_progress WHERE brigade_id = ?1",
                [brigade_id],
                |row| {
                    let tasks: String = row.get(1)?;
                    let off_plan: String = row.get(2)?;
                    Ok(ProgressRecord {
                        progress: Progress {
                            stage: row.get(0)?,
                            tasks: decode(1, &tasks)?,
                            off_plan: decode(2, &off_plan)?,
                        },
                        seq: row.get(3)?,
                        updated_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// The sequence and stage line of the brigade's Progress, for the tick; `None`
    /// when none has been written. The body is not read: a Progress is up to a few
    /// kilobytes and this runs every second for every brigade.
    pub fn brigade_progress_glance(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Option<ProgressGlance>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT CAST(seq AS INTEGER), stage FROM brigade_progress WHERE brigade_id = ?1",
                [brigade_id],
                |row| {
                    Ok(ProgressGlance {
                        seq: row.get(0)?,
                        stage: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::progress::{ProgressTask, TaskState};

    fn briefed(store: &mut Store) -> BrigadeId {
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        br
    }

    fn progress(stage: &str, tasks: &[(&str, TaskState, &str)], off: &[&str]) -> Progress {
        Progress {
            stage: stage.to_string(),
            tasks: tasks
                .iter()
                .map(|(id, state, note)| ProgressTask {
                    id: (*id).to_string(),
                    state: *state,
                    note: (*note).to_string(),
                })
                .collect(),
            off_plan: off.iter().map(|item| (*item).to_string()).collect(),
        }
    }

    #[test]
    fn no_progress_until_the_director_writes_one() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        assert_eq!(store.brigade_progress(br).unwrap(), None);
        assert_eq!(store.brigade_progress_glance(br).unwrap(), None);
    }

    #[test]
    fn a_write_is_stored_whole_with_its_time_and_japanese_survives() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let written = progress(
            "実装中",
            &[
                ("T1", TaskState::Doing, "保存層"),
                ("T2", TaskState::Todo, ""),
            ],
            &["README を直している"],
        );
        let before = system_time_to_unix_ms(SystemTime::now());
        assert_eq!(
            store.set_progress(br, "director", &written).unwrap(),
            Some(1)
        );
        let record = store.brigade_progress(br).unwrap().unwrap();
        assert_eq!(record.progress, written);
        assert_eq!(record.seq, 1);
        assert!(record.updated_at_ms >= before, "{record:?}");
        let glance = store.brigade_progress_glance(br).unwrap().unwrap();
        assert_eq!((glance.seq, glance.stage.as_str()), (1, "実装中"));
    }

    #[test]
    fn a_second_write_replaces_the_first_whole_and_counts_on() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let first = progress(
            "one",
            &[("T1", TaskState::Doing, "a"), ("T2", TaskState::Todo, "b")],
            &["x", "y"],
        );
        store.set_progress(br, "director", &first).unwrap();
        // Nothing of the first survives: not a task, not an off-plan item.
        let second = progress("two", &[("T3", TaskState::Done, "")], &[]);
        assert_eq!(
            store.set_progress(br, "director", &second).unwrap(),
            Some(2)
        );
        let record = store.brigade_progress(br).unwrap().unwrap();
        assert_eq!(record.progress, second);
        assert_eq!(record.seq, 2);
        // An identical write is still a write.
        assert_eq!(
            store.set_progress(br, "director", &second).unwrap(),
            Some(3)
        );
        // One row per brigade.
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM brigade_progress", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn progress_is_per_brigade() {
        let mut store = Store::open_in_memory().unwrap();
        let a = briefed(&mut store);
        let b = briefed(&mut store);
        store
            .set_progress(a, "director", &progress("a", &[], &[]))
            .unwrap();
        assert_eq!(store.brigade_progress(b).unwrap(), None);
        store
            .set_progress(b, "director", &progress("b", &[], &[]))
            .unwrap();
        assert_eq!(
            store.brigade_progress(a).unwrap().unwrap().progress.stage,
            "a"
        );
        assert_eq!(store.brigade_progress(b).unwrap().unwrap().seq, 1);
    }

    #[test]
    fn only_the_brigades_current_director_writes() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();
        let written = progress("s", &[], &[]);
        assert_eq!(store.set_progress(br, &worker, &written).unwrap(), None);
        assert_eq!(store.set_progress(br, "goinkyo", &written).unwrap(), None);
        // A token that is no member at all, and a brigade that does not exist.
        assert_eq!(store.set_progress(br, "nobody", &written).unwrap(), None);
        assert_eq!(
            store.set_progress(br + 100, "director", &written).unwrap(),
            None
        );
        assert_eq!(store.brigade_progress(br).unwrap(), None);
        assert_eq!(
            store.set_progress(br, "director", &written).unwrap(),
            Some(1)
        );
        // The Director leaves between the tool's check and the write: nothing changes.
        store
            .conn
            .execute(
                "DELETE FROM brigade_members WHERE brigade_id = ?1 AND member_token = 'director'",
                [br],
            )
            .unwrap();
        let later = progress("later", &[], &[]);
        assert_eq!(store.set_progress(br, "director", &later).unwrap(), None);
        assert_eq!(
            store.brigade_progress(br).unwrap().unwrap().progress.stage,
            "s"
        );
    }

    #[test]
    fn disbanding_the_brigade_deletes_its_progress_and_dismissing_a_worker_does_not() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        store
            .set_progress(br, "director", &progress("s", &[], &[]))
            .unwrap();
        store.dismiss_worker(br, &worker).unwrap();
        assert!(store.brigade_progress(br).unwrap().is_some());
        store.delete_brigade(br).unwrap();
        assert_eq!(store.brigade_progress(br).unwrap(), None);
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM brigade_progress", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn the_empty_brigade_janitor_takes_a_stray_progress_with_it() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("residue").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO brigade_progress (brigade_id, stage, tasks, off_plan, seq, updated_at_ms) \
                 VALUES (?1, 's', '[]', '[]', 1, 1)",
                [br],
            )
            .unwrap();
        assert_eq!(store.delete_empty_brigades().unwrap(), 1);
        assert_eq!(store.brigade_progress(br).unwrap(), None);
    }

    #[test]
    fn a_hand_edited_body_fails_the_screen_read_and_not_the_tick_read() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        store
            .set_progress(br, "director", &progress("s", &[], &[]))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE brigade_progress SET tasks = 'not json', seq = 4.5 WHERE brigade_id = ?1",
                [br],
            )
            .unwrap();
        assert!(store.brigade_progress(br).is_err());
        let glance = store.brigade_progress_glance(br).unwrap().unwrap();
        assert_eq!((glance.seq, glance.stage.as_str()), (4, "s"));
        // The next write still counts on from whatever the sequence read as.
        assert_eq!(
            store
                .set_progress(br, "director", &progress("t", &[], &[]))
                .unwrap(),
            Some(5)
        );
    }
}
