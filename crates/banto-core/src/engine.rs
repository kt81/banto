//! The emporium's pure core (`docs/DISCIPLINE.md` §4): `update(state, app,
//! ev, now) -> Vec<Cmd>` is a function from an [`Event`] (a fact about the
//! outside world) to state mutations and [`Cmd`]s (instructions for the
//! shell — `banto::embedded::emporium` — to execute). No clock reads, no
//! file/process/store access, no terminal access in this module; every place
//! that touches the world is named as an `Event` coming in or a `Cmd` going
//! out.
//!
//! `EmporiumState` replaces the old `Emporium` struct. The append-only
//! sessions invariant retires with it: `screens` can lose entries
//! (`PtyExited`), and `Stage` holds [`SessionKey`]s rather than indices, so a
//! removal never invalidates anything else holding a key.
//!
//! Two round trips exist here that the pre-migration code didn't need,
//! because they used to be synchronous inline store reads: resolving
//! whether a selected row is a brigade Director (`ResolveMembership`), and
//! joining/spawning a brigade in general. That's the honest cost of moving a
//! read that used to block the handler into "ask the shell, wait for the
//! fact" — see the `StoreIntent`/`Event` variants below.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui_core::layout::{Constraint, Layout, Position, Rect};
use serde::{Deserialize, Serialize};

use crate::app::{App, ClickOutcome, GroupJoinTarget, KillChoice, Modal, Mode};
use crate::config::{BrigadeConfig, RelayMode, WorkerAgentSetting};
use crate::input::{
    InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crate::key_encode::{key_to_bytes, normalize_paste_line_endings, wrap_bracketed_paste};
use crate::model::{AgentKind, BrigadeId, BrigadeRole, MemberToken, SessionRow, SessionToOpen};

/// Fixed width of the left sidebar (the session list), in columns.
pub const SIDEBAR_WIDTH: u16 = 36;
/// Details panel height: one border row plus its content rows.
pub const SUMMARY_HEIGHT: u16 = 5;
/// Below this left-column height the details panel is dropped so the list keeps
/// the room.
pub const MIN_HEIGHT_FOR_SUMMARY: u16 = 12;

/// Stable identity for a kept-alive embedded session: the real Claude
/// session id once known, or a synthetic placeholder for a freshly-launched
/// one still awaiting id discovery (see [`Self::is_synthetic`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn from_id(id: &str) -> Self {
        Self(id.to_string())
    }

    /// `discriminator` distinguishes two fresh opens into the *same* cwd
    /// before either has a real id yet (e.g. `n` pressed twice in a row) —
    /// without one, both keys would be identical and collide in every
    /// structure keyed by `SessionKey` (`pending_opens`, `screens`, the
    /// shell's PTY handle map, `Stage`), silently dropping one pane's data
    /// or cross-wiring the other's discovered id onto it. Callers get one
    /// from [`EmporiumState::mint_plain_key`], never a clock or random value
    /// (this crate is replayed deterministically from a recorded event
    /// stream — see `crate::replay` — and cannot name a clock anyway).
    fn new_plain(cwd: &std::path::Path, discriminator: u64) -> Self {
        Self(format!("new::{}::{discriminator}", cwd.display()))
    }

    fn new_worker(brigade_id: BrigadeId, token: &str) -> Self {
        Self(format!("new-worker::{brigade_id}::{token}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this key is a placeholder awaiting id discovery, rather than
    /// a real session id.
    pub fn is_synthetic(&self) -> bool {
        self.0.starts_with("new::") || self.0.starts_with("new-worker::")
    }

    /// If this is a still-awaiting-discovery Worker's placeholder (built by
    /// [`Self::new_worker`]), the `(brigade_id, token)` embedded in it —
    /// `None` for every other key, including a *resolved* Worker's real
    /// session id, which carries no brigade info of its own (that needs a
    /// store round trip — see `confirm_kill_modal`'s dismiss path).
    fn worker_identity(&self) -> Option<(BrigadeId, MemberToken)> {
        let rest = self.0.strip_prefix("new-worker::")?;
        let (id, token) = rest.split_once("::")?;
        Some((id.parse().ok()?, token.to_string()))
    }
}

/// Which side currently receives keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Pane,
}

/// What the right-hand pane region is showing: nothing, a single session, or
/// a brigade tiled across several panes.
#[derive(Debug)]
pub enum Stage {
    Empty,
    Solo(SessionKey),
    Brigade {
        id: BrigadeId,
        /// Director first.
        panes: Vec<SessionKey>,
        focused: usize,
    },
}

impl Stage {
    pub fn focused_key(&self) -> Option<&SessionKey> {
        match self {
            Stage::Empty => None,
            Stage::Solo(key) => Some(key),
            Stage::Brigade { panes, focused, .. } => panes.get(*focused),
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self, Stage::Empty)
    }

    /// Drop `key` from the stage (a session exited). `Solo` collapses to
    /// `Empty`; a brigade pane is removed with `focused` clamped into range
    /// (collapsing to `Empty` if that was the last pane).
    fn remove(&mut self, key: &SessionKey) {
        match self {
            Stage::Solo(k) if k == key => *self = Stage::Empty,
            Stage::Brigade { panes, focused, .. } => {
                if let Some(pos) = panes.iter().position(|k| k == key) {
                    panes.remove(pos);
                    if panes.is_empty() {
                        *self = Stage::Empty;
                    } else if *focused >= panes.len() {
                        *focused = panes.len() - 1;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The outer (bordered) tile rects for the currently-staged sessions, each
/// paired with its key. A solo session fills the whole pane; a brigade puts
/// the Director on the left and stacks the Workers down the right (a
/// "master + stack" layout).
pub fn stage_tiles(pane_area: Rect, stage: &Stage) -> Vec<(SessionKey, Rect)> {
    match stage {
        Stage::Empty => Vec::new(),
        Stage::Solo(key) => vec![(key.clone(), pane_area)],
        Stage::Brigade { panes, .. } => match panes.split_first() {
            None => Vec::new(),
            Some((director, [])) => vec![(director.clone(), pane_area)],
            Some((director, workers)) => {
                let [master, stack] =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(pane_area);
                let rows = Layout::vertical(vec![
                    Constraint::Ratio(1, workers.len() as u32);
                    workers.len()
                ])
                .split(stack);
                let mut tiles = vec![(director.clone(), master)];
                for (worker, row) in workers.iter().zip(rows.iter()) {
                    tiles.push((worker.clone(), *row));
                }
                tiles
            }
        },
    }
}

/// The inner content rect of the right pane (inside its border).
pub fn pane_content(pane_area: Rect) -> Rect {
    Rect {
        x: pane_area.x + 1,
        y: pane_area.y + 1,
        width: pane_area.width.saturating_sub(2).max(1),
        height: pane_area.height.saturating_sub(2).max(1),
    }
}

/// Compute the layout: a bottom status bar, and above it a left column
/// (sidebar list + details panel) beside the session pane.
pub fn layout(area: Rect) -> Areas {
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let [left, pane] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]).areas(body);
    let summary_h = if left.height < MIN_HEIGHT_FOR_SUMMARY {
        0
    } else {
        SUMMARY_HEIGHT
    };
    let [sidebar, summary] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(summary_h)]).areas(left);
    Areas {
        sidebar,
        summary,
        pane,
        status,
    }
}

/// The regions of the emporium layout.
#[derive(Clone, Copy)]
pub struct Areas {
    pub sidebar: Rect,
    pub summary: Rect,
    pub pane: Rect,
    pub status: Rect,
}

// --- Relay engine (unchanged from the pre-migration code — already pure) ---

/// Consecutive relay ticks a member must be observed idle before it's
/// eligible for a nudge.
const RELAY_IDLE_STREAK_REQUIRED: u32 = 2;
/// How long a focused pane's own recently-forwarded input suppresses a
/// nudge. Was 3s, which is a *typing-gap* threshold (how long between two
/// keystrokes before you'd call the operator "done typing"), not a
/// *composing* one — a human writing even one sentence routinely pauses
/// longer than that to think, re-read, or pick a word, so the old value let
/// a nudge land mid-sentence: confirmed in the field, where it did exactly
/// that, corrupting a message the operator was composing to a Worker (the
/// nudge text landed inside it, then [`RELAY_SUBMIT_DELAY`] later submitted
/// the mixed result before he could stop it — see [`update_key`]'s
/// `cancel_pending_submit_on_input` call for the other half of that fix).
/// 30s is chosen for the cost asymmetry, not a measured pause length: a
/// nudge arriving late only delays how soon the Worker notices unread mail
/// (and the operator can still see the unread marker in the sidebar in the
/// meantime), while a nudge arriving mid-composition can destroy what the
/// operator was actually typing — a much more expensive failure to guard a
/// few extra seconds against.
const RELAY_INPUT_QUIET_PERIOD: Duration = Duration::from_secs(30);
/// Minimum gap between nudges to the same member.
const RELAY_NUDGE_COOLDOWN: Duration = Duration::from_secs(60);
/// Give up nudging a member after this many attempts on one unseen batch.
const RELAY_MAX_ATTEMPTS: u32 = 3;
/// The fixed, ASCII-only line typed into a nudged member's stdin.
const RELAY_NUDGE_LINE: &str =
    "[banto relay] Your brigade peer sent you a message. Call the check_messages tool now.";
/// How long after the nudge text before its submitting `\r` is sent — see
/// [`update_tick`].
const RELAY_SUBMIT_DELAY: Duration = Duration::from_millis(300);
/// How long a transient status message shows before [`update_tick`] clears it.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a freshly-spawned Codex Worker pane's own output must have been
/// quiet before [`update_tick`] types [`CODEX_WORKER_KICKOFF_LINE`] into it
/// — see that constant's doc for why this exists at all. Measured
/// 2026-08-02: Codex's own boot sequence (terminal-mode escapes, the
/// hook-bypass warning banner, an MCP server booting, the composer frame
/// settling) ran for ~2s after spawn in the captured run, every gap
/// *inside* that burst under 400ms; 700ms clears the noisiest observed
/// mid-boot gap with real margin, without piling much more wait on top.
/// Confirmed end to end, not just inferred from the gap sizes: typed at the
/// 700ms-quiet mark, the kickoff line was not dropped or garbled — the
/// resulting turn ran, the hook fired, and the discovered session id
/// resolved to a real Codex `threads` row within a few seconds.
const CODEX_KICKOFF_QUIET_PERIOD: Duration = Duration::from_millis(700);

/// The fixed, ASCII-only line banto types into a freshly-spawned Codex
/// Worker's stdin once its boot output goes quiet (see
/// [`CODEX_KICKOFF_QUIET_PERIOD`]) — the turn this starts is what makes
/// Codex's own `SessionStart` hook fire at all. Measured (2026-08-02
/// investigation): an idle Codex TUI with nothing ever typed into it never
/// runs the hook, never creates a session `threads` row, never writes a
/// rollout file — a freshly spawned Worker is otherwise permanently
/// undiscoverable, since every path banto has for learning a Codex session's
/// id depends on that same first turn having happened.
///
/// This is deliberately *not* [`RELAY_NUDGE_LINE`] reused: that line
/// asserts a peer sent a message, which would be false here — nobody has,
/// this pane just started, and the member reading it would call
/// `check_messages` to find an empty inbox. Typing a false "you have mail"
/// purely to make a hook fire trades a working mechanism for a member's
/// trust in what banto tells it, which is a worse trade than the one it's
/// avoiding; this line only states what's actually true at the moment it's
/// sent. It also doesn't restate the member's role or peers — the hook's
/// own `additionalContext` delivers that on this very same turn
/// (`crate::hook`'s module doc, in the `banto` crate), so repeating it here
/// would just be noise on top of noise.
///
/// ASCII-only for the same reason [`RELAY_NUDGE_LINE`] is: this is typed as
/// literal bytes into whatever the child's own input widget does with them,
/// and nothing here has verified how a raw multi-byte UTF-8 sequence
/// arriving as one written chunk behaves in Codex's own input handling — no
/// reason to be the one to find out.
const CODEX_WORKER_KICKOFF_LINE: &str = "[banto] This pane just started as a brigade member.";

#[derive(Debug, Default, Clone, Copy)]
pub struct NudgeState {
    last_nudge: Option<Instant>,
    attempts: u32,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RelayState {
    idle_streak: u32,
    nudge: NudgeState,
}

pub fn should_nudge(
    now: Instant,
    idle_streak: u32,
    is_focused: bool,
    last_forwarded_input: Option<Instant>,
    has_unseen: bool,
    state: &NudgeState,
) -> bool {
    if !has_unseen || idle_streak < RELAY_IDLE_STREAK_REQUIRED {
        return false;
    }
    if is_focused
        && let Some(last_input) = last_forwarded_input
        && now.saturating_duration_since(last_input) < RELAY_INPUT_QUIET_PERIOD
    {
        return false;
    }
    if state.attempts >= RELAY_MAX_ATTEMPTS {
        return false;
    }
    if let Some(last_nudge) = state.last_nudge
        && now.saturating_duration_since(last_nudge) < RELAY_NUDGE_COOLDOWN
    {
        return false;
    }
    true
}

pub fn tick_relay_decision(
    states: &mut HashMap<MemberToken, RelayState>,
    token: &MemberToken,
    now: Instant,
    is_idle_this_tick: Option<bool>,
    is_focused: bool,
    last_forwarded_input: Option<Instant>,
    has_unseen: bool,
) -> bool {
    if !has_unseen {
        states.remove(token);
        return false;
    }
    let state = states.entry(token.clone()).or_default();
    state.idle_streak = if is_idle_this_tick == Some(true) {
        state.idle_streak + 1
    } else {
        0
    };
    let nudge = should_nudge(
        now,
        state.idle_streak,
        is_focused,
        last_forwarded_input,
        has_unseen,
        &state.nudge,
    );
    if nudge {
        state.nudge.last_nudge = Some(now);
        state.nudge.attempts += 1;
    }
    nudge
}

/// One relay-eligible staged member's freshly-gathered observations for this
/// tick — gathered shell-side (store + live-session reads), decided
/// core-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayObservation {
    pub token: MemberToken,
    pub key: SessionKey,
    pub has_unseen: bool,
    pub is_idle_this_tick: Option<bool>,
}

/// A nudge awaiting its phase-two Enter (see [`update_tick`]).
struct PendingSubmit {
    key: SessionKey,
    nudged_at: Instant,
}

/// Drop `key`'s pending submitting `\r`, if one is waiting, because real
/// operator input for that same pane just arrived. Widening
/// [`RELAY_INPUT_QUIET_PERIOD`] narrows the window a nudge can start in
/// while the operator is composing, but it cannot close it — the operator
/// can still start typing in the [`RELAY_SUBMIT_DELAY`] gap between a
/// nudge's text landing and its `\r` following. Left unguarded, that `\r`
/// submits banto's nudge text spliced with whatever the operator has typed
/// on top of it since, not what either of them meant to send. Called at
/// every site that forwards real keystrokes to a pane (not the mouse path —
/// an SGR report to the child isn't the operator composing text into it).
///
/// The nudge text already written to the pane is left alone: banto cannot
/// un-type it without writing more bytes into the same line the operator is
/// mid-edit on, and a guessed-at erase (backspaces sized to the nudge
/// line's length) risks deleting the operator's own text instead if the
/// child already reflowed or echoed something in between. Leaving a stray
/// line the operator can see and clear themselves is the honest outcome
/// here — silently submitting it early is the actual bug.
fn cancel_pending_submit_on_input(pending_submits: &mut Vec<PendingSubmit>, key: &SessionKey) {
    pending_submits.retain(|pending| &pending.key != key);
}

/// A freshly-spawned Codex Worker pane awaiting its own boot output going
/// quiet before [`update_tick`] asks whether it's safe to type
/// [`CODEX_WORKER_KICKOFF_LINE`] into it — see that constant's doc for why
/// the kickoff exists at all, and [`Cmd::CheckWorkerDirectoryTrust`]'s for
/// why quiet alone isn't enough to type it blind. Queued by
/// [`update_spawned`] the moment such a pane's `PendingOpen::BrigadeMember`
/// resolves; removed once the kickoff is actually sent (its `\r` becomes an
/// ordinary [`PendingSubmit`], the same two-phase shape as a relay nudge) or
/// the pane unstages first.
struct PendingKickoff {
    key: SessionKey,
    /// Baseline for the quiet-period check when [`EmporiumState::last_output_at`]
    /// has no entry yet for this pane (nothing has arrived at all) — the
    /// moment this tracker was created, i.e. spawn time.
    spawned_at: Instant,
    /// This Worker's own cwd — echoed on [`Cmd::CheckWorkerDirectoryTrust`]
    /// once the quiet period elapses, since the pure core has no way to read
    /// Codex's own trust records itself and the shell has no other way to
    /// know which directory to check.
    cwd: PathBuf,
    /// Whether [`update_worker_directory_trust_checked`] has already told
    /// the operator this pane is waiting on an unanswered trust prompt —
    /// set the first time the answer comes back "not trusted", so the
    /// status line states it once instead of restamping the same notice
    /// every tick while banto keeps quietly re-checking.
    notified_untrusted: bool,
}

/// Why an in-flight `Cmd::OpenEmbedded` was requested, so `Spawned`/
/// `SpawnFailed` know what to do with the result.
enum PendingOpen {
    /// Stage as the solo pane once spawned.
    Solo,
    /// The Director of a brigade being formed: on success, stage it and
    /// open each worker token next (workers never spawn ahead of a Director
    /// that might still fail); on failure, stop — no workers are spawned.
    BrigadeDirector {
        brigade_id: BrigadeId,
        worker_tokens: Vec<MemberToken>,
        cwd: PathBuf,
        /// Resolved once at `StoreIntent::FormBrigade`'s own press-time
        /// moment (see its doc) and carried this whole way so opening each
        /// Worker, once the Director's own spawn succeeds, never has to
        /// re-derive it from `app`.
        worker_agent: AgentKind,
        /// Same reasoning as `worker_agent` — carried through so
        /// `update_spawned` never needs `&BrigadeConfig` just to look this
        /// up again.
        worker_model: String,
    },
    /// One member (Director or Worker) of a brigade whose `Stage` already
    /// exists (or is being built alongside this open): on success, append
    /// to `panes`.
    BrigadeMember {
        brigade_id: BrigadeId,
        /// `true` only for a Worker [`open_worker`] just spawned fresh (no
        /// id yet) as a Codex agent — the one case that needs a
        /// [`PendingKickoff`] queued once it's actually spawned. Always
        /// `false` for a *resumed* member ([`stage_brigade`]'s own
        /// `Some(row)` branch inserts this same variant, for a session
        /// whose id is already known): it has nothing left to discover and
        /// no reason to spend a turn kicking it off.
        needs_codex_kickoff: bool,
        /// This member's cwd — carried through unconditionally (even when
        /// `needs_codex_kickoff` is `false`) since it's cheap to plumb and
        /// only [`PendingKickoff`] ever reads it back out. Needed there to
        /// ask the shell whether Codex trusts this exact directory before
        /// typing into it.
        cwd: PathBuf,
    },
}

/// Why a `Cmd::Store(StoreIntent::ResolveMembership)` was requested.
enum PendingMembership {
    /// Enter / double-click on the sidebar.
    Activate,
    /// `B`.
    BrigadeKey,
    /// Prefix-`x` confirmed "dismiss" on a Worker pane whose key isn't a
    /// synthetic placeholder, so its token isn't embedded in the key itself
    /// ([`SessionKey::worker_identity`]) — one round trip to learn
    /// `(brigade_id, token)` before `StoreIntent::DismissWorker` can be
    /// built. The pane to remove once dismissal succeeds is already in
    /// [`EmporiumState::pending_dismiss`] (set by `confirm_kill_modal`
    /// before this round trip was requested); this variant only carries
    /// *that* the answer means "go dismiss", not "go stage/disband".
    DismissWorker,
}

/// A brigade formation waiting on `Modal::WorkerAgentPicker` to say which
/// product (and model) its Workers will run as — everything `FormBrigade`
/// needs *except* that, stashed here the moment the modal opens (see
/// `begin_brigade_formation`'s `Select` branch) and consumed by
/// [`confirm_worker_agent_modal`] once the operator confirms. Cleared on
/// Esc too (`update_modal_key`), so a cancelled picker can never leave a
/// stale formation for some later, unrelated confirm to pick up by
/// accident.
struct PendingBrigadeFormation {
    director_row_id: String,
    name: String,
    cwd: PathBuf,
    worker_count: usize,
}

/// A Worker being added to an *already-formed* brigade (`add_worker`, `b`)
/// waiting on the same `Modal::WorkerAgentPicker` — everything
/// `StoreIntent::AddWorker` needs except the product/model, stashed the
/// moment the modal opens and consumed by [`confirm_worker_agent_modal`]
/// once the operator confirms. Cleared on Esc too, same as
/// [`PendingBrigadeFormation`] — but Esc here means something lighter: no
/// Worker gets added, an already-running brigade is otherwise untouched,
/// not "the whole cell never forms."
struct PendingWorkerAddition {
    brigade_id: BrigadeId,
    cwd: PathBuf,
}

/// A brigade formation waiting on a fresh `Cmd::CheckCodexTrust` round trip
/// before `StoreIntent::FormBrigade` is issued — everything `FormBrigade`
/// needs, stashed the moment the resolved Worker product turns out to be
/// [`AgentKind::Codex`] (`update_membership_resolved`'s non-`Select` branch,
/// or [`confirm_worker_agent_modal`] once `Select`'s own picker resolves).
/// Unlike [`PendingBrigadeFormation`], `worker_agent`/`worker_model` are
/// already known by the time this exists — the whole point of this stash is
/// that they're the fully-resolved values, not a placeholder awaiting them.
/// Consumed unconditionally by [`update_codex_trust_checked`], whichever way
/// the check comes out.
struct PendingCodexTrustFormation {
    director_row_id: String,
    name: String,
    cwd: PathBuf,
    worker_count: usize,
    worker_agent: AgentKind,
    worker_model: String,
}

/// Everything needed to open a freshly-formed brigade's Director wired
/// (MCP config / Codex `-c` overrides, plus which Workers to open once it
/// spawns) — [`PendingOpen::BrigadeDirector`]'s own payload, held here for
/// the one case that can't build `Cmd::OpenEmbedded` immediately:
/// `update_brigade_formed` finding the Director's pane still open despite
/// `begin_brigade_formation`'s own pre-formation gate (see that function's
/// doc for why this is a residual race, not the common case, and why there
/// is no aborting once we're here — the brigade record already exists).
struct PendingDirectorWiring {
    director_row_id: String,
    brigade_id: BrigadeId,
    worker_tokens: Vec<MemberToken>,
    cwd: PathBuf,
    worker_agent: AgentKind,
    worker_model: String,
}

pub struct EmporiumState {
    pub screens: HashMap<SessionKey, crate::screen::Screen>,
    pub stage: Stage,
    pub focus: Focus,
    pub status: Option<String>,
    status_set_at: Option<Instant>,
    pub relay_states: HashMap<MemberToken, RelayState>,
    /// When real operator input was last forwarded to each pane, keyed by
    /// its own [`SessionKey`] — not a single run-wide instant. `should_nudge`
    /// only ever suppresses a nudge for the pane the operator is actually
    /// typing into (`RELAY_INPUT_QUIET_PERIOD`'s whole point is protecting
    /// mid-composition text from a spliced-in nudge); a shared field here
    /// used to mean input to *any* pane silenced nudges to the *focused*
    /// one, so switching to a different pane, typing there, then tabbing
    /// back to a focused-but-untouched one kept it suppressed on someone
    /// else's keystrokes. An entry is dropped when its pane unstages (see
    /// [`Self::unstage`]) so a closed pane's key never accumulates here.
    pub last_forwarded_input: HashMap<SessionKey, Instant>,
    /// When a pane's child last produced *any* output, keyed by its own
    /// [`SessionKey`] — the baseline [`update_tick`]'s kickoff-readiness
    /// check measures quiet time against (see
    /// [`CODEX_KICKOFF_QUIET_PERIOD`]). Only meaningful for a pane with an
    /// entry in [`Self::pending_kickoffs`], but kept for every pane
    /// regardless: a `HashMap` write on every `Event::PtyOutput` is cheaper
    /// than checking membership first. Dropped when its pane unstages, same
    /// as [`Self::last_forwarded_input`].
    last_output_at: HashMap<SessionKey, Instant>,
    pending_kickoffs: Vec<PendingKickoff>,
    pending_submits: Vec<PendingSubmit>,
    pending_opens: HashMap<SessionKey, PendingOpen>,
    pending_membership: Option<PendingMembership>,
    pending_brigade_formation: Option<PendingBrigadeFormation>,
    pending_worker_addition: Option<PendingWorkerAddition>,
    pending_codex_trust_formation: Option<PendingCodexTrustFormation>,
    /// The session id of a Director-to-be whose already-open pane was just
    /// killed (`confirm_director_reopen_modal`), waiting on its
    /// `Event::PtyExited` before formation can continue — reopening any
    /// sooner would resume a session `opener::decide_inplace_resume` still
    /// sees as live and refuse. `update_pty_exited` takes it once the exit
    /// for this exact key lands. Unlike `pending_brigade_formation`/
    /// `pending_codex_trust_formation`, not cleared on every Esc: by the
    /// time this is set, `Modal::ConfirmDirectorReopen` is already closed
    /// (see that function), so no *other* modal's Esc should touch it.
    pending_director_reopen: Option<String>,
    /// The residual-race counterpart to [`Self::pending_director_reopen`] —
    /// set by `update_brigade_formed` instead of the interactive gate, so it
    /// carries the full [`PendingDirectorWiring`] rather than just a row id
    /// to re-derive a formation decision from (there's nothing left to
    /// decide by that point, only wiring to apply). `update_pty_exited`
    /// checks both; the two are never set at once in practice, since the
    /// interactive gate's own stash is always cleared before `FormBrigade`
    /// is issued.
    pending_director_wiring: Option<PendingDirectorWiring>,
    /// The Worker pane a confirmed prefix-`x` dismiss is about to remove,
    /// stashed at confirm time (`confirm_kill_modal`) — regardless of
    /// whether `StoreIntent::DismissWorker` was built immediately (a
    /// synthetic key) or needed the [`PendingMembership::DismissWorker`]
    /// round trip first — and taken by [`update_worker_dismissed`] once
    /// `Event::WorkerDismissed` lands, so a failed or foreign dismissal
    /// never leaves it stale for a later, unrelated one.
    pending_dismiss: Option<SessionKey>,
    pub size: (u16, u16),
    /// The configured prefix chord (`[keys] prefix`), fixed for the run.
    prefix: PrefixKey,
    /// When the prefix was last armed (see [`update_key`]'s prefix handling
    /// and [`PREFIX_ARM_TIMEOUT`]) — `Some` only between the prefix chord
    /// landing on a focused pane and the very next key resolving it (or the
    /// timeout disarming it on a [`Event::Tick`]). `pub` so the shell
    /// can show the pending-prefix hint while armed (reading state for
    /// drawing is legal; only `update` may write it).
    pub prefix_armed: Option<Instant>,
    /// The next discriminator [`Self::mint_plain_key`] hands out. Only ever
    /// increments, even past a closed pane's key going out of scope — a
    /// reused discriminator could attach a stale mapping (an old screen, a
    /// stale PTY handle) to a new pane that happens to mint the same key.
    next_plain_id: u64,
    /// Whether banto's own process currently holds OS focus — `true` at
    /// startup (a fresh window is normally the one the operator just
    /// launched into; the first real `Event::WindowFocusChanged` corrects
    /// this if it's ever wrong, same as any other "assume it, self-heal on
    /// the next real fact" default in this module). See
    /// [`effective_focused_pane`] for how this composes with [`Self::focus`].
    window_focused: bool,
    /// The pane [`emit_focus_transitions`] last sent `CSI I` to (`None` for
    /// "nobody, as far as that diff is concerned") — compared against
    /// [`effective_focused_pane`] on every call so a change only ever
    /// produces the one `CSI O`/`CSI I` pair it actually represents, not a
    /// repeat on every tick.
    last_focus_notified: Option<SessionKey>,
}

impl EmporiumState {
    pub fn new(prefix: PrefixKey) -> Self {
        Self {
            screens: HashMap::new(),
            stage: Stage::Empty,
            focus: Focus::Sidebar,
            status: None,
            status_set_at: None,
            relay_states: HashMap::new(),
            last_forwarded_input: HashMap::new(),
            last_output_at: HashMap::new(),
            pending_kickoffs: Vec::new(),
            pending_submits: Vec::new(),
            pending_opens: HashMap::new(),
            pending_membership: None,
            pending_brigade_formation: None,
            pending_worker_addition: None,
            pending_codex_trust_formation: None,
            pending_director_reopen: None,
            pending_director_wiring: None,
            pending_dismiss: None,
            size: (0, 0),
            prefix,
            prefix_armed: None,
            next_plain_id: 0,
            window_focused: true,
            last_focus_notified: None,
        }
    }

    fn set_status(&mut self, message: impl Into<String>, now: Instant) {
        self.status = Some(message.into());
        self.status_set_at = Some(now);
    }

    /// Mint a fresh, never-reused [`SessionKey`] for a plain (non-brigade)
    /// new-session open into `cwd` — see [`SessionKey::new_plain`] for why
    /// this can't just be `SessionKey::new_plain(cwd)` on its own.
    fn mint_plain_key(&mut self, cwd: &std::path::Path) -> SessionKey {
        let discriminator = self.next_plain_id;
        self.next_plain_id += 1;
        SessionKey::new_plain(cwd, discriminator)
    }

    /// Drop `key` from the stage, keeping [`Self::focus`] pointing at
    /// something that exists.
    ///
    /// Removing the last pane leaves [`Stage::Empty`], and a `Focus::Pane`
    /// outliving it strands every keypress: the pane branch forwards to a
    /// focused key that is now `None`, and the sidebar branch never runs
    /// because focus is not on the sidebar. Nothing blocks and nothing
    /// spins — the loop keeps drawing a correct screen while answering
    /// nothing, which reads as a freeze rather than as a bug. `prefix x` on
    /// a solo pane did exactly that, and F2 was the only way out, for
    /// someone who already knew.
    ///
    /// Unstaging goes through here rather than calling `Stage::remove`
    /// directly so that a third caller cannot reintroduce it by omitting a
    /// step it has no reason to know about.
    fn unstage(&mut self, key: &SessionKey) {
        self.stage.remove(key);
        self.last_forwarded_input.remove(key);
        self.last_output_at.remove(key);
        self.pending_kickoffs.retain(|pending| &pending.key != key);
        if !self.stage.is_active() {
            self.focus = Focus::Sidebar;
        }
    }
}

/// How long the prefix stays armed with no follow-up key before
/// [`update_tick`] disarms it on its own.
const PREFIX_ARM_TIMEOUT: Duration = Duration::from_secs(3);

/// A configured tmux-style prefix chord (`[keys] prefix` in config.toml —
/// see [`crate::config::KeysConfig`]). Parsing lives here rather than in
/// `crate::config` because the config module only validates as far as
/// "is it a string"; this is where "is it a chord" gets decided, lenient by
/// the same design as `RelayMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixKey {
    code: KeyCode,
    mods: Modifiers,
}

impl PrefixKey {
    /// `"C-<char>"` for a Control chord, or a bare single character for an
    /// unmodified key. Anything else (empty, multi-character, malformed
    /// `"C-"` forms) falls back to the default (`C-b`) with no error.
    pub fn parse(raw: &str) -> Self {
        if let Some(rest) = raw.strip_prefix("C-") {
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                return Self {
                    code: KeyCode::Char(c),
                    mods: Modifiers::CONTROL,
                };
            }
            return Self::default();
        }
        let mut chars = raw.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Self {
                code: KeyCode::Char(c),
                mods: Modifiers::NONE,
            };
        }
        Self::default()
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.mods
    }

    /// The prefix's own key event — what "send the prefix through literally"
    /// (prefix-prefix, or plain `b`) actually forwards, encoded the same way
    /// any other keypress would be (see [`key_to_bytes`]).
    fn as_key_event(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.mods)
    }
}

impl Default for PrefixKey {
    fn default() -> Self {
        Self {
            code: KeyCode::Char('b'),
            mods: Modifiers::CONTROL,
        }
    }
}

/// An arrow direction resolved while the prefix is armed (see
/// [`PrefixAction::Move`] and [`arrow_target`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// What an armed prefix's follow-up key resolves to (see [`update_key`]'s
/// prefix handling). Pure decision surface, no `KeyEvent`/`Stage` needed
/// once resolved — the unit-test target for the whole prefix feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixAction {
    /// The prefix chord again, or plain `b`: send the prefix's own byte
    /// through to the child literally (the tmux "send-prefix" convention).
    Literal,
    /// `o` or Tab: step forward through the focus ring (sidebar, then each
    /// staged pane in order), wrapping — see [`cycle_forward`].
    CyclePane,
    /// An arrow key: move by grid geometry — see [`arrow_target`].
    Move(Direction),
    /// `1`-`9`, in range: focus the pane at this 0-based index. This
    /// addresses `Stage`'s pane list directly (`1` is always `panes[0]`,
    /// the director) and is deliberately untouched by the focus ring's
    /// sidebar slot — ring position and pane number are different axes,
    /// don't conflate them when touching this again.
    FocusPane(usize),
    /// `1`-`9`, but the staged brigade doesn't have that many panes.
    OutOfRange,
    /// `s` or Esc: return focus to the sidebar.
    Sidebar,
    /// `x`: open the kill-confirm dialog for the focused pane.
    Kill,
    /// Anything else — including any of the plain-char bindings above with
    /// a modifier held (a Ctrl/Alt-mangled `o`/`s`/`x`/digit is never a
    /// binding, only ever noise) — swallowed, not forwarded, since a
    /// fat-fingered prefix command must never leak into the child as a raw
    /// keypress.
    Unbound,
}

/// Resolve one key pressed while the prefix is armed. `pane_count` is the
/// staged brigade's pane count (1 for `Solo`, the lone pane; 0 for `Empty`,
/// where digit-focus and pane-cycling are meaningless but still resolved
/// consistently — a `1` on a solo stage is simply out of range, not a
/// special case).
fn resolve_prefix_key(key: &KeyEvent, prefix: &PrefixKey, pane_count: usize) -> PrefixAction {
    if prefix.matches(key) || (key.code == KeyCode::Char('b') && key.modifiers.is_empty()) {
        return PrefixAction::Literal;
    }
    match key.code {
        KeyCode::Tab => PrefixAction::CyclePane,
        KeyCode::Char('o') if key.modifiers.is_empty() => PrefixAction::CyclePane,
        KeyCode::Left => PrefixAction::Move(Direction::Left),
        KeyCode::Right => PrefixAction::Move(Direction::Right),
        KeyCode::Up => PrefixAction::Move(Direction::Up),
        KeyCode::Down => PrefixAction::Move(Direction::Down),
        KeyCode::Char(c) if key.modifiers.is_empty() && c.is_ascii_digit() && c != '0' => {
            let n = c.to_digit(10).expect("ascii digit") as usize;
            if n <= pane_count {
                PrefixAction::FocusPane(n - 1)
            } else {
                PrefixAction::OutOfRange
            }
        }
        KeyCode::Esc => PrefixAction::Sidebar,
        KeyCode::Char('s') if key.modifiers.is_empty() => PrefixAction::Sidebar,
        KeyCode::Char('x') if key.modifiers.is_empty() => PrefixAction::Kill,
        _ => PrefixAction::Unbound,
    }
}

/// A position in the arrow/cycle focus ring: the sidebar, or a 0-based pane
/// index into the staged brigade (`0` is the director/solo pane, `1..` are
/// workers in stack order). Distinct from [`PrefixAction::FocusPane`], which
/// addresses panes directly and never includes the sidebar — see that
/// variant's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusSlot {
    Sidebar,
    Pane(usize),
}

/// Where the ring currently sits, derived from `state`'s existing
/// `focus`/`stage` rather than tracked separately.
fn current_focus_slot(state: &EmporiumState) -> FocusSlot {
    match state.focus {
        Focus::Sidebar => FocusSlot::Sidebar,
        Focus::Pane => match &state.stage {
            Stage::Brigade { focused, .. } => FocusSlot::Pane(*focused),
            // `Solo`/`Empty` have no numeric pane index; `Pane(0)` is the
            // only slot `Focus::Pane` can mean for either.
            Stage::Solo(_) | Stage::Empty => FocusSlot::Pane(0),
        },
    }
}

fn apply_focus_slot(state: &mut EmporiumState, slot: FocusSlot) {
    match slot {
        FocusSlot::Sidebar => state.focus = Focus::Sidebar,
        FocusSlot::Pane(n) => {
            state.focus = Focus::Pane;
            if let Stage::Brigade { focused, .. } = &mut state.stage {
                *focused = n;
            }
        }
    }
}

/// `o`/Tab: the next slot forward in the ring `[Sidebar, Pane(0), Pane(1),
/// ...]` (length `1 + pane_count`), wrapping. A ring of length 1 (`Empty`,
/// `pane_count == 0`) has nowhere else to go and stays put.
fn cycle_forward(from: FocusSlot, pane_count: usize) -> FocusSlot {
    let ring_len = 1 + pane_count;
    if ring_len <= 1 {
        return FocusSlot::Sidebar;
    }
    let pos = match from {
        FocusSlot::Sidebar => 0,
        FocusSlot::Pane(n) => 1 + n,
    };
    let next = (pos + 1) % ring_len;
    if next == 0 {
        FocusSlot::Sidebar
    } else {
        FocusSlot::Pane(next - 1)
    }
}

/// One armed arrow key's target slot, navigating the three-column grid
/// (sidebar | director-or-solo | worker stack) by geometry rather than ring
/// order. Left/Right cross columns; Up/Down step within the worker stack
/// only, clamped (no wrap). Every edge case (sidebar's Left, a solo/director
/// pane's Up/Down, a worker's Right) is a deliberate no-op, not an omission.
fn arrow_target(from: FocusSlot, direction: Direction, pane_count: usize) -> FocusSlot {
    match (from, direction) {
        (FocusSlot::Sidebar, Direction::Right) => {
            if pane_count == 0 {
                FocusSlot::Sidebar
            } else {
                FocusSlot::Pane(0)
            }
        }
        (FocusSlot::Sidebar, _) => FocusSlot::Sidebar,

        (FocusSlot::Pane(0), Direction::Left) => FocusSlot::Sidebar,
        (FocusSlot::Pane(0), Direction::Right) => {
            if pane_count >= 2 {
                FocusSlot::Pane(1)
            } else {
                FocusSlot::Pane(0)
            }
        }
        (FocusSlot::Pane(0), Direction::Up | Direction::Down) => FocusSlot::Pane(0),

        (FocusSlot::Pane(_), Direction::Left) => FocusSlot::Pane(0),
        (FocusSlot::Pane(n), Direction::Right) => FocusSlot::Pane(n),
        (FocusSlot::Pane(n), Direction::Up) => {
            if n > 1 {
                FocusSlot::Pane(n - 1)
            } else {
                FocusSlot::Pane(n)
            }
        }
        (FocusSlot::Pane(n), Direction::Down) => {
            if n + 1 < pane_count {
                FocusSlot::Pane(n + 1)
            } else {
                FocusSlot::Pane(n)
            }
        }
    }
}

/// A store operation the shell executes, reusing the store's existing
/// transactional functions — an intent, not a SQL statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoreIntent {
    SetPin {
        id: String,
        pinned: bool,
    },
    Archive {
        id: String,
        title: String,
    },
    JoinGroup {
        session_id: String,
        target: GroupJoinTargetData,
    },
    ResolveMembership {
        session_id: String,
    },
    FormBrigade {
        director_row_id: String,
        name: String,
        cwd: PathBuf,
        worker_count: usize,
        /// Resolved once, at the press-time moment the Director's own row
        /// (and therefore its product) is known — see [`resolve_worker_agent`]
        /// — and carried through this whole round trip the same way `cwd`
        /// already is, rather than re-resolved later from a `[brigade]`
        /// setting that could theoretically have changed mid-flight.
        /// `[brigade] worker_agent = "select"` resolves here to whatever the
        /// operator chose in `Modal::WorkerAgentPicker`, not `Inherit`'s
        /// value — see `confirm_worker_agent_modal`.
        worker_agent: AgentKind,
        /// The `--model` every fresh Worker this formation spawns launches
        /// with, empty meaning "no `--model` flag at all" — resolved
        /// alongside `worker_agent`, from `BrigadeConfig::worker_model_for`
        /// or (the `Select` picker) the operator's own edited text, so
        /// nothing downstream needs `&BrigadeConfig` just to look this up
        /// again.
        worker_model: String,
    },
    AddWorker {
        brigade_id: BrigadeId,
        cwd: PathBuf,
        /// Same reasoning as `FormBrigade`'s own `worker_agent`: resolved
        /// once, at the `B`-press moment, in [`add_worker`] — either
        /// directly, or (`[brigade] worker_agent = "select"`) via the same
        /// `Modal::WorkerAgentPicker` formation uses, see
        /// [`EmporiumState::pending_worker_addition`]. Unlike formation, a
        /// `Select`-resolved `AgentKind::Codex` here never routes through
        /// the codex-trust check — see `confirm_worker_agent_modal`'s own
        /// doc for why that's an intentional difference, not a gap this
        /// round left unclosed.
        worker_agent: AgentKind,
        /// Same reasoning as `FormBrigade`'s own `worker_model`.
        worker_model: String,
    },
    Disband {
        brigade_id: BrigadeId,
    },
    /// Remove one Worker from its brigade for good (the emporium's
    /// prefix-`x` "dismiss" choice) — membership, cursor, and any mail
    /// addressed specifically to it, all gone (`Store::dismiss_worker`).
    /// `(brigade_id, token)` are already known by the time this is built —
    /// see `confirm_kill_modal`'s dismiss path and
    /// `PendingMembership::DismissWorker`'s doc for how.
    DismissWorker {
        brigade_id: BrigadeId,
        token: MemberToken,
    },
    SetMemberSession {
        brigade_id: BrigadeId,
        token: MemberToken,
        session_id: String,
    },
}

/// Mirrors `crate::app::GroupJoinTarget`, but by value (the original borrows
/// from the modal state, which `update` cannot hold across the round trip to
/// the shell and back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupJoinTargetData {
    Existing(i64, String),
    New(String),
}

/// An instruction for the shell to execute — plain data, never executed
/// here. Derives `Serialize`/`Deserialize` alongside `Event` even though
/// only `Event` is ever written to a record/replay stream (`crate::replay`,
/// `docs/DISCIPLINE.md` §8) — the discipline's own §5.5 treats a `Cmd`
/// history as "comparable and snapshotable wholesale", which needs
/// `PartialEq`/`Eq` (and `Serialize` makes that snapshotting JSON-able too,
/// for free).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    WritePty {
        key: SessionKey,
        bytes: Vec<u8>,
    },
    ResizePty {
        key: SessionKey,
        rows: u16,
        cols: u16,
    },
    /// Stat `cwd` (the new-session modal's target when Enter was pressed) to
    /// confirm it's a real directory before launching into it —
    /// `Path::is_dir()` is filesystem I/O, which this crate may never do
    /// itself (`docs/DISCIPLINE.md` §3's "no file reads or writes"). The
    /// shell answers with `Event::NewSessionCwdChecked`. See
    /// `confirm_new_session_modal`'s doc for the full round trip, including
    /// why a stale answer must not be trusted blindly.
    CheckNewSessionCwd {
        cwd: PathBuf,
    },
    /// Spawn (or, if already running elsewhere, refuse) `target` under
    /// `key`. `brigade` wires the launch to banto's own MCP server; `model`
    /// is `--model <model>` for a freshly-spawned Worker (never set for a
    /// resume — the model was already fixed at the session's original
    /// launch). The shell answers with `Event::Spawned`/`SpawnFailed`.
    OpenEmbedded {
        key: SessionKey,
        target: SessionToOpen,
        brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
        model: Option<String>,
    },
    /// Kill the child at `key` — active termination (prefix-`x` confirm, or
    /// a disbanded brigade's Workers). The passive `Event::PtyExited` fold
    /// is what actually cleans up the pane once this takes effect; this Cmd
    /// is only ever "make the exit happen".
    KillPty {
        key: SessionKey,
    },
    /// The child hosted under `from` is now known by `to`: id discovery
    /// resolved a freshly-launched session's synthetic placeholder key into
    /// the real session id (`Event::DiscoveryResult`). The core
    /// renames its own `screens`/`Stage` entries itself; this Cmd is how the
    /// shell's PTY handle — which the core cannot touch — follows along.
    /// Without it the handle is orphaned under a key nothing references
    /// again: the pane stops accepting input and stops being reaped-safe,
    /// and the shell's "which ids are already open" set never learns the
    /// discovered id, so the next pending discovery resolves onto it a
    /// second time.
    RekeyPty {
        from: SessionKey,
        to: SessionKey,
    },
    /// Read whether banto's own `SessionStart` hook looks trusted right now
    /// (fresh, not cached — the operator may have approved it in a pane
    /// since this run started) and whether `std::env::current_exe()` can
    /// even launch it at all. Issued only when a brigade formation's already-
    /// resolved Worker product is [`AgentKind::Codex`] — see
    /// `update_membership_resolved`/`confirm_worker_agent_modal`, which stash
    /// a [`PendingCodexTrustFormation`] alongside it. The shell answers with
    /// `Event::CodexTrustChecked`.
    CheckCodexTrust,
    /// Open a solo pane running Codex's own trust-review startup (`codex -c
    /// <hook override>`, via `crate::codex_trust::trust_argv` — no cwd, no
    /// resume, no MCP overrides, nothing else) under `key`. Confirming
    /// [`Modal::ConfirmCodexTrust`] issues this; the shell answers with the
    /// same `Event::Spawned`/`SpawnFailed` any other open does, but this one
    /// is never handed to `Cmd::OpenEmbedded`/discovery — it's a throwaway
    /// review session the operator `/quit`s out of, not one banto should
    /// ever show in the sidebar.
    OpenCodexTrustPane {
        key: SessionKey,
    },
    /// Read whether Codex has already been told to trust `cwd` — a
    /// per-directory record, unrelated to `Cmd::CheckCodexTrust`'s
    /// machine-wide hook-trust one — before typing
    /// [`CODEX_WORKER_KICKOFF_LINE`] into `key`'s pane blind. Issued by
    /// [`update_tick`] once a [`PendingKickoff`]'s quiet period has elapsed,
    /// every tick, until the answer comes back trusted: a directory Codex
    /// has never seen shows its own first-run trust prompt on stdin/stdout,
    /// exactly where the kickoff line would otherwise land — measured
    /// (worker-3's report) to block on that prompt the same way an
    /// unanswered approval always has, with no flag banto already passes
    /// dismissing it. The shell answers with
    /// `Event::WorkerDirectoryTrustChecked`.
    CheckWorkerDirectoryTrust {
        key: SessionKey,
        cwd: PathBuf,
    },
    /// Re-emit a child's OSC 52 clipboard payload on banto's own stdout, so
    /// the real terminal banto is running in performs the copy — banto is a
    /// middleman here, not the clipboard owner (`crate::screen::Screen::
    /// take_clipboard`'s doc). `bytes` is the complete, already
    /// selector/alphabet/size-validated OSC 52 escape sequence
    /// ([`build_clipboard_forward`]), ready to write verbatim; the shell
    /// must not reinterpret or re-encode it.
    ForwardClipboardToHost {
        bytes: Vec<u8>,
    },
    Store(StoreIntent),
    Reload,
}

/// A fact about the outside world, fed into [`update`]. Derives
/// `Serialize`/`Deserialize`: this is the one type a record/replay stream
/// (`crate::replay`, `docs/DISCIPLINE.md` §8) actually writes to and reads
/// from disk — one JSON object per line, `{offset_ms, event}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Input(InputEvent),
    Resized {
        width: u16,
        height: u16,
    },
    PtyOutput {
        key: SessionKey,
        chunk: Vec<u8>,
    },
    PtyExited {
        key: SessionKey,
    },
    /// The shell's answer to `Cmd::CheckNewSessionCwd` — see
    /// `update_new_session_cwd_checked`'s doc for why `cwd` is echoed back
    /// and how a stale answer is handled.
    NewSessionCwdChecked {
        cwd: PathBuf,
        is_dir: bool,
    },
    Spawned {
        key: SessionKey,
    },
    SpawnFailed {
        key: SessionKey,
        error: String,
    },
    RowsLoaded {
        rows: Vec<SessionRow>,
        hidden: HashSet<String>,
        directors: HashSet<String>,
        /// Session ids with a known auto-compaction continuation (see
        /// `App::superseded`). `#[serde(default)]` so a record/replay
        /// stream captured before this field existed still deserializes.
        #[serde(default)]
        superseded: HashSet<String>,
    },
    DiscoveryResult {
        key: SessionKey,
        session_id: String,
        member: Option<(BrigadeId, MemberToken)>,
    },
    /// The shell's Codex-sourced discovery tracker for `key` gave up: too
    /// long since spawn with `BrigadeMember::briefed_session_id` still
    /// `None` (see `docs/notes/codex-briefing-spike.md`'s silent-failure
    /// list — an untrusted hook, a kickoff line that never got typed, and
    /// others all look identical from here: nothing ever arrives). Unlike a
    /// resolved [`Self::DiscoveryResult`], this never rekeys anything — the
    /// pane stays parked under its synthetic key, same degraded-but-visible
    /// outcome as an unresolved Claude discovery today, just with a status
    /// line explaining why instead of silence.
    CodexWorkerDiscoveryTimedOut {
        key: SessionKey,
        token: MemberToken,
    },
    /// The shell's Claude-sourced discovery tracker for `token` still hasn't
    /// resolved, and its own cwd definitively reads `NotTrusted` (see
    /// `banto_io::directory_trust::claude_directory_trust` — narrower than
    /// `Unknown`-also-counts on purpose, `poll_discovery`'s own doc has the
    /// reasoning) — reported once per tracker (the shell's own dedup,
    /// `DiscoveryTracker::notified_untrusted`), so this pane's silence gets
    /// explained instead of looking broken.
    /// Unlike [`Self::CodexWorkerDiscoveryTimedOut`], there is no keystroke
    /// to hold back here (Claude is never typed into by banto at all) and no
    /// give-up timeout either — Claude discovery still waits forever,
    /// unchanged; this only ever adds a one-time status line on top of that
    /// wait.
    ClaudeWorkerDirectoryUntrusted {
        token: MemberToken,
    },
    ArchiveDone {
        title: String,
        result: Result<(), String>,
    },
    GroupJoinDone {
        session_id: String,
        result: Result<(i64, String), String>,
    },
    MembershipResolved {
        session_id: String,
        membership: Option<(BrigadeId, MemberToken, BrigadeRole)>,
        /// The resolved brigade's full membership (token, role, Claude
        /// session id if known) — `Some` whenever `membership` is `Some`,
        /// bundled into the same shell read rather than a second round
        /// trip (`stage_brigade` needs the whole roster, not just the
        /// activating row's own membership).
        members: Option<Vec<(MemberToken, BrigadeRole, Option<String>)>>,
    },
    /// Answers `Cmd::CheckCodexTrust`: whether banto's `SessionStart` hook
    /// looks trusted right now, and whether it could even fire from this
    /// executable's path — see [`update_codex_trust_checked`] for how the
    /// two combine with the [`PendingCodexTrustFormation`] this always
    /// follows.
    CodexTrustChecked {
        primed: bool,
        hook_launchable: bool,
    },
    /// Answers `Cmd::CheckWorkerDirectoryTrust`: whether Codex has been told
    /// to trust `key`'s pane's own cwd. `trusted` is already collapsed to a
    /// plain bool by the shell — `banto_io::directory_trust::DirectoryTrust`'s
    /// `NotTrusted` and `Unknown` both mean "don't type into this pane yet"
    /// here, so there is nothing a third state would let
    /// [`update_worker_directory_trust_checked`] do differently.
    WorkerDirectoryTrustChecked {
        key: SessionKey,
        trusted: bool,
    },
    BrigadeFormed {
        director_row_id: String,
        name: String,
        cwd: PathBuf,
        /// Echoed straight through from `StoreIntent::FormBrigade` — see
        /// its own doc.
        worker_agent: AgentKind,
        /// Echoed straight through from `StoreIntent::FormBrigade` — see
        /// its own doc.
        worker_model: String,
        result: Result<(BrigadeId, Vec<MemberToken>), String>,
    },
    WorkerAdded {
        brigade_id: BrigadeId,
        cwd: PathBuf,
        /// Echoed straight through from `StoreIntent::AddWorker` — see its
        /// own doc.
        worker_agent: AgentKind,
        /// Echoed straight through from `StoreIntent::AddWorker` — see its
        /// own doc.
        worker_model: String,
        result: Result<MemberToken, String>,
    },
    Disbanded {
        brigade_id: BrigadeId,
        result: Result<(HashSet<String>, HashSet<String>), String>,
    },
    /// `StoreIntent::DismissWorker` completed. Same result shape as
    /// [`Self::Disbanded`] (refreshed hidden/director sets on success) —
    /// which pane to remove is not carried here at all; see
    /// `EmporiumState::pending_dismiss`.
    WorkerDismissed {
        brigade_id: BrigadeId,
        result: Result<(HashSet<String>, HashSet<String>), String>,
    },
    MemberSessionRecorded {
        hidden: HashSet<String>,
        directors: HashSet<String>,
    },
    /// A staged member's Claude session forked in place: AUTO-compaction
    /// assigns a *new* session id to the same live process (manual
    /// `/compact` does not) — the process's own `sessions/<pid>.json` live
    /// file simply starts reporting a different `sessionId`, still under
    /// the same pid. `old_id` is what the store/pane still know the member
    /// by; `new_id` is what the process now reports. Gathered shell-side
    /// (`emporium::gather_fork_observations`) alongside the relay tick, one
    /// per tick a staged member's recorded id disagrees with its own live
    /// process — so this may repeat for the same fork until the store row
    /// (and the pane it keys) actually catch up; the handler must tolerate
    /// that.
    MemberSessionForked {
        brigade_id: BrigadeId,
        token: MemberToken,
        old_id: String,
        new_id: String,
    },
    /// ~1/s: relay observations for the staged brigade's members (gathered
    /// shell-side — store + live-session reads), plus the trigger to flush
    /// any due phase-two nudge submit and expire the status message.
    Tick {
        relay: Vec<RelayObservation>,
    },
    /// banto's own process gained or lost OS focus (crossterm's
    /// `FocusGained`/`FocusLost`, reported only once `EnableFocusChange` is
    /// on) — distinct from *which pane* has focus within banto, tracked
    /// separately by [`Focus`]. A child that asked for DECSET 1004 cannot
    /// tell its terminal-within-a-terminal apart from a real one, so this
    /// and an ordinary pane-to-pane switch both resolve through the same
    /// [`emit_focus_transitions`] diff at the end of [`update`] — see
    /// [`effective_focused_pane`]'s doc for how the two compose.
    WindowFocusChanged {
        focused: bool,
    },
}

/// The core: a pure function from one [`Event`] to state mutations and
/// [`Cmd`]s. No clock reads (`now` is the only time), no I/O of any kind.
pub fn update(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    ev: Event,
    now: Instant,
) -> Vec<Cmd> {
    let mut cmds = match ev {
        Event::Input(input) => update_input(state, app, brigade, input, now),
        Event::Resized { width, height } => update_resized(state, app, width, height),
        Event::PtyOutput { key, chunk } => {
            state.last_output_at.insert(key.clone(), now);
            let mut cmds = Vec::new();
            if let Some(screen) = state.screens.get_mut(&key) {
                screen.process(&chunk);
                // Drained right after the chunk that staged it is processed
                // (Screen::take_clipboard's own doc: it's one-shot, so the
                // same copy must never be forwarded again on a later poll).
                if let Some((selection, data)) = screen.take_clipboard() {
                    match build_clipboard_forward(&selection, &data) {
                        Some(bytes) => cmds.push(Cmd::ForwardClipboardToHost { bytes }),
                        None => state.set_status(
                            format!(
                                "{}: a clipboard copy was too large or malformed to forward",
                                key.as_str()
                            ),
                            now,
                        ),
                    }
                }
            }
            cmds
        }
        Event::PtyExited { key } => update_pty_exited(state, app, brigade, key, now),
        Event::NewSessionCwdChecked { cwd, is_dir } => {
            update_new_session_cwd_checked(state, app, cwd, is_dir)
        }
        Event::Spawned { key } => update_spawned(state, key, now),
        Event::SpawnFailed { key, error } => update_spawn_failed(state, key, error, now),
        Event::RowsLoaded {
            rows,
            hidden,
            directors,
            superseded,
        } => {
            app.replace_rows(rows);
            app.set_hidden_worker_ids(hidden);
            app.set_directors(directors);
            app.set_superseded(superseded);
            Vec::new()
        }
        Event::DiscoveryResult {
            key,
            session_id,
            member,
        } => update_discovery_result(state, key, session_id, member, now),
        Event::CodexWorkerDiscoveryTimedOut { key, token } => {
            state.pending_kickoffs.retain(|pending| pending.key != key);
            state.set_status(format!("{token}: Codex briefing wasn't confirmed"), now);
            Vec::new()
        }
        Event::ClaudeWorkerDirectoryUntrusted { token } => {
            state.set_status(
                format!(
                    "{token}: Claude hasn't been told to trust this directory yet — \
                     answer its prompt in the pane"
                ),
                now,
            );
            Vec::new()
        }
        Event::ArchiveDone { title, result } => {
            state.set_status(
                match &result {
                    Ok(()) => format!("archived {title}"),
                    Err(err) => format!("failed to archive {title}: {err}"),
                },
                now,
            );
            vec![Cmd::Reload]
        }
        Event::GroupJoinDone { session_id, result } => {
            match result {
                Ok((group_id, group_name)) => {
                    state.set_status(format!("joined group \"{group_name}\""), now);
                    app.set_session_group_cache(&session_id, group_id, group_name);
                }
                Err(err) => state.set_status(format!("failed to join group: {err}"), now),
            }
            Vec::new()
        }
        Event::MembershipResolved {
            session_id,
            membership,
            members,
        } => update_membership_resolved(state, app, brigade, session_id, membership, members),
        Event::CodexTrustChecked {
            primed,
            hook_launchable,
        } => update_codex_trust_checked(state, app, primed, hook_launchable, now),
        Event::WorkerDirectoryTrustChecked { key, trusted } => {
            update_worker_directory_trust_checked(state, key, trusted, now)
        }
        Event::BrigadeFormed {
            director_row_id,
            name,
            cwd,
            worker_agent,
            worker_model,
            result,
        } => update_brigade_formed(
            state,
            app,
            FormedBrigade {
                director_row_id,
                name,
                cwd,
                worker_agent,
                worker_model,
            },
            result,
            now,
        ),
        Event::WorkerAdded {
            brigade_id,
            cwd,
            worker_agent,
            worker_model,
            result,
        } => update_worker_added(
            state,
            brigade_id,
            cwd,
            worker_agent,
            worker_model,
            result,
            now,
        ),
        Event::Disbanded { brigade_id, result } => {
            update_disbanded(state, app, brigade_id, result, now)
        }
        Event::WorkerDismissed { brigade_id, result } => {
            update_worker_dismissed(state, app, brigade_id, result, now)
        }
        Event::MemberSessionRecorded { hidden, directors } => {
            app.set_hidden_worker_ids(hidden);
            app.set_directors(directors);
            Vec::new()
        }
        Event::MemberSessionForked {
            brigade_id,
            token,
            old_id,
            new_id,
        } => update_member_session_forked(state, brigade_id, token, old_id, new_id, now),
        Event::Tick { relay } => update_tick(state, brigade, relay, now),
        Event::WindowFocusChanged { focused } => {
            state.window_focused = focused;
            Vec::new()
        }
    };
    cmds.extend(resize_staged_tiles(state));
    cmds.extend(emit_focus_transitions(state));
    cmds
}

/// Resize every currently-staged tile's `Screen` to match the current
/// layout, emitting a `Cmd::ResizePty` only for the ones that actually
/// changed (matches the pre-migration per-tick unconditional resize, whose
/// dedup lived inside `EmbeddedSession::resize` — moved here, now pure).
/// Called once at the end of every `update` regardless of event kind: cheap
/// (a HashMap lookup per staged tile) and correct without needing every
/// stage-mutating branch to remember to call it.
fn resize_staged_tiles(state: &mut EmporiumState) -> Vec<Cmd> {
    let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
    let mut cmds = Vec::new();
    for (key, rect) in stage_tiles(areas.pane, &state.stage) {
        let content = pane_content(rect);
        if let Some(screen) = state.screens.get_mut(&key)
            && screen.resize(content.height, content.width)
        {
            cmds.push(Cmd::ResizePty {
                key,
                rows: content.height,
                cols: content.width,
            });
        }
    }
    cmds
}

/// Whether a pane currently reads as focused from a hosted child's own point
/// of view — both banto's internal [`Focus`] needing to be on the pane
/// region *and* banto's own process needing OS focus. A child cannot tell
/// its terminal-within-a-terminal apart from a real one: if the operator
/// alt-tabs away from banto entirely, the pane they were typing into is, from
/// that pane's perspective, exactly as unfocused as if the operator had
/// switched to the sidebar instead. Collapsing both conditions into one
/// optional key is what lets [`emit_focus_transitions`] answer both halves
/// of DECSET 1004 with a single diff instead of two.
fn effective_focused_pane(state: &EmporiumState) -> Option<SessionKey> {
    if state.focus == Focus::Pane && state.window_focused {
        state.stage.focused_key().cloned()
    } else {
        None
    }
}

/// `CSI O` / `CSI I` for whichever pane's effective focus actually changed
/// since the last call, sent only to a pane that asked for it (DECSET 1004
/// — [`crate::screen::Screen::wants_focus_events`]). Called unconditionally
/// at the end of [`update`] (alongside [`resize_staged_tiles`], same
/// reasoning): cheap, and correct without a pane switch, a pane closing, the
/// sidebar taking focus, and banto's own window losing OS focus each needing
/// their own call site to remember this.
///
/// Deliberately does *not* short-circuit on "the focused key hasn't
/// changed": a pane almost always gains focus (on open, or by being clicked)
/// *before* its own output has had a chance to enable DECSET 1004 — Claude
/// Code's own 1004 arrives a few bytes into its startup sequence, not before
/// it. A version of this that recorded the pane as "notified" the first time
/// it was checked, regardless of whether `wants_focus_events` was true yet,
/// would compare that stale `true`-if-still-focused key against an unchanged
/// `now_focused` forever after and never revisit it — the exact shape of a
/// child that asked for focus events and then waited for a `CSI I` that had
/// already silently not been sent. So [`EmporiumState::last_focus_notified`]
/// only ever records a key once a `CSI I` was *actually* sent to it, which
/// means the pair below re-checks "does the currently-focused pane still
/// need telling" on every single call — cheap (two `HashMap` lookups), and
/// what makes a late `wants_focus_events` catch up on the very next `update`
/// instead of missing its window forever.
fn emit_focus_transitions(state: &mut EmporiumState) -> Vec<Cmd> {
    let now_focused = effective_focused_pane(state);
    let mut cmds = Vec::new();

    if state.last_focus_notified != now_focused
        && let Some(old) = state.last_focus_notified.take()
        && state
            .screens
            .get(&old)
            .is_some_and(crate::screen::Screen::wants_focus_events)
    {
        cmds.push(Cmd::WritePty {
            key: old,
            bytes: b"\x1b[O".to_vec(),
        });
    }

    if let Some(new) = &now_focused
        && state.last_focus_notified.as_ref() != Some(new)
        && state
            .screens
            .get(new)
            .is_some_and(crate::screen::Screen::wants_focus_events)
    {
        cmds.push(Cmd::WritePty {
            key: new.clone(),
            bytes: b"\x1b[I".to_vec(),
        });
        state.last_focus_notified = Some(new.clone());
    }

    cmds
}

/// The whole forwarded OSC 52 sequence — header, selector, base64 body,
/// terminator — must not exceed this many bytes. Real terminals disagree
/// wildly on their own tolerance: VTE-based terminals (GNOME Terminal and
/// its relatives) hard-cap a single OSC string at 4096 codepoints and
/// silently drop anything longer, while Windows Terminal has been measured
/// comfortable past 100 MiB. 100,000 is the number that actually converges
/// across the ecosystem rather than picking a side: it is the de facto
/// informal ceiling several independent tools already assume (Codex's own
/// OSC 52 clipboard fallback, `clipboard_copy.rs`'s `OSC52_MAX_RAW_BYTES`,
/// bounds its raw payload the same order of magnitude), and it's the default
/// Windows Terminal's own maintainers discussed when they added support.
/// Bounding here — not just trusting `vt100-psmux`'s own parse-time
/// filtering — is deliberate: forwarding to the host terminal is a new trust
/// boundary this crate is choosing to cross (arbitrary child output now
/// reaching the operator's *real* terminal), not a restatement of the
/// parser's own.
const MAX_OSC52_SEQUENCE_LEN: usize = 100_000;

/// The xterm-family OSC 52 selection alphabet: `c`/`p`/`q`/`s`
/// (clipboard/primary/secondary/select) and cut buffers `0`-`7`. Matches
/// `vt100-psmux`'s own `CLIPBOARD_SELECTOR` (`crates/vt100-psmux/src/
/// perform.rs`) byte for byte, checked independently rather than trusted —
/// see [`MAX_OSC52_SEQUENCE_LEN`]'s doc for why.
const CLIPBOARD_SELECTORS: &[u8] = b"cpqs01234567";

/// Base64 alphabet plus `=` padding, matching `vt100-psmux`'s own `BASE64`
/// constant byte for byte — same independent-check reasoning as
/// [`CLIPBOARD_SELECTORS`].
const BASE64_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";

/// Build the literal OSC 52 sequence to forward to the host terminal for a
/// clipboard payload a child staged (`Screen::take_clipboard`), or `None` if
/// it must not be forwarded. Two distinct ways to fail: `selection` names
/// something outside [`CLIPBOARD_SELECTORS`], or the fully-assembled
/// sequence would exceed [`MAX_OSC52_SEQUENCE_LEN`]. Both are silent drops
/// rather than truncation — truncating a base64 body does not shorten the
/// copied text, it corrupts it into different bytes that either fail to
/// decode or decode to something the child never asked to put on the
/// clipboard, which is worse than forwarding nothing.
fn build_clipboard_forward(selection: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if selection.is_empty() || !selection.iter().all(|b| CLIPBOARD_SELECTORS.contains(b)) {
        return None;
    }
    if data.is_empty() || !data.iter().all(|b| BASE64_ALPHABET.contains(b)) {
        return None;
    }
    let mut bytes = Vec::with_capacity(7 + selection.len() + data.len());
    bytes.extend_from_slice(b"\x1b]52;");
    bytes.extend_from_slice(selection);
    bytes.push(b';');
    bytes.extend_from_slice(data);
    bytes.push(0x07);
    (bytes.len() <= MAX_OSC52_SEQUENCE_LEN).then_some(bytes)
}

fn update_resized(state: &mut EmporiumState, app: &mut App, width: u16, height: u16) -> Vec<Cmd> {
    state.size = (width, height);
    let areas = layout(Rect::new(0, 0, width, height));
    app.set_viewport_height(areas.sidebar.height.saturating_sub(2) as usize);
    Vec::new()
}

fn update_input(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    input: InputEvent,
    now: Instant,
) -> Vec<Cmd> {
    match input {
        // The press/repeat-vs-release filter the terminal backend's raw key
        // event kind would need lives at the shell boundary now
        // (`embedded::convert`) — `InputEvent::Key` only exists at all once
        // that's already decided, so every `Key` reaching here is one to
        // act on.
        InputEvent::Key(key) => update_key(state, app, brigade, key, now),
        InputEvent::Mouse(mouse) => update_mouse(state, app, brigade, mouse, now),
        InputEvent::Paste(text) => update_paste(state, app, text, now),
        InputEvent::Resize { .. } => Vec::new(),
    }
}

/// Dispatch one key press.
fn update_key(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    key: KeyEvent,
    now: Instant,
) -> Vec<Cmd> {
    let code = key.code;

    if app.modal().is_some() {
        return update_modal_key(state, app, code);
    }
    if app.mode() == Mode::Search {
        update_search_key(app, code);
        return Vec::new();
    }
    if state.prefix_armed.is_some() {
        return resolve_armed_prefix(state, app, key, now);
    }
    // The prefix chord arms instead of dispatching normally — from either
    // focus, not just `Focus::Pane` (tmux's prefix always arms, whichever
    // pane/window has keyboard focus) — a tmux-style pane command follows
    // (see `resolve_armed_prefix`), costing a double-tap for a literal
    // prefix byte through, same as tmux. Checked before F2/F3 and the
    // per-focus dispatch below so it always wins; a configured prefix that
    // collides with another binding is the user's choice (`[keys] prefix`
    // is deliberately unvalidated against the rest of the keymap).
    if state.prefix.matches(&key) {
        state.prefix_armed = Some(now);
        return Vec::new();
    }
    if code == KeyCode::F(2) {
        state.focus = match state.focus {
            Focus::Sidebar if state.stage.is_active() => Focus::Pane,
            _ => Focus::Sidebar,
        };
        return Vec::new();
    }
    if code == KeyCode::F(3) {
        if let Stage::Brigade { panes, focused, .. } = &mut state.stage
            && !panes.is_empty()
        {
            *focused = (*focused + 1) % panes.len();
        }
        return Vec::new();
    }

    match state.focus {
        Focus::Pane => {
            if let Some(target) = state.stage.focused_key().cloned() {
                let bytes = key_to_bytes(&key);
                state.last_forwarded_input.insert(target.clone(), now);
                if bytes.is_empty() {
                    Vec::new()
                } else {
                    cancel_pending_submit_on_input(&mut state.pending_submits, &target);
                    vec![Cmd::WritePty { key: target, bytes }]
                }
            } else {
                Vec::new()
            }
        }
        Focus::Sidebar => {
            state.status = None;
            let mods = key.modifiers;
            // Every plain-char binding below fires only with no modifier
            // held (`'B'` alone is the exception: a shifted letter arrives
            // as the already-uppercased `Char` *plus* `SHIFT` set, not a
            // bare `Char('b')`) — a Ctrl/Alt-modified char is never one of
            // these bindings, just noise (e.g. the default prefix `C-b`
            // must never also fire `b`'s add-worker).
            match (code, mods) {
                (KeyCode::Char('q'), Modifiers::NONE) | (KeyCode::Esc, _) => {
                    app.request_quit();
                    Vec::new()
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), Modifiers::NONE) => {
                    app.select_prev();
                    Vec::new()
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), Modifiers::NONE) => {
                    app.select_next();
                    Vec::new()
                }
                (KeyCode::PageUp, _) => {
                    app.page_up();
                    Vec::new()
                }
                (KeyCode::PageDown, _) => {
                    app.page_down();
                    Vec::new()
                }
                (KeyCode::Home, _) => {
                    app.select_first();
                    Vec::new()
                }
                (KeyCode::End, _) => {
                    app.select_last();
                    Vec::new()
                }
                (KeyCode::Enter, _) => activate_selected(state, app),
                (KeyCode::Char('B'), Modifiers::NONE | Modifiers::SHIFT) => brigade_key(state, app),
                (KeyCode::Char('b'), Modifiers::NONE) => add_worker(state, app, brigade),
                (KeyCode::Tab, _) => {
                    app.toggle_grouped_view();
                    Vec::new()
                }
                (KeyCode::Char('/'), Modifiers::NONE) => {
                    app.enter_search();
                    Vec::new()
                }
                (KeyCode::Char('a'), Modifiers::NONE) => {
                    app.toggle_agent_filter();
                    Vec::new()
                }
                (KeyCode::Char('p'), Modifiers::NONE) => toggle_pin(app),
                (KeyCode::Char('d'), Modifiers::NONE) => {
                    app.open_confirm_archive_modal();
                    Vec::new()
                }
                (KeyCode::Char('g'), Modifiers::NONE) => {
                    app.open_group_join_modal();
                    Vec::new()
                }
                (KeyCode::Char('n'), Modifiers::NONE) => {
                    app.open_new_session_modal();
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
    }
}

/// The key following an armed prefix (see [`resolve_prefix_key`] for the
/// resolution table) — always disarms, regardless of what it resolves to.
fn resolve_armed_prefix(
    state: &mut EmporiumState,
    app: &mut App,
    key: KeyEvent,
    now: Instant,
) -> Vec<Cmd> {
    state.prefix_armed = None;
    let pane_count = match &state.stage {
        Stage::Brigade { panes, .. } => panes.len(),
        Stage::Solo(_) => 1,
        Stage::Empty => 0,
    };
    match resolve_prefix_key(&key, &state.prefix, pane_count) {
        PrefixAction::Literal => {
            let Some(target) = state.stage.focused_key().cloned() else {
                return Vec::new();
            };
            let bytes = key_to_bytes(&state.prefix.as_key_event());
            state.last_forwarded_input.insert(target.clone(), now);
            if bytes.is_empty() {
                Vec::new()
            } else {
                cancel_pending_submit_on_input(&mut state.pending_submits, &target);
                vec![Cmd::WritePty { key: target, bytes }]
            }
        }
        PrefixAction::CyclePane => {
            let next = cycle_forward(current_focus_slot(state), pane_count);
            apply_focus_slot(state, next);
            Vec::new()
        }
        PrefixAction::Move(direction) => {
            let next = arrow_target(current_focus_slot(state), direction, pane_count);
            apply_focus_slot(state, next);
            Vec::new()
        }
        PrefixAction::FocusPane(index) => {
            state.focus = Focus::Pane;
            if let Stage::Brigade { focused, .. } = &mut state.stage {
                *focused = index;
            }
            Vec::new()
        }
        PrefixAction::OutOfRange => {
            state.status = Some("prefix: no such pane".to_string());
            Vec::new()
        }
        PrefixAction::Sidebar => {
            state.focus = Focus::Sidebar;
            Vec::new()
        }
        PrefixAction::Kill => {
            let Some(target) = state.stage.focused_key().cloned() else {
                return Vec::new();
            };
            let title = app
                .row_for_id(target.as_str())
                .map(|row| row.display_title().to_string())
                .unwrap_or_else(|| target.as_str().to_string());
            // Director is always `panes[0]` (see `Stage::Brigade`'s doc), so
            // any other focused pane in a staged brigade is a Worker —
            // structural, no store round trip needed just to grow the
            // dialog its second choice.
            let is_worker = matches!(&state.stage, Stage::Brigade { focused, .. } if *focused != 0);
            app.open_confirm_kill_modal(target.as_str().to_string(), title, is_worker);
            Vec::new()
        }
        PrefixAction::Unbound => {
            state.status = Some("unbound prefix key".to_string());
            Vec::new()
        }
    }
}

fn update_search_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward(),
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home => app.move_cursor_home(),
        KeyCode::End => app.move_cursor_end(),
        KeyCode::Enter => app.confirm_search(),
        KeyCode::Esc => app.exit_search(),
        KeyCode::Char(c) => app.push_char(c),
        _ => {}
    }
}

fn update_modal_key(state: &mut EmporiumState, app: &mut App, code: KeyCode) -> Vec<Cmd> {
    match code {
        KeyCode::Esc => {
            app.close_modal();
            // Unconditional, not gated on which modal was open: cancelling
            // the Worker-agent picker must cancel whatever it was
            // interrupting too — a brigade forming (or a Worker being
            // added) anyway with whatever product happened to be selected
            // is a far worse outcome than clearing a stash that was already
            // `None` for every other modal kind. `pending_codex_trust_formation`
            // is already `None` by the time `Modal::ConfirmCodexTrust` can
            // even be open (see `update_codex_trust_checked`), but cleared
            // here too, for the same defensive reason.
            state.pending_brigade_formation = None;
            state.pending_worker_addition = None;
            state.pending_codex_trust_formation = None;
            Vec::new()
        }
        KeyCode::Up => {
            app.modal_select_prev();
            Vec::new()
        }
        KeyCode::Down => {
            app.modal_select_next();
            Vec::new()
        }
        KeyCode::Left => {
            app.modal_cursor_left();
            Vec::new()
        }
        KeyCode::Right => {
            app.modal_cursor_right();
            Vec::new()
        }
        KeyCode::Home => {
            app.modal_cursor_home();
            Vec::new()
        }
        KeyCode::End => {
            app.modal_cursor_end();
            Vec::new()
        }
        KeyCode::Tab => {
            app.modal_complete_candidate();
            Vec::new()
        }
        // New-session and Worker-agent-picker modals only (see
        // `App::modal_toggle_agent`'s doc for why the chōba binds neither);
        // a no-op for every other modal kind, same as
        // `modal_complete_candidate` above.
        KeyCode::BackTab => {
            app.modal_toggle_agent();
            Vec::new()
        }
        KeyCode::Backspace => {
            app.modal_backspace();
            Vec::new()
        }
        KeyCode::Delete => {
            app.modal_delete_forward();
            Vec::new()
        }
        KeyCode::Enter => confirm_modal(state, app),
        KeyCode::Char(c) => {
            app.modal_push_char(c);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn confirm_modal(state: &mut EmporiumState, app: &mut App) -> Vec<Cmd> {
    enum Kind {
        Archive,
        Group,
        New,
        Disband,
        Kill,
        WorkerAgent,
        CodexTrust,
        DirectorReopen,
    }
    let kind = match app.modal() {
        Some(Modal::ConfirmArchive { .. }) => Some(Kind::Archive),
        Some(Modal::GroupJoin(_)) => Some(Kind::Group),
        Some(Modal::NewSession(_)) => Some(Kind::New),
        Some(Modal::ConfirmDisband { .. }) => Some(Kind::Disband),
        Some(Modal::ConfirmKill { .. }) => Some(Kind::Kill),
        Some(Modal::WorkerAgentPicker(_)) => Some(Kind::WorkerAgent),
        Some(Modal::ConfirmCodexTrust) => Some(Kind::CodexTrust),
        Some(Modal::ConfirmDirectorReopen { .. }) => Some(Kind::DirectorReopen),
        None => None,
    };
    match kind {
        Some(Kind::Archive) => confirm_archive_modal(app),
        Some(Kind::Group) => confirm_group_join_modal(app),
        Some(Kind::New) => confirm_new_session_modal(app),
        Some(Kind::Disband) => confirm_disband_modal(app),
        Some(Kind::Kill) => confirm_kill_modal(state, app),
        Some(Kind::WorkerAgent) => confirm_worker_agent_modal(state, app),
        Some(Kind::CodexTrust) => confirm_codex_trust_modal(state, app),
        Some(Kind::DirectorReopen) => confirm_director_reopen_modal(state, app),
        None => Vec::new(),
    }
}

/// Confirm the Worker-agent picker: read its chosen agent and (already
/// trimmed-by-nothing — an empty model is meaningful, "no `--model` flag")
/// model text, then combine with whichever interruption opened this modal —
/// [`EmporiumState::pending_worker_addition`] (`add_worker`'s own `Select`
/// branch) or [`EmporiumState::pending_brigade_formation`]
/// (`begin_brigade_formation`'s) — at most one is ever set, since only one
/// modal is ever open at a time and each is stashed right before opening
/// it. A no-op (closes the modal, nothing happens) if neither is set —
/// shouldn't happen structurally, but doing nothing is the only safe
/// fallback if it ever does, not guessing at a product.
///
/// Formation alone routes a Codex choice through a trust check first
/// (`update_codex_trust_checked`) before `FormBrigade`, same as its own
/// non-`Select` path. `add_worker`'s non-`Select` path has never done this
/// (see [`PendingWorkerAddition`]'s own doc for why `Select` doesn't gain
/// it here either) — adding one more Worker to an already-running brigade
/// is the lower-stakes case, and by the time one exists to add to, any
/// Codex member already spawned would already have gone through this same
/// gate if one was ever needed (the hook's trust key is one fixed string
/// per install, not per-member — see `opener::session_start_hook_override`'s
/// doc).
fn confirm_worker_agent_modal(state: &mut EmporiumState, app: &mut App) -> Vec<Cmd> {
    let Some(Modal::WorkerAgentPicker(picker)) = app.modal() else {
        return Vec::new();
    };
    let worker_agent = picker.agent();
    let worker_model = picker.model_input().to_string();
    app.close_modal();

    if let Some(addition) = state.pending_worker_addition.take() {
        return vec![Cmd::Store(StoreIntent::AddWorker {
            brigade_id: addition.brigade_id,
            cwd: addition.cwd,
            worker_agent,
            worker_model,
        })];
    }

    let Some(formation) = state.pending_brigade_formation.take() else {
        return Vec::new();
    };
    if worker_agent == AgentKind::Codex {
        state.pending_codex_trust_formation = Some(PendingCodexTrustFormation {
            director_row_id: formation.director_row_id,
            name: formation.name,
            cwd: formation.cwd,
            worker_count: formation.worker_count,
            worker_agent,
            worker_model,
        });
        return vec![Cmd::CheckCodexTrust];
    }
    vec![Cmd::Store(StoreIntent::FormBrigade {
        director_row_id: formation.director_row_id,
        name: formation.name,
        cwd: formation.cwd,
        worker_count: formation.worker_count,
        worker_agent,
        worker_model,
    })]
}

fn confirm_archive_modal(app: &mut App) -> Vec<Cmd> {
    let Some(Modal::ConfirmArchive { session_id, title }) = app.modal() else {
        return Vec::new();
    };
    let id = session_id.clone();
    let title = title.clone();
    app.close_modal();
    vec![Cmd::Store(StoreIntent::Archive { id, title })]
}

fn confirm_group_join_modal(app: &mut App) -> Vec<Cmd> {
    let Some(Modal::GroupJoin(gstate)) = app.modal() else {
        return Vec::new();
    };
    let session_id = gstate.session_id().to_string();
    let Some(target) = app.modal_group_join_target() else {
        return Vec::new();
    };
    app.close_modal();
    let target = match target {
        GroupJoinTarget::Existing(id, name) => GroupJoinTargetData::Existing(id, name),
        GroupJoinTarget::New(name) => GroupJoinTargetData::New(name),
    };
    vec![Cmd::Store(StoreIntent::JoinGroup { session_id, target })]
}

/// `is_dir()` is a stat — file I/O this crate may never do itself
/// (`docs/DISCIPLINE.md` §3) — so Enter sends [`Cmd::CheckNewSessionCwd`]
/// and leaves the modal open rather than deciding on the spot;
/// [`update_new_session_cwd_checked`] is where the verdict actually lands
/// and the pre-round-trip success path (mint the key, stage the open) now
/// lives. `App::modal_new_session_check_pending` blocks a second Enter
/// while one round trip is already in flight — without it, Enter twice in a
/// row before the first answer lands would kick off two checks (and, since
/// the discriminator from the previous round fixed the double-open bug that
/// used to *corrupt* state, two merely-redundant panes rather than one
/// corrupted one — still wrong, still worth preventing outright).
fn confirm_new_session_modal(app: &mut App) -> Vec<Cmd> {
    let Some(Modal::NewSession(_)) = app.modal() else {
        return Vec::new();
    };
    if app.modal_new_session_check_pending() {
        return Vec::new();
    }
    let Some(cwd) = app.modal_new_session_target() else {
        return Vec::new();
    };
    app.modal_begin_new_session_check();
    vec![Cmd::CheckNewSessionCwd { cwd }]
}

/// The other half of [`confirm_new_session_modal`]'s round trip: the
/// shell's answer to whether the checked cwd is a real directory.
///
/// `App::modal_new_session_check_resolves` is the stale-result guard: the
/// operator can keep typing while the stat is in flight, so `cwd` (what was
/// actually checked) is compared against the modal's *current* target
/// before this verdict is trusted, and the pending-check marker is cleared
/// either way — a verdict that no longer applies is simply dropped, not
/// requeued, leaving the modal open and Enter live again for a fresh check
/// against whatever the operator has typed since.
fn update_new_session_cwd_checked(
    state: &mut EmporiumState,
    app: &mut App,
    cwd: PathBuf,
    is_dir: bool,
) -> Vec<Cmd> {
    if !app.modal_new_session_check_resolves(&cwd) {
        return Vec::new();
    }
    if !is_dir {
        app.modal_set_error(format!("{} is not a directory", cwd.display()));
        return Vec::new();
    }
    // Read before `close_modal` drops the `NewSessionState` this came from.
    let agent = app.modal_new_session_agent();
    app.close_modal();
    let key = state.mint_plain_key(&cwd);
    state.pending_opens.insert(key.clone(), PendingOpen::Solo);
    vec![Cmd::OpenEmbedded {
        key,
        target: SessionToOpen {
            id: String::new(),
            agent,
            title: cwd.display().to_string(),
            cwd,
        },
        brigade: None,
        model: None,
    }]
}

fn confirm_disband_modal(app: &mut App) -> Vec<Cmd> {
    let Some(Modal::ConfirmDisband { brigade_id, .. }) = app.modal() else {
        return Vec::new();
    };
    let brigade_id = *brigade_id;
    app.close_modal();
    vec![Cmd::Store(StoreIntent::Disband { brigade_id })]
}

/// Confirm the kill dialog. [`KillChoice::ClosePane`] (the only choice for a
/// Director/solo pane, and the default for a Worker's) just makes the exit
/// happen — no store mutation, membership persists, and a killed Worker
/// respawns fresh under the same token the next time its brigade is staged
/// (the existing, field-tested disposable-Worker semantics `stage_brigade`
/// already has for one whose process is simply gone). The passive
/// `Event::PtyExited` fold (unchanged, see `update_pty_exited`) is what
/// actually cleans up the pane once the kill takes effect.
///
/// [`KillChoice::Dismiss`] needs `(brigade_id, token)` before
/// `StoreIntent::DismissWorker` can be built: a still-awaiting-discovery
/// Worker's synthetic key already embeds them
/// ([`SessionKey::worker_identity`]), so that path builds the intent right
/// here; a resolved Worker's real session id does not, so that path stashes
/// [`PendingMembership::DismissWorker`] and spends one `ResolveMembership`
/// round trip instead. Either way `state.pending_dismiss` is set first, so
/// [`update_worker_dismissed`] knows which pane to actually remove once the
/// dismissal (or its round trip) comes back.
fn confirm_kill_modal(state: &mut EmporiumState, app: &mut App) -> Vec<Cmd> {
    let Some(Modal::ConfirmKill {
        key, worker_choice, ..
    }) = app.modal()
    else {
        return Vec::new();
    };
    let key = SessionKey::from_id(key);
    let dismiss = *worker_choice == Some(KillChoice::Dismiss);
    app.close_modal();
    if !dismiss {
        return vec![Cmd::KillPty { key }];
    }
    state.pending_dismiss = Some(key.clone());
    match key.worker_identity() {
        Some((brigade_id, token)) => {
            vec![Cmd::Store(StoreIntent::DismissWorker { brigade_id, token })]
        }
        None => {
            state.pending_membership = Some(PendingMembership::DismissWorker);
            vec![Cmd::Store(StoreIntent::ResolveMembership {
                session_id: key.as_str().to_string(),
            })]
        }
    }
}

/// Enter / double-click on the sidebar: request membership resolution
/// first (see the module doc) — [`update_membership_resolved`] does the
/// actual staging/opening once the shell answers.
fn activate_selected(state: &mut EmporiumState, app: &App) -> Vec<Cmd> {
    let Some(row) = app.selected_row() else {
        return Vec::new();
    };
    state.pending_membership = Some(PendingMembership::Activate);
    vec![Cmd::Store(StoreIntent::ResolveMembership {
        session_id: row.id.clone(),
    })]
}

/// `B`: same membership-resolution round trip as [`activate_selected`].
fn brigade_key(state: &mut EmporiumState, app: &App) -> Vec<Cmd> {
    let Some(row) = app.selected_row() else {
        return Vec::new();
    };
    state.pending_membership = Some(PendingMembership::BrigadeKey);
    vec![Cmd::Store(StoreIntent::ResolveMembership {
        session_id: row.id.clone(),
    })]
}

fn update_membership_resolved(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    session_id: String,
    membership: Option<(BrigadeId, MemberToken, BrigadeRole)>,
    members: Option<Vec<(MemberToken, BrigadeRole, Option<String>)>>,
) -> Vec<Cmd> {
    let Some(purpose) = state.pending_membership.take() else {
        return Vec::new();
    };
    // Handled before the `row_for_id` guard below (which `DismissWorker`
    // doesn't need `row` for at all): the dismissed pane's key already lives
    // in `pending_dismiss`, and clearing it here on an unexpected answer
    // (not a Worker anymore, or not found) matters regardless of whether
    // this session still has a row.
    if matches!(purpose, PendingMembership::DismissWorker) {
        // `Some((.., Director)) | None` spelled out rather than `_`: a role
        // added later has to land in one arm or the other explicitly, not
        // fall silently into whichever one the wildcard used to catch.
        return match membership {
            Some((brigade_id, token, BrigadeRole::Worker)) => {
                vec![Cmd::Store(StoreIntent::DismissWorker { brigade_id, token })]
            }
            Some((_, _, BrigadeRole::Director)) | None => {
                state.pending_dismiss = None;
                Vec::new()
            }
        };
    }
    let Some(row) = app.row_for_id(&session_id).cloned() else {
        return Vec::new();
    };
    match purpose {
        PendingMembership::Activate => match membership {
            Some((brigade_id, _, BrigadeRole::Director)) => stage_brigade(
                state,
                app,
                brigade_id,
                &members.unwrap_or_default(),
                brigade,
            ),
            // Spelled out rather than `_`, same reasoning as the
            // `DismissWorker` branch above.
            Some((_, _, BrigadeRole::Worker)) | None => open_solo(state, &row),
        },
        PendingMembership::BrigadeKey => match membership {
            Some((brigade_id, _, BrigadeRole::Director)) => {
                app.open_confirm_disband_modal(brigade_id, row.display_title().to_string());
                Vec::new()
            }
            Some((_, _, BrigadeRole::Worker)) => {
                state.status = Some("workers can't be promoted to Director directly".to_string());
                Vec::new()
            }
            // Outranks the two gates inside `begin_brigade_formation`
            // (`Select`'s picker, the codex-trust check): reopening the
            // Director's own pane is the most disruptive of the three
            // (running work is lost) and the cheapest to abort (nothing has
            // been picked or written yet), so it has to be asked about
            // first — picking a product and a model only to be told the
            // pane is about to restart anyway would be the wrong order. See
            // `confirm_director_reopen_modal`/`update_pty_exited` for the
            // other half of this round trip.
            None if state.screens.contains_key(&SessionKey::from_id(&row.id)) => {
                app.open_confirm_director_reopen_modal(
                    row.id.clone(),
                    row.display_title().to_string(),
                );
                Vec::new()
            }
            None => begin_brigade_formation(state, app, brigade, &row),
        },
        // Handled above, before the `row_for_id` guard.
        PendingMembership::DismissWorker => Vec::new(),
    }
}

/// The formation decision proper, once nothing needs to reopen first (see
/// `update_membership_resolved`'s `None` branch, and `update_pty_exited` for
/// the resumed-after-reopen path): `Select` interrupts with a picker instead
/// of issuing `FormBrigade` right away — the operator has to choose a
/// product (and, given the product, a model) before there's anything to
/// form, see `confirm_worker_agent_modal` for the other half of that round
/// trip. Otherwise the Worker product resolves immediately, and — if it's
/// Codex — routes through a trust check first, same reasoning, see
/// `update_codex_trust_checked`.
fn begin_brigade_formation(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    row: &SessionRow,
) -> Vec<Cmd> {
    if brigade.worker_agent == WorkerAgentSetting::Select {
        state.pending_brigade_formation = Some(PendingBrigadeFormation {
            director_row_id: row.id.clone(),
            name: row.display_title().to_string(),
            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
            worker_count: brigade.worker_count(),
        });
        app.open_worker_agent_modal(
            row.agent,
            brigade.worker_model.clone(),
            brigade.worker_model_codex.clone(),
        );
        return Vec::new();
    }
    let worker_agent = resolve_worker_agent(brigade.worker_agent, row.agent);
    let worker_model = brigade
        .worker_model_for(worker_agent)
        .unwrap_or("")
        .to_string();
    if worker_agent == AgentKind::Codex {
        state.pending_codex_trust_formation = Some(PendingCodexTrustFormation {
            director_row_id: row.id.clone(),
            name: row.display_title().to_string(),
            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
            worker_count: brigade.worker_count(),
            worker_agent,
            worker_model,
        });
        return vec![Cmd::CheckCodexTrust];
    }
    vec![Cmd::Store(StoreIntent::FormBrigade {
        director_row_id: row.id.clone(),
        name: row.display_title().to_string(),
        cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
        worker_count: brigade.worker_count(),
        worker_agent,
        worker_model,
    })]
}

/// Answers `Cmd::CheckCodexTrust`, always following a
/// [`PendingCodexTrustFormation`] stash (`update_membership_resolved`'s
/// non-`Select` branch, or `confirm_worker_agent_modal` once `Select`'s own
/// picker resolves) — a no-op if it's missing, which shouldn't happen
/// structurally.
///
/// Three outcomes, and only one of them re-issues `FormBrigade`:
/// - **Not launchable at all** (banto's own path has a space — Codex can
///   never fire a hook command from one, `hook_command_is_launchable`'s
///   doc): a trust prompt would only ask the operator to approve a hook
///   that can never run, so it's skipped — a status line names the cause
///   (the same wording `codex_trust_notice` already uses for it) and
///   formation proceeds anyway. Unlike the "not primed" case below, there's
///   no action a pane could offer that would fix this one before forming.
/// - **Primed**: proceeds straight to `FormBrigade`, unchanged from before
///   this check existed.
/// - **Neither**: opens [`Modal::ConfirmCodexTrust`] and stops — formation
///   is abandoned for this press, not resumed once the operator approves or
///   cancels. Resuming it would need to know whether the approval actually
///   took (Codex hashes its own trust record; banto cannot verify a match
///   against it — see `banto_io::codex_trust`'s module doc), and treating an
///   unverifiable "probably fine" as ground truth is exactly the shape of
///   mistake this project has paid for before. The operator presses `B`
///   again once they're done.
fn update_codex_trust_checked(
    state: &mut EmporiumState,
    app: &mut App,
    primed: bool,
    hook_launchable: bool,
    now: Instant,
) -> Vec<Cmd> {
    let Some(formation) = state.pending_codex_trust_formation.take() else {
        return Vec::new();
    };
    if !hook_launchable {
        state.set_status(
            "codex: banto's own path contains a space, which Codex cannot launch a hook \
             from — this brigade's Codex member(s) start unbriefed"
                .to_string(),
            now,
        );
        return vec![Cmd::Store(StoreIntent::FormBrigade {
            director_row_id: formation.director_row_id,
            name: formation.name,
            cwd: formation.cwd,
            worker_count: formation.worker_count,
            worker_agent: formation.worker_agent,
            worker_model: formation.worker_model,
        })];
    }
    if primed {
        return vec![Cmd::Store(StoreIntent::FormBrigade {
            director_row_id: formation.director_row_id,
            name: formation.name,
            cwd: formation.cwd,
            worker_count: formation.worker_count,
            worker_agent: formation.worker_agent,
            worker_model: formation.worker_model,
        })];
    }
    app.open_confirm_codex_trust_modal();
    Vec::new()
}

/// Confirm [`Modal::ConfirmCodexTrust`]: mint a fresh key, stage it as the
/// (eventual) solo pane the same way any other fresh open does
/// (`PendingOpen::Solo` — reused as-is, not a new variant, because staging
/// is all this needs: no session row, no discovery), and ask the shell to
/// spawn Codex's trust-review startup under it. See `Cmd::OpenCodexTrustPane`
/// for why this never becomes a tracked session.
fn confirm_codex_trust_modal(state: &mut EmporiumState, app: &mut App) -> Vec<Cmd> {
    if !matches!(app.modal(), Some(Modal::ConfirmCodexTrust)) {
        return Vec::new();
    }
    app.close_modal();
    let key = state.mint_plain_key(std::path::Path::new(".codex-trust"));
    state.pending_opens.insert(key.clone(), PendingOpen::Solo);
    vec![Cmd::OpenCodexTrustPane { key }]
}

/// Confirm [`Modal::ConfirmDirectorReopen`]: kill the Director-to-be's
/// already-open pane and stash `pending_director_reopen` so
/// [`update_pty_exited`] can pick formation back up (via
/// [`begin_brigade_formation`]) once — not before — that pane's process has
/// actually exited. Nothing is written to the store here or by that
/// continuation's first steps; Esc leaves the brigade unformed with no
/// cleanup needed, same as never having pressed `B` at all.
fn confirm_director_reopen_modal(state: &mut EmporiumState, app: &mut App) -> Vec<Cmd> {
    let Some(Modal::ConfirmDirectorReopen {
        director_row_id, ..
    }) = app.modal()
    else {
        return Vec::new();
    };
    let director_row_id = director_row_id.clone();
    app.close_modal();
    let director_key = SessionKey::from_id(&director_row_id);
    state.pending_director_reopen = Some(director_row_id);
    vec![Cmd::KillPty { key: director_key }]
}

/// Stage `row` solo: reuse its screen if already open, else request a spawn.
fn open_solo(state: &mut EmporiumState, row: &SessionRow) -> Vec<Cmd> {
    let key = SessionKey::from_id(&row.id);
    if state.screens.contains_key(&key) {
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        return Vec::new();
    }
    state.pending_opens.insert(key.clone(), PendingOpen::Solo);
    vec![Cmd::OpenEmbedded {
        key,
        target: SessionToOpen {
            id: row.id.clone(),
            agent: row.agent,
            title: row.display_title().to_string(),
            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
        },
        brigade: None,
        model: None,
    }]
}

/// Stage brigade `brigade_id`: `members` is its full roster (token, role,
/// session id if known), already fetched by the shell alongside the
/// membership resolution that led here. A member whose row is already
/// embedded is added immediately; one that resolves to a row but isn't
/// embedded yet gets an `OpenEmbedded` request; a Worker with no resolved
/// session id yet (still awaiting discovery, or its process is gone) is
/// respawned fresh under its same token — disposable, unlike the Director,
/// whose failure to resolve just counts as "missing" (mirrors the
/// pre-migration `stage_brigade`).
fn stage_brigade(
    state: &mut EmporiumState,
    app: &App,
    brigade_id: BrigadeId,
    members: &[(MemberToken, BrigadeRole, Option<String>)],
    brigade: &BrigadeConfig,
) -> Vec<Cmd> {
    let director_row = members
        .iter()
        .find(|(_, role, _)| *role == BrigadeRole::Director)
        .and_then(|(_, _, sid)| sid.as_deref())
        .and_then(|sid| app.row_for_id(sid));
    let cwd = director_row
        .and_then(|row| row.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    // A Director not yet resolved to a row can't say what product it is
    // either — the safe default rather than blocking staging on it, same
    // fallback `add_worker`/`update_brigade_formed` use.
    let director_agent = director_row.map_or(AgentKind::ClaudeCode, |row| row.agent);
    let fresh_worker_agent = resolve_worker_agent(brigade.worker_agent, director_agent);

    let mut panes = Vec::new();
    let mut cmds = Vec::new();
    let mut missing = 0;
    for (token, role, session_id) in members {
        let resolved_row = session_id.as_deref().and_then(|sid| app.row_for_id(sid));
        match resolved_row {
            Some(row) => {
                let key = SessionKey::from_id(&row.id);
                if state.screens.contains_key(&key) {
                    panes.push(key);
                } else {
                    state.pending_opens.insert(
                        key.clone(),
                        // Resuming a known id: nothing to discover, so
                        // never a kickoff candidate (see `open_worker`'s
                        // own doc on why only *it* ever sets this `true`).
                        PendingOpen::BrigadeMember {
                            brigade_id,
                            needs_codex_kickoff: false,
                            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                        },
                    );
                    // A resume's `--model` matters exactly like a fresh
                    // spawn's (see `open_worker`): a Worker resumed without
                    // it silently falls back to the operator's own default
                    // model rather than the brigade's configured one. Never
                    // for the Director — that's the operator's own session,
                    // launched (and re-launched) entirely outside banto's
                    // control. Resolved from *this row's own* `agent`, not
                    // `fresh_worker_agent`: a resumed member already has a
                    // fixed product, unrelated to what a brand-new spawn
                    // would pick.
                    let model = (*role == BrigadeRole::Worker)
                        .then(|| brigade.worker_model_for(row.agent))
                        .flatten()
                        .map(str::to_string);
                    cmds.push(Cmd::OpenEmbedded {
                        key,
                        target: SessionToOpen {
                            id: row.id.clone(),
                            agent: row.agent,
                            title: row.display_title().to_string(),
                            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                        },
                        brigade: Some((brigade_id, token.clone(), *role)),
                        model,
                    });
                }
            }
            None if *role == BrigadeRole::Worker => {
                // A Worker with no id yet is identified only by its synthetic
                // key, so "is it already open?" has to be asked against that
                // key — the `Some(row)` arm's `screens` check above cannot
                // see it. Re-staging a cell whose Worker is alive but still
                // unidentified (Claude writes a session's jsonl at its first
                // *turn*, so discovery stays pending for as long as nobody
                // types into the pane) would otherwise spawn a second child
                // under the same key: the shell's handle map would replace
                // the live one — closing its PTY, killing it on Unix — and
                // the pane would blink back to an empty prompt.
                let key = SessionKey::new_worker(brigade_id, token);
                if state.screens.contains_key(&key) {
                    panes.push(key);
                } else {
                    let model = brigade.worker_model_for(fresh_worker_agent).unwrap_or("");
                    cmds.extend(open_worker(
                        state,
                        brigade_id,
                        token,
                        &cwd,
                        model,
                        fresh_worker_agent,
                    ));
                }
            }
            None => missing += 1,
        }
    }
    if panes.is_empty() && cmds.is_empty() {
        state.status = Some("no brigade members could be opened".to_string());
        return Vec::new();
    }
    state.stage = Stage::Brigade {
        id: brigade_id,
        panes,
        focused: 0,
    };
    state.focus = Focus::Pane;
    if missing > 0 {
        state.status = Some(format!("brigade staged ({missing} member(s) not found)"));
    }
    cmds
}

fn toggle_pin(app: &mut App) -> Vec<Cmd> {
    let Some((id, pinned)) = app.toggle_pin() else {
        return Vec::new();
    };
    vec![Cmd::Store(StoreIntent::SetPin { id, pinned })]
}

/// `b`: spawn one more fresh Worker into the staged brigade. `cwd` is the
/// Director's own row cwd, resolved from `app` via the Director's key
/// (always `panes[0]`, always a known real id) — no extra round trip needed.
fn add_worker(state: &mut EmporiumState, app: &mut App, brigade: &BrigadeConfig) -> Vec<Cmd> {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        state.status = Some("no brigade staged — press B to start one".to_string());
        return Vec::new();
    };
    let brigade_id = *id;
    let director_row = panes.first().and_then(|key| app.row_for_id(key.as_str()));
    let cwd = director_row
        .and_then(|row| row.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    // A Director not yet resolved to a row can't say what product it is
    // either — the safe default rather than blocking on it, same fallback
    // `stage_brigade`/`update_brigade_formed` use.
    let director_agent = director_row.map_or(AgentKind::ClaudeCode, |row| row.agent);

    // Same interruption `begin_brigade_formation` does for `Select` — see
    // `PendingWorkerAddition`'s own doc for what Esc means here
    // specifically (lighter than formation's: this only cancels adding one
    // more Worker, not an already-running brigade).
    if brigade.worker_agent == WorkerAgentSetting::Select {
        state.pending_worker_addition = Some(PendingWorkerAddition { brigade_id, cwd });
        app.open_worker_agent_modal(
            director_agent,
            brigade.worker_model.clone(),
            brigade.worker_model_codex.clone(),
        );
        return Vec::new();
    }

    let worker_agent = resolve_worker_agent(brigade.worker_agent, director_agent);
    let worker_model = brigade
        .worker_model_for(worker_agent)
        .unwrap_or("")
        .to_string();
    vec![Cmd::Store(StoreIntent::AddWorker {
        brigade_id,
        cwd,
        worker_agent,
        worker_model,
    })]
}

/// Lines [`update_mouse`] scrolls a pane's own scrollback per wheel notch —
/// tmux's own long-standing convention, chosen so a habit already
/// muscle-memorized elsewhere carries over here.
const SCROLL_NOTCH_LINES: isize = 3;

/// Dispatch one mouse event: sidebar click/scroll, or — over a pane — focus
/// it (`Down(Left)` always moves focus, regardless of whether it wants
/// mouse), then either forward the event as an SGR report or, for the
/// wheel specifically, consume it into that pane's own scrollback — the
/// choice in both cases keyed on whether the focused pane's child asked
/// for mouse reporting in the one encoding banto speaks (see
/// [`crate::screen::Screen::wants_sgr_mouse`]): a child that never enabled
/// mouse reporting has no idea what to do with an SGR sequence arriving on
/// its stdin, so forwarding unconditionally would just leak noise into
/// whatever it's actually reading — and it has no scrollback of its own to
/// receive the wheel as input either. A child that *does* want mouse gets
/// every event forwarded exactly as before, wheel included: it can already
/// implement its own scrollback (Claude Code does), and forwarding both
/// keeps that working and never fights it over whose scroll position is
/// authoritative.
///
/// The host terminal's own mouse capture used to track this same
/// wants-mouse check, releasing itself over a pane that didn't want
/// reports so the terminal's native text selection could take over. That
/// mechanism is gone: capture is unconditional now (`setup_terminal`)
/// because releasing it had a worse cost than the native selection it
/// bought — a `BANTO_INPUT_LOG` capture showed that once the host terminal
/// itself stops delivering mouse events, banto cannot get them back short
/// of the operator manually re-enabling capture out-of-band, which trapped
/// focus on any pane whose child didn't request mouse reporting (Codex,
/// measured; any other non-mouse child equally). This function's own
/// SGR-forwarding gate above is unaffected by that removal — it was always
/// about what bytes a child understands, never about whether banto's
/// terminal receives the event in the first place.
fn update_mouse(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    mouse: MouseEvent,
    now: Instant,
) -> Vec<Cmd> {
    if app.modal().is_some() {
        return Vec::new();
    }
    let _ = brigade;
    let pos = Position::new(mouse.column, mouse.row);
    let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));

    if areas.pane.contains(pos) {
        let tiles = stage_tiles(areas.pane, &state.stage);
        let hit = tiles.iter().find(|(_, rect)| rect.contains(pos)).cloned();
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            state.focus = Focus::Pane;
            if let Stage::Brigade { panes, focused, .. } = &mut state.stage
                && let Some((key, _)) = &hit
                && let Some(p) = panes.iter().position(|k| k == key)
            {
                *focused = p;
            }
        }
        if state.focus == Focus::Pane
            && let Some((key, rect)) = hit
        {
            if state
                .screens
                .get(&key)
                .is_some_and(crate::screen::Screen::wants_sgr_mouse)
            {
                if let Some(bytes) = mouse_to_sgr(&mouse, pane_content(rect)) {
                    state.last_forwarded_input.insert(key.clone(), now);
                    return vec![Cmd::WritePty { key, bytes }];
                }
            } else {
                let notch_delta = match mouse.kind {
                    MouseEventKind::ScrollUp => Some(SCROLL_NOTCH_LINES),
                    MouseEventKind::ScrollDown => Some(-SCROLL_NOTCH_LINES),
                    _ => None,
                };
                if let Some(delta) = notch_delta
                    && let Some(screen) = state.screens.get_mut(&key)
                {
                    screen.scroll(delta);
                }
            }
        }
        return Vec::new();
    }

    let sb = areas.sidebar;
    let sidebar_inner = Rect {
        x: sb.x + 1,
        y: sb.y + 1,
        width: sb.width.saturating_sub(2),
        height: sb.height.saturating_sub(2),
    };
    match mouse.kind {
        MouseEventKind::ScrollUp if sb.contains(pos) => {
            app.scroll(-1);
            Vec::new()
        }
        MouseEventKind::ScrollDown if sb.contains(pos) => {
            app.scroll(1);
            Vec::new()
        }
        MouseEventKind::Down(MouseButton::Left) if sidebar_inner.contains(pos) => {
            state.focus = Focus::Sidebar;
            let viewport_row = (pos.y - sidebar_inner.y) as usize;
            if app.click(viewport_row, now) == Some(ClickOutcome::Activated) {
                activate_selected(state, app)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

/// Encode a mouse event as an SGR mouse report for a child whose grid starts
/// at `content` (screen coords mapped into the grid, 1-based).
fn mouse_to_sgr(mouse: &MouseEvent, content: Rect) -> Option<Vec<u8>> {
    if mouse.column < content.x || mouse.row < content.y {
        return None;
    }
    let cx = mouse.column - content.x;
    let cy = mouse.row - content.y;
    if cx >= content.width || cy >= content.height {
        return None;
    }
    let (cb, release) = match mouse.kind {
        MouseEventKind::Down(b) => (mouse_btn(b), false),
        MouseEventKind::Up(b) => (mouse_btn(b), true),
        MouseEventKind::Drag(b) => (mouse_btn(b) + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    let final_char = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{};{};{}{}", cb, cx + 1, cy + 1, final_char).into_bytes())
}

fn mouse_btn(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// The text up to (not including) the first CR or LF — conventional
/// single-line-field paste behavior. Used by [`update_paste`]'s modal and
/// search branches only: a trailing newline (the usual result of copying
/// one line) becomes a no-op instead of dumping whatever followed it into
/// the query or a stray character into a path. Deliberately changes Unix
/// behavior too (a real `Event::Paste` there previously pushed the
/// remainder straight through, since `push_char`/`modal_push_char` merely
/// skip a control character rather than stopping at it) — so both
/// platforms agree instead of diverging on which one happens to have a
/// working paste-burst source.
fn first_line(text: &str) -> &str {
    let end = text.find(['\r', '\n']).unwrap_or(text.len());
    &text[..end]
}

fn update_paste(state: &mut EmporiumState, app: &mut App, text: String, now: Instant) -> Vec<Cmd> {
    if app.modal().is_some() {
        for c in first_line(&text).chars() {
            app.modal_push_char(c);
        }
        return Vec::new();
    }
    if app.mode() == Mode::Search {
        for c in first_line(&text).chars() {
            app.push_char(c);
        }
        return Vec::new();
    }
    if state.focus == Focus::Pane
        && let Some(key) = state.stage.focused_key().cloned()
    {
        let normalized = normalize_paste_line_endings(&text);
        let bracketed = state
            .screens
            .get(&key)
            .is_some_and(|screen| screen.screen().bracketed_paste());
        let bytes = if bracketed {
            wrap_bracketed_paste(&normalized)
        } else {
            normalized.into_bytes()
        };
        state.last_forwarded_input.insert(key.clone(), now);
        cancel_pending_submit_on_input(&mut state.pending_submits, &key);
        return vec![Cmd::WritePty { key, bytes }];
    }
    Vec::new()
}

fn update_spawned(state: &mut EmporiumState, key: SessionKey, now: Instant) -> Vec<Cmd> {
    state
        .screens
        .insert(key.clone(), crate::screen::Screen::new(24, 80));
    let Some(pending) = state.pending_opens.remove(&key) else {
        return Vec::new();
    };
    match pending {
        PendingOpen::Solo => {
            state.stage = Stage::Solo(key);
            state.focus = Focus::Pane;
            Vec::new()
        }
        PendingOpen::BrigadeDirector {
            brigade_id,
            worker_tokens,
            cwd,
            worker_agent,
            worker_model,
        } => {
            state.stage = Stage::Brigade {
                id: brigade_id,
                panes: vec![key],
                focused: 0,
            };
            state.focus = Focus::Pane;
            worker_tokens
                .into_iter()
                .flat_map(|token| {
                    open_worker(state, brigade_id, &token, &cwd, &worker_model, worker_agent)
                })
                .collect()
        }
        PendingOpen::BrigadeMember {
            brigade_id,
            needs_codex_kickoff,
            cwd,
        } => {
            if let Stage::Brigade { id, panes, .. } = &mut state.stage
                && *id == brigade_id
                && !panes.contains(&key)
            {
                panes.push(key.clone());
            }
            if needs_codex_kickoff {
                state.pending_kickoffs.push(PendingKickoff {
                    key,
                    spawned_at: now,
                    cwd,
                    notified_untrusted: false,
                });
            }
            Vec::new()
        }
    }
}

/// Which product a fresh Worker spawn should use, given `[brigade]
/// worker_agent` and the brigade's own Director's product.
///
/// `Select` is a stand-in for `Inherit` here, not a considered choice of its
/// own: every call site that reaches this function runs well after
/// formation already started — a re-stage, a later `B` add-worker, or the
/// moment right after the Director itself finished spawning — with no
/// synchronous UI moment left to interrupt with a picker. The formation
/// modal that actually asks the operator up front (so every one of these
/// call sites gets an already-decided `AgentKind` before this function ever
/// runs) is separate, not-yet-landed work; until it does, falling back to
/// `Inherit`'s behavior is the least-surprising default.
fn resolve_worker_agent(setting: WorkerAgentSetting, director_agent: AgentKind) -> AgentKind {
    match setting {
        WorkerAgentSetting::Inherit | WorkerAgentSetting::Select => director_agent,
        WorkerAgentSetting::Claude => AgentKind::ClaudeCode,
        WorkerAgentSetting::Codex => AgentKind::Codex,
    }
}

/// Emit the `Cmd::OpenEmbedded` for one auto-spawned Worker, wired to the
/// brigade's MCP channel, tracked as a `BrigadeMember` open.
///
/// `agent` is always the caller's own, already-resolved choice (see
/// [`resolve_worker_agent`]) — this function never re-derives it. Whenever
/// that choice is `AgentKind::Codex`,
/// [`PendingOpen::BrigadeMember::needs_codex_kickoff`] picks it up
/// automatically: this function is the *only* place that ever sets it
/// `true`, since every call here is a fresh, id-less spawn — a resumed
/// member with a known id (`stage_brigade`'s own `Some(row)` branch) never
/// calls this at all.
fn open_worker(
    state: &mut EmporiumState,
    brigade_id: BrigadeId,
    token: &str,
    cwd: &std::path::Path,
    worker_model: &str,
    agent: AgentKind,
) -> Vec<Cmd> {
    let key = SessionKey::new_worker(brigade_id, token);
    state.pending_opens.insert(
        key.clone(),
        PendingOpen::BrigadeMember {
            brigade_id,
            needs_codex_kickoff: agent == AgentKind::Codex,
            cwd: cwd.to_path_buf(),
        },
    );
    vec![Cmd::OpenEmbedded {
        key,
        target: SessionToOpen {
            id: String::new(),
            agent,
            title: format!("worker {token}"),
            cwd: cwd.to_path_buf(),
        },
        brigade: Some((brigade_id, token.to_string(), BrigadeRole::Worker)),
        model: (!worker_model.is_empty()).then(|| worker_model.to_string()),
    }]
}

fn update_spawn_failed(
    state: &mut EmporiumState,
    key: SessionKey,
    error: String,
    now: Instant,
) -> Vec<Cmd> {
    state.pending_opens.remove(&key);
    state.set_status(format!("failed to open: {error}"), now);
    Vec::new()
}

fn update_pty_exited(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    key: SessionKey,
    now: Instant,
) -> Vec<Cmd> {
    state.screens.remove(&key);
    state.unstage(&key);
    // A Director-reopen confirm killed exactly this pane and is waiting for
    // it to actually be gone before reopening it wired — see
    // `confirm_director_reopen_modal`'s doc for why "gone" can't be assumed
    // any earlier than this event. Takes over from the generic "session
    // ended" status below: formation continuing *is* what happens next.
    if state
        .pending_director_reopen
        .as_deref()
        .is_some_and(|id| SessionKey::from_id(id) == key)
    {
        let director_row_id = state.pending_director_reopen.take().unwrap();
        return match app.row_for_id(&director_row_id).cloned() {
            Some(row) => begin_brigade_formation(state, app, brigade, &row),
            None => Vec::new(),
        };
    }
    // The residual-race counterpart above (`PendingDirectorWiring`'s own
    // doc): the brigade already exists in the store, so there is nothing
    // left to decide, only wiring to apply once this pane is confirmed gone.
    if state
        .pending_director_wiring
        .as_ref()
        .is_some_and(|wiring| SessionKey::from_id(&wiring.director_row_id) == key)
    {
        let wiring = state.pending_director_wiring.take().unwrap();
        return open_director_with_wiring(state, app, wiring);
    }
    let title = app
        .row_for_id(key.as_str())
        .map(|row| row.display_title().to_string())
        .unwrap_or_else(|| key.as_str().to_string());
    state.set_status(format!("session ended: {title}"), now);
    Vec::new()
}

fn update_discovery_result(
    state: &mut EmporiumState,
    old_key: SessionKey,
    session_id: String,
    member: Option<(BrigadeId, MemberToken)>,
    now: Instant,
) -> Vec<Cmd> {
    let new_key = SessionKey::from_id(&session_id);
    if new_key == old_key {
        return Vec::new();
    }
    // Refuse a discovery that would collapse two panes onto one key rather
    // than merging them: the second pane would draw the first one's screen,
    // be titled after it, have its own child's handle orphaned, and — via
    // the store write below — claim the first one's session id as its own
    // brigade membership. Discovery is supposed to make this unreachable
    // (the shell excludes ids it already hosts), but "supposed to" is not a
    // reason to let a destructive merge through.
    //
    // Refusing here is a dead end, not a deferral: by the time this event
    // reaches `update`, the shell's `poll_discovery` has already dropped
    // `old_key`'s tracker from its list (it `retain`s out every id it
    // resolves, unconditionally, before the event is even dispatched) — so
    // there is nothing left to retry. The degraded outcome is that
    // `old_key`'s pane keeps its synthetic placeholder key forever: no
    // `Cmd::Store(StoreIntent::SetMemberSession)` ever fires for it, so a
    // brigade member hosted there stays unidentified (no store row) for the
    // rest of the run. That is accepted on purpose rather than re-queued,
    // because a permanently-unresolvable tracker re-queued for another
    // attempt would re-run `find_new_sessions` — a full scan of the
    // projects directory, the most expensive I/O `poll_discovery` does —
    // on every poll tick (as often as ~50ms) for as long as the pane stays
    // open. And this branch is meant to be a last-resort backstop, not the
    // common case: the claimed-set guard `poll_discovery` already applies
    // (ids already held by an open pane, sourced from the live handle map)
    // is what is supposed to keep two trackers from ever resolving onto the
    // same id in the first place.
    if state.screens.contains_key(&new_key) {
        state.set_status(
            format!("discovery collision on {session_id} — ignored"),
            now,
        );
        return Vec::new();
    }
    let mut cmds = Vec::new();
    if let Some(screen) = state.screens.remove(&old_key) {
        state.screens.insert(new_key.clone(), screen);
        cmds.push(Cmd::RekeyPty {
            from: old_key.clone(),
            to: new_key.clone(),
        });
    }
    match &mut state.stage {
        Stage::Solo(k) if *k == old_key => *k = new_key.clone(),
        Stage::Brigade { panes, .. } => {
            for pane in panes.iter_mut() {
                if *pane == old_key {
                    *pane = new_key.clone();
                }
            }
        }
        _ => {}
    }
    if let Some((brigade_id, token)) = member {
        cmds.push(Cmd::Store(StoreIntent::SetMemberSession {
            brigade_id,
            token,
            session_id,
        }));
    }
    cmds
}

/// A staged member's Claude session forked in place (see
/// [`Event::MemberSessionForked`]'s doc for why). The store write always
/// happens — the fork is real regardless of what the emporium's own panes
/// look like right now — but the pane/screen rename only applies if the
/// brigade is still staged with `old_id`'s pane present; `focused` needs no
/// fixing of its own, since renaming `panes[i]`'s *value* in place never
/// moves what index it lives at. Idempotent under a repeated fact (the
/// observation that produces this repeats until the store row changes):
/// once `old_id`'s pane has already been renamed to `new_id`, `old_id` is no
/// longer in `panes` and this becomes a store-only no-op, same as the
/// "not staged" case below.
fn update_member_session_forked(
    state: &mut EmporiumState,
    brigade_id: BrigadeId,
    token: MemberToken,
    old_id: String,
    new_id: String,
    now: Instant,
) -> Vec<Cmd> {
    let old_key = SessionKey::from_id(&old_id);
    let new_key = SessionKey::from_id(&new_id);
    let mut cmds = Vec::new();
    let staged = matches!(
        &state.stage,
        Stage::Brigade { id, panes, .. } if *id == brigade_id && panes.contains(&old_key)
    );
    if staged {
        // Same refusal precedent as `update_discovery_result`'s collision
        // guard just above: if `new_key` is already a live pane (the
        // operator opened the continuation separately as a solo session,
        // say), renaming onto it would collapse two panes into one. Unlike
        // that guard, the store write below still goes through — a later
        // re-stage heals the pane once the collision clears, but the store
        // must record the truth now regardless.
        if state.screens.contains_key(&new_key) {
            state.set_status(
                format!("session fork collision on {new_id} — pane not renamed"),
                now,
            );
        } else {
            if let Some(screen) = state.screens.remove(&old_key) {
                state.screens.insert(new_key.clone(), screen);
            }
            if let Stage::Brigade { panes, .. } = &mut state.stage {
                for pane in panes.iter_mut() {
                    if *pane == old_key {
                        *pane = new_key.clone();
                    }
                }
            }
            cmds.push(Cmd::RekeyPty {
                from: old_key,
                to: new_key,
            });
        }
    }
    cmds.push(Cmd::Store(StoreIntent::SetMemberSession {
        brigade_id,
        token,
        session_id: new_id,
    }));
    cmds
}

/// `Event::BrigadeFormed`'s successful-formation payload, bundled so
/// [`update_brigade_formed`] doesn't tip into clippy's too-many-arguments —
/// these five always travel together anyway, all the way from
/// `StoreIntent::FormBrigade`.
struct FormedBrigade {
    director_row_id: String,
    name: String,
    cwd: PathBuf,
    worker_agent: AgentKind,
    worker_model: String,
}

fn update_brigade_formed(
    state: &mut EmporiumState,
    app: &mut App,
    formed: FormedBrigade,
    result: Result<(BrigadeId, Vec<MemberToken>), String>,
    now: Instant,
) -> Vec<Cmd> {
    let FormedBrigade {
        director_row_id,
        name,
        cwd,
        worker_agent,
        worker_model,
    } = formed;
    let (brigade_id, worker_tokens) = match result {
        Ok(pair) => pair,
        Err(err) => {
            state.set_status(format!("failed to form brigade: {err}"), now);
            return Vec::new();
        }
    };
    let director_key = SessionKey::from_id(&director_row_id);
    let worker_count = worker_tokens.len();
    let wiring = PendingDirectorWiring {
        director_row_id,
        brigade_id,
        worker_tokens,
        cwd,
        worker_agent,
        worker_model,
    };
    let cmds = if state.screens.contains_key(&director_key) {
        // The interactive gate (`begin_brigade_formation`'s caller,
        // `confirm_director_reopen_modal`) is meant to have already closed
        // this pane before `StoreIntent::FormBrigade` was even issued —
        // reaching here with it still open is the residual race worker-3's
        // card called out: something reopened it in the brief window
        // between that confirm and this store round trip landing. The
        // brigade record already exists by this point (this function's own
        // early return above), so there is no clean way left to abort —
        // silently staging the existing pane as-is would reproduce the
        // exact bug this round trip exists to fix (a Director with no
        // MCP/hook wiring), so it is killed and reopened here too, same as
        // the interactive gate, just without asking first.
        state.pending_director_wiring = Some(wiring);
        vec![Cmd::KillPty { key: director_key }]
    } else {
        open_director_with_wiring(state, app, wiring)
    };
    state.set_status(
        format!("brigade formed — director: {name}, {worker_count} worker(s) spawned"),
        now,
    );
    cmds
}

/// Open the Director wired (MCP config / Codex `-c` overrides — see
/// [`PendingOpen::BrigadeDirector`]'s own doc), for a `director_row_id` whose
/// pane is confirmed *not* open right now — [`update_brigade_formed`]'s
/// direct case, or [`update_pty_exited`] once a kill-and-reopen (either
/// gate) confirms the old pane is actually gone. `Vec::new()` if the row
/// can no longer be found (the session vanished from `app.rows` in the
/// interim) — nothing left to open, but the brigade record itself, and
/// whatever status message the caller already set, stand regardless.
fn open_director_with_wiring(
    state: &mut EmporiumState,
    app: &App,
    wiring: PendingDirectorWiring,
) -> Vec<Cmd> {
    let PendingDirectorWiring {
        director_row_id,
        brigade_id,
        worker_tokens,
        cwd,
        worker_agent,
        worker_model,
    } = wiring;
    let director_key = SessionKey::from_id(&director_row_id);
    state.pending_opens.insert(
        director_key.clone(),
        PendingOpen::BrigadeDirector {
            brigade_id,
            worker_tokens,
            cwd: cwd.clone(),
            worker_agent,
            worker_model,
        },
    );
    let Some(row) = app.row_for_id(&director_row_id) else {
        return Vec::new();
    };
    vec![Cmd::OpenEmbedded {
        key: director_key,
        target: SessionToOpen {
            id: director_row_id,
            agent: row.agent,
            title: row.display_title().to_string(),
            cwd,
        },
        brigade: Some((brigade_id, "director".to_string(), BrigadeRole::Director)),
        model: None,
    }]
}

fn update_worker_added(
    state: &mut EmporiumState,
    brigade_id: BrigadeId,
    cwd: PathBuf,
    worker_agent: AgentKind,
    worker_model: String,
    result: Result<MemberToken, String>,
    now: Instant,
) -> Vec<Cmd> {
    match result {
        Ok(token) => {
            state.set_status(format!("{token} added"), now);
            open_worker(state, brigade_id, &token, &cwd, &worker_model, worker_agent)
        }
        Err(err) => {
            state.set_status(format!("failed to add worker: {err}"), now);
            Vec::new()
        }
    }
}

/// On success while the disbanded brigade is staged: fall the stage back to
/// `Solo(director)` (unchanged), and additionally kill every staged Worker
/// pane (`panes[1..]` — the Director, `panes[0]`, is the operator's own
/// session and survives). Workers are banto-spawned creatures that die with
/// their brigade; the kills flow through the same passive `PtyExited` fold
/// as any other exit — a killed-but-unstaged Worker (already gone, or one
/// this stage never resolved) is simply skipped, nothing to kill.
fn update_disbanded(
    state: &mut EmporiumState,
    app: &mut App,
    brigade_id: BrigadeId,
    result: Result<(HashSet<String>, HashSet<String>), String>,
    now: Instant,
) -> Vec<Cmd> {
    match result {
        Ok((hidden, directors)) => {
            app.set_hidden_worker_ids(hidden);
            app.set_directors(directors);
            let (director_key, worker_keys) = if let Stage::Brigade { id, panes, .. } = &state.stage
                && *id == brigade_id
            {
                let mut panes = panes.iter();
                (panes.next().cloned(), panes.cloned().collect())
            } else {
                (None, Vec::new())
            };
            state.stage = match &director_key {
                Some(key) => Stage::Solo(key.clone()),
                None => Stage::Empty,
            };
            state.set_status("brigade disbanded".to_string(), now);
            worker_keys
                .into_iter()
                .map(|key| Cmd::KillPty { key })
                .collect()
        }
        Err(err) => {
            state.set_status(format!("failed to disband: {err}"), now);
            Vec::new()
        }
    }
}

/// On success: remove just the dismissed Worker's pane — unlike
/// [`update_disbanded`], which clears every Worker at once, this touches
/// only the one the operator picked. `EmporiumState::pending_dismiss`
/// (stashed by [`confirm_kill_modal`] before this round trip was requested)
/// says which pane that is; taken here regardless of outcome so a failed or
/// foreign dismissal never leaves it stale for a later, unrelated
/// `WorkerDismissed`. The Director and any other Workers stay exactly as
/// staged; `Stage::remove` clamps `focused` the same way any other pane
/// removal does. The kill only fires if the pane was actually found staged
/// for this brigade — same "already gone, nothing to kill" tolerance as
/// [`update_disbanded`].
fn update_worker_dismissed(
    state: &mut EmporiumState,
    app: &mut App,
    brigade_id: BrigadeId,
    result: Result<(HashSet<String>, HashSet<String>), String>,
    now: Instant,
) -> Vec<Cmd> {
    let Some(key) = state.pending_dismiss.take() else {
        return Vec::new();
    };
    match result {
        Ok((hidden, directors)) => {
            app.set_hidden_worker_ids(hidden);
            app.set_directors(directors);
            let staged = matches!(
                &state.stage,
                Stage::Brigade { id, panes, .. } if *id == brigade_id && panes.contains(&key)
            );
            let title = app
                .row_for_id(key.as_str())
                .map(|row| row.display_title().to_string())
                .unwrap_or_else(|| key.as_str().to_string());
            state.set_status(format!("{title} dismissed from the brigade"), now);
            if staged {
                state.unstage(&key);
                vec![Cmd::KillPty { key }]
            } else {
                Vec::new()
            }
        }
        Err(err) => {
            state.set_status(format!("failed to dismiss worker: {err}"), now);
            Vec::new()
        }
    }
}

fn update_tick(
    state: &mut EmporiumState,
    brigade: &BrigadeConfig,
    relay: Vec<RelayObservation>,
    now: Instant,
) -> Vec<Cmd> {
    let mut cmds = Vec::new();

    // Phase two of a nudge: send the delayed submitting `\r` in its own PTY
    // write. A chunk boundary can carry meaning for the embedded PTY, so
    // bundling it with the nudge text risks the child reading both as one
    // paste instead of text-then-submit (docs/REQUIREMENTS.md, "Auto-relay").
    let mut i = 0;
    while i < state.pending_submits.len() {
        if now.saturating_duration_since(state.pending_submits[i].nudged_at) >= RELAY_SUBMIT_DELAY {
            let entry = state.pending_submits.swap_remove(i);
            cmds.push(Cmd::WritePty {
                key: entry.key,
                bytes: b"\r".to_vec(),
            });
        } else {
            i += 1;
        }
    }

    // Codex Worker kickoff: once a pending pane's own output has been quiet
    // for CODEX_KICKOFF_QUIET_PERIOD, ask the shell whether Codex has been
    // told to trust this pane's cwd (`Cmd::CheckWorkerDirectoryTrust`) —
    // never type the fixed line blind. The entry stays queued, re-asked
    // every tick, until `update_worker_directory_trust_checked` sees a
    // trusted answer and does the actual typing; this also means trust
    // granted later (the operator answers the prompt themselves) is picked
    // up on its own, with no separate recovery path needed.
    for pending in &state.pending_kickoffs {
        let quiet_since = state
            .last_output_at
            .get(&pending.key)
            .copied()
            .unwrap_or(pending.spawned_at);
        if now.saturating_duration_since(quiet_since) >= CODEX_KICKOFF_QUIET_PERIOD {
            cmds.push(Cmd::CheckWorkerDirectoryTrust {
                key: pending.key.clone(),
                cwd: pending.cwd.clone(),
            });
        }
    }

    if brigade.relay == RelayMode::Auto {
        let focused = state.stage.focused_key().cloned();
        for obs in relay {
            let is_focused = state.focus == Focus::Pane && focused.as_ref() == Some(&obs.key);
            let nudge = tick_relay_decision(
                &mut state.relay_states,
                &obs.token,
                now,
                obs.is_idle_this_tick,
                is_focused,
                state.last_forwarded_input.get(&obs.key).copied(),
                obs.has_unseen,
            );
            if nudge {
                cmds.push(Cmd::WritePty {
                    key: obs.key.clone(),
                    bytes: RELAY_NUDGE_LINE.as_bytes().to_vec(),
                });
                state.pending_submits.push(PendingSubmit {
                    key: obs.key,
                    nudged_at: now,
                });
                state.set_status(format!("relay: nudged {}", obs.token), now);
            }
        }
    }

    if let Some(set_at) = state.status_set_at
        && now.saturating_duration_since(set_at) >= STATUS_TIMEOUT
    {
        state.status = None;
        state.status_set_at = None;
    }

    if let Some(armed_at) = state.prefix_armed
        && now.saturating_duration_since(armed_at) >= PREFIX_ARM_TIMEOUT
    {
        state.prefix_armed = None;
    }

    cmds
}

/// Answers `Cmd::CheckWorkerDirectoryTrust`, always following a
/// [`PendingKickoff`] whose quiet period has already elapsed.
///
/// `trusted`: types the kickoff line, exactly as [`update_tick`] used to do
/// unconditionally once quiet — this is that same action, just gated. Not
/// trusted (or unknown, already collapsed into the same `false` by the
/// shell): the entry is left in [`EmporiumState::pending_kickoffs`]
/// untouched, so [`update_tick`] asks again next tick — no separate timeout,
/// no separate recovery path; trust granted later (the operator answers the
/// prompt in the pane themselves) is picked up on the very next check. A
/// missing entry (the pane unstaged between the check being issued and its
/// answer landing) is a no-op.
fn update_worker_directory_trust_checked(
    state: &mut EmporiumState,
    key: SessionKey,
    trusted: bool,
    now: Instant,
) -> Vec<Cmd> {
    let Some(pos) = state.pending_kickoffs.iter().position(|p| p.key == key) else {
        return Vec::new();
    };
    if !trusted {
        if !state.pending_kickoffs[pos].notified_untrusted {
            state.pending_kickoffs[pos].notified_untrusted = true;
            let name = key
                .worker_identity()
                .map(|(_, token)| token)
                .unwrap_or_else(|| key.as_str().to_string());
            state.set_status(
                format!(
                    "{name}: Codex hasn't been told to trust this directory yet — \
                     answer its prompt in the pane and it will pick up its first turn"
                ),
                now,
            );
        }
        return Vec::new();
    }
    let entry = state.pending_kickoffs.remove(pos);
    let cmds = vec![Cmd::WritePty {
        key: entry.key.clone(),
        bytes: CODEX_WORKER_KICKOFF_LINE.as_bytes().to_vec(),
    }];
    state.pending_submits.push(PendingSubmit {
        key: entry.key,
        nudged_at: now,
    });
    cmds
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::Activity;
    use crate::screen::Screen;

    use super::*;

    /// See `app::tests::test_instant`'s doc for why this exists and why it's
    /// not the clock access DISCIPLINE.md §3 forbids.
    #[allow(clippy::disallowed_methods)]
    fn test_instant() -> Instant {
        Instant::now()
    }

    fn row(id: &str) -> SessionRow {
        SessionRow {
            id: id.to_string(),
            agent: AgentKind::ClaudeCode,
            title: Some(id.to_string()),
            cwd: Some(PathBuf::from("/work/alpha")),
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
        }
    }

    fn app_with(rows: Vec<SessionRow>) -> App {
        App::new(rows)
    }

    fn brigade_config() -> BrigadeConfig {
        BrigadeConfig::default()
    }

    // --- layout / stage_tiles / pane_content (adapted to SessionKey) -------

    #[test]
    fn layout_reserves_sidebar_status_bar_and_details_panel() {
        let areas = layout(Rect::new(0, 0, 120, 40));
        assert_eq!(areas.status.height, 1);
        assert_eq!(areas.status.y, 39);
        assert_eq!(areas.sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(areas.pane.x, SIDEBAR_WIDTH);
        assert_eq!(areas.pane.width, 120 - SIDEBAR_WIDTH);
        assert_eq!(areas.pane.height, 39);
        assert_eq!(areas.summary.height, SUMMARY_HEIGHT);
    }

    #[test]
    fn layout_drops_the_details_panel_when_short() {
        let areas = layout(Rect::new(0, 0, 120, MIN_HEIGHT_FOR_SUMMARY));
        assert_eq!(areas.summary.height, 0);
    }

    #[test]
    fn pane_content_shrinks_by_the_border() {
        let content = pane_content(Rect::new(36, 0, 84, 40));
        assert_eq!(content.x, 37);
        assert_eq!(content.y, 1);
        assert_eq!(content.width, 82);
        assert_eq!(content.height, 38);
    }

    #[test]
    fn solo_stage_fills_the_whole_pane() {
        let area = Rect::new(36, 0, 84, 39);
        let key = SessionKey::from_id("a");
        assert_eq!(
            stage_tiles(area, &Stage::Solo(key.clone())),
            vec![(key, area)]
        );
    }

    #[test]
    fn empty_stage_has_no_tiles() {
        let area = Rect::new(36, 0, 84, 39);
        assert!(stage_tiles(area, &Stage::Empty).is_empty());
    }

    #[test]
    fn brigade_with_one_member_fills_the_pane() {
        let area = Rect::new(36, 0, 84, 39);
        let director = SessionKey::from_id("dir");
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone()],
            focused: 0,
        };
        assert_eq!(stage_tiles(area, &stage), vec![(director, area)]);
    }

    #[test]
    fn brigade_tiles_director_left_and_stacks_workers_right() {
        let area = Rect::new(36, 0, 84, 40);
        let director = SessionKey::from_id("dir");
        let w0 = SessionKey::from_id("w0");
        let w1 = SessionKey::from_id("w1");
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), w0.clone(), w1.clone()],
            focused: 0,
        };
        let tiles = stage_tiles(area, &stage);
        assert_eq!(tiles.len(), 3);

        let (director_key, director_rect) = &tiles[0];
        assert_eq!(director_key, &director);
        assert_eq!(director_rect.x, 36);
        assert_eq!(director_rect.width, 42);
        assert_eq!(director_rect.height, 40);

        let (w0_key, w0_rect) = &tiles[1];
        let (w1_key, w1_rect) = &tiles[2];
        assert_eq!((w0_key, w1_key), (&w0, &w1));
        assert_eq!(w0_rect.x, 78);
        assert_eq!(w1_rect.x, 78);
        assert_eq!(w0_rect.width, 42);
        assert!(w1_rect.y > w0_rect.y, "workers stack downward");
        assert_eq!(
            w0_rect.height + w1_rect.height,
            40,
            "workers fill the right column"
        );
    }

    #[test]
    fn focused_key_tracks_the_focused_pane() {
        assert_eq!(Stage::Empty.focused_key(), None);
        let solo = SessionKey::from_id("a");
        assert_eq!(Stage::Solo(solo.clone()).focused_key(), Some(&solo));
        let w1 = SessionKey::from_id("w1");
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![
                SessionKey::from_id("dir"),
                SessionKey::from_id("w0"),
                w1.clone(),
            ],
            focused: 2,
        };
        assert_eq!(stage.focused_key(), Some(&w1));
    }

    #[test]
    fn stage_remove_collapses_solo_to_empty() {
        let key = SessionKey::from_id("a");
        let mut stage = Stage::Solo(key.clone());
        stage.remove(&key);
        assert!(matches!(stage, Stage::Empty));
    }

    #[test]
    fn stage_remove_clamps_focused_in_a_brigade() {
        let mut stage = Stage::Brigade {
            id: 1,
            panes: vec![
                SessionKey::from_id("dir"),
                SessionKey::from_id("w1"),
                SessionKey::from_id("w2"),
            ],
            focused: 2,
        };
        stage.remove(&SessionKey::from_id("w2"));
        match &stage {
            Stage::Brigade { panes, focused, .. } => {
                assert_eq!(panes.len(), 2);
                assert_eq!(*focused, 1);
            }
            _ => panic!("expected Brigade"),
        }
    }

    #[test]
    fn stage_remove_last_pane_collapses_to_empty() {
        let mut stage = Stage::Brigade {
            id: 1,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        stage.remove(&SessionKey::from_id("dir"));
        assert!(matches!(stage, Stage::Empty));
    }

    #[test]
    fn session_key_classifies_synthetic_keys() {
        assert!(SessionKey::new_plain(std::path::Path::new("/work/a"), 0).is_synthetic());
        assert!(SessionKey::new_worker(1, "worker-1").is_synthetic());
        assert!(!SessionKey::from_id("00000000-real-uuid").is_synthetic());
    }

    #[test]
    fn mint_plain_key_produces_distinct_keys_for_repeated_opens_into_the_same_cwd() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let cwd = std::path::Path::new("/work/same");

        let k1 = state.mint_plain_key(cwd);
        let k2 = state.mint_plain_key(cwd);

        assert_ne!(k1, k2);
        assert!(k1.is_synthetic());
        assert!(k2.is_synthetic());
    }

    #[test]
    fn two_plain_opens_into_the_same_cwd_survive_independently_in_pending_opens_and_screens() {
        // The exact collision this guards: before the discriminator, two
        // `n`-opens into the same cwd (before either resolves a real id)
        // minted the identical `SessionKey`, so the second `pending_opens`
        // insert silently overwrote the first, and the second `Event::Spawned`
        // reset the first pane's already-rendered `screens` entry.
        let mut state = EmporiumState::new(PrefixKey::default());
        let cwd = std::path::Path::new("/work/same");

        let k1 = state.mint_plain_key(cwd);
        state.pending_opens.insert(k1.clone(), PendingOpen::Solo);
        let k2 = state.mint_plain_key(cwd);
        state.pending_opens.insert(k2.clone(), PendingOpen::Solo);

        assert_eq!(state.pending_opens.len(), 2);
        assert!(state.pending_opens.contains_key(&k1));
        assert!(state.pending_opens.contains_key(&k2));

        // Mirrors `update_spawned`'s unconditional `screens.insert` once per
        // `Event::Spawned` — both must retain their own entry, not collapse
        // onto one.
        state
            .screens
            .insert(k1.clone(), crate::screen::Screen::new(24, 80));
        state
            .screens
            .insert(k2.clone(), crate::screen::Screen::new(24, 80));
        assert_eq!(state.screens.len(), 2);
    }

    // --- Relay engine: should_nudge / tick_relay_decision (unchanged logic) -

    #[test]
    fn should_nudge_happy_path() {
        let now = test_instant();
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_blocks_without_unseen_messages() {
        let now = test_instant();
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            false,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_busy_blocks() {
        let now = test_instant();
        assert!(!should_nudge(
            now,
            0,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_single_tick_idle_blocks_debounce() {
        let now = test_instant();
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED - 1,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_focused_with_recent_input_blocks() {
        let now = test_instant();
        let last_input = now - Duration::from_millis(500);
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            true,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_focused_without_recent_input_is_allowed() {
        let now = test_instant();
        let last_input = now - RELAY_INPUT_QUIET_PERIOD - Duration::from_secs(1);
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            true,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_unfocused_ignores_recent_input() {
        let now = test_instant();
        let last_input = now - Duration::from_millis(10);
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_attempt_cap_blocks() {
        let now = test_instant();
        let state = NudgeState {
            last_nudge: Some(now - RELAY_NUDGE_COOLDOWN - Duration::from_secs(1)),
            attempts: RELAY_MAX_ATTEMPTS,
        };
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_cooldown_blocks_a_too_soon_second_attempt() {
        let now = test_instant();
        let state = NudgeState {
            last_nudge: Some(now - Duration::from_secs(10)),
            attempts: 1,
        };
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_cooldown_elapsed_allows_another_attempt() {
        let now = test_instant();
        let state = NudgeState {
            last_nudge: Some(now - RELAY_NUDGE_COOLDOWN - Duration::from_secs(1)),
            attempts: 1,
        };
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_first_nudge_is_exempt_from_the_cooldown_wait() {
        let now = test_instant();
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &NudgeState {
                last_nudge: None,
                attempts: 0,
            },
        ));
    }

    #[test]
    fn tick_relay_decision_requires_two_consecutive_idle_ticks() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = test_instant();

        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert!(tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_busy_tick_resets_the_idle_streak() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = test_instant();

        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(false),
            false,
            None,
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_unknown_live_entry_never_counts_as_idle() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = test_instant();

        for _ in 0..5 {
            assert!(!tick_relay_decision(
                &mut states,
                &token,
                now,
                None,
                false,
                None,
                true,
            ));
        }
    }

    #[test]
    fn tick_relay_decision_resets_on_drain_so_the_next_batch_starts_fresh() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = test_instant();

        tick_relay_decision(&mut states, &token, now, Some(true), false, None, true);
        assert!(tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert_eq!(states.get(&token).unwrap().nudge.attempts, 1);

        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            false,
        ));
        assert!(!states.contains_key(&token));

        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert_eq!(states.get(&token).unwrap().idle_streak, 1);
        assert_eq!(states.get(&token).unwrap().nudge.attempts, 0);
    }

    #[test]
    fn tick_relay_decision_stops_after_the_attempt_cap_even_past_cooldown() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let mut now = test_instant();

        tick_relay_decision(&mut states, &token, now, Some(true), false, None, true);
        for _ in 0..RELAY_MAX_ATTEMPTS {
            now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
            assert!(tick_relay_decision(
                &mut states,
                &token,
                now,
                Some(true),
                false,
                None,
                true,
            ));
        }
        assert_eq!(
            states.get(&token).unwrap().nudge.attempts,
            RELAY_MAX_ATTEMPTS
        );

        now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    #[test]
    fn relay_state_defaults_to_a_zero_streak_and_fresh_backoff() {
        let state = RelayState::default();
        assert_eq!(state.idle_streak, 0);
        assert_eq!(state.nudge.attempts, 0);
        assert!(state.nudge.last_nudge.is_none());
    }

    // --- Event-stream tests: update() end to end, no I/O -------------------

    #[test]
    fn activate_enter_on_a_known_row_resolves_membership_then_opens_then_stages_solo() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();
        let now = test_instant();

        // Enter on the sidebar: the row isn't a known brigade member yet, so
        // the first step is always resolving membership (see the module doc).
        let key_event = KeyEvent::new(KeyCode::Enter, Modifiers::NONE);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(key_event)),
            now,
        );
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::ResolveMembership { session_id }) ] if session_id == "sess-1"
        ));

        // Not a brigade member: opens solo.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "sess-1".to_string(),
                membership: None,
                members: None,
            },
            now,
        );
        let Some(Cmd::OpenEmbedded { key, target, .. }) = cmds.into_iter().next() else {
            panic!("expected an OpenEmbedded cmd");
        };
        assert_eq!(key, SessionKey::from_id("sess-1"));
        assert_eq!(target.id, "sess-1");

        // The shell reports success: stage becomes the solo pane, focus moves
        // to it.
        update(&mut state, &mut app, &brigade, Event::Spawned { key }, now);
        assert_eq!(
            state.stage.focused_key(),
            Some(&SessionKey::from_id("sess-1"))
        );
        assert_eq!(state.focus, Focus::Pane);
        assert!(state.screens.contains_key(&SessionKey::from_id("sess-1")));
    }

    #[test]
    fn rows_loaded_applies_the_superseded_set() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("a"), row("b")]);
        let brigade = brigade_config();
        let now = test_instant();
        assert!(!app.is_selected_superseded());

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::RowsLoaded {
                rows: vec![row("a"), row("b")],
                hidden: HashSet::new(),
                directors: HashSet::new(),
                superseded: ["a".to_string()].into_iter().collect(),
            },
            now,
        );

        // `row()` fixtures are `Activity::Alive`, so "a" stays visible
        // (selected, since it sorts first) despite being superseded —
        // this only checks the set reaches `App`, not the hidden-by-default
        // interaction (covered by `App`'s own tests).
        assert!(app.is_selected_superseded());
    }

    #[test]
    fn spawn_failed_sets_status_and_leaves_stage_untouched() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Empty;
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();
        let now = test_instant();

        let key = SessionKey::from_id("sess-1");
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::SpawnFailed {
                key,
                error: "already running elsewhere".to_string(),
            },
            now,
        );
        assert!(cmds.is_empty());
        assert!(matches!(state.stage, Stage::Empty));
        assert!(state.status.unwrap().contains("already running elsewhere"));
    }

    /// A staged brigade of a Director plus `worker_keys`, every pane holding
    /// a screen — the shape id discovery resolves into.
    fn staged_brigade(
        state: &mut EmporiumState,
        director: &SessionKey,
        worker_keys: &[SessionKey],
    ) {
        state.screens.insert(director.clone(), Screen::new(24, 80));
        let mut panes = vec![director.clone()];
        for key in worker_keys {
            state.screens.insert(key.clone(), Screen::new(24, 80));
            panes.push(key.clone());
        }
        state.stage = Stage::Brigade {
            id: 1,
            panes,
            focused: 0,
        };
    }

    #[test]
    fn discovery_rekeys_the_shells_pty_handle_along_with_the_screen_and_stage() {
        // The regression this pins: the core renames its own `screens`/
        // `Stage` entries, but the PTY handle lives in the shell, keyed by
        // the same `SessionKey`. Without a `RekeyPty` telling the shell, the
        // handle is orphaned under the synthetic key — the pane stops
        // accepting input and the shell's reaper drops the handle (closing
        // the PTY master, which SIGHUPs a live child on Unix).
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let pending = SessionKey::new_worker(1, "worker-1");
        staged_brigade(&mut state, &director, std::slice::from_ref(&pending));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: pending.clone(),
                session_id: "w1".to_string(),
                member: Some((1, "worker-1".to_string())),
            },
            test_instant(),
        );

        let discovered = SessionKey::from_id("w1");
        assert!(state.screens.contains_key(&discovered));
        assert!(!state.screens.contains_key(&pending));
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(panes, &[director, discovered.clone()]);
        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::RekeyPty { from, to } if *from == pending && *to == discovered
            )),
            "the shell must be told to rekey its handle: {cmds:?}"
        );
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::SetMemberSession { token, session_id, .. })
                if token == "worker-1" && session_id == "w1"
        )));
    }

    #[test]
    fn discovery_onto_an_id_another_pane_already_holds_is_refused_whole() {
        // Two Workers spawned into one cwd: if the second one's discovery
        // resolved to the id the first already took, both panes would fold
        // onto one key — same screen, same title, one child's handle
        // orphaned, and a store write handing the first Worker's session id
        // to the second's membership row.
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let taken = SessionKey::from_id("w1");
        let pending = SessionKey::new_worker(1, "worker-2");
        staged_brigade(&mut state, &director, &[taken.clone(), pending.clone()]);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: pending.clone(),
                session_id: "w1".to_string(),
                member: Some((1, "worker-2".to_string())),
            },
            test_instant(),
        );

        assert!(
            !cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::RekeyPty { .. } | Cmd::Store(StoreIntent::SetMemberSession { .. })
            )),
            "a colliding discovery must change nothing: {cmds:?}"
        );
        assert!(state.screens.contains_key(&pending), "pane 2 keeps its key");
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(panes, &[director, taken, pending]);
        assert!(state.status.unwrap().contains("collision"));
    }

    #[test]
    fn restaging_a_cell_reuses_a_worker_pane_that_has_no_id_yet_instead_of_respawning_it() {
        // The dogfooding bug: a Worker nobody has typed into has no session
        // file, so discovery can't name it and its store row stays NULL.
        // Re-opening the cell then hit stage_brigade's "no id -> spawn one"
        // arm again and started a *second* claude under the same synthetic
        // key, which replaced the live child's handle in the shell (closing
        // its PTY, killing it on Unix) and blanked its pane.
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        state.screens.insert(director.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        let now = test_instant();
        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            ("worker-1".to_string(), BrigadeRole::Worker, None),
        ];
        let opens = |cmds: &[Cmd]| -> Vec<SessionKey> {
            cmds.iter()
                .filter_map(|cmd| match cmd {
                    Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
                    _ => None,
                })
                .collect()
        };
        let activate = |state: &mut EmporiumState, app: &mut App| {
            state.pending_membership = Some(PendingMembership::Activate);
            update(
                state,
                app,
                &brigade,
                Event::MembershipResolved {
                    session_id: "dir".to_string(),
                    membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                    members: Some(roster.clone()),
                },
                now,
            )
        };

        // First open: the Worker has no id, so it is spawned fresh.
        let cmds = activate(&mut state, &mut app);
        let worker = SessionKey::new_worker(1, "worker-1");
        assert_eq!(opens(&cmds), std::slice::from_ref(&worker));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: worker.clone(),
            },
            now,
        );
        assert!(state.screens.contains_key(&worker));

        // Re-open the same cell while that Worker is still unidentified:
        // its pane is alive, so it must be reused, not spawned again.
        let cmds = activate(&mut state, &mut app);
        assert!(
            opens(&cmds).is_empty(),
            "the live Worker must not be respawned: {cmds:?}"
        );
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(panes, &[director, worker]);
    }

    // --- compact-fork tracking: Event::MemberSessionForked -----------------

    #[test]
    fn member_session_forked_renames_the_pane_preserving_order_focus_and_screen() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let old = SessionKey::from_id("w1-old");
        let other = SessionKey::from_id("w2");
        staged_brigade(&mut state, &director, &[old.clone(), other.clone()]);
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1; // focus the forking worker's own pane
        }
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "worker-1".to_string(),
                old_id: "w1-old".to_string(),
                new_id: "w1-new".to_string(),
            },
            now,
        );

        let new_key = SessionKey::from_id("w1-new");
        assert!(state.screens.contains_key(&new_key));
        assert!(!state.screens.contains_key(&old));
        let Stage::Brigade { panes, focused, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(panes, &[director, new_key.clone(), other]);
        assert_eq!(*focused, 1, "focus stays on the same pane position");
        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::RekeyPty { from, to } if *from == old && *to == new_key
            )),
            "the shell must be told to rekey its handle: {cmds:?}"
        );
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::SetMemberSession { token, session_id, .. })
                if token == "worker-1" && session_id == "w1-new"
        )));
    }

    #[test]
    fn member_session_forked_collision_skips_the_rename_but_still_updates_the_store() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let old = SessionKey::from_id("w1-old");
        staged_brigade(&mut state, &director, std::slice::from_ref(&old));
        // The operator separately opened the continuation as its own solo
        // pane before banto ever noticed the fork.
        state
            .screens
            .insert(SessionKey::from_id("w1-new"), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "worker-1".to_string(),
                old_id: "w1-old".to_string(),
                new_id: "w1-new".to_string(),
            },
            now,
        );

        assert!(
            state.screens.contains_key(&old),
            "the old pane is left alone"
        );
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert_eq!(panes, &[director, old]);
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::RekeyPty { .. })),
            "a collision must not rekey: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::Store(StoreIntent::SetMemberSession { session_id, .. })
                    if session_id == "w1-new"
            )),
            "the store must still learn the truth: {cmds:?}"
        );
        assert!(state.status.unwrap().contains("collision"));
    }

    #[test]
    fn member_session_forked_when_old_id_is_not_staged_updates_only_the_store() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "worker-1".to_string(),
                old_id: "w1-old".to_string(),
                new_id: "w1-new".to_string(),
            },
            now,
        );

        assert_eq!(cmds.len(), 1, "store-only: {cmds:?}");
        assert!(matches!(
            cmds[0],
            Cmd::Store(StoreIntent::SetMemberSession { .. })
        ));
    }

    #[test]
    fn member_session_forked_fact_is_idempotent_under_duplicate_delivery() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let old = SessionKey::from_id("w1-old");
        staged_brigade(&mut state, &director, std::slice::from_ref(&old));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let fact = || Event::MemberSessionForked {
            brigade_id: 1,
            token: "worker-1".to_string(),
            old_id: "w1-old".to_string(),
            new_id: "w1-new".to_string(),
        };

        let first = update(&mut state, &mut app, &brigade, fact(), now);
        assert!(first.iter().any(|cmd| matches!(cmd, Cmd::RekeyPty { .. })));

        // The observation that produces this fact repeats until the store
        // row catches up: a second delivery must not panic, double-rename,
        // or emit a second RekeyPty — `old_id`'s pane is already gone.
        let second = update(&mut state, &mut app, &brigade, fact(), now);
        assert!(
            !second.iter().any(|cmd| matches!(cmd, Cmd::RekeyPty { .. })),
            "already renamed; a repeat must be store-only: {second:?}"
        );
        assert!(
            second
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::SetMemberSession { .. })))
        );
    }

    // --- worker model on resume ---------------------------------------------

    #[test]
    fn stage_brigade_passes_the_configured_worker_model_to_a_resumed_worker_but_never_to_the_director()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("w1")]);
        let brigade = brigade_config(); // worker_model defaults to "sonnet"
        let now = test_instant();
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "worker-1".to_string(),
                BrigadeRole::Worker,
                Some("w1".to_string()),
            ),
        ];
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(roster),
            },
            now,
        );

        let models: Vec<(String, Option<String>)> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::OpenEmbedded { target, model, .. } => Some((target.id.clone(), model.clone())),
                _ => None,
            })
            .collect();
        assert!(
            models
                .iter()
                .any(|(id, model)| id == "dir" && model.is_none()),
            "the Director must never carry --model: {models:?}"
        );
        assert!(
            models
                .iter()
                .any(|(id, model)| id == "w1" && model.as_deref() == Some("sonnet")),
            "a resumed Worker must carry the configured worker_model: {models:?}"
        );
    }

    // --- worker_agent: config wired into every open_worker call site -------

    #[test]
    fn resolve_worker_agent_follows_the_configured_setting() {
        assert_eq!(
            resolve_worker_agent(WorkerAgentSetting::Inherit, AgentKind::Codex),
            AgentKind::Codex,
            "Inherit copies the Director's own product"
        );
        assert_eq!(
            resolve_worker_agent(WorkerAgentSetting::Inherit, AgentKind::ClaudeCode),
            AgentKind::ClaudeCode
        );
        assert_eq!(
            resolve_worker_agent(WorkerAgentSetting::Select, AgentKind::Codex),
            AgentKind::Codex,
            "stage-1 stand-in: Select behaves like Inherit until the formation modal lands"
        );
        assert_eq!(
            resolve_worker_agent(WorkerAgentSetting::Claude, AgentKind::Codex),
            AgentKind::ClaudeCode,
            "Claude pins regardless of the Director's own product"
        );
        assert_eq!(
            resolve_worker_agent(WorkerAgentSetting::Codex, AgentKind::ClaudeCode),
            AgentKind::Codex,
            "Codex pins regardless of the Director's own product"
        );
    }

    #[test]
    fn stage_brigade_fresh_worker_spawn_inherits_a_claude_directors_product_unchanged() {
        // The explicit regression this round asked for: with the default
        // `[brigade] worker_agent = "inherit"`, a Claude Director's cell
        // must not change by a single byte.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config(); // worker_agent defaults to Inherit
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            ("worker-1".to_string(), BrigadeRole::Worker, None), // fresh, no id yet
        ];
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(roster),
            },
            test_instant(),
        );

        let worker_target = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded { target, .. } if target.id.is_empty() => Some(target),
            _ => None,
        });
        assert_eq!(worker_target.map(|t| t.agent), Some(AgentKind::ClaudeCode));
    }

    #[test]
    fn stage_brigade_fresh_worker_spawn_uses_an_explicitly_configured_codex_setting() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]); // dir's own row is Claude...
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex, // ...but pinned regardless
            ..BrigadeConfig::default()
        };
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            ("worker-1".to_string(), BrigadeRole::Worker, None),
        ];
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(roster),
            },
            test_instant(),
        );

        let worker_target = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded { target, .. } if target.id.is_empty() => Some(target),
            _ => None,
        });
        assert_eq!(worker_target.map(|t| t.agent), Some(AgentKind::Codex));
    }

    #[test]
    fn brigade_formed_kills_and_rewires_the_director_when_its_pane_is_already_open() {
        // The residual-race case `PendingDirectorWiring` exists for: by the
        // time `Event::BrigadeFormed` lands, the store record already
        // exists, so this can no longer ask — it can only kill and reopen
        // wired, same as the interactive gate, without the confirm step.
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex,
            ..BrigadeConfig::default()
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::BrigadeFormed {
                director_row_id: "dir".to_string(),
                name: "cell".to_string(),
                cwd: PathBuf::from("/work"),
                // Resolved at press-time in the real flow (`update_membership_resolved`);
                // supplied directly here since this test is about what
                // `update_brigade_formed` does with it, not how it got resolved.
                worker_agent: AgentKind::Codex,
                worker_model: "gpt-5.6-terra".to_string(),
                result: Ok((1, vec!["worker-1".to_string()])),
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: SessionKey::from_id("dir")
            }],
            "must not spawn anything until the existing pane is confirmed gone"
        );
        assert!(state.screens.contains_key(&SessionKey::from_id("dir")));

        // The shell answers once the kill actually lands.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("dir"),
            },
            test_instant(),
        );

        assert!(!state.screens.contains_key(&SessionKey::from_id("dir")));
        assert!(state.pending_director_wiring.is_none());
        let director = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded {
                target, brigade, ..
            } if target.id == "dir" => Some((target.agent, brigade.clone())),
            _ => None,
        });
        assert_eq!(
            director,
            Some((
                AgentKind::ClaudeCode,
                Some((1, "director".to_string(), BrigadeRole::Director))
            )),
            "must reopen wired to the brigade, not just re-stage: {cmds:?}"
        );
    }

    #[test]
    fn brigade_formed_then_spawned_worker_uses_the_configured_worker_agent_when_director_spawns_after()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex,
            ..BrigadeConfig::default()
        };

        // Director's own screen doesn't exist yet: `PendingOpen::BrigadeDirector`
        // must carry `worker_agent`/`worker_model` through to the later
        // `Event::Spawned`.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::BrigadeFormed {
                director_row_id: "dir".to_string(),
                name: "cell".to_string(),
                cwd: PathBuf::from("/work"),
                worker_agent: AgentKind::Codex,
                worker_model: "gpt-5.6-terra".to_string(),
                result: Ok((1, vec!["worker-1".to_string()])),
            },
            test_instant(),
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: SessionKey::from_id("dir"),
            },
            test_instant(),
        );

        let worker = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded { target, model, .. } if target.id.is_empty() => {
                Some((target.agent, model.clone()))
            }
            _ => None,
        });
        assert_eq!(
            worker,
            Some((AgentKind::Codex, Some("gpt-5.6-terra".to_string())))
        );
    }

    #[test]
    fn add_worker_resolves_the_configured_agent_from_the_staged_directors_row() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director],
            focused: 0,
        };
        let mut codex_director_row = row("dir");
        codex_director_row.agent = AgentKind::Codex;
        let mut app = app_with(vec![codex_director_row]);
        let brigade = brigade_config(); // Inherit — must follow the staged Director's own product

        let cmds = add_worker(&mut state, &mut app, &brigade);

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::AddWorker {
                worker_agent: AgentKind::Codex,
                ..
            })]
        ));
    }

    #[test]
    fn worker_added_passes_the_carried_agent_and_model_straight_to_open_worker() {
        // `worker_agent`/`worker_model` are already fully resolved by the
        // time this event exists (`add_worker`, at `B`-press) — this only
        // pins that `update_worker_added` passes them through unchanged
        // rather than re-deriving anything from `brigade`.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerAdded {
                brigade_id: 1,
                cwd: PathBuf::from("/work"),
                worker_agent: AgentKind::Codex,
                worker_model: "gpt-5.6-terra".to_string(),
                result: Ok("worker-2".to_string()),
            },
            test_instant(),
        );

        match cmds.as_slice() {
            [Cmd::OpenEmbedded { target, model, .. }] => {
                assert_eq!(target.agent, AgentKind::Codex);
                assert_eq!(model.as_deref(), Some("gpt-5.6-terra"));
            }
            other => panic!("expected exactly one OpenEmbedded: {other:?}"),
        }
    }

    // --- worker_agent = "select": the formation-time picker modal ----------

    #[test]
    fn brigade_key_forms_immediately_when_worker_agent_is_not_select() {
        // The explicit regression for this half too: the default `[brigade]
        // worker_agent = "inherit"` must keep forming a brigade the instant
        // `B` resolves, exactly as before this modal existed.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        state.pending_membership = Some(PendingMembership::BrigadeKey);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
            },
            test_instant(),
        );

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::FormBrigade {
                worker_agent: AgentKind::ClaudeCode,
                ..
            })]
        ));
        assert!(app.modal().is_none());
    }

    #[test]
    fn brigade_key_opens_the_director_reopen_modal_when_its_pane_is_already_open() {
        // Not Codex-specific: a Claude Director's own pane can't carry MCP
        // config any more than a Codex one can carry `-c` overrides, so this
        // must fire regardless of product.
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        state.pending_membership = Some(PendingMembership::BrigadeKey);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
            },
            test_instant(),
        );

        assert!(
            cmds.is_empty(),
            "must not form, kill, or check anything until the operator answers"
        );
        assert!(matches!(
            app.modal(),
            Some(Modal::ConfirmDirectorReopen { director_row_id, name })
                if director_row_id == "dir" && name == "dir"
        ));
    }

    #[test]
    fn confirming_director_reopen_kills_the_pane_then_forms_once_it_actually_exits() {
        // The full interactive round trip, gate to `FormBrigade`: this is
        // the "confirming reopens wired, not just re-stages" requirement,
        // exercised through the *interactive* gate rather than the
        // residual-race path `brigade_formed_kills_and_rewires_...` covers.
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        app.open_confirm_director_reopen_modal("dir".to_string(), "dir".to_string());

        let cmds = confirm_director_reopen_modal(&mut state, &mut app);

        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: SessionKey::from_id("dir")
            }]
        );
        assert!(app.modal().is_none());
        assert!(state.pending_director_reopen.is_some());
        assert!(
            state.screens.contains_key(&SessionKey::from_id("dir")),
            "the pane isn't actually gone until Event::PtyExited says so"
        );

        let brigade = brigade_config();
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("dir"),
            },
            test_instant(),
        );

        assert!(!state.screens.contains_key(&SessionKey::from_id("dir")));
        assert!(state.pending_director_reopen.is_none());
        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::FormBrigade {
                worker_agent: AgentKind::ClaudeCode,
                ..
            })]
        ));
    }

    #[test]
    fn esc_on_director_reopen_modal_cancels_the_formation_entirely() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        app.open_confirm_director_reopen_modal("dir".to_string(), "dir".to_string());

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc);

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
        assert!(
            state.pending_director_reopen.is_none(),
            "nothing should be waiting on a kill that was never issued"
        );
        assert!(
            state.screens.contains_key(&SessionKey::from_id("dir")),
            "Esc must not touch the still-running pane"
        );
    }

    #[test]
    fn director_reopen_gate_runs_before_the_select_picker_and_codex_trust_check() {
        // All three formation gates in one press: pane already open, AND
        // `worker_agent = "select"`, AND (once picked) Codex untrusted.
        // Director-reopen must resolve completely — kill, wait, exit —
        // before the picker ever opens; the picker's own Codex pick must
        // then resolve before `FormBrigade` is even considered.
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Select,
            ..BrigadeConfig::default()
        };
        state.pending_membership = Some(PendingMembership::BrigadeKey);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
            },
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(
            matches!(app.modal(), Some(Modal::ConfirmDirectorReopen { .. })),
            "director-reopen must come first, not the picker"
        );

        let cmds = confirm_director_reopen_modal(&mut state, &mut app);
        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: SessionKey::from_id("dir")
            }]
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("dir"),
            },
            test_instant(),
        );
        assert!(cmds.is_empty(), "nothing forms yet — the picker is next");
        assert!(matches!(app.modal(), Some(Modal::WorkerAgentPicker(_))));

        app.modal_toggle_agent(); // Claude -> Codex
        let cmds = confirm_worker_agent_modal(&mut state, &mut app);

        assert_eq!(
            cmds,
            vec![Cmd::CheckCodexTrust],
            "the picker's Codex pick must route through the trust check next"
        );
        assert!(state.pending_codex_trust_formation.is_some());
    }

    #[test]
    fn brigade_key_checks_codex_trust_before_forming_when_worker_agent_resolves_to_codex() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex,
            ..BrigadeConfig::default()
        };
        state.pending_membership = Some(PendingMembership::BrigadeKey);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::CheckCodexTrust],
            "must not form yet — a Codex Worker needs the trust check first"
        );
        assert!(app.modal().is_none());
        assert!(state.pending_codex_trust_formation.is_some());
    }

    #[test]
    fn brigade_key_opens_the_worker_agent_modal_when_select_is_configured() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Select,
            ..BrigadeConfig::default()
        };
        state.pending_membership = Some(PendingMembership::BrigadeKey);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: None,
                members: None,
            },
            test_instant(),
        );

        assert!(cmds.is_empty(), "nothing forms until the picker confirms");
        assert!(matches!(app.modal(), Some(Modal::WorkerAgentPicker(_))));
    }

    #[test]
    fn confirm_worker_agent_modal_forms_the_brigade_with_the_chosen_agent_and_model() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_brigade_formation = Some(PendingBrigadeFormation {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_count: 2,
        });
        app.open_worker_agent_modal(
            AgentKind::Codex,
            "sonnet".to_string(),
            "gpt-5.6-terra".to_string(),
        );
        app.modal_toggle_agent(); // Codex -> Claude, re-seeds the model input
        app.modal_push_char('!'); // then the operator edits it further

        let cmds = confirm_worker_agent_modal(&mut state, &mut app);

        assert!(
            matches!(
                cmds.as_slice(),
                [Cmd::Store(StoreIntent::FormBrigade {
                    director_row_id,
                    name,
                    cwd,
                    worker_count: 2,
                    worker_agent: AgentKind::ClaudeCode,
                    worker_model,
                })] if director_row_id == "dir"
                    && name == "cell"
                    && cwd == &PathBuf::from("/work")
                    && worker_model == "sonnet!"
            ),
            "expected a FormBrigade carrying the picker's own final state: {cmds:?}"
        );
        assert!(app.modal().is_none());
        assert!(state.pending_brigade_formation.is_none());
    }

    #[test]
    fn confirm_worker_agent_modal_checks_codex_trust_instead_of_forming_directly_when_codex_is_chosen()
     {
        // The whole point of this round trip: a Codex pick must not form the
        // brigade on the spot, because trust can only be verified by the
        // shell (`Cmd::CheckCodexTrust`) — see `update_codex_trust_checked`.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_brigade_formation = Some(PendingBrigadeFormation {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_count: 2,
        });
        app.open_worker_agent_modal(
            AgentKind::ClaudeCode,
            "sonnet".to_string(),
            "gpt-5.6-terra".to_string(),
        );
        app.modal_toggle_agent(); // Claude -> Codex

        let cmds = confirm_worker_agent_modal(&mut state, &mut app);

        assert_eq!(cmds, vec![Cmd::CheckCodexTrust]);
        assert!(app.modal().is_none());
        assert!(state.pending_brigade_formation.is_none());
        assert!(state.pending_codex_trust_formation.is_some());
    }

    fn pending_codex_trust_formation() -> PendingCodexTrustFormation {
        PendingCodexTrustFormation {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_count: 2,
            worker_agent: AgentKind::Codex,
            worker_model: "gpt-5.6-terra".to_string(),
        }
    }

    #[test]
    fn codex_trust_checked_forms_the_brigade_when_primed() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_codex_trust_formation = Some(pending_codex_trust_formation());

        let cmds = update_codex_trust_checked(&mut state, &mut app, true, true, test_instant());

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::FormBrigade {
                worker_agent: AgentKind::Codex,
                ..
            })]
        ));
        assert!(app.modal().is_none());
        assert!(state.pending_codex_trust_formation.is_none());
    }

    #[test]
    fn codex_trust_checked_opens_the_confirm_modal_when_not_primed_but_launchable() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_codex_trust_formation = Some(pending_codex_trust_formation());

        let cmds = update_codex_trust_checked(&mut state, &mut app, false, true, test_instant());

        assert!(cmds.is_empty(), "nothing forms until the operator acts");
        assert!(matches!(app.modal(), Some(Modal::ConfirmCodexTrust)));
        assert!(
            state.pending_codex_trust_formation.is_none(),
            "consumed unconditionally — resuming later would need to verify \
             the approval actually took, which banto cannot do"
        );
    }

    #[test]
    fn codex_trust_checked_forms_anyway_and_reports_the_space_when_not_launchable() {
        // Not launchable outranks "not primed": no pane could fix this one,
        // so there is nothing useful to confirm, unlike the primed-eventually
        // case above.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_codex_trust_formation = Some(pending_codex_trust_formation());

        let cmds = update_codex_trust_checked(&mut state, &mut app, false, false, test_instant());

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::FormBrigade {
                worker_agent: AgentKind::Codex,
                ..
            })]
        ));
        assert!(app.modal().is_none());
        let status = state.status.as_deref().unwrap_or_default();
        assert!(status.contains("space"), "must name the cause: {status}");
    }

    #[test]
    fn codex_trust_checked_is_a_no_op_without_a_pending_formation() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);

        let cmds = update_codex_trust_checked(&mut state, &mut app, true, true, test_instant());

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
    }

    #[test]
    fn confirm_codex_trust_modal_opens_a_solo_pane_and_never_touches_pending_brigade_formation() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        app.open_confirm_codex_trust_modal();

        let cmds = confirm_codex_trust_modal(&mut state, &mut app);

        let key = match cmds.as_slice() {
            [Cmd::OpenCodexTrustPane { key }] => key.clone(),
            other => panic!("expected exactly one OpenCodexTrustPane: {other:?}"),
        };
        assert!(app.modal().is_none());
        assert!(matches!(
            state.pending_opens.get(&key),
            Some(PendingOpen::Solo)
        ));
    }

    #[test]
    fn confirm_codex_trust_modal_is_a_no_op_when_a_different_modal_is_open() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        app.open_confirm_archive_modal();

        let cmds = confirm_codex_trust_modal(&mut state, &mut app);

        assert!(cmds.is_empty());
        assert!(matches!(app.modal(), Some(Modal::ConfirmArchive { .. })));
    }

    #[test]
    fn esc_on_worker_agent_modal_cancels_the_formation_entirely() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_brigade_formation = Some(PendingBrigadeFormation {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_count: 1,
        });
        app.open_worker_agent_modal(AgentKind::Codex, String::new(), String::new());

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc);

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
        assert!(
            state.pending_brigade_formation.is_none(),
            "a cancelled picker must never leave a formation for a later confirm to pick up"
        );
    }

    #[test]
    fn backtab_toggles_the_worker_agent_modals_agent() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        app.open_worker_agent_modal(AgentKind::ClaudeCode, String::new(), String::new());

        update_modal_key(&mut state, &mut app, KeyCode::BackTab);

        assert!(matches!(
            app.modal(),
            Some(Modal::WorkerAgentPicker(picker)) if picker.agent() == AgentKind::Codex
        ));
    }

    #[test]
    fn add_worker_opens_the_worker_agent_modal_when_select_is_configured() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        state.stage = Stage::Brigade {
            id: 7,
            panes: vec![director],
            focused: 0,
        };
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Select,
            ..BrigadeConfig::default()
        };

        let cmds = add_worker(&mut state, &mut app, &brigade);

        assert!(cmds.is_empty(), "nothing added until the picker confirms");
        assert!(matches!(app.modal(), Some(Modal::WorkerAgentPicker(_))));
    }

    #[test]
    fn confirm_worker_agent_modal_adds_the_worker_with_the_chosen_agent_and_model() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_worker_addition = Some(PendingWorkerAddition {
            brigade_id: 7,
            cwd: PathBuf::from("/work"),
        });
        app.open_worker_agent_modal(
            AgentKind::ClaudeCode,
            "sonnet".to_string(),
            "gpt-5.6-terra".to_string(),
        );
        app.modal_push_char('!'); // edit the seeded Claude default, no toggle

        let cmds = confirm_worker_agent_modal(&mut state, &mut app);

        assert!(
            matches!(
                cmds.as_slice(),
                [Cmd::Store(StoreIntent::AddWorker {
                    brigade_id: 7,
                    cwd,
                    worker_agent: AgentKind::ClaudeCode,
                    worker_model,
                })] if cwd == &PathBuf::from("/work") && worker_model == "sonnet!"
            ),
            "expected an AddWorker carrying the picker's own final state: {cmds:?}"
        );
        assert!(app.modal().is_none());
        assert!(state.pending_worker_addition.is_none());
        assert!(
            state.pending_brigade_formation.is_none(),
            "an addition must never leave a formation stash behind either"
        );
    }

    #[test]
    fn confirm_worker_agent_modal_never_trust_checks_a_codex_addition() {
        // Deliberate difference from formation — see `PendingWorkerAddition`'s
        // own doc and `confirm_worker_agent_modal`'s for why.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_worker_addition = Some(PendingWorkerAddition {
            brigade_id: 7,
            cwd: PathBuf::from("/work"),
        });
        app.open_worker_agent_modal(AgentKind::ClaudeCode, String::new(), String::new());
        app.modal_toggle_agent(); // Claude -> Codex

        let cmds = confirm_worker_agent_modal(&mut state, &mut app);

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::AddWorker {
                worker_agent: AgentKind::Codex,
                ..
            })]
        ));
        assert!(state.pending_codex_trust_formation.is_none());
    }

    #[test]
    fn esc_on_worker_agent_modal_cancels_the_addition_too() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        state.pending_worker_addition = Some(PendingWorkerAddition {
            brigade_id: 7,
            cwd: PathBuf::from("/work"),
        });
        app.open_worker_agent_modal(AgentKind::Codex, String::new(), String::new());

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc);

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
        assert!(
            state.pending_worker_addition.is_none(),
            "a cancelled picker must never leave an addition for a later confirm to pick up"
        );
    }

    #[test]
    fn pty_output_feeds_the_screen() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"hi".to_vec(),
            },
            test_instant(),
        );

        let screen = state.screens.get(&key).unwrap();
        assert_eq!(screen.screen().cell(0, 0).unwrap().contents(), "h");
        assert_eq!(screen.screen().cell(0, 1).unwrap().contents(), "i");
    }

    // --- mouse: SGR forwarding gated on the child's own mouse-protocol state

    /// A left-button-down at the top-left corner of `state`'s pane content
    /// area — assumes a `Solo` (or single-member `Brigade`) stage, whose one
    /// tile fills the whole pane area, so this always lands inside it.
    fn click_inside_pane(state: &EmporiumState) -> MouseEvent {
        let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
        let content = pane_content(areas.pane);
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: content.x,
            row: content.y,
        }
    }

    /// A `Screen` already sized to match `state`'s own pane content area
    /// (same `Solo`/single-member `Brigade` assumption as
    /// [`click_inside_pane`]) — so `update`'s own per-tick `resize_staged_tiles`
    /// has nothing to correct and doesn't add an incidental `Cmd::ResizePty`
    /// these tests aren't about.
    fn screen_sized_for_pane(state: &EmporiumState) -> Screen {
        let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
        let content = pane_content(areas.pane);
        Screen::new(content.height, content.width)
    }

    #[test]
    fn mouse_click_over_a_pane_with_no_screen_focuses_it_but_forwards_nothing() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        // No `state.screens` entry at all: nothing spawned yet, or nothing
        // heard from it — either way, it can't have asked for mouse.
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mouse = click_inside_pane(&state);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert_eq!(state.focus, Focus::Pane);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::WritePty { .. })),
            "nothing should be forwarded to a child that never asked: {cmds:?}"
        );
    }

    #[test]
    fn mouse_click_over_a_pane_that_enabled_sgr_mouse_forwards_the_report() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        let mut screen = screen_sized_for_pane(&state);
        screen.process(b"\x1b[?1003h\x1b[?1006h"); // any-motion mode, SGR encoding
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mouse = click_inside_pane(&state);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert_eq!(state.focus, Focus::Pane);
        match cmds.as_slice() {
            [
                Cmd::WritePty {
                    key: got_key,
                    bytes,
                },
            ] => {
                assert_eq!(got_key, &key);
                assert_eq!(bytes, b"\x1b[<0;1;1M");
            }
            other => panic!("expected exactly one WritePty, got {other:?}"),
        }
    }

    #[test]
    fn mouse_click_over_a_pane_with_a_non_sgr_encoding_forwards_nothing() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        let mut screen = screen_sized_for_pane(&state);
        // Mouse mode is on, but in the UTF-8 encoding, not SGR — banto has
        // no encoder for this, so it must refuse rather than send SGR bytes
        // this child never asked for.
        screen.process(b"\x1b[?1000h\x1b[?1005h");
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mouse = click_inside_pane(&state);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::WritePty { .. })),
            "a non-SGR encoding must not be sent SGR bytes: {cmds:?}"
        );
    }

    #[test]
    fn wheel_over_a_pane_that_does_not_want_sgr_mouse_scrolls_its_own_scrollback() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut screen = screen_sized_for_pane(&state);
        // No mouse mode enabled at all, so this pane never asked for SGR —
        // enough output written first that there's real scrollback to move
        // into.
        for i in 0..50 {
            screen.process(format!("line{i}\r\n").as_bytes());
        }
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..click_inside_pane(&state)
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert!(
            cmds.is_empty(),
            "scrolling a pane's own scrollback is pure state, no Cmd needed: {cmds:?}"
        );
        assert_eq!(state.screens.get(&key).unwrap().scrollback(), 3);
    }

    #[test]
    fn wheel_over_a_pane_that_wants_sgr_mouse_is_forwarded_not_consumed() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut screen = screen_sized_for_pane(&state);
        screen.process(b"\x1b[?1003h\x1b[?1006h"); // any-motion mode, SGR encoding
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..click_inside_pane(&state)
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        match cmds.as_slice() {
            [
                Cmd::WritePty {
                    key: got_key,
                    bytes,
                },
            ] => {
                assert_eq!(got_key, &key);
                assert_eq!(bytes, b"\x1b[<64;1;1M");
            }
            other => panic!("expected exactly one WritePty, got {other:?}"),
        }
        assert_eq!(
            state.screens.get(&key).unwrap().scrollback(),
            0,
            "a child that wants mouse handles its own scrollback; banto must not also consume the wheel"
        );
    }

    #[test]
    fn left_click_on_another_pane_moves_focus_even_when_the_focused_pane_does_not_want_sgr_mouse() {
        // The regression this guards: a child that never asks for SGR mouse
        // (Codex, measured) must not be able to trap focus on its own pane.
        // Host mouse capture is unconditional now (see `update_mouse`'s own
        // doc), so the click always reaches here regardless of what the
        // currently-focused child wants; hit-testing the target pane must
        // not depend on `wants_sgr_mouse` either. Neither pane has a
        // `Screen` here, so neither could want SGR even if asked.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker.clone()],
            focused: 0,
        };
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
        let tiles = stage_tiles(areas.pane, &state.stage);
        let (_, worker_rect) = tiles.iter().find(|(key, _)| *key == worker).unwrap();
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: worker_rect.x,
            row: worker_rect.y,
        };

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 1),
            _ => panic!("expected a Brigade stage"),
        }
    }

    #[test]
    fn sidebar_click_works_while_focus_is_on_a_pane_that_does_not_want_sgr_mouse() {
        // Same regression, the sidebar side: a click there must reach
        // `update_mouse`'s sidebar branch regardless of what the
        // currently-focused pane wants — that branch itself never checked
        // `state.focus` to begin with, but this pins the case that used to
        // fail at the I/O boundary (capture released, event never arriving).
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: areas.sidebar.x + 1,
            row: areas.sidebar.y + 1,
        };

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert_eq!(state.focus, Focus::Sidebar);
    }

    #[test]
    fn sidebar_scroll_works_while_focus_is_on_a_pane_that_does_not_want_sgr_mouse() {
        // `App::scroll` moves the viewport, not the selection, so a
        // one-row viewport over several rows is what makes it observable.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        let mut app = app_with((1..=5).map(|n| row(&format!("sess-{n}"))).collect());
        app.set_viewport_height(1);
        let brigade = brigade_config();
        let before = app.selected_in_viewport();

        let areas = layout(Rect::new(0, 0, state.size.0, state.size.1));
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: areas.sidebar.x,
            row: areas.sidebar.y,
        };

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Mouse(mouse)),
            test_instant(),
        );

        assert_ne!(app.selected_in_viewport(), before);
    }

    #[test]
    fn pty_exited_on_a_staged_solo_collapses_to_empty_with_status() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited { key: key.clone() },
            test_instant(),
        );

        assert!(matches!(state.stage, Stage::Empty));
        assert!(!state.screens.contains_key(&key));
        assert!(state.status.unwrap().contains("session ended"));
        assert_eq!(
            state.focus,
            Focus::Sidebar,
            "focus must not outlive the pane it points at"
        );
    }

    #[test]
    fn the_sidebar_answers_keys_again_after_its_last_pane_exits() {
        // The assertion above is the state; this is what the operator
        // experiences. A solo pane's exit used to leave `Focus::Pane`
        // behind, so every later key was forwarded to a pane that no longer
        // existed and the sidebar never saw one — a correctly drawn screen
        // that answered nothing.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut app = app_with(vec![row("sess-1"), row("sess-2")]);
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited { key: key.clone() },
            now,
        );

        let before = app.selected_row().map(|row| row.id.clone());
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('j'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert_ne!(
            app.selected_row().map(|row| row.id.clone()),
            before,
            "`j` should have moved the sidebar selection"
        );
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            "nothing should be written to a pane that no longer exists"
        );
    }

    #[test]
    fn pty_exited_on_a_brigade_worker_removes_its_pane_and_clamps_focus() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        for key in [&director, &worker] {
            state.screens.insert(key.clone(), Screen::new(24, 80));
        }
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), worker.clone()],
            focused: 1,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: worker.clone(),
            },
            test_instant(),
        );

        match &state.stage {
            Stage::Brigade { panes, focused, .. } => {
                assert_eq!(panes, &[director]);
                assert_eq!(*focused, 0);
            }
            other => panic!(
                "expected a surviving Brigade stage, got a different Stage variant: {}",
                match other {
                    Stage::Empty => "Empty",
                    Stage::Solo(_) => "Solo",
                    Stage::Brigade { .. } => "Brigade",
                }
            ),
        }
    }

    #[test]
    fn key_in_pane_focus_forwards_a_write_pty_cmd_with_the_right_encoding() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE,
            ))),
            now,
        );
        assert!(matches!(
            cmds.first(),
            Some(Cmd::WritePty { key: k, bytes }) if *k == key && bytes == b"a"
        ));
        assert_eq!(state.last_forwarded_input.get(&key), Some(&now));
    }

    #[test]
    fn paste_in_pane_focus_is_a_single_write_pty_bracketed_when_the_child_has_paste_mode_on() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        let mut screen = Screen::new(24, 80);
        // Turn on bracketed paste mode (DECSET 2004) in the child's model.
        screen.process(b"\x1b[?2004h");
        state.screens.insert(key.clone(), screen);
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Paste("a\nb".to_string())),
            now,
        );
        // Exactly one WritePty (a stray ResizePty may also come back — the
        // fresh 24x80 `Screen` almost certainly doesn't match the pane
        // geometry computed from `EmporiumState::new()`'s default `size`,
        // which is incidental here and not what this test is about).
        let writes: Vec<&Cmd> = cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::WritePty { .. }))
            .collect();
        assert_eq!(writes.len(), 1, "paste forwards as exactly one WritePty");
        assert!(matches!(
            writes[0],
            Cmd::WritePty { key: k, bytes }
                if *k == key && bytes == b"\x1b[200~a\rb\x1b[201~"
        ));
    }

    #[test]
    fn a_multi_line_paste_into_the_search_box_keeps_only_the_first_line() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("a")]);
        app.enter_search();
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Paste("first line\nsecond line".to_string())),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(app.query(), "first line");
        assert_eq!(
            app.mode(),
            Mode::Search,
            "a truncated newline must not have confirmed/exited search"
        );
    }

    #[test]
    fn a_multi_line_paste_into_a_text_field_modal_keeps_only_the_first_line() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("a")]);
        app.open_group_join_modal();
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Paste("myteam\nrest".to_string())),
            now,
        );

        let Some(Modal::GroupJoin(gstate)) = app.modal() else {
            panic!("expected the group-join modal to still be open");
        };
        assert_eq!(gstate.input(), "myteam");
    }

    #[test]
    fn first_line_stops_at_either_line_ending_and_returns_the_whole_text_when_there_is_none() {
        assert_eq!(first_line("abc\ndef"), "abc");
        assert_eq!(first_line("abc\r\ndef"), "abc");
        assert_eq!(first_line("abc\rdef"), "abc");
        assert_eq!(first_line("no newline here"), "no newline here");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn tick_nudges_then_flushes_the_delayed_submit() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.screens.insert(director.clone(), Screen::new(24, 80));
        state.screens.insert(worker.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker.clone()],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mut now = test_instant();

        let observation = || RelayObservation {
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
        };
        // Isolates the WritePty cmds from any incidental ResizePty (the
        // fresh 24x80 `Screen`s almost certainly don't match the pane
        // geometry computed from `EmporiumState::new()`'s default `size` —
        // not what this test is about).
        let writes = |cmds: &[Cmd]| -> Vec<(SessionKey, Vec<u8>)> {
            cmds.iter()
                .filter_map(|cmd| match cmd {
                    Cmd::WritePty { key, bytes } => Some((key.clone(), bytes.clone())),
                    _ => None,
                })
                .collect()
        };

        // First tick: idle streak only reaches 1 — not eligible yet.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        assert!(writes(&cmds).is_empty());

        // Second consecutive idle tick: eligible — nudge text goes out, and
        // the submitting `\r` is recorded as pending, not sent yet.
        now += Duration::from_secs(1);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        assert_eq!(
            writes(&cmds),
            vec![(worker.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec())]
        );
        assert!(state.status.as_deref().unwrap().contains("nudged worker-1"));

        // A tick before the submit delay has elapsed: nothing flushed yet.
        now += Duration::from_millis(50);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            now,
        );
        assert!(writes(&cmds).is_empty(), "submit not due yet");

        // Once RELAY_SUBMIT_DELAY has passed, the next tick flushes the
        // lone submitting `\r` — in its own chunk, per the relay engine's
        // whole reason for existing (see `update_tick`'s doc).
        now += RELAY_SUBMIT_DELAY;
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            now,
        );
        assert_eq!(writes(&cmds), vec![(worker.clone(), b"\r".to_vec())]);
    }

    // --- Codex Worker kickoff: open_worker / update_spawned / update_tick --

    #[test]
    fn open_worker_for_codex_marks_the_pending_open_for_a_kickoff() {
        let mut state = EmporiumState::new(PrefixKey::default());
        open_worker(
            &mut state,
            1,
            "worker-1",
            std::path::Path::new("/work"),
            "",
            AgentKind::Codex,
        );
        let key = SessionKey::new_worker(1, "worker-1");
        assert!(matches!(
            state.pending_opens.get(&key),
            Some(PendingOpen::BrigadeMember {
                needs_codex_kickoff: true,
                ..
            })
        ));
    }

    #[test]
    fn open_worker_for_claude_never_needs_a_kickoff() {
        let mut state = EmporiumState::new(PrefixKey::default());
        open_worker(
            &mut state,
            1,
            "worker-1",
            std::path::Path::new("/work"),
            "",
            AgentKind::ClaudeCode,
        );
        let key = SessionKey::new_worker(1, "worker-1");
        assert!(matches!(
            state.pending_opens.get(&key),
            Some(PendingOpen::BrigadeMember {
                needs_codex_kickoff: false,
                ..
            })
        ));
    }

    #[test]
    fn spawning_a_kickoff_eligible_worker_queues_a_pending_kickoff() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        state.pending_opens.insert(
            key.clone(),
            PendingOpen::BrigadeMember {
                brigade_id: 1,
                needs_codex_kickoff: true,
                cwd: PathBuf::from("/work"),
            },
        );

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned { key: key.clone() },
            test_instant(),
        );

        assert_eq!(state.pending_kickoffs.len(), 1);
        assert_eq!(state.pending_kickoffs[0].key, key);
        assert_eq!(state.pending_kickoffs[0].cwd, PathBuf::from("/work"));
    }

    #[test]
    fn spawning_a_non_kickoff_worker_queues_nothing() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        state.pending_opens.insert(
            key.clone(),
            PendingOpen::BrigadeMember {
                brigade_id: 1,
                needs_codex_kickoff: false,
                cwd: PathBuf::from("/work"),
            },
        );

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned { key },
            test_instant(),
        );

        assert!(state.pending_kickoffs.is_empty());
    }

    #[test]
    fn tick_checks_directory_trust_once_quiet_long_enough_but_not_before() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        let spawned_at = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            spawned_at + CODEX_KICKOFF_QUIET_PERIOD - Duration::from_millis(1),
        );
        assert!(cmds.is_empty(), "not quiet long enough yet");
        assert_eq!(state.pending_kickoffs.len(), 1);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            spawned_at + CODEX_KICKOFF_QUIET_PERIOD,
        );
        assert_eq!(
            cmds,
            vec![Cmd::CheckWorkerDirectoryTrust {
                key: key.clone(),
                cwd: PathBuf::from("/work"),
            }]
        );
        // Never typed blind: the entry stays queued until the trust check
        // actually answers.
        assert_eq!(state.pending_kickoffs.len(), 1);
        assert!(state.pending_submits.is_empty());
    }

    #[test]
    fn a_trusted_directory_answer_types_the_kickoff_line() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked {
                key: key.clone(),
                trusted: true,
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key: key.clone(),
                bytes: CODEX_WORKER_KICKOFF_LINE.as_bytes().to_vec(),
            }]
        );
        assert!(state.pending_kickoffs.is_empty());
        assert_eq!(state.pending_submits.len(), 1);
        assert_eq!(state.pending_submits[0].key, key);
    }

    #[test]
    fn an_untrusted_directory_answer_keeps_waiting_and_notifies_once() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked {
                key: key.clone(),
                trusted: false,
            },
            test_instant(),
        );

        assert!(
            cmds.is_empty(),
            "must never type the kickoff line into an untrusted/unknown cwd"
        );
        assert_eq!(
            state.pending_kickoffs.len(),
            1,
            "stays queued so the next tick asks again"
        );
        let first_status = state.status.clone();
        assert!(first_status.is_some());

        // A second untrusted answer must not restamp the notice (no new
        // `status_set_at`, and no duplicate wording).
        state.status = Some("something else the operator is looking at".to_string());
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked {
                key,
                trusted: false,
            },
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert_eq!(
            state.status,
            Some("something else the operator is looking at".to_string()),
            "already notified once; must not clobber an unrelated status"
        );
    }

    #[test]
    fn a_trust_check_answer_for_an_unstaged_pane_is_a_noop() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        // No entry in pending_kickoffs at all — the pane unstaged before the
        // answer landed.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked { key, trusted: true },
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(state.pending_kickoffs.is_empty());
    }

    #[test]
    fn pty_output_pushes_the_kickoff_quiet_deadline_back() {
        // A pane that's still noisy at the original deadline must not get
        // typed into — see CODEX_KICKOFF_QUIET_PERIOD's own doc for why an
        // early write risks the input being dropped.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        let spawned_at = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });

        let output_at = spawned_at + CODEX_KICKOFF_QUIET_PERIOD - Duration::from_millis(50);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"boot noise".to_vec(),
            },
            output_at,
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            spawned_at + CODEX_KICKOFF_QUIET_PERIOD,
        );
        assert!(
            cmds.is_empty(),
            "quiet since the LATEST output, not the original spawn"
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            output_at + CODEX_KICKOFF_QUIET_PERIOD,
        );
        assert!(!cmds.is_empty(), "quiet long enough since the last output");
    }

    #[test]
    fn codex_worker_discovery_timed_out_sets_status_and_drops_any_pending_kickoff() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::CodexWorkerDiscoveryTimedOut {
                key,
                token: "worker-1".to_string(),
            },
            test_instant(),
        );

        assert!(cmds.is_empty());
        assert!(state.pending_kickoffs.is_empty());
        assert_eq!(
            state.status.as_deref(),
            Some("worker-1: Codex briefing wasn't confirmed")
        );
    }

    #[test]
    fn pty_exited_clears_pending_kickoff_and_last_output_for_that_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key.clone());
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
        });
        state.last_output_at.insert(key.clone(), test_instant());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited { key: key.clone() },
            test_instant(),
        );

        assert!(state.pending_kickoffs.is_empty());
        assert!(!state.last_output_at.contains_key(&key));
    }

    /// Two-pane brigade (Director + Worker), both `Screen`s registered, `now`
    /// and `app`/`brigade` fixtures ready — the shared shape the
    /// `last_forwarded_input` per-pane tests below build on.
    fn two_pane_brigade_focused_on(
        focused: usize,
    ) -> (
        EmporiumState,
        App,
        BrigadeConfig,
        SessionKey,
        SessionKey,
        Instant,
    ) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.screens.insert(director.clone(), Screen::new(24, 80));
        state.screens.insert(worker.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), worker.clone()],
            focused,
        };
        state.focus = Focus::Pane;
        let app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();
        (state, app, brigade, director, worker, now)
    }

    fn writes_of(cmds: &[Cmd]) -> Vec<(SessionKey, Vec<u8>)> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::WritePty { key, bytes } => Some((key.clone(), bytes.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_nudge_for_one_pane_is_not_suppressed_by_input_forwarded_to_a_different_pane() {
        // The bug this round fixes: `last_forwarded_input` used to be one
        // run-wide `Instant`, so typing into the focused Director pane
        // silenced a nudge to the Worker the moment focus moved there —
        // even though the Worker itself had never received a keystroke.
        let (mut state, mut app, brigade, director, worker, mut now) =
            two_pane_brigade_focused_on(0); // Director focused first

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE,
            ))),
            now,
        );
        assert!(state.last_forwarded_input.contains_key(&director));
        assert!(!state.last_forwarded_input.contains_key(&worker));

        // Operator tabs over to the Worker pane, which has unseen mail and
        // goes idle for two consecutive ticks.
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        let observation = || RelayObservation {
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
        };
        now += Duration::from_millis(10);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        now += Duration::from_secs(1);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );

        assert_eq!(
            writes_of(&cmds),
            vec![(worker.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec())],
            "the Worker pane never received a keystroke; the Director's own \
             recent input must not suppress its nudge"
        );
    }

    #[test]
    fn a_nudge_for_a_pane_is_still_suppressed_by_recent_input_to_that_same_pane() {
        // Regression: the per-pane guard must still protect the pane the
        // operator is actually composing into.
        let (mut state, mut app, brigade, _director, worker, mut now) =
            two_pane_brigade_focused_on(1); // Worker focused

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE,
            ))),
            now,
        );

        let observation = || RelayObservation {
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
        };
        now += Duration::from_millis(10);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        now += Duration::from_secs(1);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );

        assert!(
            writes_of(&cmds).is_empty(),
            "recent input to the focused pane itself must still suppress its own nudge"
        );
    }

    #[test]
    fn a_pane_no_longer_focused_is_not_suppressed_by_its_own_earlier_input() {
        // `is_focused` already gates the quiet-period check, but this proves
        // it holds through the per-pane storage change too: the Director
        // pane keeps a very recent `last_forwarded_input` entry, yet once
        // focus has moved off it, its own nudge is not suppressed by it.
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0); // Director focused first

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE,
            ))),
            now,
        );
        assert!(state.last_forwarded_input.contains_key(&director));

        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1; // focus moves to the Worker; Director is no longer focused
        }
        let observation = || RelayObservation {
            token: "director".to_string(),
            key: director.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
        };
        now += Duration::from_millis(10);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        now += Duration::from_secs(1);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );

        assert_eq!(
            writes_of(&cmds),
            vec![(director.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec())],
            "the Director pane is no longer focused, so its own earlier \
             keystroke must not suppress its nudge"
        );
    }

    #[test]
    fn unstaging_a_pane_drops_its_last_forwarded_input_entry() {
        let (mut state, mut app, brigade, director, worker, now) = two_pane_brigade_focused_on(0);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                Modifiers::NONE,
            ))),
            now,
        );
        assert!(state.last_forwarded_input.contains_key(&director));

        state.unstage(&director);

        assert!(
            !state.last_forwarded_input.contains_key(&director),
            "a closed pane's key must not linger in the map forever"
        );
        // The still-open Worker pane's own (absent) entry is untouched.
        assert!(!state.last_forwarded_input.contains_key(&worker));
    }

    /// Shared setup for the two tests below: a solo, focused pane, ticked
    /// idle twice so it's nudge-eligible, then nudged. `Focus::Pane` and no
    /// prior `last_forwarded_input` together mean the quiet-period guard
    /// (`RELAY_INPUT_QUIET_PERIOD`) never blocks the nudge — this models the
    /// operator sitting on a focused pane they have not yet typed into when
    /// the nudge fires, exactly the case a keystroke can still land in
    /// before the delayed `\r`. Returns `(state, app, brigade, worker, now)`
    /// with `now` positioned right after the nudge text went out.
    fn nudged_focused_pane_awaiting_submit()
    -> (EmporiumState, App, BrigadeConfig, SessionKey, Instant) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let worker = SessionKey::from_id("w1");
        state.screens.insert(worker.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(worker.clone());
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mut now = test_instant();

        let observation = || RelayObservation {
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
        };

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        now += Duration::from_secs(1);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
            },
            now,
        );
        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key: worker.clone(),
                bytes: RELAY_NUDGE_LINE.as_bytes().to_vec(),
            }],
            "setup error: expected the nudge text to have gone out"
        );

        (state, app, brigade, worker, now)
    }

    #[test]
    fn real_input_in_the_submit_gap_cancels_the_pending_r() {
        let (mut state, mut app, brigade, worker, mut now) = nudged_focused_pane_awaiting_submit();

        // The operator starts typing into that same focused pane before
        // RELAY_SUBMIT_DELAY elapses.
        now += Duration::from_millis(50);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('h'),
                Modifiers::NONE,
            ))),
            now,
        );
        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key: worker.clone(),
                bytes: b"h".to_vec(),
            }],
            "the operator's own keystroke must still reach the pane"
        );

        // Once RELAY_SUBMIT_DELAY has fully elapsed, nothing flushes: the
        // pending `\r` was cancelled the moment real input arrived.
        now += RELAY_SUBMIT_DELAY;
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            now,
        );
        assert!(
            cmds.is_empty(),
            "a cancelled submit must never fire: {cmds:?}"
        );
    }

    #[test]
    fn a_pending_submit_still_flushes_when_nothing_interrupts_it() {
        let (mut state, mut app, brigade, worker, mut now) = nudged_focused_pane_awaiting_submit();

        now += RELAY_SUBMIT_DELAY;
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            now,
        );
        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key: worker,
                bytes: b"\r".to_vec(),
            }]
        );
    }

    #[test]
    fn archive_done_sets_status_and_reloads() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::ArchiveDone {
                title: "Fix login".to_string(),
                result: Ok(()),
            },
            test_instant(),
        );
        assert!(matches!(cmds.as_slice(), [Cmd::Reload]));
        assert_eq!(state.status.as_deref(), Some("archived Fix login"));
    }

    #[test]
    fn archive_done_failure_sets_status_but_still_reloads() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::ArchiveDone {
                title: "Fix login".to_string(),
                result: Err("disk full".to_string()),
            },
            test_instant(),
        );
        assert!(matches!(cmds.as_slice(), [Cmd::Reload]));
        let status = state.status.unwrap();
        assert!(status.contains("failed to archive Fix login"));
        assert!(status.contains("disk full"));
    }

    // --- PrefixKey::parse ---------------------------------------------------

    #[test]
    fn prefix_key_parses_a_control_chord() {
        assert_eq!(PrefixKey::parse("C-b"), PrefixKey::default());
        assert_eq!(
            PrefixKey::parse("C-x"),
            PrefixKey {
                code: KeyCode::Char('x'),
                mods: Modifiers::CONTROL,
            }
        );
    }

    #[test]
    fn prefix_key_parses_a_bare_character() {
        assert_eq!(
            PrefixKey::parse("x"),
            PrefixKey {
                code: KeyCode::Char('x'),
                mods: Modifiers::NONE,
            }
        );
    }

    #[test]
    fn prefix_key_falls_back_to_the_default_on_garbage() {
        for raw in ["", "C-", "C-bb", "toolong"] {
            assert_eq!(PrefixKey::parse(raw), PrefixKey::default(), "raw = {raw:?}");
        }
    }

    // --- prefix arming / resolution (Focus::Pane, update_key) --------------

    #[test]
    fn prefix_chord_in_pane_focus_arms_instead_of_forwarding() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                Modifiers::CONTROL,
            ))),
            now,
        );

        // A stray Cmd::ResizePty may come back regardless (the fresh 24x80
        // Screen almost certainly doesn't match the pane geometry computed
        // from EmporiumState::new()'s default `size`, see the paste test
        // above) — what matters here is that nothing was forwarded.
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            "arming the prefix must not forward a key"
        );
        assert_eq!(state.prefix_armed, Some(now));
    }

    #[test]
    fn armed_prefix_pressed_again_sends_the_literal_prefix_byte() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                Modifiers::CONTROL,
            ))),
            now,
        );

        let writes: Vec<&Cmd> = cmds
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::WritePty { .. }))
            .collect();
        assert_eq!(writes.len(), 1, "expected exactly one WritePty");
        assert!(matches!(
            writes[0],
            Cmd::WritePty { key: k, bytes } if *k == key && bytes.as_slice() == [0x02]
        ));
        assert_eq!(state.prefix_armed, None);
    }

    #[test]
    fn armed_o_cycles_the_focused_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('o'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.prefix_armed, None);
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 1),
            _ => panic!("expected a Brigade stage"),
        }
    }

    #[test]
    fn armed_digit_focuses_pane_by_one_based_index() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        let w2 = SessionKey::from_id("w2");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, w1, w2],
            focused: 0,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('3'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 2),
            _ => panic!("expected a Brigade stage"),
        }
    }

    #[test]
    fn armed_digit_out_of_range_swallows_and_sets_status() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('5'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.status.as_deref(), Some("prefix: no such pane"));
        assert_eq!(state.prefix_armed, None);
    }

    #[test]
    fn armed_s_returns_focus_to_the_sidebar() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('s'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.focus, Focus::Sidebar);
        assert_eq!(state.prefix_armed, None);
    }

    #[test]
    fn armed_x_opens_the_kill_confirm_modal_for_the_focused_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.prefix_armed, None);
        assert!(matches!(
            app.modal(),
            Some(Modal::ConfirmKill { key, .. }) if key == "sess-1"
        ));
    }

    #[test]
    fn armed_x_on_a_worker_pane_grows_the_dismiss_choice() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 1,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![row("dir"), row("w1")]);
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(matches!(
            app.modal(),
            Some(Modal::ConfirmKill {
                key,
                worker_choice: Some(KillChoice::ClosePane),
                ..
            }) if key == "w1"
        ));
    }

    #[test]
    fn armed_x_on_the_director_pane_has_no_dismiss_choice() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![row("dir"), row("w1")]);
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('x'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(matches!(
            app.modal(),
            Some(Modal::ConfirmKill {
                key,
                worker_choice: None,
                ..
            }) if key == "dir"
        ));
    }

    #[test]
    fn armed_unbound_key_is_swallowed_not_forwarded() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('z'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            "an unbound prefix key must never forward as a WritePty"
        );
        assert_eq!(state.status.as_deref(), Some("unbound prefix key"));
        assert_eq!(state.prefix_armed, None);
    }

    // --- prefix arm timeout (Event::Tick) -----------------------------------

    #[test]
    fn tick_past_the_arm_timeout_disarms_the_prefix() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let armed_at = test_instant();
        state.prefix_armed = Some(armed_at);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            armed_at + PREFIX_ARM_TIMEOUT,
        );

        assert_eq!(state.prefix_armed, None);
    }

    #[test]
    fn tick_before_the_arm_timeout_leaves_the_prefix_armed() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let armed_at = test_instant();
        state.prefix_armed = Some(armed_at);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick { relay: vec![] },
            armed_at + Duration::from_secs(1),
        );

        assert_eq!(state.prefix_armed, Some(armed_at));
    }

    // --- new-session modal: cwd-check round trip ----------------------------

    fn press_enter(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        now: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        )
    }

    #[test]
    fn new_session_confirm_emits_check_cwd_and_leaves_the_modal_open() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]); // seeds candidate cwd "/work/alpha"
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = press_enter(&mut state, &mut app, &brigade, now);

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::CheckNewSessionCwd { cwd }] if cwd == &PathBuf::from("/work/alpha")
        ));
        assert!(matches!(app.modal(), Some(Modal::NewSession(_))));
        assert!(app.modal_new_session_check_pending());
    }

    #[test]
    fn new_session_confirm_is_a_noop_while_a_check_is_already_pending() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]);
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        let first = press_enter(&mut state, &mut app, &brigade, now);
        assert_eq!(first.len(), 1);

        let second = press_enter(&mut state, &mut app, &brigade, now);
        assert!(second.is_empty());
    }

    #[test]
    fn new_session_cwd_checked_true_closes_the_modal_and_opens() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]);
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        press_enter(&mut state, &mut app, &brigade, now);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked {
                cwd: PathBuf::from("/work/alpha"),
                is_dir: true,
            },
            now,
        );

        assert!(app.modal().is_none());
        assert_eq!(state.pending_opens.len(), 1);
        match cmds.as_slice() {
            [
                Cmd::OpenEmbedded {
                    key,
                    target,
                    brigade: None,
                    model: None,
                },
            ] => {
                assert!(key.is_synthetic());
                assert_eq!(target.id, "");
                assert_eq!(target.cwd, PathBuf::from("/work/alpha"));
                assert_eq!(target.agent, AgentKind::ClaudeCode);
            }
            other => panic!("expected a single OpenEmbedded cmd, got {other:?}"),
        }
    }

    #[test]
    fn new_session_modal_backtab_toggles_the_agent_and_the_open_reflects_it() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]);
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::BackTab,
                Modifiers::NONE,
            ))),
            now,
        );
        assert_eq!(app.modal_new_session_agent(), AgentKind::Codex);

        press_enter(&mut state, &mut app, &brigade, now);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked {
                cwd: PathBuf::from("/work/alpha"),
                is_dir: true,
            },
            now,
        );

        match cmds.as_slice() {
            [Cmd::OpenEmbedded { target, .. }] => {
                assert_eq!(target.agent, AgentKind::Codex);
            }
            other => panic!("expected a single OpenEmbedded cmd, got {other:?}"),
        }
    }

    #[test]
    fn new_session_cwd_checked_false_sets_the_error_and_leaves_enter_live_again() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]);
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        press_enter(&mut state, &mut app, &brigade, now);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked {
                cwd: PathBuf::from("/work/alpha"),
                is_dir: false,
            },
            now,
        );

        assert!(cmds.is_empty());
        let Some(Modal::NewSession(ns)) = app.modal() else {
            panic!("expected the new-session modal to stay open");
        };
        assert_eq!(ns.error(), Some("/work/alpha is not a directory"));
        assert!(!app.modal_new_session_check_pending());
    }

    #[test]
    fn new_session_cwd_checked_ignores_a_stale_verdict_after_the_target_changed() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]); // candidate cwd "/work/alpha"
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        press_enter(&mut state, &mut app, &brigade, now);
        // The operator keeps typing while the stat is in flight, moving the
        // target away from what was actually sent for checking.
        for c in "/elsewhere".chars() {
            app.modal_push_char(c);
        }
        assert_eq!(
            app.modal_new_session_target(),
            Some(PathBuf::from("/elsewhere"))
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked {
                cwd: PathBuf::from("/work/alpha"),
                is_dir: true,
            },
            now,
        );

        assert!(cmds.is_empty());
        assert!(matches!(app.modal(), Some(Modal::NewSession(_))));
        // Cleared regardless of relevance, so a fresh Enter is live again.
        assert!(!app.modal_new_session_check_pending());
    }

    #[test]
    fn new_session_cwd_checked_is_a_noop_after_the_modal_was_cancelled() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("s1")]);
        app.open_new_session_modal();
        let brigade = brigade_config();
        let now = test_instant();

        press_enter(&mut state, &mut app, &brigade, now);
        app.close_modal();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::NewSessionCwdChecked {
                cwd: PathBuf::from("/work/alpha"),
                is_dir: true,
            },
            now,
        );

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
    }

    // --- kill-confirm modal --------------------------------------------------

    #[test]
    fn kill_confirm_enter_emits_kill_pty() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        app.open_confirm_kill_modal("sess-1".to_string(), "sess-1".to_string(), false);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::KillPty { key }] if key.as_str() == "sess-1"
        ));
        assert!(app.modal().is_none());
    }

    #[test]
    fn kill_confirm_esc_closes_with_no_cmd() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        app.open_confirm_kill_modal("sess-1".to_string(), "sess-1".to_string(), false);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert!(app.modal().is_none());
    }

    // --- dismiss a Worker ----------------------------------------------------

    #[test]
    fn kill_confirm_dismiss_on_a_synthetic_worker_key_emits_dismiss_worker_directly() {
        // Still awaiting discovery: brigade_id/token are embedded in the key
        // itself, so no ResolveMembership round trip is needed at all.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::new_worker(1, "worker-1");
        let mut app = app_with(vec![]);
        app.open_confirm_kill_modal(key.as_str().to_string(), "worker-1".to_string(), true);
        app.modal_select_next(); // ClosePane -> Dismiss
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        );

        assert_eq!(
            cmds,
            vec![Cmd::Store(StoreIntent::DismissWorker {
                brigade_id: 1,
                token: "worker-1".to_string(),
            })]
        );
        assert_eq!(state.pending_dismiss, Some(key));
        assert!(app.modal().is_none());
    }

    #[test]
    fn kill_confirm_dismiss_on_a_resolved_worker_key_spends_a_resolve_membership_round_trip() {
        // A real session id carries no brigade info of its own — one
        // ResolveMembership round trip is needed before DismissWorker can
        // be built.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("w1")]);
        app.open_confirm_kill_modal("w1".to_string(), "w1".to_string(), true);
        app.modal_select_next(); // ClosePane -> Dismiss
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        );

        assert_eq!(
            cmds,
            vec![Cmd::Store(StoreIntent::ResolveMembership {
                session_id: "w1".to_string(),
            })]
        );
        assert_eq!(state.pending_dismiss, Some(SessionKey::from_id("w1")));

        // The round trip answers: w1 is indeed a Worker of brigade 1.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "w1".to_string(),
                membership: Some((1, "worker-1".to_string(), BrigadeRole::Worker)),
                members: None,
            },
            now,
        );

        assert_eq!(
            cmds,
            vec![Cmd::Store(StoreIntent::DismissWorker {
                brigade_id: 1,
                token: "worker-1".to_string(),
            })]
        );
    }

    #[test]
    fn kill_confirm_close_pane_never_touches_the_store_even_on_a_worker_pane() {
        // The default choice: identical to today's plain kill, no round
        // trip, no DismissWorker — a Worker pane's dialog only grows a
        // second choice, it never changes the first one's meaning.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        app.open_confirm_kill_modal("w1".to_string(), "w1".to_string(), true);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        );

        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: SessionKey::from_id("w1")
            }]
        );
        assert!(state.pending_dismiss.is_none());
    }

    #[test]
    fn worker_dismissed_removes_only_that_pane_kills_it_and_shrinks_the_hidden_set() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        let w2 = SessionKey::from_id("w2");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), w1.clone(), w2.clone()],
            focused: 1,
        };
        state.pending_dismiss = Some(w1.clone());
        let mut app = app_with(vec![row("dir"), row("w1"), row("w2")])
            .with_hidden_worker_ids(["w1".to_string(), "w2".to_string()].into_iter().collect());
        assert_eq!(app.filtered_len(), 1); // only "dir" visible before dismissal
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDismissed {
                brigade_id: 1,
                result: Ok((["w2".to_string()].into_iter().collect(), HashSet::new())),
            },
            now,
        );

        assert_eq!(cmds, vec![Cmd::KillPty { key: w1 }]);
        // w1 left the hidden set (it's a dismissed member now, honestly
        // surfaced as an ordinary session) — w2 is still hidden.
        assert_eq!(app.filtered_len(), 2);
        match &state.stage {
            Stage::Brigade { panes, .. } => assert_eq!(panes, &vec![director, w2]),
            other => panic!("expected Stage::Brigade, got {other:?}"),
        }
        assert!(state.pending_dismiss.is_none());
    }

    #[test]
    fn worker_dismissed_failure_leaves_the_stage_untouched() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, w1.clone()],
            focused: 1,
        };
        state.pending_dismiss = Some(w1);
        let mut app = app_with(vec![row("dir"), row("w1")]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDismissed {
                brigade_id: 1,
                result: Err("db locked".to_string()),
            },
            now,
        );

        assert!(cmds.is_empty());
        match &state.stage {
            Stage::Brigade { panes, .. } => assert_eq!(panes.len(), 2),
            other => panic!("expected Stage::Brigade, got {other:?}"),
        }
        assert_eq!(
            state.status.as_deref(),
            Some("failed to dismiss worker: db locked")
        );
        assert!(state.pending_dismiss.is_none());
    }

    // --- disband kills staged workers ----------------------------------------

    #[test]
    fn disband_while_staged_kills_workers_but_not_the_director() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        let w2 = SessionKey::from_id("w2");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), w1.clone(), w2.clone()],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );

        let killed: Vec<&SessionKey> = cmds
            .iter()
            .map(|cmd| match cmd {
                Cmd::KillPty { key } => key,
                _ => panic!("expected only KillPty cmds from a disband"),
            })
            .collect();
        assert_eq!(killed.len(), 2);
        assert!(killed.contains(&&w1));
        assert!(killed.contains(&&w2));
        assert!(!killed.contains(&&director));
        match &state.stage {
            Stage::Solo(key) => assert_eq!(key, &director),
            _ => panic!("expected the director to remain staged solo"),
        }
    }

    #[test]
    fn disband_of_an_unstaged_brigade_kills_nothing() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Empty;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );

        assert!(cmds.is_empty());
        assert!(matches!(state.stage, Stage::Empty));
    }

    // --- F2/F3 regression ----------------------------------------------------

    #[test]
    fn f2_toggles_focus_between_sidebar_and_pane_when_a_stage_is_active() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::F(2),
                Modifiers::NONE,
            ))),
            now,
        );
        assert_eq!(state.focus, Focus::Pane);
        assert!(cmds.is_empty());

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::F(2),
                Modifiers::NONE,
            ))),
            now,
        );
        assert_eq!(state.focus, Focus::Sidebar);
        assert!(cmds.is_empty());
    }

    #[test]
    fn f3_cycles_the_focused_pane_within_a_staged_brigade() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::F(3),
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 1),
            _ => panic!("expected a Brigade stage"),
        }
    }

    // --- modifier gating: modifier-blind matching ---------------------------

    #[test]
    fn ctrl_b_in_the_sidebar_arms_the_prefix_instead_of_adding_a_worker() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director],
            focused: 0,
        };
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                Modifiers::CONTROL,
            ))),
            now,
        );

        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::Store(_))),
            "Ctrl+B must arm the prefix, not fire the plain-b add-worker binding"
        );
        assert_eq!(state.prefix_armed, Some(now));
        assert_eq!(state.focus, Focus::Sidebar);
    }

    #[test]
    fn ctrl_d_in_the_sidebar_does_not_open_the_archive_modal() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('d'),
                Modifiers::CONTROL,
            ))),
            now,
        );

        assert!(app.modal().is_none());
        assert_eq!(state.prefix_armed, None);
    }

    #[test]
    fn armed_ctrl_o_swallows_without_cycling_or_forwarding() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('o'),
                Modifiers::CONTROL,
            ))),
            now,
        );

        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            "an unbound (modifier-mangled) prefix key must never forward"
        );
        assert_eq!(state.prefix_armed, None);
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 0, "must not cycle"),
            _ => panic!("expected a Brigade stage"),
        }
        assert_eq!(state.status.as_deref(), Some("unbound prefix key"));
    }

    // --- the focus ring: sidebar joins the ring -----------------------------

    #[test]
    fn cycle_forward_wraps_through_sidebar_director_and_workers() {
        let pane_count = 3; // director + 2 workers
        assert_eq!(
            cycle_forward(FocusSlot::Sidebar, pane_count),
            FocusSlot::Pane(0)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(0), pane_count),
            FocusSlot::Pane(1)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(1), pane_count),
            FocusSlot::Pane(2)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(2), pane_count),
            FocusSlot::Sidebar
        );
    }

    #[test]
    fn cycle_forward_on_a_solo_stage_toggles_between_sidebar_and_the_pane() {
        assert_eq!(cycle_forward(FocusSlot::Sidebar, 1), FocusSlot::Pane(0));
        assert_eq!(cycle_forward(FocusSlot::Pane(0), 1), FocusSlot::Sidebar);
    }

    #[test]
    fn cycle_forward_on_an_empty_stage_is_a_noop() {
        assert_eq!(cycle_forward(FocusSlot::Sidebar, 0), FocusSlot::Sidebar);
    }

    #[test]
    fn arming_from_the_sidebar_then_o_lands_on_the_first_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        state.focus = Focus::Sidebar;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let armed_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('b'),
                Modifiers::CONTROL,
            ))),
            now,
        );
        assert!(!armed_cmds.iter().any(|cmd| matches!(cmd, Cmd::Store(_))));
        assert_eq!(state.prefix_armed, Some(now));

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('o'),
                Modifiers::NONE,
            ))),
            now,
        );

        assert_eq!(state.prefix_armed, None);
        assert_eq!(state.focus, Focus::Pane);
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 0),
            _ => panic!("expected a Brigade stage"),
        }
    }

    // --- arrow navigation ----------------------------------------------------

    #[test]
    fn arrow_target_covers_the_full_navigation_matrix() {
        use Direction::{Down, Left, Right, Up};
        use FocusSlot::{Pane, Sidebar};

        // (from, direction, pane_count, expected)
        let cases = [
            // Sidebar column (3-pane brigade: director + 2 workers)
            (Sidebar, Left, 3, Sidebar),
            (Sidebar, Right, 3, Pane(0)),
            (Sidebar, Up, 3, Sidebar),
            (Sidebar, Down, 3, Sidebar),
            // Director column
            (Pane(0), Left, 3, Sidebar),
            (Pane(0), Right, 3, Pane(1)),
            (Pane(0), Up, 3, Pane(0)),
            (Pane(0), Down, 3, Pane(0)),
            // Worker stack: first worker, clamped at the top
            (Pane(1), Left, 3, Pane(0)),
            (Pane(1), Right, 3, Pane(1)),
            (Pane(1), Up, 3, Pane(1)),
            (Pane(1), Down, 3, Pane(2)),
            // Worker stack: last worker, clamped at the bottom
            (Pane(2), Left, 3, Pane(0)),
            (Pane(2), Right, 3, Pane(2)),
            (Pane(2), Up, 3, Pane(1)),
            (Pane(2), Down, 3, Pane(2)),
            // Solo column: one pane, no worker stack to enter
            (Sidebar, Right, 1, Pane(0)),
            (Pane(0), Left, 1, Sidebar),
            (Pane(0), Right, 1, Pane(0)),
            (Pane(0), Up, 1, Pane(0)),
            (Pane(0), Down, 1, Pane(0)),
            // Empty stage: the sidebar is the only slot there is
            (Sidebar, Left, 0, Sidebar),
            (Sidebar, Right, 0, Sidebar),
            (Sidebar, Up, 0, Sidebar),
            (Sidebar, Down, 0, Sidebar),
        ];

        for (from, direction, pane_count, expected) in cases {
            assert_eq!(
                arrow_target(from, direction, pane_count),
                expected,
                "from {from:?}, {direction:?}, pane_count {pane_count}"
            );
        }
    }

    #[test]
    fn armed_right_arrow_from_the_sidebar_focuses_the_director_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 0,
        };
        state.focus = Focus::Sidebar;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Right,
                Modifiers::NONE,
            ))),
            now,
        );

        assert_eq!(state.focus, Focus::Pane);
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 0),
            _ => panic!("expected a Brigade stage"),
        }
        assert!(cmds.is_empty());
    }

    #[test]
    fn armed_left_arrow_from_a_worker_returns_to_the_director() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director, worker],
            focused: 1,
        };
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Left,
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.focus, Focus::Pane);
        match &state.stage {
            Stage::Brigade { focused, .. } => assert_eq!(*focused, 0),
            _ => panic!("expected a Brigade stage"),
        }
    }

    #[test]
    fn armed_left_arrow_from_the_director_or_solo_pane_returns_to_the_sidebar() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key);
        state.focus = Focus::Pane;
        state.prefix_armed = Some(test_instant());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Left,
                Modifiers::NONE,
            ))),
            now,
        );

        assert!(cmds.is_empty());
        assert_eq!(state.focus, Focus::Sidebar);
    }

    // --- focus-event forwarding (DECSET 1004: CSI I / CSI O) ---------------

    /// A neutral event whose own handler never emits a `Cmd` on this fixture
    /// (fresh state, no pending kickoffs/submits, empty relay) — used purely
    /// to run `update`'s unconditional tail (`emit_focus_transitions`) so a
    /// test can inspect what it did without an unrelated handler's own `Cmd`s
    /// mixed in.
    fn settle(state: &mut EmporiumState, app: &mut App, brigade: &BrigadeConfig) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Tick { relay: Vec::new() },
            test_instant(),
        )
    }

    #[test]
    fn switching_pane_focus_sends_csi_o_to_the_loser_and_csi_i_to_the_winner() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), worker.clone()],
            focused: 0,
        };
        state.focus = Focus::Pane;
        let mut director_screen = screen_sized_for_pane(&state);
        director_screen.process(b"\x1b[?1004h");
        state.screens.insert(director.clone(), director_screen);
        let mut worker_screen = screen_sized_for_pane(&state);
        worker_screen.process(b"\x1b[?1004h");
        state.screens.insert(worker.clone(), worker_screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        // The very first call always emits a `CSI I` for whoever already has
        // focus (`last_focus_notified` starts `None`) — settle that before
        // the transition this test actually checks.
        settle(&mut state, &mut app, &brigade);

        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        let cmds = settle(&mut state, &mut app, &brigade);

        assert_eq!(
            cmds,
            vec![
                Cmd::WritePty {
                    key: director,
                    bytes: b"\x1b[O".to_vec(),
                },
                Cmd::WritePty {
                    key: worker,
                    bytes: b"\x1b[I".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn a_pane_that_never_asked_for_focus_events_is_sent_neither_csi() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            panes: vec![director.clone(), worker.clone()],
            focused: 0,
        };
        state.focus = Focus::Pane;
        // Neither pane ever sent DECSET 1004.
        state
            .screens
            .insert(director.clone(), screen_sized_for_pane(&state));
        state
            .screens
            .insert(worker.clone(), screen_sized_for_pane(&state));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        settle(&mut state, &mut app, &brigade);

        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        let cmds = settle(&mut state, &mut app, &brigade);

        assert!(cmds.is_empty(), "neither pane asked: {cmds:?}");
    }

    #[test]
    fn focus_moving_to_the_sidebar_sends_csi_o_to_the_pane_that_lost_it() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut screen = screen_sized_for_pane(&state);
        screen.process(b"\x1b[?1004h");
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        settle(&mut state, &mut app, &brigade);

        state.focus = Focus::Sidebar;
        let cmds = settle(&mut state, &mut app, &brigade);

        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key,
                bytes: b"\x1b[O".to_vec(),
            }]
        );
    }

    #[test]
    fn banto_losing_and_regaining_os_focus_is_reported_to_the_focused_pane() {
        // The operator alt-tabbing away from banto entirely must read to a
        // hosted child exactly like losing focus within banto — it cannot
        // tell the two apart.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut screen = screen_sized_for_pane(&state);
        screen.process(b"\x1b[?1004h");
        state.screens.insert(key.clone(), screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        settle(&mut state, &mut app, &brigade);

        let lost = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: false },
            test_instant(),
        );
        assert_eq!(
            lost,
            vec![Cmd::WritePty {
                key: key.clone(),
                bytes: b"\x1b[O".to_vec(),
            }]
        );

        let regained = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: true },
            test_instant(),
        );
        assert_eq!(
            regained,
            vec![Cmd::WritePty {
                key,
                bytes: b"\x1b[I".to_vec(),
            }]
        );
    }

    #[test]
    fn an_unrelated_tick_does_not_repeat_a_focus_notification() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        let mut screen = screen_sized_for_pane(&state);
        screen.process(b"\x1b[?1004h");
        state.screens.insert(key, screen);
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let first = settle(&mut state, &mut app, &brigade);
        assert_eq!(first.len(), 1, "the first settle reports the initial focus");

        let second = settle(&mut state, &mut app, &brigade);
        assert!(second.is_empty(), "nothing changed: {second:?}");
    }

    #[test]
    fn a_pane_that_asks_for_focus_events_only_after_already_being_focused_still_gets_csi_i() {
        // The bug this guards: a pane almost always gains focus (on open, or
        // by being clicked) before its own output has had a chance to enable
        // DECSET 1004 — Claude Code's own 1004 arrives a few bytes into its
        // startup sequence, not before it. `emit_focus_transitions` must not
        // treat "the focused key hasn't changed" as "nothing to do" — the
        // pane's *readiness* changed even though its identity didn't.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.size = (120, 40);
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        // Screen exists, but has not enabled DECSET 1004 yet.
        state
            .screens
            .insert(key.clone(), screen_sized_for_pane(&state));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let before_1004 = settle(&mut state, &mut app, &brigade);
        assert!(
            before_1004.is_empty(),
            "nothing to send before the pane asks: {before_1004:?}"
        );

        // The child's first bytes enable focus reporting — no pane switch,
        // no window-focus change, `state.focus`/`state.stage` untouched.
        state.screens.get_mut(&key).unwrap().process(b"\x1b[?1004h");
        let after_1004 = settle(&mut state, &mut app, &brigade);

        assert_eq!(
            after_1004,
            vec![Cmd::WritePty {
                key,
                bytes: b"\x1b[I".to_vec(),
            }],
            "the pane asked for focus events while already focused — it must still get one"
        );
    }

    // --- clipboard forwarding (OSC 52) --------------------------------------

    #[test]
    fn a_well_formed_clipboard_copy_is_forwarded_to_the_host() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key,
                chunk: b"\x1b]52;c;aGVsbG8=\x07".to_vec(),
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::ForwardClipboardToHost {
                bytes: b"\x1b]52;c;aGVsbG8=\x07".to_vec(),
            }]
        );
    }

    #[test]
    fn an_oversized_clipboard_copy_is_dropped_not_forwarded() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        // Comfortably over `MAX_OSC52_SEQUENCE_LEN`, and still valid base64
        // alphabet throughout, so size is the only thing that can reject it.
        let oversized_data = vec![b'A'; MAX_OSC52_SEQUENCE_LEN];
        let mut chunk = b"\x1b]52;c;".to_vec();
        chunk.extend_from_slice(&oversized_data);
        chunk.push(0x07);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput { key, chunk },
            test_instant(),
        );

        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::ForwardClipboardToHost { .. })),
            "an oversized payload must never reach the host terminal: {cmds:?}"
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("too large")),
            "the drop must be visible, not silent: {:?}",
            state.status
        );
    }

    #[test]
    fn build_clipboard_forward_rejects_a_selector_outside_the_recognized_alphabet() {
        assert_eq!(build_clipboard_forward(b"x", b"aGVsbG8="), None);
    }

    #[test]
    fn build_clipboard_forward_rejects_data_outside_the_base64_alphabet() {
        assert_eq!(build_clipboard_forward(b"c", b"not base64!!"), None);
    }

    #[test]
    fn build_clipboard_forward_accepts_every_recognized_selector() {
        for selector in CLIPBOARD_SELECTORS {
            let selection = [*selector];
            assert!(
                build_clipboard_forward(&selection, b"aGVsbG8=").is_some(),
                "selector {selector:?} should be accepted"
            );
        }
    }

    #[test]
    fn a_clipboard_read_request_is_not_forwarded() {
        // `]52;c;?` is a read request, not a copy — `vt100-psmux` only
        // stages `Screen::take_clipboard`'s slot from the copy path
        // (`set_clipboard`), never from a `?` query, so this reaches
        // `update` as ordinary output with nothing to drain.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key,
                chunk: b"\x1b]52;c;?\x07".to_vec(),
            },
            test_instant(),
        );

        assert!(
            cmds.is_empty(),
            "a read request must not be forwarded: {cmds:?}"
        );
    }
}
