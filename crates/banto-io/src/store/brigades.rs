//! Brigades: an internal operational cell of one Director session and its
//! Worker sessions (formed with at least one; dismissing Workers can later
//! leave it with none), hosted together as tiled panes in the emporium.
//!
//! A brigade is a *separate* concept from a group: a group is the user's own
//! filing (project / phase), while a brigade is a live operational unit — a
//! Director commanding Worker(s).
//!
//! `brigade_members` is keyed by a banto-owned `member_token` ('director',
//! 'worker-1', 'worker-2', ...) rather than the session id itself: a
//! Worker is formed *before* its agent product assigns it a session id (banto
//! auto-spawns a fresh `claude` process and only later discovers its id —
//! see `banto_core::engine`'s `PendingOpen` flow), so the id has to be
//! a nullable column (`session_id`) filled in after the fact, not the
//! primary identity. Two policies are layered here in code rather than
//! enforced by the schema:
//! - a brigade has exactly one Director — that rule lives in the formation
//!   layer (the emporium), not this table;
//! - a session belongs to at most one brigade at a time — also the
//!   formation layer's job (it only ever forms a brigade from a session with
//!   no existing membership, and Workers are always freshly spawned).

use std::time::SystemTime;

use rusqlite::types::Type;
use rusqlite::{OptionalExtension, params};

use banto_core::display_text::display_safe;
use banto_core::member_name;
use banto_core::model::{
    AgentKind, Brigade, BrigadeId, BrigadeMember, BrigadeMessage, BrigadeRole, InterruptOutcome,
    MemberToken, MessageGlance, SessionId, StoredLayout, StoredMemberLayout,
};

use super::archive::archive_on;
use super::consultations::record_on;
use super::plans::{PlanClosed, PlanClosing, close_open_plan_on};
use super::{Store, StoreError, system_time_to_unix_ms, unix_ms_to_system_time};

/// What `brigades.request_text` holds while a Director's request is open. A
/// constant: the request is the fact that the brigade is waiting on the operator's
/// decision, and nothing an agent wrote decides whether one is open. The question a
/// Director may attach lives in its own column (`request_question`), so that a
/// hand-edited or oversized question can never change what the tick reads.
pub const REQUEST_OPEN: &str = "open";

/// A brigade's open decision request as the reading screen reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDetail {
    pub seq: i64,
    /// What the Director attached, `None` when it attached no question.
    pub question: Option<String>,
    /// When the request was made, `None` for a row whose time is not a number.
    pub requested_at_ms: Option<i64>,
}

/// An `interrupt_worker` request the UI has not decided or its Worker has not read,
/// as the UI reads it each tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInterrupt {
    /// The message row the request belongs to.
    pub message_id: i64,
    pub member: MemberToken,
    pub created_at_ms: i64,
    /// The Worker's read cursor is already at or past the message: it pulled the
    /// message before the UI looked, which the cursor says whatever the screen
    /// does.
    pub read: bool,
    /// The word the row carries: `None` for no word, and for one this build does
    /// not know.
    pub outcome: Option<InterruptOutcome>,
    /// The UI has not decided this request: the row has no word, or only the
    /// Worker's own `_mcp` has written one (`gated`, `capped`). A row with any
    /// other word was decided, and is listed only because it is unread.
    pub undecided: bool,
    /// The first line of the interrupt's own message, which is what the operator is
    /// shown of it: the member's newest message is not necessarily this one.
    pub line: String,
}

/// The statement behind [`Store::pending_interrupts`], a function of its own so
/// that a test can ask SQLite how it runs it. It leans on the partial index of
/// migration v22 (`WHERE interrupt = 1`), which is why `interrupt = 1` stays in
/// the inner select where the planner can match it.
fn pending_interrupts_sql() -> String {
    format!(
        "SELECT id, to_member, created_at_ms, read, word,
                word IS NULL OR word IN ('{0}', '{1}'), line FROM (
             SELECT m.id AS id, m.to_member AS to_member, m.created_at_ms AS created_at_ms,
                    m.interrupt_outcome AS word,
                    substr(m.body, 1, instr(m.body || char(10), char(10)) - 1) AS line,
                    m.id <= COALESCE((SELECT c.last_seen_id FROM brigade_cursors c
                                      WHERE c.brigade_id = m.brigade_id
                                        AND c.member_token = m.to_member), 0) AS read
             FROM brigade_messages m
             WHERE m.brigade_id = ?1 AND m.interrupt = 1 AND m.to_member IS NOT NULL)
         WHERE word IS NULL OR NOT read
         ORDER BY id",
        InterruptOutcome::Gated.as_token(),
        InterruptOutcome::Capped.as_token()
    )
}

/// An `interrupt_worker` request its Worker has not read yet, as
/// `brigade_status` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadInterrupt {
    pub member: MemberToken,
    /// `None`: the UI has not decided yet, or wrote a word this build does not
    /// know.
    pub outcome: Option<InterruptOutcome>,
    pub created_at_ms: i64,
}

/// Where an `interrupt_worker` request stands, for the tool that waits on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptState {
    /// The row is gone: the member was dismissed (its mail goes with it), or the
    /// brigade was disbanded.
    Gone,
    /// The UI has not recorded an outcome yet.
    Undecided,
    Decided(InterruptOutcome),
}

/// Archives the session of every Goinkyo and Planner row about to be deleted from
/// `brigade_id` — only the one named by `token` when given, every one when not —
/// inside the transaction that deletes it. A dismissed consultation's session
/// would otherwise return to the list as an ordinary row titled with banto's own
/// first prompt, and the operator's standing instruction (2026-11-20) is that
/// banto buries those itself; `D` restores any one of them. A Planner's session is
/// the same kind of thing and is buried the same way.
///
/// The role and the id are read from the row, here, in the transaction: the
/// caller's `token` is not a reason to archive (a Worker's dismissal comes
/// through the same caller), and an id a concurrent writer bound a moment ago
/// is the one that is archived. The caller's transaction must be `IMMEDIATE`:
/// this reads before the delete writes, and a deferred one would start as a
/// reader that cannot wait out another process's write lock (the emporium and a
/// Director's `_mcp` are two writers of one file) but fails at once instead.
/// A row with no id yet archives nothing — it has
/// no session to bury — and only the id the row holds *now* is covered, not one
/// it held before a session move.
///
/// The same rows also leave the consultation's title behind
/// ([`record_on`]), because the row that holds it is about to go: only a row that
/// has a title, which every consultation filed since v20 does, so a Goinkyo from
/// before titles is archived and records nothing. The two writes are one
/// transaction with the delete, so no reader sees a session archived and not yet
/// titled, or the reverse.
pub(super) fn archive_ended_sessions(
    tx: &rusqlite::Transaction<'_>,
    brigade_id: BrigadeId,
    token: Option<&str>,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare(
        "SELECT session_id, consult_title, role FROM brigade_members
         WHERE brigade_id = ?1 AND role IN (?2, ?3) AND session_id IS NOT NULL
           AND (?4 IS NULL OR member_token = ?4)",
    )?;
    let rows = stmt
        .query_map(
            params![
                brigade_id,
                BrigadeRole::Goinkyo.as_token(),
                BrigadeRole::Planner.as_token(),
                token
            ],
            |row| {
                Ok((
                    SessionId(row.get(0)?),
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, title, role) in &rows {
        archive_on(tx, id)?;
        if let (Some(title), Some(role)) = (title, BrigadeRole::from_token(role)) {
            record_on(tx, id, role, title)?;
        }
    }
    Ok(())
}

/// The token of the brigade's Director, `None` when it has none.
pub(super) fn director_token_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT member_token FROM brigade_members WHERE brigade_id = ?1 AND role = ?2",
            params![brigade_id, BrigadeRole::Director.as_token()],
            |row| row.get(0),
        )
        .optional()?)
}

/// Appends a message to the brigade's queue on whatever connection or transaction
/// the caller holds. `from_token` is a member token, or `banto_core::model::BANTO_SENDER` for the
/// messages banto itself writes.
pub(super) fn enqueue_message_on(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
    from_token: &str,
    to_role: BrigadeRole,
    to_member: Option<&str>,
    body: &str,
) -> Result<i64, StoreError> {
    conn.execute(
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
    Ok(conn.last_insert_rowid())
}

/// A member's row, its read cursor, the mail addressed to it, and the solo it may
/// hold, removed in the caller's transaction. Returns whether a row was there.
///
/// Not an operation anything calls on a role's say-so: [`Store::dismiss_worker`]
/// uses it after refusing a Planner, and the Plan's close uses it for the Planner
/// only after closing the Plan.
pub(super) fn remove_member_on(
    tx: &rusqlite::Transaction<'_>,
    brigade_id: BrigadeId,
    token: &str,
) -> Result<bool, StoreError> {
    let removed = tx.execute(
        "DELETE FROM brigade_members WHERE brigade_id = ?1 AND member_token = ?2",
        params![brigade_id, token],
    )? > 0;
    tx.execute(
        "DELETE FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
        params![brigade_id, token],
    )?;
    tx.execute(
        "DELETE FROM brigade_messages WHERE brigade_id = ?1 AND to_member = ?2",
        params![brigade_id, token],
    )?;
    // The member's own arrangement (rank, weight, collapsed) went with its row. The
    // soloed member is a brigade-level column naming a token, so it needs clearing
    // by hand: the Goinkyo's token is reused by the next consultation, and the
    // Planner's by the next Plan, and neither may inherit a solo.
    tx.execute(
        "UPDATE brigades SET solo_member = NULL WHERE id = ?1 AND solo_member = ?2",
        params![brigade_id, token],
    )?;
    Ok(removed)
}

impl Store {
    /// A brigade with no member yet. Formation does not use this: between this
    /// commit and the first member's, the janitor would delete the brigade (see
    /// [`Self::create_brigade_with_director`]).
    pub fn create_brigade(&self, name: &str) -> Result<BrigadeId, StoreError> {
        self.conn.execute(
            "INSERT INTO brigades (name, created_at_ms) VALUES (?1, ?2)",
            params![name, system_time_to_unix_ms(SystemTime::now())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Creates a brigade and its Director in one transaction, so that no commit
    /// ever holds a brigade with nobody in it.
    ///
    /// As two commits (`create_brigade`, then a member) they would leave a
    /// window in which the startup janitor ([`Self::delete_empty_brigades`])
    /// sees an empty brigade and deletes it, and the Director row is then
    /// written under an id no brigade has any more: the schema has no foreign
    /// key to refuse it, so the formation would report success for a brigade
    /// that is not there. Only one banto UI at a time uses a database, so no
    /// other process runs that janitor now; the single transaction keeps the
    /// guarantee without leaning on that.
    pub fn create_brigade_with_director(
        &mut self,
        name: &str,
        director_token: &str,
        session_id: &SessionId,
        agent: AgentKind,
    ) -> Result<BrigadeId, StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO brigades (name, created_at_ms) VALUES (?1, ?2)",
            params![name, system_time_to_unix_ms(SystemTime::now())],
        )?;
        let id = tx.last_insert_rowid();
        insert_member(
            &tx,
            id,
            director_token,
            BrigadeRole::Director,
            Some(session_id),
            Some(agent),
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Deletes a brigade together with its membership, queued messages, and
    /// read cursors — nothing is left orphaned for a deleted brigade id to
    /// accumulate under. (Pruning *read* messages out of a still-live brigade
    /// is a separate concern, left for later: this only clears a brigade that
    /// is gone entirely.) A consultation still open in it is over with the
    /// brigade, so its Goinkyo's session is archived in the same transaction
    /// ([`archive_ended_sessions`]).
    ///
    /// A Plan still open is closed first, in that same transaction, as `abandoned`
    /// by the operator, and its Planner's session is archived and titled with it:
    /// the Planner's row goes only with its Plan. The closed Plan is returned so
    /// that the caller can write its export, which outlives the brigade; the rows
    /// themselves are deleted with the rest.
    pub fn delete_brigade(
        &mut self,
        id: BrigadeId,
    ) -> Result<Option<super::plans::ClosedPlan>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let closed = match close_open_plan_on(&tx, id, &PlanClosing::Disband)? {
            PlanClosed::Closed(closed) => Some(*closed),
            // `Disband` has no caller to refuse and no result to check, so the
            // other two answers are "there was no Plan".
            PlanClosed::NoOpenPlan | PlanClosed::NotDirector | PlanClosed::Refused(_) => None,
        };
        archive_ended_sessions(&tx, id, None)?;
        tx.execute("DELETE FROM brigade_members WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigade_messages WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigade_cursors WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigade_progress WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM plans WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM plan_versions WHERE brigade_id = ?1", [id])?;
        tx.execute("DELETE FROM brigades WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(closed)
    }

    /// Records that a Director is waiting on the operator's decision.
    /// `request_text` holds the constant [`REQUEST_OPEN`] while a request is open
    /// and is NULL when none is, `request_seq` counts requests (so a second call is
    /// a new request even though nothing in the row differs) and `request_at_ms`
    /// stamps the time. `question` is the text the Director attached, already
    /// reduced by `banto_core::progress::check_question`, or `None`: it replaces
    /// whatever an earlier request held, so a second request with no question does
    /// not show the first one's. One `BEGIN IMMEDIATE` transaction, so two writers
    /// get two numbers. Returns the new sequence, `None` when there is no such
    /// brigade or `director_token` is not, in that same transaction, the
    /// brigade's Director: the caller checked its role a moment before, and a
    /// membership that has changed since (the brigade disbanded, the member
    /// removed) is not written as. Lives on the brigade's own row, so disbanding
    /// takes it with it.
    pub fn set_request(
        &mut self,
        brigade_id: BrigadeId,
        director_token: &str,
        question: Option<&str>,
    ) -> Result<Option<i64>, StoreError> {
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
        // CAST: whatever a hand-edited column holds reads as an integer.
        let changed = tx.execute(
            "UPDATE brigades SET request_text = ?2, \
             request_seq = CAST(request_seq AS INTEGER) + 1, request_at_ms = ?3, \
             request_question = ?4 \
             WHERE id = ?1",
            params![
                brigade_id,
                REQUEST_OPEN,
                system_time_to_unix_ms(SystemTime::now()),
                question
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let seq: i64 = tx.query_row(
            "SELECT CAST(request_seq AS INTEGER) FROM brigades WHERE id = ?1",
            [brigade_id],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(seq))
    }

    /// Clears the brigade's request when, and only when, it is still the one
    /// numbered `seq`: an answer to an older request never wipes a newer one.
    /// The sequence stays, so the next request is still a new number. `true`
    /// when a request was cleared.
    pub fn clear_request(&mut self, brigade_id: BrigadeId, seq: i64) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE brigades SET request_text = NULL, request_question = NULL \
             WHERE id = ?1 AND CAST(request_seq AS INTEGER) = ?2 AND request_text IS NOT NULL",
            params![brigade_id, seq],
        )?;
        Ok(changed > 0)
    }

    /// The sequence of the brigade's open request; `None` when there is none (or
    /// no such brigade). Open means `request_text` is not NULL, whatever it holds:
    /// the value is never read, so a hand edit (bytes that are not text, a number)
    /// cannot fail the read, which would drop the whole brigade from the tick's
    /// facts every tick.
    pub fn brigade_request(&self, brigade_id: BrigadeId) -> Result<Option<i64>, StoreError> {
        let row: Option<(bool, i64)> = self
            .conn
            .query_row(
                "SELECT request_text IS NOT NULL, CAST(request_seq AS INTEGER) \
                 FROM brigades WHERE id = ?1",
                [brigade_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(open, seq)| open.then_some(seq)))
    }

    /// What the brigade's open request holds for the reading screen: its sequence,
    /// the question the Director attached (`None` when it attached none), and when
    /// it was made. `None` when no request is open, or there is no such brigade.
    ///
    /// Read only while the screen is open, never by the tick, so unlike
    /// [`Self::brigade_request`] it reads the text: bytes that are not text in the
    /// column fail this read (the screen then says it could not read), not the
    /// tick's.
    pub fn brigade_request_detail(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Option<RequestDetail>, StoreError> {
        let row: Option<(bool, i64, Option<String>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT request_text IS NOT NULL, CAST(request_seq AS INTEGER), \
                 request_question, CAST(request_at_ms AS INTEGER) \
                 FROM brigades WHERE id = ?1",
                [brigade_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(row.and_then(|(open, seq, question, at_ms)| {
            open.then_some(RequestDetail {
                seq,
                question,
                requested_at_ms: at_ms,
            })
        }))
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
    /// its session id (the Director always does, at formation time;
    /// a freshly-spawned Worker starts with `None`, filled in later via
    /// [`Self::set_member_session`] once its agent product assigns one). Also
    /// seeds the member's cursor to "now" (the current max message id), so
    /// it sees only messages enqueued *after* joining.
    pub fn add_brigade_member(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        role: BrigadeRole,
        session_id: Option<&SessionId>,
    ) -> Result<(), StoreError> {
        self.add_brigade_member_with_agent(brigade_id, token, role, session_id, None)
    }

    /// Adds a member while recording the product banto selected for it, when
    /// that fact is available at creation time.
    ///
    /// Not how a Worker is added: a Worker's token is numbered by the brigade's
    /// counter, which only [`Self::add_worker`] advances. This is for the
    /// Director (formation) and fixtures. The Goinkyo has its own door,
    /// [`Self::add_goinkyo_member`], because its row carries a title.
    pub fn add_brigade_member_with_agent(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        role: BrigadeRole,
        session_id: Option<&SessionId>,
        agent: Option<AgentKind>,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        insert_member(&tx, brigade_id, token, role, session_id, agent)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the brigade's next Worker and returns its token (`worker-<n>`), one
    /// `BEGIN IMMEDIATE` transaction: `n` is `max(the brigade's counter, the
    /// highest `worker-<n>` among its current members) + 1`, the row is
    /// inserted under that token and the counter is set to `n`. The write lock
    /// is held from the read of the counter to its update, so two processes
    /// adding at once get different numbers.
    ///
    /// A Worker token is therefore never reused within a brigade: dismissing the
    /// highest-numbered Worker, or every one, does not hand its number back (the
    /// counter remembers the highest ever issued — what `AUTOINCREMENT` does for
    /// a rowid), and a gap left by dismissing a middle one stays a gap. The
    /// `max` with the current members covers a counter that is behind them
    /// (a hand-edited store; a brigade older than the counter, see the v16
    /// migration). A brigade has its own counter, and a disbanded brigade's is
    /// deleted with it — a new brigade has a new id and starts at 1. Fails if
    /// the brigade does not exist, and with
    /// [`StoreError::WorkerNumbersExhausted`] (nothing created) when the next
    /// number would pass [`MAX_WORKER_NUMBER`], the largest `worker_number`
    /// reads back: issuing a token the parser would not recognise would end the
    /// protection the `max` with the current members gives.
    ///
    /// The one way banto creates a Worker row: formation and the emporium's
    /// add-worker both come through here.
    pub fn add_worker(
        &mut self,
        brigade_id: BrigadeId,
        agent: Option<AgentKind>,
    ) -> Result<MemberToken, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // CAST so that whatever a hand-edited column holds (text, a real, a huge
        // value) reads as an integer instead of failing; then clamped to the range
        // numbers are issued in. A counter above the bound is as good as the bound
        // (every number is issued: exhausted), and a negative one is as good as 0
        // (the members decide); either way nothing below overflows.
        let issued: i64 = tx
            .query_row(
                "SELECT CAST(worker_seq AS INTEGER) FROM brigades WHERE id = ?1",
                [brigade_id],
                |row| row.get::<_, i64>(0),
            )?
            .clamp(0, MAX_WORKER_NUMBER);
        let highest_member = {
            let mut stmt =
                tx.prepare("SELECT member_token FROM brigade_members WHERE brigade_id = ?1")?;
            let tokens = stmt.query_map([brigade_id], |row| row.get::<_, String>(0))?;
            let mut highest = 0;
            for token in tokens {
                if let Some(n) = worker_number(&token?) {
                    highest = highest.max(n);
                }
            }
            highest
        };
        // Both are at most the bound, so this cannot overflow; past the bound there
        // is no number the parser would still recognise, so none is issued.
        let next = issued.max(highest_member) + 1;
        if next > MAX_WORKER_NUMBER {
            return Err(StoreError::WorkerNumbersExhausted);
        }
        let token = format!("worker-{next}");
        insert_member(&tx, brigade_id, &token, BrigadeRole::Worker, None, agent)?;
        tx.execute(
            "UPDATE brigades SET worker_seq = ?2 WHERE id = ?1",
            params![brigade_id, next],
        )?;
        tx.commit()?;
        Ok(token)
    }

    /// Records the session id a member's agent product assigned to a member
    /// banto spawned ahead of time (a Worker's `session_id`, initially
    /// `None`). A no-op if `(brigade_id, token)` doesn't exist.
    ///
    /// One session belongs to at most one member: any other row
    /// holding this id is cleared back to "awaiting discovery" in the same
    /// transaction. A duplicate would make `brigade_of_session`
    /// answer with whichever row sorted first, so an `_mcp` connection
    /// could speak as the wrong member — worth spending a transaction to
    /// make unrepresentable, and it repairs a row a past duplicate left
    /// behind.
    pub fn set_member_session(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE brigade_members SET session_id = NULL
             WHERE session_id = ?1
               AND NOT (brigade_id = ?2 AND member_token = ?3)",
            params![session_id.0, brigade_id, token],
        )?;
        tx.execute(
            "UPDATE brigade_members SET session_id = ?1
             WHERE brigade_id = ?2 AND member_token = ?3",
            params![session_id.0, brigade_id, token],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records the product used to open a member pane. This is independent
    /// of session discovery: Claude can publish live state before it writes
    /// the transcript that creates an App row, and relay must not deadlock on
    /// that missing row.
    pub fn set_member_agent(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        agent: AgentKind,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE brigade_members SET agent = ?1
             WHERE brigade_id = ?2 AND member_token = ?3",
            params![agent.as_token(), brigade_id, token],
        )?;
        Ok(())
    }

    /// Resets a member's `session_id` back to `None` — the fix for a
    /// stranded Goinkyo, one that never ran a single turn: Claude Code
    /// writes no `projects/*.jsonl` transcript until a session's first turn,
    /// so `session_id` staying set forever (nothing else ever clears it)
    /// keeps `gather_goinkyo_observations`' `AwaitingSpawn` check (which
    /// fires only on `session_id: None`) from ever seeing this row as
    /// spawnable again — a member stuck exactly as unreachable-but-not-gone
    /// as it looks, until dismissed. Its consultation request file outlives
    /// this (kept until `dismiss_worker`/`dismiss_goinkyo` actually removes
    /// the row), so the next `AwaitingSpawn` restarts the very same
    /// consultation, not a fresh one. A no-op if `(brigade_id, token)`
    /// doesn't exist.
    pub fn clear_member_session(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE brigade_members SET session_id = NULL
             WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, token],
        )?;
        Ok(())
    }

    /// Removes a member from a brigade together with its cursor, and nothing
    /// else: no mail, no solo, and — unlike [`Self::dismiss_worker`] — no archive
    /// of a Goinkyo's session. Removing a non-member is a no-op, and so is removing
    /// a Planner, which goes only with its Plan ([`Self::close_plan`]). Nothing in the
    /// product calls this; it is the bare row removal the store tests build their
    /// rosters with, and a caller that ends a member for the operator wants
    /// `dismiss_worker`.
    pub fn remove_brigade_member(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM brigade_members
             WHERE brigade_id = ?1 AND member_token = ?2 AND role <> ?3",
            params![brigade_id, token, BrigadeRole::Planner.as_token()],
        )?;
        tx.execute(
            "DELETE FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2
             AND NOT EXISTS (SELECT 1 FROM brigade_members
                             WHERE brigade_id = ?1 AND member_token = ?2)",
            params![brigade_id, token],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes a member from its brigade for good — the emporium's prefix-`x`
    /// on a Worker or a Goinkyo pane, a Worker that ended on its own and could
    /// not be relaunched, and `dismiss_goinkyo`'s own way of ending a
    /// consultation from the other process. `token` is the only thing this ever
    /// filters by, no role — named for the Worker callers, kept for the
    /// Goinkyo's rather than adding a same-shaped sibling with a different name (see
    /// `crate::mcp::tool_dismiss_goinkyo`'s own doc): membership, its read
    /// cursor, AND — unlike [`Self::remove_brigade_member`], which leaves
    /// mail alone — any message addressed specifically to it
    /// (`to_member = token`). The mail purge is
    /// load-bearing, not tidiness: a token can come to denote a different
    /// member (the Goinkyo's is the fixed `goinkyo`, reused by every
    /// consultation; a Worker's `worker-N` never is within its brigade, but a
    /// hand-edited store could), so leaving its addressed-but-unread mail behind
    /// would hand it straight to whichever stranger inherits the token next. Broadcasts (`to_member IS NULL`)
    /// are untouched — they belong to the role, not this one member, and
    /// every other member's own cursor still governs what they've seen of
    /// them. Dismissing a non-member is a no-op. Returns whether a member row
    /// was there to remove (`false`: it was already gone).
    ///
    /// A *Goinkyo's* session is archived in the same transaction, decided by the
    /// role the row itself holds and never by `token` (the exited-Worker path
    /// comes through here too): once the row is gone its session would come back
    /// to the list as an ordinary row, and the operator has said those are to be
    /// buried ([`archive_ended_sessions`]).
    ///
    /// A *Planner* is never removed here, by whatever token the caller names: its
    /// row lives exactly as long as its Plan is open and is removed only in the
    /// transaction that closes the Plan ([`Self::close_plan`]). Returns `false`
    /// for it, and writes nothing, so a caller that dismissed it learns the row
    /// is still there only by reading the roster.
    pub fn dismiss_worker(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let is_planner: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM brigade_members
             WHERE brigade_id = ?1 AND member_token = ?2 AND role = ?3)",
            params![brigade_id, token, BrigadeRole::Planner.as_token()],
            |row| row.get(0),
        )?;
        if is_planner {
            return Ok(false);
        }
        archive_ended_sessions(&tx, brigade_id, Some(token))?;
        let removed = remove_member_on(&tx, brigade_id, token)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Every member row of every brigade as the bare pair `(brigade, token)`, in
    /// one read. What the data directory's sweep (`crate::data_dir::sweep`) asks
    /// before it deletes a working file, so it must not know *less* than the store
    /// does: a row is listed whatever its role or agent say, and a row that cannot
    /// be read at all fails the whole read (nothing is then deleted) instead of
    /// being left out — which would make its member's files look ownerless.
    pub fn all_member_tokens(&self) -> Result<Vec<(BrigadeId, MemberToken)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT brigade_id, member_token FROM brigade_members")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns a brigade's members: the Director first, then every other role
    /// ordered by token. This puts `goinkyo` before `worker-1`, and Worker
    /// tokens sort as text (`worker-10` before `worker-2`: a brigade's Worker
    /// counter only grows, so numbers pass 9 easily). Readers that need the
    /// order members joined use [`Self::brigade_members_in_join_order`].
    pub fn brigade_members(&self, brigade_id: BrigadeId) -> Result<Vec<BrigadeMember>, StoreError> {
        read_roster(
            &self.conn,
            brigade_id,
            "CASE role WHEN 'director' THEN 0 ELSE 1 END, member_token",
        )
    }

    /// The same members as [`Self::brigade_members`], in the order they joined
    /// the brigade: the Director first, then everyone else by when its row was
    /// added. This is the order the emporium stacks the tiles of a brigade
    /// nobody has rearranged, so that it is the same every time the brigade is
    /// staged and across a restart.
    ///
    /// The join order is the table's `rowid`, which SQLite assigns in
    /// insertion order, so it needs no column of its own: a member added
    /// later has a larger one; a dismissed member's row is deleted, so a member
    /// added under the same token afterwards (only the Goinkyo's `goinkyo`, by
    /// each new consultation, or a hand-edited store — a Worker's number is
    /// never reissued, [`Self::add_worker`]) is a new row and joins last;
    /// updating a row (its session id, its briefing) never moves it.
    ///
    /// What this does and does not promise. The table has a composite primary
    /// key and no `INTEGER PRIMARY KEY`, so its rowids are the hidden kind that
    /// SQLite may renumber on `VACUUM` or when a table is rebuilt. Nothing in
    /// banto does either (it never runs `VACUUM`, and no migration after v7
    /// rebuilds this table), so the order holds for a database only banto has
    /// written to; an external `VACUUM` or rebuild may renumber it. A future
    /// migration that rebuilds the table has to copy the rows `ORDER BY rowid`.
    ///
    /// A brigade that already existed when migration v7 rebuilt the table
    /// (the one that introduced member tokens) has the order that migration
    /// wrote, not the order its members originally joined — that was never
    /// recorded anywhere the migration kept. The script inserted the
    /// Directors, then the Workers numbered `worker-1`, `worker-2`, … by their
    /// old session ids, with no `ORDER BY` on the `INSERT … SELECT`; with the
    /// bundled SQLite (3.53) the rows land in that numbering order, which
    /// SQLite does not guarantee. `brigade_members` itself
    /// orders by token and is left alone — it puts `goinkyo` before `worker-1`
    /// and `worker-10` before `worker-2`, and other readers (the MCP roster)
    /// are used to it.
    pub fn brigade_members_in_join_order(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Vec<BrigadeMember>, StoreError> {
        read_roster(
            &self.conn,
            brigade_id,
            "CASE role WHEN 'director' THEN 0 ELSE 1 END, rowid",
        )
    }

    /// One member of a brigade, as the roster reads present it — a roster read
    /// and a pick, not a query of its own, so this can never disagree with
    /// [`Self::brigade_members`] about what the member is called (whether
    /// another member holds the same name is a fact about the whole roster, and
    /// a single row cannot answer it).
    pub fn brigade_member(
        &self,
        brigade_id: BrigadeId,
        token: &str,
    ) -> Result<Option<BrigadeMember>, StoreError> {
        Ok(self
            .brigade_members(brigade_id)?
            .into_iter()
            .find(|member| member.token == token))
    }

    /// Sets, changes or (with an empty `input`) clears the display name of
    /// `token`'s member, deciding everything inside one `BEGIN IMMEDIATE`
    /// transaction: it reads the roster, finds the member by its token, checks it
    /// is a Worker, applies the rule (`banto_core::member_name::validate`,
    /// uniqueness against the roster as read *there*) and writes. While that
    /// transaction holds the write lock no other process can dismiss or add a
    /// member, so nothing the decision rests on can change before it is made —
    /// the window a read-then-write split would leave. Two callers cannot both
    /// claim a name either: the second waits for the first's commit and is then
    /// refused it as taken. A refusal changes nothing.
    ///
    /// A name can therefore never land on a replacement Worker: a Worker's token
    /// is never reused in its brigade ([`Self::add_worker`]), so `worker-2` is the
    /// same member for as long as it exists. (The Goinkyo's `goinkyo` is reused by
    /// each consultation, but it cannot be named.) Addressing by token is the
    /// caller's only handle, as with `send_to_peer`.
    ///
    /// Touches nothing but the one column, so the member's place in the join
    /// order and every other preference are unchanged.
    pub fn name_member(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        input: &str,
    ) -> Result<MemberNaming, StoreError> {
        self.name_member_with(brigade_id, token, input, || {})
    }

    /// [`Self::name_member`] with a hook that runs right after the roster has
    /// been read and before anything is decided or written — the point a test
    /// uses to show that the write lock is already held, i.e. that nothing can
    /// change between the read and the write.
    fn name_member_with(
        &mut self,
        brigade_id: BrigadeId,
        token: &str,
        input: &str,
        after_read: impl FnOnce(),
    ) -> Result<MemberNaming, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let roster = read_roster(
            &tx,
            brigade_id,
            "CASE role WHEN 'director' THEN 0 ELSE 1 END, rowid",
        )?;
        after_read();
        let Some(target) = roster.iter().find(|member| member.token == token) else {
            return Ok(MemberNaming::NoSuchMember);
        };
        if target.role != BrigadeRole::Worker {
            return Ok(MemberNaming::NotAWorker(target.role));
        }
        let names: Vec<(&str, Option<&str>)> = roster
            .iter()
            .map(|member| (member.token.as_str(), member.display_name.as_deref()))
            .collect();
        let name = match member_name::validate(input, token, &names) {
            Ok(name) => name,
            Err(rejection) => return Ok(MemberNaming::Refused(rejection)),
        };
        tx.execute(
            "UPDATE brigade_members SET display_name = ?3
             WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, token, name],
        )?;
        let previous = target.display_name.clone();
        tx.commit()?;
        Ok(MemberNaming::Set { name, previous })
    }

    /// Records that `banto _hook` reached this member during `session_id` —
    /// the canary `docs/notes/codex-briefing-spike.md` exists for: every
    /// failure mode the spike measured for a Codex member's briefing
    /// (deferred tool, unapproved MCP call, untrusted hook) fails *silently*,
    /// so "the hook fired at all" has to be observable some other way. Not
    /// tied to a briefing template actually existing — even a role with an
    /// empty (disabled) prompt still proves the hook reached it, which is
    /// exactly the fact worth keeping distinguishable from "never fired".
    /// A `token` this brigade no longer has is a no-op: nothing to record
    /// against, not an error (the hook that calls this must never fail on
    /// its account — see `crate::hook`'s module doc).
    pub fn record_briefing(
        &self,
        brigade_id: BrigadeId,
        token: &str,
        session_id: &SessionId,
        at: SystemTime,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE brigade_members SET briefed_at_ms = ?1, briefed_session_id = ?2
             WHERE brigade_id = ?3 AND member_token = ?4",
            params![system_time_to_unix_ms(at), session_id.0, brigade_id, token],
        )?;
        Ok(())
    }

    /// Returns the brigade a session belongs to, its token, and its
    /// role there, if any. If the session is (unusually) linked from more
    /// than one member, returns the lowest brigade id, deterministically.
    pub fn brigade_of_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(BrigadeId, MemberToken, BrigadeRole)>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT brigade_id, member_token, role FROM brigade_members
                 WHERE session_id = ?1
                 ORDER BY brigade_id LIMIT 1",
                [&session_id.0],
                |row| {
                    let id: BrigadeId = row.get(0)?;
                    let token: String = row.get(1)?;
                    let role_token: String = row.get(2)?;
                    let role = BrigadeRole::from_token(&role_token)
                        .ok_or_else(|| unrecognized_role_error(2, &role_token))?;
                    Ok((id, token, role))
                },
            )
            .optional()?)
    }

    /// Every known Worker's or Goinkyo's session id, across every brigade,
    /// that has been assigned one so far (a member banto is still waiting on
    /// its agent product to assign one has no entry). Used to hide these
    /// from the session list — their own pane already represents them, so a
    /// second row for the same content is noise, not information. The
    /// Director is deliberately excluded: that pane is the operator's own
    /// interactive session, not a redundant view of it.
    ///
    /// A Goinkyo is hidden only *while* it's still a member: dismissal deletes
    /// its row (`Self::dismiss_worker`), so it drops out of this query. Its
    /// session is archived in that same transaction, so the row the list gets
    /// back is one the operator has to ask for (`a` to the archive level; `D`
    /// restores it) — the transcript is still the only place its answer is
    /// readable, which is why it is archived rather than deleted or left hidden
    /// for good.
    pub fn brigade_hidden_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM brigade_members
             WHERE role IN ('worker', 'goinkyo', 'planner') AND session_id IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok(SessionId(row.get(0)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every known Director's session id, across every brigade. Used
    /// to mark Directors in the session list.
    pub fn brigade_director_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id FROM brigade_members
             WHERE role = 'director' AND session_id IS NOT NULL",
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
        enqueue_message_on(&self.conn, brigade_id, from_token, to_role, to_member, body)
    }

    /// Pull the messages in `brigade_id` addressed to `recipient_role` (and,
    /// within that role, either broadcast or specifically to `member_token` —
    /// see [`Self::enqueue_brigade_message`]) that `member_token` has not
    /// seen yet (id past its cursor), oldest first, and advance that
    /// member's cursor past them — so a later call returns only what has
    /// arrived since. The cursor is scoped to `(brigade_id, member_token)`,
    /// so it doesn't carry over if the token's underlying session
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
                "SELECT id, from_session, body, to_member, interrupt_outcome FROM brigade_messages
                 WHERE brigade_id = ?1 AND to_role = ?2 AND id > ?3
                   AND (to_member IS NULL OR to_member = ?4)
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(
                params![brigade_id, recipient_role.as_token(), cursor, member_token],
                |row| {
                    let outcome: Option<String> = row.get(4)?;
                    Ok(BrigadeMessage {
                        id: row.get(0)?,
                        from_token: row.get(1)?,
                        body: row.get(2)?,
                        to_member: row.get(3)?,
                        interrupted: outcome
                            .and_then(|token| InterruptOutcome::from_token(&token))
                            .filter(|outcome| outcome.gate_refused()),
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
            tx.execute("DELETE FROM brigade_progress WHERE brigade_id = ?1", [id])?;
            tx.execute("DELETE FROM plans WHERE brigade_id = ?1", [id])?;
            tx.execute("DELETE FROM plan_versions WHERE brigade_id = ?1", [id])?;
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
    /// When `member_token` last had a message in either direction, as unix
    /// milliseconds, or `None` if it never has.
    ///
    /// Matches on the token in both columns because `from_session` holds a
    /// member token despite its name (see [`Self::enqueue_brigade_message`]'s
    /// `from_token` parameter). A broadcast (`to_member IS NULL`) is
    /// deliberately not counted as reaching this member: the one caller that
    /// needs this asks about a Goinkyo, and a Director's broadcast never
    /// reaches one.
    pub fn last_member_exchange_ms(
        &self,
        brigade_id: BrigadeId,
        member_token: &str,
    ) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MAX(created_at_ms) FROM brigade_messages
             WHERE brigade_id = ?1 AND (from_session = ?2 OR to_member = ?2)",
            params![brigade_id, member_token],
            |row| row.get::<_, Option<i64>>(0),
        )?)
    }

    /// A brigade's stored arrangement preferences: the brigade-level values
    /// and, for every member that has any, its stack rank, row weight and
    /// collapsed flag. A brigade that does not exist reads as no preference.
    ///
    /// The file is banto's own but it outlives versions, so every value is
    /// validated on the way out and an unusable one reads as "no
    /// preference", never as an error or an out-of-range number: a permille
    /// outside 0..=1000, a width outside 1..=1000 columns, a negative or
    /// absurd rank, a weight outside 1..=65535, and a `solo_member` that is
    /// not a member of this brigade.
    pub fn brigade_layout(&self, brigade_id: BrigadeId) -> Result<StoredLayout, StoreError> {
        let brigade = self
            .conn
            .query_row(
                "SELECT master_permille, collapsed_stack_width, solo_member
                 FROM brigades WHERE id = ?1",
                [brigade_id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((master_permille, collapsed_stack_width, solo_member)) = brigade else {
            return Ok(StoredLayout::default());
        };
        let mut stmt = self.conn.prepare(
            "SELECT member_token, stack_rank, row_weight, minimized
             FROM brigade_members WHERE brigade_id = ?1
             ORDER BY member_token",
        )?;
        let rows = stmt.query_map([brigade_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut tokens = Vec::new();
        let mut members = Vec::new();
        for row in rows {
            let (token, rank, weight, minimized) = row?;
            let entry = StoredMemberLayout {
                token: token.clone(),
                stack_rank: rank
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| *v < MAX_STORED_RANK),
                row_weight: weight
                    .and_then(|v| u16::try_from(v).ok())
                    .filter(|v| *v >= 1),
                // Only 1 is "collapsed"; any other number is not one of ours
                // and reads as no preference like every other stored value.
                minimized: minimized == 1,
            };
            tokens.push(token);
            if entry.stack_rank.is_some() || entry.row_weight.is_some() || entry.minimized {
                members.push(entry);
            }
        }
        Ok(StoredLayout {
            master_permille: master_permille
                .and_then(|v| u16::try_from(v).ok())
                .filter(|v| *v <= 1000),
            collapsed_stack_width: collapsed_stack_width
                .and_then(|v| u16::try_from(v).ok())
                .filter(|v| (1..=1000).contains(v)),
            solo_member: solo_member.filter(|token| tokens.contains(token)),
            members,
        })
    }

    /// Replace a brigade's stored arrangement from a whole snapshot, in one
    /// transaction: the brigade-level values (a `None` writes NULL, "the
    /// default") and every member the snapshot lists.
    ///
    /// A token in the snapshot with no member row is ignored — this never
    /// inserts a member — and a member row the snapshot does not list is left
    /// exactly as it is. A solo naming a token with no member row is written
    /// as none. Idempotent: writing the same snapshot again changes nothing.
    /// A brigade that does not exist (disbanded since the snapshot was taken)
    /// is a silent no-op: no row is created and nothing is reported.
    pub fn save_brigade_layout(
        &mut self,
        brigade_id: BrigadeId,
        layout: &StoredLayout,
    ) -> Result<(), StoreError> {
        // IMMEDIATE: the statements below must see one consistent state of the
        // brigade and its members, not one another process changes midway.
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // The soloed member is written only if it is a member of this brigade
        // *now*. A snapshot can be older than a dismissal that happened in
        // another process (the emporium, an agent's MCP server): without the
        // check the late save would put a dismissed member's token back, and
        // a later member under that token (the Goinkyo's is reused) would
        // inherit the solo.
        tx.execute(
            "UPDATE brigades
             SET master_permille = ?2,
                 collapsed_stack_width = ?3,
                 solo_member = CASE
                     WHEN ?4 IS NOT NULL AND EXISTS (
                         SELECT 1 FROM brigade_members
                         WHERE brigade_id = ?1 AND member_token = ?4
                     ) THEN ?4
                     ELSE NULL
                 END
             WHERE id = ?1",
            params![
                brigade_id,
                layout.master_permille,
                layout.collapsed_stack_width,
                layout.solo_member
            ],
        )?;
        for member in &layout.members {
            tx.execute(
                "UPDATE brigade_members
                 SET stack_rank = ?3, row_weight = ?4, minimized = ?5
                 WHERE brigade_id = ?1 AND member_token = ?2",
                params![
                    brigade_id,
                    member.token,
                    member.stack_rank,
                    member.row_weight,
                    i64::from(member.minimized)
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The newest message in `brigade_id` that `member_token` (of
    /// `recipient_role`) can receive — addressed to its role and either a
    /// broadcast or to it by name, the same visibility rule as
    /// [`Self::fetch_brigade_messages`] — reduced to a one-line glance, with
    /// its sender's token.
    ///
    /// Not bounded by the read cursor, on purpose and without cost to the
    /// nudge: whenever [`Self::has_unseen_brigade_messages`] is true some
    /// matching message is past the cursor, and the newest matching message
    /// has an id at least as large, so it is past the cursor too — it is
    /// *the* newest unread one. Once everything is read this is the newest
    /// read one, which is what the collapsed tile shows.
    ///
    /// Only messages past the member's own `joined_after_id` count: mail queued
    /// before it joined is not mail it could ever receive (delivery starts at
    /// the same point), and a Goinkyo's token is reused by the next consultation.
    /// A token with no member row, or a row from before that column, has no
    /// bound.
    ///
    /// Read-only and never touches the read cursor: what the operator glances
    /// at on a collapsed tile must not count as the member having read it.
    pub fn latest_inbound_glance(
        &self,
        brigade_id: BrigadeId,
        member_token: &str,
        recipient_role: BrigadeRole,
    ) -> Result<Option<MessageGlance>, StoreError> {
        self.glance(
            "SELECT substr(ltrim(body, char(9, 10, 11, 12, 13, 32)), 1, ?4), created_at_ms,
                    from_session
             FROM brigade_messages
             WHERE brigade_id = ?1 AND to_role = ?2
               AND (to_member IS NULL OR to_member = ?3)
               AND id > COALESCE((SELECT joined_after_id FROM brigade_members
                                  WHERE brigade_id = ?1 AND member_token = ?3), 0)
             ORDER BY id DESC LIMIT 1",
            params![
                brigade_id,
                recipient_role.as_token(),
                member_token,
                GLANCE_READ_CHARS
            ],
        )
    }

    /// The newest message `member_token` sent in `brigade_id`, reduced to a
    /// one-line glance. Read-only. Bounded by the member's `joined_after_id`
    /// like [`Self::latest_inbound_glance`]: a sender is identified only by its
    /// token, and the Goinkyo's is reused, so what an earlier holder of the
    /// token sent is not this member's.
    pub fn latest_sent_glance(
        &self,
        brigade_id: BrigadeId,
        member_token: &str,
    ) -> Result<Option<MessageGlance>, StoreError> {
        self.glance(
            "SELECT substr(ltrim(body, char(9, 10, 11, 12, 13, 32)), 1, ?3), created_at_ms,
                    from_session
             FROM brigade_messages
             WHERE brigade_id = ?1 AND from_session = ?2
               AND id > COALESCE((SELECT joined_after_id FROM brigade_members
                                  WHERE brigade_id = ?1 AND member_token = ?2), 0)
             ORDER BY id DESC LIMIT 1",
            params![brigade_id, member_token, GLANCE_READ_CHARS],
        )
    }

    /// The highest message id in the store, or 0 when it has none. A message
    /// glance can only change when a row is appended (a dismissal or disband
    /// deletes rows of members that are gone, who are no longer asked
    /// about), so an unchanged value means no glance read can have a new
    /// answer. One rowid lookup, not a scan.
    pub fn max_brigade_message_id(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM brigade_messages",
            [],
            |row| row.get(0),
        )?)
    }

    fn glance(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Option<MessageGlance>, StoreError> {
        let row = self
            .conn
            .query_row(sql, params, |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .optional()?;
        Ok(row.map(|(body, created_at_ms, from_token)| MessageGlance {
            created_at_ms,
            line: glance_line(&body),
            from_token: Some(from_token),
        }))
    }

    /// Enqueues `body` to the Worker `member_token` with a request to stop its
    /// running turn, in one transaction that first checks, as `set_request`
    /// does, that `director_token` is the brigade's Director and `member_token`
    /// one of its Workers. `None` when either no longer holds: the caller
    /// checked a moment before, and a membership that changed since (a dismissal,
    /// a disband) is not written as. Returns the message id, which is also the
    /// handle of the request.
    pub fn enqueue_interrupt_message(
        &mut self,
        brigade_id: BrigadeId,
        director_token: &str,
        member_token: &str,
        body: &str,
    ) -> Result<Option<i64>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let holds: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM brigade_members
                           WHERE brigade_id = ?1 AND member_token = ?2 AND role = ?3)
                AND EXISTS(SELECT 1 FROM brigade_members
                           WHERE brigade_id = ?1 AND member_token = ?4 AND role = ?5)",
            params![
                brigade_id,
                director_token,
                BrigadeRole::Director.as_token(),
                member_token,
                BrigadeRole::Worker.as_token()
            ],
            |row| row.get(0),
        )?;
        if !holds {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO brigade_messages
                 (brigade_id, from_session, to_role, to_member, body, created_at_ms, interrupt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                brigade_id,
                director_token,
                BrigadeRole::Worker.as_token(),
                member_token,
                body,
                system_time_to_unix_ms(SystemTime::now())
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    /// The brigade's interrupt requests the UI has not decided or their Worker has
    /// not read, oldest first, with the word each carries. Two lists in one query,
    /// told apart by the row: `undecided` (no word, or only the Worker's own `_mcp`
    /// has written one, `gated` or `capped`) is what the UI still has to decide,
    /// and not `read` is what the operator may need to hear about. The gate can
    /// refuse a call before the UI's first tick sees the request, and a row it got
    /// to first is still the UI's to decide: the relay nudge state is reset and the
    /// status line shown once per request, and the UI's own word goes over no word
    /// only, so the gate's stays.
    ///
    /// Rows with no word are not bounded by the read cursor, on purpose: a Worker
    /// can pull the message before the UI looks, and the request still has to be
    /// answered (the answer is then `read`). `read` says which case a row is.
    pub fn pending_interrupts(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Vec<PendingInterrupt>, StoreError> {
        let mut stmt = self.conn.prepare(&pending_interrupts_sql())?;
        let rows = stmt.query_map([brigade_id], |row| {
            Ok(PendingInterrupt {
                message_id: row.get(0)?,
                member: row.get(1)?,
                created_at_ms: row.get(2)?,
                read: row.get(3)?,
                outcome: row
                    .get::<_, Option<String>>(4)?
                    .and_then(|token| InterruptOutcome::from_token(&token)),
                undecided: row.get(5)?,
                line: row.get::<_, String>(6)?.trim_end_matches('\r').to_string(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Records what was decided about the request on message `message_id`: the
    /// UI's first decision, or the Worker's own `_mcp` writing `gated` or
    /// `capped`. The word is written only over what
    /// [`InterruptOutcome::replaceable`] allows, or over no word, so a repeated or
    /// late write is a visible no-op, never an overwrite. `true` when the row
    /// changed.
    pub fn record_interrupt_outcome(
        &mut self,
        message_id: i64,
        outcome: InterruptOutcome,
    ) -> Result<bool, StoreError> {
        // The tokens are fixed strings written in `InterruptOutcome`, so
        // spelling them into the statement is not an injection and keeps the
        // table the only place the rule is written.
        let replaceable = outcome
            .replaceable()
            .iter()
            .map(|word| format!("'{}'", word.as_token()))
            .collect::<Vec<_>>();
        let guard = if replaceable.is_empty() {
            "interrupt_outcome IS NULL".to_string()
        } else {
            format!(
                "(interrupt_outcome IS NULL OR interrupt_outcome IN ({}))",
                replaceable.join(", ")
            )
        };
        let sql = format!(
            "UPDATE brigade_messages SET interrupt_outcome = ?2
             WHERE id = ?1 AND interrupt = 1 AND {guard}"
        );
        Ok(self
            .conn
            .execute(&sql, params![message_id, outcome.as_token()])?
            > 0)
    }

    /// Where the request on `message_id` stands. An outcome token this build does
    /// not know (a newer banto wrote it) reads as undecided.
    pub fn interrupt_state(&self, message_id: i64) -> Result<InterruptState, StoreError> {
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT interrupt_outcome FROM brigade_messages
                 WHERE id = ?1 AND interrupt = 1",
                [message_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match row {
            None => InterruptState::Gone,
            Some(outcome) => match outcome.and_then(|token| InterruptOutcome::from_token(&token)) {
                Some(outcome) => InterruptState::Decided(outcome),
                None => InterruptState::Undecided,
            },
        })
    }

    /// The brigade's interrupt requests whose message its Worker has not read,
    /// oldest first, each with its outcome and when it was written. What
    /// `brigade_status` shows, and only while the message is unread: once it is
    /// read there is nothing left to say, and the witness of delivery is the read
    /// cursor, never the input line.
    pub fn unread_interrupts(
        &self,
        brigade_id: BrigadeId,
    ) -> Result<Vec<UnreadInterrupt>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.to_member, m.interrupt_outcome, m.created_at_ms
             FROM brigade_messages m
             WHERE m.brigade_id = ?1 AND m.interrupt = 1 AND m.to_member IS NOT NULL
               AND m.id > COALESCE((SELECT c.last_seen_id FROM brigade_cursors c
                                    WHERE c.brigade_id = m.brigade_id
                                      AND c.member_token = m.to_member), 0)
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map([brigade_id], |row| {
            let outcome: Option<String> = row.get(1)?;
            Ok(UnreadInterrupt {
                member: row.get(0)?,
                outcome: outcome.and_then(|token| InterruptOutcome::from_token(&token)),
                created_at_ms: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The ids of the interrupt requests `member_token` has not read, oldest
    /// first, whatever their outcome: what the Worker's gate refuses its tool
    /// calls for. Empty once the Worker has pulled them (the read cursor passed
    /// them), which is the whole of the gate's release.
    pub fn unread_interrupt_ids(
        &self,
        brigade_id: BrigadeId,
        member_token: &str,
    ) -> Result<Vec<i64>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id
             FROM brigade_messages m
             WHERE m.brigade_id = ?1 AND m.interrupt = 1 AND m.to_member = ?2
               AND m.id > COALESCE((SELECT c.last_seen_id FROM brigade_cursors c
                                    WHERE c.brigade_id = m.brigade_id
                                      AND c.member_token = m.to_member), 0)
             ORDER BY m.id",
        )?;
        let rows = stmt.query_map(params![brigade_id, member_token], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

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

    /// Issue a wake, retaining it until a waiter consumes it.
    pub fn issue_wake(&self, brigade_id: BrigadeId, member: &str) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "UPDATE brigade_members SET wake_issued = wake_issued + 1 WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, member],
        )? != 0)
    }

    pub fn wake_pending(&self, brigade_id: BrigadeId, member: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT wake_issued > wake_received FROM brigade_members WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, member], |row| row.get(0),
        ).optional()?.unwrap_or(false))
    }

    /// Register a waiter, superseding every earlier waiter for this member.
    pub fn arm_wake(&self, brigade_id: BrigadeId, member: &str) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "UPDATE brigade_members SET wake_waiter = wake_waiter + 1 WHERE brigade_id = ?1 AND member_token = ?2 RETURNING wake_waiter",
            params![brigade_id, member], |row| row.get(0),
        ).optional()?)
    }

    /// `None` means this waiter has been superseded or its member removed.
    pub fn poll_wake(
        &mut self,
        brigade_id: BrigadeId,
        member: &str,
        generation: i64,
    ) -> Result<Option<bool>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(i64, i64, i64, String)> = tx.query_row(
            "SELECT wake_waiter, wake_issued, wake_received, role FROM brigade_members WHERE brigade_id = ?1 AND member_token = ?2",
            params![brigade_id, member], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((waiter, issued, received, role)) = row else {
            return Ok(None);
        };
        if waiter != generation {
            return Ok(None);
        }
        let mut unread = false;
        if issued > received {
            tx.execute("UPDATE brigade_members SET wake_received = wake_issued WHERE brigade_id = ?1 AND member_token = ?2", params![brigade_id, member])?;
            unread = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM brigade_messages WHERE brigade_id = ?1 AND to_role = ?2 AND id > COALESCE((SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?3), 0) AND (to_member IS NULL OR to_member = ?3))",
                params![brigade_id, role, member], |row| row.get(0),
            )?;
        }
        tx.commit()?;
        Ok(Some(unread))
    }
}

/// Ranks beyond this read as "no preference": no brigade has anything like
/// this many members, so a larger stored number is not one of ours.
const MAX_STORED_RANK: u32 = 10_000;

/// How many characters of a body the glance queries read. A body can be many
/// kilobytes of pasted output; the glance only needs its first non-empty line.
/// The bound is applied *after* SQLite has trimmed the body's leading ASCII
/// whitespace (`ltrim(body, char(9, 10, 11, 12, 13, 32))`), so any amount of
/// blank lines before the first line still finds it, and only that many
/// characters ever cross into Rust. What this does not skip: a first "line"
/// made of thousands of other characters that [`glance_line`] then reduces to
/// nothing (control or format characters only), which yields a blank glance.
const GLANCE_READ_CHARS: i64 = 4096;

/// The longest a glance line is cut to, in characters. The drawing clips by
/// display width on top of this.
const GLANCE_LINE_CHARS: usize = 200;

/// A message body reduced to one line for a collapsed tile: its first
/// non-empty line, with control characters and Unicode format characters
/// removed (a tab counts as a space) and surrounding whitespace trimmed, cut
/// to [`GLANCE_LINE_CHARS`] characters. Done where the text leaves the store,
/// so nothing a member wrote — an escape sequence, a stray newline, a bidi
/// override that would reorder the row, zero-width padding — can steer how
/// the strip is drawn. The reduction itself is `banto_core::display_text::display_safe`,
/// the one policy for text someone else wrote.
pub fn glance_line(body: &str) -> String {
    body.split('\n')
        .map(|line| display_safe(line).trim().to_string())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(GLANCE_LINE_CHARS).collect())
        .unwrap_or_default()
}

/// A `brigade_members.role` column value [`BrigadeRole::from_token`] does
/// not recognize, surfaced the same way any other malformed row would be
/// (`rusqlite::Error`, bubbling up through the caller's own `?` into
/// [`StoreError::Sqlite`]) rather than silently coerced — see
/// `BrigadeRole::from_token`'s own doc for why this can only mean the row
/// did not come from banto's own write path. `brigade_messages.to_role`
/// never goes through this: this crate only ever writes that column
/// (`to_role.as_token()`) or filters by it in a `WHERE` clause, never reads
/// it back and parses it into a `BrigadeRole`.
fn unrecognized_role_error(column: usize, token: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        Type::Text,
        format!("unrecognized brigade role {token:?}").into(),
    )
}

/// The largest Worker number: nine digits, what [`worker_number`] and the v16
/// backfill read back. A brigade that has issued it has run out.
pub const MAX_WORKER_NUMBER: i64 = 999_999_999;

/// The number in a Worker token: `Some(3)` for `worker-3`, `None` for anything
/// else (`worker-`, `worker-3a`, `worker-007x`, `scout`, a number of more than
/// nine digits, which banto never issues — see [`MAX_WORKER_NUMBER`]). Leading
/// zeros parse (`worker-007` is 7).
fn worker_number(token: &str) -> Option<i64> {
    let digits = token.strip_prefix("worker-")?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The two inserts that make a member: its row and its read cursor, both seeded
/// to "now" so it sees only mail enqueued after it joined. The row keeps that
/// value (`joined_after_id`) because the cursor moves on as the member reads,
/// while the glances need to know where its mail began; the two are written from
/// the same expression, in one transaction, so they start equal.
pub(super) fn insert_member(
    tx: &rusqlite::Transaction,
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    session_id: Option<&SessionId>,
    agent: Option<AgentKind>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO brigade_members (brigade_id, member_token, role, session_id, agent, joined_after_id)
         VALUES (?1, ?2, ?3, ?4, ?5, COALESCE((SELECT MAX(id) FROM brigade_messages), 0))",
        params![
            brigade_id,
            token,
            role.as_token(),
            session_id.map(|s| s.0.as_str()),
            agent.map(AgentKind::as_token),
        ],
    )?;
    tx.execute(
        "INSERT INTO brigade_cursors (brigade_id, member_token, last_seen_id)
         VALUES (?1, ?2, COALESCE((SELECT MAX(id) FROM brigade_messages), 0))",
        params![brigade_id, token],
    )?;
    Ok(())
}

/// What [`Store::name_member`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberNaming {
    /// Written: `name` is the canonical name now stored (`None` when the name
    /// was cleared) and `previous` what readers saw before.
    Set {
        name: Option<String>,
        previous: Option<String>,
    },
    /// The rule refused it; nothing was written.
    Refused(member_name::NameRejection),
    /// No member of the brigade has that token (any more); nothing was written.
    NoSuchMember,
    /// The member is not a Worker, and only a Worker can be named; nothing was
    /// written.
    NotAWorker(BrigadeRole),
}

/// The roster `SELECT`: the member columns [`brigade_member_from_row`] maps,
/// ordered by `order_by`, with the names resolved as readers see them
/// ([`readable_roster`]). Takes a bare connection so a transaction can read it
/// too.
fn read_roster(
    conn: &rusqlite::Connection,
    brigade_id: BrigadeId,
    order_by: &str,
) -> Result<Vec<BrigadeMember>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT member_token, role, session_id, agent, briefed_at_ms, briefed_session_id,
                display_name, rowid
         FROM brigade_members
         WHERE brigade_id = ?1
         ORDER BY {order_by}"
    ))?;
    let rows = stmt.query_map([brigade_id], brigade_member_with_rowid)?;
    Ok(readable_roster(rows.collect::<Result<Vec<_>, _>>()?))
}

/// Shared row mapping for the roster `SELECT` ([`read_roster`]): the six member
/// columns, then the display name.
fn brigade_member_from_row(row: &rusqlite::Row) -> rusqlite::Result<BrigadeMember> {
    let display_name: Option<String> = row.get(6)?;
    let token: String = row.get(0)?;
    let role_token: String = row.get(1)?;
    let session_id: Option<String> = row.get(2)?;
    let agent: Option<String> = row.get(3)?;
    let briefed_at_ms: Option<i64> = row.get(4)?;
    let briefed_session_id: Option<String> = row.get(5)?;
    let role = BrigadeRole::from_token(&role_token)
        .ok_or_else(|| unrecognized_role_error(1, &role_token))?;
    Ok(BrigadeMember {
        token,
        role,
        session_id: session_id.map(SessionId),
        agent: agent.as_deref().and_then(AgentKind::from_token),
        briefed_at: briefed_at_ms.map(unix_ms_to_system_time),
        briefed_session_id: briefed_session_id.map(SessionId),
        display_name,
    })
}

/// [`brigade_member_from_row`] plus the row's `rowid` (column 7), which orders a
/// roster by when its members joined.
fn brigade_member_with_rowid(row: &rusqlite::Row) -> rusqlite::Result<(i64, BrigadeMember)> {
    Ok((row.get(7)?, brigade_member_from_row(row)?))
}

/// A roster as readers may show it: each stored display name goes through the
/// rule (`banto_core::member_name`), and a name that fails it, equals a
/// token, or repeats an earlier member's reads as `None`. "Earlier" is by join
/// order (`rowid`), whatever order `members` came in, so the member that held a
/// name first keeps it and both roster queries agree on who that is.
fn readable_roster(rows: Vec<(i64, BrigadeMember)>) -> Vec<BrigadeMember> {
    let mut by_join: Vec<usize> = (0..rows.len()).collect();
    by_join.sort_by_key(|&index| rows[index].0);
    let stored: Vec<(&str, Option<&str>)> = by_join
        .iter()
        .map(|&index| {
            let member = &rows[index].1;
            (member.token.as_str(), member.display_name.as_deref())
        })
        .collect();
    let names = member_name::dedupe_roster(&stored);
    let mut resolved: Vec<Option<String>> = vec![None; rows.len()];
    for (&index, name) in by_join.iter().zip(names) {
        resolved[index] = name;
    }
    rows.into_iter()
        .zip(resolved)
        .map(|((_, mut member), name)| {
            member.display_name = name;
            member
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(s: &str) -> SessionId {
        SessionId(s.to_string())
    }

    /// Name a member the way a caller that has just read its roster does.
    fn name_it(store: &mut Store, br: BrigadeId, token: &str, name: &str) -> MemberNaming {
        match store.brigade_member(br, token).unwrap() {
            Some(_) => store.name_member(br, token, name).unwrap(),
            None => MemberNaming::NoSuchMember,
        }
    }

    // --- a Director's open request -------------------------------------------

    /// A brigade with its Director, the member a request is written as.
    fn briefed(store: &mut Store) -> BrigadeId {
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        br
    }

    fn held(store: &Store, br: BrigadeId) -> Option<String> {
        store
            .conn
            .query_row(
                "SELECT CAST(request_text AS TEXT) FROM brigades WHERE id = ?1",
                [br],
                |row| row.get(0),
            )
            .unwrap()
    }

    // --- an interrupt request ------------------------------------------------

    /// A brigade with a Director, a Worker, and the Goinkyo.
    fn with_a_worker(store: &mut Store) -> (BrigadeId, String) {
        let br = briefed(store);
        let worker = store.add_worker(br, None).unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();
        (br, worker)
    }

    #[test]
    fn an_interrupt_is_enqueued_only_director_to_worker_and_in_one_row() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        // Neither end of the pair is checked by the caller alone: the write
        // checks both, in its own transaction.
        assert_eq!(
            store
                .enqueue_interrupt_message(br, &worker, &worker, "x")
                .unwrap(),
            None,
            "a Worker is not the Director"
        );
        assert_eq!(
            store
                .enqueue_interrupt_message(br, "goinkyo", &worker, "x")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .enqueue_interrupt_message(br, "director", "director", "x")
                .unwrap(),
            None,
            "the Director is not a Worker"
        );
        assert_eq!(
            store
                .enqueue_interrupt_message(br, "director", "goinkyo", "x")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .enqueue_interrupt_message(br, "director", "worker-99", "x")
                .unwrap(),
            None
        );
        assert!(
            store.pending_interrupts(br).unwrap().is_empty(),
            "nothing written"
        );

        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "stop, change course")
            .unwrap()
            .expect("written");
        // One row carrying both the message and the request: the Worker's own
        // pull returns it as an ordinary message addressed to it.
        let pulled = store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].id, id);
        assert_eq!(pulled[0].body, "stop, change course");
        assert_eq!(pulled[0].from_token, "director");
        assert_eq!(pulled[0].to_member.as_deref(), Some(worker.as_str()));
        assert_eq!(pulled[0].interrupted, None, "no outcome had been decided");
    }

    #[test]
    fn a_pending_interrupt_says_whether_its_message_was_already_read() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        let pending = store.pending_interrupts(br).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message_id, id);
        assert_eq!(pending[0].member, worker);
        assert!(!pending[0].read);
        assert!(pending[0].created_at_ms > 0);
        assert_eq!(pending[0].line, "m");

        // The Worker pulls before the UI looked: still listed (it has to be
        // answered), and now says so.
        store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        let pending = store.pending_interrupts(br).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].read);

        // Another brigade's requests are not this one's.
        let other = store.create_brigade("other").unwrap();
        store
            .add_brigade_member(other, "director", BrigadeRole::Director, None)
            .unwrap();
        let other_worker = store.add_worker(other, None).unwrap();
        store
            .enqueue_interrupt_message(other, "director", &other_worker, "o")
            .unwrap();
        assert_eq!(store.pending_interrupts(br).unwrap().len(), 1);
        assert_eq!(store.pending_interrupts(other).unwrap().len(), 1);
    }

    #[test]
    fn a_request_the_gate_reached_first_is_still_undecided_and_every_unread_one_is_listed() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let mut ids = Vec::new();
        for outcome in [
            InterruptOutcome::Gated,
            InterruptOutcome::Capped,
            InterruptOutcome::Armed,
            InterruptOutcome::NoPane,
        ] {
            let id = store
                .enqueue_interrupt_message(br, "director", &worker, "m")
                .unwrap()
                .unwrap();
            assert!(store.record_interrupt_outcome(id, outcome).unwrap());
            ids.push(id);
        }
        let listed = store.pending_interrupts(br).unwrap();
        assert_eq!(
            listed.iter().map(|p| p.message_id).collect::<Vec<_>>(),
            ids,
            "unread, whatever the word"
        );
        assert_eq!(
            listed.iter().map(|p| p.undecided).collect::<Vec<_>>(),
            [true, true, false, false],
            "the gate's words are still the UI's to decide, the UI's are not"
        );
        assert_eq!(
            listed.iter().map(|p| p.outcome).collect::<Vec<_>>(),
            [
                Some(InterruptOutcome::Gated),
                Some(InterruptOutcome::Capped),
                Some(InterruptOutcome::Armed),
                Some(InterruptOutcome::NoPane)
            ]
        );

        // Read, they are the Worker's no longer.
        store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        assert!(store.pending_interrupts(br).unwrap().is_empty());
    }

    #[test]
    fn a_pending_interrupt_carries_the_first_line_of_its_own_message() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        store
            .enqueue_interrupt_message(
                br,
                "director",
                &worker,
                "stop the migration\r\nit drops a table",
            )
            .unwrap();
        // A later ordinary message to the same Worker is not what is quoted.
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Worker,
                Some(&worker),
                "carry on",
            )
            .unwrap();
        store
            .enqueue_interrupt_message(br, "director", &worker, "")
            .unwrap();
        let lines: Vec<String> = store
            .pending_interrupts(br)
            .unwrap()
            .into_iter()
            .map(|p| p.line)
            .collect();
        assert_eq!(lines, ["stop the migration", ""]);
    }

    #[test]
    fn an_ordinary_message_is_never_a_pending_interrupt() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some(&worker), "hi")
            .unwrap();
        assert!(store.pending_interrupts(br).unwrap().is_empty());
        assert!(store.unread_interrupts(br).unwrap().is_empty());
    }

    #[test]
    fn the_uis_decision_is_written_once_and_the_gates_words_follow_the_table() {
        use InterruptOutcome::*;
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.interrupt_state(id).unwrap(),
            InterruptState::Undecided
        );

        assert!(store.record_interrupt_outcome(id, Armed).unwrap());
        assert_eq!(
            store.interrupt_state(id).unwrap(),
            InterruptState::Decided(Armed)
        );
        // Taken: nothing left for the UI to decide, but it is listed while unread,
        // for the notice that it sits unread.
        let listed = store.pending_interrupts(br).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].undecided);
        assert_eq!(listed[0].outcome, Some(Armed));
        // A second first-decision does not overwrite the first, whatever it says.
        for again in [Read, Codex, RelayManual, NoPane, Armed] {
            assert!(
                !store.record_interrupt_outcome(id, again).unwrap(),
                "{again:?}"
            );
        }
        // The gate writes over `armed`, and `capped` over `gated`.
        assert!(store.record_interrupt_outcome(id, Gated).unwrap());
        assert!(!store.record_interrupt_outcome(id, Gated).unwrap());
        assert!(!store.record_interrupt_outcome(id, Armed).unwrap());
        assert!(store.record_interrupt_outcome(id, Capped).unwrap());
        // Nothing goes over `capped`.
        for later in [Read, Codex, RelayManual, NoPane, Armed, Gated, Capped] {
            assert!(
                !store.record_interrupt_outcome(id, later).unwrap(),
                "{later:?}"
            );
        }
        assert_eq!(
            store.interrupt_state(id).unwrap(),
            InterruptState::Decided(Capped)
        );

        // A row that is not an interrupt cannot be given an outcome at all.
        let plain = store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some(&worker), "p")
            .unwrap();
        assert!(!store.record_interrupt_outcome(plain, Armed).unwrap());
        assert_eq!(store.interrupt_state(plain).unwrap(), InterruptState::Gone);
    }

    #[test]
    fn the_gate_may_write_before_the_uis_decision_has_landed_and_the_late_decision_is_a_no_op() {
        use InterruptOutcome::*;
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        assert!(store.record_interrupt_outcome(id, Gated).unwrap());
        assert!(!store.record_interrupt_outcome(id, Armed).unwrap());
        assert_eq!(
            store.interrupt_state(id).unwrap(),
            InterruptState::Decided(Gated)
        );

        // `capped` also goes over no word at all, and over `armed`.
        for before in [None, Some(Armed)] {
            let id = store
                .enqueue_interrupt_message(br, "director", &worker, "m")
                .unwrap()
                .unwrap();
            if let Some(before) = before {
                assert!(store.record_interrupt_outcome(id, before).unwrap());
            }
            assert!(
                store.record_interrupt_outcome(id, Capped).unwrap(),
                "{before:?}"
            );
        }
    }

    #[test]
    fn a_word_a_ui_wrote_is_never_replaced_by_the_gate() {
        use InterruptOutcome::*;
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        for decided in [Read, Codex, RelayManual, NoPane] {
            let id = store
                .enqueue_interrupt_message(br, "director", &worker, "m")
                .unwrap()
                .unwrap();
            assert!(store.record_interrupt_outcome(id, decided).unwrap());
            for gate in [Gated, Capped] {
                assert!(
                    !store.record_interrupt_outcome(id, gate).unwrap(),
                    "{gate:?} over {decided:?}"
                );
            }
            assert_eq!(
                store.interrupt_state(id).unwrap(),
                InterruptState::Decided(decided)
            );
        }
    }

    #[test]
    fn the_inbox_flag_is_true_exactly_for_a_row_whose_call_the_gate_refused() {
        use InterruptOutcome::*;
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let mut ids = Vec::new();
        for outcome in [Armed, Gated, Capped, Codex, RelayManual, NoPane, Read] {
            let id = store
                .enqueue_interrupt_message(br, "director", &worker, "m")
                .unwrap()
                .unwrap();
            assert!(store.record_interrupt_outcome(id, outcome).unwrap());
            ids.push((id, outcome));
        }
        // A row nothing was decided for.
        let undecided = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        let pulled = store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        let flags: Vec<(i64, Option<InterruptOutcome>)> =
            pulled.iter().map(|m| (m.id, m.interrupted)).collect();
        let mut expected: Vec<(i64, Option<InterruptOutcome>)> = ids
            .iter()
            .map(|(id, outcome)| (*id, outcome.gate_refused().then_some(*outcome)))
            .collect();
        expected.push((undecided, None));
        assert_eq!(flags, expected);
        assert_eq!(
            flags.iter().filter(|(_, flag)| flag.is_some()).count(),
            2,
            "gated and capped"
        );
    }

    #[test]
    fn an_outcome_this_build_does_not_know_reads_as_undecided_and_is_not_an_interrupt_header() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        // `pressed` is what a banto that typed an Esc wrote.
        store
            .conn
            .execute(
                "UPDATE brigade_messages SET interrupt_outcome = 'pressed' WHERE id = ?1",
                [id],
            )
            .unwrap();
        assert_eq!(
            store.interrupt_state(id).unwrap(),
            InterruptState::Undecided
        );
        let pulled = store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        assert_eq!(pulled[0].interrupted, None);
    }

    #[test]
    fn unread_interrupts_show_their_outcome_and_age_until_the_worker_reads_them() {
        use InterruptOutcome::*;
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let first = store
            .enqueue_interrupt_message(br, "director", &worker, "a")
            .unwrap()
            .unwrap();
        let second = store
            .enqueue_interrupt_message(br, "director", &worker, "b")
            .unwrap()
            .unwrap();
        store.record_interrupt_outcome(first, Armed).unwrap();
        let listed = |store: &Store| -> Vec<(MemberToken, Option<InterruptOutcome>)> {
            store
                .unread_interrupts(br)
                .unwrap()
                .into_iter()
                .map(|unread| (unread.member, unread.outcome))
                .collect()
        };
        assert_eq!(
            listed(&store),
            [(worker.clone(), Some(Armed)), (worker.clone(), None)]
        );
        store.record_interrupt_outcome(first, Gated).unwrap();
        store.record_interrupt_outcome(second, Codex).unwrap();
        assert_eq!(
            listed(&store),
            [(worker.clone(), Some(Gated)), (worker.clone(), Some(Codex))]
        );
        let written: Vec<i64> = store
            .unread_interrupts(br)
            .unwrap()
            .into_iter()
            .map(|unread| unread.created_at_ms)
            .collect();
        assert!(written.iter().all(|ms| *ms > 0), "{written:?}");
        // The witness of delivery is the read cursor: once read, nothing is said.
        store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        assert!(store.unread_interrupts(br).unwrap().is_empty());
    }

    #[test]
    fn the_unread_interrupt_ids_are_one_members_and_end_when_it_reads_them() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let other = store.add_worker(br, None).unwrap();
        let first = store
            .enqueue_interrupt_message(br, "director", &worker, "a")
            .unwrap()
            .unwrap();
        // An ordinary message, another Worker's interrupt: neither is this one's.
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some(&worker), "p")
            .unwrap();
        store
            .enqueue_interrupt_message(br, "director", &other, "o")
            .unwrap();
        let second = store
            .enqueue_interrupt_message(br, "director", &worker, "b")
            .unwrap()
            .unwrap();
        assert_eq!(
            store.unread_interrupt_ids(br, &worker).unwrap(),
            [first, second]
        );
        store
            .fetch_brigade_messages(br, &worker, BrigadeRole::Worker)
            .unwrap();
        assert!(store.unread_interrupt_ids(br, &worker).unwrap().is_empty());
        assert_eq!(store.unread_interrupt_ids(br, &other).unwrap().len(), 1);
    }

    #[test]
    fn dismissing_the_worker_takes_its_interrupt_with_it() {
        let mut store = Store::open_in_memory().unwrap();
        let (br, worker) = with_a_worker(&mut store);
        let id = store
            .enqueue_interrupt_message(br, "director", &worker, "m")
            .unwrap()
            .unwrap();
        store.dismiss_worker(br, &worker).unwrap();
        assert_eq!(store.interrupt_state(id).unwrap(), InterruptState::Gone);
        assert!(store.pending_interrupts(br).unwrap().is_empty());
        assert!(
            !store
                .record_interrupt_outcome(id, InterruptOutcome::Armed)
                .unwrap()
        );
    }

    #[test]
    fn the_schema_carries_the_interrupt_columns_and_the_partial_index() {
        let store = Store::open_in_memory().unwrap();
        let columns: Vec<String> = store
            .conn
            .prepare("SELECT name FROM pragma_table_info('brigade_messages')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"interrupt".to_string()), "{columns:?}");
        assert!(
            columns.contains(&"interrupt_outcome".to_string()),
            "{columns:?}"
        );
        let index: Option<String> = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'brigade_messages_open_interrupts'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(index.is_some_and(|sql| sql.contains("WHERE")));
    }

    #[test]
    fn the_pending_interrupts_query_is_answered_from_the_partial_index() {
        // Every second, per brigade, on a table that only grows: a scan of it is
        // what v22 exists to prevent.
        let store = Store::open_in_memory().unwrap();
        let sql = pending_interrupts_sql();
        let plan: Vec<String> = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([1], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let plan = plan.join(
            "
",
        );
        assert!(
            plan.contains("USING INDEX brigade_messages_interrupts")
                || plan.contains("USING COVERING INDEX brigade_messages_interrupts"),
            "{plan}"
        );
        assert!(!plan.contains("SCAN m"), "{plan}");
    }

    #[test]
    fn a_request_is_the_fact_that_the_director_waits_and_is_stored_as_a_constant() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        assert_eq!(store.brigade_request(br).unwrap(), None);
        assert_eq!(held(&store, br), None);
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(1));
        assert_eq!(store.brigade_request(br).unwrap(), Some(1));
        // Whatever a question says, the column the tick reads holds the one value.
        assert_eq!(held(&store, br).as_deref(), Some(REQUEST_OPEN));
        // A second call is a new request: the sequence is what says so.
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(2));
        assert_eq!(store.brigade_request(br).unwrap(), Some(2));
        assert_eq!(held(&store, br).as_deref(), Some(REQUEST_OPEN));
        let at: Option<i64> = store
            .conn
            .query_row(
                "SELECT request_at_ms FROM brigades WHERE id = ?1",
                [br],
                |row| row.get(0),
            )
            .unwrap();
        assert!(at.is_some_and(|ms| ms > 0), "the time is recorded: {at:?}");
        // No brigade, no request.
        assert_eq!(store.set_request(br + 100, "director", None).unwrap(), None);
    }

    #[test]
    fn a_question_lives_and_dies_with_its_request_and_never_leaks_into_the_next() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        assert_eq!(store.brigade_request_detail(br).unwrap(), None);
        let asked = "Ship it?\n- yes\n- no";
        assert_eq!(
            store.set_request(br, "director", Some(asked)).unwrap(),
            Some(1)
        );
        let detail = store.brigade_request_detail(br).unwrap().unwrap();
        assert_eq!(detail.seq, 1);
        assert_eq!(detail.question.as_deref(), Some(asked));
        assert!(detail.requested_at_ms.is_some_and(|ms| ms > 0));
        // The tick's column is the constant whatever the question says.
        assert_eq!(held(&store, br).as_deref(), Some(REQUEST_OPEN));
        // A second request with no question does not show the first one's.
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(2));
        assert_eq!(
            store.brigade_request_detail(br).unwrap().unwrap().question,
            None
        );
        // An answer clears it with the request; a stale answer does not.
        store.set_request(br, "director", Some("again")).unwrap();
        assert!(!store.clear_request(br, 2).unwrap());
        assert_eq!(
            store
                .brigade_request_detail(br)
                .unwrap()
                .unwrap()
                .question
                .as_deref(),
            Some("again")
        );
        assert!(store.clear_request(br, 3).unwrap());
        assert_eq!(store.brigade_request_detail(br).unwrap(), None);
        let left: Option<String> = store
            .conn
            .query_row(
                "SELECT request_question FROM brigades WHERE id = ?1",
                [br],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, None);
    }

    #[test]
    fn a_request_is_written_only_by_the_brigades_current_director() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        let worker = store.add_worker(br, None).unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();
        // The tool checked the role a moment ago; the write checks it again, in its
        // own transaction, so a membership that has changed since is not written as.
        assert_eq!(store.set_request(br, &worker, None).unwrap(), None);
        assert_eq!(store.set_request(br, "goinkyo", None).unwrap(), None);
        assert_eq!(store.brigade_request(br).unwrap(), None);
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(1));
        // The Director leaves the brigade between the check and the write.
        store
            .conn
            .execute(
                "DELETE FROM brigade_members WHERE brigade_id = ?1 AND member_token = 'director'",
                [br],
            )
            .unwrap();
        assert_eq!(store.set_request(br, "director", None).unwrap(), None);
        assert_eq!(
            store.brigade_request(br).unwrap(),
            Some(1),
            "nothing was written"
        );
    }

    #[test]
    fn a_request_is_cleared_only_by_the_sequence_that_was_answered() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        store.set_request(br, "director", None).unwrap();
        store.set_request(br, "director", None).unwrap();
        // The answer was to the first: a newer request is never wiped by it.
        assert!(!store.clear_request(br, 1).unwrap());
        assert_eq!(store.brigade_request(br).unwrap(), Some(2));
        assert!(store.clear_request(br, 2).unwrap());
        assert_eq!(store.brigade_request(br).unwrap(), None);
        // Clearing twice is harmless, and the next request still has a new number.
        assert!(!store.clear_request(br, 2).unwrap());
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(3));
    }

    #[test]
    fn a_disbanded_brigade_takes_its_request_with_it() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        store.set_request(br, "director", None).unwrap();
        store.delete_brigade(br).unwrap();
        assert_eq!(store.brigade_request(br).unwrap(), None);
        // A new brigade has its own, from the start.
        let other = briefed(&mut store);
        assert_eq!(store.set_request(other, "director", None).unwrap(), Some(1));
    }

    #[test]
    fn a_hand_edited_request_still_reads_and_counts_on() {
        let mut store = Store::open_in_memory().unwrap();
        let br = briefed(&mut store);
        // A sequence that is a real number: open, whatever the text holds.
        store
            .conn
            .execute(
                "UPDATE brigades SET request_text = 42, request_seq = 7.5 WHERE id = ?1",
                [br],
            )
            .unwrap();
        assert_eq!(store.brigade_request(br).unwrap(), Some(7));
        assert_eq!(store.set_request(br, "director", None).unwrap(), Some(8));
        // Bytes that are not valid text in the column: the value is never read, so
        // it cannot fail the read (which would drop the brigade from every tick).
        store
            .conn
            .execute(
                "UPDATE brigades SET request_text = X'46FF4F4F', request_seq = 4 WHERE id = ?1",
                [br],
            )
            .unwrap();
        assert_eq!(store.brigade_request(br).unwrap(), Some(4));
        assert!(store.brigade_members(br).is_ok());
        // NULL is no request, whatever the sequence says.
        store
            .conn
            .execute(
                "UPDATE brigades SET request_text = NULL WHERE id = ?1",
                [br],
            )
            .unwrap();
        assert_eq!(store.brigade_request(br).unwrap(), None);
    }

    #[test]
    fn every_member_row_is_listed_whatever_its_role_says() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("d")))
            .unwrap();
        let worker = store.add_worker(br, None).unwrap();
        // A row another banto (or a hand edit) wrote with a role this one does not
        // know: the roster readers cannot make sense of it, the sweep's read must
        // still count it.
        store
            .conn
            .execute(
                "INSERT INTO brigade_members (brigade_id, member_token, role) VALUES (?1, ?2, ?3)",
                params![br, "worker-77", "a role from the future"],
            )
            .unwrap();
        assert!(store.brigade_members(br).is_err());
        let mut listed = store.all_member_tokens().unwrap();
        listed.sort();
        assert_eq!(
            listed,
            [
                (br, "director".to_string()),
                (br, worker),
                (br, "worker-77".to_string())
            ]
        );
        // Once a member is gone, so is its pair.
        assert!(store.dismiss_worker(br, "worker-77").unwrap());
        assert_eq!(store.all_member_tokens().unwrap().len(), 2);
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
                    session_id: Some(sid("dir")),
                    agent: None,
                    briefed_at: None,
                    briefed_session_id: None,
                    display_name: None,
                },
                BrigadeMember {
                    token: "worker-1".to_string(),
                    role: BrigadeRole::Worker,
                    session_id: Some(sid("w1")),
                    agent: None,
                    briefed_at: None,
                    briefed_session_id: None,
                    display_name: None,
                },
                BrigadeMember {
                    token: "worker-2".to_string(),
                    role: BrigadeRole::Worker,
                    session_id: Some(sid("w2")),
                    agent: None,
                    briefed_at: None,
                    briefed_session_id: None,
                    display_name: None,
                },
            ]
        );
    }

    #[test]
    fn worker_can_be_added_with_no_session_yet() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.session_id, None);

        store
            .set_member_session(br, "worker-1", &sid("w1"))
            .unwrap();
        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.session_id, Some(sid("w1")));
    }

    #[test]
    fn member_agent_round_trips_and_unknown_token_is_absent() {
        let mut store = Store::open_in_memory().unwrap();
        let brigade = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member_with_agent(
                brigade,
                "worker-1",
                BrigadeRole::Worker,
                None,
                Some(AgentKind::ClaudeCode),
            )
            .unwrap();
        assert_eq!(
            store
                .brigade_member(brigade, "worker-1")
                .unwrap()
                .unwrap()
                .agent,
            Some(AgentKind::ClaudeCode)
        );

        store
            .conn
            .execute(
                "UPDATE brigade_members SET agent = 'future-agent' WHERE brigade_id = ?1",
                [brigade],
            )
            .unwrap();
        assert_eq!(
            store
                .brigade_member(brigade, "worker-1")
                .unwrap()
                .unwrap()
                .agent,
            None
        );
    }

    #[test]
    fn clear_member_session_resets_it_back_to_none() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();

        store.clear_member_session(br, "goinkyo").unwrap();

        let member = store.brigade_member(br, "goinkyo").unwrap().unwrap();
        assert_eq!(member.session_id, None);
    }

    #[test]
    fn clear_member_session_of_a_non_member_is_a_noop() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        store.clear_member_session(br, "goinkyo").unwrap();
    }

    #[test]
    fn one_session_belongs_to_one_member_the_older_row_is_cleared() {
        // Two Workers spawned into one cwd once resolved to a single
        // discovered id, leaving both rows claiming it — after which
        // `brigade_of_session` answered with whichever sorted first
        // and an `_mcp` connection could speak as the wrong member. The
        // second assignment now clears the first row back to "awaiting
        // discovery" instead, which also repairs data a past duplicate left.
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, None)
            .unwrap();

        store
            .set_member_session(br, "worker-1", &sid("w1"))
            .unwrap();
        store
            .set_member_session(br, "worker-2", &sid("w1"))
            .unwrap();

        let first = store.brigade_member(br, "worker-1").unwrap().unwrap();
        let second = store.brigade_member(br, "worker-2").unwrap().unwrap();
        assert_eq!(first.session_id, None);
        assert_eq!(second.session_id, Some(sid("w1")));
        assert_eq!(
            store.brigade_of_session(&sid("w1")).unwrap(),
            Some((br, "worker-2".to_string(), BrigadeRole::Worker))
        );
    }

    #[test]
    fn re_recording_the_same_member_with_the_same_id_keeps_it() {
        // The clear-others sweep must not clear the row it is about to set.
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        store
            .set_member_session(br, "worker-1", &sid("w1"))
            .unwrap();
        store
            .set_member_session(br, "worker-1", &sid("w1"))
            .unwrap();

        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.session_id, Some(sid("w1")));
    }

    #[test]
    fn record_briefing_sets_both_columns_and_is_a_noop_for_an_unknown_token() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();

        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.briefed_at, None);
        assert_eq!(member.briefed_session_id, None);

        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1000);
        store
            .record_briefing(br, "worker-1", &sid("w1"), at)
            .unwrap();

        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.briefed_at, Some(at));
        assert_eq!(member.briefed_session_id, Some(sid("w1")));

        // A token this brigade doesn't have: not an error, nothing to record.
        store.record_briefing(br, "nobody", &sid("w1"), at).unwrap();
    }

    #[test]
    fn brigade_of_session_reports_membership_token_and_role() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        assert_eq!(store.brigade_of_session(&sid("dir")).unwrap(), None);

        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        assert_eq!(
            store.brigade_of_session(&sid("dir")).unwrap(),
            Some((br, "director".to_string(), BrigadeRole::Director))
        );
    }

    #[test]
    fn brigade_of_session_is_none_for_a_worker_awaiting_discovery() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();

        assert_eq!(store.brigade_of_session(&sid("w1")).unwrap(), None);
    }

    #[test]
    fn hidden_and_director_session_id_sets_are_reported_separately() {
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
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();

        let mut hidden = store.brigade_hidden_session_ids().unwrap();
        hidden.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(hidden, [sid("g1"), sid("w1")]);
        assert_eq!(store.brigade_director_session_ids().unwrap(), [sid("dir")]);
    }

    #[test]
    fn a_dismissed_goinkyo_drops_out_of_the_hidden_set() {
        // The Director is never hidden (that pane is the operator's own
        // session), and a dismissed Goinkyo's row is gone entirely, so it
        // simply stops matching this query. What keeps its session out of the
        // ordinary list from then on is the archive, not this set
        // (`dismissing_a_goinkyo_archives_its_session`).
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();
        assert_eq!(store.brigade_hidden_session_ids().unwrap(), [sid("g1")]);

        store.dismiss_worker(br, "goinkyo").unwrap();

        assert_eq!(store.brigade_hidden_session_ids().unwrap(), []);
    }

    /// The two ways a Goinkyo's row is deleted by banto, so every archive test
    /// below runs against both: a dismissal, and the disbanding of its brigade.
    fn end_goinkyo(store: &mut Store, brigade: BrigadeId, how: EndedBy) {
        match how {
            EndedBy::Dismissal => {
                store.dismiss_worker(brigade, "goinkyo").unwrap();
            }
            EndedBy::Disband => {
                store.delete_brigade(brigade).unwrap();
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum EndedBy {
        Dismissal,
        Disband,
    }

    const BOTH: [EndedBy; 2] = [EndedBy::Dismissal, EndedBy::Disband];

    fn archive_time(store: &Store, id: &str) -> Option<i64> {
        use rusqlite::OptionalExtension;
        store
            .conn
            .query_row(
                "SELECT archived_at_ms FROM archived WHERE session_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn ending_a_goinkyo_archives_the_session_its_row_held() {
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
                .unwrap();
            store
                .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
                .unwrap();

            end_goinkyo(&mut store, br, how);

            assert_eq!(store.archived_ids().unwrap(), [sid("g1")], "{how:?}");
        }
    }

    #[test]
    fn a_goinkyo_that_never_got_a_session_id_archives_nothing() {
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
                .unwrap();
            store
                .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
                .unwrap();

            end_goinkyo(&mut store, br, how);

            assert!(store.archived_ids().unwrap().is_empty(), "{how:?}");
        }
    }

    #[test]
    fn ending_a_brigade_member_other_than_the_goinkyo_archives_nothing() {
        // A Worker's token is the one `dismiss_worker` is given on the exited
        // path too, so the role has to come from the row. The Director is no
        // Goinkyo either: disbanding the brigade deletes its row, and its
        // session is the operator's own.
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
                .unwrap();
            store
                .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
                .unwrap();
            match how {
                EndedBy::Dismissal => {
                    store.dismiss_worker(br, "worker-1").unwrap();
                }
                EndedBy::Disband => {
                    store.delete_brigade(br).unwrap();
                }
            }

            assert!(store.archived_ids().unwrap().is_empty(), "{how:?}");
        }
    }

    #[test]
    fn dismissing_a_worker_leaves_the_goinkyo_and_its_session_alone() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();

        store.dismiss_worker(br, "worker-1").unwrap();

        assert!(store.archived_ids().unwrap().is_empty());
        assert_eq!(store.brigade_hidden_session_ids().unwrap(), [sid("g1")]);
    }

    #[test]
    fn disbanding_one_brigade_archives_only_its_own_goinkyo() {
        let mut store = Store::open_in_memory().unwrap();
        let one = store.create_brigade("one").unwrap();
        let other = store.create_brigade("other").unwrap();
        store
            .add_brigade_member(one, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();
        store
            .add_brigade_member(other, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g2")))
            .unwrap();

        store.delete_brigade(one).unwrap();

        assert_eq!(store.archived_ids().unwrap(), [sid("g1")]);
    }

    #[test]
    fn ending_a_goinkyo_keeps_the_time_its_session_was_first_archived() {
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO archived (session_id, archived_at_ms) VALUES ('g1', 1234)",
                    [],
                )
                .unwrap();

            end_goinkyo(&mut store, br, how);

            assert_eq!(archive_time(&store, "g1"), Some(1234), "{how:?}");
        }
    }

    #[test]
    fn an_archived_goinkyo_session_is_restored_by_unarchiving_it() {
        // `D` is the way back, and nothing re-archives it behind the operator's
        // back: the archive happens at the dismissal, once.
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
            .unwrap();
        store.dismiss_worker(br, "goinkyo").unwrap();

        store.unarchive_session(&sid("g1")).unwrap();

        assert!(store.archived_ids().unwrap().is_empty());
    }

    /// A brigade with its Director and a Goinkyo filed with `title`, whose session
    /// id is `session` once discovery has bound one (`None`: not yet).
    fn consulting(title: &str, session: Option<&str>) -> (Store, BrigadeId) {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store.add_goinkyo_member(br, title).unwrap();
        if let Some(session) = session {
            store
                .set_member_session(br, "goinkyo", &sid(session))
                .unwrap();
        }
        (store, br)
    }

    #[test]
    fn ending_a_goinkyo_records_the_title_its_row_held_against_its_session() {
        for how in BOTH {
            let (mut store, br) = consulting("Layout drag", Some("g1"));
            assert!(store.consultation_titles().unwrap().is_empty());

            end_goinkyo(&mut store, br, how);

            assert_eq!(
                store.consultation_titles().unwrap(),
                [(sid("g1"), "Layout drag".to_string())],
                "{how:?}"
            );
            assert_eq!(store.archived_ids().unwrap(), [sid("g1")], "{how:?}");
        }
    }

    #[test]
    fn a_goinkyo_with_no_session_id_leaves_no_title_either() {
        for how in BOTH {
            let (mut store, br) = consulting("Layout drag", None);

            end_goinkyo(&mut store, br, how);

            assert!(store.consultation_titles().unwrap().is_empty(), "{how:?}");
        }
    }

    #[test]
    fn a_goinkyo_row_with_no_title_archives_its_session_and_records_nothing() {
        // The shape of a consultation filed before v20.
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("g1")))
                .unwrap();

            end_goinkyo(&mut store, br, how);

            assert_eq!(store.archived_ids().unwrap(), [sid("g1")], "{how:?}");
            assert!(store.consultation_titles().unwrap().is_empty(), "{how:?}");
        }
    }

    #[test]
    fn ending_a_worker_records_no_title() {
        // Not by the token it is dismissed by, and not for a Worker whose row
        // somehow carries a title: the role is read from the row.
        for how in BOTH {
            let mut store = Store::open_in_memory().unwrap();
            let br = store.create_brigade("cell").unwrap();
            store
                .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
                .unwrap();
            store
                .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET consult_title = 'stray' WHERE member_token = 'worker-1'",
                    [],
                )
                .unwrap();
            match how {
                EndedBy::Dismissal => {
                    store.dismiss_worker(br, "worker-1").unwrap();
                }
                EndedBy::Disband => {
                    store.delete_brigade(br).unwrap();
                }
            }

            assert!(store.consultation_titles().unwrap().is_empty(), "{how:?}");
        }
    }

    #[test]
    fn disbanding_one_brigade_records_only_its_own_goinkyos_title() {
        let (mut store, one) = consulting("First", Some("g1"));
        let other = store.create_brigade("other").unwrap();
        store.add_goinkyo_member(other, "Second").unwrap();
        store
            .set_member_session(other, "goinkyo", &sid("g2"))
            .unwrap();

        store.delete_brigade(one).unwrap();

        assert_eq!(
            store.consultation_titles().unwrap(),
            [(sid("g1"), "First".to_string())]
        );
    }

    #[test]
    fn the_title_survives_restoring_the_session_and_the_brigade_going_away() {
        // `D` deletes the archived row. The title is the one thing that must
        // outlive that: restoring the session is when it is wanted.
        let (mut store, br) = consulting("Layout drag", Some("g1"));
        store.dismiss_worker(br, "goinkyo").unwrap();

        store.unarchive_session(&sid("g1")).unwrap();
        store.delete_brigade(br).unwrap();

        assert!(store.archived_ids().unwrap().is_empty());
        assert_eq!(
            store.consultation_titles().unwrap(),
            [(sid("g1"), "Layout drag".to_string())]
        );
    }

    #[test]
    fn a_title_carries_to_the_session_the_row_holds_after_a_move() {
        let (mut store, br) = consulting("Layout drag", Some("old"));
        store
            .set_member_session(br, "goinkyo", &sid("new"))
            .unwrap();

        store.dismiss_worker(br, "goinkyo").unwrap();

        assert_eq!(
            store.consultation_titles().unwrap(),
            [(sid("new"), "Layout drag".to_string())],
            "only the id the row holds at the end is named, as with the archive"
        );
    }

    #[test]
    fn the_next_consultation_starts_without_the_last_ones_title() {
        let (mut store, br) = consulting("First", Some("g1"));
        store.dismiss_worker(br, "goinkyo").unwrap();

        store.add_goinkyo_member(br, "Second").unwrap();
        store.set_member_session(br, "goinkyo", &sid("g2")).unwrap();
        store.dismiss_worker(br, "goinkyo").unwrap();

        assert_eq!(
            store.consultation_titles().unwrap(),
            [
                (sid("g1"), "First".to_string()),
                (sid("g2"), "Second".to_string())
            ]
        );
    }

    #[test]
    fn a_session_ended_twice_keeps_the_first_title() {
        let (mut store, br) = consulting("First", Some("g1"));
        store.dismiss_worker(br, "goinkyo").unwrap();
        store.add_goinkyo_member(br, "Second").unwrap();
        store.set_member_session(br, "goinkyo", &sid("g1")).unwrap();

        store.dismiss_worker(br, "goinkyo").unwrap();

        assert_eq!(
            store.consultation_titles().unwrap(),
            [(sid("g1"), "First".to_string())]
        );
    }

    #[test]
    fn a_consultation_is_told_from_the_operators_own_archive_by_its_record() {
        // The archived table cannot say who archived a session; this one can.
        let (mut store, br) = consulting("Layout drag", Some("g1"));
        store.archive_session(&sid("mine")).unwrap();
        store.dismiss_worker(br, "goinkyo").unwrap();

        let ended: Vec<SessionId> = store
            .consultation_titles()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(ended.contains(&sid("g1")));
        assert!(!ended.contains(&sid("mine")));
        assert_eq!(store.archived_ids().unwrap().len(), 2);
    }

    #[test]
    fn a_goinkyos_earlier_session_after_a_move_is_not_archived() {
        // `update_member_session_moved` replaces the id the row holds; the
        // dismissal archives only the id it holds then.
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, Some(&sid("old")))
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE brigade_members SET session_id = 'new' WHERE member_token = 'goinkyo'",
                [],
            )
            .unwrap();

        store.dismiss_worker(br, "goinkyo").unwrap();

        assert_eq!(store.archived_ids().unwrap(), [sid("new")]);
    }

    #[test]
    fn director_message_reaches_the_worker_then_the_cursor_advances() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();

        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Worker,
                None,
                "please run the tests",
            )
            .unwrap();

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
                session_id: Some(sid("dir")),
                agent: None,
                briefed_at: None,
                briefed_session_id: None,
                display_name: None,
            }]
        );
        // See remove_brigade_member's doc: a non-member is a no-op.
        store.remove_brigade_member(br, "nobody").unwrap();

        store.delete_brigade(br).unwrap();
        assert!(store.brigade_members(br).unwrap().is_empty());
        assert_eq!(store.brigade_of_session(&sid("dir")).unwrap(), None);
    }

    fn join_order(store: &Store, br: BrigadeId) -> Vec<String> {
        store
            .brigade_members_in_join_order(br)
            .unwrap()
            .into_iter()
            .map(|m| m.token)
            .collect()
    }

    #[test]
    fn members_come_back_in_the_order_they_joined_the_director_first() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        // Another brigade's rows interleave with this one's.
        let other = store.create_brigade("other").unwrap();
        for (brigade, token, role) in [
            (br, "director", BrigadeRole::Director),
            (other, "director", BrigadeRole::Director),
            (br, "worker-1", BrigadeRole::Worker),
            (br, "worker-2", BrigadeRole::Worker),
            (other, "worker-1", BrigadeRole::Worker),
            // A Goinkyo, then a Worker added after it, and a double-digit one.
            (br, "goinkyo", BrigadeRole::Goinkyo),
            (br, "worker-3", BrigadeRole::Worker),
            (br, "worker-10", BrigadeRole::Worker),
        ] {
            store
                .add_brigade_member(brigade, token, role, None)
                .unwrap();
        }

        assert_eq!(
            join_order(&store, br),
            [
                "director",
                "worker-1",
                "worker-2",
                "goinkyo",
                "worker-3",
                "worker-10"
            ]
        );
        assert_eq!(join_order(&store, other), ["director", "worker-1"]);
        // The token order other readers get is a different thing.
        let by_token: Vec<String> = store
            .brigade_members(br)
            .unwrap()
            .into_iter()
            .map(|m| m.token)
            .collect();
        assert_eq!(
            by_token,
            [
                "director",
                "goinkyo",
                "worker-1",
                "worker-10",
                "worker-2",
                "worker-3"
            ]
        );
    }

    #[test]
    fn the_director_leads_even_when_its_row_was_added_last() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        assert_eq!(join_order(&store, br), ["director", "worker-1"]);
    }

    #[test]
    fn updating_a_member_never_moves_it_and_a_token_reused_by_hand_joins_last() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        for (token, role) in [
            ("director", BrigadeRole::Director),
            ("worker-1", BrigadeRole::Worker),
            ("worker-2", BrigadeRole::Worker),
            ("goinkyo", BrigadeRole::Goinkyo),
        ] {
            store.add_brigade_member(br, token, role, None).unwrap();
        }
        let before = join_order(&store, br);

        // Session ids set, healed to another id, cleared, briefing recorded.
        store.set_member_session(br, "worker-1", &sid("a")).unwrap();
        store.set_member_session(br, "worker-1", &sid("b")).unwrap();
        store.set_member_session(br, "goinkyo", &sid("g")).unwrap();
        store.clear_member_session(br, "goinkyo").unwrap();
        assert_eq!(join_order(&store, br), before);

        // Dismissed and numbered again: a new row, so the last.
        store.dismiss_worker(br, "worker-1").unwrap();
        assert_eq!(join_order(&store, br), ["director", "worker-2", "goinkyo"]);
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(
            join_order(&store, br),
            ["director", "worker-2", "goinkyo", "worker-1"]
        );
        // The same after the last-joined row was the one removed and re-added
        // (SQLite hands the freed rowid back; it is still the largest).
        store.dismiss_worker(br, "worker-1").unwrap();
        store
            .add_brigade_member(br, "worker-3", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(
            join_order(&store, br),
            ["director", "worker-2", "goinkyo", "worker-3"]
        );
    }

    #[test]
    fn the_join_order_survives_closing_and_reopening_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let expected = ["director", "worker-2", "worker-1", "goinkyo", "worker-3"];
        {
            let mut store = Store::open(&path).unwrap();
            let br = store.create_brigade("cell").unwrap();
            for token in expected {
                let role = match token {
                    "director" => BrigadeRole::Director,
                    "goinkyo" => BrigadeRole::Goinkyo,
                    _ => BrigadeRole::Worker,
                };
                store.add_brigade_member(br, token, role, None).unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(join_order(&store, 1), expected);
    }

    fn named_brigade() -> (Store, BrigadeId) {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        for (token, role) in [
            ("director", BrigadeRole::Director),
            ("worker-1", BrigadeRole::Worker),
            ("worker-2", BrigadeRole::Worker),
            ("goinkyo", BrigadeRole::Goinkyo),
        ] {
            store.add_brigade_member(br, token, role, None).unwrap();
        }
        (store, br)
    }

    fn names(store: &Store, br: BrigadeId) -> Vec<(String, Option<String>)> {
        store
            .brigade_members(br)
            .unwrap()
            .into_iter()
            .map(|m| (m.token, m.display_name))
            .collect()
    }

    #[test]
    fn a_display_name_is_set_cleared_and_a_missing_member_is_a_no_op() {
        let (mut store, br) = named_brigade();
        assert!(names(&store, br).iter().all(|(_, name)| name.is_none()));

        assert!(matches!(
            name_it(&mut store, br, "worker-1", "impl"),
            MemberNaming::Set { .. }
        ));
        assert_eq!(
            names(&store, br),
            [
                ("director".to_string(), None),
                ("goinkyo".to_string(), None),
                ("worker-1".to_string(), Some("impl".to_string())),
                ("worker-2".to_string(), None),
            ]
        );
        // Both roster queries and the single read carry it.
        let join: Vec<_> = store
            .brigade_members_in_join_order(br)
            .unwrap()
            .into_iter()
            .filter_map(|m| m.display_name)
            .collect();
        assert_eq!(join, ["impl"]);
        assert_eq!(
            store
                .brigade_member(br, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            Some("impl".to_string())
        );

        // Changed, then cleared.
        name_it(&mut store, br, "worker-1", "review");
        assert_eq!(
            store
                .brigade_member(br, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            Some("review".to_string())
        );
        assert!(matches!(
            name_it(&mut store, br, "worker-1", ""),
            MemberNaming::Set { .. }
        ));
        assert!(names(&store, br).iter().all(|(_, name)| name.is_none()));

        // A member that is not there, in a brigade that is not there: nothing
        // happens, and nothing is created.
        assert_eq!(
            name_it(&mut store, br, "worker-9", "ghost"),
            MemberNaming::NoSuchMember
        );
        assert_eq!(
            name_it(&mut store, br + 1, "worker-1", "ghost"),
            MemberNaming::NoSuchMember
        );
        assert_eq!(names(&store, br).len(), 4);
    }

    #[test]
    fn a_name_belongs_to_its_brigade_and_does_not_move_the_member() {
        let (mut store, br) = named_brigade();
        let other = store.create_brigade("other").unwrap();
        store
            .add_brigade_member(other, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        let before = join_order(&store, br);

        name_it(&mut store, br, "worker-2", "impl");
        // The same token in another brigade is another member; and the same
        // name may be used there.
        assert_eq!(
            store
                .brigade_member(other, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            None
        );
        name_it(&mut store, other, "worker-1", "impl");
        assert_eq!(join_order(&store, br), before, "naming never reorders");
    }

    #[test]
    fn dismissal_takes_the_name_and_a_member_under_the_same_token_by_hand_starts_unnamed() {
        let (mut store, br) = named_brigade();
        name_it(&mut store, br, "worker-2", "impl");
        store.dismiss_worker(br, "worker-2").unwrap();
        assert!(store.brigade_member(br, "worker-2").unwrap().is_none());

        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(
            store
                .brigade_member(br, "worker-2")
                .unwrap()
                .unwrap()
                .display_name,
            None
        );
        // And the name is free to give again.
        assert!(matches!(
            name_it(&mut store, br, "worker-1", "impl"),
            MemberNaming::Set { .. }
        ));
    }

    #[test]
    fn stored_values_that_fail_the_rule_read_as_no_name() {
        let (store, br) = named_brigade();
        // Written behind the tool's back: old or hand-edited data.
        let write = |token: &str, value: &str| {
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET display_name = ?3
                     WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token, value],
                )
                .unwrap();
        };
        for garbage in [
            "",
            "  padded  ",
            "esc\u{1b}[31m",
            "bidi\u{202e}x",
            "quote\"d",
            "worker-3",
            "Goinkyo",
            &"w".repeat(25),
            "two\nlines",
        ] {
            write("worker-1", garbage);
            assert_eq!(
                store
                    .brigade_member(br, "worker-1")
                    .unwrap()
                    .unwrap()
                    .display_name,
                None,
                "{garbage:?}"
            );
            assert!(
                names(&store, br).iter().all(|(_, name)| name.is_none()),
                "{garbage:?}"
            );
        }
        // A good value still reads.
        write("worker-1", "impl");
        assert_eq!(names(&store, br)[2].1.as_deref(), Some("impl"));
    }

    #[test]
    fn a_name_two_members_hold_is_read_only_for_the_one_that_joined_first() {
        let (store, br) = named_brigade();
        for token in ["worker-1", "worker-2"] {
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET display_name = 'Impl'
                     WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token],
                )
                .unwrap();
        }
        // A name equal to a token is no name either.
        store
            .conn
            .execute(
                "UPDATE brigade_members SET display_name = 'worker-2'
                 WHERE member_token = 'goinkyo'",
                [],
            )
            .unwrap();
        // worker-1 joined before worker-2 (and the two queries agree on that,
        // though they list the roster in different orders).
        for roster in [
            store.brigade_members(br).unwrap(),
            store.brigade_members_in_join_order(br).unwrap(),
        ] {
            let named: Vec<(&str, Option<&str>)> = roster
                .iter()
                .map(|m| (m.token.as_str(), m.display_name.as_deref()))
                .collect();
            assert!(named.contains(&("worker-1", Some("Impl"))), "{named:?}");
            assert!(named.contains(&("worker-2", None)), "{named:?}");
            assert!(named.contains(&("goinkyo", None)), "{named:?}");
        }
    }

    #[test]
    fn naming_applies_the_rule_in_the_store_and_reports_what_it_did() {
        let (mut store, br) = named_brigade();
        assert_eq!(
            store.name_member(br, "worker-1", " impl ").unwrap(),
            MemberNaming::Set {
                name: Some("impl".to_string()),
                previous: None
            }
        );
        // Taken, token-like, quote, bracket, too wide: refused, nothing written.
        for (input, rejection) in [
            (
                "IMPL",
                member_name::NameRejection::Taken {
                    by: "worker-1".to_string(),
                },
            ),
            ("worker-9", member_name::NameRejection::LooksLikeToken),
            ("a\"b", member_name::NameRejection::HasQuote),
            ("a]b", member_name::NameRejection::HasBracket),
            (
                &"x".repeat(25),
                member_name::NameRejection::TooWide { columns: 25 },
            ),
        ] {
            assert_eq!(
                store.name_member(br, "worker-2", input).unwrap(),
                MemberNaming::Refused(rejection),
                "{input:?}"
            );
        }
        assert_eq!(
            store
                .brigade_member(br, "worker-2")
                .unwrap()
                .unwrap()
                .display_name,
            None
        );
        // Its own name again, then cleared with the previous one reported.
        assert_eq!(
            store.name_member(br, "worker-1", "impl").unwrap(),
            MemberNaming::Set {
                name: Some("impl".to_string()),
                previous: Some("impl".to_string())
            }
        );
        assert_eq!(
            store.name_member(br, "worker-1", "").unwrap(),
            MemberNaming::Set {
                name: None,
                previous: Some("impl".to_string())
            }
        );
    }

    #[test]
    fn a_missing_token_and_a_member_that_is_not_a_worker_are_distinct_outcomes() {
        let (mut store, br) = named_brigade();
        assert_eq!(
            store.name_member(br, "worker-9", "impl").unwrap(),
            MemberNaming::NoSuchMember
        );
        // A token of another brigade is no member of this one.
        assert_eq!(
            store.name_member(br + 1, "worker-1", "impl").unwrap(),
            MemberNaming::NoSuchMember
        );
        assert_eq!(
            store.name_member(br, "director", "boss").unwrap(),
            MemberNaming::NotAWorker(BrigadeRole::Director)
        );
        assert_eq!(
            store.name_member(br, "goinkyo", "elder").unwrap(),
            MemberNaming::NotAWorker(BrigadeRole::Goinkyo)
        );
        // Nothing was written, not even a clear.
        assert!(names(&store, br).iter().all(|(_, name)| name.is_none()));
    }

    /// Two connections to one file, as the emporium and a Director's MCP server are.
    fn naming_connections() -> (tempfile::TempDir, std::path::PathBuf, Store, BrigadeId) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let mut store = Store::open(&path).unwrap();
        let br = store.create_brigade("cell").unwrap();
        for (token, role) in [
            ("director", BrigadeRole::Director),
            ("worker-1", BrigadeRole::Worker),
            ("worker-2", BrigadeRole::Worker),
        ] {
            store.add_brigade_member(br, token, role, None).unwrap();
        }
        (dir, path, store, br)
    }

    #[test]
    fn another_connection_cannot_dismiss_and_re_add_while_the_naming_transaction_holds_the_lock() {
        let (_dir, path, mut director_side, br) = naming_connections();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut rival: Option<std::thread::JoinHandle<()>> = None;

        let outcome = director_side
            .name_member_with(br, "worker-2", "reviewer", || {
                // The roster has been read, nothing decided or written yet: the
                // emporium tries to dismiss worker-2 and add a Worker under the
                // same token.
                rival = Some(std::thread::spawn(move || {
                    let mut emporium = Store::open(&path).unwrap();
                    emporium.dismiss_worker(br, "worker-2").unwrap();
                    emporium
                        .add_brigade_member(br, "worker-2", BrigadeRole::Worker, None)
                        .unwrap();
                    done_tx.send(()).unwrap();
                }));
                // It is held at the write lock, not racing past it.
                assert!(
                    done_rx
                        .recv_timeout(std::time::Duration::from_millis(300))
                        .is_err(),
                    "the other connection got in while the naming transaction was open"
                );
            })
            .unwrap();
        // The name landed on the member that was there when the transaction began.
        assert_eq!(
            outcome,
            MemberNaming::Set {
                name: Some("reviewer".to_string()),
                previous: None
            }
        );
        // Only now does the dismissal run: it takes the name away with the row, and
        // the newcomer under the token starts unnamed.
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the dismissal ran after the commit");
        rival.unwrap().join().unwrap();
        assert_eq!(
            director_side
                .brigade_member(br, "worker-2")
                .unwrap()
                .unwrap()
                .display_name,
            None,
            "the newcomer is unnamed"
        );
        assert!(
            names(&director_side, br)
                .iter()
                .all(|(_, name)| name.is_none())
        );
    }

    #[test]
    fn two_connections_naming_to_the_same_name_at_once_exactly_one_wins() {
        for _ in 0..8 {
            let (_dir, path, store, br) = naming_connections();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = ["worker-1", "worker-2"]
                .into_iter()
                .map(|token| {
                    let path = path.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        let mut mine = Store::open(&path).unwrap();
                        barrier.wait();
                        mine.name_member(br, token, "impl").unwrap()
                    })
                })
                .collect();
            let outcomes: Vec<MemberNaming> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            let wins = outcomes
                .iter()
                .filter(|o| matches!(o, MemberNaming::Set { .. }))
                .count();
            let taken = outcomes
                .iter()
                .filter(|o| {
                    matches!(
                        o,
                        MemberNaming::Refused(member_name::NameRejection::Taken { .. })
                    )
                })
                .count();
            assert_eq!((wins, taken), (1, 1), "{outcomes:?}");
            let named = store
                .brigade_members(br)
                .unwrap()
                .into_iter()
                .filter(|m| m.display_name.is_some())
                .count();
            assert_eq!(named, 1);
        }
    }

    #[test]
    fn the_single_member_read_agrees_with_the_roster_on_a_name_two_members_hold() {
        let (store, br) = named_brigade();
        for token in ["worker-1", "worker-2"] {
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET display_name = 'Impl'
                     WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token],
                )
                .unwrap();
        }
        for member in store.brigade_members(br).unwrap() {
            assert_eq!(
                store.brigade_member(br, &member.token).unwrap().as_ref(),
                Some(&member),
                "{}",
                member.token
            );
        }
        assert_eq!(
            store
                .brigade_member(br, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            Some("Impl".to_string())
        );
        assert_eq!(
            store
                .brigade_member(br, "worker-2")
                .unwrap()
                .unwrap()
                .display_name,
            None,
            "the later joiner's duplicate is not shown by any read"
        );
    }

    #[test]
    fn a_stored_name_with_a_bracket_reads_as_no_name() {
        let (store, br) = named_brigade();
        store
            .conn
            .execute(
                "UPDATE brigade_members SET display_name = 'a] — to you'
                 WHERE member_token = 'worker-1'",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .brigade_member(br, "worker-1")
                .unwrap()
                .unwrap()
                .display_name,
            None
        );
    }

    // --- Worker numbering: a Worker token is never reused within a brigade ----

    fn tokens_of(store: &Store, br: BrigadeId) -> Vec<String> {
        let mut tokens: Vec<String> = store
            .brigade_members(br)
            .unwrap()
            .into_iter()
            .filter(|m| m.role == BrigadeRole::Worker)
            .map(|m| m.token)
            .collect();
        tokens.sort_by_key(|t| worker_number(t));
        tokens
    }

    fn worker_seq(store: &Store, br: BrigadeId) -> i64 {
        store
            .conn
            .query_row("SELECT worker_seq FROM brigades WHERE id = ?1", [br], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn brigade_with_director() -> (Store, BrigadeId) {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        (store, br)
    }

    #[test]
    fn workers_are_numbered_one_after_another_and_formation_leaves_the_counter_at_n() {
        let (mut store, br) = brigade_with_director();
        for n in 1..=3 {
            assert_eq!(
                store.add_worker(br, Some(AgentKind::ClaudeCode)).unwrap(),
                format!("worker-{n}")
            );
        }
        assert_eq!(worker_seq(&store, br), 3);
        // Then one more, as the emporium's add-worker does.
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-4");
        assert_eq!(worker_seq(&store, br), 4);
        // The row is a Worker with its agent, no session yet, and a read cursor.
        let member = store.brigade_member(br, "worker-1").unwrap().unwrap();
        assert_eq!(member.role, BrigadeRole::Worker);
        assert_eq!(member.agent, Some(AgentKind::ClaudeCode));
        assert_eq!(member.session_id, None);
        assert!(
            !store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
    }

    #[test]
    fn dismissing_the_highest_worker_does_not_hand_its_number_to_the_next() {
        let (mut store, br) = brigade_with_director();
        for _ in 0..3 {
            store.add_worker(br, None).unwrap();
        }
        store.dismiss_worker(br, "worker-3").unwrap();
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-4");
        assert_eq!(tokens_of(&store, br), ["worker-1", "worker-2", "worker-4"]);
    }

    #[test]
    fn dismissing_every_worker_does_not_restart_the_numbering() {
        let (mut store, br) = brigade_with_director();
        for _ in 0..2 {
            store.add_worker(br, None).unwrap();
        }
        store.dismiss_worker(br, "worker-1").unwrap();
        store.dismiss_worker(br, "worker-2").unwrap();
        assert!(tokens_of(&store, br).is_empty());
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-3");
    }

    #[test]
    fn dismissing_a_middle_worker_leaves_a_gap_and_numbering_goes_on_past_the_highest() {
        let (mut store, br) = brigade_with_director();
        for _ in 0..3 {
            store.add_worker(br, None).unwrap();
        }
        store.dismiss_worker(br, "worker-2").unwrap();
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-4");
        assert_eq!(tokens_of(&store, br), ["worker-1", "worker-3", "worker-4"]);
    }

    #[test]
    fn a_counter_behind_the_members_still_issues_above_the_highest_member() {
        // Hand-edited, or a brigade older than the counter that the backfill missed.
        let (mut store, br) = brigade_with_director();
        for token in ["worker-2", "worker-5"] {
            store
                .add_brigade_member(br, token, BrigadeRole::Worker, None)
                .unwrap();
        }
        assert_eq!(worker_seq(&store, br), 0);
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-6");
        // A counter ahead of the members wins, too (the dismissed ones' numbers).
        store
            .conn
            .execute("UPDATE brigades SET worker_seq = 10 WHERE id = ?1", [br])
            .unwrap();
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-11");
        assert_eq!(worker_seq(&store, br), 11);
    }

    #[test]
    fn only_exact_worker_numbers_count_toward_the_next_number() {
        let (mut store, br) = brigade_with_director();
        for token in [
            "worker-x",
            "worker-3a",
            "scout",
            "worker-",
            "Worker-9",
            "worker-4",
        ] {
            store
                .add_brigade_member(br, token, BrigadeRole::Worker, None)
                .unwrap();
        }
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-5");
        for (token, n) in [
            ("worker-1", Some(1)),
            ("worker-007", Some(7)),
            ("worker-123456789", Some(123_456_789)),
            ("worker-1234567890", None),
            ("worker-", None),
            ("worker-3a", None),
            ("worker--3", None),
            ("goinkyo", None),
            ("director", None),
            ("Worker-3", None),
        ] {
            assert_eq!(worker_number(token), n, "{token}");
        }
    }

    fn set_counter(store: &Store, br: BrigadeId, value: &str) {
        store
            .conn
            .execute(
                &format!("UPDATE brigades SET worker_seq = {value} WHERE id = ?1"),
                [br],
            )
            .unwrap();
    }

    #[test]
    fn numbers_are_issued_up_to_the_largest_the_parser_reads_and_no_further() {
        assert_eq!(MAX_WORKER_NUMBER, 999_999_999);
        assert_eq!(
            worker_number(&format!("worker-{MAX_WORKER_NUMBER}")),
            Some(MAX_WORKER_NUMBER)
        );
        assert_eq!(
            worker_number(&format!("worker-{}", MAX_WORKER_NUMBER + 1)),
            None
        );

        let (mut store, br) = brigade_with_director();
        // One below the last: the last is issued, and recognised afterwards.
        set_counter(&store, br, "999999997");
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-999999998");
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-999999999");
        assert_eq!(worker_seq(&store, br), MAX_WORKER_NUMBER);
        // Past it: refused, with the reason, and nothing changes.
        let members = store.brigade_members(br).unwrap().len();
        let err = store.add_worker(br, None).unwrap_err();
        assert!(matches!(err, StoreError::WorkerNumbersExhausted), "{err:?}");
        assert_eq!(
            err.to_string(),
            "this brigade has run out of Worker numbers"
        );
        assert_eq!(store.brigade_members(br).unwrap().len(), members);
        assert_eq!(worker_seq(&store, br), MAX_WORKER_NUMBER);
        // Dismissing the last does not make it available again.
        store
            .dismiss_worker(br, &format!("worker-{MAX_WORKER_NUMBER}"))
            .unwrap();
        assert!(store.add_worker(br, None).is_err());
    }

    #[test]
    fn a_member_at_the_bound_exhausts_a_counter_that_is_behind_it() {
        let (mut store, br) = brigade_with_director();
        store
            .add_brigade_member(br, "worker-999999999", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(worker_seq(&store, br), 0);
        assert!(matches!(
            store.add_worker(br, None),
            Err(StoreError::WorkerNumbersExhausted)
        ));
        // A longer number is not one banto issued: it does not count.
        let (mut store, br) = brigade_with_director();
        store
            .add_brigade_member(br, "worker-1000000000", BrigadeRole::Worker, None)
            .unwrap();
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-1");
    }

    #[test]
    fn an_absurd_stored_counter_neither_overflows_nor_goes_backwards() {
        // Above the range: read as the bound, so the brigade is exhausted.
        for value in ["9223372036854775807", "1000000000", "999999999", "1e30"] {
            let (mut store, br) = brigade_with_director();
            set_counter(&store, br, value);
            assert!(
                matches!(
                    store.add_worker(br, None),
                    Err(StoreError::WorkerNumbersExhausted)
                ),
                "{value}"
            );
            assert!(tokens_of(&store, br).is_empty(), "{value}: nothing created");
        }
        // Below the range: read as 0, and the members decide.
        for value in ["-1", "-9223372036854775808", "'not a number'", "'-5'"] {
            let (mut store, br) = brigade_with_director();
            store
                .add_brigade_member(br, "worker-4", BrigadeRole::Worker, None)
                .unwrap();
            set_counter(&store, br, value);
            assert_eq!(store.add_worker(br, None).unwrap(), "worker-5", "{value}");
            assert_eq!(worker_seq(&store, br), 5, "{value}");
        }
        // One step short of the bound works too.
        let (mut store, br) = brigade_with_director();
        set_counter(&store, br, "999999998");
        assert_eq!(store.add_worker(br, None).unwrap(), "worker-999999999");
    }

    #[test]
    fn a_disbanded_brigades_numbering_does_not_carry_over() {
        let (mut store, br) = brigade_with_director();
        for _ in 0..3 {
            store.add_worker(br, None).unwrap();
        }
        store.delete_brigade(br).unwrap();
        let next = store.create_brigade("next").unwrap();
        assert_ne!(next, br, "a new brigade has a new id");
        store
            .add_brigade_member(next, "director", BrigadeRole::Director, None)
            .unwrap();
        assert_eq!(store.add_worker(next, None).unwrap(), "worker-1");
        assert_eq!(worker_seq(&store, next), 1);
    }

    #[test]
    fn each_brigade_counts_for_itself_and_a_missing_brigade_is_an_error() {
        let (mut store, one) = brigade_with_director();
        let two = store.create_brigade("two").unwrap();
        store.add_worker(one, None).unwrap();
        store.add_worker(one, None).unwrap();
        assert_eq!(store.add_worker(two, None).unwrap(), "worker-1");
        assert_eq!(store.add_worker(one, None).unwrap(), "worker-3");
        assert!(store.add_worker(two + 1, None).is_err());
        // And nothing was created for it.
        assert!(store.brigade_members(two + 1).unwrap().is_empty());
    }

    #[test]
    fn two_connections_adding_workers_at_once_get_different_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let mut first = Store::open(&path).unwrap();
        let br = first.create_brigade("cell").unwrap();
        first
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut mine = Store::open(&path).unwrap();
                    barrier.wait();
                    (0..10)
                        .map(|_| mine.add_worker(br, None).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<String> = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect();
        all.sort_by_key(|token| worker_number(token));
        let expected: Vec<String> = (1..=20).map(|n| format!("worker-{n}")).collect();
        assert_eq!(all, expected, "twenty adds, twenty distinct numbers");
        assert_eq!(worker_seq(&first, br), 20);
    }

    #[test]
    fn dismissing_says_whether_a_member_was_there_to_remove() {
        let (mut store, br) = brigade_with_director();
        let token = store.add_worker(br, None).unwrap();
        assert!(store.dismiss_worker(br, &token).unwrap());
        // Already gone: a no-op, and it says so.
        assert!(!store.dismiss_worker(br, &token).unwrap());
        assert!(!store.dismiss_worker(br, "worker-9").unwrap());
        assert!(!store.dismiss_worker(br + 1, "worker-1").unwrap());
        // Nothing else was touched.
        assert_eq!(store.brigade_members(br).unwrap().len(), 1);
    }

    #[test]
    fn dismiss_worker_removes_membership_cursor_and_its_addressed_mail() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("w1")))
            .unwrap();
        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, Some(&sid("w2")))
            .unwrap();

        // Addressed to the dismissed worker: must be purged.
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some("worker-1"), "hi")
            .unwrap();
        // Addressed to the surviving worker: must NOT be touched.
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some("worker-2"), "hey")
            .unwrap();
        // A broadcast: belongs to the role, must survive too.
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "all")
            .unwrap();

        store.dismiss_worker(br, "worker-1").unwrap();

        // Membership and cursor are gone; the director and worker-2 remain.
        let tokens: Vec<String> = store
            .brigade_members(br)
            .unwrap()
            .into_iter()
            .map(|m| m.token)
            .collect();
        assert_eq!(tokens, ["director", "worker-2"]);

        // worker-2 still sees its own addressed message and the broadcast —
        // neither was collateral damage from worker-1's dismissal.
        let worker2_messages = store
            .fetch_brigade_messages(br, "worker-2", BrigadeRole::Worker)
            .unwrap();
        let bodies: Vec<String> = worker2_messages.into_iter().map(|m| m.body).collect();
        assert_eq!(bodies, ["hey".to_string(), "all".to_string()]);
    }

    #[test]
    fn dismiss_worker_of_a_non_member_is_a_noop() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();

        store.dismiss_worker(br, "worker-1").unwrap();

        assert_eq!(store.brigade_members(br).unwrap().len(), 1);
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

        // See delete_empty_brigades's doc: a populated brigade is untouched.
        assert_eq!(store.brigade_members(populated).unwrap().len(), 1);
    }

    // --- message glances -------------------------------------------------------

    fn line_of(glance: Option<MessageGlance>) -> Option<String> {
        glance.map(|g| g.line)
    }

    #[test]
    fn the_inbound_glance_is_the_newest_message_the_member_could_receive() {
        let store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("a").unwrap();
        let b = store.create_brigade("b").unwrap();
        let send = |store: &Store, br, from: &str, role, to: Option<&str>, body: &str| {
            store
                .enqueue_brigade_message(br, from, role, to, body)
                .unwrap()
        };
        send(
            &store,
            a,
            "director",
            BrigadeRole::Worker,
            None,
            "broadcast one",
        );
        send(
            &store,
            a,
            "director",
            BrigadeRole::Worker,
            Some("worker-2"),
            "only for two",
        );
        send(
            &store,
            a,
            "director",
            BrigadeRole::Worker,
            Some("worker-1"),
            "only for one",
        );
        // Another brigade's mail, and mail for the other role, are not theirs.
        send(
            &store,
            b,
            "director",
            BrigadeRole::Worker,
            None,
            "other brigade",
        );
        send(
            &store,
            a,
            "worker-1",
            BrigadeRole::Director,
            None,
            "to the director",
        );

        // worker-1 sees what was addressed to it, newest first among what it can see.
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(a, "worker-1", BrigadeRole::Worker)
                    .unwrap()
            ),
            Some("only for one".to_string())
        );
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(a, "worker-2", BrigadeRole::Worker)
                    .unwrap()
            ),
            Some("only for two".to_string())
        );
        // A worker nobody wrote to by name sees the broadcast.
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(a, "worker-3", BrigadeRole::Worker)
                    .unwrap()
            ),
            Some("broadcast one".to_string())
        );
        // The Director's inbound is the worker's message, not the workers' mail.
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(a, "director", BrigadeRole::Director)
                    .unwrap()
            ),
            Some("to the director".to_string())
        );
        // A newer broadcast supersedes older addressed mail.
        send(
            &store,
            a,
            "director",
            BrigadeRole::Worker,
            None,
            "later broadcast",
        );
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(a, "worker-1", BrigadeRole::Worker)
                    .unwrap()
            ),
            Some("later broadcast".to_string())
        );
        // Nothing for a role with no mail in that brigade.
        assert_eq!(
            store
                .latest_inbound_glance(b, "goinkyo", BrigadeRole::Goinkyo)
                .unwrap(),
            None
        );
    }

    #[test]
    fn the_inbound_glance_names_its_sender_and_the_sent_glance_the_member_itself() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "from the boss")
            .unwrap();
        store
            .enqueue_brigade_message(
                br,
                "worker-2",
                BrigadeRole::Worker,
                Some("worker-1"),
                "psst",
            )
            .unwrap();
        store
            .enqueue_brigade_message(br, "worker-1", BrigadeRole::Director, None, "done")
            .unwrap();

        let to_one = store
            .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
            .unwrap()
            .unwrap();
        assert_eq!(
            (to_one.line.as_str(), to_one.from_token.as_deref()),
            ("psst", Some("worker-2"))
        );
        let to_two = store
            .latest_inbound_glance(br, "worker-2", BrigadeRole::Worker)
            .unwrap()
            .unwrap();
        assert_eq!(
            to_two.from_token.as_deref(),
            Some("director"),
            "the broadcast, from the director"
        );
        let to_director = store
            .latest_inbound_glance(br, "director", BrigadeRole::Director)
            .unwrap()
            .unwrap();
        assert_eq!(to_director.from_token.as_deref(), Some("worker-1"));
        let sent = store.latest_sent_glance(br, "worker-1").unwrap().unwrap();
        assert_eq!(sent.from_token.as_deref(), Some("worker-1"));
    }

    #[test]
    fn whenever_a_member_has_unseen_mail_its_inbound_glance_is_the_newest_unread_message() {
        // The nudge quotes the glance; the trigger is `has_unseen`. They use
        // the same addressing, and the glance is unbounded by the cursor, so
        // the newest matching message is unread whenever anything is.
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        let workers = ["worker-1", "worker-2", "worker-3"];
        let mut n = 0;
        // A deterministic mix of broadcasts, addressed sends to each worker,
        // mail to the Director, and fetches in between.
        for step in 0..60 {
            n += 1;
            let body = format!("message {n}");
            match step % 7 {
                0 | 3 => store
                    .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, &body)
                    .unwrap(),
                1 | 4 => store
                    .enqueue_brigade_message(
                        br,
                        "director",
                        BrigadeRole::Worker,
                        Some(workers[step % 3]),
                        &body,
                    )
                    .unwrap(),
                2 => store
                    .enqueue_brigade_message(
                        br,
                        workers[step % 3],
                        BrigadeRole::Director,
                        None,
                        &body,
                    )
                    .unwrap(),
                _ => {
                    store
                        .fetch_brigade_messages(br, workers[step % 3], BrigadeRole::Worker)
                        .unwrap();
                    0
                }
            };
            for token in workers {
                let unseen = store
                    .has_unseen_brigade_messages(br, token, BrigadeRole::Worker)
                    .unwrap();
                let glance = store
                    .latest_inbound_glance(br, token, BrigadeRole::Worker)
                    .unwrap();
                if unseen {
                    // What a fetch would hand over, newest last — without
                    // consuming it: read it on a scratch copy of the cursor.
                    let newest_unread = unread_newest(&mut store, br, token);
                    assert_eq!(
                        glance.map(|g| g.line),
                        Some(newest_unread),
                        "step {step}, {token}: the glance must be the newest unread message"
                    );
                }
            }
        }
    }

    /// The newest message `token` has not read, found by fetching and then
    /// restoring the cursor.
    fn unread_newest(store: &mut Store, br: BrigadeId, token: &str) -> String {
        let before: Option<i64> = store
            .conn
            .query_row(
                "SELECT last_seen_id FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
                params![br, token],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        let fetched = store
            .fetch_brigade_messages(br, token, BrigadeRole::Worker)
            .unwrap();
        match before {
            Some(id) => store
                .conn
                .execute(
                    "UPDATE brigade_cursors SET last_seen_id = ?3 WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token, id],
                )
                .unwrap(),
            None => store
                .conn
                .execute(
                    "DELETE FROM brigade_cursors WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token],
                )
                .unwrap(),
        };
        glance_line(&fetched.last().expect("something was unseen").body)
    }

    #[test]
    fn the_sent_glance_is_the_newest_message_that_member_sent_in_that_brigade() {
        let store = Store::open_in_memory().unwrap();
        let a = store.create_brigade("a").unwrap();
        let b = store.create_brigade("b").unwrap();
        store
            .enqueue_brigade_message(a, "worker-1", BrigadeRole::Director, None, "first")
            .unwrap();
        store
            .enqueue_brigade_message(a, "worker-2", BrigadeRole::Director, None, "from two")
            .unwrap();
        store
            .enqueue_brigade_message(a, "worker-1", BrigadeRole::Director, None, "second")
            .unwrap();
        store
            .enqueue_brigade_message(b, "worker-1", BrigadeRole::Director, None, "other brigade")
            .unwrap();
        store
            .enqueue_brigade_message(a, "director", BrigadeRole::Worker, None, "to them")
            .unwrap();

        assert_eq!(
            line_of(store.latest_sent_glance(a, "worker-1").unwrap()),
            Some("second".to_string())
        );
        assert_eq!(
            line_of(store.latest_sent_glance(a, "worker-2").unwrap()),
            Some("from two".to_string())
        );
        assert_eq!(
            line_of(store.latest_sent_glance(b, "worker-1").unwrap()),
            Some("other brigade".to_string())
        );
        assert_eq!(store.latest_sent_glance(a, "worker-3").unwrap(), None);
        assert_eq!(
            line_of(store.latest_sent_glance(a, "director").unwrap()),
            Some("to them".to_string())
        );
    }

    #[test]
    fn a_new_members_inbound_glance_skips_mail_queued_before_it_joined() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, None)
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "before you")
            .unwrap();

        let token = store.add_worker(br, None).unwrap();

        assert_eq!(
            store
                .latest_inbound_glance(br, &token, BrigadeRole::Worker)
                .unwrap(),
            None,
            "a broadcast queued before the Worker existed is not mail it ever could receive"
        );
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "for you")
            .unwrap();
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(br, &token, BrigadeRole::Worker)
                    .unwrap()
            ),
            Some("for you".to_string())
        );
    }

    #[test]
    fn a_replaced_goinkyo_does_not_show_the_previous_ones_sent_line() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();
        store
            .enqueue_brigade_message(br, "goinkyo", BrigadeRole::Director, None, "my verdict")
            .unwrap();
        assert_eq!(
            line_of(store.latest_sent_glance(br, "goinkyo").unwrap()),
            Some("my verdict".to_string()),
            "the first Goinkyo's own line shows for it"
        );

        assert!(store.dismiss_worker(br, "goinkyo").unwrap());
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();

        assert_eq!(
            store.latest_sent_glance(br, "goinkyo").unwrap(),
            None,
            "the token is reused, the member is not: it has sent nothing yet"
        );
        store
            .enqueue_brigade_message(br, "goinkyo", BrigadeRole::Director, None, "second opinion")
            .unwrap();
        assert_eq!(
            line_of(store.latest_sent_glance(br, "goinkyo").unwrap()),
            Some("second opinion".to_string())
        );
    }

    #[test]
    fn a_replaced_goinkyos_inbound_glance_skips_mail_for_the_one_before_it() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Goinkyo, None, "first question")
            .unwrap();
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(br, "goinkyo", BrigadeRole::Goinkyo)
                    .unwrap()
            ),
            Some("first question".to_string())
        );

        store.dismiss_worker(br, "goinkyo").unwrap();
        store
            .add_brigade_member(br, "goinkyo", BrigadeRole::Goinkyo, None)
            .unwrap();

        assert_eq!(
            store
                .latest_inbound_glance(br, "goinkyo", BrigadeRole::Goinkyo)
                .unwrap(),
            None,
            "role-wide mail for the previous consultation is not this one's"
        );
        store
            .enqueue_brigade_message(
                br,
                "director",
                BrigadeRole::Goinkyo,
                None,
                "second question",
            )
            .unwrap();
        assert_eq!(
            line_of(
                store
                    .latest_inbound_glance(br, "goinkyo", BrigadeRole::Goinkyo)
                    .unwrap()
            ),
            Some("second question".to_string())
        );
    }

    #[test]
    fn a_glance_carries_when_the_message_was_queued() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        let before = system_time_to_unix_ms(SystemTime::now());
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();
        let after = system_time_to_unix_ms(SystemTime::now());

        let glance = store
            .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
            .unwrap()
            .unwrap();

        assert!((before..=after).contains(&glance.created_at_ms));
    }

    #[test]
    fn reading_a_glance_never_advances_the_cursor() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "one")
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some("worker-1"), "two")
            .unwrap();

        for _ in 0..3 {
            store
                .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
                .unwrap();
            store.latest_sent_glance(br, "director").unwrap();
        }

        assert!(
            store
                .has_unseen_brigade_messages(br, "worker-1", BrigadeRole::Worker)
                .unwrap()
        );
        let fetched = store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(fetched.len(), 2, "everything is still there to be pulled");
        let cursors: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_cursors WHERE brigade_id = ?1",
                [br],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursors, 1, "only the real fetch wrote a cursor");
    }

    #[test]
    fn the_max_message_id_moves_only_when_a_message_is_appended() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        assert_eq!(store.max_brigade_message_id().unwrap(), 0);
        let first = store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "x")
            .unwrap();
        assert_eq!(store.max_brigade_message_id().unwrap(), first);
        store
            .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        assert_eq!(store.max_brigade_message_id().unwrap(), first);
        let second = store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "y")
            .unwrap();
        assert!(second > first);
        assert_eq!(store.max_brigade_message_id().unwrap(), second);
    }

    #[test]
    fn a_glance_line_is_the_first_non_empty_line_trimmed() {
        assert_eq!(glance_line("hello"), "hello");
        assert_eq!(glance_line("first\nsecond\nthird"), "first");
        assert_eq!(glance_line("\n\n   \n  padded line  \nnext"), "padded line");
        assert_eq!(glance_line(""), "");
        assert_eq!(glance_line("\n \t \n"), "");
        // Windows line endings: the carriage return is a control character.
        assert_eq!(glance_line("one\r\ntwo"), "one");
        assert_eq!(glance_line("\r\n\r\nafter blanks"), "after blanks");
    }

    #[test]
    fn a_glance_line_has_no_control_characters_so_nothing_can_steer_the_terminal() {
        assert_eq!(glance_line("a\u{1b}[31mred\u{1b}[0m"), "a[31mred[0m");
        assert_eq!(glance_line("bell\u{7}here"), "bellhere");
        assert_eq!(glance_line("nul\0byte"), "nulbyte");
        // A tab reads as a space, not as a word gluing together.
        assert_eq!(glance_line("col1\tcol2"), "col1 col2");
        // A line of nothing but controls is skipped like a blank one.
        assert_eq!(glance_line("\u{1b}\u{7}\nreal"), "real");
        assert!(
            glance_line("x\u{85}y\u{9f}z")
                .chars()
                .all(|c| !c.is_control())
        );
    }

    #[test]
    fn a_glance_line_is_cut_to_two_hundred_characters_not_bytes_or_cells() {
        let long = "a".repeat(500);
        assert_eq!(glance_line(&long).chars().count(), 200);
        // Wide characters count as one character each here (the drawing
        // clips by display width on top), and the cut never splits one.
        let wide = "\u{30c6}".repeat(300);
        let cut = glance_line(&wide);
        assert_eq!(cut.chars().count(), 200);
        assert!(cut.chars().all(|c| c == '\u{30c6}'));
        // Exactly 200 stays whole.
        assert_eq!(glance_line(&"b".repeat(200)).len(), 200);
    }

    #[test]
    fn a_glance_line_has_no_format_characters_so_nothing_can_reorder_or_pad_the_row() {
        // A right-to-left override would draw the rest of the row backwards.
        assert_eq!(glance_line("abc\u{202e}def"), "abcdef");
        // An isolate pair, and the embeddings/overrides around it.
        assert_eq!(glance_line("\u{2066}secret\u{2069} rest"), "secret rest");
        assert_eq!(
            glance_line("\u{202a}a\u{202b}b\u{202c}c\u{202d}d\u{202e}e"),
            "abcde"
        );
        // A zero-width joiner (and space, non-joiner, word joiner) inside a word.
        assert_eq!(
            glance_line("pa\u{200d}ss\u{200b}wo\u{200c}rd\u{2060}!"),
            "password!"
        );
        // A byte-order mark at the start.
        assert_eq!(glance_line("\u{feff}hello"), "hello");
        // Soft hyphen, a tag character, and a line of nothing but format
        // characters is skipped like a blank one.
        assert_eq!(glance_line("co\u{ad}op"), "coop");
        assert_eq!(glance_line("x\u{e0041}y"), "xy");
        assert_eq!(
            glance_line("\u{200b}\u{feff}\u{2066}\nreal line"),
            "real line"
        );
        // Ordinary text, including other scripts and emoji joined by a ZWJ's
        // neighbours, is untouched apart from the joiner itself.
        assert_eq!(
            glance_line("\u{30c6}\u{30b9}\u{30c8} ok"),
            "\u{30c6}\u{30b9}\u{30c8} ok"
        );
    }

    #[test]
    fn the_glance_finds_the_first_line_after_any_amount_of_leading_whitespace() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        // Far more blank lines and spaces than the read bound, in every mix
        // the trim covers, then the real first line and a long tail.
        let lead = "\n \r\n\t\u{b}\u{c}".repeat(2_000);
        let body = format!("{lead}the headline\nsecond\n{}", "x".repeat(100_000));
        assert!(lead.chars().count() > 4096 * 2);
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, &body)
            .unwrap();
        store
            .enqueue_brigade_message(
                br,
                "worker-1",
                BrigadeRole::Director,
                None,
                &format!("{lead}reply line"),
            )
            .unwrap();

        let inbound = store
            .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
            .unwrap()
            .unwrap();
        let sent = store.latest_sent_glance(br, "worker-1").unwrap().unwrap();

        assert_eq!(inbound.line, "the headline");
        assert_eq!(sent.line, "reply line");
    }

    #[test]
    fn a_huge_body_is_read_only_as_far_as_the_glance_needs() {
        let store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("a").unwrap();
        let body = format!("headline\n{}", "x".repeat(200_000));
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, &body)
            .unwrap();

        let glance = store
            .latest_inbound_glance(br, "worker-1", BrigadeRole::Worker)
            .unwrap()
            .unwrap();

        assert_eq!(glance.line, "headline");
    }

    /// Throwaway measurement, not a check: what the two glance reads add to
    /// one relay tick for three members on a store of a few thousand messages
    /// across several brigades. Synthetic data only. Run with
    /// `cargo test -p banto-io --release glance_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn glance_cost_per_tick_on_a_synthetic_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("synthetic.db")).unwrap();
        let brigades: Vec<BrigadeId> = (0..6)
            .map(|n| store.create_brigade(&format!("b{n}")).unwrap())
            .collect();
        let senders = ["director", "worker-1", "worker-2", "worker-3"];
        for i in 0..6000usize {
            let br = brigades[i % brigades.len()];
            let from = senders[i % senders.len()];
            let (role, to) = if from == "director" {
                (BrigadeRole::Worker, (i % 3 == 0).then_some("worker-1"))
            } else {
                (BrigadeRole::Director, None)
            };
            store
                .enqueue_brigade_message(
                    br,
                    from,
                    role,
                    to,
                    &format!("message {i}\nsecond line of {i}"),
                )
                .unwrap();
        }
        // Three live members: the one the busiest brigade talks to a lot, and
        // two for which the newest match is far back (the worst case for a
        // scan from the newest row).
        let members: [(BrigadeId, &str, BrigadeRole); 3] = [
            (brigades[0], "worker-1", BrigadeRole::Worker),
            (brigades[1], "worker-3", BrigadeRole::Worker),
            (brigades[2], "director", BrigadeRole::Director),
        ];
        let ticks = 200;
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            for (br, token, role) in members {
                std::hint::black_box(store.latest_inbound_glance(br, token, role).unwrap());
                std::hint::black_box(store.latest_sent_glance(br, token).unwrap());
            }
        }
        let per_tick = start.elapsed() / ticks;
        // What the relay adds per tick for a member with unseen mail: its
        // inbound half only (the sent half is kept by the gate).
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            for (br, token, role) in members {
                std::hint::black_box(store.latest_inbound_glance(br, token, role).unwrap());
            }
        }
        let inbound_only = start.elapsed() / ticks;
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            std::hint::black_box(store.max_brigade_message_id().unwrap());
        }
        let gate = start.elapsed() / ticks;
        // A worst case: a member nobody has ever written to or heard from.
        let start = std::time::Instant::now();
        for _ in 0..ticks {
            std::hint::black_box(
                store
                    .latest_inbound_glance(brigades[3], "worker-9", BrigadeRole::Goinkyo)
                    .unwrap(),
            );
            std::hint::black_box(store.latest_sent_glance(brigades[3], "worker-9").unwrap());
        }
        let never = start.elapsed() / ticks;
        println!(
            "glance reads, 6000 messages / 6 brigades: 3 members {per_tick:?} per tick; \
             inbound half only, 3 members {inbound_only:?}; \
             one never-matching member {never:?}; MAX(id) gate {gate:?}"
        );
    }

    // --- stored arrangement ------------------------------------------------------

    fn member_layout(
        token: &str,
        rank: Option<u32>,
        weight: Option<u16>,
        minimized: bool,
    ) -> StoredMemberLayout {
        StoredMemberLayout {
            token: token.to_string(),
            stack_rank: rank,
            row_weight: weight,
            minimized,
        }
    }

    fn brigade_with(store: &mut Store, tokens: &[&str]) -> BrigadeId {
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "director", BrigadeRole::Director, Some(&sid("dir")))
            .unwrap();
        for token in tokens {
            store
                .add_brigade_member(br, token, BrigadeRole::Worker, Some(&sid(token)))
                .unwrap();
        }
        br
    }

    #[test]
    fn every_stored_arrangement_field_round_trips() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1", "worker-2", "worker-3"]);
        let layout = StoredLayout {
            master_permille: Some(640),
            collapsed_stack_width: Some(44),
            solo_member: Some("worker-2".to_string()),
            members: vec![
                member_layout("worker-1", Some(2), Some(7), true),
                member_layout("worker-2", Some(0), None, false),
                member_layout("worker-3", None, Some(12), false),
            ],
        };

        store.save_brigade_layout(br, &layout).unwrap();
        let mut read = store.brigade_layout(br).unwrap();

        // Members come back in token order; the director has nothing stored.
        read.members.sort_by(|a, b| a.token.cmp(&b.token));
        assert_eq!(read, layout);
    }

    #[test]
    fn a_new_brigade_and_a_missing_one_read_as_no_preference() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1"]);
        assert_eq!(store.brigade_layout(br).unwrap(), StoredLayout::default());
        assert_eq!(store.brigade_layout(9999).unwrap(), StoredLayout::default());
        // Saving to a brigade that does not exist is a harmless no-op.
        store
            .save_brigade_layout(
                9999,
                &StoredLayout {
                    master_permille: Some(1),
                    ..StoredLayout::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn a_snapshot_ignores_unknown_tokens_and_leaves_absent_members_alone() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1", "worker-2"]);
        store
            .save_brigade_layout(
                br,
                &StoredLayout {
                    members: vec![
                        member_layout("worker-1", Some(0), Some(5), true),
                        member_layout("worker-2", Some(1), Some(6), false),
                    ],
                    ..StoredLayout::default()
                },
            )
            .unwrap();
        let members_before = store.brigade_members(br).unwrap().len();

        // A later snapshot that lists only worker-1 (changed) and a ghost.
        store
            .save_brigade_layout(
                br,
                &StoredLayout {
                    master_permille: Some(300),
                    members: vec![
                        member_layout("worker-1", None, None, false),
                        member_layout("ghost", Some(0), Some(1), true),
                    ],
                    ..StoredLayout::default()
                },
            )
            .unwrap();

        assert_eq!(
            store.brigade_members(br).unwrap().len(),
            members_before,
            "a snapshot never inserts a member"
        );
        let read = store.brigade_layout(br).unwrap();
        assert_eq!(read.master_permille, Some(300));
        assert!(read.members.iter().all(|m| m.token != "ghost"));
        assert!(
            read.members.iter().all(|m| m.token != "worker-1"),
            "worker-1 was written back to nothing"
        );
        let two = read.members.iter().find(|m| m.token == "worker-2").unwrap();
        assert_eq!(
            two,
            &member_layout("worker-2", Some(1), Some(6), false),
            "left as it was"
        );
    }

    #[test]
    fn saving_the_same_snapshot_twice_changes_nothing() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1"]);
        let layout = StoredLayout {
            master_permille: Some(700),
            collapsed_stack_width: None,
            solo_member: Some("worker-1".to_string()),
            members: vec![member_layout("worker-1", Some(0), Some(9), true)],
        };
        store.save_brigade_layout(br, &layout).unwrap();
        let once = store.brigade_layout(br).unwrap();
        store.save_brigade_layout(br, &layout).unwrap();
        assert_eq!(store.brigade_layout(br).unwrap(), once);
        // Brigade-level `None` writes NULL: back to the default.
        store
            .save_brigade_layout(br, &StoredLayout::default())
            .unwrap();
        let read = store.brigade_layout(br).unwrap();
        assert_eq!((read.master_permille, read.solo_member), (None, None));
    }

    #[test]
    fn a_dismissed_member_forgets_its_arrangement_and_a_token_reused_by_hand_starts_fresh() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1", "worker-2"]);
        store
            .save_brigade_layout(
                br,
                &StoredLayout {
                    solo_member: Some("worker-2".to_string()),
                    members: vec![
                        member_layout("worker-1", Some(1), Some(8), true),
                        member_layout("worker-2", Some(0), Some(3), true),
                    ],
                    ..StoredLayout::default()
                },
            )
            .unwrap();

        store.dismiss_worker(br, "worker-2").unwrap();
        let after = store.brigade_layout(br).unwrap();
        assert!(after.members.iter().all(|m| m.token != "worker-2"));
        assert_eq!(
            after.solo_member, None,
            "a solo naming a gone member reads as none"
        );

        // A member under the same token again — by hand: add_worker never
        // reuses a Worker's, but the Goinkyo's is reused and a hand-edited store
        // could.
        store
            .add_brigade_member(br, "worker-2", BrigadeRole::Worker, Some(&sid("fresh")))
            .unwrap();
        let again = store.brigade_layout(br).unwrap();
        assert!(
            again.members.iter().all(|m| m.token != "worker-2"),
            "{again:?}"
        );
        // The solo named the dismissed member; the brigade row was cleared
        // with it, so the new Worker under the same token does not inherit it.
        assert_eq!(again.solo_member, None);
        assert!(again.members.iter().any(|m| m.token == "worker-1"));
    }

    /// Two connections to one file-backed store, the way the emporium and an
    /// agent's MCP server hold it.
    fn two_connections() -> (tempfile::TempDir, Store, Store) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("banto.db");
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        (dir, a, b)
    }

    #[test]
    fn a_late_save_cannot_put_a_dismissed_members_solo_back() {
        let (_dir, mut a, mut b) = two_connections();
        let br = brigade_with(&mut a, &["worker-1", "worker-2"]);
        // A took its snapshot while worker-1 was soloed...
        let stale = StoredLayout {
            master_permille: Some(600),
            solo_member: Some("worker-1".to_string()),
            members: vec![member_layout("worker-1", Some(0), Some(4), true)],
            ..StoredLayout::default()
        };
        // ...then another process dismissed worker-1...
        b.dismiss_worker(br, "worker-1").unwrap();
        // ...and A's save lands afterwards.
        a.save_brigade_layout(br, &stale).unwrap();

        let read = a.brigade_layout(br).unwrap();
        assert_eq!(
            read.solo_member, None,
            "the dismissed member's solo stays gone"
        );
        assert_eq!(
            read.master_permille,
            Some(600),
            "the rest of the snapshot still lands"
        );
        // A member under the same token (by hand, as above) must not inherit
        // the solo.
        b.add_brigade_member(br, "worker-1", BrigadeRole::Worker, Some(&sid("fresh")))
            .unwrap();
        let again = a.brigade_layout(br).unwrap();
        assert_eq!(again.solo_member, None);
        assert!(again.members.iter().all(|m| m.token != "worker-1"));
    }

    #[test]
    fn a_solo_naming_a_current_member_is_written() {
        let (_dir, mut a, _b) = two_connections();
        let br = brigade_with(&mut a, &["worker-1"]);
        a.save_brigade_layout(
            br,
            &StoredLayout {
                solo_member: Some("worker-1".to_string()),
                ..StoredLayout::default()
            },
        )
        .unwrap();
        assert_eq!(
            a.brigade_layout(br).unwrap().solo_member.as_deref(),
            Some("worker-1")
        );
        // The Director is a member too: soloing the master is stored.
        a.save_brigade_layout(
            br,
            &StoredLayout {
                solo_member: Some("director".to_string()),
                ..StoredLayout::default()
            },
        )
        .unwrap();
        assert_eq!(
            a.brigade_layout(br).unwrap().solo_member.as_deref(),
            Some("director")
        );
    }

    #[test]
    fn a_save_for_a_brigade_disbanded_in_the_meantime_is_a_silent_no_op() {
        let (_dir, mut a, mut b) = two_connections();
        let br = brigade_with(&mut a, &["worker-1"]);
        let stale = StoredLayout {
            master_permille: Some(300),
            collapsed_stack_width: Some(50),
            solo_member: Some("worker-1".to_string()),
            members: vec![member_layout("worker-1", Some(0), Some(9), true)],
        };
        b.delete_brigade(br).unwrap();

        a.save_brigade_layout(br, &stale)
            .expect("nothing to report: the brigade is simply gone");

        let count = |table: &str| -> i64 {
            a.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(count("brigades"), 0, "no brigade row was created");
        assert_eq!(count("brigade_members"), 0, "no member row was created");
        assert_eq!(a.brigade_layout(br).unwrap(), StoredLayout::default());
    }

    #[test]
    fn only_one_in_the_minimized_flag_means_collapsed() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1", "worker-2", "worker-3"]);
        for (token, flag) in [("worker-1", 1), ("worker-2", 2), ("worker-3", -1)] {
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET minimized = ?3
                     WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token, flag],
                )
                .unwrap();
        }

        let read = store.brigade_layout(br).unwrap();

        assert_eq!(
            read.members,
            [member_layout("worker-1", None, None, true)],
            "2 and -1 are no preference, so those members have no entry at all"
        );
    }

    #[test]
    fn disbanding_a_brigade_removes_its_arrangement() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(&mut store, &["worker-1"]);
        store
            .save_brigade_layout(
                br,
                &StoredLayout {
                    master_permille: Some(500),
                    members: vec![member_layout("worker-1", Some(0), Some(4), true)],
                    ..StoredLayout::default()
                },
            )
            .unwrap();

        store.delete_brigade(br).unwrap();

        assert_eq!(store.brigade_layout(br).unwrap(), StoredLayout::default());
        let rows: i64 = store
            .conn
            .query_row("SELECT count(*) FROM brigade_members", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn nonsensical_stored_values_read_as_no_preference_and_never_panic() {
        let mut store = Store::open_in_memory().unwrap();
        let br = brigade_with(
            &mut store,
            &["worker-1", "worker-2", "worker-3", "worker-4"],
        );
        let set_member = |token: &str, rank: i64, weight: i64, minimized: i64| {
            store
                .conn
                .execute(
                    "UPDATE brigade_members SET stack_rank = ?3, row_weight = ?4, minimized = ?5
                     WHERE brigade_id = ?1 AND member_token = ?2",
                    params![br, token, rank, weight, minimized],
                )
                .unwrap();
        };
        set_member("worker-1", -1, -5, 0);
        set_member("worker-2", 4_000_000_000, 70_000, 0);
        set_member("worker-3", 3, 0, 7);
        set_member("worker-4", 2, 65_535, 1);
        store
            .conn
            .execute(
                "UPDATE brigades SET master_permille = 5000, collapsed_stack_width = -3,
                 solo_member = 'not-a-member' WHERE id = ?1",
                [br],
            )
            .unwrap();

        let read = store.brigade_layout(br).unwrap();

        assert_eq!(read.master_permille, None);
        assert_eq!(read.collapsed_stack_width, None);
        assert_eq!(read.solo_member, None);
        let by = |token: &str| read.members.iter().find(|m| m.token == token);
        assert!(
            by("worker-1").is_none(),
            "negative rank and weight: nothing"
        );
        assert!(by("worker-2").is_none(), "huge rank and weight: nothing");
        assert_eq!(
            by("worker-3"),
            Some(&member_layout("worker-3", Some(3), None, false)),
            "a 7 in the flag is not a collapse; the rank beside it is still good"
        );
        assert_eq!(
            by("worker-4"),
            Some(&member_layout("worker-4", Some(2), Some(65_535), true))
        );
        // Boundary values that are fine stay.
        store
            .conn
            .execute(
                "UPDATE brigades SET master_permille = 1000, collapsed_stack_width = 1000 WHERE id = ?1",
                [br],
            )
            .unwrap();
        let read = store.brigade_layout(br).unwrap();
        assert_eq!(
            (read.master_permille, read.collapsed_stack_width),
            (Some(1000), Some(1000))
        );
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
    fn a_brigade_is_created_together_with_its_director() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store
            .create_brigade_with_director("cell", "director", &sid("dir"), AgentKind::Codex)
            .unwrap();

        assert_eq!(
            store.list_brigades().unwrap(),
            [Brigade {
                id: br,
                name: "cell".to_string(),
            }]
        );
        let director = store.brigade_member(br, "director").unwrap().unwrap();
        assert_eq!(director.role, BrigadeRole::Director);
        assert_eq!(director.session_id, Some(sid("dir")));
        assert_eq!(director.agent, Some(AgentKind::Codex));
        // The same rows `add_brigade_member_with_agent` writes: the cursor too.
        let cursors: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM brigade_cursors WHERE brigade_id = ?1",
                [br],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cursors, 1);
        // Nothing for the janitor to find.
        assert_eq!(store.delete_empty_brigades().unwrap(), 0);
    }

    /// The janitor deletes any brigade without a member, so what is committed
    /// must never be one. A Director row that cannot be written has to take the
    /// brigade row with it.
    #[test]
    fn a_director_row_that_fails_leaves_no_brigade_behind() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "CREATE TRIGGER refuse_members BEFORE INSERT ON brigade_members
                 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();

        let err = store
            .create_brigade_with_director("cell", "director", &sid("dir"), AgentKind::ClaudeCode)
            .unwrap_err();
        assert!(err.to_string().contains("injected failure"), "{err}");
        assert_eq!(store.list_brigades().unwrap(), []);

        // Not a poisoned store: with the fault gone the same call succeeds.
        store
            .conn
            .execute_batch("DROP TRIGGER refuse_members;")
            .unwrap();
        let br = store
            .create_brigade_with_director("cell", "director", &sid("dir"), AgentKind::ClaudeCode)
            .unwrap();
        assert_eq!(store.list_brigades().unwrap().len(), 1);
        assert!(store.brigade_member(br, "director").unwrap().is_some());
    }

    #[test]
    fn wake_waiter_consumes_issues_and_newest_generation_wins() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        let first = store.arm_wake(br, "worker-1").unwrap().unwrap();
        let second = store.arm_wake(br, "worker-1").unwrap().unwrap();
        assert_eq!(store.poll_wake(br, "worker-1", first).unwrap(), None);
        assert_eq!(
            store.poll_wake(br, "worker-1", second).unwrap(),
            Some(false)
        );
        assert!(!store.wake_pending(br, "worker-1").unwrap());
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        assert!(store.wake_pending(br, "worker-1").unwrap());
        assert_eq!(store.poll_wake(br, "worker-1", second).unwrap(), Some(true));
        assert!(!store.wake_pending(br, "worker-1").unwrap());
        assert_eq!(
            store.poll_wake(br, "worker-1", second).unwrap(),
            Some(false)
        );
        store
            .fetch_brigade_messages(br, "worker-1", BrigadeRole::Worker)
            .unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        assert_eq!(
            store.poll_wake(br, "worker-1", second).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn wake_only_delivers_mail_addressed_to_the_waiter() {
        let mut store = Store::open_in_memory().unwrap();
        let br = store.create_brigade("cell").unwrap();
        for member in ["worker-1", "worker-2"] {
            store
                .add_brigade_member(br, member, BrigadeRole::Worker, None)
                .unwrap();
        }
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, Some("worker-2"), "hi")
            .unwrap();
        let first = store.arm_wake(br, "worker-1").unwrap().unwrap();
        let second = store.arm_wake(br, "worker-2").unwrap().unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        store.issue_wake(br, "worker-2").unwrap();
        assert_eq!(store.poll_wake(br, "worker-1", first).unwrap(), Some(false));
        assert!(!store.wake_pending(br, "worker-1").unwrap());
        assert_eq!(store.poll_wake(br, "worker-2", second).unwrap(), Some(true));
    }

    #[test]
    fn wake_poll_waits_for_a_competing_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let mut store = Store::open(&path).unwrap();
        let br = store.create_brigade("cell").unwrap();
        store
            .add_brigade_member(br, "worker-1", BrigadeRole::Worker, None)
            .unwrap();
        store
            .enqueue_brigade_message(br, "director", BrigadeRole::Worker, None, "hi")
            .unwrap();
        let generation = store.arm_wake(br, "worker-1").unwrap().unwrap();
        store.issue_wake(br, "worker-1").unwrap();
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let poller = std::thread::spawn(move || {
            tx.send(()).unwrap();
            store.poll_wake(br, "worker-1", generation)
        });
        rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!poller.is_finished(), "poll did not wait for the writer");
        blocker.execute_batch("COMMIT").unwrap();
        assert_eq!(poller.join().unwrap().unwrap(), Some(true));
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
    fn a_deleted_brigades_id_is_never_issued_again() {
        // `brigades.id` is AUTOINCREMENT: the brigade message log names its file by
        // this id, so a reused one would pour a new brigade's messages into a
        // disbanded one's file.
        let mut store = Store::open_in_memory().unwrap();
        let first = store.create_brigade("a").unwrap();
        let second = store.create_brigade("b").unwrap();
        store.delete_brigade(second).unwrap();
        let third = store.create_brigade("c").unwrap();
        assert!(third > second, "{first} {second} {third}");
        store.delete_brigade(third).unwrap();
        store.delete_brigade(first).unwrap();
        assert!(store.create_brigade("d").unwrap() > third);
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
