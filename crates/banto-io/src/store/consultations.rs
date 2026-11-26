//! What a Goinkyo's session was called, kept after its consultation ended.
//!
//! The title lives on the Goinkyo's member row while the consultation is open
//! (`brigade_members.consult_title`, written with the row by
//! [`Store::add_goinkyo_member`]). Ending the consultation deletes that row, so
//! the same transaction copies the title here, keyed by the session, which is
//! the only thing the session list can look a row up by.
//!
//! Not a column on `archived`: `D` deletes the archived row, and the title has
//! to survive the operator restoring the session — that is the moment it is
//! wanted. And a row here is also the one fact that says banto ended this
//! session's consultation: the archived table cannot tell banto's archive from
//! the operator's `d`, this one can (see [`Store::consultation_titles`]).

use std::time::SystemTime;

use rusqlite::params;

use banto_core::model::{AgentKind, BrigadeId, BrigadeRole, GOINKYO_TOKEN, SessionId};

use super::brigades::insert_member;
use super::{Store, StoreError, system_time_to_unix_ms};

/// Records that `session_id` was a `role`'s session called `title`, ended now, on
/// whatever connection or transaction the caller holds. `role` is the Goinkyo's or
/// the Planner's: the list labels them differently.
///
/// `ON CONFLICT DO NOTHING`, like [`super::archive::archive_on`]: a session ends
/// once, and the first record is the one that stands.
pub(super) fn record_on(
    conn: &rusqlite::Connection,
    session_id: &SessionId,
    role: BrigadeRole,
    title: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO consultations (session_id, title, ended_at_ms, role) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING",
        params![
            session_id.0,
            title,
            system_time_to_unix_ms(SystemTime::now()),
            role.as_token()
        ],
    )?;
    Ok(())
}

impl Store {
    /// Adds the brigade's Goinkyo, carrying the title its Director gave the
    /// consultation, in one transaction. The title is stored as given: the
    /// caller must have reduced and checked it with
    /// `banto_core::consultation::check_title`, the single gate: an empty or
    /// over-wide title is accepted here, and it would be listed as it is.
    ///
    /// The Goinkyo's token is the fixed `goinkyo`, reused by every consultation,
    /// so a title belongs to the row and never to the token: ending the
    /// consultation removes the row and copies the title out
    /// ([`Self::dismiss_worker`], [`Self::delete_brigade`]), and the next
    /// consultation starts from nothing.
    pub fn add_goinkyo_member(
        &mut self,
        brigade_id: BrigadeId,
        title: &str,
    ) -> Result<(), StoreError> {
        self.add_goinkyo_member_with_agent(brigade_id, title, Some(AgentKind::ClaudeCode))
    }

    /// [`Self::add_goinkyo_member`] recording the product the Goinkyo will run as,
    /// which is what the emporium spawns it as. `None` leaves the row without a
    /// product, for the emporium to resolve from the setting and its Director.
    pub fn add_goinkyo_member_with_agent(
        &mut self,
        brigade_id: BrigadeId,
        title: &str,
        agent: Option<AgentKind>,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        insert_member(
            &tx,
            brigade_id,
            GOINKYO_TOKEN,
            BrigadeRole::Goinkyo,
            None,
            agent,
        )?;
        tx.execute(
            "UPDATE brigade_members SET consult_title = ?3
             WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, GOINKYO_TOKEN, title],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every ended consultation's session and the title its Director gave it,
    /// oldest first (id as tie-breaker). A session listed here had its
    /// consultation ended by banto, which archived it in the same transaction;
    /// the archived row may be gone since (`D`), this one stays. A session that is
    /// archived and not listed is the operator's own `d` — or a consultation that
    /// predates titles, or one that ended before its session id was bound, which
    /// leave nothing here.
    pub fn consultation_titles(&self) -> Result<Vec<(SessionId, String)>, StoreError> {
        self.ended_titles(BrigadeRole::Goinkyo)
    }

    /// The same for ended Planners: the session and the Plan's title, which the
    /// list shows behind its own label.
    pub fn planner_titles(&self) -> Result<Vec<(SessionId, String)>, StoreError> {
        self.ended_titles(BrigadeRole::Planner)
    }

    fn ended_titles(&self, role: BrigadeRole) -> Result<Vec<(SessionId, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, title FROM consultations WHERE role = ?1
             ORDER BY ended_at_ms, session_id",
        )?;
        let rows = stmt.query_map([role.as_token()], |row| {
            Ok((SessionId(row.get(0)?), row.get(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}
