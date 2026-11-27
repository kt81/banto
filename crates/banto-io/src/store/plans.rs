//! A brigade's Plan, the Planner's output, and the Planner's membership that lives
//! exactly as long as the Plan is open.
//!
//! What may be written is decided in `banto_core::plan` (sizes, the rule for
//! `completed`); this module stores what passed and enforces who writes and what
//! state the Plan is in. Every write re-reads the caller's role and the Plan's
//! state inside its own `BEGIN IMMEDIATE` transaction, the way
//! [`Store::set_progress`] does: the tool that called checked a moment before, and
//! a membership or a Plan that changed since is not written as or into.
//!
//! The invariant this file is built around: **the Planner's member row disappears
//! only inside the transaction that closes its Plan**. [`close_open_plan_on`] is
//! the one place that does both, and [`Store::dismiss_worker`] refuses to remove a
//! Planner; deleting a whole brigade closes the Plan first, in the same
//! transaction.

use std::time::SystemTime;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};

use banto_core::config::GoinkyoConsult;
use banto_core::model::{AgentKind, BANTO_SENDER, BrigadeId, BrigadeRole, PLANNER_TOKEN};
use banto_core::plan::{
    CompletionRefusal, PLAN_VERSIONS_MAX, Plan, PlanCloser, PlanGlance, PlanResult, PlanState,
    PlanVersion, PlanView, VersionKind, approval_notice, completion_refusal, operator_close_notice,
    operator_close_result, release_notice,
};
use banto_core::progress::Progress;

use super::brigades::{
    archive_ended_sessions, director_token_on, enqueue_message_on, insert_member, remove_member_on,
};
use super::progress::read_progress_on;
use super::{Store, StoreError, system_time_to_unix_ms};

/// A Plan that has just closed, with what an export of it needs: the text as it
/// stands and the Progress at the end. Returned by the writes that close a Plan so
/// that the shell can write the file without a second, racing read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedPlan {
    pub brigade_id: BrigadeId,
    pub view: PlanView,
    pub progress: Option<Progress>,
    /// The message the closing transaction wrote to the Director, when it wrote
    /// one (the operator's close from the Planner's tile).
    pub notice: Option<Notice>,
}

/// A message banto wrote to the Director inside a transaction, handed back so that
/// the process that made the write can append it to the opt-in message log once the
/// transaction has committed. The store itself logs nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub queue_id: i64,
    /// The Director's token, which the message was addressed to.
    pub to_member: String,
    pub body: String,
}

/// How a Plan is being closed, which decides who may and what the result is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanClosing {
    /// `close_planner`: the Director's call, with the result it chose.
    Director { token: String, result: PlanResult },
    /// The operator's prefix-`x` on the Planner's tile: `completed` when
    /// [`completion_refusal`] has nothing to say, otherwise `abandoned`.
    Operator,
    /// The brigade is being closed or disbanded: always `abandoned`.
    Disband,
}

/// What [`Store::open_planner`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenedPlanner {
    Opened {
        number: i64,
    },
    /// The caller is not, in the transaction, the brigade's Director.
    NotDirector,
    /// The brigade has a Planner already.
    PlannerExists,
    /// The brigade has an open Plan already.
    PlanOpen,
}

/// What [`Store::write_plan`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWritten {
    Written {
        number: i64,
        version: i64,
    },
    NotPlanner,
    NoOpenPlan,
    /// The Plan holds [`PLAN_VERSIONS_MAX`] versions already.
    TooManyVersions,
}

/// What [`Store::approve_plan`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanApproval {
    Approved {
        number: i64,
        version: i64,
        /// `None` when the brigade has no Director to tell.
        notice: Option<Notice>,
    },
    /// There is no open Plan with that number: it closed, or another opened.
    NoOpenPlan,
    /// A newer version than the one the operator was looking at exists; nothing
    /// was approved, and the screen should read again.
    Stale { latest: i64 },
    /// The newest version is not a draft (it was approved or released already), or
    /// there is none.
    NothingToApprove,
}

/// What [`Store::release_plan`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReleased {
    Released {
        number: i64,
        version: i64,
        /// `None` when the brigade has no Director to tell.
        notice: Option<Notice>,
    },
    NotPlanner,
    NoOpenPlan,
    /// The operator has approved no version yet: a release changes what is in
    /// force, and nothing is.
    NotApproved,
    /// The newest version is not a draft.
    NothingToRelease,
}

/// What a close did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanClosed {
    Closed(Box<ClosedPlan>),
    NotDirector,
    NoOpenPlan,
    Refused(CompletionRefusal),
}

fn unreadable(column: usize, what: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, Type::Text, what.into())
}

fn plan_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Plan> {
    let state: String = row.get(2)?;
    let result: Option<String> = row.get(4)?;
    let closed_by: Option<String> = row.get(5)?;
    Ok(Plan {
        number: row.get(0)?,
        title: row.get(1)?,
        state: PlanState::from_token(&state)
            .ok_or_else(|| unreadable(2, format!("unrecognized plan state {state:?}")))?,
        request: row.get(3)?,
        result: result.as_deref().and_then(PlanResult::from_token),
        closed_by: closed_by.as_deref().and_then(PlanCloser::from_token),
        opened_at_ms: row.get(6)?,
        closed_at_ms: row.get(7)?,
    })
}

const PLAN_COLUMNS: &str = "CAST(number AS INTEGER), title, state, request, result, closed_by, \
     CAST(opened_at_ms AS INTEGER), CAST(closed_at_ms AS INTEGER)";

const VERSION_COLUMNS: &str = "CAST(version AS INTEGER), body, summary, \
     CAST(created_at_ms AS INTEGER), kind, CAST(approved_at_ms AS INTEGER)";

/// The brigade's open Plan's number and state, `None` when there is none. A row
/// whose state is not a word this build knows is an error, not a missing Plan:
/// the unique index counts it as open, and so must a reader.
fn open_plan_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
) -> Result<Option<(i64, PlanState)>, StoreError> {
    let row = conn
        .query_row(
            "SELECT CAST(number AS INTEGER), state FROM plans
             WHERE brigade_id = ?1 AND state <> 'closed'",
            [brigade_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((number, state)) => {
            let state = PlanState::from_token(&state)
                .ok_or_else(|| unreadable(1, format!("unrecognized plan state {state:?}")))?;
            Ok(Some((number, state)))
        }
    }
}

/// A Plan, open or closed, with its version in force and its pending draft.
fn plan_view_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
    number: i64,
) -> Result<Option<PlanView>, StoreError> {
    let plan = conn
        .query_row(
            &format!("SELECT {PLAN_COLUMNS} FROM plans WHERE brigade_id = ?1 AND number = ?2"),
            params![brigade_id, number],
            plan_from_row,
        )
        .optional()?;
    let Some(plan) = plan else {
        return Ok(None);
    };
    let in_force = conn
        .query_row(
            &format!(
                "SELECT {VERSION_COLUMNS} FROM plan_versions
                 WHERE brigade_id = ?1 AND plan_number = ?2 AND kind <> 'draft'
                 ORDER BY version DESC LIMIT 1"
            ),
            params![brigade_id, number],
            version_from_row,
        )
        .optional()?;
    let floor = in_force.as_ref().map_or(0, |version| version.version);
    let pending = conn
        .query_row(
            &format!(
                "SELECT {VERSION_COLUMNS} FROM plan_versions
                 WHERE brigade_id = ?1 AND plan_number = ?2 AND kind = 'draft' AND version > ?3
                 ORDER BY version DESC LIMIT 1"
            ),
            params![brigade_id, number, floor],
            version_from_row,
        )
        .optional()?;
    Ok(Some(PlanView {
        plan,
        in_force,
        pending,
    }))
}

/// A version as [`VERSION_COLUMNS`] selects it: version, body, summary, created,
/// kind, approved.
fn version_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanVersion> {
    let kind: String = row.get(4)?;
    Ok(PlanVersion {
        version: row.get(0)?,
        body: row.get(1)?,
        summary: row.get(2)?,
        created_at_ms: row.get(3)?,
        kind: VersionKind::from_token(&kind)
            .ok_or_else(|| unreadable(4, format!("unrecognized version kind {kind:?}")))?,
        approved_at_ms: row.get(5)?,
    })
}

fn is_member_with_role_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM brigade_members
         WHERE brigade_id = ?1 AND member_token = ?2 AND role = ?3)",
        params![brigade_id, token, role.as_token()],
        |row| row.get(0),
    )?)
}

/// The newest version of a Plan as `(number, kind word)`, `None` for a Plan with
/// no version yet.
fn latest_version_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
    number: i64,
) -> Result<Option<(i64, String)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT CAST(version AS INTEGER), kind FROM plan_versions
             WHERE brigade_id = ?1 AND plan_number = ?2
             ORDER BY version DESC LIMIT 1",
            params![brigade_id, number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Closes the brigade's open Plan and removes its Planner, in the caller's
/// transaction, which must be `IMMEDIATE`: this reads before it writes. The Plan
/// row, the Planner's archived session (and its title), and the Planner's member
/// row, cursor and mail all change together or not at all.
pub(super) fn close_open_plan_on(
    tx: &rusqlite::Transaction<'_>,
    brigade_id: BrigadeId,
    closing: &PlanClosing,
) -> Result<PlanClosed, StoreError> {
    let Some((number, state)) = open_plan_on(tx, brigade_id)? else {
        return Ok(PlanClosed::NoOpenPlan);
    };
    if let PlanClosing::Director { token, .. } = closing
        && !is_member_with_role_on(tx, brigade_id, token, BrigadeRole::Director)?
    {
        return Ok(PlanClosed::NotDirector);
    }
    let progress = read_progress_on(tx, brigade_id)?.map(|record| record.progress);
    let (result, closer) = match closing {
        PlanClosing::Director { result, .. } => {
            if *result == PlanResult::Completed
                && let Some(refusal) = completion_refusal(state, progress.as_ref())
            {
                return Ok(PlanClosed::Refused(refusal));
            }
            (*result, PlanCloser::Director)
        }
        PlanClosing::Operator => (
            operator_close_result(state, progress.as_ref()),
            PlanCloser::Operator,
        ),
        PlanClosing::Disband => (PlanResult::Abandoned, PlanCloser::Operator),
    };
    tx.execute(
        "UPDATE plans SET state = ?3, result = ?4, closed_by = ?5, closed_at_ms = ?6
         WHERE brigade_id = ?1 AND number = ?2",
        params![
            brigade_id,
            number,
            PlanState::Closed.as_token(),
            result.as_token(),
            closer.as_token(),
            system_time_to_unix_ms(SystemTime::now())
        ],
    )?;
    archive_ended_sessions(tx, brigade_id, Some(PLANNER_TOKEN))?;
    remove_member_on(tx, brigade_id, PLANNER_TOKEN)?;
    // The Director is told in the same transaction when the operator closed the
    // Plan from the Planner's tile: it asked for nothing, so nothing else would
    // tell it. A Director that closed the Plan knows, and a disbanded brigade has
    // no Director left to tell.
    let mut notice = None;
    if matches!(closing, PlanClosing::Operator)
        && let Some(director) = director_token_on(tx, brigade_id)?
    {
        let body = operator_close_notice(number, result);
        let queue_id = enqueue_message_on(
            tx,
            brigade_id,
            BANTO_SENDER,
            BrigadeRole::Director,
            Some(&director),
            &body,
        )?;
        notice = Some(Notice {
            queue_id,
            to_member: director,
            body,
        });
    }
    let view = plan_view_on(tx, brigade_id, number)?
        .ok_or_else(|| unreadable(0, "the plan that was just closed is gone".to_string()))?;
    Ok(PlanClosed::Closed(Box::new(ClosedPlan {
        brigade_id,
        view,
        progress,
        notice,
    })))
}

impl Store {
    /// Opens a Plan and its Planner together, in one transaction: the Plan row
    /// (number, title, request, `drafting`) and the Planner's member row, which
    /// has no session yet (the emporium launches it on its next tick). `title` and
    /// `request` are stored as given: the caller reduced and checked them
    /// (`banto_core::consultation::check_title`, `banto_core::plan::check_request`).
    ///
    /// Refused, with nothing written, when `caller_token` is not the brigade's
    /// Director, when the brigade has a Planner, or when it has an open Plan. The
    /// number is one past the largest the brigade has used, closed Plans included.
    /// `agent` is the product the Planner runs as, written on its row; the row stays
    /// the authority until the Plan closes.
    pub fn open_planner(
        &mut self,
        brigade_id: BrigadeId,
        caller_token: &str,
        title: &str,
        request: &str,
        agent: AgentKind,
    ) -> Result<OpenedPlanner, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !is_member_with_role_on(&tx, brigade_id, caller_token, BrigadeRole::Director)? {
            return Ok(OpenedPlanner::NotDirector);
        }
        if is_member_with_role_on(&tx, brigade_id, PLANNER_TOKEN, BrigadeRole::Planner)? {
            return Ok(OpenedPlanner::PlannerExists);
        }
        if open_plan_on(&tx, brigade_id)?.is_some() {
            return Ok(OpenedPlanner::PlanOpen);
        }
        let number: i64 = tx.query_row(
            "SELECT COALESCE(MAX(CAST(number AS INTEGER)), 0) + 1 FROM plans WHERE brigade_id = ?1",
            [brigade_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO plans (brigade_id, number, title, request, state, opened_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                brigade_id,
                number,
                title,
                request,
                PlanState::Drafting.as_token(),
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        insert_member(
            &tx,
            brigade_id,
            PLANNER_TOKEN,
            BrigadeRole::Planner,
            None,
            Some(agent),
        )?;
        tx.execute(
            "UPDATE brigade_members SET consult_title = ?3
             WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, PLANNER_TOKEN, title],
        )?;
        tx.commit()?;
        Ok(OpenedPlanner::Opened { number })
    }

    /// The brigade's open Plan with its version in force and its pending draft;
    /// `None` when none is open.
    pub fn open_plan_view(&self, brigade_id: BrigadeId) -> Result<Option<PlanView>, StoreError> {
        match open_plan_on(&self.conn, brigade_id)? {
            Some((number, _)) => plan_view_on(&self.conn, brigade_id, number),
            None => Ok(None),
        }
    }

    /// What the tick needs of the brigade's open Plan, without a body; `None` when
    /// none is open.
    pub fn open_plan_glance(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Option<PlanGlance>, StoreError> {
        let Some((number, state)) = open_plan_on(&self.conn, brigade_id)? else {
            return Ok(None);
        };
        let latest = latest_version_on(&self.conn, brigade_id, number)?;
        Ok(Some(PlanGlance {
            number,
            latest_version: latest.as_ref().map_or(0, |(version, _)| *version),
            state,
            pending: latest
                .as_ref()
                .is_some_and(|(_, kind)| kind == VersionKind::Draft.as_token()),
        }))
    }

    /// The Planner's `write_plan`: a new draft, the next version of the open Plan.
    /// The text was checked by `banto_core::plan::check_write`. Allowed whatever the
    /// Plan's state: a draft written after an approval is a newer draft that awaits
    /// the operator's key, and does not change what is in force.
    pub fn write_plan(
        &mut self,
        brigade_id: BrigadeId,
        caller_token: &str,
        body: &str,
        summary: &str,
    ) -> Result<PlanWritten, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !is_member_with_role_on(&tx, brigade_id, caller_token, BrigadeRole::Planner)? {
            return Ok(PlanWritten::NotPlanner);
        }
        let Some((number, _)) = open_plan_on(&tx, brigade_id)? else {
            return Ok(PlanWritten::NoOpenPlan);
        };
        let (count, latest): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(MAX(CAST(version AS INTEGER)), 0) FROM plan_versions
             WHERE brigade_id = ?1 AND plan_number = ?2",
            params![brigade_id, number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count >= PLAN_VERSIONS_MAX {
            return Ok(PlanWritten::TooManyVersions);
        }
        let version = latest + 1;
        tx.execute(
            "INSERT INTO plan_versions
             (brigade_id, plan_number, version, body, summary, kind, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                brigade_id,
                number,
                version,
                body,
                summary,
                VersionKind::Draft.as_token(),
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        tx.commit()?;
        Ok(PlanWritten::Written { number, version })
    }

    /// The operator's approval of exactly version `version` of Plan `number`, and,
    /// in the same transaction, the message to the Director that says so. The
    /// message is banto's own ([`BANTO_SENDER`]), never the Planner's: the approval
    /// is the operator's act, and nothing an agent wrote may stand in for it.
    ///
    /// `review` is the operator's `goinkyo_consult`, which the message turns into what
    /// the Director does about a review ([`banto_core::plan::review_step`]); the store
    /// reads no configuration.
    ///
    /// Approves only the newest version and only while it is still a draft, so a
    /// draft the Planner wrote after the operator opened the screen is not approved
    /// unseen ([`PlanApproval::Stale`]).
    pub fn approve_plan(
        &mut self,
        brigade_id: BrigadeId,
        number: i64,
        version: i64,
        review: Option<GoinkyoConsult>,
    ) -> Result<PlanApproval, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match open_plan_on(&tx, brigade_id)? {
            Some((open, _)) if open == number => {}
            _ => return Ok(PlanApproval::NoOpenPlan),
        }
        let Some((latest, kind)) = latest_version_on(&tx, brigade_id, number)? else {
            return Ok(PlanApproval::NothingToApprove);
        };
        if latest != version {
            return Ok(PlanApproval::Stale { latest });
        }
        if kind != VersionKind::Draft.as_token() {
            return Ok(PlanApproval::NothingToApprove);
        }
        let now = system_time_to_unix_ms(SystemTime::now());
        tx.execute(
            "UPDATE plan_versions SET kind = ?4, approved_at_ms = ?5
             WHERE brigade_id = ?1 AND plan_number = ?2 AND version = ?3",
            params![
                brigade_id,
                number,
                version,
                VersionKind::OperatorApproved.as_token(),
                now
            ],
        )?;
        tx.execute(
            "UPDATE plans SET state = ?3 WHERE brigade_id = ?1 AND number = ?2",
            params![brigade_id, number, PlanState::Approved.as_token()],
        )?;
        let mut notice = None;
        if let Some(director) = director_token_on(&tx, brigade_id)? {
            let body = approval_notice(number, version, review);
            let queue_id = enqueue_message_on(
                &tx,
                brigade_id,
                BANTO_SENDER,
                BrigadeRole::Director,
                Some(&director),
                &body,
            )?;
            notice = Some(Notice {
                queue_id,
                to_member: director,
                body,
            });
        }
        tx.commit()?;
        Ok(PlanApproval::Approved {
            number,
            version,
            notice,
        })
    }

    /// The Planner's `release_plan`: its newest draft goes into force, without the
    /// operator, for an already approved Plan. What a release may change (splitting
    /// or reordering tasks) is a rule of the briefing and of the Planner's prompt,
    /// not of this table; what this enforces is that there is an approval to work
    /// within and a draft to release. The Director is told, in the same transaction,
    /// by banto's own message.
    pub fn release_plan(
        &mut self,
        brigade_id: BrigadeId,
        caller_token: &str,
    ) -> Result<PlanReleased, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if !is_member_with_role_on(&tx, brigade_id, caller_token, BrigadeRole::Planner)? {
            return Ok(PlanReleased::NotPlanner);
        }
        let Some((number, state)) = open_plan_on(&tx, brigade_id)? else {
            return Ok(PlanReleased::NoOpenPlan);
        };
        if state != PlanState::Approved {
            return Ok(PlanReleased::NotApproved);
        }
        let Some((version, kind)) = latest_version_on(&tx, brigade_id, number)? else {
            return Ok(PlanReleased::NothingToRelease);
        };
        if kind != VersionKind::Draft.as_token() {
            return Ok(PlanReleased::NothingToRelease);
        }
        let summary: String = tx.query_row(
            "SELECT summary FROM plan_versions
             WHERE brigade_id = ?1 AND plan_number = ?2 AND version = ?3",
            params![brigade_id, number, version],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE plan_versions SET kind = ?4
             WHERE brigade_id = ?1 AND plan_number = ?2 AND version = ?3",
            params![
                brigade_id,
                number,
                version,
                VersionKind::PlannerReleased.as_token()
            ],
        )?;
        let mut notice = None;
        if let Some(director) = director_token_on(&tx, brigade_id)? {
            let body = release_notice(number, version, &summary);
            let queue_id = enqueue_message_on(
                &tx,
                brigade_id,
                BANTO_SENDER,
                BrigadeRole::Director,
                Some(&director),
                &body,
            )?;
            notice = Some(Notice {
                queue_id,
                to_member: director,
                body,
            });
        }
        tx.commit()?;
        Ok(PlanReleased::Released {
            number,
            version,
            notice,
        })
    }

    /// Closes the brigade's open Plan and removes its Planner, in one transaction
    /// (see [`close_open_plan_on`]). The Planner's session is archived and titled in
    /// that transaction too.
    pub fn close_plan(
        &mut self,
        brigade_id: BrigadeId,
        closing: &PlanClosing,
    ) -> Result<PlanClosed, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let closed = close_open_plan_on(&tx, brigade_id, closing)?;
        if matches!(closed, PlanClosed::Closed(_)) {
            tx.commit()?;
        }
        Ok(closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::{BANTO_SENDER, SessionId};
    use banto_core::progress::{Progress, ProgressTask, TaskState};

    fn briefed(store: &mut Store) -> BrigadeId {
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        br
    }

    fn opened(store: &mut Store) -> BrigadeId {
        let br = briefed(store);
        assert_eq!(
            store
                .open_planner(
                    br,
                    "director",
                    "Layout",
                    "make it better",
                    AgentKind::ClaudeCode
                )
                .unwrap(),
            OpenedPlanner::Opened { number: 1 }
        );
        br
    }

    fn tasks(items: &[(&str, TaskState)]) -> Progress {
        Progress {
            stage: "s".to_string(),
            tasks: items
                .iter()
                .map(|(id, state)| ProgressTask {
                    id: (*id).to_string(),
                    state: *state,
                    note: String::new(),
                })
                .collect(),
            off_plan: Vec::new(),
        }
    }

    fn has_planner(store: &Store, br: BrigadeId) -> bool {
        store
            .brigade_members(br)
            .unwrap()
            .iter()
            .any(|m| m.role == BrigadeRole::Planner)
    }

    fn unseen(store: &Store, br: BrigadeId, token: &str, role: BrigadeRole) -> bool {
        store.has_unseen_brigade_messages(br, token, role).unwrap()
    }

    #[test]
    fn opening_makes_the_plan_and_the_planner_row_together() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let planner = store.brigade_member(br, PLANNER_TOKEN).unwrap().unwrap();
        assert_eq!(planner.role, BrigadeRole::Planner);
        assert_eq!(planner.agent, Some(AgentKind::ClaudeCode));
        assert_eq!(planner.session_id, None, "the next tick launches it");
        let view = store.open_plan_view(br).unwrap().unwrap();
        assert_eq!(view.plan.number, 1);
        assert_eq!(view.plan.title, "Layout");
        assert_eq!(view.plan.request, "make it better");
        assert_eq!(view.plan.state, PlanState::Drafting);
        assert_eq!((view.in_force, view.pending), (None, None));
    }

    #[test]
    fn opening_is_refused_for_a_non_director_a_second_planner_and_an_open_plan() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        for caller in [worker.as_str(), "nobody"] {
            assert_eq!(
                store
                    .open_planner(br, caller, "t", "r", AgentKind::ClaudeCode)
                    .unwrap(),
                OpenedPlanner::NotDirector,
                "{caller}"
            );
        }
        assert!(!has_planner(&store, br), "a refusal wrote nothing");
        assert_eq!(store.open_plan_view(br).unwrap(), None);

        store
            .open_planner(br, "director", "t", "r", AgentKind::ClaudeCode)
            .unwrap();
        assert_eq!(
            store
                .open_planner(br, "director", "t2", "r2", AgentKind::ClaudeCode)
                .unwrap(),
            OpenedPlanner::PlannerExists
        );
        // A Plan that outlived its Planner's row (a hand-edited store) still
        // blocks a second one, and the row is not quietly re-made.
        store
            .conn
            .execute(
                "DELETE FROM brigade_members WHERE member_token = 'planner'",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .open_planner(br, "director", "t2", "r2", AgentKind::ClaudeCode)
                .unwrap(),
            OpenedPlanner::PlanOpen
        );
    }

    #[test]
    fn a_plan_number_is_one_past_the_largest_used_closed_plans_included() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        assert!(matches!(
            store.close_plan(br, &PlanClosing::Operator).unwrap(),
            PlanClosed::Closed(_)
        ));
        assert_eq!(
            store
                .open_planner(br, "director", "again", "r", AgentKind::ClaudeCode)
                .unwrap(),
            OpenedPlanner::Opened { number: 2 }
        );
    }

    #[test]
    fn only_the_planner_writes_and_each_write_is_the_next_version() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        for caller in ["director", worker.as_str(), "nobody"] {
            assert_eq!(
                store.write_plan(br, caller, "b", "s").unwrap(),
                PlanWritten::NotPlanner,
                "{caller}"
            );
        }
        assert_eq!(
            store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap(),
            PlanWritten::Written {
                number: 1,
                version: 1
            }
        );
        assert_eq!(
            store
                .write_plan(br, PLANNER_TOKEN, "two", "second")
                .unwrap(),
            PlanWritten::Written {
                number: 1,
                version: 2
            }
        );
        let view = store.open_plan_view(br).unwrap().unwrap();
        assert_eq!(view.in_force, None, "a draft is not in force");
        let pending = view.pending.unwrap();
        assert_eq!((pending.version, pending.body.as_str()), (2, "two"));
        let glance = store.open_plan_glance(br).unwrap().unwrap();
        assert_eq!(
            (glance.number, glance.latest_version, glance.pending),
            (1, 2, true)
        );
    }

    #[test]
    fn a_write_after_the_plan_is_gone_is_refused_whether_or_not_a_row_is_left() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        store.close_plan(br, &PlanClosing::Operator).unwrap();
        // The row went with the Plan, so the caller is no longer the Planner.
        assert_eq!(
            store.write_plan(br, PLANNER_TOKEN, "b", "s").unwrap(),
            PlanWritten::NotPlanner
        );
        // A row left behind with no Plan (a hand-edited store) is the other refusal.
        store
            .conn
            .execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role)
                 VALUES (?1, 'planner', 'planner')",
                [br],
            )
            .unwrap();
        assert_eq!(
            store.write_plan(br, PLANNER_TOKEN, "b", "s").unwrap(),
            PlanWritten::NoOpenPlan
        );
    }

    #[test]
    fn the_versions_of_a_plan_are_bounded() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        for _ in 0..PLAN_VERSIONS_MAX {
            assert!(matches!(
                store.write_plan(br, PLANNER_TOKEN, "b", "s").unwrap(),
                PlanWritten::Written { .. }
            ));
        }
        assert_eq!(
            store.write_plan(br, PLANNER_TOKEN, "b", "s").unwrap(),
            PlanWritten::TooManyVersions
        );
    }

    #[test]
    fn approval_puts_exactly_the_shown_version_in_force_and_tells_the_director_as_banto() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        assert_eq!(
            store
                .approve_plan(br, 1, 1, Some(GoinkyoConsult::Design))
                .unwrap(),
            PlanApproval::NothingToApprove,
            "nothing written yet"
        );
        store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap();
        store
            .write_plan(br, PLANNER_TOKEN, "two", "second")
            .unwrap();
        assert_eq!(
            store
                .approve_plan(br, 1, 1, Some(GoinkyoConsult::Design))
                .unwrap(),
            PlanApproval::Stale { latest: 2 },
            "the operator was looking at version 1 and version 2 arrived"
        );
        assert_eq!(
            store
                .approve_plan(br, 9, 2, Some(GoinkyoConsult::Design))
                .unwrap(),
            PlanApproval::NoOpenPlan
        );
        assert!(
            !unseen(&store, br, "director", BrigadeRole::Director),
            "a refused approval tells nobody"
        );
        let approved = store
            .approve_plan(br, 1, 2, Some(GoinkyoConsult::Design))
            .unwrap();
        let PlanApproval::Approved {
            number: 1,
            version: 2,
            notice: Some(notice),
        } = approved
        else {
            panic!("{approved:?}");
        };

        let view = store.open_plan_view(br).unwrap().unwrap();
        assert_eq!(view.plan.state, PlanState::Approved);
        let in_force = view.in_force.unwrap();
        assert_eq!(
            (in_force.version, in_force.kind),
            (2, VersionKind::OperatorApproved)
        );
        assert!(in_force.approved_at_ms.is_some());
        assert_eq!(view.pending, None);
        // The same version cannot be approved twice.
        assert_eq!(
            store
                .approve_plan(br, 1, 2, Some(GoinkyoConsult::Design))
                .unwrap(),
            PlanApproval::NothingToApprove
        );
        let mail = store
            .fetch_brigade_messages(br, "director", BrigadeRole::Director)
            .unwrap();
        assert_eq!(mail.len(), 1, "exactly one notice");
        // The notice handed back is the row the transaction wrote.
        assert_eq!(
            (notice.queue_id, notice.to_member.as_str(), &notice.body),
            (mail[0].id, "director", &mail[0].body)
        );
        assert_eq!(mail[0].from_token, BANTO_SENDER);
        assert_eq!(
            mail[0].body,
            approval_notice(1, 2, Some(GoinkyoConsult::Design))
        );
        assert_eq!(mail[0].to_member.as_deref(), Some("director"));
        assert!(
            !unseen(&store, br, PLANNER_TOKEN, BrigadeRole::Planner),
            "the Planner is not told by mail"
        );
    }

    #[test]
    fn a_release_needs_an_approval_to_work_within_and_a_draft_to_release() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap();
        assert_eq!(
            store.release_plan(br, PLANNER_TOKEN).unwrap(),
            PlanReleased::NotApproved,
            "a first version is the operator's"
        );
        for caller in ["director", worker.as_str()] {
            assert_eq!(
                store.release_plan(br, caller).unwrap(),
                PlanReleased::NotPlanner,
                "{caller}"
            );
        }
        store
            .approve_plan(br, 1, 1, Some(GoinkyoConsult::Design))
            .unwrap();
        assert_eq!(
            store.release_plan(br, PLANNER_TOKEN).unwrap(),
            PlanReleased::NothingToRelease,
            "version 1 is in force and no newer draft exists"
        );
        store
            .write_plan(br, PLANNER_TOKEN, "split", "tasks split")
            .unwrap();
        let released = store.release_plan(br, PLANNER_TOKEN).unwrap();
        let PlanReleased::Released {
            number: 1,
            version: 2,
            notice: Some(notice),
        } = released
        else {
            panic!("{released:?}");
        };
        assert_eq!(notice.to_member, "director");
        assert_eq!(notice.body, release_notice(1, 2, "tasks split"));
        let view = store.open_plan_view(br).unwrap().unwrap();
        let in_force = view.in_force.unwrap();
        assert_eq!(
            (in_force.version, in_force.kind),
            (2, VersionKind::PlannerReleased)
        );
        assert_eq!(
            in_force.approved_at_ms, None,
            "the operator did not approve it"
        );
        assert_eq!(view.plan.state, PlanState::Approved);
        let mail = store
            .fetch_brigade_messages(br, "director", BrigadeRole::Director)
            .unwrap();
        let bodies: Vec<&str> = mail.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(
            bodies,
            [
                approval_notice(1, 1, Some(GoinkyoConsult::Design)).as_str(),
                release_notice(1, 2, "tasks split").as_str()
            ]
        );
        assert!(mail.iter().all(|m| m.from_token == BANTO_SENDER));
    }

    #[test]
    fn completed_is_refused_until_approved_with_tasks_all_settled() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let completed = PlanClosing::Director {
            token: "director".to_string(),
            result: PlanResult::Completed,
        };
        let refused = |store: &mut Store| match store.close_plan(br, &completed).unwrap() {
            PlanClosed::Refused(refusal) => refusal,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(refused(&mut store), CompletionRefusal::NeverApproved);
        store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap();
        store
            .approve_plan(br, 1, 1, Some(GoinkyoConsult::Design))
            .unwrap();
        assert_eq!(refused(&mut store), CompletionRefusal::NoTasks);
        store
            .set_progress(
                br,
                "director",
                &tasks(&[
                    ("T1", TaskState::Done),
                    ("T2", TaskState::Doing),
                    ("T3", TaskState::Blocked),
                    ("T4", TaskState::Todo),
                    ("T5", TaskState::Dropped),
                ]),
            )
            .unwrap();
        assert_eq!(
            refused(&mut store),
            CompletionRefusal::OpenTasks(vec!["T2".into(), "T3".into(), "T4".into()])
        );
        assert!(has_planner(&store, br), "a refusal removed nothing");
        assert!(store.open_plan_view(br).unwrap().is_some());

        store
            .set_progress(
                br,
                "director",
                &tasks(&[("T1", TaskState::Done), ("T5", TaskState::Dropped)]),
            )
            .unwrap();
        let PlanClosed::Closed(closed) = store.close_plan(br, &completed).unwrap() else {
            panic!("settled tasks must allow completion");
        };
        assert_eq!(closed.view.plan.result, Some(PlanResult::Completed));
        assert_eq!(closed.view.plan.closed_by, Some(PlanCloser::Director));
        assert_eq!(closed.view.plan.state, PlanState::Closed);
        assert_eq!(closed.progress.unwrap().tasks.len(), 2);
    }

    #[test]
    fn abandoned_is_never_refused_and_only_the_director_may_ask() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        for caller in [worker.as_str(), PLANNER_TOKEN, "nobody"] {
            assert_eq!(
                store
                    .close_plan(
                        br,
                        &PlanClosing::Director {
                            token: caller.to_string(),
                            result: PlanResult::Abandoned
                        }
                    )
                    .unwrap(),
                PlanClosed::NotDirector,
                "{caller}"
            );
        }
        assert!(has_planner(&store, br));
        let PlanClosed::Closed(closed) = store
            .close_plan(
                br,
                &PlanClosing::Director {
                    token: "director".to_string(),
                    result: PlanResult::Abandoned,
                },
            )
            .unwrap()
        else {
            panic!("abandoning is never refused");
        };
        assert_eq!(closed.view.plan.result, Some(PlanResult::Abandoned));
        assert_eq!(
            store.close_plan(br, &PlanClosing::Operator).unwrap(),
            PlanClosed::NoOpenPlan
        );
    }

    #[test]
    fn the_operator_closes_as_completed_only_when_the_conditions_hold() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let PlanClosed::Closed(closed) = store.close_plan(br, &PlanClosing::Operator).unwrap()
        else {
            panic!("the operator can always close")
        };
        assert_eq!(closed.view.plan.result, Some(PlanResult::Abandoned));
        assert_eq!(closed.view.plan.closed_by, Some(PlanCloser::Operator));

        store
            .open_planner(br, "director", "two", "r", AgentKind::ClaudeCode)
            .unwrap();
        store.write_plan(br, PLANNER_TOKEN, "b", "s").unwrap();
        store
            .approve_plan(br, 2, 1, Some(GoinkyoConsult::Design))
            .unwrap();
        store
            .set_progress(br, "director", &tasks(&[("T1", TaskState::Done)]))
            .unwrap();
        let PlanClosed::Closed(closed) = store.close_plan(br, &PlanClosing::Operator).unwrap()
        else {
            panic!("the operator can always close")
        };
        assert_eq!(closed.view.plan.result, Some(PlanResult::Completed));
    }

    #[test]
    fn closing_removes_the_row_archives_and_titles_the_session_in_one_step() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let sid = SessionId("planner-session".to_string());
        store.set_member_session(br, PLANNER_TOKEN, &sid).unwrap();
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Planner,
                Some(PLANNER_TOKEN),
                "hi",
            )
            .unwrap();
        store.close_plan(br, &PlanClosing::Operator).unwrap();
        assert!(!has_planner(&store, br));
        assert_eq!(store.archived_ids().unwrap(), std::slice::from_ref(&sid));
        assert_eq!(
            store.planner_titles().unwrap(),
            [(sid.clone(), "Layout".to_string())]
        );
        assert!(
            store.consultation_titles().unwrap().is_empty(),
            "a Planner's title is not listed as a Goinkyo's"
        );
        assert!(
            !unseen(&store, br, PLANNER_TOKEN, BrigadeRole::Planner),
            "mail to a gone Planner is purged"
        );
        assert_eq!(store.open_plan_view(br).unwrap(), None);
    }

    /// Archiving and titling are by session id, whatever product's session it is: a
    /// Codex Planner's thread id is archived in banto's own table (`~/.codex` is
    /// never written) and titled the same way.
    #[test]
    fn closing_a_codex_planners_plan_archives_and_titles_its_session_too() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        store
            .open_planner(br, "director", "Layout", "make it better", AgentKind::Codex)
            .unwrap();
        let sid = SessionId("019a0000-codex-thread".to_string());
        store.set_member_session(br, PLANNER_TOKEN, &sid).unwrap();
        store.close_plan(br, &PlanClosing::Operator).unwrap();
        assert!(!has_planner(&store, br));
        assert_eq!(store.archived_ids().unwrap(), std::slice::from_ref(&sid));
        assert_eq!(
            store.planner_titles().unwrap(),
            [(sid, "Layout".to_string())]
        );
    }

    #[test]
    fn only_the_operators_close_tells_the_director_and_tells_it_as_banto() {
        // The operator's close from the Planner's tile: the Director asked for
        // nothing, so this is the one notice it gets.
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        store.close_plan(br, &PlanClosing::Operator).unwrap();
        let mail = store
            .fetch_brigade_messages(br, "director", BrigadeRole::Director)
            .unwrap();
        assert_eq!(mail.len(), 1, "{mail:?}");
        assert_eq!(mail[0].from_token, BANTO_SENDER);
        assert_eq!(
            mail[0].body,
            banto_core::plan::operator_close_notice(1, PlanResult::Abandoned)
        );

        // A Director that closed the Plan knows, and says nothing to itself.
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        store
            .close_plan(
                br,
                &PlanClosing::Director {
                    token: "director".to_string(),
                    result: PlanResult::Abandoned,
                },
            )
            .unwrap();
        assert!(!unseen(&store, br, "director", BrigadeRole::Director));
    }

    #[test]
    fn the_planner_row_survives_every_removal_that_is_not_closing_its_plan() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let sid = SessionId("planner-session".to_string());
        store.set_member_session(br, PLANNER_TOKEN, &sid).unwrap();

        // Dismissing it, as the operator would a Worker.
        assert!(!store.dismiss_worker(br, PLANNER_TOKEN).unwrap());
        assert!(has_planner(&store, br));
        assert!(
            store.archived_ids().unwrap().is_empty(),
            "nothing archived either"
        );
        // The bare removal.
        store.remove_brigade_member(br, PLANNER_TOKEN).unwrap();
        assert!(has_planner(&store, br));
        // A brigade with a Planner is not "empty".
        assert_eq!(store.delete_empty_brigades().unwrap(), 0);
        assert!(has_planner(&store, br));
        // Its Plan is still open: nothing above touched it.
        assert!(store.open_plan_view(br).unwrap().is_some());
    }

    #[test]
    fn disbanding_closes_the_plan_first_returns_it_for_export_and_deletes_every_row() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let sid = SessionId("planner-session".to_string());
        store.set_member_session(br, PLANNER_TOKEN, &sid).unwrap();
        store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap();
        store
            .set_progress(br, "director", &tasks(&[("T1", TaskState::Doing)]))
            .unwrap();
        let closed = store.delete_brigade(br).unwrap().expect("an open Plan");
        assert_eq!(closed.brigade_id, br);
        assert_eq!(closed.view.plan.result, Some(PlanResult::Abandoned));
        assert_eq!(closed.view.plan.closed_by, Some(PlanCloser::Operator));
        assert_eq!(
            closed.progress.unwrap().tasks.len(),
            1,
            "Progress as it ended"
        );
        assert_eq!(
            closed.view.pending.map(|v| v.body),
            Some("one".to_string()),
            "the draft is in the export, as a draft"
        );
        assert_eq!(store.archived_ids().unwrap(), std::slice::from_ref(&sid));
        assert_eq!(
            store.planner_titles().unwrap(),
            [(sid, "Layout".to_string())]
        );
        for table in [
            "plans",
            "plan_versions",
            "brigade_members",
            "brigade_progress",
        ] {
            let rows: i64 = store
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }

    #[test]
    fn disbanding_a_brigade_with_no_plan_returns_none() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        assert_eq!(store.delete_brigade(br).unwrap(), None);
    }

    #[test]
    fn dismissing_a_worker_leaves_the_plan_and_its_versions_alone() {
        let mut store = Store::open_in_memory().unwrap();
        let br = opened(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        store.write_plan(br, PLANNER_TOKEN, "one", "first").unwrap();
        assert!(store.dismiss_worker(br, &worker).unwrap());
        let view = store.open_plan_view(br).unwrap().unwrap();
        assert_eq!(view.pending.unwrap().version, 1);
        assert!(has_planner(&store, br));
    }

    #[test]
    fn plans_of_different_brigades_do_not_meet() {
        let mut store = Store::open_in_memory().unwrap();
        let first = opened(&mut store);
        let second = store.create_brigade("other").unwrap();
        store
            .add_brigade_member(second, "director", BrigadeRole::Director, None)
            .unwrap();
        assert_eq!(store.open_plan_view(second).unwrap(), None);
        assert_eq!(
            store
                .open_planner(second, "director", "t", "r", AgentKind::ClaudeCode)
                .unwrap(),
            OpenedPlanner::Opened { number: 1 },
            "numbers are per brigade"
        );
        store.write_plan(first, PLANNER_TOKEN, "mine", "s").unwrap();
        assert_eq!(store.open_plan_view(second).unwrap().unwrap().pending, None);
    }
}
