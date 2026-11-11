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

use ratatui_core::layout::{Position, Rect};
use serde::{Deserialize, Serialize};

use crate::app::{App, ClickOutcome, GroupId, GroupJoinTarget, KillChoice, Modal, Mode};
use crate::config::{BrigadeConfig, RelayMode, WorkerAgentSetting};
use crate::display_text::{display_name, shorten_to_columns};
use crate::input::{
    InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crate::input_line::{InputLine, read_input_line, typed_text_without_whitespace};
use crate::key_encode::{
    ForwardedInput, InputEffect, input_effect, key_to_bytes, normalize_paste_line_endings,
    wrap_bracketed_paste,
};
use crate::model::{
    AgentKind, BrigadeId, BrigadeRole, DIRECTOR_TOKEN, GOINKYO_TOKEN, MemberGlance, MemberToken,
    MessageGlance, PtyExitReason, SessionRow, SessionToOpen, StoredLayout,
};

pub use crate::layout::{
    Areas, Axis, Boundary, BrigadeLayout, ButtonKind, COLLAPSED_TILE_HEIGHT,
    DEFAULT_COLLAPSED_STACK_WIDTH, DEFAULT_MASTER_PERMILLE, Geometry, Handle,
    MIN_HEIGHT_FOR_SUMMARY, MIN_MASTER_WIDTH, MIN_PANE_AREA_WIDTH, MIN_SIDEBAR_WIDTH,
    MIN_STACK_TILE_HEIGHT, MIN_STACK_WIDTH, SIDEBAR_WIDTH, SUMMARY_HEIGHT, TileButton, layout,
    master_width, pane_content, stack_heights, stage_tiles,
};
use crate::layout::{LayoutPrefs, master_permille_for, sort_stack};

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

    /// Named (and shaped) for a Worker, its first use, but carries no role
    /// of its own — `update_goinkyo_awaiting_spawn` reuses it verbatim for
    /// a still-undiscovered Goinkyo's placeholder, `token: "goinkyo"`.
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

    /// If this is a still-awaiting-discovery placeholder (built by
    /// [`Self::new_worker`] — a Worker's or, per that constructor's own
    /// doc, a Goinkyo's), the `(brigade_id, token)` embedded in it — `None`
    /// for every other key, including a *resolved* member's real session
    /// id, which carries no brigade info of its own (that needs a store
    /// round trip — see `confirm_kill_modal`'s dismiss path).
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
        /// The Director's own key, authoritative — role no longer has to be
        /// derived from position anywhere (`add_worker`, `update_disbanded`,
        /// `PrefixAction::Kill`, `gather_goinkyo_observation`, `tile_title`
        /// all read this instead of assuming `panes[0]`). `None` is a
        /// legitimate state, not a gap to close: `stage_brigade` sets it
        /// leniently from the store's own member row (no session id
        /// recorded yet there → `None`), and closing just the Director's
        /// own pane (prefix-`x`, `ClosePane` — never routed through
        /// dismiss, see `PrefixAction::Kill`'s own doc; `Stage::remove`
        /// clears this field the same moment it drops that pane from
        /// `panes`) leaves a brigade legitimately staged around its other
        /// members with the Director simply unknown-for-now. Every
        /// consumer's own `None` behavior is its safe default, not an
        /// error path — see each site's own doc.
        director: Option<SessionKey>,
        /// Every staged pane, Director included. Order is display/geometry
        /// only now (`stage_tiles`'s master+stack layout, `FocusPane`'s
        /// `1`-`9`) — a best-effort convention (`update_spawned`'s
        /// `BrigadeMember` arm still inserts the Director's own key at the
        /// front when it recognizes it, everyone else appended in arrival
        /// order), never a source of role truth. Past `panes[0]`, the
        /// operator's preferred order (`BrigadeLayout::stack_order`), then the
        /// order the members joined the brigade (`EmporiumState::member_order`),
        /// is re-applied at the end of every `update`, so whatever rebuilt or
        /// appended to this list is put back in it. Nothing here still infers
        /// a role — or, since `EmporiumState::member_tokens` landed, a
        /// member's identity at all — from where a key sits; `director`
        /// above and `member_tokens` are what changed that.
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
    /// (collapsing to `Empty` if that was the last pane) — and, if `key` was
    /// the Director's own, `director` clears to `None` in the same step:
    /// the invariant every consumer relies on is that `Some` always still
    /// has a pane in `panes` to back it, never a stale reference to one
    /// that's already gone.
    fn remove(&mut self, key: &SessionKey) {
        match self {
            Stage::Solo(k) if k == key => *self = Stage::Empty,
            Stage::Brigade {
                director,
                panes,
                focused,
                ..
            } => {
                if director.as_ref() == Some(key) {
                    *director = None;
                }
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
/// nudge arriving late only delays how soon the Worker notices unread mail,
/// while a nudge arriving mid-composition can destroy what the operator was
/// actually typing — a much more expensive failure to guard a few extra
/// seconds against. Nothing marks unread mail anywhere on screen; once the
/// pane also holds an unsent draft, the held nudge is announced on the
/// status line (see [`StatusReminder`]).
const RELAY_INPUT_QUIET_PERIOD: Duration = Duration::from_secs(30);
/// How long after banto last forwarded operator input to a pane an `Empty`
/// reading of its input line is not believed. A pane's screen lags its
/// keystrokes: on a throwaway Claude Code session hosted by banto
/// (2026-11-10, 19 keystrokes) the echo that shows a typed character reached
/// the screen 36–108ms after the key, and for that long the screen still
/// shows the empty prompt of a line that is no longer empty. Believing that
/// frame would clear the draft mark and let a nudge land on the draft. Three
/// seconds is a margin of about thirty times the slowest echo measured, not a
/// measured threshold.
const INPUT_ECHO_SETTLE: Duration = Duration::from_secs(3);
/// How long an idle Claude pane may stay unreadable (its input line not the
/// recognised shape, see [`crate::input_line`]) before the operator is told
/// once that draft detection has fallen back to keystroke tracking. Claude
/// Code's screen layout changes without notice, and a reader that quietly
/// stopped recognising it would look exactly like one that works. A minute
/// outlasts the menus and pickers that legitimately replace the input block
/// for a while; nothing measured it.
const INPUT_LINE_UNREADABLE_AFTER: Duration = Duration::from_secs(60);
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
/// How long the status line stays clear — showing the key guidance — between
/// one showing of a recurring notice and the next (see [`StatusReminder`]).
/// With [`STATUS_TIMEOUT`] that makes a 30s cycle, five seconds of notice and
/// twenty-five of guidance. The operator asked for exactly that rhythm: a
/// notice that stays up, or returns the instant it clears, would hide the
/// key guidance for as long as the condition lasts.
const STATUS_REMINDER_GAP: Duration = Duration::from_secs(25);
/// Minimum gap between host bells for newly visible attention marks. This is
/// a policy throttle, not a measured terminal limit: an arrival remains
/// prompt, while a burst of panes cannot turn into an alarm.
const ATTENTION_BELL_COOLDOWN: Duration = Duration::from_secs(3);

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

/// What banto knows about the operator's input to one pane when deciding
/// whether to nudge it. Bundled so [`tick_relay_decision`] stays under
/// clippy's too-many-arguments.
#[derive(Debug, Default, Clone, Copy)]
pub struct PaneInput {
    /// The pane is the one the operator's keystrokes would reach right now.
    pub is_focused: bool,
    /// When banto last forwarded operator input to this pane.
    pub last_forwarded: Option<Instant>,
    /// [`EmporiumState::has_unsent_operator_input`] for this pane.
    pub has_unsent: bool,
}

pub fn should_nudge(
    now: Instant,
    idle_streak: u32,
    input: PaneInput,
    has_unseen: bool,
    state: &NudgeState,
) -> bool {
    if !has_unseen || idle_streak < RELAY_IDLE_STREAK_REQUIRED {
        return false;
    }
    // Neither focus nor elapsed time is consulted: an unsent draft does not
    // age out and does not move with the operator's attention.
    if input.has_unsent {
        return false;
    }
    if input.is_focused
        && let Some(last_input) = input.last_forwarded
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

/// What the screen and the keystroke mark together say about one pane:
/// whether unsent operator text is there, and whether the mark should be
/// dropped.
///
/// The screen only ever adds information it can be believed about. `HasText`
/// holds the nudge whether or not banto saw the typing (recalled history is
/// text no keystroke announced). `Empty` clears the mark, but only once
/// [`INPUT_ECHO_SETTLE`] has passed since banto last forwarded input to the
/// pane, because until then the screen has not caught up with the keys.
/// `Unrecognized` — and any `Empty` that is not yet believed — leaves the
/// keystroke mark in charge, as it was before the screen was read.
///
/// `since_last_forwarded` is `None` when banto never forwarded input to the
/// pane.
fn unsent_verdict(
    reading: InputLine,
    marked: bool,
    since_last_forwarded: Option<Duration>,
) -> (bool, bool) {
    match reading {
        InputLine::HasText => (true, false),
        InputLine::Empty if since_last_forwarded.is_none_or(|d| d >= INPUT_ECHO_SETTLE) => {
            (false, true)
        }
        InputLine::Empty | InputLine::Unrecognized => (marked, false),
    }
}

pub fn tick_relay_decision(
    states: &mut HashMap<(BrigadeId, MemberToken), RelayState>,
    brigade_id: BrigadeId,
    token: &MemberToken,
    now: Instant,
    is_idle_this_tick: Option<bool>,
    input: PaneInput,
    has_unseen: bool,
) -> bool {
    let identity = (brigade_id, token.clone());
    if !has_unseen {
        states.remove(&identity);
        return false;
    }
    let state = states.entry(identity).or_default();
    state.idle_streak = if is_idle_this_tick == Some(true) {
        state.idle_streak + 1
    } else {
        0
    };
    let nudge = should_nudge(now, state.idle_streak, input, has_unseen, &state.nudge);
    if nudge {
        state.nudge.last_nudge = Some(now);
        state.nudge.attempts += 1;
    }
    nudge
}

/// What the relay gather read about one brigade, enough to name it to the
/// operator: the title frozen at formation (`brigades.name`) and the session
/// id the roster records for its Director (`None` until one is recorded).
/// Replaced each tick it arrives, dropped on disband.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrigadeNameFacts {
    pub brigade_id: BrigadeId,
    pub name: String,
    pub director_session: Option<String>,
}

/// One relay-eligible live member's freshly-gathered observations for this
/// tick — gathered shell-side (store + live-session reads), decided
/// core-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayObservation {
    pub brigade_id: BrigadeId,
    pub token: MemberToken,
    pub key: SessionKey,
    pub has_unseen: bool,
    pub is_idle_this_tick: Option<bool>,
    /// Which product hosts this member, as the shell knows it (the store's
    /// recorded choice, else the member's row). Only a Claude pane has an
    /// input line this build can read. `None` in a stream recorded before
    /// this field existed, which reads as "not Claude".
    #[serde(default)]
    pub agent: Option<AgentKind>,
    /// The member's newest inbound and sent message, for its collapsed tile.
    /// `None` means "not read this tick, nothing new to say" — the shell skips
    /// the reads while the store has no new message — so it keeps whatever
    /// the core already holds; `Some` replaces it (even with two empty
    /// halves, which says the member has no mail). Message text, so it ends
    /// up in a recorded stream (`BANTO_RECORD_EVENTS`).
    #[serde(default)]
    pub glance: Option<MemberGlance>,
}

/// What banto knows about a live member that is neither on its screen nor in
/// the session list: the product it runs as (the shell resolves it from the
/// store's recorded choice, else the member's row) and its newest inbound and
/// sent message. Kept per `(BrigadeId, MemberToken)` from the relay
/// observations; a collapsed tile's body is drawn from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberFacts {
    pub agent: Option<AgentKind>,
    pub inbound: Option<MessageGlance>,
    pub sent: Option<MessageGlance>,
}

/// A brigade's Goinkyo member row that exists but has neither a session id
/// nor a pane yet — what [`update_goinkyo_awaiting_spawn`] needs to decide
/// whether to spawn it. Gathered shell-side (store + `app` reads) alongside
/// [`RelayObservation`] but reported separately, not folded into it: a
/// `RelayObservation` only ever describes a member already staged with a
/// live pane, which a Goinkyo waiting to be opened has neither of — there
/// is no shared shape to widen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoinkyoSpawnCandidate {
    pub brigade_id: BrigadeId,
    pub cwd: PathBuf,
    /// The brigade's members' tokens in the order they joined, as the store
    /// has them now — the Goinkyo among them, since its row exists. This is the
    /// earliest the core can learn where the consultation joined: its row is
    /// written by the Director's own MCP process, which no event reports, so
    /// the first the core hears is the tick that finds the row without a
    /// session. A Worker added in the instant between the two would otherwise be
    /// recorded first. Empty in a capture recorded before this field existed.
    #[serde(default)]
    pub roster: Vec<MemberToken>,
}

/// What the shell observed about the staged brigade's Goinkyo this tick —
/// [`Event::GoinkyoAwaitingSpawn`]'s payload. A plain `Option<GoinkyoSpawnCandidate>`
/// cannot say why there's nothing to spawn, and
/// [`update_goinkyo_awaiting_spawn`] needs that distinction: "no Goinkyo
/// member row, on a brigade that's still staged" is the one case that means
/// a consultation was dismissed out from under it, and is the signal that
/// releases (and closes the pane tracked by) [`EmporiumState::goinkyo_pane`]
/// for that brigade — every other "nothing to report" reason must leave it
/// untouched. See `goinkyo_pane`'s own doc for why a disbanded brigade
/// never reaches this case at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoinkyoObservation {
    /// Not a staged brigade, or the Goinkyo row already has a session id
    /// (already spawned, or resolved by discovery) — nothing to do either
    /// way.
    Unchanged,
    /// A brigade with a Goinkyo pane no longer has a Goinkyo member row:
    /// its consultation has ended.
    ///
    /// Deliberately not limited to the staged brigade. `dismiss_goinkyo` is
    /// an MCP call the Director's own session makes, so it lands whenever
    /// that session decides to make it — including while the operator is
    /// looking at a different brigade, or at no brigade at all. Tied to the
    /// stage, this would never be reported for those, and the pane would go
    /// on running with no row left to justify it. A Worker's dismissal
    /// cannot reach that state: it is a key the operator presses while
    /// looking at the brigade it belongs to.
    NoGoinkyo { brigade_id: BrigadeId },
    /// A member row exists with no session id yet.
    AwaitingSpawn(GoinkyoSpawnCandidate),
}

/// Shows a status message again and again for as long as a condition holds,
/// without ever covering another message or leaving the key guidance hidden:
/// one showing, then [`STATUS_TIMEOUT`] of notice, then
/// [`STATUS_REMINDER_GAP`] of ordinary status line, then the next showing.
///
/// One instance per condition, kept in [`EmporiumState`]. Each tick the owner
/// asks [`Self::due`] with whether the condition still holds; on `true` it
/// calls `set_status` itself, because the message text is the owner's and the
/// reminder holds no state beyond when it last spoke.
#[derive(Debug, Default)]
pub struct StatusReminder {
    last_shown: Option<Instant>,
}

impl StatusReminder {
    /// Whether the message should go up now. `status_free` is whether the
    /// status line is empty: while something else is showing — an error, say
    /// — this stays pending rather than skipping the showing, so the notice
    /// appears as soon as the line clears. A `true` is taken as shown, so the
    /// caller must show it. Once `active` goes false the cycle resets, and a
    /// condition that returns shows immediately.
    pub fn due(&mut self, active: bool, status_free: bool, now: Instant) -> bool {
        if !active {
            self.last_shown = None;
            return false;
        }
        if !status_free {
            return false;
        }
        let due = self.last_shown.is_none_or(|shown| {
            now.saturating_duration_since(shown) >= STATUS_TIMEOUT + STATUS_REMINDER_GAP
        });
        if due {
            self.last_shown = Some(now);
        }
        due
    }
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
    sent_at: Option<Instant>,
    attempts: u32,
}

/// How long a freshly-spawned Goinkyo pane's own output must have been
/// quiet before [`update_tick`] checks whether it's safe to type
/// [`GOINKYO_KICKOFF_LINE`] into it — the same bootstrap problem
/// [`CODEX_KICKOFF_QUIET_PERIOD`]/[`CODEX_WORKER_KICKOFF_LINE`] solve for a
/// Codex Worker (see that constant's own doc): a Claude session left with
/// nothing ever typed into it never writes a `projects/*.jsonl` transcript
/// either, so — like an idle Codex TUI that never runs its `SessionStart`
/// hook — it stays permanently undiscoverable, and unreachable by relay,
/// whose own idle detection (`gather_relay_observations`) needs that
/// transcript to resolve a row at all. Measured 2026-08-04 on a real
/// Goinkyo left with nothing typed into it: its `sessions/<pid>.json` live
/// file was written once, 122ms after spawn, and never updated again for
/// the next ~12 minutes — consistent with a process sitting at an empty
/// prompt, not one that crashed. Reuses Codex's own measured value rather
/// than inventing an unmeasured one for Claude's boot noise specifically:
/// no reason to expect it settles slower, but this has not been separately
/// measured the way [`CODEX_KICKOFF_QUIET_PERIOD`] was.
const GOINKYO_KICKOFF_QUIET_PERIOD: Duration = Duration::from_millis(700);
/// Claude's post-Enter transition was not measured, so a retry waits far
/// longer than boot quiet before risking another bootstrap turn.
const KICKOFF_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// After this many tries, report the failure rather than silently repeating.
/// Banto cannot read the input line to distinguish a lost Enter from text
/// still present there, which can duplicate the kickoff sentence.
const KICKOFF_MAX_ATTEMPTS: u32 = 3;

/// The fixed, ASCII-only line banto types into a freshly-spawned Goinkyo's
/// stdin once its boot output goes quiet (see [`GOINKYO_KICKOFF_QUIET_PERIOD`])
/// — the turn this starts is what lets it act on the request already
/// delivered via `--append-system-prompt` (`goinkyo_prompt`'s own
/// `{request}` substitution) at all: Claude Code does nothing with a
/// system prompt alone until it receives an actual turn, confirmed on the
/// same real Goinkyo cited above — no error, no crash, simply nothing
/// happens until something is typed into it.
///
/// Deliberately doesn't repeat what the briefing already says (the
/// request's path, its peers, why it was summoned) — what's missing is the
/// trigger, not information; mirrors [`CODEX_WORKER_KICKOFF_LINE`]'s own
/// "states only what's true, adds no content the model doesn't already
/// have" philosophy. ASCII-only for the same untested-raw-UTF-8-paste
/// reason [`RELAY_NUDGE_LINE`] is.
const GOINKYO_KICKOFF_LINE: &str = "[banto] This pane just started as your brigade's Goinkyo.";

/// A freshly-spawned Goinkyo pane awaiting its own boot output going quiet
/// before [`update_tick`] asks whether it's safe to type
/// [`GOINKYO_KICKOFF_LINE`] into it — see that constant's own doc for why
/// the kickoff exists at all, and [`Cmd::CheckGoinkyoDirectoryTrust`]'s for
/// why quiet alone isn't enough to type it blind. Queued by
/// [`update_goinkyo_awaiting_spawn`]'s own `AwaitingSpawn` success branch —
/// the exact same moment [`EmporiumState::goinkyo_pane`] records the pane,
/// so the "attempt once per consultation" guarantee that already covers
/// the spawn itself covers this too, with no separate marker needed.
/// Removed once the kickoff is actually sent (its `\r` becomes an ordinary
/// [`PendingSubmit`], the same two-phase shape as a relay nudge or a Codex
/// kickoff) or the pane unstages first.
struct PendingGoinkyoKickoff {
    key: SessionKey,
    /// Baseline for the quiet-period check when [`EmporiumState::last_output_at`]
    /// has no entry yet for this pane (nothing has arrived at all) — the
    /// moment this tracker was created, i.e. spawn time.
    spawned_at: Instant,
    /// This Goinkyo's own cwd — echoed on [`Cmd::CheckGoinkyoDirectoryTrust`]
    /// once the quiet period elapses, since the pure core has no way to read
    /// Claude's own trust registry itself and the shell has no other way to
    /// know which directory to check.
    cwd: PathBuf,
    /// Whether [`update_goinkyo_directory_trust_checked`] has already told
    /// the operator this pane is waiting on an unanswered trust prompt —
    /// same reasoning as [`PendingKickoff::notified_untrusted`].
    notified_untrusted: bool,
    sent_at: Option<Instant>,
    attempts: u32,
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
    director_agent: AgentKind,
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
    director_agent: AgentKind,
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

/// A boundary drag in progress: which boundary, where the pointer took hold
/// of it, and where it is previewed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Drag {
    boundary: Boundary,
    /// How far the boundary's position was from the pointer when the press
    /// landed (a press can land on either border column of a vertical
    /// boundary, or on either of the two rows of the stack's), kept for the
    /// whole gesture: the preview is the boundary where it was grabbed plus
    /// how far the pointer has moved since, so the line neither jumps on the
    /// press nor on the release.
    grab_offset: i32,
    /// The previewed position of the boundary, already clamped.
    preview: u16,
    /// Whether every stack tile was collapsed when the press landed. The
    /// master/stack boundary sets a different preference in each state
    /// (the collapsed stack width, or the master's share) and keeps its
    /// identity across them, so a gesture that began in one must not commit
    /// in the other.
    all_collapsed: bool,
}

/// What the status line says while a boundary is being dragged.
const DRAG_HINT: &str = "resize: release to apply · Esc cancels";

/// A stack tile being dragged to another slot by its title label: which tile,
/// and where the pointer was last. Exclusive with a boundary [`Drag`]; the
/// stack does not change until the release, like the resize.
///
/// The slot the pointer selects is derived from the current geometry wherever
/// it is needed (the preview line, the commit), never stored: an index into
/// `panes` means something else the moment a pane arrives or leaves while the
/// button is held. Only the pointer's row picks the slot; the column only
/// places the ghost that follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MoveDrag {
    key: SessionKey,
    column: u16,
    row: u16,
    /// Whether a `Drag` event has arrived since the press. A bare press shows
    /// the tile marked and nothing else.
    dragged: bool,
}

/// What a mouse gesture in progress adds to the finished frame, decided here
/// and painted by the shell (`drag_overlay`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragOverlay {
    /// A line: the boundary being resized, or the edge a moved tile would
    /// land at. `None` while a moved tile is over its own slot.
    pub line: Option<(Rect, Axis)>,
    /// The tile being moved, drawn in the gesture colour for the whole
    /// gesture, from the press.
    pub marked: Option<SessionKey>,
    /// The moved tile's label following the pointer, once it has moved.
    pub ghost: Option<Ghost>,
}

/// A piece of text painted at a screen cell, already clipped to the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ghost {
    pub text: String,
    pub x: u16,
    pub y: u16,
}

/// What the status line says while a tile is being moved.
const MOVE_HINT: &str = "move: release to drop · Esc cancels";

pub struct EmporiumState {
    pub screens: HashMap<SessionKey, crate::screen::Screen>,
    pub stage: Stage,
    pub focus: Focus,
    pub status: Option<String>,
    status_set_at: Option<Instant>,
    /// Relay state belongs to the store's unread-cursor identity, not to a
    /// display name. Two live brigades both call their leader `director`, and
    /// the first off-stage Director left with unread mail exposed that sharing
    /// a cooldown or attempt count across those cells is wrong.
    pub relay_states: HashMap<(BrigadeId, MemberToken), RelayState>,
    /// See [`MemberFacts`]. Forgotten when the member is dismissed or the
    /// brigade disbanded, like the layout preferences; a pane exiting or
    /// being relaunched keeps it.
    member_facts: HashMap<(BrigadeId, MemberToken), MemberFacts>,
    /// The arrangement the store last held for each brigade — what it was
    /// read as, or last given. [`persist_preferences`] writes a brigade's
    /// layout whenever it differs from this, so a write follows exactly the
    /// changes that matter and nothing else.
    persisted_layouts: HashMap<BrigadeId, BrigadeLayout>,
    /// What the relay gather last read about each brigade, to name it in the
    /// operator-facing statuses (see [`Self::brigade_name`]). Seeded at
    /// formation, replaced each tick, dropped on disband.
    brigade_names: HashMap<BrigadeId, BrigadeNameFacts>,
    /// The order each brigade's members joined it, by token: what places a
    /// tile the operator never moved (`layout::sort_stack`). Seeded from the
    /// store's roster whenever a brigade is staged — the store is the one
    /// place that remembers join order across a restart — and at formation
    /// from the order the Workers were numbered; a Worker added is appended
    /// at `WorkerAdded` (`record_join`), and the Goinkyo's place comes from the
    /// roster its spawn candidate carries. A member the core still meets in a
    /// stack unlisted is appended on sight, as a last resort. A dismissed member is
    /// dropped, so a reused token is new and goes to the bottom; disbanding
    /// drops the brigade's list.
    member_order: HashMap<BrigadeId, Vec<MemberToken>>,
    /// The same for the sidebar width.
    persisted_sidebar_width: u16,
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
    /// Panes the operator has put text into that banto has not seen them
    /// send — see [`Self::has_unsent_operator_input`], the one way to read
    /// it. A pane enters when a forwarded key or paste may have added text
    /// ([`InputEffect::AddsText`]) and leaves when a plain Enter is forwarded
    /// ([`InputEffect::Submits`]) or the pane unstages; nothing else clears
    /// it, deliberately, because which keys empty an agent's input line is
    /// the agent's business and changes between versions.
    ///
    /// Exists because elapsed time cannot measure a draft: the first nudge
    /// that corrupted a message was cured by widening a quiet period
    /// ([`RELAY_INPUT_QUIET_PERIOD`]), and the second — a draft left in the
    /// Director's input line while the operator worked elsewhere — got past
    /// that same period twice over, by moving focus off the pane (the
    /// period only applies to the focused one) and by simply outlasting 30s.
    /// A draft is a fact about what was typed and not yet sent, so it is
    /// recorded as that.
    unsent_operator_input: HashSet<SessionKey>,
    /// Announces on the status line that a nudge is being held back by
    /// [`Self::unsent_operator_input`]; see [`update_tick`].
    draft_hold_reminder: StatusReminder,
    kickoff_trust_reminder: StatusReminder,
    kickoff_failed_reminder: StatusReminder,
    /// Since when each idle Claude member's input line has read as
    /// [`InputLine::Unrecognized`], unbroken. Keyed by member rather than by
    /// pane so a rename of the pane's key does not restart the count.
    unreadable_since: HashMap<(BrigadeId, MemberToken), Instant>,
    /// Members already told about an unreadable input line, so the status
    /// line says it once, not every [`INPUT_LINE_UNREADABLE_AFTER`].
    unreadable_noticed: HashSet<(BrigadeId, MemberToken)>,
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
    /// The Goinkyo analog of [`Self::pending_kickoffs`] — see
    /// [`PendingGoinkyoKickoff`]'s own doc for why it's a separate list
    /// rather than folded into that one (a different underlying trust
    /// registry, no `needs_codex_kickoff`-style spawn-time flag to key off
    /// since only [`update_goinkyo_awaiting_spawn`] ever creates one).
    pending_goinkyo_kickoffs: Vec<PendingGoinkyoKickoff>,
    pending_submits: Vec<PendingSubmit>,
    pending_opens: HashMap<SessionKey, PendingOpen>,
    pending_membership: Option<PendingMembership>,
    pending_brigade_formation: Option<PendingBrigadeFormation>,
    pending_worker_addition: Option<PendingWorkerAddition>,
    pending_codex_trust_formation: Option<PendingCodexTrustFormation>,
    /// The session id of a Director-to-be whose already-open pane was just
    /// killed (`confirm_director_reopen_modal`), waiting on its
    /// `Event::PtyExited` before formation can continue — reopening any
    /// sooner would resume a session `banto::opener::decide_inplace_resume`
    /// still sees as live and refuse. `update_pty_exited` takes it once the exit
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
    /// The `(brigade, token)` that dismissal will remove from the store,
    /// stashed beside [`Self::pending_dismiss`] where the intent is built.
    /// `WorkerDismissed` carries only the brigade, and a pane that already
    /// exited has lost its `member_tokens` entry, so the token cannot be
    /// recovered from the key by then. Needed because tokens are reused —
    /// add-worker takes the highest existing number plus one, so a dismissed
    /// last Worker's token comes back for the next one — and a row weight
    /// still filed under it would be inherited.
    pending_dismiss_member: Option<(BrigadeId, MemberToken)>,
    pub size: (u16, u16),
    /// The sidebar's width in columns, as the operator last dragged it
    /// (default [`SIDEBAR_WIDTH`]). A preference: clamped wherever it is
    /// used and never rewritten to fit a small terminal, so widening the
    /// terminal restores it.
    sidebar_width: u16,
    /// Layout preferences per brigade, keyed by [`BrigadeId`] and — inside —
    /// by member token, so a brigade that goes off-stage and is staged again
    /// in the same run gets its split back, and a pane's key changing
    /// (discovery, a compaction fork) loses nothing. Dropped only when the
    /// brigade is disbanded.
    brigade_layouts: HashMap<BrigadeId, BrigadeLayout>,
    /// The boundary drag in progress, if any. While it is `Some` the real
    /// geometry does not change: it holds only the previewed position, and
    /// the layout moves once, when the button is released
    /// ([`update_drag_mouse`]).
    drag: Option<Drag>,
    /// The tile move in progress, if any (see [`MoveDrag`]).
    move_drag: Option<MoveDrag>,
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
    /// This map carries two roles at once — spawn-attempt guard, and live-
    /// consultation pane ledger — and every drift found here so far sat at
    /// the seam between the store's own row (truth) and this map (cache).
    /// Before changing anything, check all eight cells of (row present? ×
    /// pane staged? × entry present?) rather than trusting a fresh read of
    /// the design below.
    ///
    /// The brigades [`update_goinkyo_awaiting_spawn`] has already issued a
    /// `Cmd::OpenEmbedded` for, each mapped to the `SessionKey` that spawn
    /// used — a brigade is inserted the moment that happens, success or
    /// failure, and *not* removed by [`update_spawn_failed`] the way
    /// [`Self::pending_opens`] is. Without an entry surviving past that
    /// removal, a failed spawn would look identical next tick to "never
    /// tried" (no session id, no pane, either way), and the tick fires
    /// every second the condition holds — this is what turns that into
    /// "attempt once per consultation" instead of "once a second until it
    /// works or the brigade is gone". A `HashMap` rather than a single
    /// `Option<BrigadeId>` slot: more than one brigade can be staged (in the
    /// store, even if only one is staged in the UI at a time) and each
    /// attempts independently — a single slot would let staging brigade B
    /// overwrite brigade A's own entry, so switching back to A (its Goinkyo
    /// still unresolved) would look like "never tried" again and re-fire.
    ///
    /// The key half serves a second purpose besides the guard: when
    /// [`update_goinkyo_awaiting_spawn`] is told a brigade's Goinkyo row is
    /// gone (`GoinkyoObservation::NoGoinkyo` — dismissed via
    /// `end_consultation`/an operator's own dismiss, both of which delete
    /// the row while leaving the brigade staged), the entry is removed —
    /// unblocking a later consultation on the same brigade — and, if the
    /// pane it names is still actually staged, that pane is unstaged and
    /// killed right there, the same way [`update_worker_dismissed`] already
    /// closes a dismissed Worker's. The shell has no way to identify which
    /// staged pane was the Goinkyo's once its row is gone (nothing else
    /// about a pane says so), which is why this tracks it here instead of
    /// asking the shell to work it out fresh each tick the way every other
    /// gather function does. Kept current across a discovery rename
    /// (`update_discovery_result`) or a compact fork
    /// (`update_member_session_forked`) — both know the renamed member's
    /// own token, so both update this entry in place when that token is
    /// `"goinkyo"` and an entry for its brigade already exists — and
    /// dropped (not `Cmd::KillPty`'d — nothing to kill twice) if the pane
    /// closes on its own first (`update_pty_exited`).
    ///
    /// Disband does *not* reach the `NoGoinkyo` removal above: it un-stages
    /// the brigade first (`update_disbanded`), so the next gather sees no
    /// staged brigade at all and reports `Unchanged`, never `NoGoinkyo` —
    /// the entry for a disbanded brigade simply stays here forever,
    /// harmlessly, since that brigade id is never staged (and so never
    /// gathered for) again.
    goinkyo_pane: HashMap<BrigadeId, SessionKey>,
    /// Durable brigade member identity carried alongside a live pane key.
    /// Every key in `Stage::Brigade::panes` has one entry here; `panes`
    /// deliberately remains geometry-only, so the renderer reads this map
    /// rather than deriving a member name from a tile position. Rekey paths
    /// keep the token attached when a live key changes.
    member_tokens: HashMap<SessionKey, MemberToken>,
    /// Brigade ownership travels with a pane even while another brigade is
    /// displayed. The exit handler needs it to reopen a Worker whose cell is
    /// off-stage; `Stage` only describes the one currently visible cell.
    member_brigades: HashMap<SessionKey, BrigadeId>,
    /// The staged panes with an on-screen attention mark: loaded Claude
    /// Waiting rows plus BEL latches. This is the one authoritative tile
    /// display set; rendering reads it for magenta borders while the same
    /// update-boundary computation derives host-bell edges.
    /// Derived by [`refresh_waiting_display`] only; the shell gets it through
    /// [`Self::attention_panes`], never a mutable field.
    attention_panes: HashSet<SessionKey>,
    /// Number of distinct Waiting sessions the operator can reach through the
    /// current list result or a staged pane. Kept alongside [`Self::attention_panes`]
    /// because the sidebar's labelled Claude count includes list rows too,
    /// while its border set can only contain panes.
    visible_waiting_count: usize,
    /// Panes whose child rang BEL while they were not effectively focused.
    /// Folded into `attention_panes`, never drawn separately.
    latched_attention_panes: HashSet<SessionKey>,
    /// Cached list-visible Waiting ids as pane keys, refreshed only when
    /// App's matching cache generation changes. Discovery imposes no bound on
    /// these ids, so the edge diff must not rebuild this set for every PTY
    /// chunk merely because each one runs the display refresh.
    ring_list_waiting: HashSet<SessionKey>,
    ring_list_generation: u64,
    /// `None` seeds the first refresh silently; thereafter this is the prior
    /// visible-mark set used to detect attention edges.
    previous_ring_set: Option<HashSet<SessionKey>>,
    last_attention_bell: Option<Instant>,
    /// Panes banto itself asked to end, so that [`update_pty_exited`] can
    /// tell an exit banto caused from one it merely observed.
    ///
    /// Those two arrive as the same event through the same poll loop
    /// (`Cmd::KillPty`'s own doc: the Cmd only makes the exit happen, the
    /// passive fold is what cleans up), and only this record separates them.
    /// Written by [`kill_pane`] and nowhere else — a `Cmd::KillPty` built by
    /// hand somewhere else would read, correctly as far as this state can
    /// see, as a child that died on its own.
    expected_exits: HashSet<SessionKey>,
    /// When each member's pane was last restarted by [`update_pty_exited`],
    /// so a member that cannot stay up is not restarted forever. Never
    /// cleared on success: a member that dies once an hour should be
    /// restarted each time, and one that dies twice in a minute should not.
    last_relaunch: HashMap<SessionKey, Instant>,
}

/// How soon after restarting a member's pane banto declines to restart it
/// again. A member that dies once — an agent CLI replacing itself during an
/// auto-update is the case this was built for — is back before the operator
/// notices. A member that cannot start at all dies again immediately, and
/// without this it would be respawned as fast as the process can fail.
const RELAUNCH_COOLDOWN: Duration = Duration::from_secs(60);

/// Ask for `key`'s pane to end, recording that banto is the one asking.
///
/// Every kill banto performs goes through here. The alternative — reading the
/// surrounding state at exit time — very nearly works: dismissing a Worker,
/// disbanding, and both Goinkyo paths all un-stage before they kill, so by
/// the time the exit arrives the key is already gone from the stage. The one
/// that does not is the plain `x` kill, which sets nothing and un-stages
/// nothing, and is therefore indistinguishable at exit time from a child that
/// died by itself. That asymmetry is what this exists to remove.
fn kill_pane(state: &mut EmporiumState, key: SessionKey) -> Cmd {
    state.expected_exits.insert(key.clone());
    Cmd::KillPty { key }
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
            member_facts: HashMap::new(),
            persisted_layouts: HashMap::new(),
            brigade_names: HashMap::new(),
            member_order: HashMap::new(),
            persisted_sidebar_width: SIDEBAR_WIDTH,
            last_forwarded_input: HashMap::new(),
            unsent_operator_input: HashSet::new(),
            draft_hold_reminder: StatusReminder::default(),
            kickoff_trust_reminder: StatusReminder::default(),
            kickoff_failed_reminder: StatusReminder::default(),
            unreadable_since: HashMap::new(),
            unreadable_noticed: HashSet::new(),
            last_output_at: HashMap::new(),
            pending_kickoffs: Vec::new(),
            pending_goinkyo_kickoffs: Vec::new(),
            pending_submits: Vec::new(),
            pending_opens: HashMap::new(),
            pending_membership: None,
            pending_brigade_formation: None,
            pending_worker_addition: None,
            pending_codex_trust_formation: None,
            pending_director_reopen: None,
            pending_director_wiring: None,
            pending_dismiss: None,
            pending_dismiss_member: None,
            size: (0, 0),
            sidebar_width: SIDEBAR_WIDTH,
            brigade_layouts: HashMap::new(),
            drag: None,
            move_drag: None,
            prefix,
            prefix_armed: None,
            next_plain_id: 0,
            window_focused: true,
            last_focus_notified: None,
            goinkyo_pane: HashMap::new(),
            member_tokens: HashMap::new(),
            member_brigades: HashMap::new(),
            attention_panes: HashSet::new(),
            visible_waiting_count: 0,
            latched_attention_panes: HashSet::new(),
            ring_list_waiting: HashSet::new(),
            ring_list_generation: 0,
            previous_ring_set: None,
            last_attention_bell: None,
            expected_exits: HashSet::new(),
            last_relaunch: HashMap::new(),
        }
    }

    /// Every brigade this run has a Goinkyo pane recorded for, smallest id
    /// first. Read by the shell's own gather so that noticing a consultation
    /// has ended does not depend on which brigade is on stage — see
    /// [`GoinkyoObservation::NoGoinkyo`]. Sorted because a `HashMap`'s order
    /// is not one, and an observation that varies run to run is a poor thing
    /// to hand a fold that gets replayed.
    pub fn goinkyo_brigades(&self) -> Vec<BrigadeId> {
        let mut ids: Vec<BrigadeId> = self.goinkyo_pane.keys().copied().collect();
        ids.sort_unstable();
        ids
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

    /// Move every pane-owned fact as one transaction when discovery or
    /// compaction replaces its key. Keeping this list here makes a new
    /// `SessionKey`-indexed field reviewable at its only migration point.
    fn rekey_pane_state(&mut self, old_key: &SessionKey, new_key: &SessionKey) -> bool {
        let had_screen = if let Some(screen) = self.screens.remove(old_key) {
            self.screens.insert(new_key.clone(), screen);
            true
        } else {
            false
        };
        match &mut self.stage {
            Stage::Solo(key) if key == old_key => *key = new_key.clone(),
            Stage::Brigade {
                director, panes, ..
            } => {
                if director.as_ref() == Some(old_key) {
                    *director = Some(new_key.clone());
                }
                for key in panes {
                    if key == old_key {
                        *key = new_key.clone();
                    }
                }
            }
            _ => {}
        }
        if let Some(value) = self.last_forwarded_input.remove(old_key) {
            self.last_forwarded_input.insert(new_key.clone(), value);
        }
        if self.unsent_operator_input.remove(old_key) {
            self.unsent_operator_input.insert(new_key.clone());
        }
        if let Some(value) = self.last_output_at.remove(old_key) {
            self.last_output_at.insert(new_key.clone(), value);
        }
        if let Some(value) = self.pending_opens.remove(old_key) {
            self.pending_opens.insert(new_key.clone(), value);
        }
        if self.attention_panes.remove(old_key) {
            self.attention_panes.insert(new_key.clone());
        }
        if self.latched_attention_panes.remove(old_key) {
            self.latched_attention_panes.insert(new_key.clone());
        }
        if self.ring_list_waiting.remove(old_key) {
            self.ring_list_waiting.insert(new_key.clone());
        }
        if let Some(previous) = &mut self.previous_ring_set
            && previous.remove(old_key)
        {
            previous.insert(new_key.clone());
        }
        if self.expected_exits.remove(old_key) {
            self.expected_exits.insert(new_key.clone());
        }
        if let Some(value) = self.last_relaunch.remove(old_key) {
            self.last_relaunch.insert(new_key.clone(), value);
        }
        if self.pending_dismiss.as_ref() == Some(old_key) {
            self.pending_dismiss = Some(new_key.clone());
        }
        if self.last_focus_notified.as_ref() == Some(old_key) {
            self.last_focus_notified = Some(new_key.clone());
        }
        if let Some(value) = self.member_tokens.remove(old_key) {
            self.member_tokens.insert(new_key.clone(), value);
        }
        if let Some(value) = self.member_brigades.remove(old_key) {
            self.member_brigades.insert(new_key.clone(), value);
        }
        for key in self.goinkyo_pane.values_mut() {
            if key == old_key {
                *key = new_key.clone();
            }
        }
        for pending in &mut self.pending_submits {
            if pending.key == *old_key {
                pending.key = new_key.clone();
            }
        }
        for pending in &mut self.pending_kickoffs {
            if pending.key == *old_key {
                pending.key = new_key.clone();
            }
        }
        for pending in &mut self.pending_goinkyo_kickoffs {
            if pending.key == *old_key {
                pending.key = new_key.clone();
            }
        }
        had_screen
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
        let token = self.member_tokens.remove(key);
        let brigade_id = self.member_brigades.remove(key);
        if let (Some(token), Some(brigade_id)) = (token, brigade_id) {
            self.relay_states.remove(&(brigade_id, token));
        }
        self.last_forwarded_input.remove(key);
        self.unsent_operator_input.remove(key);
        self.last_output_at.remove(key);
        self.latched_attention_panes.remove(key);
        self.pending_kickoffs.retain(|pending| &pending.key != key);
        self.pending_goinkyo_kickoffs
            .retain(|pending| &pending.key != key);
        if !self.stage.is_active() {
            self.focus = Focus::Sidebar;
        }
    }

    /// Whether the operator has typed or pasted text into `key`'s pane that
    /// banto has not seen them send. The question every automated write into
    /// a pane has to ask first: such a write lands after the draft, and a
    /// following `\r` sends the two together.
    ///
    /// Knows only what banto forwarded — text the operator put there some
    /// other way, or keys an agent binds to clearing its line, are invisible
    /// (see [`Self::unsent_operator_input`] for what clears it).
    pub fn has_unsent_operator_input(&self, key: &SessionKey) -> bool {
        self.unsent_operator_input.contains(key)
    }

    /// The pane's member as the operator is told about it in a kickoff
    /// status: `worker-1 of "Fix the relay draft guard"`, or `member worker-1`
    /// when no brigade is known for the pane.
    fn kickoff_label(&self, app: &App, key: &SessionKey) -> String {
        self.members_phrase(app, &[self.kickoff_member(key)])
    }

    /// The `(brigade, token)` a kickoff status names a pane by.
    fn kickoff_member(&self, key: &SessionKey) -> (Option<BrigadeId>, MemberToken) {
        let token = self
            .member_tokens
            .get(key)
            .cloned()
            .or_else(|| key.worker_identity().map(|(_, token)| token))
            .unwrap_or_else(|| key.as_str().to_string());
        (self.member_brigades.get(key).copied(), token)
    }

    /// How the operator is told which brigade is meant: the Director's
    /// current session title in ASCII quotes, `"Fix the relay draft guard"`,
    /// else the title frozen when the brigade was formed, else — only then —
    /// `brigade 7`.
    ///
    /// The Director's session is looked for first in what the core already
    /// holds (the staged brigade's `director`, then any pane whose member is
    /// the Director of this brigade — kept through a compaction fork, which
    /// re-keys them at once), and only then in the id the last tick read from
    /// the store, which is the one source that still answers when the
    /// Director's pane is closed. A title is used only if the row has one: a
    /// row without a title would "name" the brigade by its session uuid.
    ///
    /// Shortened to [`BRIGADE_NAME_COLUMNS`] display columns on one line.
    pub fn brigade_name(&self, app: &App, brigade_id: BrigadeId) -> String {
        let mut sessions: Vec<&str> = Vec::new();
        if let Stage::Brigade { id, director, .. } = &self.stage
            && *id == brigade_id
            && let Some(key) = director
        {
            sessions.push(key.as_str());
        }
        for (key, id) in &self.member_brigades {
            if *id == brigade_id
                && self.member_tokens.get(key).map(String::as_str) == Some(DIRECTOR_TOKEN)
            {
                sessions.push(key.as_str());
            }
        }
        let facts = self.brigade_names.get(&brigade_id);
        if let Some(session) = facts.and_then(|facts| facts.director_session.as_deref()) {
            sessions.push(session);
        }
        let title = sessions.into_iter().find_map(|session| {
            app.row_for_id(session)
                .and_then(|row| row.title.as_deref())
                .and_then(display_name)
        });
        match title.or_else(|| facts.and_then(|facts| display_name(&facts.name))) {
            // An ASCII quote inside the name would end the outer quoting early.
            Some(name) => format!(
                "\"{}\"",
                shorten_to_columns(&name.replace('"', "'"), BRIGADE_NAME_COLUMNS)
            ),
            None => format!("brigade {brigade_id}"),
        }
    }

    /// Members as one phrase, grouped by brigade so each name is said once:
    /// `worker-1, worker-3 of "A" and worker-2 of "B"`. Groups follow the
    /// order their brigade first appears, members the order given; a member
    /// with no brigade stands alone as `member {token}`. Three or more groups
    /// are separated by `;` (a group's own members already use commas).
    fn members_phrase(&self, app: &App, members: &[(Option<BrigadeId>, MemberToken)]) -> String {
        enum Group<'a> {
            Brigade(BrigadeId, Vec<&'a str>),
            Lone(&'a str),
        }
        let mut groups: Vec<Group> = Vec::new();
        for (brigade_id, token) in members {
            match brigade_id {
                None => groups.push(Group::Lone(token)),
                Some(id) => {
                    let existing = groups.iter_mut().find_map(|group| match group {
                        Group::Brigade(other, tokens) if other == id => Some(tokens),
                        _ => None,
                    });
                    match existing {
                        Some(tokens) => tokens.push(token),
                        None => groups.push(Group::Brigade(*id, vec![token])),
                    }
                }
            }
        }
        let phrases: Vec<String> = groups
            .into_iter()
            .map(|group| match group {
                Group::Lone(token) => format!("member {token}"),
                Group::Brigade(id, tokens) => {
                    format!("{} of {}", tokens.join(", "), self.brigade_name(app, id))
                }
            })
            .collect();
        match phrases.as_slice() {
            [] => String::new(),
            [only] => only.clone(),
            [rest @ .., last] if rest.len() == 1 => format!("{} and {last}", rest[0]),
            [rest @ .., last] => format!("{} and {last}", rest.join("; ")),
        }
    }

    /// Record what forwarding `input` to `key`'s pane means for
    /// [`Self::has_unsent_operator_input`]. The only writer of the mark apart
    /// from [`Self::unstage`] and the rekeys; the rule itself lives in
    /// [`input_effect`].
    fn note_forwarded_input(&mut self, key: &SessionKey, input: ForwardedInput<'_>) {
        match input_effect(input) {
            InputEffect::AddsText => {
                self.unsent_operator_input.insert(key.clone());
            }
            InputEffect::Submits => {
                self.unsent_operator_input.remove(key);
            }
            InputEffect::Neutral => {}
        }
    }

    /// `brigade_id`'s Goinkyo pane, if [`Self::goinkyo_pane`] currently
    /// tracks one — read-only, for the shell's own display purposes
    /// (`tile_title`): the field itself stays private so only this crate's
    /// own one-shot-guard bookkeeping ever writes to it.
    pub fn goinkyo_pane_for(&self, brigade_id: BrigadeId) -> Option<&SessionKey> {
        self.goinkyo_pane.get(&brigade_id)
    }

    /// The durable brigade token associated with a currently staged pane.
    /// This is supplied by formation and membership events, never inferred
    /// from a pane's geometry position.
    pub fn member_token_for(&self, key: &SessionKey) -> Option<&str> {
        self.member_tokens.get(key).map(String::as_str)
    }

    /// One weight per stack tile of the staged brigade, in `panes` order.
    /// A member with no stored weight gets the mean of the stored weights of
    /// the stack's members (1 when there are none), so a Worker added later
    /// lands at an average height rather than a sliver or a slab.
    fn stack_weights(&self) -> Vec<u16> {
        let Stage::Brigade { id, panes, .. } = &self.stage else {
            return Vec::new();
        };
        let stored = self.brigade_layouts.get(id);
        let known: Vec<Option<u16>> = panes
            .iter()
            .skip(1)
            .map(|key| {
                let token = self.member_tokens.get(key)?;
                stored?.row_weights.get(token).copied()
            })
            .collect();
        let (sum, count) = known
            .iter()
            .flatten()
            .fold((0u32, 0u32), |(sum, count), w| {
                (sum + u32::from(*w), count + 1)
            });
        let mean = (sum + count / 2)
            .checked_div(count)
            .map_or(1, |mean| mean.max(1) as u16);
        known
            .into_iter()
            .map(|weight| weight.unwrap_or(mean).max(1))
            .collect()
    }

    /// The whole screen geometry for a terminal `area`. The one place the
    /// emporium's rectangles come from: drawing, mouse hit-testing and PTY
    /// sizing all call this, so they cannot disagree, and a caller cannot
    /// forget one of the layout preferences it takes.
    pub fn geometry(&self, area: Rect) -> Geometry {
        let layout = match &self.stage {
            Stage::Brigade { id, .. } => self.brigade_layouts.get(id),
            _ => None,
        };
        let stack_weights = self.stack_weights();
        let stack_collapsed = self.stack_collapsed();
        crate::layout::geometry(
            area,
            &self.stage,
            &LayoutPrefs {
                sidebar_width: self.sidebar_width,
                master_permille: layout
                    .map_or(DEFAULT_MASTER_PERMILLE, |layout| layout.master_permille),
                collapsed_stack_width: layout.map_or(DEFAULT_COLLAPSED_STACK_WIDTH, |layout| {
                    layout.collapsed_stack_width
                }),
                stack_weights: &stack_weights,
                stack_collapsed: &stack_collapsed,
                master_soloable: matches!(
                    &self.stage,
                    Stage::Brigade { panes, .. }
                        if panes.len() >= 2 && self.member_tokens.contains_key(&panes[0])
                ),
            },
            &|key| self.tile_label(key),
        )
    }

    /// One flag per stack tile of the staged brigade, in `panes` order: shown
    /// collapsed. Solo is an overlay on the operator's own collapsing: while
    /// the soloed member is a staged tile — the master or a stack tile —
    /// every stack tile but it is collapsed, whatever `minimized` says (so
    /// soloing the master collapses the whole stack); with no active solo a
    /// tile is collapsed iff its member is in `minimized`. A soloed member
    /// whose pane is momentarily gone (it exited, it is being relaunched)
    /// leaves the preference in place but inactive.
    pub fn stack_collapsed(&self) -> Vec<bool> {
        let Stage::Brigade { id, panes, .. } = &self.stage else {
            return Vec::new();
        };
        let layout = self.brigade_layouts.get(id);
        let token_of = |key: &SessionKey| self.member_tokens.get(key).map(String::as_str);
        let solo = layout
            .and_then(|layout| layout.solo_member.as_deref())
            .filter(|soloed| panes.iter().any(|key| token_of(key) == Some(*soloed)));
        panes
            .get(1..)
            .unwrap_or_default()
            .iter()
            .map(|key| match solo {
                Some(soloed) => token_of(key) != Some(soloed),
                None => token_of(key)
                    .is_some_and(|token| layout.is_some_and(|l| l.minimized.contains(token))),
            })
            .collect()
    }

    /// The tile that is the active solo, if any (see [`Self::stack_collapsed`]
    /// for when a solo is active).
    pub fn soloed_key(&self) -> Option<&SessionKey> {
        let Stage::Brigade { id, panes, .. } = &self.stage else {
            return None;
        };
        let soloed = self.brigade_layouts.get(id)?.solo_member.as_deref()?;
        panes
            .iter()
            .find(|key| self.member_tokens.get(*key).map(String::as_str) == Some(soloed))
    }

    /// What banto knows about the member of the staged brigade whose pane is
    /// `key`, if the relay has observed it yet.
    pub fn member_facts(&self, key: &SessionKey) -> Option<&MemberFacts> {
        let Stage::Brigade { id, .. } = &self.stage else {
            return None;
        };
        self.member_facts
            .get(&(*id, self.member_tokens.get(key)?.clone()))
    }

    /// When the pane last produced output.
    pub fn last_output_at(&self, key: &SessionKey) -> Option<Instant> {
        self.last_output_at.get(key).copied()
    }

    /// One flag per staged pane, `panes` order (the master's always false).
    fn pane_collapsed(&self) -> Vec<bool> {
        let mut flags = vec![false];
        flags.extend(self.stack_collapsed());
        flags
    }

    /// The label a tile's title shows — role, not position: `director`
    /// (`Stage::Brigade`'s own field) answers the Director case directly, and
    /// `goinkyo_pane_for` — already tracking the Goinkyo's current pane
    /// through every rename, built for the one-shot spawn guard — answers the
    /// Goinkyo one. Workers read their durable member token from core state,
    /// so the title agrees with the token accepted by the brigade tools even
    /// when pane geometry changes.
    ///
    /// In core, beside the geometry, because the stack boundary's hit area
    /// has to stay off exactly the cells this text is drawn on
    /// ([`crate::layout::title_columns`]).
    pub fn tile_label(&self, key: &SessionKey) -> String {
        match &self.stage {
            Stage::Brigade { id, director, .. } => {
                if director.as_ref() == Some(key) {
                    "director".to_string()
                } else if self.goinkyo_pane_for(*id) == Some(key) {
                    "goinkyo".to_string()
                } else {
                    self.member_token_for(key)
                        .map(str::to_string)
                        .unwrap_or_else(|| "session".to_string())
                }
            }
            _ => "session".to_string(),
        }
    }

    /// What to draw over the finished frame for the mouse gesture in
    /// progress; `None` when there is none.
    ///
    /// A resize is a line at the boundary. A move is the tile marked from the
    /// press, a line at the edge it would land at (none over its own slot),
    /// and, once the pointer has moved, the tile's label at the pointer.
    pub fn drag_overlay(&self, area: Rect) -> Option<DragOverlay> {
        if let Some(moving) = &self.move_drag {
            let Stage::Brigade { panes, .. } = &self.stage else {
                return None;
            };
            let from = panes.iter().position(|key| *key == moving.key)?;
            let geometry = self.geometry(area);
            let line = geometry
                .drop_slot(moving.row)
                .and_then(|slot| geometry.move_line(from, slot))
                .map(|line| (line, Axis::Horizontal));
            let ghost = moving
                .dragged
                .then(|| {
                    clip_to_screen(
                        crate::layout::title_text(&self.tile_label(&moving.key)),
                        moving.column,
                        moving.row,
                        area,
                    )
                })
                .flatten();
            return Some(DragOverlay {
                line,
                marked: Some(moving.key.clone()),
                ghost,
            });
        }
        let drag = self.drag.as_ref()?;
        let geometry = self.geometry(area);
        let handle = geometry.handle_for(&drag.boundary)?;
        // Clamped against the handle as it is now: the pane list can change
        // under a held button and move the limits the stored preview was
        // clamped to. This is what a release at the same pointer would commit.
        let preview = handle.clamp(i32::from(drag.preview));
        Some(DragOverlay {
            line: Some((handle.line_at(preview), handle.axis)),
            marked: None,
            ghost: None,
        })
    }

    /// End whatever mouse gesture is in progress without applying it.
    fn cancel_gestures(&mut self) {
        self.drag = None;
        self.move_drag = None;
    }

    /// Put the staged brigade's stack in the order its operator expects (see
    /// [`layout::sort_stack`]): the places he gave tiles, then the order the
    /// members joined. Run at the end of every `update`, whatever the update
    /// was, so every path that builds or extends `panes` — staging, a pane
    /// arriving, a relaunch, a late Director — is covered by this one place.
    ///
    /// A member whose token is not in the brigade's join order yet is
    /// appended to it first, in the order it appears in `panes`. That is the
    /// last resort, not the mechanism: a membership is recorded when it is
    /// created ([`record_join`] at `WorkerAdded`, the roster at staging and at
    /// the Goinkyo's spawn), and a token only gets here if a pane showed up for
    /// a member none of those told the core about. Learned this way it joins
    /// last, and where its pane appeared among others unlisted at the same time
    /// is arrival order — the thing everything else exists to avoid.
    fn apply_stack_order(&mut self) {
        let Stage::Brigade {
            id, panes, focused, ..
        } = &mut self.stage
        else {
            return;
        };
        let tokens = &self.member_tokens;
        let joined = self.member_order.entry(*id).or_default();
        for key in panes.iter() {
            if let Some(token) = tokens.get(key)
                && !joined.contains(token)
            {
                joined.push(token.clone());
            }
        }
        let ranked: &[MemberToken] = self
            .brigade_layouts
            .get(id)
            .map_or(&[], |layout| layout.stack_order.as_slice());
        sort_stack(panes, focused, ranked, joined, |key| {
            tokens.get(key).map(String::as_str)
        });
    }

    /// The status line's text: the drag hint for as long as a drag is in
    /// progress, otherwise the transient status. Derived rather than set, so
    /// it cannot outlast the gesture or expire under it (a status times out
    /// after [`STATUS_TIMEOUT`], a held button does not).
    pub fn displayed_status(&self) -> Option<&str> {
        if self.drag.is_some() {
            Some(DRAG_HINT)
        } else if self.move_drag.is_some() {
            Some(MOVE_HINT)
        } else {
            self.status.as_deref()
        }
    }

    /// The derived staged panes currently marked as needing operator attention.
    /// [`refresh_waiting_display`] is its sole writer.
    pub fn attention_panes(&self) -> &HashSet<SessionKey> {
        &self.attention_panes
    }

    /// The derived count for the sidebar's labelled Claude Waiting display.
    /// [`refresh_waiting_display`] is its sole writer.
    pub fn visible_waiting_count(&self) -> usize {
        self.visible_waiting_count
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
    /// `m`: collapse the focused stack tile to a strip, or open it again.
    ToggleMinimize,
    /// `z`: solo the focused tile — master or stack — (every stack tile but it
    /// collapsed), or turn its solo off. Mixer solo, not `Stage::Solo`.
    ToggleSolo,
    /// `M`: open every collapsed tile and turn the solo off.
    RestoreAll,
    /// Shift+Up / Shift+Down: move the focused stack tile one slot, see
    /// [`move_focused_tile`]. Plain arrows (and any other modifier
    /// combination) stay [`Self::Move`].
    MoveTile(Direction),
    /// `1`-`9`, in range: focus the pane at this 0-based index. This
    /// addresses `Stage`'s pane list directly (`1` is always `panes[0]`,
    /// the master tile — geometry, not role; see `Stage::Brigade::panes`'s
    /// own doc) and is deliberately untouched by the focus ring's sidebar
    /// slot — ring position and pane number are different axes, don't
    /// conflate them when touching this again.
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
        KeyCode::Up if key.modifiers == Modifiers::SHIFT => PrefixAction::MoveTile(Direction::Up),
        KeyCode::Down if key.modifiers == Modifiers::SHIFT => {
            PrefixAction::MoveTile(Direction::Down)
        }
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
        KeyCode::Char('m') if key.modifiers.is_empty() => PrefixAction::ToggleMinimize,
        KeyCode::Char('z') if key.modifiers.is_empty() => PrefixAction::ToggleSolo,
        // A shifted letter arrives as the uppercased `Char` with or without
        // the modifier flag, depending on the terminal.
        KeyCode::Char('M') if key.modifiers.is_empty() || key.modifiers == Modifiers::SHIFT => {
            PrefixAction::RestoreAll
        }
        KeyCode::Char('s') if key.modifiers.is_empty() => PrefixAction::Sidebar,
        KeyCode::Char('x') if key.modifiers.is_empty() => PrefixAction::Kill,
        _ => PrefixAction::Unbound,
    }
}

/// A position in the arrow/cycle focus ring: the sidebar, or a 0-based pane
/// index into the staged brigade (`0` is the master/solo pane, `1..` are the
/// stack, in `panes` order — geometry, not role). Distinct from
/// [`PrefixAction::FocusPane`], which addresses panes directly and never
/// includes the sidebar — see that variant's doc comment.
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
/// ...]` (length `1 + pane_count`), wrapping, skipping collapsed panes
/// (`collapsed`: one flag per pane, a missing one counts as expanded). A ring
/// of length 1 (`Empty`, `pane_count == 0`) has nowhere else to go and stays
/// put.
fn cycle_forward(from: FocusSlot, pane_count: usize, collapsed: &[bool]) -> FocusSlot {
    let ring_len = 1 + pane_count;
    if ring_len <= 1 {
        return FocusSlot::Sidebar;
    }
    let pos = match from {
        FocusSlot::Sidebar => 0,
        FocusSlot::Pane(n) => 1 + n,
    };
    let mut next = (pos + 1) % ring_len;
    while next != 0 && collapsed.get(next - 1).copied().unwrap_or(false) {
        next = (next + 1) % ring_len;
    }
    if next == 0 {
        FocusSlot::Sidebar
    } else {
        FocusSlot::Pane(next - 1)
    }
}

/// One armed arrow key's target slot, navigating the three-column grid
/// (sidebar | master/solo | stack) by geometry rather than ring order.
/// Left/Right cross columns; Up/Down step within the stack only, clamped
/// (no wrap). Every edge case (sidebar's Left, a solo/master pane's
/// Up/Down, a stack pane's Right) is a deliberate no-op, not an omission.
fn arrow_target(
    from: FocusSlot,
    direction: Direction,
    pane_count: usize,
    collapsed: &[bool],
) -> FocusSlot {
    let expanded = |n: usize| !collapsed.get(n).copied().unwrap_or(false);
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
        (FocusSlot::Pane(0), Direction::Right) => (1..pane_count)
            .find(|n| expanded(*n))
            .map_or(FocusSlot::Pane(0), FocusSlot::Pane),
        (FocusSlot::Pane(0), Direction::Up | Direction::Down) => FocusSlot::Pane(0),

        (FocusSlot::Pane(_), Direction::Left) => FocusSlot::Pane(0),
        (FocusSlot::Pane(n), Direction::Right) => FocusSlot::Pane(n),
        (FocusSlot::Pane(n), Direction::Up) => (1..n)
            .rev()
            .find(|m| expanded(*m))
            .map_or(FocusSlot::Pane(n), FocusSlot::Pane),
        (FocusSlot::Pane(n), Direction::Down) => (n + 1..pane_count)
            .find(|m| expanded(*m))
            .map_or(FocusSlot::Pane(n), FocusSlot::Pane),
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
    /// The other direction, reached by `D` on a row the operator can see
    /// only at [`crate::app::Reveal::Archived`].
    Unarchive {
        id: String,
        title: String,
    },
    JoinGroup {
        session_id: String,
        target: GroupJoinTargetData,
    },
    RenameGroup {
        group_id: GroupId,
        old_name: String,
        new_name: String,
    },
    /// `name` travels with the id purely for the status message
    /// (`Event::GroupDeleteDone`'s doc) — `Store::delete_group` itself only
    /// needs `group_id`.
    DeleteGroup {
        group_id: GroupId,
        name: String,
    },
    ResolveMembership {
        session_id: String,
    },
    /// Persist a brigade's arrangement preferences: the whole snapshot, in one
    /// transaction (`Store::save_brigade_layout`). Fire and forget, like a
    /// pin: a failed write loses a preference, not the session.
    SaveBrigadeLayout {
        brigade_id: BrigadeId,
        layout: StoredLayout,
    },
    /// Persist the sidebar width (`Store::set_sidebar_width`).
    SaveSidebarWidth {
        width: u16,
    },
    FormBrigade {
        director_row_id: String,
        director_agent: AgentKind,
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
    /// Resets a Goinkyo's `session_id` back to `None` — see
    /// `stage_brigade`'s own doc on the stranded-Goinkyo case this answers,
    /// and `Store::clear_member_session`'s own doc for why the store side is
    /// safe to call unconditionally.
    ClearMemberSession {
        brigade_id: BrigadeId,
        token: MemberToken,
    },
}

/// Mirrors `crate::app::GroupJoinTarget`, but by value (the original borrows
/// from the modal state, which `update` cannot hold across the round trip to
/// the shell and back).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupJoinTargetData {
    Existing(i64, String),
    New(String),
    /// Take the session out of whatever group it is in, joining no other.
    Leave,
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
    /// is `--model <model>` for a freshly-spawned Worker or Goinkyo (never
    /// set for a resume — the model was already fixed at the session's
    /// original launch). `effort` is `--effort <level>`, so far only ever
    /// set for a freshly-spawned Goinkyo (`BrigadeConfig::goinkyo_effort`)
    /// — the shell drops it for a Codex target, which has no such flag; see
    /// `banto::opener::AgentLaunch::Claude`'s own `effort` field. `permission_mode`
    /// (`--permission-mode <mode>`, `BrigadeConfig::goinkyo_permission_mode`)
    /// and `disallowed_tools` (`--disallowedTools <tools>`,
    /// `BrigadeConfig::goinkyo_disallowed_tools`) both diverge from `effort`
    /// the same way and for the same reason: unlike a model or a reasoning
    /// level, who may approve a tool call and which tools exist to call are
    /// properties of the *role*, not of whether this particular launch
    /// happens to be fresh or a resume — so both are set for a Goinkyo
    /// either way, never for a Worker or Director, and never for Codex
    /// (Claude-only flags; `banto::opener::AgentLaunch::Codex` carries neither
    /// field at all). The shell answers with `Event::Spawned`/`SpawnFailed`.
    OpenEmbedded {
        key: SessionKey,
        target: SessionToOpen,
        brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
        disallowed_tools: Option<String>,
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
    /// Read whether Claude has already been told to trust `cwd` — before
    /// typing [`GOINKYO_KICKOFF_LINE`] into `key`'s pane blind. Mirrors
    /// [`Cmd::CheckWorkerDirectoryTrust`]'s reasoning above, but backed by
    /// `banto_io::directory_trust::claude_directory_trust` — a different
    /// underlying trust registry Codex's own check does not read
    /// (`execute_check_worker_directory_trust` calls `codex_directory_trust`
    /// specifically). The Goinkyo's `cwd` is ordinarily the Director's own
    /// already-trusted one, so this is expected to pass immediately in the
    /// common case — but this is the first time banto ever types blind into
    /// a *Claude* pane at all (a Worker never gets typed into —
    /// `poll_discovery`'s own doc), so the same risk
    /// `CheckWorkerDirectoryTrust` exists to avoid for Codex applies here
    /// too, however rarely it actually fires. Issued by [`update_tick`]
    /// once a [`PendingGoinkyoKickoff`]'s quiet period has elapsed, every
    /// tick, until the answer comes back trusted. The shell answers with
    /// `Event::GoinkyoDirectoryTrustChecked`.
    CheckGoinkyoDirectoryTrust {
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
    /// Ring the host terminal once for one or more newly visible attention
    /// marks. A bare BEL changes neither cells nor cursor, so the shell may
    /// emit it before the next draw just like OSC 52 forwarding.
    RingHostBell,
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
        /// What the shell's own `child.wait()` reported, if anything —
        /// `None` covers both a stream recorded before this field existed
        /// (`#[serde(default)]`, same precedent as `RowsLoaded::superseded`)
        /// and a live tick where the shell reported the pane gone before its
        /// own wait-observation caught up (`banto::embedded::session::
        /// PtyHandle::poll`'s doc has the race). [`PtyExitReason::Unknown`]
        /// is different from either: the shell *did* observe the exit, but
        /// `wait()` itself returned an error rather than a status.
        #[serde(default)]
        reason: Option<PtyExitReason>,
    },
    /// The shell's `Cmd::WritePty` executor found no live handle for `key`
    /// — see `update_pty_write_dropped`'s own doc for why this is expected
    /// to mean a bug, not an ordinary race, and is surfaced rather than
    /// silently swallowed the way it used to be.
    PtyWriteDropped {
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
    UnarchiveDone {
        title: String,
        result: Result<(), String>,
    },
    GroupJoinDone {
        session_id: String,
        /// `Ok(None)` is the session leaving its group rather than joining
        /// one — the same modal's other outcome. An `Option` rather than its
        /// own event so the failure path stays in one place; a recorded
        /// stream from before it existed still reads back, since a present
        /// pair deserializes as `Some`.
        result: Result<Option<(i64, String)>, String>,
    },
    /// Answers `Cmd::Store(StoreIntent::RenameGroup)`. `old_name`/`new_name`
    /// travel through even on `Err` so the status message can name both —
    /// `Store::rename_group` fails on a duplicate name, and "failed to
    /// rename to X" without saying what it was renaming from reads as if the
    /// modal lost track of its own target.
    GroupRenameDone {
        group_id: GroupId,
        old_name: String,
        new_name: String,
        result: Result<(), String>,
    },
    /// Answers `Cmd::Store(StoreIntent::DeleteGroup)`.
    GroupDeleteDone {
        group_id: GroupId,
        name: String,
        result: Result<(), String>,
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
        /// The resolved brigade's stored arrangement preferences, read in the
        /// same shell round trip so they are in state before the brigade's
        /// first geometry is computed. `None` for no brigade, no usable
        /// read, or a capture recorded before this field existed.
        #[serde(default)]
        layout: Option<StoredLayout>,
    },
    /// The sidebar width the store held at startup, if it held one. Fed once,
    /// ahead of the first resize, as an `Event` rather than constructor
    /// state: a replay rebuilds every pane's size from the event stream, and
    /// a width that reached state any other way would size them differently.
    SidebarWidthLoaded {
        width: Option<u16>,
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
    /// Answers `Cmd::CheckGoinkyoDirectoryTrust`: whether Claude has been
    /// told to trust `key`'s pane's own cwd. Same collapsed-bool reasoning
    /// as [`Self::WorkerDirectoryTrustChecked`].
    GoinkyoDirectoryTrustChecked {
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
    /// A live member's Claude session forked in place: AUTO-compaction
    /// assigns a *new* session id to the same live process (manual
    /// `/compact` does not) — the process's own `sessions/<pid>.json` live
    /// file simply starts reporting a different `sessionId`, still under
    /// the same pid. `old_id` is what the store/pane still know the member
    /// by; `new_id` is what the process now reports. Gathered shell-side
    /// (`emporium::gather_fork_observations`) alongside the relay tick, one
    /// per tick a live member's recorded id disagrees with its own live
    /// process — so this may repeat for the same fork until the store row
    /// (and the pane it keys) actually catch up; the handler must tolerate
    /// that.
    MemberSessionForked {
        brigade_id: BrigadeId,
        token: MemberToken,
        old_id: String,
        new_id: String,
    },
    /// ~1/s: relay observations for every live brigade member (gathered
    /// shell-side — store + live-session reads), plus the trigger to flush
    /// any due phase-two nudge submit and expire the status message.
    Tick {
        relay: Vec<RelayObservation>,
        /// What the same gather read about each brigade it visited: its
        /// stored name and its Director's session id. Names a brigade to the
        /// operator in the statuses that mention one. Empty when the gather
        /// reads nothing (a store error) and in captures recorded before
        /// this field existed.
        #[serde(default)]
        brigades: Vec<BrigadeNameFacts>,
    },
    /// ~1/s: what the shell observed about the staged brigade's Goinkyo —
    /// gathered shell-side alongside `Tick`'s own observations, but its own
    /// event rather than a field added to `Tick`: every existing
    /// `Event::Tick` literal (production and the many in tests) would
    /// otherwise need a new field just to keep compiling, for a fact only
    /// ever relevant to a brigade that has summoned a Goinkyo. See
    /// [`GoinkyoObservation`]'s own doc for what each case means.
    GoinkyoAwaitingSpawn {
        observation: GoinkyoObservation,
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
            let mut rang = false;
            if let Some(screen) = state.screens.get_mut(&key) {
                screen.process(&chunk);
                rang = screen.take_audible_bell();
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
            if rang && effective_focused_pane(state).as_ref() != Some(&key) {
                state.latched_attention_panes.insert(key.clone());
            }
            cmds
        }
        Event::PtyExited { key, reason } => {
            update_pty_exited(state, app, brigade, key, reason, now)
        }
        Event::PtyWriteDropped { key } => update_pty_write_dropped(state, key, now),
        Event::NewSessionCwdChecked { cwd, is_dir } => {
            update_new_session_cwd_checked(state, app, cwd, is_dir)
        }
        Event::Spawned { key } => update_spawned(state, brigade, key, now),
        Event::SpawnFailed { key, error } => update_spawn_failed(state, key, error, now),
        Event::RowsLoaded {
            rows,
            hidden,
            directors,
            superseded,
        } => {
            app.replace_rows(rows);
            app.set_hidden_member_ids(hidden);
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
        Event::UnarchiveDone { title, result } => {
            let message = match result {
                Ok(()) => format!("restored session {title}"),
                Err(err) => format!("failed to restore session {title}: {err}"),
            };
            state.set_status(message, now);
            vec![Cmd::Reload]
        }
        Event::GroupJoinDone { session_id, result } => {
            match result {
                Ok(Some((group_id, group_name))) => {
                    state.set_status(format!("joined group \"{group_name}\""), now);
                    app.set_session_group_cache(&session_id, group_id, group_name);
                }
                Ok(None) => {
                    state.set_status("left the group".to_string(), now);
                    app.clear_session_group_cache(&session_id);
                }
                Err(err) => state.set_status(format!("failed to join group: {err}"), now),
            }
            Vec::new()
        }
        Event::GroupRenameDone {
            group_id,
            old_name,
            new_name,
            result,
        } => {
            match result {
                Ok(()) => {
                    state.set_status(
                        format!("renamed group \"{old_name}\" to \"{new_name}\""),
                        now,
                    );
                    app.rename_group_cache(group_id, new_name);
                }
                Err(err) => {
                    state.set_status(format!("failed to rename group \"{old_name}\": {err}"), now)
                }
            }
            Vec::new()
        }
        Event::GroupDeleteDone {
            group_id,
            name,
            result,
        } => {
            match result {
                Ok(()) => {
                    state.set_status(
                        format!("deleted group \"{name}\" \u{2014} its members are now ungrouped"),
                        now,
                    );
                    app.delete_group_cache(group_id);
                }
                Err(err) => {
                    state.set_status(format!("failed to delete group \"{name}\": {err}"), now)
                }
            }
            Vec::new()
        }
        Event::MembershipResolved {
            session_id,
            membership,
            members,
            layout,
        } => {
            update_membership_resolved(state, app, brigade, session_id, membership, members, layout)
        }
        Event::SidebarWidthLoaded { width } => {
            if let Some(width) = width {
                // A preference like any other: clamped wherever it is used,
                // and what the store holds, so not written back.
                state.sidebar_width = width;
                state.persisted_sidebar_width = width;
            }
            Vec::new()
        }
        Event::CodexTrustChecked {
            primed,
            hook_launchable,
        } => update_codex_trust_checked(state, app, primed, hook_launchable, now),
        Event::WorkerDirectoryTrustChecked { key, trusted } => {
            update_worker_directory_trust_checked(state, app, key, trusted, now)
        }
        Event::GoinkyoDirectoryTrustChecked { key, trusted } => {
            update_goinkyo_directory_trust_checked(state, app, key, trusted, now)
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
            brigade,
            AddedWorker {
                brigade_id,
                cwd,
                worker_agent,
                worker_model,
            },
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
            app.set_hidden_member_ids(hidden);
            app.set_directors(directors);
            Vec::new()
        }
        Event::MemberSessionForked {
            brigade_id,
            token,
            old_id,
            new_id,
        } => update_member_session_forked(state, brigade_id, token, old_id, new_id, now),
        Event::Tick { relay, brigades } => update_tick(state, app, brigade, relay, brigades, now),
        Event::GoinkyoAwaitingSpawn { observation } => {
            update_goinkyo_awaiting_spawn(state, brigade, observation, now)
        }
        Event::WindowFocusChanged { focused } => {
            state.window_focused = focused;
            // Focus leaving the window is the one way a release can be lost
            // without a `Moved` ever arriving: the terminal stops reporting.
            if !focused {
                state.cancel_gestures();
            }
            Vec::new()
        }
    };
    // Before anything sizes a tile: whatever rebuilt `panes` this event is
    // put back in the operator's order first.
    state.apply_stack_order();
    refocus_expanded(state, None);
    validate_drag(state, app);
    cmds.extend(resize_staged_tiles(state));
    cmds.extend(persist_preferences(state));
    cmds.extend(refresh_waiting_display(state, app, now));
    cmds.extend(emit_focus_transitions(state));
    cmds
}

/// Refresh the one fact behind both attention views: a mark is on screen when
/// a Claude Waiting row/tile is visible or a staged pane has latched BEL. The
/// border reads the staged subset, the sidebar keeps its Claude-only count,
/// and the host bell is the edge where this full visible set gains a member.
/// Keeping all three here prevents their filters from drifting apart.
///
/// App caches its list result; this refresh copies that cache only when its
/// generation changes. Ordinary PTY chunks therefore inspect only staged
/// panes, rather than re-scanning a potentially large Waiting list.
fn refresh_waiting_display(state: &mut EmporiumState, app: &App, now: Instant) -> Vec<Cmd> {
    // `effective_focused_pane` is also the contract used for DECSET 1004:
    // `window_focused` starts true and self-corrects on crossterm's first
    // FocusGained/FocusLost, so before that small self-healing window banto
    // assumes it is frontmost. Clearing here therefore means the operator can
    // see this pane by the same already-accepted focus fact, not a new host
    // focus-reporting mechanism.
    if let Some(key) = effective_focused_pane(state) {
        state.latched_attention_panes.remove(&key);
    }

    let list_changed = state.ring_list_generation != app.filtered_waiting_generation();
    if list_changed {
        state.ring_list_waiting = app
            .filtered_waiting_ids()
            .iter()
            .map(|id| SessionKey::from_id(id))
            .collect();
        state.ring_list_generation = app.filtered_waiting_generation();
    }

    let mut count = app.filtered_waiting_count();
    let mut staged_ids = HashSet::new();
    let mut panes = HashSet::new();
    let mut inspect = |key: &SessionKey| {
        if app.is_waiting_id(key.as_str()) {
            if !app.is_filtered_waiting_id(key.as_str()) && staged_ids.insert(key.clone()) {
                count += 1;
            }
            panes.insert(key.clone());
        }
        if state.latched_attention_panes.contains(key) {
            panes.insert(key.clone());
        }
    };
    match &state.stage {
        Stage::Empty => {}
        Stage::Solo(key) => inspect(key),
        Stage::Brigade { panes: staged, .. } => staged.iter().for_each(&mut inspect),
    }
    let old_panes = std::mem::replace(&mut state.attention_panes, panes);
    state.visible_waiting_count = count;

    let mut gained = false;
    match &mut state.previous_ring_set {
        None => {
            let mut seeded = state.ring_list_waiting.clone();
            seeded.extend(state.attention_panes.iter().cloned());
            state.previous_ring_set = Some(seeded);
        }
        Some(previous) if list_changed => {
            let mut current = state.ring_list_waiting.clone();
            current.extend(state.attention_panes.iter().cloned());
            gained = current.iter().any(|key| !previous.contains(key));
            *previous = current;
        }
        Some(previous) => {
            let mut changed = old_panes;
            changed.extend(state.attention_panes.iter().cloned());
            for key in changed {
                let visible =
                    state.ring_list_waiting.contains(&key) || state.attention_panes.contains(&key);
                if visible {
                    gained |= previous.insert(key);
                } else {
                    previous.remove(&key);
                }
            }
        }
    }

    if gained
        && state
            .last_attention_bell
            .is_none_or(|last| now.saturating_duration_since(last) >= ATTENTION_BELL_COOLDOWN)
    {
        state.last_attention_bell = Some(now);
        vec![Cmd::RingHostBell]
    } else {
        Vec::new()
    }
}

/// Resize every currently-staged tile's `Screen` to match the current
/// layout, emitting a `Cmd::ResizePty` only for the ones that actually
/// changed (matches the pre-migration per-tick unconditional resize, whose
/// dedup lived inside `EmbeddedSession::resize` — moved here, now pure).
/// Called once at the end of every `update` regardless of event kind: cheap
/// (a HashMap lookup per staged tile) and correct without needing every
/// stage-mutating branch to remember to call it.
fn resize_staged_tiles(state: &mut EmporiumState) -> Vec<Cmd> {
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    let mut cmds = Vec::new();
    for ((key, rect), collapsed) in geometry.tiles.into_iter().zip(geometry.collapsed) {
        // A collapsed tile's child keeps the size it had: the grid's resize
        // truncates rather than reflows, and a child repainting into a
        // three-row grid would destroy the screen the operator comes back to
        // (an off-stage pane lives the same way). It is sized by this same
        // pass the moment it expands.
        if collapsed {
            continue;
        }
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
    // The terminal changed under the gesture: whatever the drag was holding
    // was measured against geometry that no longer exists.
    state.cancel_gestures();
    let areas = state.geometry(Rect::new(0, 0, width, height)).areas;
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

    // Esc cancels a drag (a boundary, or a tile being moved) and goes no
    // further: it must not reach the child or the sidebar.
    if (state.drag.is_some() || state.move_drag.is_some()) && code == KeyCode::Esc {
        state.cancel_gestures();
        return Vec::new();
    }

    if app.modal().is_some() {
        return update_modal_key(state, app, code, key.modifiers);
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
        let collapsed = state.pane_collapsed();
        if let Stage::Brigade { panes, focused, .. } = &mut state.stage
            && !panes.is_empty()
        {
            // The next expanded pane; the master is never collapsed, so the
            // walk always ends somewhere.
            let mut next = (*focused + 1) % panes.len();
            while collapsed.get(next).copied().unwrap_or(false) {
                next = (next + 1) % panes.len();
            }
            *focused = next;
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
                    state.note_forwarded_input(&target, ForwardedInput::Key(&key));
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
                    let reveal = app.cycle_reveal();
                    state.set_status(format!("showing {}", reveal.label()), now);
                    Vec::new()
                }
                (KeyCode::Char('D'), Modifiers::NONE) => unarchive_selected(state, app, now),
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
            let literal = state.prefix.as_key_event();
            let bytes = key_to_bytes(&literal);
            state.last_forwarded_input.insert(target.clone(), now);
            if bytes.is_empty() {
                Vec::new()
            } else {
                state.note_forwarded_input(&target, ForwardedInput::Key(&literal));
                cancel_pending_submit_on_input(&mut state.pending_submits, &target);
                vec![Cmd::WritePty { key: target, bytes }]
            }
        }
        PrefixAction::CyclePane => {
            let next = cycle_forward(
                current_focus_slot(state),
                pane_count,
                &state.pane_collapsed(),
            );
            apply_focus_slot(state, next);
            Vec::new()
        }
        PrefixAction::Move(direction) => {
            let next = arrow_target(
                current_focus_slot(state),
                direction,
                pane_count,
                &state.pane_collapsed(),
            );
            apply_focus_slot(state, next);
            Vec::new()
        }
        PrefixAction::MoveTile(direction) => move_focused_tile(state, direction),
        PrefixAction::ToggleMinimize => collapse_focused_tile(state),
        PrefixAction::ToggleSolo => solo_focused_tile(state),
        PrefixAction::RestoreAll => {
            if matches!(state.stage, Stage::Brigade { .. }) {
                restore_all(state);
            } else {
                state.status = Some("prefix: no stack to restore".to_string());
            }
            Vec::new()
        }
        PrefixAction::FocusPane(index) => {
            // A collapsed pane is opened first: addressing it by number is
            // asking to work in it.
            if let Stage::Brigade { panes, .. } = &state.stage
                && index > 0
                && state.pane_collapsed().get(index).copied().unwrap_or(false)
                && let Some(key) = panes.get(index).cloned()
            {
                toggle_minimize(state, &key);
            }
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
            // Role, not position: the focused pane is dismissable when we
            // know who the Director is *and* it isn't them —
            // `update_membership_resolved` is what actually tells Worker
            // from Goinkyo apart once dismiss is confirmed, so no store
            // round trip is needed just to grow the dialog its second
            // choice. `director == None` (the Director's own pane closed,
            // or never resolved) is the safe side: refuse rather than guess
            // — see `Stage::Brigade::director`'s own doc.
            let dismissable = matches!(
                &state.stage,
                Stage::Brigade { director, focused, panes, .. }
                    if director.is_some() && director.as_ref() != panes.get(*focused)
            );
            app.open_confirm_kill_modal(target.as_str().to_string(), title, dismissable);
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

/// Keys while a modal is open. `mods` exists solely for the `g` dialog's
/// Ctrl+R/Ctrl+X verbs (rename/delete the highlighted group) — every other
/// arm ignores it, same as it always has, since a modal's other keys have
/// never been chords. Checked first and unconditionally: `code` alone (a
/// bare `Char('r')`) cannot tell Ctrl+R from a plain `r` typed into a text
/// field, and typed text must never be swallowed by a would-be verb that
/// doesn't apply to whatever modal happens to be open (`App::open_group_
/// rename_modal`/`open_confirm_group_delete_modal` are themselves the guard
/// for "only on `GroupJoin`, only on an existing group" — this dispatch
/// doesn't duplicate that check).
fn update_modal_key(
    state: &mut EmporiumState,
    app: &mut App,
    code: KeyCode,
    mods: Modifiers,
) -> Vec<Cmd> {
    if mods.ctrl {
        match code {
            KeyCode::Char('r') => app.open_group_rename_modal(),
            KeyCode::Char('x') => app.open_confirm_group_delete_modal(),
            _ => {}
        }
        return Vec::new();
    }
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
        GroupRename,
        GroupDelete,
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
        Some(Modal::GroupRename(_)) => Some(Kind::GroupRename),
        Some(Modal::ConfirmGroupDelete { .. }) => Some(Kind::GroupDelete),
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
        Some(Kind::GroupRename) => confirm_group_rename_modal(app),
        Some(Kind::GroupDelete) => confirm_group_delete_modal(app),
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
/// per install, not per-member — see `banto::opener::session_start_hook_override`'s
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
            director_agent: formation.director_agent,
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
        director_agent: formation.director_agent,
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
        GroupJoinTarget::Leave => GroupJoinTargetData::Leave,
    };
    vec![Cmd::Store(StoreIntent::JoinGroup { session_id, target })]
}

fn confirm_group_rename_modal(app: &mut App) -> Vec<Cmd> {
    let Some((group_id, old_name, new_name)) = app.modal_group_rename_target() else {
        return Vec::new();
    };
    app.close_modal();
    vec![Cmd::Store(StoreIntent::RenameGroup {
        group_id,
        old_name,
        new_name,
    })]
}

fn confirm_group_delete_modal(app: &mut App) -> Vec<Cmd> {
    let Some(Modal::ConfirmGroupDelete { group_id, name }) = app.modal() else {
        return Vec::new();
    };
    let group_id = *group_id;
    let name = name.clone();
    app.close_modal();
    vec![Cmd::Store(StoreIntent::DeleteGroup { group_id, name })]
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
        effort: None,
        permission_mode: None,
        disallowed_tools: None,
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
/// Director/solo pane, and the default for a dismissable member's — Worker
/// or Goinkyo) just makes the exit happen — no store mutation, membership
/// persists, and a killed member respawns fresh under the same token the
/// next time its brigade is staged (the existing, field-tested disposable-
/// Worker semantics `stage_brigade` already has for one whose process is
/// simply gone — equally true for a Goinkyo with a resolved session id, its
/// own resumed-member branch there). The passive `Event::PtyExited` fold
/// (unchanged, see `update_pty_exited`) is what actually cleans up the pane
/// once the kill takes effect.
///
/// [`KillChoice::Dismiss`] needs `(brigade_id, token)` before
/// `StoreIntent::DismissWorker` can be built: a still-awaiting-discovery
/// member's synthetic key already embeds them
/// ([`SessionKey::worker_identity`] — Worker-named, Goinkyo-shaped too, see
/// its own doc), so that path builds the intent right here; a resolved
/// member's real session id does not, so that path stashes
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
        return vec![kill_pane(state, key)];
    }
    state.pending_dismiss = Some(key.clone());
    state.pending_dismiss_member = None;
    match key.worker_identity() {
        Some((brigade_id, token)) => {
            state.pending_dismiss_member = Some((brigade_id, token.clone()));
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
    layout: Option<StoredLayout>,
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
        // A Goinkyo dismisses exactly the way a Worker does — same store
        // operation, same pane cleanup — so it shares this arm rather than
        // landing in the abort one below; only a Director can't be
        // dismissed this way (that's disband's job, `B`). `Some((_, _,
        // BrigadeRole::Director)) | None` spelled out rather than `_`: a
        // role added later has to land in one arm or the other explicitly,
        // not fall silently into whichever one the wildcard used to catch —
        // an unexpected answer here has always meant "abort", never
        // "guess".
        return match membership {
            Some((brigade_id, token, BrigadeRole::Worker | BrigadeRole::Goinkyo)) => {
                state.pending_dismiss_member = Some((brigade_id, token.clone()));
                vec![Cmd::Store(StoreIntent::DismissWorker { brigade_id, token })]
            }
            Some((_, _, BrigadeRole::Director)) | None => {
                state.pending_dismiss = None;
                state.pending_dismiss_member = None;
                Vec::new()
            }
        };
    }
    let Some(row) = app.row_for_id(&session_id).cloned() else {
        return Vec::new();
    };
    match purpose {
        PendingMembership::Activate => match membership {
            Some((brigade_id, _, BrigadeRole::Director)) => {
                adopt_stored_layout(state, brigade_id, layout.as_ref());
                stage_brigade(
                    state,
                    app,
                    brigade_id,
                    &members.unwrap_or_default(),
                    brigade,
                )
            }
            // Spelled out rather than `_`, same reasoning as the
            // `DismissWorker` branch above: staging the whole brigade is a
            // Director-only action, so a Goinkyo activated directly opens
            // solo, same as a Worker does.
            Some((_, _, BrigadeRole::Worker | BrigadeRole::Goinkyo)) | None => {
                open_solo(state, &row)
            }
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
            // Its own arm rather than folded into the Worker one above: a
            // Goinkyo is not a Worker, and reusing that exact message would
            // tell the operator something false about who was refused.
            Some((_, _, BrigadeRole::Goinkyo)) => {
                state.status = Some("a goinkyo can't be promoted to Director directly".to_string());
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
            director_agent: row.agent,
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
            director_agent: row.agent,
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
        director_agent: row.agent,
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
            director_agent: formation.director_agent,
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
            director_agent: formation.director_agent,
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
    vec![kill_pane(state, director_key)]
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
        effort: None,
        permission_mode: None,
        disallowed_tools: None,
    }]
}

/// What a role is launched with, by role and product — everything about a
/// member that banto's configuration decides rather than the session itself.
///
/// All four are properties of the *role*, on every launch, fresh or resumed,
/// which is why they are computed in one place from the role alone and not
/// at each site that happens to start a member.
///
/// `model` used to look like a launch property instead, passed only for a
/// resumed Worker, because of an incident: before `46f2b8a8` (2026-07-25)
/// added that branch, a resumed Worker silently ran on the operator's own
/// default model instead of the brigade's configured one, observed live as
/// Opus 5 Workers on WSL. The Goinkyo role didn't exist yet when that landed
/// (added 2026-08-04, adf9a27/26a3987), and nobody made the equivalent trip
/// for it afterward — not a decision that a Goinkyo's model should be fixed
/// at birth, just a branch written before the role it was missing even
/// existed. Two launch sites drifting apart is the whole failure mode; there
/// is one now.
///
/// Measured (claude 2.1.223, 2026-08-06, one scratch session resumed
/// repeatedly) that Claude Code remembers a session's model without being
/// told again — `--model haiku` once, then resumed with no `--model` at all,
/// stayed on Haiku — while it does *not* remember effort the same way:
/// `--effort low` once, then resumed with no `--effort` at all, reverted
/// straight to the operator's own global `effortLevel` setting, not to `low`.
/// That difference does not mean model may be omitted; it means giving it
/// again is usually a no-op, and "usually" is exactly how the Goinkyo's own
/// missing case went unnoticed for two days — the value happened to already
/// be right.
///
/// `permission_mode` answers "who's here to click 'allow'", not "what
/// should this session think with" — a Worker's own operator is no more
/// watching every pane than the Goinkyo's, so it gets the same non-default.
/// `disallowed_tools` stays Goinkyo-only: it answers "what may this role
/// even attempt", which only needs guarding for a role meant to be called
/// holding nothing of its own — a Worker's whole job is the editing that
/// setting exists to deter (see `BrigadeConfig::goinkyo_disallowed_tools`'s
/// own doc). Closing a consultation's pane mid-conversation and re-staging
/// the brigade brings the Goinkyo right back to an unattended prompt (or an
/// editable working tree) if one comes up, same as a fresh spawn.
///
/// The Director gets none of them: `BrigadeConfig` has no `director_model`
/// or `director_effort` field at all — that session is the operator's own,
/// launched (and re-launched) entirely outside banto's control.
struct RoleLaunch {
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    disallowed_tools: Option<String>,
}

impl RoleLaunch {
    fn of(role: BrigadeRole, agent: AgentKind, brigade: &BrigadeConfig) -> Self {
        Self {
            model: match role {
                BrigadeRole::Worker => brigade.worker_model_for(agent),
                BrigadeRole::Goinkyo => brigade.goinkyo_model(),
                BrigadeRole::Director => None,
            }
            .map(str::to_string),
            effort: (role == BrigadeRole::Goinkyo)
                .then(|| brigade.goinkyo_effort())
                .flatten()
                .map(str::to_string),
            permission_mode: match role {
                BrigadeRole::Worker => brigade.worker_permission_mode(),
                BrigadeRole::Goinkyo => brigade.goinkyo_permission_mode(),
                BrigadeRole::Director => None,
            }
            .map(str::to_string),
            disallowed_tools: (role == BrigadeRole::Goinkyo)
                .then(|| brigade.goinkyo_disallowed_tools())
                .flatten()
                .map(str::to_string),
        }
    }
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
    let director_session_id = members
        .iter()
        .find(|(_, role, _)| *role == BrigadeRole::Director)
        .and_then(|(_, _, sid)| sid.as_deref());
    // Leniently `None` when the store's own Director row has no session id
    // recorded yet — `Stage::Brigade::director`'s own doc explains why that's
    // a legitimate state, not something to work around here.
    let director = director_session_id.map(SessionKey::from_id);
    let director_row = director_session_id.and_then(|sid| app.row_for_id(sid));
    let cwd = director_row
        .and_then(|row| row.cwd.clone())
        .unwrap_or_else(|| PathBuf::from("."));
    // A Director not yet resolved to a row can't say what product it is
    // either — the safe default rather than blocking staging on it, same
    // fallback `add_worker`/`update_brigade_formed` use.
    let director_agent = director_row.map_or(AgentKind::ClaudeCode, |row| row.agent);
    let fresh_worker_agent = resolve_worker_agent(brigade.worker_agent, director_agent);

    // The store's roster is in the order the members joined: the one record of
    // it that survives a restart. It replaces what this run learned, so the
    // order a brigade shows never depends on which pane happened to arrive
    // first or on how long the brigade was off the stage.
    state.member_order.insert(
        brigade_id,
        members.iter().map(|(token, _, _)| token.clone()).collect(),
    );
    let mut panes = Vec::new();
    let mut cmds = Vec::new();
    let mut missing = 0;
    for (token, role, session_id) in members {
        let resolved_row = session_id.as_deref().and_then(|sid| app.row_for_id(sid));
        match resolved_row {
            Some(row) => {
                let key = SessionKey::from_id(&row.id);
                state.member_tokens.insert(key.clone(), token.clone());
                state.member_brigades.insert(key.clone(), brigade_id);
                // Seeded here, ahead of the resume's own success or failure:
                // this is the one-shot spawn guard `update_goinkyo_awaiting_
                // spawn` checks (and, from here, the record that lets a
                // *later* re-stage tell "this consultation already has a
                // pane, live or in flight" from "the row is new" — see that
                // function's own doc for how it now tells the two apart by
                // whether the key it finds here is synthetic or real). Never
                // seeded for a Worker or Director — `goinkyo_pane` only ever
                // means "the current brigade's Goinkyo", nothing else reads
                // it for any other role.
                if *role == BrigadeRole::Goinkyo {
                    state.goinkyo_pane.insert(brigade_id, key.clone());
                }
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
                    let launch = RoleLaunch::of(*role, row.agent, brigade);
                    cmds.push(Cmd::OpenEmbedded {
                        key,
                        target: SessionToOpen {
                            id: row.id.clone(),
                            agent: row.agent,
                            title: row.display_title().to_string(),
                            cwd: row.cwd.clone().unwrap_or_else(|| PathBuf::from(".")),
                        },
                        brigade: Some((brigade_id, token.clone(), *role)),
                        model: launch.model,
                        effort: launch.effort,
                        permission_mode: launch.permission_mode,
                        disallowed_tools: launch.disallowed_tools,
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
                state.member_tokens.insert(key.clone(), token.clone());
                state.member_brigades.insert(key.clone(), brigade_id);
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
                        brigade.worker_permission_mode(),
                        fresh_worker_agent,
                    ));
                }
            }
            // The stranded case: a Goinkyo whose row has a `session_id` but
            // no resolvable row — unlike an undiscovered Worker (whose
            // `session_id` is still `None` at this point, caught above), a
            // Goinkyo's discovery already ran once and assigned it a real
            // id, but `app.row_for_id` still can't find a row for it because
            // Claude Code writes no `projects/*.jsonl` transcript until the
            // Goinkyo's first turn. Left as a plain `missing`, this member
            // is permanently stuck: `session_id` never gets a chance to
            // clear on its own, so `gather_goinkyo_observation`'s
            // `AwaitingSpawn` check (which only fires on `session_id: None`)
            // never sees this row as spawnable again — a member the
            // operator can't restart short of dismissing it outright.
            None if *role == BrigadeRole::Goinkyo && session_id.is_some() => {
                let key = SessionKey::from_id(session_id.as_deref().unwrap());
                state.member_tokens.insert(key.clone(), token.clone());
                state.member_brigades.insert(key.clone(), brigade_id);
                if state.screens.contains_key(&key) {
                    // The discovery window: `Cmd::RekeyPty` already renamed
                    // this pane's key to the real id (`update_discovery_
                    // result`), so it's alive right now — `resolved_row`
                    // above came back `None` only because `app`'s own row
                    // list hasn't caught up with the not-yet-written
                    // transcript, not because the pane is gone; this Goinkyo
                    // isn't stranded at all, just momentarily unresolved.
                    // Reusing it here, the same way the `Some(row)` arm's
                    // own `screens` check does just above, is what keeps
                    // this from being treated as the true stranded case
                    // below: clearing `session_id` on a live pane would
                    // sever it from the row identifying it mid-consultation
                    // — its own `_mcp` connection resolves who's calling
                    // through `Store::brigade_of_session`, keyed on this
                    // same id, so `send_to_peer`/`check_messages` would stop
                    // working for it the moment the id went missing.
                    //
                    // Also seeded here for the same reason the `Some(row)`
                    // arm's own resume seeds it: after a banto restart,
                    // `goinkyo_pane` starts empty, so reusing an
                    // already-live pane through *this* arm needs to record
                    // it too — not just a resume through the other one — or
                    // the guard stays unset for a Goinkyo this build never
                    // saw spawn.
                    state.goinkyo_pane.insert(brigade_id, key.clone());
                    panes.push(key);
                } else {
                    // Truly stranded, not a timing window: no live pane is
                    // claiming this id, and none ever will — a Goinkyo that
                    // never ran a turn writes no transcript, ever, not just
                    // on this tick. Reset `session_id` so the next tick's
                    // `AwaitingSpawn` restarts the very same consultation
                    // (its request file outlives this — kept until
                    // dismissal, see `StoreIntent::ClearMemberSession`'s own
                    // doc) — the Goinkyo analog of a Worker's disposable,
                    // respawnable design, through this same spawn path
                    // rather than a parallel one.
                    cmds.push(Cmd::Store(StoreIntent::ClearMemberSession {
                        brigade_id,
                        token: token.clone(),
                    }));
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
        director,
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
/// Director's own row cwd, resolved from `app` via `Stage::Brigade::director`
/// — no extra round trip needed as long as that's `Some` and the row is
/// already known; `None` (either one) falls back to `"."`, same as an
/// unresolved Director always has.
fn add_worker(state: &mut EmporiumState, app: &mut App, brigade: &BrigadeConfig) -> Vec<Cmd> {
    let Stage::Brigade { id, director, .. } = &state.stage else {
        state.status = Some("no brigade staged — press B to start one".to_string());
        return Vec::new();
    };
    let brigade_id = *id;
    let director_row = director
        .as_ref()
        .and_then(|key| app.row_for_id(key.as_str()));
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
    // A drag in progress owns every mouse event until it ends: nothing is
    // forwarded to a child, focused or scrolled, however many panes the
    // pointer crosses.
    if state.drag.is_some() {
        return update_drag_mouse(state, mouse);
    }
    if state.move_drag.is_some() {
        return update_move_mouse(state, mouse);
    }
    let pos = Position::new(mouse.column, mouse.row);
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    let areas = geometry.areas;

    // A left press on a title-row button does what the button says at once:
    // nothing is forwarded, nothing scrolls, no gesture starts.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(button) = geometry.button_at(mouse.column, mouse.row)
    {
        let (key, kind) = (button.key.clone(), button.kind);
        match kind {
            ButtonKind::Solo => toggle_solo(state, &key),
            ButtonKind::Minimize => toggle_minimize(state, &key),
        }
        refocus_expanded(state, Some(&key));
        return Vec::new();
    }

    // A left press on a boundary picks it up — and does nothing else: no
    // focus change, no report to a child, no scrolling.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(handle) = geometry.handle_at(mouse.column, mouse.row)
    {
        let along = match handle.axis {
            Axis::Vertical => mouse.column,
            Axis::Horizontal => mouse.row,
        };
        state.drag = Some(Drag {
            boundary: handle.boundary.clone(),
            grab_offset: i32::from(handle.position) - i32::from(along),
            preview: handle.position,
            all_collapsed: geometry.all_collapsed,
        });
        return Vec::new();
    }

    if areas.pane.contains(pos) {
        let tiles = &geometry.tiles;
        let hit = tiles.iter().find(|(_, rect)| rect.contains(pos)).cloned();
        // The space under a column of strips holds nothing: a press, a wheel
        // turn or a drag there does nothing at all.
        if hit.is_none() && !tiles.is_empty() {
            return Vec::new();
        }
        // A collapsed tile is not a place to click: its body and border do
        // nothing (no focus, no report to the child, no scrolling). Its label
        // still takes hold of it to move it, without focusing it; its buttons
        // were handled above.
        if let Some((key, _)) = &hit
            && geometry.is_collapsed(key)
        {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
                && geometry.label_at(mouse.column, mouse.row) == Some(key)
                && state.member_tokens.contains_key(key)
            {
                state.move_drag = Some(MoveDrag {
                    key: key.clone(),
                    column: mouse.column,
                    row: mouse.row,
                    dragged: false,
                });
            }
            return Vec::new();
        }
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            state.focus = Focus::Pane;
            if let Stage::Brigade { panes, focused, .. } = &mut state.stage
                && let Some((key, _)) = &hit
                && let Some(p) = panes.iter().position(|k| k == key)
            {
                *focused = p;
            }
            // The press also takes hold of a stack tile by its title label.
            // The focus it just gave is the same as ever; a tile with no
            // member token has no place in an order preference, so it is not
            // picked up.
            if let Some(key) = geometry.label_at(mouse.column, mouse.row)
                && state.member_tokens.contains_key(key)
            {
                state.move_drag = Some(MoveDrag {
                    key: key.clone(),
                    column: mouse.column,
                    row: mouse.row,
                    dragged: false,
                });
                return Vec::new();
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

/// `text` placed with its first cell at (`x`, `y`) and cut to what fits in
/// `area`, by display width; `None` when nothing of it fits.
fn clip_to_screen(text: String, x: u16, y: u16, area: Rect) -> Option<Ghost> {
    if !area.contains(Position::new(x, y)) {
        return None;
    }
    let room = usize::from(area.right() - x);
    let mut clipped = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cell > room {
            break;
        }
        width += cell;
        clipped.push(ch);
    }
    (!clipped.is_empty()).then_some(Ghost {
        text: clipped,
        x,
        y,
    })
}

/// One mouse event while a tile is being moved by its title label.
///
/// Owned exactly like a boundary drag (see [`update_drag_mouse`]): nothing is
/// forwarded to a child, focused or scrolled however many panes the pointer
/// crosses, a press of any button or a `Moved` cancels, the stack changes
/// only on the release. Where the tile would land is decided by the
/// pointer's row alone — its column may wander over the master or the
/// sidebar.
fn update_move_mouse(state: &mut EmporiumState, mouse: MouseEvent) -> Vec<Cmd> {
    let Some(moving) = state.move_drag.clone() else {
        return Vec::new();
    };
    let slot = state
        .geometry(Rect::new(0, 0, state.size.0, state.size.1))
        .drop_slot(mouse.row);
    match mouse.kind {
        MouseEventKind::Down(_) | MouseEventKind::Moved => state.move_drag = None,
        MouseEventKind::Drag(MouseButton::Left) => match slot {
            Some(_) => {
                state.move_drag = Some(MoveDrag {
                    column: mouse.column,
                    row: mouse.row,
                    dragged: true,
                    ..moving
                })
            }
            None => state.move_drag = None,
        },
        MouseEventKind::Up(MouseButton::Left) => {
            state.move_drag = None;
            if let Some(target) = slot {
                move_tile(state, &moving.key, target);
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Bring a brigade's stored arrangement into state before its stage is built,
/// so the very first geometry — the first PTY sizes — already uses it.
///
/// In-memory wins: a brigade this run already holds a layout for keeps it. Every
/// change is written as it happens, so what memory holds is never older than
/// the store, and a stale read must not undo a change made since.
fn adopt_stored_layout(
    state: &mut EmporiumState,
    brigade_id: BrigadeId,
    stored: Option<&StoredLayout>,
) {
    let Some(stored) = stored else {
        return;
    };
    if state.brigade_layouts.contains_key(&brigade_id) {
        return;
    }
    let layout = BrigadeLayout::from_stored(stored);
    state.persisted_layouts.insert(brigade_id, layout.clone());
    state.brigade_layouts.insert(brigade_id, layout);
}

/// The writes that keep the store matching what the operator has arranged:
/// one `SaveBrigadeLayout` for each brigade whose layout differs from what the
/// store last held, one `SaveSidebarWidth` if the width does. Run at the end
/// of every `update`; since it only compares, a draw-only or tick-only event
/// writes nothing, and neither does a gesture that committed nothing, a
/// refused key, or a "restore all" with nothing to restore.
fn persist_preferences(state: &mut EmporiumState) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    if state.sidebar_width != state.persisted_sidebar_width {
        state.persisted_sidebar_width = state.sidebar_width;
        cmds.push(Cmd::Store(StoreIntent::SaveSidebarWidth {
            width: state.sidebar_width,
        }));
    }
    let mut brigades: Vec<BrigadeId> = state.brigade_layouts.keys().copied().collect();
    brigades.sort_unstable();
    for brigade_id in brigades {
        let layout = &state.brigade_layouts[&brigade_id];
        let previous = state.persisted_layouts.get(&brigade_id);
        let unchanged = match previous {
            Some(previous) => previous == layout,
            None => *layout == BrigadeLayout::default(),
        };
        if unchanged {
            continue;
        }
        let snapshot = layout.to_stored(previous);
        state.persisted_layouts.insert(brigade_id, layout.clone());
        cmds.push(Cmd::Store(StoreIntent::SaveBrigadeLayout {
            brigade_id,
            layout: snapshot,
        }));
    }
    cmds
}

/// Move the stack tile `key` to slot `to` (an index into `panes`), the tiles
/// in between shifting by one, by recording the result as the brigade's
/// preferred order: the staged stack tiles in their new order, then any
/// earlier-ranked members that are not staged right now, in their old
/// relative order, so a member that is off-stage keeps its place. The panes
/// themselves are reordered by `apply_stack_order` at the end of the same
/// `update`. Row weights are keyed by token, so a tile takes its height
/// with it and nothing is rewritten here.
fn move_tile(state: &mut EmporiumState, key: &SessionKey, to: usize) {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        return;
    };
    let Some(from) = panes.iter().position(|pane| pane == key) else {
        return;
    };
    if from == 0 || to == 0 || to >= panes.len() || from == to {
        return;
    }
    let mut stack: Vec<&SessionKey> = panes[1..].iter().collect();
    let moved = stack.remove(from - 1);
    stack.insert(to - 1, moved);
    let staged: Vec<MemberToken> = stack
        .into_iter()
        .filter_map(|pane| state.member_tokens.get(pane).cloned())
        .collect();
    let layout = state.brigade_layouts.entry(*id).or_default();
    let off_stage: Vec<MemberToken> = layout
        .stack_order
        .iter()
        .filter(|token| !staged.contains(token))
        .cloned()
        .collect();
    layout.stack_order = staged.into_iter().chain(off_stage).collect();
}

/// The staged brigade's id and the member token of the stack tile `key`, when
/// `key` is a stack tile that has a token to keep preferences under.
fn stack_tile_member(state: &EmporiumState, key: &SessionKey) -> Option<(BrigadeId, MemberToken)> {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        return None;
    };
    if !panes.get(1..).unwrap_or_default().contains(key) {
        return None;
    }
    Some((*id, state.member_tokens.get(key)?.clone()))
}

/// Collapse the stack tile `key` to a strip, or open it again.
///
/// While a solo is active this ends it first, and the tile ends up as the
/// click asked: a tile shown collapsed is opened (taken out of `minimized`),
/// the soloed tile itself is collapsed (put into it); the other tiles fall
/// back to their own flags. Without a solo it flips the tile's membership.
fn toggle_minimize(state: &mut EmporiumState, key: &SessionKey) {
    let Some((brigade_id, token)) = stack_tile_member(state, key) else {
        return;
    };
    // A gesture held across a change of what is collapsed was measured
    // against a layout that no longer exists.
    state.cancel_gestures();
    let solo_active = state.soloed_key().is_some();
    let shown_collapsed = match &state.stage {
        Stage::Brigade { panes, .. } => panes
            .get(1..)
            .unwrap_or_default()
            .iter()
            .position(|pane| pane == key)
            .and_then(|at| state.stack_collapsed().get(at).copied())
            .unwrap_or(false),
        _ => false,
    };
    let layout = state.brigade_layouts.entry(brigade_id).or_default();
    if solo_active {
        layout.solo_member = None;
        if shown_collapsed {
            layout.minimized.remove(&token);
        } else {
            layout.minimized.insert(token);
        }
    } else if !layout.minimized.remove(&token) {
        layout.minimized.insert(token);
    }
}

/// The staged brigade's id and the member token of the tile `key`, master or
/// stack, when the brigade has a stack at all (a lone pane has nothing to
/// solo against) and `key` has a token to keep the preference under.
fn soloable_member(state: &EmporiumState, key: &SessionKey) -> Option<(BrigadeId, MemberToken)> {
    let Stage::Brigade { id, panes, .. } = &state.stage else {
        return None;
    };
    if panes.len() < 2 || !panes.contains(key) {
        return None;
    }
    Some((*id, state.member_tokens.get(key)?.clone()))
}

/// Solo the tile `key` (the master or a stack tile), or turn its solo off when
/// it already is the one. Moving the solo from another tile just names the
/// new one. Never touches `minimized`.
fn toggle_solo(state: &mut EmporiumState, key: &SessionKey) {
    let Some((brigade_id, token)) = soloable_member(state, key) else {
        return;
    };
    state.cancel_gestures();
    let layout = state.brigade_layouts.entry(brigade_id).or_default();
    layout.solo_member = if layout.solo_member.as_ref() == Some(&token) {
        None
    } else {
        Some(token)
    };
}

/// Open every collapsed tile and turn the solo off.
fn restore_all(state: &mut EmporiumState) {
    let Stage::Brigade { id, .. } = &state.stage else {
        return;
    };
    let id = *id;
    state.cancel_gestures();
    let layout = state.brigade_layouts.entry(id).or_default();
    layout.solo_member = None;
    layout.minimized.clear();
    refocus_expanded(state, None);
}

/// A collapsed tile is not focusable. If the focused pane is collapsed, move
/// the focus to the tile just acted on if that is expanded, else to the
/// nearest expanded stack tile (below first, then above), else to the master.
/// Run after every operation and at the end of every `update`, so nothing
/// that collapses a tile — a solo returning with its pane — has to remember.
fn refocus_expanded(state: &mut EmporiumState, acted: Option<&SessionKey>) {
    let collapsed = state.pane_collapsed();
    let Stage::Brigade { panes, focused, .. } = &mut state.stage else {
        return;
    };
    if !collapsed.get(*focused).copied().unwrap_or(false) {
        return;
    }
    let expanded = |index: &usize| !collapsed.get(*index).copied().unwrap_or(false);
    let acted = acted
        .and_then(|key| panes.iter().position(|pane| pane == key))
        .filter(expanded);
    *focused = acted
        .or_else(|| (*focused + 1..panes.len()).find(expanded))
        .or_else(|| (1..*focused).rev().find(expanded))
        .unwrap_or(0);
}

/// Prefix + `m`: collapse the focused tile, or open it again.
fn collapse_focused_tile(state: &mut EmporiumState) -> Vec<Cmd> {
    let refusal = match (&state.stage, state.focus) {
        (_, Focus::Sidebar) => Some("prefix: focus a tile to collapse it"),
        (Stage::Brigade { focused: 0, .. } | Stage::Solo(_) | Stage::Empty, _) => {
            Some("prefix: the master tile can't be collapsed")
        }
        _ => None,
    };
    let key = state.stage.focused_key().cloned();
    let refusal = refusal.or_else(|| {
        key.as_ref()
            .is_none_or(|key| stack_tile_member(state, key).is_none())
            .then_some("prefix: this tile has no member to collapse")
    });
    if let Some(reason) = refusal {
        state.status = Some(reason.to_string());
        return Vec::new();
    }
    if let Some(key) = key {
        toggle_minimize(state, &key);
        refocus_expanded(state, Some(&key));
    }
    Vec::new()
}

/// Prefix + `z`: solo the focused tile — the master included — or turn its
/// solo off.
fn solo_focused_tile(state: &mut EmporiumState) -> Vec<Cmd> {
    if state.focus == Focus::Sidebar {
        state.status = Some("prefix: focus a tile to solo it".to_string());
        return Vec::new();
    }
    let Some(key) = state.stage.focused_key().cloned() else {
        return Vec::new();
    };
    if soloable_member(state, &key).is_none() {
        let alone = !matches!(&state.stage, Stage::Brigade { panes, .. } if panes.len() >= 2);
        state.status = Some(
            if alone {
                "prefix: no stack to solo"
            } else {
                "prefix: this tile has no member to solo"
            }
            .to_string(),
        );
        return Vec::new();
    }
    toggle_solo(state, &key);
    refocus_expanded(state, Some(&key));
    Vec::new()
}

/// Prefix + Shift+Up / Shift+Down: move the focused stack tile one slot, the
/// focus staying on it. Every refusal says why, the way an out-of-range
/// pane number does.
fn move_focused_tile(state: &mut EmporiumState, direction: Direction) -> Vec<Cmd> {
    let refusal = match (&state.stage, state.focus) {
        (_, Focus::Sidebar) => Some("prefix: focus a tile to move it"),
        (Stage::Brigade { focused: 0, .. } | Stage::Solo(_) | Stage::Empty, _) => {
            Some("prefix: the master tile stays put")
        }
        (Stage::Brigade { focused, .. }, _) if direction == Direction::Up && *focused <= 1 => {
            Some("prefix: already at the top of the stack")
        }
        (Stage::Brigade { focused, panes, .. }, _)
            if direction == Direction::Down && focused + 1 >= panes.len() =>
        {
            Some("prefix: already at the bottom of the stack")
        }
        (Stage::Brigade { focused, panes, .. }, _)
            if panes
                .get(*focused)
                .is_none_or(|key| !state.member_tokens.contains_key(key)) =>
        {
            Some("prefix: this tile has no member to keep an order for")
        }
        _ => None,
    };
    if let Some(reason) = refusal {
        state.status = Some(reason.to_string());
        return Vec::new();
    }
    if let Stage::Brigade { focused, panes, .. } = &state.stage {
        let to = if direction == Direction::Up {
            *focused - 1
        } else {
            *focused + 1
        };
        let key = panes[*focused].clone();
        move_tile(state, &key, to);
    }
    Vec::new()
}

/// The boundary position a pointer at `mouse` previews, clamped to what that
/// boundary may take. `None` when the boundary no longer exists.
fn dragged_position(state: &EmporiumState, drag: &Drag, mouse: &MouseEvent) -> Option<u16> {
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    let handle = geometry.handle_for(&drag.boundary)?;
    let along = match handle.axis {
        Axis::Vertical => mouse.column,
        Axis::Horizontal => mouse.row,
    };
    Some(handle.clamp(i32::from(along) + drag.grab_offset))
}

/// One mouse event while a boundary drag is in progress.
///
/// The real geometry does not move during the drag — only `Drag::preview`
/// does — and the layout changes once, on release. Every layout change is a
/// `Cmd::ResizePty` per tile whose size changed, executed immediately and
/// without debounce by the shell, and each makes the child repaint its whole
/// screen; resize under stress is on the embedded-pty spike's own
/// not-yet-verified list (`docs/notes/embedded-pty-spike.md`). One resize
/// per gesture is the path a window resize already exercises; one per
/// dragged cell is the untested one.
///
/// A press of any button, or a `Moved` (which can only mean no button is held
/// any more: the release happened outside the window), ends the gesture
/// without applying it. Everything else that is not the left button's drag or
/// release — the wheel, the other buttons — is swallowed.
fn update_drag_mouse(state: &mut EmporiumState, mouse: MouseEvent) -> Vec<Cmd> {
    let Some(drag) = state.drag.clone() else {
        return Vec::new();
    };
    match mouse.kind {
        MouseEventKind::Down(_) | MouseEventKind::Moved => state.drag = None,
        MouseEventKind::Drag(MouseButton::Left) => match dragged_position(state, &drag, &mouse) {
            Some(preview) => state.drag = Some(Drag { preview, ..drag }),
            None => state.drag = None,
        },
        MouseEventKind::Up(MouseButton::Left) => {
            state.drag = None;
            if let Some(position) = dragged_position(state, &drag, &mouse) {
                commit_drag(state, &drag.boundary, position);
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Apply a finished drag: store the new preference for `boundary`. The tiles
/// whose content size changed are resized by the `resize_staged_tiles` at the
/// end of this same `update`.
///
/// A release where the boundary already was stores nothing: a click on a
/// handle must not turn a proportional layout into frozen row counts.
fn commit_drag(state: &mut EmporiumState, boundary: &Boundary, position: u16) {
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    let Some(handle) = geometry.handle_for(boundary) else {
        return;
    };
    if position == handle.position {
        return;
    }
    match boundary {
        // The sidebar starts at column 0, so its width is where the pane
        // area now begins.
        Boundary::Sidebar => state.sidebar_width = position,
        Boundary::Master { brigade } => {
            let pane = geometry.areas.pane;
            let layout = state.brigade_layouts.entry(*brigade).or_default();
            if geometry.all_collapsed {
                // The column of strips has a width of its own: where the
                // boundary sits is how much room the stack gets.
                layout.collapsed_stack_width = pane.x + pane.width - position;
            } else {
                layout.master_permille = master_permille_for(position - pane.x, pane.width);
            }
        }
        Boundary::Stack {
            brigade,
            above,
            below,
        } => {
            // Every *expanded* stack member's weight becomes the height it ends
            // up with, so a later window resize scales the whole expanded
            // stack as dragged rather than only the two tiles that moved.
            // A collapsed member keeps the weight it has: its strip height is
            // not a height it chose.
            let mut heights: Vec<(SessionKey, u16)> = geometry
                .tiles
                .iter()
                .zip(&geometry.collapsed)
                .skip(1)
                .filter(|(_, collapsed)| !**collapsed)
                .map(|((key, rect), _)| (key.clone(), rect.height))
                .collect();
            let moved = heights.iter().position(|(key, _)| key == above);
            if let Some(i) = moved
                && heights.get(i + 1).is_some_and(|(key, _)| key == below)
                && let Some(above_tile) = geometry.tiles.iter().find(|(key, _)| key == above)
            {
                let together = heights[i].1 + heights[i + 1].1;
                let upper = position.saturating_sub(above_tile.1.y);
                heights[i].1 = upper;
                heights[i + 1].1 = together.saturating_sub(upper);
            }
            let layout = state.brigade_layouts.entry(*brigade).or_default();
            for (key, height) in heights {
                if let Some(token) = state.member_tokens.get(&key) {
                    layout.row_weights.insert(token.clone(), height.max(1));
                }
            }
        }
    }
}

/// Drop a drag whose gesture can no longer be completed: a modal opened over
/// it, or the boundary it holds is not in the current geometry (the stage
/// changed — a pane closed, another brigade staged). Run at the end of every
/// `update`, so no event handler has to remember to cancel it.
fn validate_drag(state: &mut EmporiumState, app: &App) {
    if let Some(moving) = &state.move_drag {
        // A tile being moved must still be a stack tile, in a stack that has
        // somewhere to put it.
        let still_movable = matches!(
            &state.stage,
            Stage::Brigade { panes, .. }
                if panes.len() >= 3 && panes[1..].contains(&moving.key)
        );
        if app.modal().is_some() || !still_movable {
            state.move_drag = None;
        }
    }
    let Some(drag) = &state.drag else {
        return;
    };
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    // The master/stack boundary is the same boundary whether or not the stack
    // is all strips, but it means a different preference in each; a pane
    // arriving or leaving can flip that under a held button.
    let meaning_changed = matches!(drag.boundary, Boundary::Master { .. })
        && drag.all_collapsed != geometry.all_collapsed;
    if app.modal().is_some() || geometry.handle_for(&drag.boundary).is_none() || meaning_changed {
        state.drag = None;
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
        state.note_forwarded_input(&key, ForwardedInput::Paste(&text));
        cancel_pending_submit_on_input(&mut state.pending_submits, &key);
        return vec![Cmd::WritePty { key, bytes }];
    }
    Vec::new()
}

fn update_spawned(
    state: &mut EmporiumState,
    brigade: &BrigadeConfig,
    key: SessionKey,
    now: Instant,
) -> Vec<Cmd> {
    // A kill that never produced an exit — the child was already gone — would
    // otherwise leave its record behind to suppress the first genuine crash
    // of whatever opens under this key next.
    state.expected_exits.remove(&key);
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
            state
                .member_tokens
                .insert(key.clone(), DIRECTOR_TOKEN.to_string());
            state.member_brigades.insert(key.clone(), brigade_id);
            state.stage = Stage::Brigade {
                id: brigade_id,
                director: Some(key.clone()),
                panes: vec![key],
                focused: 0,
            };
            state.focus = Focus::Pane;
            worker_tokens
                .into_iter()
                .flat_map(|token| {
                    open_worker(
                        state,
                        brigade_id,
                        &token,
                        &cwd,
                        &worker_model,
                        brigade.worker_permission_mode(),
                        worker_agent,
                    )
                })
                .collect()
        }
        PendingOpen::BrigadeMember {
            brigade_id,
            needs_codex_kickoff,
            cwd,
        } => {
            if let Stage::Brigade {
                id,
                director,
                panes,
                ..
            } = &mut state.stage
                && *id == brigade_id
                && !panes.contains(&key)
            {
                // Best-effort Director-first ordering (display only — see
                // `Stage::Brigade::panes`'s own doc): if this arrival is
                // the Director's own key, put it back at the front rather
                // than wherever it landed in arrival order. Where everyone
                // else belongs is not decided here: `apply_stack_order`
                // places them at the end of this same `update`.
                if director.as_ref() == Some(&key) {
                    panes.insert(0, key.clone());
                } else {
                    panes.push(key.clone());
                }
            }
            if needs_codex_kickoff {
                state.pending_kickoffs.push(PendingKickoff {
                    key,
                    spawned_at: now,
                    cwd,
                    notified_untrusted: false,
                    sent_at: None,
                    attempts: 0,
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
    permission_mode: Option<&str>,
    agent: AgentKind,
) -> Vec<Cmd> {
    let key = SessionKey::new_worker(brigade_id, token);
    state.member_tokens.insert(key.clone(), token.to_string());
    state.member_brigades.insert(key.clone(), brigade_id);
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
        effort: None,
        permission_mode: permission_mode.map(str::to_string),
        disallowed_tools: None,
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

/// What [`relaunch_member`] decided about an exit it recognised.
enum ExitHandling {
    /// Put the member back: the tile stays, this rebuilds what was under it.
    /// Boxed because a `Cmd` is far larger than the other answer, and clippy
    /// is right that an enum should not cost its largest variant everywhere.
    Relaunch(Box<Cmd>),
    /// A member that would have been restarted, except that it was restarted
    /// a moment ago and is evidently not able to stay up. Handled as an
    /// ordinary exit from here, but the operator is told which kind it was.
    GaveUp,
}

/// A living Worker's pane ended without banto asking, so put it back.
///
/// The case this exists for: an agent CLI updates itself, its process is
/// replaced, and the pane goes with it. Everything banto persists about the
/// member survives that — the store row, its token, its session id — so what
/// was lost is only the pane, and re-opening the whole brigade is a heavy way
/// to get one back. `Some(cmd)` means the tile stays where it is and its
/// screen is rebuilt when the spawn lands (`update_spawned` already tolerates
/// a key that is still staged); `None` means this exit is handled the way
/// every exit was before.
///
/// Five things have to hold, and each of them is a way this could resurrect
/// something that was meant to stay dead:
///
/// - **banto did not ask for the exit** ([`kill_pane`]). This is what keeps a
///   plain `x` kill dead: the operator ending a Worker's pane is the one
///   deliberate exit that changes nothing else observable, and without the
///   record it looks exactly like a process that fell over.
/// - **The pane is still staged.** Dismissing, disbanding and both Goinkyo
///   paths all un-stage before they kill, so anything already gone from the
///   stage was on its way out for a reason this function cannot see.
/// - **It is a Worker.** The Director's own pane has two reopen paths of its
///   own that run before this, and a Goinkyo's absence is answered by
///   `gather_goinkyo_observation` against the store rather than here.
/// - **Its session resolves to a row**, because this restarts a session
///   rather than starting one. A Worker still awaiting discovery has nothing
///   to resume; it keeps the old behaviour, and staging respawns it fresh.
/// - **It was not restarted a moment ago** ([`RELAUNCH_COOLDOWN`]).
fn relaunch_member(
    state: &mut EmporiumState,
    app: &App,
    brigade: &BrigadeConfig,
    key: &SessionKey,
    banto_asked: bool,
    now: Instant,
) -> Option<ExitHandling> {
    if banto_asked {
        return None;
    }
    if !state.screens.contains_key(key) {
        return None;
    }
    let brigade_id = *state.member_brigades.get(key)?;
    let token = state.member_tokens.get(key)?.clone();
    if token == DIRECTOR_TOKEN
        || token == GOINKYO_TOKEN
        || state.goinkyo_pane.values().any(|pane| pane == key)
    {
        return None;
    }
    let row = app.row_for_id(key.as_str())?;
    if state
        .last_relaunch
        .get(key)
        .is_some_and(|at| now.saturating_duration_since(*at) < RELAUNCH_COOLDOWN)
    {
        return Some(ExitHandling::GaveUp);
    }
    state.last_relaunch.insert(key.clone(), now);
    state.screens.remove(key);
    // The process that held any draft is gone; the restarted one starts with
    // an empty input line.
    state.unsent_operator_input.remove(key);
    let cwd = row.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
    state.pending_opens.insert(
        key.clone(),
        PendingOpen::BrigadeMember {
            brigade_id,
            // Resuming a session that already exists, exactly as a re-stage
            // does — the kickoff only ever belongs to a Codex Worker being
            // started for the first time.
            needs_codex_kickoff: false,
            cwd: cwd.clone(),
        },
    );
    let launch = RoleLaunch::of(BrigadeRole::Worker, row.agent, brigade);
    state.set_status(format!("worker {token} exited — restarting"), now);
    Some(ExitHandling::Relaunch(Box::new(Cmd::OpenEmbedded {
        key: key.clone(),
        target: SessionToOpen {
            id: row.id.clone(),
            agent: row.agent,
            title: row.display_title().to_string(),
            cwd,
        },
        brigade: Some((brigade_id, token, BrigadeRole::Worker)),
        model: launch.model,
        effort: launch.effort,
        permission_mode: launch.permission_mode,
        disallowed_tools: launch.disallowed_tools,
    })))
}

/// `D`: take the selected session back out of banto's archive. No confirm —
/// see the chōba's own `unarchive_selected` for why burying is guarded and
/// restoring is not.
fn unarchive_selected(state: &mut EmporiumState, app: &mut App, now: Instant) -> Vec<Cmd> {
    let Some(row) = app.selected_row() else {
        return Vec::new();
    };
    if !row.archived {
        state.set_status(
            "that session is not archived — press `a` until archived sessions show",
            now,
        );
        return Vec::new();
    }
    let id = row.id.clone();
    let title = row.display_title().to_string();
    vec![Cmd::Store(StoreIntent::Unarchive { id, title })]
}

fn update_pty_exited(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: &BrigadeConfig,
    key: SessionKey,
    reason: Option<PtyExitReason>,
    now: Instant,
) -> Vec<Cmd> {
    let banto_asked = state.expected_exits.remove(&key);
    let gave_up = match relaunch_member(state, app, brigade, &key, banto_asked, now) {
        Some(ExitHandling::Relaunch(cmd)) => return vec![*cmd],
        Some(ExitHandling::GaveUp) => true,
        None => false,
    };
    state.screens.remove(&key);
    state.unstage(&key);
    // A Goinkyo pane can close this way without ever being dismissed (the
    // operator plain-kills it, or its process just dies) — `goinkyo_pane`'s
    // own doc: still discoverable, so a later re-stage reopens it the same
    // way a killed Worker's pane does. Dropping the entry here is state
    // hygiene, not correctness (a stale entry only means a future
    // `NoGoinkyo` tries to unstage/kill an already-gone key, both harmless
    // no-ops), but leaving it around would be one more stale-value question
    // a future reader has to work through.
    state.goinkyo_pane.retain(|_, pane| *pane != key);
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
    let mut status = match reason {
        Some(reason) => format!("session ended: {title} — {}", reason.describe()),
        None => format!("session ended: {title}"),
    };
    if gave_up {
        // The one exit that would otherwise read as an ordinary one while
        // meaning something else entirely: banto tried this member a moment
        // ago and is declining to try again.
        status.push_str(" (restarted once already — left closed)");
    }
    state.set_status(status, now);
    Vec::new()
}

/// `Cmd::WritePty`'s target had no live handle when the shell went to write
/// it — `execute_cmd`'s own `Cmd::WritePty` arm in `emporium.rs` reports
/// this instead of the silent no-op it used to be. Deliberately not
/// filtered by which `Cmd::WritePty` producer this traces back to: every
/// direct keystroke-forwarding path (`Focus::Pane`'s own key handling,
/// `PrefixAction::Literal`, mouse SGR forwarding, paste) reads its target
/// key fresh out of `state.stage`/`state.screens` on the same `update` call
/// that emits the `Cmd` — the same state `Cmd::RekeyPty` keeps in lockstep
/// with the shell's own handle map — so none of those can name a key with
/// no live handle. Only a *queued* write can: one stashed under a key at
/// one tick (a Goinkyo/Codex kickoff's own `PendingGoinkyoKickoff`/
/// `PendingKickoff`, a relay nudge's `PendingSubmit`) and executed at a
/// later one, after that key might have been renamed or the pane closed in
/// between. This is exactly the shape of the bug `update_discovery_result`/
/// `update_member_session_forked`'s `pending_goinkyo_kickoffs` follow now
/// fixes for the Goinkyo kickoff specifically — this status line is the
/// generic backstop for that whole class, Goinkyo or otherwise, so a future
/// instance of it is never silent again. Never fires on an ordinary
/// keystroke, so this cannot become a per-keypress status flicker.
fn update_pty_write_dropped(state: &mut EmporiumState, key: SessionKey, now: Instant) -> Vec<Cmd> {
    state.set_status(
        format!(
            "internal: a write to {} found no live pane — this is a bug",
            key.as_str()
        ),
        now,
    );
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
    if state.rekey_pane_state(&old_key, &new_key) {
        cmds.push(Cmd::RekeyPty {
            from: old_key.clone(),
            to: new_key.clone(),
        });
    }
    // A discovered Codex Worker session is the acknowledgement that its
    // first turn started. It must win over a timeout retry in a later tick.
    state
        .pending_kickoffs
        .retain(|pending| pending.key != new_key);
    if let Some((brigade_id, token)) = member {
        state
            .member_tokens
            .entry(new_key.clone())
            .or_insert_with(|| token.clone());
        state
            .member_brigades
            .entry(new_key.clone())
            .or_insert(brigade_id);
        cmds.push(Cmd::Store(StoreIntent::SetMemberSession {
            brigade_id,
            token,
            session_id,
        }));
    }
    cmds
}

/// A live member's Claude session forked in place (see
/// [`Event::MemberSessionForked`]'s doc for why). The store write always
/// happens — the fork is real regardless of what the emporium's own panes
/// look like right now — but the pane/screen rename applies whenever the
/// pane is still alive, including while another brigade is displayed.
/// `focused` needs no fixing of its own, since renaming `panes[i]`'s *value*
/// in place never moves what index it lives at. Idempotent under a repeated fact (the
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
    if state.screens.contains_key(&old_key) {
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
            state.rekey_pane_state(&old_key, &new_key);
            state
                .member_tokens
                .entry(new_key.clone())
                .or_insert_with(|| token.clone());
            state
                .member_brigades
                .entry(new_key.clone())
                .or_insert(brigade_id);
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
    // Joined in the order they were numbered, whichever pane spawns first.
    state.member_order.insert(
        brigade_id,
        std::iter::once(DIRECTOR_TOKEN.to_string())
            .chain(worker_tokens.iter().cloned())
            .collect(),
    );
    // The brigade is named from this moment, not from its first tick: the
    // Director is the row the operator pressed B on, and `name` is its title.
    state.brigade_names.insert(
        brigade_id,
        BrigadeNameFacts {
            brigade_id,
            name: name.clone(),
            director_session: Some(director_row_id.clone()),
        },
    );
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
        vec![kill_pane(state, director_key)]
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
        brigade: Some((
            brigade_id,
            DIRECTOR_TOKEN.to_string(),
            BrigadeRole::Director,
        )),
        model: None,
        effort: None,
        permission_mode: None,
        disallowed_tools: None,
    }]
}

/// Note that `token` joined `brigade_id` now, after everyone already listed in
/// its join order. Called where the membership is *created* as the core sees
/// it, so where a member's tile goes never depends on when its pane happens to
/// appear (`apply_stack_order` learns an unlisted token on sight only as a
/// last resort).
fn record_join(state: &mut EmporiumState, brigade_id: BrigadeId, token: &str) {
    let joined = state.member_order.entry(brigade_id).or_default();
    if !joined.iter().any(|listed| listed == token) {
        joined.push(token.to_string());
    }
}

/// `Event::WorkerAdded`'s successful-addition payload, bundled so
/// [`update_worker_added`] doesn't tip into clippy's too-many-arguments —
/// these four always travel together anyway, all the way from
/// `StoreIntent::AddWorker`.
struct AddedWorker {
    brigade_id: BrigadeId,
    cwd: PathBuf,
    worker_agent: AgentKind,
    worker_model: String,
}

fn update_worker_added(
    state: &mut EmporiumState,
    brigade: &BrigadeConfig,
    added: AddedWorker,
    result: Result<MemberToken, String>,
    now: Instant,
) -> Vec<Cmd> {
    let AddedWorker {
        brigade_id,
        cwd,
        worker_agent,
        worker_model,
    } = added;
    match result {
        Ok(token) => {
            state.set_status(format!("{token} added"), now);
            record_join(state, brigade_id, &token);
            open_worker(
                state,
                brigade_id,
                &token,
                &cwd,
                &worker_model,
                brigade.worker_permission_mode(),
                worker_agent,
            )
        }
        Err(err) => {
            state.set_status(format!("failed to add worker: {err}"), now);
            Vec::new()
        }
    }
}

/// On success while the disbanded brigade is staged: fall the stage back to
/// `Solo(director)` and kill every *other* staged pane — Workers and a
/// Goinkyo alike, whatever role they hold; disband tears down the whole
/// cell, not just Workers specifically. Requires knowing the Director for
/// certain: `Stage::Brigade::director` is `Some` *and* its own pane is
/// still actually staged (the ordinary case — belt-and-suspenders past the
/// `Some` alone, since kill is irreversible and this is cheap to check).
/// Anything short of that certainty — `director` is `None`, or points at a
/// pane that already isn't staged — kills nobody and drops straight to
/// `Stage::Empty` instead of guessing who's safe to spare: an operator who
/// disbands with the Director's own pane already closed gets a
/// no-op-on-the-kills-but-still-collapses-the-stage outcome, not a wrong
/// kill. Workers are banto-spawned creatures that die with their brigade;
/// the kills flow through the same passive `PtyExited` fold as any other
/// exit — a killed-but-unstaged member (already gone, or one this stage
/// never resolved) is simply skipped, nothing to kill.
fn update_disbanded(
    state: &mut EmporiumState,
    app: &mut App,
    brigade_id: BrigadeId,
    result: Result<(HashSet<String>, HashSet<String>), String>,
    now: Instant,
) -> Vec<Cmd> {
    match result {
        Ok((hidden, directors)) => {
            app.set_hidden_member_ids(hidden);
            app.set_directors(directors);
            let (director_key, other_keys) = if let Stage::Brigade {
                id,
                director,
                panes,
                ..
            } = &state.stage
                && *id == brigade_id
                && let Some(d) = director
                && panes.contains(d)
            {
                (
                    Some(d.clone()),
                    panes.iter().filter(|k| *k != d).cloned().collect(),
                )
            } else {
                (None, Vec::new())
            };
            state.stage = match &director_key {
                Some(key) => Stage::Solo(key.clone()),
                None => Stage::Empty,
            };
            state.brigade_layouts.remove(&brigade_id);
            state.persisted_layouts.remove(&brigade_id);
            state.brigade_names.remove(&brigade_id);
            state.member_order.remove(&brigade_id);
            state.member_facts.retain(|(id, _), _| *id != brigade_id);
            state.set_status("brigade disbanded".to_string(), now);
            other_keys
                .into_iter()
                .map(|key| kill_pane(state, key))
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
    // Only a result for the brigade the dismissal was captured in may spend
    // the captured member. Another brigade's result touches neither the
    // entry nor that brigade's weights: its token can be spelled the same
    // (`worker-2` exists in every brigade).
    let member = match &state.pending_dismiss_member {
        Some((captured, _)) if *captured == brigade_id => {
            state.pending_dismiss_member.take().map(|(_, token)| token)
        }
        _ => None,
    };
    match result {
        Ok((hidden, directors)) => {
            app.set_hidden_member_ids(hidden);
            app.set_directors(directors);
            // Forget everything kept under the dismissed member's token: its
            // layout preferences and what its tile shows. Only a dismissal
            // does this — a pane exiting, being relaunched or leaving the
            // stage keeps the member, and what is kept for it. (Tokens are
            // reused, so anything left would be inherited by the next Worker.)
            if let Some(token) = member {
                state.member_facts.remove(&(brigade_id, token.clone()));
                if let Some(joined) = state.member_order.get_mut(&brigade_id) {
                    joined.retain(|listed| *listed != token);
                }
                // The store forgets the member's row with its arrangement
                // (and the solo naming it), so the persisted copy forgets it
                // too: nothing is written for a dismissal.
                for layout in [
                    state.brigade_layouts.get_mut(&brigade_id),
                    state.persisted_layouts.get_mut(&brigade_id),
                ]
                .into_iter()
                .flatten()
                {
                    layout.row_weights.remove(&token);
                    layout.stack_order.retain(|ranked| *ranked != token);
                    layout.minimized.remove(&token);
                    if layout.solo_member.as_ref() == Some(&token) {
                        layout.solo_member = None;
                    }
                }
            }
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
                vec![kill_pane(state, key)]
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

/// The widest the quoted first line of a message may be in a status, in
/// display columns, ellipsis included.
const NUDGE_PROMPT_COLUMNS: usize = 60;

/// ` ← worker-3: first line of the message` for the nudge status — the sender
/// when it is known, the line cut by display width. Empty when the message has
/// no line to quote: never an arrow with nothing after it.
fn nudge_prompt(message: &MessageGlance) -> String {
    let Some(line) = display_name(&message.line) else {
        return String::new();
    };
    let line = shorten_to_columns(&line, NUDGE_PROMPT_COLUMNS);
    match &message.from_token {
        Some(sender) => format!(" \u{2190} {sender}: {line}"),
        None => format!(" \u{2190} {line}"),
    }
}

/// The widest a brigade's name is allowed to be in a one-line status, in
/// display columns, ellipsis included.
const BRIGADE_NAME_COLUMNS: usize = 28;

fn update_tick(
    state: &mut EmporiumState,
    app: &App,
    brigade: &BrigadeConfig,
    relay: Vec<RelayObservation>,
    brigades: Vec<BrigadeNameFacts>,
    now: Instant,
) -> Vec<Cmd> {
    let mut cmds = Vec::new();

    // First, so the statuses below name each brigade by what this very tick
    // read. A brigade the tick does not list keeps its last facts (a failed
    // gather reads nothing and must not blank every name).
    for facts in brigades {
        state.brigade_names.insert(facts.brigade_id, facts);
    }

    // What each observed member's collapsed tile shows. A member the shell did
    // not re-read (`glance: None`) keeps what it had; an agent the shell
    // could not resolve this tick does not erase one it could before.
    for observation in &relay {
        let facts = state
            .member_facts
            .entry((observation.brigade_id, observation.token.clone()))
            .or_default();
        if observation.agent.is_some() {
            facts.agent = observation.agent;
        }
        if let Some(MemberGlance { inbound, sent }) = &observation.glance {
            facts.inbound = inbound.clone();
            facts.sent = sent.clone();
        }
    }

    // Phase two of a nudge: send the delayed submitting `\r` in its own PTY
    // write. A chunk boundary can carry meaning for the embedded PTY, so
    // bundling it with the nudge text risks the child reading both as one
    // paste instead of text-then-submit (docs/REQUIREMENTS.md, "Auto-relay").
    let mut i = 0;
    while i < state.pending_submits.len() {
        if now.saturating_duration_since(state.pending_submits[i].nudged_at) >= RELAY_SUBMIT_DELAY {
            let entry = state.pending_submits.swap_remove(i);
            // A draft here sends together with the line just written. Input
            // typed after the nudge already cancels the submit
            // ([`cancel_pending_submit_on_input`]); this is for a draft that
            // was already there.
            if !state.has_unsent_operator_input(&entry.key) {
                cmds.push(Cmd::WritePty {
                    key: entry.key,
                    bytes: b"\r".to_vec(),
                });
            }
        } else {
            i += 1;
        }
    }

    // A Goinkyo can have been discovered before its first turn starts. Only
    // a row that appears after a write acknowledges that write.
    state.pending_goinkyo_kickoffs.retain(|pending| {
        pending.sent_at.is_none() || app.row_for_id(pending.key.as_str()).is_none()
    });

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
        let due = match pending.sent_at {
            Some(sent_at) => now.saturating_duration_since(sent_at) >= KICKOFF_ACK_TIMEOUT,
            None => now.saturating_duration_since(quiet_since) >= CODEX_KICKOFF_QUIET_PERIOD,
        };
        if due
            && pending.attempts < KICKOFF_MAX_ATTEMPTS
            && !state.has_unsent_operator_input(&pending.key)
        {
            cmds.push(Cmd::CheckWorkerDirectoryTrust {
                key: pending.key.clone(),
                cwd: pending.cwd.clone(),
            });
        }
    }

    // Goinkyo kickoff: same shape as the Codex Worker kickoff above, once
    // its own quiet period elapses — see `GOINKYO_KICKOFF_QUIET_PERIOD`'s
    // own doc for why this exists, and `Cmd::CheckGoinkyoDirectoryTrust`'s
    // for why quiet alone still isn't enough to type blind.
    for pending in &state.pending_goinkyo_kickoffs {
        let quiet_since = state
            .last_output_at
            .get(&pending.key)
            .copied()
            .unwrap_or(pending.spawned_at);
        let due = match pending.sent_at {
            Some(sent_at) => now.saturating_duration_since(sent_at) >= KICKOFF_ACK_TIMEOUT,
            None => now.saturating_duration_since(quiet_since) >= GOINKYO_KICKOFF_QUIET_PERIOD,
        };
        if due
            && pending.attempts < KICKOFF_MAX_ATTEMPTS
            && !state.has_unsent_operator_input(&pending.key)
        {
            cmds.push(Cmd::CheckGoinkyoDirectoryTrust {
                key: pending.key.clone(),
                cwd: pending.cwd.clone(),
            });
        }
    }

    // Members whose nudge only an unsent draft is holding back.
    let mut held_by_draft: Vec<(Option<BrigadeId>, MemberToken)> = Vec::new();
    // Idle Claude members whose input line has been unreadable long enough
    // to say so.
    let mut unreadable_due: Vec<(BrigadeId, MemberToken)> = Vec::new();
    if brigade.relay == RelayMode::Auto {
        let focused = state.stage.focused_key().cloned();
        for obs in relay {
            // Read at the moment of the decision and nowhere else: only a
            // Claude pane has a line this build can read, and a pane with no
            // screen yet reads as nothing.
            let reading = match (obs.agent, state.screens.get(&obs.key)) {
                (Some(AgentKind::ClaudeCode), Some(pane)) => Some(read_input_line(pane)),
                _ => None,
            };
            let last_forwarded = state.last_forwarded_input.get(&obs.key).copied();
            let (has_unsent, clear_mark) = unsent_verdict(
                reading.unwrap_or(InputLine::Unrecognized),
                state.has_unsent_operator_input(&obs.key),
                last_forwarded.map(|at| now.saturating_duration_since(at)),
            );
            if clear_mark {
                state.unsent_operator_input.remove(&obs.key);
            }
            // Waiting (a permission dialog) and busy are not idle, so an
            // unreadable screen there is ordinary and is not counted.
            let member = (obs.brigade_id, obs.token.clone());
            if reading == Some(InputLine::Unrecognized) && obs.is_idle_this_tick == Some(true) {
                let since = *state.unreadable_since.entry(member.clone()).or_insert(now);
                if now.saturating_duration_since(since) >= INPUT_LINE_UNREADABLE_AFTER
                    && !state.unreadable_noticed.contains(&member)
                {
                    unreadable_due.push(member);
                }
            } else {
                state.unreadable_since.remove(&member);
            }
            let input = PaneInput {
                is_focused: state.focus == Focus::Pane && focused.as_ref() == Some(&obs.key),
                last_forwarded,
                has_unsent,
            };
            let nudge = tick_relay_decision(
                &mut state.relay_states,
                obs.brigade_id,
                &obs.token,
                now,
                obs.is_idle_this_tick,
                input,
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
                let member =
                    state.members_phrase(app, &[(Some(obs.brigade_id), obs.token.clone())]);
                // What prompted it: the newest message to this member, as
                // applied from this very tick's observation above.
                let prompt = state
                    .member_facts
                    .get(&(obs.brigade_id, obs.token.clone()))
                    .and_then(|facts| facts.inbound.as_ref())
                    .map(nudge_prompt)
                    .unwrap_or_default();
                state.set_status(format!("relay: nudged {member}{prompt}"), now);
            } else if input.has_unsent
                && state
                    .relay_states
                    .get(&(obs.brigade_id, obs.token.clone()))
                    .is_some_and(|relay_state| {
                        should_nudge(
                            now,
                            relay_state.idle_streak,
                            PaneInput {
                                has_unsent: false,
                                ..input
                            },
                            obs.has_unseen,
                            &relay_state.nudge,
                        )
                    })
            {
                // Every other condition says nudge; the draft alone says no.
                held_by_draft.push((Some(obs.brigade_id), obs.token.clone()));
            }
        }
    }

    if let Some(set_at) = state.status_set_at
        && now.saturating_duration_since(set_at) >= STATUS_TIMEOUT
    {
        state.status = None;
        state.status_set_at = None;
    }

    // After the expiry above, so a showing that just ran out can be followed
    // by the cycle's own gap rather than waiting out another tick.
    if state
        .draft_hold_reminder
        .due(!held_by_draft.is_empty(), state.status.is_none(), now)
    {
        let members = state.members_phrase(app, &held_by_draft);
        state.set_status(
            format!("relay: holding the nudge to {members} — unsent input in the pane"),
            now,
        );
    }

    // Said once per member, and only into a free status line: a member left
    // due stays due until it fits.
    if !unreadable_due.is_empty() && state.status.is_none() {
        let members: Vec<(Option<BrigadeId>, MemberToken)> = unreadable_due
            .iter()
            .map(|(brigade_id, token)| (Some(*brigade_id), token.clone()))
            .collect();
        let members = state.members_phrase(app, &members);
        state.set_status(
            format!(
                "relay: cannot read the input line of {members} — unsent-draft detection is back \
                 to keystroke tracking"
            ),
            now,
        );
        state.unreadable_noticed.extend(unreadable_due);
    }

    let mut trust_waiting: Vec<(Option<BrigadeId>, MemberToken)> = state
        .pending_kickoffs
        .iter()
        .filter(|pending| pending.notified_untrusted)
        .map(|pending| state.kickoff_member(&pending.key))
        .collect();
    trust_waiting.extend(
        state
            .pending_goinkyo_kickoffs
            .iter()
            .filter(|pending| pending.notified_untrusted)
            .map(|pending| state.kickoff_member(&pending.key)),
    );
    if state
        .kickoff_trust_reminder
        .due(!trust_waiting.is_empty(), state.status.is_none(), now)
    {
        let members = state.members_phrase(app, &trust_waiting);
        state.set_status(
            format!("kickoff: waiting for directory trust in {members}"),
            now,
        );
    }

    let mut failures: Vec<(Option<BrigadeId>, MemberToken)> = state
        .pending_kickoffs
        .iter()
        .filter(|pending| pending.attempts >= KICKOFF_MAX_ATTEMPTS)
        .map(|pending| state.kickoff_member(&pending.key))
        .collect();
    failures.extend(
        state
            .pending_goinkyo_kickoffs
            .iter()
            .filter(|pending| pending.attempts >= KICKOFF_MAX_ATTEMPTS)
            .map(|pending| state.kickoff_member(&pending.key)),
    );
    if state
        .kickoff_failed_reminder
        .due(!failures.is_empty(), state.status.is_none(), now)
    {
        let members = state.members_phrase(app, &failures);
        state.set_status(
            format!(
                "kickoff: no acknowledgement after {KICKOFF_MAX_ATTEMPTS} attempts — {members}"
            ),
            now,
        );
    }

    if let Some(armed_at) = state.prefix_armed
        && now.saturating_duration_since(armed_at) >= PREFIX_ARM_TIMEOUT
    {
        state.prefix_armed = None;
    }

    cmds
}

/// Answers `Event::GoinkyoAwaitingSpawn`: spawn the Goinkyo `observation`
/// names, at most once per consultation; once a consultation is observed to
/// have ended, release the guard and close whatever pane it left staged.
///
/// The one-shot guard is [`EmporiumState::goinkyo_pane`], not
/// [`EmporiumState::pending_opens`] (which this still populates, the same
/// way [`open_worker`] does, so [`update_spawned`] knows to append the new
/// pane to the staged brigade once it lands): `pending_opens` is cleared by
/// [`update_spawn_failed`] the moment a spawn fails, which leaves exactly
/// the same trace a Goinkyo that was never even attempted has — no session
/// id, no pane, either way. Without an entry that survives past that
/// removal, this — reachable roughly once a second for as long as the
/// condition holds — could not tell "just failed" from "never tried", and
/// would fire again next tick. Success needs no matching care: once the
/// Goinkyo actually has a pane, shell-side gathering stops reporting a
/// candidate for it at all, so nothing is left here to re-fire on
/// regardless of the guard.
///
/// Also queues a [`PendingGoinkyoKickoff`] on success — see that struct's
/// own doc for why the Goinkyo needs one at all (unlike a Worker: an
/// operator is always the one giving a fresh Worker its first turn, but
/// nobody sits at the Goinkyo's own keyboard) — riding on this same
/// one-shot guard rather than a separate marker of its own.
///
/// The `AwaitingSpawn` arm reads more than presence out of the guard: a
/// *synthetic* key there means a spawn for the current consultation is in
/// flight or already failed (the ordinary attempt-once case, above); a
/// *real* one means `goinkyo_pane` is still naming a pane from a
/// consultation that already ended without `NoGoinkyo` ever firing to
/// release it (`dismiss_goinkyo` immediately followed by a fresh
/// `consult_goinkyo`, or a banto restart before the resumed guard — see
/// `stage_brigade`'s own doc — ever got the chance to). Left alone, that
/// stale pane's own eventual discovery would silently write its session id
/// into whichever row now holds the `goinkyo` token, adopting it as the new
/// consultation's member with no generation check to catch the mismatch —
/// so this tears it down the same way `NoGoinkyo` does, rather than
/// spawning the new one on top of it.
fn update_goinkyo_awaiting_spawn(
    state: &mut EmporiumState,
    brigade: &BrigadeConfig,
    observation: GoinkyoObservation,
    now: Instant,
) -> Vec<Cmd> {
    let candidate = match observation {
        GoinkyoObservation::Unchanged => return Vec::new(),
        // The row disappearing out from under a still-staged brigade is
        // what actually means a consultation was dismissed — see
        // `GoinkyoObservation`'s own doc for why a disbanded brigade never
        // reaches this arm at all. Releasing the guard here rather than on
        // success/failure is what lets a *later* consultation on the same
        // brigade spawn again, without needing to know anything about the
        // one before it — and closing whatever pane the removed entry names
        // is the other half of `goinkyo_pane`'s own doc: nothing else knows
        // which staged pane was the Goinkyo's once its row is gone.
        GoinkyoObservation::NoGoinkyo { brigade_id } => {
            // Whether or not a pane is left to close, the consultation is over:
            // the next one joins the brigade afresh, after whoever joined
            // meanwhile.
            if let Some(joined) = state.member_order.get_mut(&brigade_id) {
                joined.retain(|listed| listed != GOINKYO_TOKEN);
            }
            let Some(key) = state.goinkyo_pane.remove(&brigade_id) else {
                return Vec::new();
            };
            // Unstaging only applies to a pane that is on stage; ending it
            // applies either way. A pane whose brigade is off stage is still
            // a running process — panes are pumped whether staged or not —
            // and this entry was the last thing that knew which process it
            // was. `Cmd::KillPty` naming a key with no handle is a no-op, so
            // a pane that already closed on its own costs nothing here.
            let staged = matches!(
                &state.stage,
                Stage::Brigade { id, panes, .. } if *id == brigade_id && panes.contains(&key)
            );
            if staged {
                state.unstage(&key);
            }
            return vec![kill_pane(state, key)];
        }
        GoinkyoObservation::AwaitingSpawn(candidate) => candidate,
    };
    // The Goinkyo's row exists, so it has joined: the store's roster is the
    // truth about where (it replaces what this run learned), and without one
    // (an old capture) it joins at the end, which is where it is when nothing
    // else joined in the gap.
    if candidate.roster.is_empty() {
        record_join(state, candidate.brigade_id, GOINKYO_TOKEN);
    } else {
        state
            .member_order
            .insert(candidate.brigade_id, candidate.roster.clone());
    }
    if let Some(existing) = state.goinkyo_pane.get(&candidate.brigade_id).cloned() {
        if existing.is_synthetic() {
            // A spawn for the current consultation is already in flight, or
            // already failed and left this as its trace (see this
            // function's own doc on "just failed" vs "never tried") —
            // either way the attempt-once guarantee holds: don't retry.
            return Vec::new();
        }
        // A *real* key here, while this observation is `AwaitingSpawn` (the
        // row it's looking at right now has no session id), can only mean
        // one thing: `goinkyo_pane` still names a pane from a consultation
        // that already ended. Discovery and fork-rename both always write a
        // real id back to a row (`update_discovery_result`/`update_member_
        // session_forked`), and a live consultation's own row always keeps
        // its session id (`gather_goinkyo_observation` reports `Unchanged`
        // for one, never `AwaitingSpawn`) — so a real key paired with a
        // session-id-less row cannot arise from the same, still-open
        // consultation; the row behind that key was deleted and a new one
        // took its place. Two ways that happens without `NoGoinkyo` ever
        // firing to release the guard first: `dismiss_goinkyo` immediately
        // followed by a fresh `consult_goinkyo`, close enough together that
        // the ~1Hz tick never observes the row-less moment in between; or a
        // banto restart, after which `stage_brigade`'s own resume of an
        // already-discovered Goinkyo is what seeds this entry (`stage_
        // brigade`'s own doc) — not `update_goinkyo_awaiting_spawn` — so a
        // dismiss right after *that* restart, before any fresh spawn ever
        // ran, hits the exact same "no entry to release" gap `NoGoinkyo`'s
        // own `let Some(key) = ... else { return }` already tolerates.
        // Left alone, the stale pane keeps running — invisibly adopted as
        // the new consultation's own member the moment its now-delayed
        // discovery finally writes a session id into whichever row still
        // holds the `goinkyo` token, since nothing here checks that it's
        // still the *same* request. Same teardown `NoGoinkyo` performs, so
        // the *next* tick starts the new consultation clean instead of
        // layering it onto a still-running process from the old one.
        state.goinkyo_pane.remove(&candidate.brigade_id);
        let staged = matches!(
            &state.stage,
            Stage::Brigade { id, panes, .. }
                if *id == candidate.brigade_id && panes.contains(&existing)
        );
        if staged {
            state.unstage(&existing);
            return vec![kill_pane(state, existing)];
        }
        return Vec::new();
    }
    let key = SessionKey::new_worker(candidate.brigade_id, GOINKYO_TOKEN);
    // Belt-and-suspenders alongside `goinkyo_pane` above: the guard is what
    // actually distinguishes "just failed" from "never tried" (see this
    // function's own doc), but a pane or an in-flight open already existing
    // for this key — the same evidence `open_worker`'s reuse branch checks
    // (line ~2674) — is a second, independent reason not to spawn again,
    // cheap to check now that this crate can build the key at all (the
    // shell-side gatherer cannot: `SessionKey::new_worker` is private to
    // this crate, which is exactly why this check lives here and not
    // there).
    if state.screens.contains_key(&key) || state.pending_opens.contains_key(&key) {
        return Vec::new();
    }
    state.goinkyo_pane.insert(candidate.brigade_id, key.clone());
    state
        .member_tokens
        .insert(key.clone(), GOINKYO_TOKEN.to_string());
    state.pending_opens.insert(
        key.clone(),
        PendingOpen::BrigadeMember {
            brigade_id: candidate.brigade_id,
            // A Codex kickoff is a Worker-only concern (`open_worker`'s own
            // doc) — a Goinkyo is always Claude, never Codex (see
            // `BrigadeConfig::goinkyo_model`'s doc), so this never applies.
            needs_codex_kickoff: false,
            cwd: candidate.cwd.clone(),
        },
    );
    state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
        key: key.clone(),
        spawned_at: now,
        cwd: candidate.cwd.clone(),
        notified_untrusted: false,
        sent_at: None,
        attempts: 0,
    });
    vec![Cmd::OpenEmbedded {
        key,
        target: SessionToOpen {
            id: String::new(),
            agent: AgentKind::ClaudeCode,
            title: "goinkyo".to_string(),
            cwd: candidate.cwd,
        },
        brigade: Some((
            candidate.brigade_id,
            GOINKYO_TOKEN.to_string(),
            BrigadeRole::Goinkyo,
        )),
        model: brigade.goinkyo_model().map(str::to_string),
        effort: brigade.goinkyo_effort().map(str::to_string),
        permission_mode: brigade.goinkyo_permission_mode().map(str::to_string),
        disallowed_tools: brigade.goinkyo_disallowed_tools().map(str::to_string),
    }]
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
    app: &App,
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
            let name = state.kickoff_label(app, &key);
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
    if state.has_unsent_operator_input(&key)
        || state.pending_kickoffs[pos].attempts >= KICKOFF_MAX_ATTEMPTS
        || state.pending_kickoffs[pos]
            .sent_at
            .is_some_and(|sent_at| now.saturating_duration_since(sent_at) < KICKOFF_ACK_TIMEOUT)
    {
        return Vec::new();
    }
    let entry = &mut state.pending_kickoffs[pos];
    entry.sent_at = Some(now);
    entry.attempts += 1;
    let cmds = vec![Cmd::WritePty {
        key: entry.key.clone(),
        bytes: CODEX_WORKER_KICKOFF_LINE.as_bytes().to_vec(),
    }];
    state.pending_submits.push(PendingSubmit {
        key: entry.key.clone(),
        nudged_at: now,
    });
    cmds
}

/// Answers `Cmd::CheckGoinkyoDirectoryTrust`, always following a
/// [`PendingGoinkyoKickoff`] whose quiet period has already elapsed. Same
/// shape as [`update_worker_directory_trust_checked`] — see its own doc —
/// typing [`GOINKYO_KICKOFF_LINE`] instead of [`CODEX_WORKER_KICKOFF_LINE`]
/// once trusted, and re-asked every tick while it isn't.
fn update_goinkyo_directory_trust_checked(
    state: &mut EmporiumState,
    app: &App,
    key: SessionKey,
    trusted: bool,
    now: Instant,
) -> Vec<Cmd> {
    let Some(pos) = state
        .pending_goinkyo_kickoffs
        .iter()
        .position(|p| p.key == key)
    else {
        return Vec::new();
    };
    if !trusted {
        if !state.pending_goinkyo_kickoffs[pos].notified_untrusted {
            state.pending_goinkyo_kickoffs[pos].notified_untrusted = true;
            let name = state.kickoff_label(app, &key);
            state.set_status(
                format!(
                    "{name}: Claude hasn't been told to trust this directory yet — \
                     answer its prompt in the pane and it will pick up its first turn"
                ),
                now,
            );
        }
        return Vec::new();
    }
    if state.has_unsent_operator_input(&key)
        || state.pending_goinkyo_kickoffs[pos].attempts >= KICKOFF_MAX_ATTEMPTS
        || state.pending_goinkyo_kickoffs[pos]
            .sent_at
            .is_some_and(|sent_at| now.saturating_duration_since(sent_at) < KICKOFF_ACK_TIMEOUT)
    {
        return Vec::new();
    }
    // A retry means the sentence was typed once and no turn followed. If the
    // input line still holds exactly that sentence — and, by the check above,
    // the operator typed nothing — the Enter is what was lost, and typing the
    // sentence again would send it twice. Compared by text, not just "there
    // is some text": the line could hold something else.
    let sentence_still_typed = state.pending_goinkyo_kickoffs[pos].sent_at.is_some()
        && state.screens.get(&key).is_some_and(|pane| {
            read_input_line(pane) == InputLine::HasText
                && typed_text_without_whitespace(pane).as_deref()
                    == Some(&*GOINKYO_KICKOFF_LINE.replace(char::is_whitespace, ""))
        });
    let entry = &mut state.pending_goinkyo_kickoffs[pos];
    entry.sent_at = Some(now);
    entry.attempts += 1;
    if sentence_still_typed {
        return vec![Cmd::WritePty {
            key: entry.key.clone(),
            bytes: b"\r".to_vec(),
        }];
    }
    let cmds = vec![Cmd::WritePty {
        key: entry.key.clone(),
        bytes: GOINKYO_KICKOFF_LINE.as_bytes().to_vec(),
    }];
    state.pending_submits.push(PendingSubmit {
        key: entry.key.clone(),
        nudged_at: now,
    });
    cmds
}

#[cfg(test)]
mod stack_order_tests;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::model::{Activity, StoredMemberLayout};
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
            archived: false,
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
        let areas = layout(Rect::new(0, 0, 120, 40), SIDEBAR_WIDTH);
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
        let areas = layout(Rect::new(0, 0, 120, MIN_HEIGHT_FOR_SUMMARY), SIDEBAR_WIDTH);
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
            stage_tiles(
                area,
                &Stage::Solo(key.clone()),
                DEFAULT_MASTER_PERMILLE,
                &[]
            ),
            vec![(key, area)]
        );
    }

    #[test]
    fn empty_stage_has_no_tiles() {
        let area = Rect::new(36, 0, 84, 39);
        assert!(stage_tiles(area, &Stage::Empty, DEFAULT_MASTER_PERMILLE, &[]).is_empty());
    }

    #[test]
    fn brigade_with_one_member_fills_the_pane() {
        let area = Rect::new(36, 0, 84, 39);
        let director = SessionKey::from_id("dir");
        let stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![director.clone()],
            focused: 0,
        };
        assert_eq!(
            stage_tiles(area, &stage, DEFAULT_MASTER_PERMILLE, &[1, 1]),
            vec![(director, area)]
        );
    }

    #[test]
    fn brigade_tiles_director_left_and_stacks_workers_right() {
        let area = Rect::new(36, 0, 84, 40);
        let director = SessionKey::from_id("dir");
        let w0 = SessionKey::from_id("w0");
        let w1 = SessionKey::from_id("w1");
        let stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![director.clone(), w0.clone(), w1.clone()],
            focused: 0,
        };
        let tiles = stage_tiles(area, &stage, DEFAULT_MASTER_PERMILLE, &[1, 1]);
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
            director: None,
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
            director: None,
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
            director: None,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        stage.remove(&SessionKey::from_id("dir"));
        assert!(matches!(stage, Stage::Empty));
    }

    #[test]
    fn stage_remove_of_the_directors_own_pane_clears_director_but_leaves_other_panes_staged() {
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        let mut stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![director.clone(), w1.clone()],
            focused: 0,
        };
        stage.remove(&director);
        match &stage {
            Stage::Brigade {
                director, panes, ..
            } => {
                assert!(director.is_none());
                assert_eq!(panes, &vec![w1]);
            }
            other => panic!("expected the brigade to stay staged, got {other:?}"),
        }
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
            PaneInput::default(),
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
            PaneInput::default(),
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
            PaneInput::default(),
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
            PaneInput::default(),
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
            PaneInput {
                is_focused: true,
                last_forwarded: Some(last_input),
                has_unsent: false,
            },
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
            PaneInput {
                is_focused: true,
                last_forwarded: Some(last_input),
                has_unsent: false,
            },
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
            PaneInput {
                is_focused: false,
                last_forwarded: Some(last_input),
                has_unsent: false,
            },
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
            PaneInput::default(),
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
            PaneInput::default(),
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
            PaneInput::default(),
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
            PaneInput::default(),
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
        let brigade_id = 1;
        let token = "worker-1".to_string();
        let now = test_instant();

        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert!(tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_keeps_same_named_members_in_different_brigades_separate() {
        let mut states = HashMap::new();
        let token = DIRECTOR_TOKEN.to_string();
        let now = test_instant();

        assert!(!tick_relay_decision(
            &mut states,
            1,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert!(tick_relay_decision(
            &mut states,
            1,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            2,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert!(tick_relay_decision(
            &mut states,
            2,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));

        assert_eq!(states.get(&(1, token.clone())).unwrap().nudge.attempts, 1);
        assert_eq!(states.get(&(2, token)).unwrap().nudge.attempts, 1);
    }

    #[test]
    fn tick_relay_decision_busy_tick_resets_the_idle_streak() {
        let mut states = HashMap::new();
        let brigade_id = 1;
        let token = "worker-1".to_string();
        let now = test_instant();

        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(false),
            PaneInput::default(),
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_unknown_live_entry_never_counts_as_idle() {
        let mut states = HashMap::new();
        let brigade_id = 1;
        let token = "worker-1".to_string();
        let now = test_instant();

        for _ in 0..5 {
            assert!(!tick_relay_decision(
                &mut states,
                brigade_id,
                &token,
                now,
                None,
                PaneInput::default(),
                true,
            ));
        }
    }

    #[test]
    fn tick_relay_decision_resets_on_drain_so_the_next_batch_starts_fresh() {
        let mut states = HashMap::new();
        let brigade_id = 1;
        let token = "worker-1".to_string();
        let now = test_instant();

        tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        );
        assert!(tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert_eq!(
            states
                .get(&(brigade_id, token.clone()))
                .unwrap()
                .nudge
                .attempts,
            1
        );

        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            false,
        ));
        assert!(!states.contains_key(&(brigade_id, token.clone())));

        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        ));
        assert_eq!(
            states
                .get(&(brigade_id, token.clone()))
                .unwrap()
                .idle_streak,
            1
        );
        assert_eq!(
            states
                .get(&(brigade_id, token.clone()))
                .unwrap()
                .nudge
                .attempts,
            0
        );
    }

    #[test]
    fn tick_relay_decision_stops_after_the_attempt_cap_even_past_cooldown() {
        let mut states = HashMap::new();
        let brigade_id = 1;
        let token = "worker-1".to_string();
        let mut now = test_instant();

        tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
            true,
        );
        for _ in 0..RELAY_MAX_ATTEMPTS {
            now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
            assert!(tick_relay_decision(
                &mut states,
                brigade_id,
                &token,
                now,
                Some(true),
                PaneInput::default(),
                true,
            ));
        }
        assert_eq!(
            states
                .get(&(brigade_id, token.clone()))
                .unwrap()
                .nudge
                .attempts,
            RELAY_MAX_ATTEMPTS
        );

        now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
        assert!(!tick_relay_decision(
            &mut states,
            brigade_id,
            &token,
            now,
            Some(true),
            PaneInput::default(),
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
                layout: None,
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
            director: Some(director.clone()),
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
        assert_eq!(state.member_token_for(&discovered), Some("worker-1"));
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
    fn discovery_result_on_the_directors_own_key_renames_the_director_field() {
        // Defensive path per `update_discovery_result`'s own doc: the
        // Director's key is never actually synthetic in production, but the
        // rename must still be correct if that assumption is ever wrong.
        let mut state = EmporiumState::new(PrefixKey::default());
        let pending_director = SessionKey::new_worker(1, "director");
        let worker = SessionKey::from_id("w1");
        staged_brigade(&mut state, &pending_director, std::slice::from_ref(&worker));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let _cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: pending_director.clone(),
                session_id: "dir".to_string(),
                member: None,
            },
            test_instant(),
        );

        let discovered = SessionKey::from_id("dir");
        let Stage::Brigade {
            director, panes, ..
        } = &state.stage
        else {
            panic!("expected a staged brigade");
        };
        assert_eq!(director.as_ref(), Some(&discovered));
        assert_eq!(panes, &[discovered, worker]);
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
                    layout: None,
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
    fn member_session_forked_on_the_directors_own_session_renames_the_director_field() {
        // Genuinely reachable, unlike discovery's own version of this: an
        // auto-compaction fork can happen to any already-running Claude
        // session, the Director's own included.
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir-old");
        let worker = SessionKey::from_id("w1");
        staged_brigade(&mut state, &director, std::slice::from_ref(&worker));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let _cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "director".to_string(),
                old_id: "dir-old".to_string(),
                new_id: "dir-new".to_string(),
            },
            now,
        );

        let new_director = SessionKey::from_id("dir-new");
        let Stage::Brigade {
            director, panes, ..
        } = &state.stage
        else {
            panic!("expected a staged brigade");
        };
        assert_eq!(director.as_ref(), Some(&new_director));
        assert_eq!(panes, &[new_director, worker]);
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

    // --- model on resume ------------------------------------------------------

    #[test]
    fn stage_brigade_passes_the_configured_model_to_a_resumed_worker_and_goinkyo_but_never_to_the_director()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("w1"), row("g1")]);
        let brigade = brigade_config(); // worker_model "sonnet", goinkyo_model "fable"
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
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
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
                layout: None,
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
        assert!(
            models
                .iter()
                .any(|(id, model)| id == "g1" && model.as_deref() == Some("fable")),
            "a resumed Goinkyo must carry the configured goinkyo_model: {models:?}"
        );
    }

    /// Shared by the `permission_mode`/`disallowed_tools` resume tests below:
    /// stage a Director/Worker/Goinkyo trio, each already resolved to a row,
    /// under `brigade`, and return `(id, permission_mode, disallowed_tools)`
    /// for every `Cmd::OpenEmbedded` the stage produced.
    fn staged_trio_launch_modes(
        brigade: &BrigadeConfig,
    ) -> Vec<(String, Option<String>, Option<String>)> {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("w1"), row("g1")]);
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
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
            ),
        ];
        let cmds = update(
            &mut state,
            &mut app,
            brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(roster),
                layout: None,
            },
            now,
        );

        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::OpenEmbedded {
                    target,
                    permission_mode,
                    disallowed_tools,
                    ..
                } => Some((
                    target.id.clone(),
                    permission_mode.clone(),
                    disallowed_tools.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stage_brigade_passes_the_configured_permission_mode_to_a_resumed_worker_and_goinkyo_but_never_to_director()
     {
        let brigade = BrigadeConfig {
            worker_permission_mode: "acceptEdits".to_string(),
            goinkyo_permission_mode: "bypassPermissions".to_string(),
            ..BrigadeConfig::default()
        };
        let modes = staged_trio_launch_modes(&brigade);

        assert!(
            modes
                .iter()
                .any(|(id, mode, _)| id == "dir" && mode.is_none()),
            "the Director must never carry --permission-mode: {modes:?}"
        );
        assert!(
            modes
                .iter()
                .any(|(id, mode, _)| id == "w1" && mode.as_deref() == Some("acceptEdits")),
            "a resumed Worker must carry the configured worker_permission_mode: {modes:?}"
        );
        assert!(
            modes
                .iter()
                .any(|(id, mode, _)| id == "g1" && mode.as_deref() == Some("bypassPermissions")),
            "a resumed Goinkyo must carry the configured goinkyo_permission_mode: {modes:?}"
        );
    }

    #[test]
    fn stage_brigade_passes_the_configured_disallowed_tools_to_a_resumed_goinkyo_but_never_to_worker_or_director()
     {
        let brigade = BrigadeConfig {
            goinkyo_disallowed_tools: "Bash".to_string(),
            ..BrigadeConfig::default()
        };
        let modes = staged_trio_launch_modes(&brigade);

        assert!(
            modes
                .iter()
                .any(|(id, _, tools)| id == "dir" && tools.is_none()),
            "the Director must never carry --disallowedTools: {modes:?}"
        );
        assert!(
            modes
                .iter()
                .any(|(id, _, tools)| id == "w1" && tools.is_none()),
            "a resumed Worker must never carry --disallowedTools: {modes:?}"
        );
        assert!(
            modes
                .iter()
                .any(|(id, _, tools)| id == "g1" && tools.as_deref() == Some("Bash")),
            "a resumed Goinkyo must carry the configured goinkyo_disallowed_tools: {modes:?}"
        );
    }

    #[test]
    fn stage_brigade_passes_the_configured_effort_to_a_resumed_goinkyo_but_never_to_worker_or_director()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("w1"), row("g1")]);
        let brigade = BrigadeConfig {
            goinkyo_effort: "low".to_string(),
            ..BrigadeConfig::default()
        };
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
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
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
                layout: None,
            },
            now,
        );

        let efforts: Vec<(String, Option<String>)> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::OpenEmbedded { target, effort, .. } => {
                    Some((target.id.clone(), effort.clone()))
                }
                _ => None,
            })
            .collect();
        assert!(
            efforts
                .iter()
                .any(|(id, effort)| id == "dir" && effort.is_none()),
            "the Director must never carry --effort: {efforts:?}"
        );
        assert!(
            efforts
                .iter()
                .any(|(id, effort)| id == "w1" && effort.is_none()),
            "a resumed Worker must never carry --effort: {efforts:?}"
        );
        assert!(
            efforts
                .iter()
                .any(|(id, effort)| id == "g1" && effort.as_deref() == Some("low")),
            "a resumed Goinkyo must carry the configured effort: {efforts:?}"
        );
    }

    #[test]
    fn stage_brigade_resumed_goinkyo_carries_no_effort_when_the_config_value_is_the_empty_escape_hatch()
     {
        // The same accessor (`BrigadeConfig::goinkyo_effort`) backs both the
        // fresh-spawn path (`update_goinkyo_awaiting_spawn`) and this resume
        // path, so an empty-string config value can't mean "pass nothing" on
        // one and something else on the other — this pins that down for the
        // resume path specifically.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("g1")]);
        let brigade = BrigadeConfig {
            goinkyo_effort: String::new(),
            ..BrigadeConfig::default()
        };
        let now = test_instant();
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
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
                layout: None,
            },
            now,
        );

        let goinkyo_effort = cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded { target, effort, .. } if target.id == "g1" => Some(effort.clone()),
            _ => None,
        });
        assert_eq!(
            goinkyo_effort,
            Some(None),
            "expected no --effort: {goinkyo_effort:?}"
        );
    }

    // --- stage_brigade: a stranded Goinkyo -----------------------------------

    #[test]
    fn stage_brigade_resets_a_stranded_goinkyos_session_id_instead_of_reporting_it_missing() {
        // A Goinkyo that never ran a single turn: `session_id` is set (its
        // own discovery already ran once) but no row exists for it (no
        // `projects/*.jsonl` transcript ever written, so `app.row_for_id`
        // never resolves it) — and no pane is alive under that id either
        // (`state.screens` empty of it), so this one is truly stranded, not
        // just caught in the discovery window (see the reuse test just
        // below).
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        let now = test_instant();
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
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
                layout: None,
            },
            now,
        );

        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Cmd::Store(StoreIntent::ClearMemberSession { brigade_id: 1, token })
                    if token == "goinkyo"
            )),
            "expected a ClearMemberSession for the stranded goinkyo: {cmds:?}"
        );
        match &state.stage {
            Stage::Brigade { panes, .. } => assert!(
                !panes.iter().any(|k| k.as_str() == "g1"),
                "the stranded goinkyo has no live pane to stage: {panes:?}"
            ),
            other => panic!("expected a staged brigade, got {other:?}"),
        }
        // Not counted as `missing`: this is a self-healing case, not an
        // unexplained absence the operator needs a status line about.
        assert!(
            state.status.is_none(),
            "resetting a stranded goinkyo must not read as \"not found\": {:?}",
            state.status
        );
    }

    #[test]
    fn stage_brigade_reuses_a_goinkyos_live_pane_in_the_discovery_window_instead_of_clearing_it() {
        // The trap the stranded-goinkyo fix must not fall into: `session_id`
        // is set and `app.row_for_id` still can't resolve it (same as the
        // truly stranded case above), but this time the pane is already
        // alive under that id key — `Cmd::RekeyPty` already renamed it
        // (`update_discovery_result`), `app`'s own row list just hasn't
        // caught up yet (no transcript written until the first turn). If
        // this cleared `session_id` here, it would sever a *live* Goinkyo
        // from the row its own `_mcp` connection resolves itself through
        // (`Store::brigade_of_session`), breaking `send_to_peer`/
        // `check_messages` for it mid-consultation.
        let mut state = EmporiumState::new(PrefixKey::default());
        state
            .screens
            .insert(SessionKey::from_id("g1"), Screen::new(24, 80));
        let mut app = app_with(vec![row("dir")]);
        let brigade = brigade_config();
        let now = test_instant();
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
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
                layout: None,
            },
            now,
        );

        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::ClearMemberSession { .. }))),
            "a live pane must never be cleared out from under itself: {cmds:?}"
        );
        match &state.stage {
            Stage::Brigade { panes, .. } => assert!(
                panes.iter().any(|k| k.as_str() == "g1"),
                "the already-live goinkyo pane must be reused, not re-spawned: {panes:?}"
            ),
            other => panic!("expected a staged brigade, got {other:?}"),
        }
        assert!(state.status.is_none());
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
                layout: None,
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
                layout: None,
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
                reason: None,
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
    fn the_director_never_carries_a_permission_mode() {
        // `permission_mode` never reaches the Director (`RoleLaunch::of`) —
        // proven here with deliberately non-default `worker_permission_mode`/
        // `goinkyo_permission_mode`, so a leak from either sibling role would
        // show up as `Some(...)`, not just an unexercised `None`.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex,
            worker_permission_mode: "acceptEdits".to_string(),
            goinkyo_permission_mode: "bypassPermissions".to_string(),
            ..BrigadeConfig::default()
        };

        let director_cmds = update(
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
        let [
            Cmd::OpenEmbedded {
                permission_mode, ..
            },
        ] = director_cmds.as_slice()
        else {
            panic!("expected exactly one OpenEmbedded, got {director_cmds:?}");
        };
        assert_eq!(permission_mode, &None, "the Director's own launch");
    }

    #[test]
    fn a_freshly_spawned_worker_carries_the_configured_permission_mode() {
        // Unlike the Director (see `the_director_never_carries_a_permission_
        // mode`), a fresh Worker spawn carries `worker_permission_mode` on
        // `Cmd::OpenEmbedded` regardless of which product it resolves to —
        // the Claude/Codex split (and the fact that Codex has no
        // `--permission-mode` equivalent to carry it into) happens
        // downstream, in `crate::embedded::emporium::build_open_launch`, not
        // here. This brigade's Worker is deliberately Codex to pin exactly
        // that.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        let brigade = BrigadeConfig {
            worker_agent: WorkerAgentSetting::Codex,
            worker_permission_mode: "acceptEdits".to_string(),
            ..BrigadeConfig::default()
        };

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
        let worker_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: SessionKey::from_id("dir"),
            },
            test_instant(),
        );
        let worker_permission_mode = worker_cmds.iter().find_map(|cmd| match cmd {
            Cmd::OpenEmbedded {
                target,
                permission_mode,
                ..
            } if target.id.is_empty() => Some(permission_mode.clone()),
            _ => None,
        });
        assert_eq!(
            worker_permission_mode,
            Some(Some("acceptEdits".to_string())),
            "the Worker's own launch"
        );
    }

    #[test]
    fn add_worker_resolves_the_configured_agent_from_the_staged_directors_row() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
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
    fn add_worker_resolves_the_directors_row_even_when_panes_are_reversed() {
        // The Worker staged solo first, ahead of the Director, so
        // `panes[0]` is the Worker — proves the cwd/agent lookup reads
        // `director`, not position (`panes[0]`'s own row is a distinct,
        // deliberately different cwd/agent to catch a positional read).
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![worker.clone(), director],
            focused: 0,
        };
        let mut worker_row = row("w1");
        worker_row.agent = AgentKind::Codex;
        worker_row.cwd = Some(PathBuf::from("/work/wrong"));
        let mut director_row = row("dir");
        director_row.cwd = Some(PathBuf::from("/work/right"));
        let mut app = app_with(vec![worker_row, director_row]);
        let brigade = brigade_config(); // Inherit — must follow the Director's own product

        let cmds = add_worker(&mut state, &mut app, &brigade);

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::AddWorker {
                cwd,
                worker_agent: AgentKind::ClaudeCode,
                ..
            })] if cwd == &PathBuf::from("/work/right")
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
                layout: None,
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
                layout: None,
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
                reason: None,
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

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc, Modifiers::NONE);

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
                layout: None,
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
                reason: None,
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
                layout: None,
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
                layout: None,
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
            director_agent: AgentKind::ClaudeCode,
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
                    ..
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
            director_agent: AgentKind::ClaudeCode,
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
            director_agent: AgentKind::ClaudeCode,
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
            director_agent: AgentKind::ClaudeCode,
            name: "cell".to_string(),
            cwd: PathBuf::from("/work"),
            worker_count: 1,
        });
        app.open_worker_agent_modal(AgentKind::Codex, String::new(), String::new());

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc, Modifiers::NONE);

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

        update_modal_key(&mut state, &mut app, KeyCode::BackTab, Modifiers::NONE);

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
            director: Some(director.clone()),
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

        let cmds = update_modal_key(&mut state, &mut app, KeyCode::Esc, Modifiers::NONE);

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
        let areas = state
            .geometry(Rect::new(0, 0, state.size.0, state.size.1))
            .areas;
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
        let areas = state
            .geometry(Rect::new(0, 0, state.size.0, state.size.1))
            .areas;
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
            director: None,
            panes: vec![director, worker.clone()],
            focused: 0,
        };
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let tiles = state
            .geometry(Rect::new(0, 0, state.size.0, state.size.1))
            .tiles;
        let (_, worker_rect) = tiles.iter().find(|(key, _)| *key == worker).unwrap();
        // Inside the tile: its left border column is the master/stack
        // boundary's handle now, and a press there picks the boundary up.
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: worker_rect.x + 2,
            row: worker_rect.y + 2,
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

        let areas = state
            .geometry(Rect::new(0, 0, state.size.0, state.size.1))
            .areas;
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

        let areas = state
            .geometry(Rect::new(0, 0, state.size.0, state.size.1))
            .areas;
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
            Event::PtyExited {
                key: key.clone(),
                reason: None,
            },
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
    fn pty_exited_with_no_reason_keeps_the_plain_status_wording() {
        // `reason: None` is what an old-format replay stream deserializes
        // to (`#[serde(default)]`) as well as a live tick that lost the
        // shell's own observation race — both must fall back to exactly the
        // pre-existing wording, with no trailing " — ..." clause.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited { key, reason: None },
            test_instant(),
        );

        assert_eq!(state.status.as_deref(), Some("session ended: sess-1"));
    }

    #[test]
    fn pty_exited_reports_a_zero_exit_code_as_exited_normally() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key,
                reason: Some(PtyExitReason::Code(0)),
            },
            test_instant(),
        );

        assert_eq!(
            state.status.as_deref(),
            Some("session ended: sess-1 — exited normally")
        );
    }

    #[test]
    fn pty_exited_reports_a_nonzero_exit_code_signal_and_unknown_reason_distinctly() {
        let brigade = brigade_config();
        let reasons_and_expected = [
            (
                PtyExitReason::Code(1),
                "session ended: sess-1 — exited with code 1",
            ),
            (
                PtyExitReason::Signal("SIGKILL".to_string()),
                "session ended: sess-1 — terminated by SIGKILL",
            ),
            (
                PtyExitReason::Unknown,
                "session ended: sess-1 — exit reason unknown",
            ),
        ];
        for (reason, expected) in reasons_and_expected {
            let mut state = EmporiumState::new(PrefixKey::default());
            let key = SessionKey::from_id("sess-1");
            state.stage = Stage::Solo(key.clone());
            let mut app = app_with(vec![row("sess-1")]);

            update(
                &mut state,
                &mut app,
                &brigade,
                Event::PtyExited {
                    key,
                    reason: Some(reason.clone()),
                },
                test_instant(),
            );

            assert_eq!(state.status.as_deref(), Some(expected), "for {reason:?}");
        }
    }

    #[test]
    fn a_pty_write_dropped_for_lack_of_a_live_handle_is_reported_not_silent() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::from_id("gone");

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyWriteDropped { key },
            test_instant(),
        );

        assert!(cmds.is_empty());
        let status = state.status.as_deref().unwrap();
        assert!(status.contains("gone"), "got {status:?}");
        assert!(status.contains("bug"), "got {status:?}");
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
            Event::PtyExited {
                key: key.clone(),
                reason: None,
            },
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
            director: Some(director.clone()),
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
                reason: None,
            },
            test_instant(),
        );

        match &state.stage {
            Stage::Brigade {
                director: staged_director,
                panes,
                focused,
                ..
            } => {
                assert_eq!(panes, std::slice::from_ref(&director));
                assert_eq!(*focused, 0);
                assert_eq!(
                    staged_director.as_ref(),
                    Some(&director),
                    "a Worker's own pane exiting must not touch director"
                );
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

    // --- Ctrl+R/Ctrl+X in the `g` dialog (rename/delete a group) -----------

    fn app_with_one_group() -> App {
        let mut app =
            App::new(vec![row("a")]).with_groups(vec![(1, "work".to_string())], HashMap::new());
        app.set_viewport_height(10);
        app
    }

    fn ctrl_key(c: char) -> Event {
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            Modifiers::CONTROL,
        )))
    }

    #[test]
    fn ctrl_r_in_the_group_join_modal_opens_the_rename_sub_modal() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.open_group_join_modal();
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            ctrl_key('r'),
            test_instant(),
        );

        assert!(
            cmds.is_empty(),
            "opening the sub-modal is a pure App mutation, no Cmd"
        );
        let Some(Modal::GroupRename(gstate)) = app.modal() else {
            panic!(
                "expected the rename sub-modal to open, got {:?}",
                app.modal().is_some()
            );
        };
        assert_eq!(gstate.group_id(), 1);
        assert_eq!(gstate.old_name(), "work");
    }

    #[test]
    fn ctrl_r_on_the_leave_row_is_a_noop() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.set_session_group_cache("a", 1, "work".to_string());
        app.open_group_join_modal();
        app.modal_select_next(); // onto the `(no group)` row
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            ctrl_key('r'),
            test_instant(),
        );

        assert!(
            matches!(app.modal(), Some(Modal::GroupJoin(_))),
            "the leave row names no group to rename"
        );
    }

    #[test]
    fn ctrl_x_in_the_group_join_modal_opens_the_delete_confirm_sub_modal() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.open_group_join_modal();
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            ctrl_key('x'),
            test_instant(),
        );

        assert!(cmds.is_empty());
        assert!(matches!(
            app.modal(),
            Some(Modal::ConfirmGroupDelete { group_id: 1, name }) if name == "work"
        ));
    }

    #[test]
    fn ctrl_x_on_the_leave_row_is_a_noop() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.set_session_group_cache("a", 1, "work".to_string());
        app.open_group_join_modal();
        app.modal_select_next();
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            ctrl_key('x'),
            test_instant(),
        );

        assert!(matches!(app.modal(), Some(Modal::GroupJoin(_))));
    }

    #[test]
    fn confirming_group_rename_issues_a_rename_group_store_cmd_and_closes_the_modal() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.open_group_join_modal();
        app.open_group_rename_modal();
        for c in "projects".chars() {
            app.modal_push_char(c);
        }
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            test_instant(),
        );

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::RenameGroup { group_id: 1, old_name, new_name })]
                if old_name == "work" && new_name == "projects"
        ));
        assert!(app.modal().is_none());
    }

    #[test]
    fn confirming_group_delete_issues_a_delete_group_store_cmd_and_closes_the_modal() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        app.open_group_join_modal();
        app.open_confirm_group_delete_modal();
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            test_instant(),
        );

        assert!(matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::DeleteGroup { group_id: 1, name })] if name == "work"
        ));
        assert!(app.modal().is_none());
    }

    #[test]
    fn group_rename_done_updates_the_cache_on_success() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::GroupRenameDone {
                group_id: 1,
                old_name: "work".to_string(),
                new_name: "projects".to_string(),
                result: Ok(()),
            },
            test_instant(),
        );

        app.open_group_join_modal();
        let Some(Modal::GroupJoin(gstate)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert_eq!(gstate.candidates(), vec!["projects"]);
    }

    #[test]
    fn group_rename_done_leaves_the_cache_alone_on_failure() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::GroupRenameDone {
                group_id: 1,
                old_name: "work".to_string(),
                new_name: "play".to_string(),
                result: Err("duplicate name".to_string()),
            },
            test_instant(),
        );

        app.open_group_join_modal();
        let Some(Modal::GroupJoin(gstate)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert_eq!(
            gstate.candidates(),
            vec!["work"],
            "a failed rename must not touch the cached name"
        );
    }

    #[test]
    fn group_delete_done_removes_the_group_from_the_cache_on_success() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with_one_group();
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::GroupDeleteDone {
                group_id: 1,
                name: "work".to_string(),
                result: Ok(()),
            },
            test_instant(),
        );

        app.open_group_join_modal();
        let Some(Modal::GroupJoin(gstate)) = app.modal() else {
            panic!("expected an open group-join modal");
        };
        assert!(gstate.candidates().is_empty());
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
            director: None,
            panes: vec![director, worker.clone()],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let mut now = test_instant();

        let observation = || RelayObservation {
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
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
                brigades: Vec::new(),
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
                brigades: Vec::new(),
            },
            now,
        );
        assert_eq!(
            writes(&cmds),
            vec![(worker.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec())]
        );
        assert!(
            state
                .status
                .as_deref()
                .unwrap()
                .contains("nudged worker-1 of brigade 1")
        );

        // A tick before the submit delay has elapsed: nothing flushed yet.
        now += Duration::from_millis(50);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert_eq!(writes(&cmds), vec![(worker.clone(), b"\r".to_vec())]);
    }

    #[test]
    fn a_draft_in_an_offstage_brigade_holds_its_nudge_with_a_qualified_notice() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let visible = SessionKey::from_id("visible-director");
        let offstage = SessionKey::from_id("offstage-director");
        state.screens.insert(visible.clone(), Screen::new(24, 80));
        state.screens.insert(offstage.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(visible.clone()),
            panes: vec![visible],
            focused: 0,
        };
        state.unsent_operator_input.insert(offstage.clone());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let observation = || RelayObservation {
            glance: None,
            brigade_id: 2,
            token: DIRECTOR_TOKEN.to_string(),
            key: offstage.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
        };
        let now = test_instant();

        let first = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
            },
            now,
        );
        let second = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
            },
            now + Duration::from_secs(1),
        );

        assert!(writes_of(&first).is_empty());
        assert!(writes_of(&second).is_empty());
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("director of brigade 2")),
            "the notice must distinguish the off-stage director: {:?}",
            state.status
        );
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
            None,
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
            None,
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
            sent_at: None,
            attempts: 0,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            spawned_at + CODEX_KICKOFF_QUIET_PERIOD - Duration::from_millis(1),
        );
        assert!(cmds.is_empty(), "not quiet long enough yet");
        assert_eq!(state.pending_kickoffs.len(), 1);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            sent_at: None,
            attempts: 0,
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
        assert_eq!(state.pending_kickoffs[0].attempts, 1);
        assert!(state.pending_kickoffs[0].sent_at.is_some());
        assert_eq!(state.pending_submits.len(), 1);
        assert_eq!(state.pending_submits[0].key, key);
    }

    #[test]
    fn kickoff_retries_only_after_timeout_and_stops_at_the_cap() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        let t0 = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: t0,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        for attempt in 1..=KICKOFF_MAX_ATTEMPTS {
            let at = t0 + KICKOFF_ACK_TIMEOUT * (attempt - 1);
            if attempt > 1 {
                let cmds = update(
                    &mut state,
                    &mut app,
                    &brigade,
                    Event::Tick {
                        relay: vec![],
                        brigades: Vec::new(),
                    },
                    at,
                );
                assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::CheckWorkerDirectoryTrust { key: checked, .. } if checked == &key)));
            }
            let cmds = update(
                &mut state,
                &mut app,
                &brigade,
                Event::WorkerDirectoryTrustChecked {
                    key: key.clone(),
                    trusted: true,
                },
                at,
            );
            assert!(matches!(cmds.as_slice(), [Cmd::WritePty { .. }]));
        }
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + KICKOFF_ACK_TIMEOUT * KICKOFF_MAX_ATTEMPTS,
        );
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::CheckWorkerDirectoryTrust { .. }))
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("no acknowledgement"))
        );
    }

    #[test]
    fn kickoff_never_overwrites_unsent_operator_input() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, "worker-1");
        let t0 = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: t0,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        state.unsent_operator_input.insert(key.clone());

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + CODEX_KICKOFF_QUIET_PERIOD,
        );
        assert!(cmds.is_empty());
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked { key, trusted: true },
            t0,
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn rekeyed_output_keeps_the_kickoff_quiet_window() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let old = SessionKey::new_worker(1, "worker-1");
        let new = SessionKey::from_id("worker-real");
        let t0 = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: old.clone(),
            spawned_at: t0 - Duration::from_secs(1),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        state.last_output_at.insert(old.clone(), t0);
        state.rekey_pane_state(&old, &new);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + CODEX_KICKOFF_QUIET_PERIOD - Duration::from_millis(1),
        );
        assert!(
            cmds.is_empty(),
            "the old output must still delay the renamed pane"
        );
    }

    #[test]
    fn a_sent_goinkyo_is_acknowledged_by_its_new_row_without_retrying() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("goinkyo-real")]);
        let brigade = brigade_config();
        let key = SessionKey::from_id("goinkyo-real");
        let now = test_instant();
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key,
            spawned_at: now,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: Some(now),
            attempts: 1,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now + KICKOFF_ACK_TIMEOUT,
        );
        assert!(cmds.is_empty());
        assert!(state.pending_goinkyo_kickoffs.is_empty());
    }

    #[test]
    fn trust_waiting_kickoff_reminds_again_after_the_status_expires() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(3, "worker-1");
        let t0 = test_instant();
        state.pending_kickoffs.push(PendingKickoff {
            key: key.clone(),
            spawned_at: t0,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked {
                key,
                trusted: false,
            },
            t0,
        );
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + STATUS_TIMEOUT,
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("waiting for directory trust"))
        );
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
            sent_at: None,
            attempts: 0,
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
            sent_at: None,
            attempts: 0,
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            sent_at: None,
            attempts: 0,
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
            sent_at: None,
            attempts: 0,
        });
        state.last_output_at.insert(key.clone(), test_instant());
        let mut app = app_with(vec![row("sess-1")]);
        let brigade = brigade_config();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key.clone(),
                reason: None,
            },
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
            director: Some(director.clone()),
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
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
        };
        now += Duration::from_millis(10);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
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
                brigades: Vec::new(),
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
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
        };
        now += Duration::from_millis(10);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
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
                brigades: Vec::new(),
            },
            now,
        );

        assert!(
            writes_of(&cmds).is_empty(),
            "recent input to the focused pane itself must still suppress its own nudge"
        );
    }

    /// Forward one key press to whichever pane `state` has focused.
    fn forward_key(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        code: KeyCode,
        modifiers: Modifiers,
        now: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(code, modifiers))),
            now,
        )
    }

    /// Two consecutive idle ticks a second apart with unread mail for
    /// `key` (the debounce a nudge needs), advancing `now` past both.
    /// Returns every PTY write the two ticks produced.
    fn nudge_ticks(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        token: &str,
        key: &SessionKey,
        now: &mut Instant,
    ) -> Vec<(SessionKey, Vec<u8>)> {
        let mut writes = Vec::new();
        for _ in 0..2 {
            *now += Duration::from_secs(1);
            let cmds = update(
                state,
                app,
                brigade,
                Event::Tick {
                    relay: vec![RelayObservation {
                        glance: None,
                        brigade_id: 1,
                        token: token.to_string(),
                        key: key.clone(),
                        has_unseen: true,
                        is_idle_this_tick: Some(true),
                        agent: None,
                    }],
                    brigades: Vec::new(),
                },
                *now,
            );
            writes.extend(writes_of(&cmds));
        }
        writes
    }

    /// The nudge, and the Enter the second tick of a call that began with the
    /// idle streak already built up flushes behind it.
    fn nudge_then_enter_for(key: &SessionKey) -> Vec<(SessionKey, Vec<u8>)> {
        let mut writes = nudge_for(key);
        writes.push((key.clone(), b"\r".to_vec()));
        writes
    }

    fn nudge_for(key: &SessionKey) -> Vec<(SessionKey, Vec<u8>)> {
        vec![(key.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec())]
    }

    #[test]
    fn a_pane_no_longer_focused_is_not_suppressed_by_the_time_of_its_own_earlier_keystroke() {
        // `is_focused` gates the quiet-period check, and this proves it holds
        // through the per-pane storage change too: the Director pane keeps a
        // very recent `last_forwarded_input` entry, yet once focus has moved
        // off it, that timestamp alone does not hold its nudge back. The
        // keystroke is an arrow, which leaves no draft behind — a typed
        // character would (see the draft tests below), and the timestamp is
        // not what stops that one.
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0); // Director focused first

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Left,
            Modifiers::NONE,
            now,
        );
        assert!(state.last_forwarded_input.contains_key(&director));
        assert!(!state.has_unsent_operator_input(&director));

        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1; // focus moves to the Worker; Director is no longer focused
        }
        let writes = nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        );

        assert_eq!(
            writes,
            nudge_for(&director),
            "the Director pane is no longer focused, so the time of its own \
             earlier keystroke must not suppress its nudge"
        );
    }

    #[test]
    fn a_pane_the_operator_typed_into_is_not_nudged_after_focus_moves_off_it() {
        // The second reported corruption: a draft left in the Director's input
        // line while the operator went elsewhere in banto. The quiet period
        // never applied once focus left the pane.
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        assert!(state.has_unsent_operator_input(&director));
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }

        let mut writes = nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        );
        now += Duration::from_secs(600);
        writes.extend(nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        ));

        assert!(
            writes.is_empty(),
            "an unfocused pane with an unsent draft must not be written to: {writes:?}"
        );
        assert!(state.pending_submits.is_empty());
    }

    #[test]
    fn a_pane_the_operator_typed_into_is_not_nudged_however_long_it_stays_focused() {
        // The same corruption by the other route: focus stays on the pane and
        // the operator simply stops typing for longer than the quiet period.
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        now += Duration::from_secs(3600);

        let writes = nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        );

        assert!(
            writes.is_empty(),
            "an hour of silence is not a sent message: {writes:?}"
        );
    }

    #[test]
    fn sending_with_a_plain_enter_releases_the_hold() {
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);

        for (code, modifiers) in [
            (KeyCode::Char('a'), Modifiers::NONE),
            (KeyCode::Enter, Modifiers::NONE),
        ] {
            forward_key(&mut state, &mut app, &brigade, code, modifiers, now);
        }
        assert!(!state.has_unsent_operator_input(&director));
        // Still the focused pane, so the quiet period (which is about
        // typing, not drafts) has to elapse as well.
        now += RELAY_INPUT_QUIET_PERIOD + Duration::from_secs(1);

        let writes = nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        );

        assert_eq!(writes, nudge_for(&director));
    }

    #[test]
    fn an_enter_the_child_receives_as_a_carriage_return_releases_the_hold() {
        // Alt and Ctrl+Enter reach the child as the same bytes as a plain
        // Enter (`key_encode::key_to_bytes`), so each one sends whatever was
        // typed; a mark that survived them would outlive an empty line.
        for modifiers in [Modifiers::NONE, Modifiers::ALT, Modifiers::CONTROL] {
            let (mut state, mut app, brigade, director, _worker, mut now) =
                two_pane_brigade_focused_on(0);
            for (code, modifiers) in [
                (KeyCode::Char('a'), Modifiers::NONE),
                (KeyCode::Enter, modifiers),
            ] {
                forward_key(&mut state, &mut app, &brigade, code, modifiers, now);
            }
            assert!(
                !state.has_unsent_operator_input(&director),
                "{modifiers:?}+Enter sends, so nothing is left unsent"
            );
            now += RELAY_INPUT_QUIET_PERIOD + Duration::from_secs(1);

            let writes = nudge_ticks(
                &mut state, &mut app, &brigade, "director", &director, &mut now,
            );
            assert_eq!(writes, nudge_for(&director), "{modifiers:?}+Enter");
        }
    }

    #[test]
    fn shift_enter_is_a_newline_so_it_marks_the_pane_and_holds_the_nudge() {
        // It reaches the child as a line feed, not a send, so a line the
        // operator is still composing is exactly what it leaves behind.
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);

        let cmds = forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Enter,
            Modifiers::SHIFT,
            now,
        );

        assert_eq!(
            writes_of(&cmds),
            vec![(director.clone(), vec![10])],
            "a line feed, not a carriage return"
        );
        assert!(state.has_unsent_operator_input(&director));
        now += Duration::from_secs(3600);
        let writes = nudge_ticks(
            &mut state, &mut app, &brigade, "director", &director, &mut now,
        );
        assert!(writes.is_empty(), "{writes:?}");
    }

    #[test]
    fn a_plain_enter_after_a_shift_enter_releases_the_hold() {
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        for (code, modifiers) in [
            (KeyCode::Char('a'), Modifiers::NONE),
            (KeyCode::Enter, Modifiers::SHIFT),
            (KeyCode::Char('b'), Modifiers::NONE),
        ] {
            forward_key(&mut state, &mut app, &brigade, code, modifiers, now);
        }
        assert!(state.has_unsent_operator_input(&director));

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Enter,
            Modifiers::NONE,
            now,
        );

        assert!(!state.has_unsent_operator_input(&director));
    }

    #[test]
    fn a_paste_marks_the_pane() {
        let (mut state, mut app, brigade, director, worker, now) = two_pane_brigade_focused_on(0);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Paste("some text".to_string())),
            now,
        );

        assert!(state.has_unsent_operator_input(&director));
        assert!(
            !state.has_unsent_operator_input(&worker),
            "only the pane the paste went to"
        );
    }

    #[test]
    fn a_paste_with_nothing_in_it_does_not_mark_the_pane() {
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Paste(String::new())),
            now,
        );

        assert!(!state.has_unsent_operator_input(&director));
    }

    #[test]
    fn keys_that_add_no_text_neither_mark_nor_unmark_a_pane() {
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        let neutral = [
            (KeyCode::Left, Modifiers::NONE),
            (KeyCode::Up, Modifiers::NONE),
            (KeyCode::Backspace, Modifiers::NONE),
            (KeyCode::Esc, Modifiers::NONE),
            (KeyCode::F(5), Modifiers::NONE),
            (KeyCode::Char('c'), Modifiers::CONTROL),
        ];

        for (code, modifiers) in neutral {
            forward_key(&mut state, &mut app, &brigade, code, modifiers, now);
        }
        assert!(
            !state.has_unsent_operator_input(&director),
            "arrows, Backspace, Esc and Ctrl+C alone leave nothing to protect"
        );

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        for (code, modifiers) in neutral {
            forward_key(&mut state, &mut app, &brigade, code, modifiers, now);
        }
        assert!(
            state.has_unsent_operator_input(&director),
            "and none of them clears a draft: which keys empty an agent's input \
             line is the agent's business"
        );
    }

    #[test]
    fn the_relays_own_writes_do_not_mark_the_pane() {
        let (state, _app, _brigade, worker, _now) = nudged_focused_pane_awaiting_submit();
        assert!(
            !state.has_unsent_operator_input(&worker),
            "the nudge line is banto's own write, not the operator's draft"
        );
    }

    #[test]
    fn the_focus_report_banto_writes_does_not_mark_the_pane() {
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        // A child that asked for DECSET 1004 is told when it gains focus.
        state
            .screens
            .get_mut(&director)
            .unwrap()
            .process(b"\x1b[?1004h");

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(
            writes_of(&cmds),
            vec![(director.clone(), b"\x1b[I".to_vec())],
            "fixture check: the focus report really was written"
        );
        assert!(!state.has_unsent_operator_input(&director));
    }

    #[test]
    fn an_unsent_draft_survives_discovery_renaming_the_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let old_key = SessionKey::new_worker(1, "worker-1");
        state.screens.insert(old_key.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(old_key.clone());
        state.focus = Focus::Pane;
        let mut now = test_instant();
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );

        let new_key = SessionKey::from_id("w1-real");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: old_key.clone(),
                session_id: "w1-real".to_string(),
                member: None,
            },
            now,
        );

        assert!(state.has_unsent_operator_input(&new_key));
        assert!(!state.has_unsent_operator_input(&old_key));
        now += Duration::from_secs(3600);
        let writes = nudge_ticks(
            &mut state, &mut app, &brigade, "worker-1", &new_key, &mut now,
        );
        assert!(writes.is_empty(), "{writes:?}");
    }

    #[test]
    fn an_unsent_draft_survives_a_compaction_fork_renaming_the_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let old = SessionKey::from_id("w1-old");
        let other = SessionKey::from_id("w2");
        staged_brigade(&mut state, &director, &[old.clone(), other]);
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );

        update(
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

        assert!(state.has_unsent_operator_input(&SessionKey::from_id("w1-new")));
        assert!(!state.has_unsent_operator_input(&old));
    }

    /// One relay tick at `now` with `key` idle, carrying (or not) unread mail.
    fn tick_for(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        token: &str,
        key: &SessionKey,
        has_unseen: bool,
        now: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Tick {
                relay: vec![RelayObservation {
                    glance: None,
                    brigade_id: 1,
                    token: token.to_string(),
                    key: key.clone(),
                    has_unseen,
                    is_idle_this_tick: Some(true),
                    agent: None,
                }],
                brigades: Vec::new(),
            },
            now,
        )
    }

    /// A Director holding a draft while the operator is on the Worker pane,
    /// with mail waiting for it, ticked until the hold is announced.
    /// Returns the state and the instant of the showing.
    fn announced_hold() -> (EmporiumState, App, BrigadeConfig, SessionKey, Instant) {
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        for _ in 0..2 {
            now += Duration::from_secs(1);
            tick_for(
                &mut state, &mut app, &brigade, "director", &director, true, now,
            );
        }
        (state, app, brigade, director, now)
    }

    #[test]
    fn a_nudge_held_back_by_a_draft_is_announced_on_the_status_line() {
        let (state, _app, _brigade, _director, _now) = announced_hold();

        let status = state.status.as_deref().expect("the hold is announced");
        assert!(status.contains("director"), "{status}");
        assert!(status.contains("unsent input"), "{status}");
    }

    #[test]
    fn the_announcement_clears_after_the_status_timeout_and_returns_after_the_gap() {
        let (mut state, mut app, brigade, director, shown) = announced_hold();
        let tick = |state: &mut EmporiumState, app: &mut App, at: Duration| {
            tick_for(
                state,
                app,
                &brigade,
                "director",
                &director,
                true,
                shown + at,
            );
        };

        tick(&mut state, &mut app, STATUS_TIMEOUT);
        assert_eq!(state.status, None, "back to the key guidance");

        tick(
            &mut state,
            &mut app,
            STATUS_TIMEOUT + STATUS_REMINDER_GAP - Duration::from_secs(1),
        );
        assert_eq!(state.status, None, "the gap is for the guidance");

        tick(&mut state, &mut app, STATUS_TIMEOUT + STATUS_REMINDER_GAP);
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("director")),
            "announced again: {:?}",
            state.status
        );
    }

    #[test]
    fn a_draft_with_no_mail_waiting_announces_nothing() {
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }

        for _ in 0..120 {
            now += Duration::from_secs(1);
            tick_for(
                &mut state, &mut app, &brigade, "director", &director, false, now,
            );
            assert_eq!(state.status, None);
        }
    }

    #[test]
    fn an_announcement_never_covers_another_status_and_waits_for_the_line_to_clear() {
        let (mut state, mut app, brigade, director, shown) = announced_hold();
        let before_due = shown + STATUS_TIMEOUT + STATUS_REMINDER_GAP - Duration::from_secs(1);
        state.set_status("failed to open: boom", before_due);

        let due = shown + STATUS_TIMEOUT + STATUS_REMINDER_GAP;
        tick_for(
            &mut state, &mut app, &brigade, "director", &director, true, due,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("failed to open: boom"),
            "due, but the line is taken"
        );

        tick_for(
            &mut state,
            &mut app,
            &brigade,
            "director",
            &director,
            true,
            before_due + STATUS_TIMEOUT,
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("director")),
            "the error expired, so the announcement follows at once: {:?}",
            state.status
        );
    }

    #[test]
    fn the_announcement_stops_when_the_mail_is_gone() {
        let (mut state, mut app, brigade, director, shown) = announced_hold();

        let mut at = shown;
        for _ in 0..90 {
            at += Duration::from_secs(1);
            tick_for(
                &mut state, &mut app, &brigade, "director", &director, false, at,
            );
            // The showing already up runs out on its own; none follows it.
            if at - shown >= STATUS_TIMEOUT {
                assert_eq!(state.status, None, "at +{:?}", at - shown);
            }
        }
    }

    #[test]
    fn the_announcement_stops_when_the_pane_is_gone() {
        let (mut state, mut app, brigade, _director, shown) = announced_hold();

        let mut at = shown;
        for _ in 0..90 {
            at += Duration::from_secs(1);
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Tick {
                    relay: vec![],
                    brigades: Vec::new(),
                },
                at,
            );
            if at - shown >= STATUS_TIMEOUT {
                assert_ne!(
                    state.status.as_deref().map(|s| s.contains("holding")),
                    Some(true),
                    "at +{:?}",
                    at - shown
                );
            }
        }
    }

    #[test]
    fn sending_the_draft_ends_the_announcements() {
        let (mut state, mut app, brigade, director, _worker, mut now) =
            two_pane_brigade_focused_on(0);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        // Still focused, so the quiet period has to pass before the draft is
        // the only thing holding the nudge.
        now += RELAY_INPUT_QUIET_PERIOD + Duration::from_secs(1);
        for _ in 0..2 {
            now += Duration::from_secs(1);
            tick_for(
                &mut state, &mut app, &brigade, "director", &director, true, now,
            );
        }
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("holding")),
            "fixture check: announced while held: {:?}",
            state.status
        );

        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Enter,
            Modifiers::NONE,
            now,
        );
        now += RELAY_INPUT_QUIET_PERIOD + Duration::from_secs(1);
        let mut wrote_nudge = false;
        for _ in 0..180 {
            now += Duration::from_secs(1);
            let cmds = tick_for(
                &mut state, &mut app, &brigade, "director", &director, true, now,
            );
            wrote_nudge |= writes_of(&cmds)
                .contains(&(director.clone(), RELAY_NUDGE_LINE.as_bytes().to_vec()));
            assert!(
                !state
                    .status
                    .as_deref()
                    .is_some_and(|s| s.contains("holding")),
                "no announcement once the draft is sent: {:?}",
                state.status
            );
        }
        assert!(wrote_nudge, "and the nudge itself went out");
    }

    #[test]
    fn a_status_reminder_waits_for_a_free_line_without_skipping_its_showing() {
        let mut reminder = StatusReminder::default();
        let t0 = test_instant();

        assert!(
            !reminder.due(false, true, t0),
            "no condition, nothing to say"
        );
        assert!(!reminder.due(true, false, t0), "line taken");
        assert!(reminder.due(true, true, t0), "first showing is immediate");
        assert!(
            !reminder.due(true, true, t0 + STATUS_TIMEOUT),
            "inside the cycle"
        );
        let next = t0 + STATUS_TIMEOUT + STATUS_REMINDER_GAP;
        assert!(
            !reminder.due(true, false, next),
            "due, but the line is taken"
        );
        assert!(
            reminder.due(true, true, next + Duration::from_secs(3)),
            "pending until free"
        );

        assert!(!reminder.due(false, true, next + Duration::from_secs(4)));
        assert!(
            reminder.due(true, true, next + Duration::from_secs(5)),
            "a condition that returns shows at once"
        );
    }

    // --- reading the input line off the pane's screen ---------------------

    /// Draw a Claude-shaped prompt block at the bottom of `screen`: a
    /// full-width rule, `❯` + no-break space + either `typed` (normal
    /// attributes) or a dim placeholder, a rule, a footer row, and the cursor
    /// after what is on the prompt row. The shape `crate::input_line` reads;
    /// hand-made, nothing recorded.
    fn draw_prompt_on(screen: &mut Screen, typed: Option<&str>) {
        let (rows, cols) = screen.screen().size();
        let rule = "\u{2500}".repeat(cols as usize);
        let p = rows - 3; // 1-based prompt row; the footer is the last row but one
        let body = match typed {
            Some(text) => text.to_string(),
            None => "\x1b[2mTry something\x1b[0m".to_string(),
        };
        let cursor_col = 3 + typed.map_or(0, |text| text.chars().count());
        let bytes = format!(
            "\x1b[2J\x1b[{};1H{rule}\x1b[{p};1H\u{276f}\u{a0}{body}\x1b[{};1H{rule}\x1b[{};1H  footer\x1b[{p};{cursor_col}H",
            p - 1,
            p + 1,
            p + 2,
        );
        screen.process(bytes.as_bytes());
    }

    fn draw_prompt(state: &mut EmporiumState, key: &SessionKey, typed: Option<&str>) {
        draw_prompt_on(state.screens.get_mut(key).unwrap(), typed);
    }

    /// The Director pane's screen is laid out for real (a 160x50 terminal)
    /// so the prompt drawn on it is the size the pane has, and focus is on
    /// the Worker so the 30s typing quiet period does not apply to the
    /// Director — leaving the draft rules as the only thing under test.
    fn reading_fixture() -> (EmporiumState, App, BrigadeConfig, SessionKey, Instant) {
        let (mut state, app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        state.size = (160, 50);
        resize_staged_tiles(&mut state);
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        (state, app, brigade, director, now)
    }

    /// What the shell reports about one member for a tick, beyond its key.
    #[derive(Clone, Copy)]
    struct Seen {
        agent: Option<AgentKind>,
        idle: Option<bool>,
        has_unseen: bool,
    }

    /// One relay tick for `key` with the given observations.
    fn tick_obs(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
        seen: Seen,
        now: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Tick {
                relay: vec![RelayObservation {
                    glance: None,
                    brigade_id: 1,
                    token: "director".to_string(),
                    key: key.clone(),
                    has_unseen: seen.has_unseen,
                    is_idle_this_tick: seen.idle,
                    agent: seen.agent,
                }],
                brigades: Vec::new(),
            },
            now,
        )
    }

    /// Two idle ticks a second apart (the debounce) for `key` as `agent`,
    /// advancing `now`; every PTY write they produced.
    fn two_ticks_as(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
        agent: Option<AgentKind>,
        now: &mut Instant,
    ) -> Vec<(SessionKey, Vec<u8>)> {
        let mut writes = Vec::new();
        for _ in 0..2 {
            *now += Duration::from_secs(1);
            let cmds = tick_obs(
                state,
                app,
                brigade,
                key,
                Seen {
                    agent,
                    idle: Some(true),
                    has_unseen: true,
                },
                *now,
            );
            writes.extend(writes_of(&cmds));
        }
        writes
    }

    const CLAUDE: Option<AgentKind> = Some(AgentKind::ClaudeCode);

    #[test]
    fn text_on_the_screen_holds_the_nudge_even_though_banto_saw_no_typing() {
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        draw_prompt(&mut state, &director, Some("recalled"));
        assert!(!state.has_unsent_operator_input(&director), "no keystroke");

        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);

        assert!(writes.is_empty(), "{writes:?}");
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.contains("holding the nudge")),
            "the hold is announced like any other: {:?}",
            state.status
        );
    }

    #[test]
    fn an_empty_screen_drops_a_stale_mark_and_lets_the_nudge_through() {
        // The mark is the stand-in for exactly this: the operator typed, then
        // cleared the line by a route banto does not model (Ctrl+C here).
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        // The operator's focus was the Director when they typed; it has since
        // moved to the Worker (see the fixture).
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        // (the key above went to the Director pane only if focus was there)
        state.unsent_operator_input.insert(director.clone());
        draw_prompt(&mut state, &director, None);
        now += INPUT_ECHO_SETTLE + Duration::from_secs(1);

        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);

        assert_eq!(writes, nudge_for(&director));
        assert!(
            !state.has_unsent_operator_input(&director),
            "the mark is gone"
        );
    }

    #[test]
    fn an_empty_screen_just_after_a_keystroke_is_not_believed() {
        // Measured: the echo of a key reaches the screen 36-108ms later, and
        // until then the screen still shows the empty prompt.
        let (mut state, mut app, brigade, director, _) = reading_fixture();
        let typed_at = test_instant();
        state.unsent_operator_input.insert(director.clone());
        state
            .last_forwarded_input
            .insert(director.clone(), typed_at);
        draw_prompt(&mut state, &director, None);
        let mut now = typed_at;

        let early = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);
        assert!(early.is_empty(), "inside the settle window: {early:?}");
        assert!(state.has_unsent_operator_input(&director));

        now = typed_at + INPUT_ECHO_SETTLE;
        let later = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);
        assert_eq!(
            later,
            nudge_then_enter_for(&director),
            "past it, the empty screen is believed"
        );
        assert!(!state.has_unsent_operator_input(&director));
    }

    #[test]
    fn an_unreadable_screen_leaves_the_keystroke_mark_in_charge() {
        // A blank screen: no prompt block to read.
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        state.unsent_operator_input.insert(director.clone());
        let held = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);
        assert!(held.is_empty(), "marked: {held:?}");

        state.unsent_operator_input.remove(&director);
        let sent = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);
        assert_eq!(sent, nudge_then_enter_for(&director), "unmarked");
    }

    #[test]
    fn a_codex_pane_is_always_judged_by_the_mark_alone() {
        let codex = Some(AgentKind::Codex);
        // Text drawn like Claude's, no mark: the screen is not read, so the
        // nudge goes.
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        draw_prompt(&mut state, &director, Some("text"));
        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, codex, &mut now);
        assert_eq!(writes, nudge_for(&director));

        // A mark and an empty-looking screen: the mark stands.
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        state.unsent_operator_input.insert(director.clone());
        draw_prompt(&mut state, &director, None);
        now += INPUT_ECHO_SETTLE + Duration::from_secs(1);
        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, codex, &mut now);
        assert!(writes.is_empty(), "{writes:?}");
        assert!(state.has_unsent_operator_input(&director));
    }

    #[test]
    fn a_member_with_no_recorded_product_is_judged_by_the_mark_alone() {
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        draw_prompt(&mut state, &director, Some("text"));
        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, None, &mut now);
        assert_eq!(writes, nudge_for(&director));
    }

    #[test]
    fn a_pending_enter_still_trusts_only_the_mark_not_the_screen() {
        // By the time the delayed Enter is due, the line holds banto's own
        // nudge sentence, which reads as text; that must not stop the Enter.
        let (mut state, mut app, brigade, director, mut now) = reading_fixture();
        let writes = two_ticks_as(&mut state, &mut app, &brigade, &director, CLAUDE, &mut now);
        assert_eq!(writes, nudge_for(&director));
        draw_prompt(&mut state, &director, Some("[banto relay] nudge text"));
        assert!(!state.has_unsent_operator_input(&director));

        now += RELAY_SUBMIT_DELAY;
        let cmds = tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Seen {
                agent: CLAUDE,
                idle: Some(false),
                has_unseen: true,
            },
            now,
        );

        assert_eq!(writes_of(&cmds), vec![(director.clone(), b"\r".to_vec())]);
    }

    #[test]
    fn an_idle_claude_pane_that_stays_unreadable_is_reported_once() {
        let (mut state, mut app, brigade, director, start) = reading_fixture();
        let tick = |state: &mut EmporiumState, app: &mut App, idle: Option<bool>, at: Duration| {
            tick_obs(
                state,
                app,
                &brigade,
                &director,
                Seen {
                    agent: CLAUDE,
                    idle,
                    has_unseen: false,
                },
                start + at,
            );
        };

        tick(&mut state, &mut app, Some(true), Duration::ZERO);
        tick(
            &mut state,
            &mut app,
            Some(true),
            INPUT_LINE_UNREADABLE_AFTER - Duration::from_secs(1),
        );
        assert!(
            !state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("cannot read")),
            "not yet: {:?}",
            state.status
        );

        tick(
            &mut state,
            &mut app,
            Some(true),
            INPUT_LINE_UNREADABLE_AFTER,
        );
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("cannot read")),
            "{:?}",
            state.status
        );

        // The status runs out and nothing says it again.
        for extra in [5, 40, 400] {
            tick(
                &mut state,
                &mut app,
                Some(true),
                INPUT_LINE_UNREADABLE_AFTER + Duration::from_secs(extra),
            );
        }
        assert!(
            !state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("cannot read")),
            "said once: {:?}",
            state.status
        );
    }

    #[test]
    fn an_unreadable_screen_while_waiting_or_busy_is_not_reported() {
        // A permission dialog replaces the input block; that is normal.
        for idle in [Some(false), None] {
            let (mut state, mut app, brigade, director, start) = reading_fixture();
            for secs in [0, 30, 60, 120, 600] {
                tick_obs(
                    &mut state,
                    &mut app,
                    &brigade,
                    &director,
                    Seen {
                        agent: CLAUDE,
                        idle,
                        has_unseen: false,
                    },
                    start + Duration::from_secs(secs),
                );
                assert!(
                    !state
                        .status
                        .as_deref()
                        .is_some_and(|s| s.contains("cannot read")),
                    "idle={idle:?} at +{secs}s: {:?}",
                    state.status
                );
            }
        }
    }

    #[test]
    fn a_readable_screen_resets_the_unreadable_clock() {
        let (mut state, mut app, brigade, director, start) = reading_fixture();
        tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Seen {
                agent: CLAUDE,
                idle: Some(true),
                has_unseen: false,
            },
            start,
        );
        draw_prompt(&mut state, &director, None);
        tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Seen {
                agent: CLAUDE,
                idle: Some(true),
                has_unseen: false,
            },
            start + INPUT_LINE_UNREADABLE_AFTER - Duration::from_secs(1),
        );
        // Unreadable again from here; the minute starts over.
        state
            .screens
            .get_mut(&director)
            .unwrap()
            .process(b"\x1b[2J");
        tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Seen {
                agent: CLAUDE,
                idle: Some(true),
                has_unseen: false,
            },
            start + INPUT_LINE_UNREADABLE_AFTER,
        );
        assert!(
            !state
                .status
                .as_deref()
                .is_some_and(|s| s.contains("cannot read")),
            "{:?}",
            state.status
        );
    }

    // --- Goinkyo kickoff retry: Enter alone when the sentence is still typed -

    fn goinkyo_kickoff_retry_state(
        screen_setup: impl FnOnce(&mut Screen),
        sent: bool,
    ) -> (EmporiumState, App, BrigadeConfig, SessionKey, Instant) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::from_id("goinkyo-real");
        let t0 = test_instant();
        let mut screen = Screen::new(24, 80);
        screen_setup(&mut screen);
        state.screens.insert(key.clone(), screen);
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: key.clone(),
            spawned_at: t0,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: sent.then_some(t0),
            attempts: u32::from(sent),
        });
        (state, app, brigade, key, t0)
    }

    fn goinkyo_trust_answer(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
        at: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: key.clone(),
                trusted: true,
            },
            at,
        )
    }

    #[test]
    fn a_retried_goinkyo_kickoff_presses_enter_alone_when_its_sentence_is_still_typed() {
        let (mut state, mut app, brigade, key, t0) = goinkyo_kickoff_retry_state(
            |screen| draw_prompt_on(screen, Some(GOINKYO_KICKOFF_LINE)),
            true,
        );

        let cmds = goinkyo_trust_answer(
            &mut state,
            &mut app,
            &brigade,
            &key,
            t0 + KICKOFF_ACK_TIMEOUT,
        );

        assert_eq!(
            cmds.len(),
            1,
            "just the Enter, not the sentence again: {cmds:?}"
        );
        assert_eq!(writes_of(&cmds), vec![(key.clone(), b"\r".to_vec())]);
        assert!(state.pending_submits.is_empty(), "no delayed Enter queued");
        assert_eq!(state.pending_goinkyo_kickoffs[0].attempts, 2);
    }

    #[test]
    fn a_retried_goinkyo_kickoff_types_the_sentence_again_when_it_is_not_on_the_line() {
        let sentence = GOINKYO_KICKOFF_LINE.as_bytes().to_vec();
        for (name, screen_setup) in [
            (
                "empty line",
                Box::new(|screen: &mut Screen| draw_prompt_on(screen, None))
                    as Box<dyn FnOnce(&mut Screen)>,
            ),
            (
                "unreadable screen",
                Box::new(|_: &mut Screen| {}) as Box<dyn FnOnce(&mut Screen)>,
            ),
            (
                "some other text",
                Box::new(|screen: &mut Screen| draw_prompt_on(screen, Some("something else")))
                    as Box<dyn FnOnce(&mut Screen)>,
            ),
        ] {
            let (mut state, mut app, brigade, key, t0) =
                goinkyo_kickoff_retry_state(screen_setup, true);
            let cmds = goinkyo_trust_answer(
                &mut state,
                &mut app,
                &brigade,
                &key,
                t0 + KICKOFF_ACK_TIMEOUT,
            );
            assert_eq!(
                writes_of(&cmds),
                vec![(key.clone(), sentence.clone())],
                "{name}"
            );
            assert_eq!(state.pending_submits.len(), 1, "{name}: its Enter follows");
        }
    }

    #[test]
    fn a_first_goinkyo_kickoff_is_not_shortened_by_what_the_screen_shows() {
        let (mut state, mut app, brigade, key, t0) = goinkyo_kickoff_retry_state(
            |screen| draw_prompt_on(screen, Some(GOINKYO_KICKOFF_LINE)),
            false,
        );

        let cmds = goinkyo_trust_answer(&mut state, &mut app, &brigade, &key, t0);

        assert_eq!(
            writes_of(&cmds),
            vec![(key.clone(), GOINKYO_KICKOFF_LINE.as_bytes().to_vec())]
        );
    }

    #[test]
    fn a_retried_goinkyo_kickoff_does_nothing_over_the_operators_own_draft() {
        let (mut state, mut app, brigade, key, t0) = goinkyo_kickoff_retry_state(
            |screen| draw_prompt_on(screen, Some(GOINKYO_KICKOFF_LINE)),
            true,
        );
        state.unsent_operator_input.insert(key.clone());

        let cmds = goinkyo_trust_answer(
            &mut state,
            &mut app,
            &brigade,
            &key,
            t0 + KICKOFF_ACK_TIMEOUT,
        );

        assert!(cmds.is_empty(), "{cmds:?}");
    }

    // --- dragging a boundary --------------------------------------------------

    const DRAG_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 120,
        height: 40,
    };

    /// A 120x40 terminal showing brigade 1: the Director plus `workers`
    /// Workers, every pane with a screen and a member token, focus on the
    /// Director's pane. Sizes settled with one `Resized`.
    fn drag_fixture(workers: usize) -> (EmporiumState, App, BrigadeConfig, Instant) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut panes = vec![SessionKey::from_id("dir")];
        state
            .member_tokens
            .insert(panes[0].clone(), DIRECTOR_TOKEN.to_string());
        for i in 1..=workers {
            let key = SessionKey::from_id(&format!("w{i}"));
            state
                .member_tokens
                .insert(key.clone(), format!("worker-{i}"));
            panes.push(key);
        }
        for key in &panes {
            state.screens.insert(key.clone(), Screen::new(24, 80));
        }
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(panes[0].clone()),
            panes,
            focused: 0,
        };
        state.focus = Focus::Pane;
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 120,
                height: 40,
            },
            now,
        );
        (state, app, brigade, now)
    }

    fn mouse_event(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        kind: MouseEventKind,
        column: u16,
        row: u16,
        now: Instant,
    ) -> Vec<Cmd> {
        update(
            state,
            app,
            brigade,
            Event::Input(InputEvent::Mouse(MouseEvent { kind, column, row })),
            now,
        )
    }

    /// The line the overlay would draw, if any.
    fn overlay_line(state: &EmporiumState) -> Option<(Rect, Axis)> {
        state
            .drag_overlay(DRAG_AREA)
            .and_then(|overlay| overlay.line)
    }

    fn handle_of(state: &EmporiumState, wanted: &Boundary) -> Handle {
        state
            .geometry(DRAG_AREA)
            .handle_for(wanted)
            .expect("the boundary exists")
            .clone()
    }

    /// A cell on `handle` that starts a drag, and the position along its axis.
    fn grab_point(handle: &Handle) -> (u16, u16) {
        match handle.axis {
            Axis::Vertical => (handle.position, 20),
            Axis::Horizontal => (handle.hits()[0].x + 2, handle.position - 1),
        }
    }

    /// Press on `boundary`, drag (left button) to `to` along its axis and
    /// leave the button down. Returns the cmds of every event.
    fn drag_to(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        boundary: &Boundary,
        to: u16,
        now: Instant,
    ) -> Vec<Cmd> {
        let handle = handle_of(state, boundary);
        let (column, row) = grab_point(&handle);
        let mut cmds = mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            now,
        );
        let (c, r) = match handle.axis {
            Axis::Vertical => (
                to.saturating_sub(handle.position.saturating_sub(column)),
                row,
            ),
            Axis::Horizontal => (
                column,
                to.saturating_sub(handle.position.saturating_sub(row)),
            ),
        };
        cmds.extend(mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Drag(MouseButton::Left),
            c,
            r,
            now,
        ));
        cmds
    }

    fn release_at(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        boundary: &Boundary,
        to: u16,
        now: Instant,
    ) -> Vec<Cmd> {
        let handle = handle_of(state, boundary);
        let (column, row) = grab_point(&handle);
        let (c, r) = match handle.axis {
            Axis::Vertical => (
                to.saturating_sub(handle.position.saturating_sub(column)),
                row,
            ),
            Axis::Horizontal => (
                column,
                to.saturating_sub(handle.position.saturating_sub(row)),
            ),
        };
        mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Up(MouseButton::Left),
            c,
            r,
            now,
        )
    }

    fn resizes(cmds: &[Cmd]) -> Vec<(SessionKey, u16, u16)> {
        let mut resized: Vec<_> = cmds
            .iter()
            .filter_map(|cmd| match cmd {
                Cmd::ResizePty { key, rows, cols } => Some((key.clone(), *rows, *cols)),
                _ => None,
            })
            .collect();
        resized.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        resized
    }

    fn sizes(state: &EmporiumState) -> Vec<(String, (u16, u16))> {
        let mut sizes: Vec<_> = state
            .screens
            .iter()
            .map(|(key, screen)| (key.as_str().to_string(), screen.screen().size()))
            .collect();
        sizes.sort();
        sizes
    }

    fn stack_height_sum(state: &EmporiumState) -> u16 {
        state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .skip(1)
            .map(|(_, rect)| rect.height)
            .sum()
    }

    #[test]
    fn the_default_geometry_is_the_old_one_for_one_to_four_panes() {
        for workers in 0..=3 {
            let (state, _, _, _) = drag_fixture(workers);
            let geometry = state.geometry(DRAG_AREA);
            assert_eq!(geometry.areas.sidebar.width, 36, "{workers} workers");
            let (panes, total) = (geometry.areas.pane, 1 + workers);
            let [master, stack] = ratatui_core::layout::Layout::horizontal([
                ratatui_core::layout::Constraint::Percentage(50),
                ratatui_core::layout::Constraint::Percentage(50),
            ])
            .areas(panes);
            if total == 1 {
                assert_eq!(geometry.tiles[0].1, panes);
                continue;
            }
            assert_eq!(geometry.tiles[0].1, master);
            let rows = ratatui_core::layout::Layout::vertical(vec![
                ratatui_core::layout::Constraint::Ratio(1, workers as u32);
                workers
            ])
            .split(stack);
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(geometry.tiles[1 + i].1, *row, "{workers} workers, tile {i}");
            }
        }
    }

    #[test]
    fn a_press_on_each_handle_starts_a_drag_and_nothing_else_does() {
        let (state, _, _, _) = drag_fixture(3);
        let boundaries: Vec<Boundary> = state
            .geometry(DRAG_AREA)
            .handles()
            .iter()
            .map(|h| h.boundary.clone())
            .collect();
        assert_eq!(boundaries.len(), 4);
        for boundary in boundaries {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            let handle = handle_of(&state, &boundary);
            let (column, row) = grab_point(&handle);
            mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                now,
            );
            assert_eq!(
                state.drag.as_ref().map(|d| d.boundary.clone()),
                Some(boundary),
            );
        }

        let geometry = state.geometry(DRAG_AREA);
        let lower = geometry.tiles[2].1;
        let not_handles = [
            ("a stack tile's title row", lower.x + 5, lower.y),
            ("a tile's interior", lower.x + 5, lower.y + 1),
            ("the master's title row", geometry.tiles[0].1.x + 5, 0),
            ("the sidebar's interior", 10, 10),
        ];
        for (what, column, row) in not_handles {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                now,
            );
            assert!(state.drag.is_none(), "{what}");
        }
        // Only the left button picks a boundary up.
        let handle = geometry.handles()[0].clone();
        let (column, row) = grab_point(&handle);
        for button in [MouseButton::Right, MouseButton::Middle] {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(button),
                column,
                row,
                now,
            );
            assert!(state.drag.is_none(), "{button:?}");
        }
    }

    fn w1_over_w2() -> Boundary {
        Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        }
    }

    /// Every cell a press can pick `boundary` up at that is worth trying: for
    /// a vertical handle both border columns, for the stack boundary both rows.
    fn presses_on(state: &EmporiumState, boundary: &Boundary) -> Vec<(u16, u16)> {
        let handle = handle_of(state, boundary);
        match handle.axis {
            Axis::Vertical => vec![(handle.position - 1, 20), (handle.position, 20)],
            // One press per row of the gap (the lower row's last cell, the
            // right corner, is a second rectangle on the same row).
            Axis::Horizontal => {
                let mut rows: Vec<(u16, u16)> = Vec::new();
                for hit in handle.hits() {
                    if rows.iter().all(|(_, y)| *y != hit.y) {
                        rows.push((hit.x + 2.min(hit.width - 1), hit.y));
                    }
                }
                rows
            }
        }
    }

    /// Press at `at`, drag by (`dx`, `dy`) from there, release there; the
    /// tiles that result.
    fn press_drag_release(
        at: (u16, u16),
        by: (i32, i32),
        boundary: &Boundary,
        workers: usize,
    ) -> (EmporiumState, Vec<(SessionKey, Rect)>) {
        let (mut state, mut app, brigade, now) = drag_fixture(workers);
        let to = (
            (i32::from(at.0) + by.0) as u16,
            (i32::from(at.1) + by.1) as u16,
        );
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            at.0,
            at.1,
            now,
        );
        assert!(
            state.drag.is_some(),
            "a press at {at:?} picks {boundary:?} up"
        );
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            to.0,
            to.1,
            now,
        );
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            to.0,
            to.1,
            now,
        );
        let tiles = state.geometry(DRAG_AREA).tiles;
        (state, tiles)
    }

    #[test]
    fn grabbing_the_stack_boundary_on_either_row_commits_the_same_geometry() {
        let (probe, _, _, _) = drag_fixture(3);
        let boundary = w1_over_w2();
        let presses = presses_on(&probe, &boundary);
        assert_eq!(presses.len(), 2, "the upper row and the lower row");
        assert_eq!(presses[1].1, presses[0].1 + 1, "adjacent rows");

        let before = probe.geometry(DRAG_AREA).tiles;
        for dy in [-3, 4] {
            let committed: Vec<_> = presses
                .iter()
                .map(|press| press_drag_release(*press, (0, dy), &boundary, 3).1)
                .collect();
            assert_eq!(
                committed[0], committed[1],
                "same movement, same result (dy {dy})"
            );
            let moved_by = i32::from(committed[0][1].1.height) - i32::from(before[1].1.height);
            assert_eq!(
                moved_by, dy,
                "the boundary moved exactly as far as the pointer"
            );
        }
    }

    #[test]
    fn grabbing_any_handle_on_any_of_its_cells_does_not_move_the_preview() {
        let (probe, _, _, _) = drag_fixture(3);
        for boundary in [
            Boundary::Sidebar,
            Boundary::Master { brigade: 1 },
            w1_over_w2(),
        ] {
            let handle = handle_of(&probe, &boundary);
            for press in presses_on(&probe, &boundary) {
                let (mut state, mut app, brigade, now) = drag_fixture(3);
                mouse_event(
                    &mut state,
                    &mut app,
                    &brigade,
                    MouseEventKind::Down(MouseButton::Left),
                    press.0,
                    press.1,
                    now,
                );
                assert_eq!(
                    state.drag.as_ref().unwrap().preview,
                    handle.position,
                    "{boundary:?} pressed at {press:?}: the line is where the boundary is"
                );
                // A drag event on the press cell: still no jump.
                mouse_event(
                    &mut state,
                    &mut app,
                    &brigade,
                    MouseEventKind::Drag(MouseButton::Left),
                    press.0,
                    press.1,
                    now,
                );
                assert_eq!(state.drag.as_ref().unwrap().preview, handle.position);
                // And movement is relative to the press, along the axis.
                let step = |dx: i32, dy: i32| {
                    (
                        (i32::from(press.0) + dx) as u16,
                        (i32::from(press.1) + dy) as u16,
                    )
                };
                let target = match handle.axis {
                    Axis::Vertical => step(2, 0),
                    Axis::Horizontal => step(0, 2),
                };
                mouse_event(
                    &mut state,
                    &mut app,
                    &brigade,
                    MouseEventKind::Drag(MouseButton::Left),
                    target.0,
                    target.1,
                    now,
                );
                assert_eq!(
                    state.drag.as_ref().unwrap().preview,
                    handle.position + 2,
                    "{boundary:?} pressed at {press:?}"
                );
            }
        }
    }

    #[test]
    fn a_press_and_release_on_the_same_cell_of_either_row_stores_nothing() {
        let (probe, _, _, _) = drag_fixture(3);
        for press in presses_on(&probe, &w1_over_w2()) {
            let (state, tiles) = press_drag_release(press, (0, 0), &w1_over_w2(), 3);
            assert!(state.brigade_layouts.is_empty(), "{press:?}");
            assert_eq!(tiles, probe.geometry(DRAG_AREA).tiles);
        }
    }

    #[test]
    fn a_press_on_a_tiles_title_label_starts_no_resize_and_focuses_the_tile() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let lower = state.geometry(DRAG_AREA).tiles[2].1;
        assert_eq!(state.tile_label(&SessionKey::from_id("w2")), "worker-2");
        state.focus = Focus::Sidebar;

        // " worker-2 " is ten cells after the left corner.
        for column in [lower.x + 1, lower.x + 5, lower.x + 10] {
            mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                column,
                lower.y,
                now,
            );
            assert!(
                state.drag.is_none(),
                "column {column} is the label's, not the boundary's"
            );
            // The label is what a tile is picked up by (stage 2): the press
            // that focuses it also starts a move.
            assert!(state.move_drag.is_some());
            state.move_drag = None;
            assert_eq!(state.focus, Focus::Pane);
            assert!(
                matches!(&state.stage, Stage::Brigade { focused: 2, .. }),
                "focuses w2"
            );
            state.focus = Focus::Sidebar;
            if let Stage::Brigade { focused, .. } = &mut state.stage {
                *focused = 0;
            }
        }
        // The first cell past the label is the boundary's again.
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            lower.x + 11,
            lower.y,
            now,
        );
        assert!(state.drag.is_some());
    }

    // --- moving a tile within the stack ---------------------------------------

    fn order_of(state: &EmporiumState) -> Vec<String> {
        match &state.stage {
            Stage::Brigade { panes, .. } => panes.iter().map(|k| k.as_str().to_string()).collect(),
            _ => Vec::new(),
        }
    }

    fn focused_name(state: &EmporiumState) -> Option<String> {
        state.stage.focused_key().map(|k| k.as_str().to_string())
    }

    fn tile_of(state: &EmporiumState, name: &str) -> Rect {
        state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .find(|(key, _)| key.as_str() == name)
            .map(|(_, rect)| *rect)
            .expect("the tile is staged")
    }

    /// A cell on `name`'s title label.
    fn label_cell(state: &EmporiumState, name: &str) -> (u16, u16) {
        let tile = tile_of(state, name);
        (tile.x + 2, tile.y)
    }

    /// Give `worker-1..=n` distinct row weights, so a tile's height says
    /// whose it is.
    fn weigh_workers(state: &mut EmporiumState, workers: usize) {
        let mut layout = BrigadeLayout::default();
        for n in 1..=workers {
            layout
                .row_weights
                .insert(format!("worker-{n}"), (n * 10) as u16);
        }
        state.brigade_layouts.insert(1, layout);
    }

    fn press_label(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        now: Instant,
    ) -> Vec<Cmd> {
        let (column, row) = label_cell(state, name);
        mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            now,
        )
    }

    /// Press `name`'s label, drag to `to_row` (the column wanders off over
    /// the master), release there. Returns every command.
    fn move_with_mouse(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        to_row: u16,
        now: Instant,
    ) -> Vec<Cmd> {
        let mut cmds = press_label(state, app, brigade, name, now);
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            cmds.extend(mouse_event(state, app, brigade, kind, 10, to_row, now));
        }
        cmds
    }

    /// Move `name` onto the slot of the tile `onto`: press its label, drag to
    /// a row inside the target, release there.
    fn move_onto(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        onto: &str,
        now: Instant,
    ) -> Vec<Cmd> {
        let row = tile_of(state, onto).y + 1;
        move_with_mouse(state, app, brigade, name, row, now)
    }

    /// `order` with the element at stack position `from` moved to `to`.
    fn moved(order: &[&str], from: usize, to: usize) -> Vec<String> {
        let mut order: Vec<String> = order.iter().map(|s| s.to_string()).collect();
        let tile = order.remove(from);
        order.insert(to, tile);
        order
    }

    #[test]
    fn dragging_a_tile_onto_any_other_slot_gives_the_expected_order() {
        for workers in [3usize, 4] {
            let names: Vec<String> = (1..=workers).map(|n| format!("w{n}")).collect();
            let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
            for from in 0..workers {
                for to in (0..workers).filter(|to| *to != from) {
                    let (mut state, mut app, brigade, now) = drag_fixture(workers);
                    weigh_workers(&mut state, workers);
                    let weights_before = state.brigade_layouts[&1].row_weights.clone();
                    let target_row = tile_of(&state, &names[to]).y + 1;

                    move_with_mouse(
                        &mut state,
                        &mut app,
                        &brigade,
                        &names[from],
                        target_row,
                        now,
                    );

                    let mut expected = vec!["dir".to_string()];
                    expected.extend(moved(&names_ref, from, to));
                    assert_eq!(
                        order_of(&state),
                        expected,
                        "{workers} workers, tile {from} onto slot {to}"
                    );
                    assert_eq!(state.focus, Focus::Pane);
                    assert_eq!(
                        focused_name(&state).as_deref(),
                        Some(names[from].as_str()),
                        "focus follows the moved tile"
                    );
                    assert_eq!(
                        state.brigade_layouts[&1].row_weights, weights_before,
                        "a move rewrites no weight"
                    );
                    // The weights travel with their tiles: top to bottom they
                    // are the moved order's weights.
                    let expected_weights: Vec<u16> = moved(&names_ref, from, to)
                        .iter()
                        .map(|name| name[1..].parse::<u16>().unwrap() * 10)
                        .collect();
                    assert_eq!(state.stack_weights(), expected_weights);
                    assert!(state.move_drag.is_none());
                }
            }
        }
    }

    #[test]
    fn dragging_below_the_stack_drops_last_and_a_press_alone_changes_nothing() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        // Row 39 is the status bar, below every tile.
        move_with_mouse(&mut state, &mut app, &brigade, "w1", 39, now);
        assert_eq!(order_of(&state), ["dir", "w2", "w3", "w1"]);

        let (mut state, mut app, brigade, now) = drag_fixture(3);
        press_label(&mut state, &mut app, &brigade, "w2", now);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            10,
            39,
            now,
        );
        // w2 is not last, so that was a move to the end.
        assert_eq!(order_of(&state), ["dir", "w1", "w3", "w2"]);
    }

    #[test]
    fn releasing_over_the_tiles_own_slot_changes_nothing() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let own = tile_of(&state, "w2");
        let before = order_of(&state);

        press_label(&mut state, &mut app, &brigade, "w2", now);
        assert_eq!(overlay_line(&state), None, "nothing to preview yet");
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            70,
            own.y + 3,
            now,
        );
        assert_eq!(overlay_line(&state), None, "still its own slot");
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            70,
            own.y + 3,
            now,
        );

        assert_eq!(order_of(&state), before);
        assert!(
            state.brigade_layouts.is_empty(),
            "no preference was recorded"
        );
        assert_eq!(
            focused_name(&state).as_deref(),
            Some("w2"),
            "the press's focus stays"
        );
    }

    #[test]
    fn a_move_shows_a_line_at_the_edge_where_the_tile_would_land_and_a_hint() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let (w1, w3) = (tile_of(&state, "w1"), tile_of(&state, "w3"));
        assert_eq!(state.displayed_status(), None);

        press_label(&mut state, &mut app, &brigade, "w2", now);
        assert_eq!(
            state.displayed_status(),
            Some("move: release to drop · Esc cancels")
        );
        // Up: the target's top edge, across the stack.
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            5,
            w1.y + 2,
            now,
        );
        assert_eq!(
            overlay_line(&state),
            Some((Rect::new(w1.x, w1.y, w1.width, 1), Axis::Horizontal))
        );
        // Down: the target's bottom edge.
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            5,
            w3.y + 2,
            now,
        );
        assert_eq!(
            overlay_line(&state),
            Some((
                Rect::new(w3.x, w3.y + w3.height - 1, w3.width, 1),
                Axis::Horizontal
            ))
        );

        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            5,
            w3.y + 2,
            now,
        );
        assert_eq!(overlay_line(&state), None);
        assert_eq!(state.displayed_status(), None);
    }

    fn drag_to_cell(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        column: u16,
        row: u16,
        now: Instant,
    ) {
        mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Drag(MouseButton::Left),
            column,
            row,
            now,
        );
    }

    #[test]
    fn a_bare_press_marks_the_moved_tile_and_shows_no_ghost_and_no_line() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        assert_eq!(
            state.drag_overlay(DRAG_AREA),
            None,
            "nothing before the press"
        );

        press_label(&mut state, &mut app, &brigade, "w2", now);

        assert_eq!(
            state.drag_overlay(DRAG_AREA),
            Some(DragOverlay {
                line: None,
                marked: Some(SessionKey::from_id("w2")),
                ghost: None,
            })
        );
    }

    #[test]
    fn the_first_drag_adds_a_ghost_at_the_pointer_with_or_without_a_landing_line() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let (w1, w2) = (tile_of(&state, "w1"), tile_of(&state, "w2"));
        press_label(&mut state, &mut app, &brigade, "w2", now);

        // Over its own slot: marked and ghosted, no line to land at.
        drag_to_cell(&mut state, &mut app, &brigade, 70, w2.y + 3, now);
        assert_eq!(
            state.drag_overlay(DRAG_AREA),
            Some(DragOverlay {
                line: None,
                marked: Some(SessionKey::from_id("w2")),
                ghost: Some(Ghost {
                    text: " worker-2 ".to_string(),
                    x: 70,
                    y: w2.y + 3,
                }),
            })
        );

        // Over another slot: the same plus the line.
        drag_to_cell(&mut state, &mut app, &brigade, 50, w1.y + 2, now);
        assert_eq!(
            state.drag_overlay(DRAG_AREA),
            Some(DragOverlay {
                line: Some((Rect::new(w1.x, w1.y, w1.width, 1), Axis::Horizontal)),
                marked: Some(SessionKey::from_id("w2")),
                ghost: Some(Ghost {
                    text: " worker-2 ".to_string(),
                    x: 50,
                    y: w1.y + 2,
                }),
            })
        );

        // The column does not choose the slot, only where the ghost is.
        let line = overlay_line(&state);
        drag_to_cell(&mut state, &mut app, &brigade, 5, w1.y + 2, now);
        let overlay = state.drag_overlay(DRAG_AREA).unwrap();
        assert_eq!(overlay.line, line);
        assert_eq!(overlay.ghost.map(|g| (g.x, g.y)), Some((5, w1.y + 2)));
    }

    #[test]
    fn the_ghost_is_clipped_at_the_right_edge_and_can_sit_on_the_status_bar_row() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        press_label(&mut state, &mut app, &brigade, "w2", now);
        let ghost_at = |state: &EmporiumState| state.drag_overlay(DRAG_AREA).unwrap().ghost;

        // 120 columns: from 115 there are five cells left.
        drag_to_cell(&mut state, &mut app, &brigade, 115, 10, now);
        assert_eq!(
            ghost_at(&state),
            Some(Ghost {
                text: " work".to_string(),
                x: 115,
                y: 10
            })
        );
        drag_to_cell(&mut state, &mut app, &brigade, 119, 10, now);
        assert_eq!(
            ghost_at(&state),
            Some(Ghost {
                text: " ".to_string(),
                x: 119,
                y: 10
            }),
            "one cell is all that is left"
        );
        // Row 39 is the status bar: still on screen, still ghosted (and the
        // pointer there means the last slot).
        drag_to_cell(&mut state, &mut app, &brigade, 40, 39, now);
        assert_eq!(
            ghost_at(&state),
            Some(Ghost {
                text: " worker-2 ".to_string(),
                x: 40,
                y: 39
            })
        );
        // Off the screen there is nothing to paint.
        drag_to_cell(&mut state, &mut app, &brigade, 120, 10, now);
        assert_eq!(ghost_at(&state), None);
        assert!(state.drag_overlay(DRAG_AREA).unwrap().marked.is_some());
    }

    #[test]
    fn both_cues_are_gone_after_the_release() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let w1 = tile_of(&state, "w1");
        press_label(&mut state, &mut app, &brigade, "w3", now);
        drag_to_cell(&mut state, &mut app, &brigade, 60, w1.y + 1, now);
        assert!(state.drag_overlay(DRAG_AREA).is_some());

        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            60,
            w1.y + 1,
            now,
        );

        assert_eq!(state.drag_overlay(DRAG_AREA), None);
    }

    #[test]
    fn a_resize_has_a_line_and_neither_mark_nor_ghost() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let boundary = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &boundary).position;
        drag_to(&mut state, &mut app, &brigade, &boundary, start + 5, now);

        let overlay = state.drag_overlay(DRAG_AREA).unwrap();

        assert!(overlay.line.is_some());
        assert_eq!(overlay.marked, None);
        assert_eq!(overlay.ghost, None);
    }

    // --- collapsing and solo ---------------------------------------------------

    /// Which stack tiles are shown collapsed, by pane name, in stack order.
    fn collapsed_names(state: &EmporiumState) -> Vec<String> {
        let Stage::Brigade { panes, .. } = &state.stage else {
            return Vec::new();
        };
        panes[1..]
            .iter()
            .zip(state.stack_collapsed())
            .filter(|(_, collapsed)| *collapsed)
            .map(|(key, _)| key.as_str().to_string())
            .collect()
    }

    fn set_collapse(state: &mut EmporiumState, minimized: &[&str], solo: Option<&str>) {
        let layout = state.brigade_layouts.entry(1).or_default();
        layout.minimized = minimized.iter().map(|t| t.to_string()).collect();
        layout.solo_member = solo.map(str::to_string);
    }

    fn layout_of(state: &EmporiumState) -> BrigadeLayout {
        state.brigade_layouts.get(&1).cloned().unwrap_or_default()
    }

    fn token_set(tokens: &[&str]) -> HashSet<MemberToken> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    fn key_of(name: &str) -> SessionKey {
        SessionKey::from_id(name)
    }

    fn press_button(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        kind: ButtonKind,
        now: Instant,
    ) -> Vec<Cmd> {
        let button = state
            .geometry(DRAG_AREA)
            .buttons()
            .iter()
            .find(|b| b.key.as_str() == name && b.kind == kind)
            .cloned()
            .unwrap_or_else(|| panic!("{name} has a {kind:?} button"));
        mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Down(MouseButton::Left),
            button.rect.x + 1,
            button.rect.y,
            now,
        )
    }

    #[test]
    fn a_stack_tile_is_collapsed_by_its_own_flag_unless_a_solo_overrides_it() {
        let cases: [(&[&str], Option<&str>, &[&str]); 7] = [
            (&[], None, &[]),
            (&["worker-2"], None, &["w2"]),
            (&["worker-1", "worker-3"], None, &["w1", "w3"]),
            (&[], Some("worker-1"), &["w2", "w3"]),
            // The soloed tile is shown whatever its own flag says.
            (&["worker-1"], Some("worker-1"), &["w2", "w3"]),
            // The solo moving to another tile: the flags are not consulted.
            (&["worker-2"], Some("worker-3"), &["w1", "w2"]),
            // A soloed member that is not staged leaves the flags in force.
            (&["worker-2"], Some("worker-9"), &["w2"]),
        ];
        for (minimized, solo, expected) in cases {
            let (mut state, _, _, _) = drag_fixture(3);
            set_collapse(&mut state, minimized, solo);
            assert_eq!(
                collapsed_names(&state),
                expected,
                "minimized {minimized:?}, solo {solo:?}"
            );
        }
    }

    #[test]
    fn a_solo_is_inactive_while_its_pane_is_gone_and_comes_back_with_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-3"], Some("worker-1"));
        assert_eq!(collapsed_names(&state), ["w2", "w3"]);

        // The soloed pane exits: the preference stays, but the others show as
        // their own flags say.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w1"),
                reason: None,
            },
            now,
        );
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-1"));
        assert_eq!(collapsed_names(&state), ["w3"]);
        assert_eq!(state.soloed_key(), None);

        // It comes back (relaunched in the same place in the order).
        let again = key_of("w1");
        state.screens.insert(again.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(again.clone(), "worker-1".to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.insert(1, again.clone());
        }
        assert_eq!(collapsed_names(&state), ["w2", "w3"]);
        assert_eq!(state.soloed_key(), Some(&again));
    }

    #[test]
    fn each_operation_leaves_exactly_the_sets_it_should() {
        let (mut state, _, _, _) = drag_fixture(3);
        let (w1, w2, w3) = (key_of("w1"), key_of("w2"), key_of("w3"));

        // Without a solo, minimize flips membership.
        toggle_minimize(&mut state, &w2);
        assert_eq!(layout_of(&state).minimized, token_set(&["worker-2"]));
        toggle_minimize(&mut state, &w2);
        assert_eq!(layout_of(&state).minimized, token_set(&[]));

        // Solo names a tile, moves to another, and goes off on the same one;
        // it never touches `minimized`.
        set_collapse(&mut state, &["worker-3"], None);
        toggle_solo(&mut state, &w1);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-1"));
        toggle_solo(&mut state, &w2);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-2"));
        assert_eq!(layout_of(&state).minimized, token_set(&["worker-3"]));
        toggle_solo(&mut state, &w2);
        assert_eq!(layout_of(&state).solo_member, None);

        // Minimize while a solo is on, on a tile shown collapsed: the solo
        // ends and the tile is opened (taken out of the set); the others fall
        // back to their own flags.
        set_collapse(&mut state, &["worker-3"], Some("worker-2"));
        toggle_minimize(&mut state, &w1);
        assert_eq!(layout_of(&state).solo_member, None);
        assert_eq!(layout_of(&state).minimized, token_set(&["worker-3"]));
        assert_eq!(collapsed_names(&state), ["w3"]);

        // ...on the soloed tile itself: the solo ends and it is collapsed.
        set_collapse(&mut state, &["worker-3"], Some("worker-2"));
        toggle_minimize(&mut state, &w2);
        assert_eq!(layout_of(&state).solo_member, None);
        assert_eq!(
            layout_of(&state).minimized,
            token_set(&["worker-2", "worker-3"])
        );
        assert_eq!(collapsed_names(&state), ["w2", "w3"]);

        // Restore all.
        restore_all(&mut state);
        assert_eq!(layout_of(&state).minimized, token_set(&[]));
        assert_eq!(layout_of(&state).solo_member, None);
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
        let _ = w3;
    }

    #[test]
    fn collapsing_the_focused_tile_moves_focus_below_then_above_then_to_the_master() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 2); // w2
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(collapsed_names(&state), ["w2"]);
        assert_eq!(
            focused_name(&state).as_deref(),
            Some("w3"),
            "the nearest expanded one below"
        );

        // Collapse w3 as well: nothing below, w1 is above.
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(collapsed_names(&state), ["w2", "w3"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w1"));

        // The last expanded one: the master.
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);
        assert_eq!(focused_name(&state).as_deref(), Some("dir"));
        assert_eq!(state.focus, Focus::Pane);
    }

    #[test]
    fn soloing_another_tile_focuses_it_and_restoring_leaves_focus_where_it_is() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 1); // w1
        // Solo w3 from its button: w1, focused, is collapsed, so focus goes to
        // the tile acted on.
        press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        assert_eq!(collapsed_names(&state), ["w1", "w2"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));

        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('M'),
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
    }

    #[test]
    fn a_solo_returning_with_its_pane_cannot_leave_focus_on_a_collapsed_tile() {
        // The safety net at the end of every update, not an operation.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &[], Some("worker-3"));
        focus_stack_slot(&mut state, 1); // w1, which the solo collapses

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(collapsed_names(&state), ["w1", "w2"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
    }

    #[test]
    fn a_collapsed_tile_gets_no_resize_and_exactly_one_when_it_opens() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let size_of =
            |state: &EmporiumState, name: &str| state.screens[&key_of(name)].screen().size();
        let before = size_of(&state, "w2");

        focus_stack_slot(&mut state, 2);
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );

        assert_eq!(collapsed_names(&state), ["w2"]);
        assert!(
            resizes(&cmds)
                .iter()
                .all(|(key, _, _)| key.as_str() != "w2"),
            "{cmds:?}"
        );
        assert_eq!(
            size_of(&state, "w2"),
            before,
            "its screen was not resized either"
        );
        // The neighbours, now taller, were.
        let resized: Vec<String> = resizes(&cmds)
            .into_iter()
            .map(|(k, _, _)| k.as_str().to_string())
            .collect();
        assert_eq!(resized, ["w1", "w3"]);

        // While it is collapsed its weight changes, so the size it opens at is
        // not the size it kept.
        state
            .brigade_layouts
            .get_mut(&1)
            .unwrap()
            .row_weights
            .extend([
                ("worker-1".to_string(), 10),
                ("worker-2".to_string(), 30),
                ("worker-3".to_string(), 10),
            ]);

        // Open it from its [+] button: one resize for it.
        let cmds = press_button(
            &mut state,
            &mut app,
            &brigade,
            "w2",
            ButtonKind::Minimize,
            now,
        );
        let for_w2: Vec<_> = resizes(&cmds)
            .into_iter()
            .filter(|(k, _, _)| k.as_str() == "w2")
            .collect();
        assert_eq!(for_w2.len(), 1, "{cmds:?}");
        let content = pane_content(tile_of(&state, "w2"));
        assert_eq!(for_w2[0].1, content.height);
        assert_eq!(for_w2[0].2, content.width);
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
    }

    #[test]
    fn a_row_drag_across_a_collapsed_tile_rewrites_only_the_expanded_weights() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let mut layout = BrigadeLayout::default();
        layout.row_weights.insert("worker-1".to_string(), 1);
        layout.row_weights.insert("worker-2".to_string(), 99);
        layout.row_weights.insert("worker-3".to_string(), 1);
        layout.minimized = token_set(&["worker-2"]);
        state.brigade_layouts.insert(1, layout);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        let boundary = Boundary::Stack {
            brigade: 1,
            above: key_of("w1"),
            below: key_of("w3"),
        };
        let (w1, w3) = (tile_of(&state, "w1"), tile_of(&state, "w3"));
        let strip = tile_of(&state, "w2");
        let start = handle_of(&state, &boundary).position;

        drag_to(&mut state, &mut app, &brigade, &boundary, start + 3, now);
        release_at(&mut state, &mut app, &brigade, &boundary, start + 3, now);

        let weights = layout_of(&state).row_weights;
        assert_eq!(
            weights["worker-2"], 99,
            "the collapsed member keeps its own"
        );
        assert_eq!(weights["worker-1"], w1.height + 3);
        assert_eq!(weights["worker-3"], w3.height - 3);
        assert_eq!(
            tile_of(&state, "w2").height,
            strip.height,
            "the strip did not change"
        );
    }

    #[test]
    fn the_buttons_do_what_they_say_at_once_and_start_nothing_else() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        // Every pane asks for mouse reports, so a stray press would be
        // forwarded to the pane under it.
        for name in ["dir", "w1", "w2", "w3"] {
            state
                .screens
                .get_mut(&key_of(name))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        let focus_before = (state.focus, focused_name(&state));

        let cmds = press_button(&mut state, &mut app, &brigade, "w2", ButtonKind::Solo, now);
        assert!(writes_of(&cmds).is_empty(), "{cmds:?}");
        assert!(state.drag.is_none() && state.move_drag.is_none());
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-2"));
        assert_eq!(collapsed_names(&state), ["w1", "w3"]);
        assert_eq!(
            (state.focus, focused_name(&state)),
            focus_before,
            "master focus stays"
        );

        // [S] on a collapsed tile makes it the solo.
        press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-3"));
        assert_eq!(collapsed_names(&state), ["w1", "w2"]);
        // [S] on the soloed tile turns it off.
        press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member, None);

        // [_] collapses, [+] opens.
        press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert_eq!(collapsed_names(&state), ["w1"]);
        let cmds = press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert!(writes_of(&cmds).is_empty());
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
    }

    #[test]
    fn a_press_on_a_collapsed_tiles_body_or_border_changes_nothing_and_its_label_moves_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        for name in ["dir", "w1", "w2", "w3"] {
            state
                .screens
                .get_mut(&key_of(name))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        set_collapse(&mut state, &["worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        let strip = tile_of(&state, "w2");
        let before = (state.focus, focused_name(&state));

        for (column, row) in [
            (strip.x + 5, strip.y + 2),  // body
            (strip.x, strip.y + 2),      // left border (a boundary's cell? no: master handle)
            (strip.x + 20, strip.y + 4), // bottom border
        ] {
            let cmds = mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                now,
            );
            if column == strip.x {
                // The master/stack boundary's column: that is a resize, not a click.
                state.drag = None;
                continue;
            }
            assert!(cmds.is_empty(), "{cmds:?}");
            assert_eq!(
                (state.focus, focused_name(&state)),
                before,
                "({column},{row})"
            );
            assert!(state.drag.is_none() && state.move_drag.is_none());
        }
        let cmds = mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::ScrollUp,
            strip.x + 5,
            strip.y + 2,
            now,
        );
        assert!(cmds.is_empty());

        // Its label starts a move without focusing it.
        let (column, row) = label_cell(&state, "w2");
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            now,
        );
        assert!(state.move_drag.is_some());
        assert_eq!((state.focus, focused_name(&state)), before, "not focused");
    }

    #[test]
    fn the_focus_ring_f3_and_the_arrows_skip_collapsed_tiles() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        // The ring: master, w1, w3, sidebar, master.
        focus_stack_slot(&mut state, 0);
        let mut seen = Vec::new();
        for _ in 0..4 {
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char('o'),
                Modifiers::NONE,
                now,
            );
            seen.push(if state.focus == Focus::Sidebar {
                "sidebar".to_string()
            } else {
                focused_name(&state).unwrap()
            });
        }
        assert_eq!(seen, ["w1", "w3", "sidebar", "dir"]);

        // A collapsed last tile: the ring and F3 wrap past it. (Landing on it
        // and being refocused would send them back to where they were.)
        set_collapse(&mut state, &["worker-3"], None);
        focus_stack_slot(&mut state, 2);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('o'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(state.focus, Focus::Sidebar, "the ring skips w3");
        focus_stack_slot(&mut state, 2);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::F(3),
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("dir"), "F3 skips w3");
        set_collapse(&mut state, &["worker-2"], None);

        // F3 steps through the panes, skipping the collapsed one.
        focus_stack_slot(&mut state, 1);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::F(3),
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::F(3),
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("dir"));

        // Arrows: Up/Down jump over it, Right from the master goes to the
        // first expanded tile.
        focus_stack_slot(&mut state, 1);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w1"));
        focus_stack_slot(&mut state, 0);
        set_collapse(&mut state, &["worker-1"], None);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Right,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));

        // With every stack tile collapsed Right from the master does nothing.
        focus_stack_slot(&mut state, 0);
        set_collapse(&mut state, &["worker-1", "worker-2", "worker-3"], None);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Right,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("dir"));
    }

    #[test]
    fn a_pane_number_opens_a_collapsed_pane_and_focuses_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-2", "worker-3"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('3'),
            Modifiers::NONE,
            now,
        );

        assert_eq!(layout_of(&state).minimized, token_set(&["worker-3"]));
        assert_eq!(collapsed_names(&state), ["w3"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));
        assert_eq!(state.focus, Focus::Pane);

        // With a solo on, the number ends it and opens the pane.
        set_collapse(&mut state, &["worker-3"], Some("worker-1"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('4'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member, None);
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
    }

    #[test]
    fn a_strip_is_moved_by_the_pointers_row_among_strips_with_a_horizontal_line() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-2", "worker-3"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        let (w1, w3) = (tile_of(&state, "w1"), tile_of(&state, "w3"));

        press_label(&mut state, &mut app, &brigade, "w1", now);
        // The column wanders over the master; only the row counts.
        drag_to_cell(&mut state, &mut app, &brigade, 40, w3.y + 2, now);
        assert_eq!(
            overlay_line(&state),
            Some((
                Rect::new(w3.x, w3.y + w3.height - 1, w3.width, 1),
                Axis::Horizontal
            ))
        );
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            40,
            w3.y + 2,
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w2", "w3", "w1"]);
        assert_eq!(w1.width, w3.width);

        // Released in the empty space under the strips: the last slot.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-2", "worker-3"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        press_label(&mut state, &mut app, &brigade, "w1", now);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            110,
            30,
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w2", "w3", "w1"]);
    }

    /// Release the held left button over `name`'s tile (a row
    /// inside it) and report the order.
    fn release_over(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        name: &str,
        now: Instant,
    ) -> Vec<String> {
        let tile = tile_of(state, name);
        let (column, row) = (tile.x + 3, tile.y + 1);
        drag_to_cell(state, app, brigade, column, row, now);
        mouse_event(
            state,
            app,
            brigade,
            MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            now,
        );
        order_of(state)
    }

    #[test]
    fn collapsing_the_tile_under_a_held_label_so_that_every_tile_is_collapsed_ends_the_move() {
        // Only w2 is expanded; a column-mode move of it is under way.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-3"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        press_label(&mut state, &mut app, &brigade, "w2", now);
        assert!(state.move_drag.is_some());

        // prefix + m collapses w2: every stack tile is collapsed.
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );

        assert!(state.geometry(DRAG_AREA).all_collapsed);
        assert!(
            state.move_drag.is_none(),
            "the gesture ended with the change"
        );
        assert_eq!(state.drag_overlay(DRAG_AREA), None);
        // The gesture is gone, so the release that follows reorders nothing.
        let order = release_over(&mut state, &mut app, &brigade, "w3", now);
        assert_eq!(order, ["dir", "w1", "w2", "w3"]);
        assert!(layout_of(&state).stack_order.is_empty());
    }

    #[test]
    fn any_collapse_operation_ends_a_gesture_even_when_the_layout_stays_a_column() {
        for key in [
            ('m', Modifiers::NONE),
            ('z', Modifiers::NONE),
            ('M', Modifiers::SHIFT),
        ] {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            // Held on w1's label; w1 is also the focused tile, so m/z act on it.
            press_label(&mut state, &mut app, &brigade, "w1", now);
            assert!(state.move_drag.is_some());
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char(key.0),
                key.1,
                now,
            );
            assert!(state.move_drag.is_none(), "{key:?} ends the move");
            assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3"]);
        }
        // A resize held on a boundary ends too.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let boundary = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &boundary).position;
        drag_to(&mut state, &mut app, &brigade, &boundary, start + 4, now);
        assert!(state.drag.is_some());
        focus_stack_slot(&mut state, 2);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert!(state.drag.is_none());
        let sidebar_width_before = state.sidebar_width;
        release_at(&mut state, &mut app, &brigade, &boundary, start + 4, now);
        assert_eq!(state.sidebar_width, sidebar_width_before);
        assert!(layout_of(&state).master_permille == DEFAULT_MASTER_PERMILLE);
    }

    #[test]
    fn the_last_expanded_pane_leaving_under_a_held_label_keeps_the_move_alive_and_by_row() {
        // No operation at all: w3 exits, which leaves the stack all
        // collapsed and narrows the column. The move reads the pointer's row
        // either way, so its meaning did not change.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        press_label(&mut state, &mut app, &brigade, "w1", now);
        assert!(state.move_drag.is_some());

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w3"),
                reason: None,
            },
            now,
        );

        assert!(state.geometry(DRAG_AREA).all_collapsed);
        assert!(state.move_drag.is_some());
        let order = release_over(&mut state, &mut app, &brigade, "w2", now);
        assert_eq!(order, ["dir", "w2", "w1"]);
    }

    #[test]
    fn a_pane_arriving_into_an_all_collapsed_stack_under_a_held_label_keeps_the_move_alive() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        set_collapse(&mut state, &["worker-1", "worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        press_label(&mut state, &mut app, &brigade, "w1", now);

        open_worker(
            &mut state,
            1,
            "worker-3",
            std::path::Path::new("/work"),
            "",
            None,
            AgentKind::ClaudeCode,
        );
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: SessionKey::new_worker(1, "worker-3"),
            },
            now,
        );

        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        assert!(
            state.move_drag.is_some(),
            "a row move means the same in both"
        );
        let order = release_over(
            &mut state,
            &mut app,
            &brigade,
            "new-worker::1::worker-3",
            now,
        );
        assert_eq!(order, ["dir", "w2", "new-worker::1::worker-3", "w1"]);
    }

    #[test]
    fn a_solo_returning_in_a_column_layout_keeps_the_held_move_alive() {
        // The soloed member's pane is gone, so the solo is inactive and the
        // stack is a column; it comes back, every other tile collapses.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &[], Some("worker-3"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w3"),
                reason: None,
            },
            now,
        );
        assert_eq!(collapsed_names(&state), Vec::<String>::new());
        press_label(&mut state, &mut app, &brigade, "w1", now);
        assert!(state.move_drag.is_some());

        let again = key_of("w3");
        state.screens.insert(again.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(again.clone(), "worker-3".to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.push(again);
        }
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        // Column mode throughout, so the gesture's meaning did not change:
        // it survives, as a pane arriving under a held button always did.
        assert_eq!(collapsed_names(&state), ["w1", "w2"]);
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        assert!(state.move_drag.is_some());
    }

    // --- every stack tile collapsed: the narrow column of strips ---------------

    fn all_collapsed_fixture(workers: usize) -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade, now) = drag_fixture(workers);
        let tokens: Vec<String> = (1..=workers).map(|n| format!("worker-{n}")).collect();
        let tokens: Vec<&str> = tokens.iter().map(String::as_str).collect();
        set_collapse(&mut state, &tokens, None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        (state, app, brigade, now)
    }

    fn master_width(state: &EmporiumState) -> u16 {
        state.geometry(DRAG_AREA).tiles[0].1.width
    }

    #[test]
    fn the_stack_stays_at_the_side_when_every_tile_is_collapsed_and_nothing_is_a_band() {
        let (state, _, _, _) = all_collapsed_fixture(3);
        let geometry = state.geometry(DRAG_AREA);
        let pane = geometry.areas.pane;
        let master = geometry.tiles[0].1;
        assert_eq!(master.height, pane.height, "the master keeps every row");
        assert_eq!(master.width, pane.width - DEFAULT_COLLAPSED_STACK_WIDTH);
        for (_, strip) in &geometry.tiles[1..] {
            assert!(strip.x >= master.x + master.width, "right of the master");
            assert_eq!(strip.width, DEFAULT_COLLAPSED_STACK_WIDTH);
            assert_eq!(strip.height, COLLAPSED_TILE_HEIGHT);
        }
        assert_eq!(geometry.tiles[1].1.y, pane.y, "from the top");
    }

    #[test]
    fn the_collapsed_stack_width_follows_the_collapse_state_through_solo_and_restore() {
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(3);
        let narrow = master_width(&state);
        let wide = {
            let (open, _, _, _) = drag_fixture(3);
            master_width(&open)
        };
        assert!(
            narrow > wide,
            "a 36-column stack leaves the master more than half"
        );

        // A solo on a collapsed tile opens it: the ordinary share is back.
        press_button(&mut state, &mut app, &brigade, "w2", ButtonKind::Solo, now);
        assert_eq!(master_width(&state), wide);
        // The solo moving to another tile changes nothing about the width.
        press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        assert_eq!(master_width(&state), wide);
        // The solo ending returns to the underlying all-collapsed state.
        press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        assert_eq!(master_width(&state), narrow);
        // Opening any one tile.
        press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert_eq!(master_width(&state), wide);
        // Collapsing it again, and restoring all.
        press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert_eq!(master_width(&state), narrow);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('M'),
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(master_width(&state), wide);
    }

    #[test]
    fn the_masters_pty_is_resized_once_entering_and_once_leaving_the_all_collapsed_state() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        focus_stack_slot(&mut state, 3); // w3, the last open tile
        let for_master = |cmds: &[Cmd]| {
            resizes(cmds)
                .into_iter()
                .filter(|(key, _, _)| key.as_str() == "dir")
                .count()
        };

        // Entering: collapsing the last open tile.
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        assert_eq!(for_master(&cmds), 1, "{cmds:?}");
        assert!(
            resizes(&cmds)
                .iter()
                .all(|(key, _, _)| key.as_str() == "dir"),
            "strips are never resized: {cmds:?}"
        );
        let content = pane_content(tile_of(&state, "dir"));
        assert_eq!(
            resizes(&cmds)[0].1..=resizes(&cmds)[0].2,
            content.height..=content.width
        );

        // A tick changes nothing.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert!(resizes(&cmds).is_empty());

        // Leaving: opening a tile.
        let cmds = press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        assert_eq!(for_master(&cmds), 1, "{cmds:?}");
    }

    #[test]
    fn dragging_the_master_stack_boundary_sets_the_preference_of_the_state_it_is_in() {
        // All collapsed: the collapsed stack width, and only that.
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(3);
        let boundary = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &boundary).position;
        // (4 columns wider: the ordinary stack is 42, and the strips never get
        // more than that.)
        drag_to(&mut state, &mut app, &brigade, &boundary, start - 4, now);
        release_at(&mut state, &mut app, &brigade, &boundary, start - 4, now);
        let layout = layout_of(&state);
        assert_eq!(
            layout.collapsed_stack_width,
            DEFAULT_COLLAPSED_STACK_WIDTH + 4
        );
        assert_eq!(
            layout.master_permille, DEFAULT_MASTER_PERMILLE,
            "the share is not touched"
        );
        assert_eq!(
            tile_of(&state, "w1").width,
            DEFAULT_COLLAPSED_STACK_WIDTH + 4
        );

        // Opening a tile: the ordinary share applies, and dragging sets it.
        press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        let share_before = layout_of(&state).master_permille;
        assert_eq!(share_before, DEFAULT_MASTER_PERMILLE);
        let start = handle_of(&state, &boundary).position;
        // (To the left, so the ordinary stack gets wider than the collapsed
        // width and the dragged collapsed width is not capped when it comes
        // back below.)
        drag_to(&mut state, &mut app, &brigade, &boundary, start - 8, now);
        release_at(&mut state, &mut app, &brigade, &boundary, start - 8, now);
        let layout = layout_of(&state);
        assert_ne!(layout.master_permille, DEFAULT_MASTER_PERMILLE);
        assert_eq!(
            layout.collapsed_stack_width,
            DEFAULT_COLLAPSED_STACK_WIDTH + 4,
            "the collapsed width is not touched"
        );

        // Collapsing again brings back the dragged collapsed width.
        press_button(
            &mut state,
            &mut app,
            &brigade,
            "w1",
            ButtonKind::Minimize,
            now,
        );
        assert_eq!(
            tile_of(&state, "w2").width,
            DEFAULT_COLLAPSED_STACK_WIDTH + 4
        );
    }

    #[test]
    fn the_collapsed_width_drag_is_clamped_and_the_preference_survives_a_roomier_terminal() {
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(2);
        let boundary = Boundary::Master { brigade: 1 };
        // Far to the right: the stack would be narrower than its minimum.
        drag_to(&mut state, &mut app, &brigade, &boundary, 119, now);
        release_at(&mut state, &mut app, &brigade, &boundary, 119, now);
        assert_eq!(tile_of(&state, "w1").width, MIN_STACK_WIDTH);
        // Far to the left: the master would be narrower than the ordinary
        // split's (42 of 84), which is as far as it may go.
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(2);
        drag_to(&mut state, &mut app, &brigade, &boundary, 0, now);
        release_at(&mut state, &mut app, &brigade, &boundary, 0, now);
        assert_eq!(master_width(&state), 42);
    }

    #[test]
    fn a_held_master_stack_drag_ends_when_a_pane_flips_the_state_under_it() {
        // All collapsed, drag held; an open Worker arrives.
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(2);
        let boundary = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &boundary).position;
        drag_to(&mut state, &mut app, &brigade, &boundary, start - 6, now);
        assert!(state.drag.is_some());
        open_worker(
            &mut state,
            1,
            "worker-3",
            std::path::Path::new("/work"),
            "",
            None,
            AgentKind::ClaudeCode,
        );
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: SessionKey::new_worker(1, "worker-3"),
            },
            now,
        );
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        assert!(
            state.drag.is_none(),
            "the same boundary now means another preference"
        );
        release_at(&mut state, &mut app, &brigade, &boundary, start - 6, now);
        let layout = layout_of(&state);
        assert_eq!(layout.collapsed_stack_width, DEFAULT_COLLAPSED_STACK_WIDTH);
        assert_eq!(
            layout.master_permille, DEFAULT_MASTER_PERMILLE,
            "nothing was committed"
        );

        // The other way: one open tile, drag held, that tile's pane exits.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-1", "worker-2"], None);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        let start = handle_of(&state, &boundary).position;
        drag_to(&mut state, &mut app, &brigade, &boundary, start + 6, now);
        assert!(state.drag.is_some());
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w3"),
                reason: None,
            },
            now,
        );
        assert!(state.geometry(DRAG_AREA).all_collapsed);
        assert!(state.drag.is_none());
        assert_eq!(layout_of(&state).master_permille, DEFAULT_MASTER_PERMILLE);
    }

    #[test]
    fn a_press_in_the_empty_space_under_the_strips_does_nothing() {
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(2);
        for name in ["dir", "w1", "w2"] {
            state
                .screens
                .get_mut(&key_of(name))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        focus_stack_slot(&mut state, 0);
        state.focus = Focus::Sidebar;
        let before = (state.focus, focused_name(&state));
        let strip = tile_of(&state, "w2");
        let (column, row) = (strip.x + 5, strip.y + strip.height + 4);
        assert!(
            state
                .geometry(DRAG_AREA)
                .tiles
                .iter()
                .all(|(_, t)| !t.contains(ratatui_core::layout::Position::new(column, row)))
        );

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Moved,
        ] {
            let cmds = mouse_event(&mut state, &mut app, &brigade, kind, column, row, now);
            assert!(cmds.is_empty(), "{kind:?}: {cmds:?}");
        }
        assert_eq!(
            (state.focus, focused_name(&state)),
            before,
            "no focus change"
        );
        assert!(state.drag.is_none() && state.move_drag.is_none());
    }

    #[test]
    fn a_tiny_terminal_never_panics_with_every_tile_collapsed() {
        for (width, height) in [
            (0u16, 0u16),
            (1, 1),
            (10, 5),
            (36, 8),
            (40, 6),
            (60, 12),
            (120, 7),
        ] {
            let (mut state, mut app, brigade, now) = all_collapsed_fixture(4);
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Resized { width, height },
                now,
            );
            let area = Rect::new(0, 0, width, height);
            let handles: Vec<Handle> = state.geometry(area).handles().to_vec();
            for handle in handles {
                let column = handle.hits()[0].x;
                let row = handle.hits()[0].y;
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Drag(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    mouse_event(&mut state, &mut app, &brigade, kind, column, row, now);
                }
            }
            let _ = state.drag_overlay(area);
        }
    }

    // --- soloing the master -------------------------------------------------------

    fn master_button_count(state: &EmporiumState) -> usize {
        state
            .geometry(DRAG_AREA)
            .buttons()
            .iter()
            .filter(|b| b.key.as_str() == "dir")
            .count()
    }

    #[test]
    fn the_master_can_be_the_soloed_tile_and_that_collapses_every_stack_tile() {
        let cases: [(&[&str], Option<&str>, &[&str]); 5] = [
            (&[], Some("director"), &["w1", "w2", "w3"]),
            // The flags are not consulted while a solo is active.
            (&["worker-2"], Some("director"), &["w1", "w2", "w3"]),
            // The solo moves from the master to a stack tile and back.
            (&[], Some("worker-2"), &["w1", "w3"]),
            (&["worker-1"], Some("director"), &["w1", "w2", "w3"]),
            // Off: each tile is back on its own flag.
            (&["worker-1"], None, &["w1"]),
        ];
        for (minimized, solo, expected) in cases {
            let (mut state, _, _, _) = drag_fixture(3);
            set_collapse(&mut state, minimized, solo);
            assert_eq!(collapsed_names(&state), expected, "{minimized:?} {solo:?}");
        }
        let (mut state, _, _, _) = drag_fixture(3);
        set_collapse(&mut state, &[], Some("director"));
        assert_eq!(state.soloed_key(), Some(&key_of("dir")));
    }

    #[test]
    fn soloing_the_master_reaches_the_all_collapsed_column_and_leaving_it_returns() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let wide = master_width(&state);
        press_button(&mut state, &mut app, &brigade, "dir", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        let geometry = state.geometry(DRAG_AREA);
        assert!(geometry.all_collapsed);
        assert_eq!(
            geometry.tiles[1].1.width, DEFAULT_COLLAPSED_STACK_WIDTH,
            "the existing rule, nothing new"
        );
        assert!(master_width(&state) > wide);

        // Moving the solo master -> a strip -> the master again.
        press_button(&mut state, &mut app, &brigade, "w2", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-2"));
        assert_eq!(collapsed_names(&state), ["w1", "w3"]);
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        press_button(&mut state, &mut app, &brigade, "dir", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert!(state.geometry(DRAG_AREA).all_collapsed);

        // Off from the master's own button: the ordinary layout is back.
        press_button(&mut state, &mut app, &brigade, "dir", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member, None);
        assert!(!state.geometry(DRAG_AREA).all_collapsed);
        assert_eq!(master_width(&state), wide);
        assert!(layout_of(&state).minimized.is_empty(), "never touched");
    }

    #[test]
    fn the_master_has_a_solo_button_only_where_it_has_something_to_solo_against() {
        // A brigade with a stack, the master's member known.
        let (state, _, _, _) = drag_fixture(2);
        assert_eq!(master_button_count(&state), 1);
        let button = state
            .geometry(DRAG_AREA)
            .buttons()
            .iter()
            .find(|b| b.key.as_str() == "dir")
            .cloned()
            .unwrap();
        assert_eq!(button.kind, ButtonKind::Solo);
        assert_eq!(button.text, "[S]");
        let master = tile_of(&state, "dir");
        let right = master.x + master.width - 1;
        assert_eq!(
            button.rect,
            Rect::new(right - 3, master.y, 3, 1),
            "ending before the corner"
        );

        // One pane: nothing to collapse.
        let (state, _, _, _) = drag_fixture(0);
        assert_eq!(master_button_count(&state), 0);
        // A single-session stage.
        let (mut state, _, _, _) = drag_fixture(2);
        state.stage = Stage::Solo(key_of("dir"));
        assert_eq!(master_button_count(&state), 0);
        // A master with no member token.
        let (mut state, _, _, _) = drag_fixture(2);
        state.member_tokens.remove(&key_of("dir"));
        assert_eq!(master_button_count(&state), 0);
    }

    #[test]
    fn the_master_button_needs_room_for_the_label_a_gap_and_itself() {
        // " director " is 10 cells; the button needs 10 + 1 + 3 inner columns.
        let mut seen = (false, false);
        for width in 40..130u16 {
            let (mut state, mut app, brigade, now) = drag_fixture(2);
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Resized { width, height: 40 },
                now,
            );
            let area = Rect::new(0, 0, width, 40);
            let master = state.geometry(area).tiles[0].1;
            let has = state
                .geometry(area)
                .buttons()
                .iter()
                .any(|b| b.key.as_str() == "dir");
            assert_eq!(
                has,
                master.width - 2 >= 10 + 1 + 3,
                "master {} wide",
                master.width
            );
            if has {
                seen.0 = true;
            } else {
                seen.1 = true;
            }
        }
        assert!(seen.0 && seen.1, "the sweep crosses the threshold");
    }

    #[test]
    fn the_masters_title_row_has_no_handle_and_its_label_still_starts_no_move() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let master = tile_of(&state, "dir");
        let right = master.x + master.width - 1;
        let geometry = state.geometry(DRAG_AREA);
        // Nothing along the master's title row between its corners is a
        // handle (the corners are the vertical boundaries' own).
        for x in master.x + 1..right {
            assert!(geometry.handle_at(x, master.y).is_none(), "column {x}");
        }
        // The label cells are not a move handle either (master label).
        let (column, row) = (master.x + 2, master.y);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            now,
        );
        assert!(state.move_drag.is_none() && state.drag.is_none());
        assert_eq!(focused_name(&state).as_deref(), Some("dir"));
    }

    #[test]
    fn the_masters_solo_button_acts_at_once_cancels_a_held_gesture_and_forwards_nothing() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        for name in ["dir", "w1", "w2", "w3"] {
            state
                .screens
                .get_mut(&key_of(name))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        focus_stack_slot(&mut state, 2); // a stack tile has focus

        let cmds = press_button(&mut state, &mut app, &brigade, "dir", ButtonKind::Solo, now);

        assert!(writes_of(&cmds).is_empty(), "{cmds:?}");
        assert!(state.drag.is_none() && state.move_drag.is_none());
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);
        // The focused tile was just collapsed: focus goes to the tile acted
        // on, the master.
        assert_eq!(focused_name(&state).as_deref(), Some("dir"));
        assert_eq!(state.focus, Focus::Pane);

        // Like any solo operation it ends a held gesture.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        press_label(&mut state, &mut app, &brigade, "w2", now);
        assert!(state.move_drag.is_some());
        toggle_solo(&mut state, &key_of("dir"));
        assert!(state.move_drag.is_none());
        let _ = (&mut app, &brigade, now);
    }

    #[test]
    fn prefix_z_on_the_master_toggles_the_masters_solo_and_on_a_stack_tile_moves_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 0);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);
        assert_eq!(
            focused_name(&state).as_deref(),
            Some("dir"),
            "the master stays focused"
        );

        // Solo a strip from the key: it needs focus, and a strip cannot have
        // it; the strip's own [S] moves the solo there.
        press_button(&mut state, &mut app, &brigade, "w1", ButtonKind::Solo, now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-1"));
        focus_stack_slot(&mut state, 0);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(
            layout_of(&state).solo_member.as_deref(),
            Some("director"),
            "moved back"
        );
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member, None);
    }

    #[test]
    fn opening_a_strip_during_a_master_solo_ends_the_solo_and_leaves_the_rest_on_their_flags() {
        for open in ["button", "number"] {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            set_collapse(&mut state, &["worker-3"], Some("director"));
            assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);

            if open == "button" {
                press_button(
                    &mut state,
                    &mut app,
                    &brigade,
                    "w2",
                    ButtonKind::Minimize,
                    now,
                );
            } else {
                prefix_key(
                    &mut state,
                    &mut app,
                    &brigade,
                    KeyCode::Char('3'),
                    Modifiers::NONE,
                    now,
                );
            }

            assert_eq!(layout_of(&state).solo_member, None, "{open}");
            // w2 is open; w1 has no flag of its own, w3 has.
            assert_eq!(collapsed_names(&state), ["w3"], "{open}");
            assert_eq!(
                layout_of(&state).minimized,
                token_set(&["worker-3"]),
                "{open}"
            );
            if open == "number" {
                assert_eq!(focused_name(&state).as_deref(), Some("w2"));
            }
        }
    }

    #[test]
    fn soloing_the_master_from_the_sidebar_in_a_narrow_pane_area_keeps_its_button() {
        // The reviewer's case: a 32-column pane area (68 columns in all), the
        // ordinary master 16 wide, room for " director " + a gap + [S].
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 68,
                height: 40,
            },
            now,
        );
        let area = Rect::new(0, 0, 68, 40);
        assert_eq!(state.geometry(area).areas.pane.width, 32);
        let master_button = |state: &EmporiumState| {
            state
                .geometry(area)
                .buttons()
                .iter()
                .find(|b| b.key.as_str() == "dir")
                .cloned()
        };
        let before = master_button(&state).expect("the button fits in the ordinary split");
        state.focus = Focus::Sidebar;
        let press = |state: &mut EmporiumState, app: &mut App, button: &TileButton| {
            mouse_event(
                state,
                app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                button.rect.x + 1,
                button.rect.y,
                now,
            );
        };

        press(&mut state, &mut app, &before);

        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert!(state.geometry(area).all_collapsed);
        assert_eq!(
            state.focus,
            Focus::Sidebar,
            "a button press does not move focus"
        );
        let after = master_button(&state).expect("the button that was just clicked is still there");
        assert!(state.geometry(area).tiles[0].1.width >= 16);
        // A second press on it turns the solo off again.
        press(&mut state, &mut app, &after);
        assert_eq!(layout_of(&state).solo_member, None);
        assert!(!state.geometry(area).all_collapsed);
        assert!(master_button(&state).is_some());
        // And prefix z from the sidebar is still not how you get out of it
        // (it is refused there), which is why the button has to stay.
        let again = master_button(&state).unwrap();
        press(&mut state, &mut app, &again);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prefix: focus a tile to solo it")
        );
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
    }

    #[test]
    fn a_stack_member_dismissed_during_a_master_solo_keeps_the_solo() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-2"], Some("director"));
        state.pending_dismiss = Some(key_of("w2"));
        state.pending_dismiss_member = Some((1, "worker-2".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(1), now);

        let layout = layout_of(&state);
        assert_eq!(layout.solo_member.as_deref(), Some("director"));
        assert!(
            layout.minimized.is_empty(),
            "the dismissed member's flag went"
        );
        assert_eq!(collapsed_names(&state), ["w1", "w3"]);
    }

    #[test]
    fn the_soloed_masters_pane_exiting_leaves_the_solo_inactive_and_returning_revives_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-3"], Some("director"));
        assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("dir"),
                reason: None,
            },
            now,
        );

        // The preference stays; w1 now holds the master position and the
        // stack is back on its own flags.
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert_eq!(state.soloed_key(), None);
        assert_eq!(collapsed_names(&state), ["w3"]);

        // The Director's pane returns to the master position.
        let again = key_of("dir");
        state.screens.insert(again.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(again.clone(), DIRECTOR_TOKEN.to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.insert(0, again.clone());
        }
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert_eq!(state.soloed_key(), Some(&again));
        assert_eq!(collapsed_names(&state), ["w1", "w2", "w3"]);
    }

    #[test]
    fn a_master_solo_is_forgotten_with_its_member_and_on_disband() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        set_collapse(&mut state, &[], Some("director"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );
        assert!(!state.brigade_layouts.contains_key(&1));
    }

    // --- persistence ---------------------------------------------------------------

    /// The `SaveBrigadeLayout` snapshots among `cmds`.
    fn layout_writes(cmds: &[Cmd]) -> Vec<(BrigadeId, StoredLayout)> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::Store(StoreIntent::SaveBrigadeLayout { brigade_id, layout }) => {
                    Some((*brigade_id, layout.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn width_writes(cmds: &[Cmd]) -> Vec<u16> {
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::Store(StoreIntent::SaveSidebarWidth { width }) => Some(*width),
                _ => None,
            })
            .collect()
    }

    fn any_preference_write(cmds: &[Cmd]) -> bool {
        !layout_writes(cmds).is_empty() || !width_writes(cmds).is_empty()
    }

    fn member_entry<'a>(layout: &'a StoredLayout, token: &str) -> Option<&'a StoredMemberLayout> {
        layout.members.iter().find(|m| m.token == token)
    }

    #[test]
    fn a_committed_resize_of_each_boundary_writes_exactly_once_with_the_right_snapshot() {
        // The sidebar: the width, not a brigade.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        drag_to(&mut state, &mut app, &brigade, &Boundary::Sidebar, 48, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &Boundary::Sidebar, 48, now);
        assert_eq!(width_writes(&cmds), [48]);
        assert!(layout_writes(&cmds).is_empty());

        // The master/stack edge in the ordinary state: the share.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start + 6, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &master, start + 6, now);
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1, "{cmds:?}");
        assert_eq!(writes[0].0, 1);
        assert_eq!(
            writes[0].1.master_permille,
            Some(layout_of(&state).master_permille)
        );
        assert_eq!(writes[0].1.collapsed_stack_width, None);
        assert!(width_writes(&cmds).is_empty());

        // The same edge while every tile is collapsed: the collapsed width.
        let (mut state, mut app, brigade, now) = all_collapsed_fixture(2);
        let before = layout_writes(&update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        ));
        assert!(before.is_empty());
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start - 3, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &master, start - 3, now);
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].1.collapsed_stack_width,
            Some(DEFAULT_COLLAPSED_STACK_WIDTH + 3)
        );
        assert_eq!(writes[0].1.master_permille, None);

        // A row boundary: the expanded members' weights.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let row = w1_over_w2();
        let start = handle_of(&state, &row).position;
        drag_to(&mut state, &mut app, &brigade, &row, start + 2, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &row, start + 2, now);
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1);
        let snapshot = &writes[0].1;
        for token in ["worker-1", "worker-2", "worker-3"] {
            let entry = member_entry(snapshot, token).expect(token);
            assert_eq!(
                entry.row_weight,
                layout_of(&state).row_weights.get(token).copied()
            );
        }
    }

    #[test]
    fn moving_collapsing_and_soloing_each_write_once_with_the_resulting_arrangement() {
        // A mouse move.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let cmds = move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1, "{cmds:?}");
        let ranks: Vec<(String, Option<u32>)> = writes[0]
            .1
            .members
            .iter()
            .map(|m| (m.token.clone(), m.stack_rank))
            .collect();
        assert_eq!(
            ranks,
            [
                ("worker-1".to_string(), Some(1)),
                ("worker-2".to_string(), Some(2)),
                ("worker-3".to_string(), Some(0)),
            ]
        );

        // A keyboard move.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 2);
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(layout_writes(&cmds).len(), 1, "{cmds:?}");

        // Minimize, then open again: two writes in all, one each.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 2);
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1);
        assert!(member_entry(&writes[0].1, "worker-2").unwrap().minimized);
        let cmds = press_button(
            &mut state,
            &mut app,
            &brigade,
            "w2",
            ButtonKind::Minimize,
            now,
        );
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1);
        assert!(
            !member_entry(&writes[0].1, "worker-2").unwrap().minimized,
            "a cleared flag is written (the member is listed though nothing is set)"
        );

        // Solo: the member's token at brigade level.
        let cmds = press_button(&mut state, &mut app, &brigade, "w3", ButtonKind::Solo, now);
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1.solo_member.as_deref(), Some("worker-3"));
        // Restore-all: the solo and every flag go.
        set_collapse(&mut state, &["worker-1"], Some("worker-3"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('M'),
            Modifiers::SHIFT,
            now,
        );
        let writes = layout_writes(&cmds);
        assert_eq!(writes.len(), 1, "{cmds:?}");
        assert_eq!(writes[0].1.solo_member, None);
        assert!(writes[0].1.members.iter().all(|m| !m.minimized));
    }

    #[test]
    fn nothing_is_written_for_a_gesture_that_committed_nothing_or_a_refused_key_or_a_tick() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let mut all = Vec::new();

        // Press and release on the spot, on a resize handle and on a label.
        let master = Boundary::Master { brigade: 1 };
        let handle = handle_of(&state, &master);
        let (column, row) = grab_point(&handle);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            all.extend(mouse_event(
                &mut state, &mut app, &brigade, kind, column, row, now,
            ));
        }
        let (c, r) = label_cell(&state, "w2");
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            all.extend(mouse_event(&mut state, &mut app, &brigade, kind, c, r, now));
        }
        // A drag cancelled with Esc, and a move cancelled the same way.
        drag_to(
            &mut state,
            &mut app,
            &brigade,
            &master,
            handle.position + 5,
            now,
        );
        all.extend(forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Esc,
            Modifiers::NONE,
            now,
        ));
        press_label(&mut state, &mut app, &brigade, "w3", now);
        drag_to_cell(&mut state, &mut app, &brigade, 60, 3, now);
        all.extend(forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Esc,
            Modifiers::NONE,
            now,
        ));
        // Refused keys: collapse on the master, shift-up at the top, restore-all with nothing.
        focus_stack_slot(&mut state, 0);
        all.extend(prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        ));
        focus_stack_slot(&mut state, 1);
        all.extend(prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::SHIFT,
            now,
        ));
        all.extend(prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('M'),
            Modifiers::SHIFT,
            now,
        ));
        // Ticks, output, a resize and focus changes.
        all.extend(update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        ));
        all.extend(update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key_of("w1"),
                chunk: b"x".to_vec(),
            },
            now,
        ));
        all.extend(update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 100,
                height: 30,
            },
            now,
        ));
        all.extend(update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: false },
            now,
        ));

        assert!(!any_preference_write(&all), "{all:?}");
    }

    #[test]
    fn dismissal_and_disband_write_nothing_the_store_forgets_them_itself() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-3"], Some("worker-3"));
        // Settle: the setup above is a change like any other; flush its write.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        state.pending_dismiss = Some(key_of("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));
        let cmds = update(&mut state, &mut app, &brigade, dismissed(1), now);
        assert!(!any_preference_write(&cmds), "{cmds:?}");
        assert_eq!(layout_of(&state).solo_member, None);

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
        assert!(!any_preference_write(&cmds), "{cmds:?}");
        assert!(state.persisted_layouts.is_empty());
    }

    /// A staged-by-the-store brigade: the Director and two Workers that all
    /// have session rows, resolved through the real event.
    fn resolve_brigade_with(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        stored: Option<StoredLayout>,
        now: Instant,
    ) -> Vec<Cmd> {
        state.pending_membership = Some(PendingMembership::Activate);
        update(
            state,
            app,
            brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(vec![
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
                    (
                        "worker-2".to_string(),
                        BrigadeRole::Worker,
                        Some("w2".to_string()),
                    ),
                ]),
                layout: stored,
            },
            now,
        )
    }

    fn stored_arrangement() -> StoredLayout {
        StoredLayout {
            master_permille: Some(700),
            collapsed_stack_width: Some(40),
            solo_member: None,
            members: vec![
                StoredMemberLayout {
                    token: "worker-2".to_string(),
                    stack_rank: Some(0),
                    row_weight: Some(1),
                    minimized: false,
                },
                StoredMemberLayout {
                    token: "worker-1".to_string(),
                    stack_rank: Some(1),
                    row_weight: Some(3),
                    minimized: false,
                },
            ],
        }
    }

    fn fresh_state_sized_120x40() -> (EmporiumState, App, BrigadeConfig, Instant) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("w1"), row("w2")]);
        let brigade = brigade_config();
        let now = test_instant();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 120,
                height: 40,
            },
            now,
        );
        (state, app, brigade, now)
    }

    fn spawn_all(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        now: Instant,
    ) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for name in ["dir", "w1", "w2"] {
            cmds.extend(update(
                state,
                app,
                brigade,
                Event::Spawned { key: key_of(name) },
                now,
            ));
        }
        cmds
    }

    #[test]
    fn a_stored_layout_is_in_state_before_the_first_sizing_so_no_pane_is_sized_to_the_default() {
        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        let resolve = resolve_brigade_with(
            &mut state,
            &mut app,
            &brigade,
            Some(stored_arrangement()),
            now,
        );
        assert!(
            !any_preference_write(&resolve),
            "adopting what the store holds is not a change"
        );
        let cmds = spawn_all(&mut state, &mut app, &brigade, now);

        // The stored order, weights and share are what is on screen.
        assert_eq!(order_of(&state), ["dir", "w2", "w1"]);
        let pane = state.geometry(DRAG_AREA).areas.pane;
        let expected_master = crate::layout::master_width(pane.width, 700);
        assert_eq!(tile_of(&state, "dir").width, expected_master);
        let (w2, w1) = (tile_of(&state, "w2"), tile_of(&state, "w1"));
        assert!(w1.height > w2.height, "weights 3 : 1");

        // Every size the master was ever given is either the lone full-width
        // pane or the stored share: never the default 50% split.
        let default_master = crate::layout::master_width(pane.width, DEFAULT_MASTER_PERMILLE);
        let master_widths: Vec<u16> = resizes(&cmds)
            .into_iter()
            .filter(|(key, _, _)| key.as_str() == "dir")
            .map(|(_, _, cols)| cols)
            .collect();
        assert!(!master_widths.is_empty());
        assert!(
            master_widths.iter().all(|cols| *cols
                != pane_content(Rect {
                    width: default_master,
                    ..pane
                })
                .width),
            "{master_widths:?}"
        );
        assert_eq!(
            *master_widths.last().unwrap(),
            pane_content(tile_of(&state, "dir")).width
        );
        // The Workers were sized to their stored tiles at their last resize.
        for (name, tile) in [("w1", w1), ("w2", w2)] {
            let last = resizes(&cmds)
                .into_iter()
                .rev()
                .find(|(key, _, _)| key.as_str() == name)
                .expect(name);
            let content = pane_content(tile);
            assert_eq!((last.1, last.2), (content.height, content.width), "{name}");
        }
    }

    #[test]
    fn a_stored_collapsed_tile_is_never_sized_and_a_stored_solo_collapses_the_others() {
        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        let mut stored = stored_arrangement();
        stored.members[1].minimized = true; // worker-1
        resolve_brigade_with(&mut state, &mut app, &brigade, Some(stored), now);
        let cmds = spawn_all(&mut state, &mut app, &brigade, now);
        assert_eq!(collapsed_names(&state), ["w1"]);
        assert!(
            resizes(&cmds)
                .iter()
                .all(|(key, _, _)| key.as_str() != "w1"),
            "a stored collapsed tile keeps the size it was spawned at: {cmds:?}"
        );

        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        let mut stored = stored_arrangement();
        stored.solo_member = Some("worker-2".to_string());
        resolve_brigade_with(&mut state, &mut app, &brigade, Some(stored), now);
        spawn_all(&mut state, &mut app, &brigade, now);
        assert_eq!(collapsed_names(&state), ["w1"]);
        assert_eq!(state.soloed_key(), Some(&key_of("w2")));
    }

    #[test]
    fn what_this_run_holds_wins_over_a_later_stored_copy() {
        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        resolve_brigade_with(
            &mut state,
            &mut app,
            &brigade,
            Some(stored_arrangement()),
            now,
        );
        spawn_all(&mut state, &mut app, &brigade, now);
        // The operator rearranges: the change is written.
        let cmds = move_onto(&mut state, &mut app, &brigade, "w1", "w2", now);
        assert_eq!(layout_writes(&cmds).len(), 1);
        assert_eq!(order_of(&state), ["dir", "w1", "w2"]);

        // Staged again (the shell reads the store, which a stale copy of is
        // what `stored_arrangement` is): memory is not overwritten.
        state.stage = Stage::Empty;
        let cmds = resolve_brigade_with(
            &mut state,
            &mut app,
            &brigade,
            Some(stored_arrangement()),
            now,
        );
        assert!(!any_preference_write(&cmds));
        assert_eq!(layout_of(&state).stack_order, ["worker-1", "worker-2"]);
        assert_eq!(layout_of(&state).master_permille, 700);
    }

    #[test]
    fn a_brigade_with_no_stored_arrangement_uses_the_defaults_and_writes_nothing() {
        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        let a = resolve_brigade_with(&mut state, &mut app, &brigade, None, now);
        let b = spawn_all(&mut state, &mut app, &brigade, now);
        assert!(!any_preference_write(&a) && !any_preference_write(&b));
        assert_eq!(order_of(&state), ["dir", "w1", "w2"]);
        assert!(state.brigade_layouts.is_empty());
        // An empty stored layout is the same as none.
        let (mut state, mut app, brigade, now) = fresh_state_sized_120x40();
        resolve_brigade_with(
            &mut state,
            &mut app,
            &brigade,
            Some(StoredLayout::default()),
            now,
        );
        let cmds = spawn_all(&mut state, &mut app, &brigade, now);
        assert!(!any_preference_write(&cmds));
        assert_eq!(order_of(&state), ["dir", "w1", "w2"]);
    }

    #[test]
    fn the_stored_sidebar_width_arrives_as_an_event_applies_clamps_and_is_not_written_back() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::SidebarWidthLoaded { width: Some(50) },
            now,
        );
        assert!(!any_preference_write(&cmds));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 120,
                height: 40,
            },
            now,
        );
        assert_eq!(state.geometry(Rect::new(0, 0, 120, 40)).areas.pane.x, 50);

        // Clamped at use, never rewritten: a small terminal squeezes it, a
        // big one gets it back.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 60,
                height: 40,
            },
            now,
        );
        assert!(state.geometry(Rect::new(0, 0, 60, 40)).areas.pane.x <= 40);
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 160,
                height: 40,
            },
            now,
        );
        assert!(!any_preference_write(&cmds));
        assert_eq!(state.geometry(Rect::new(0, 0, 160, 40)).areas.pane.x, 50);

        // Nothing stored: the default.
        let mut state = EmporiumState::new(PrefixKey::default());
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::SidebarWidthLoaded { width: None },
            now,
        );
        assert_eq!(state.sidebar_width, SIDEBAR_WIDTH);
        // An absurd stored width is still only a preference.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::SidebarWidthLoaded { width: Some(1000) },
            now,
        );
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Resized {
                width: 100,
                height: 40,
            },
            now,
        );
        let geometry = state.geometry(Rect::new(0, 0, 100, 40));
        assert!(geometry.areas.pane.width >= MIN_PANE_AREA_WIDTH);
    }

    // --- naming a brigade to the operator --------------------------------------------

    fn titled(id: &str, title: Option<&str>) -> SessionRow {
        let mut r = row(id);
        r.title = title.map(str::to_string);
        r
    }

    fn name_facts(id: BrigadeId, name: &str, director: Option<&str>) -> BrigadeNameFacts {
        BrigadeNameFacts {
            brigade_id: id,
            name: name.to_string(),
            director_session: director.map(str::to_string),
        }
    }

    fn state_with_facts(facts: Vec<BrigadeNameFacts>) -> EmporiumState {
        let mut state = EmporiumState::new(PrefixKey::default());
        for f in facts {
            state.brigade_names.insert(f.brigade_id, f);
        }
        state
    }

    #[test]
    fn a_brigade_is_named_by_its_directors_current_title_then_its_stored_name_then_its_number() {
        let state = state_with_facts(vec![name_facts(1, "Frozen at formation", Some("dir"))]);

        // The Director's row has a title: that wins over the frozen one.
        let app = app_with(vec![titled("dir", Some("Fix the relay draft guard"))]);
        assert_eq!(state.brigade_name(&app, 1), "\"Fix the relay draft guard\"");
        // The title follows a rename: same state, a reloaded row.
        let app = app_with(vec![titled("dir", Some("Renamed by the operator"))]);
        assert_eq!(state.brigade_name(&app, 1), "\"Renamed by the operator\"");

        // No row for the Director session: the stored name.
        let app = app_with(vec![]);
        assert_eq!(state.brigade_name(&app, 1), "\"Frozen at formation\"");
        // A row without a title would name the brigade by a uuid: not used.
        let app = app_with(vec![titled("dir", None)]);
        assert_eq!(state.brigade_name(&app, 1), "\"Frozen at formation\"");
        // No Director session recorded yet: the stored name.
        let state = state_with_facts(vec![name_facts(1, "Frozen at formation", None)]);
        assert_eq!(
            state.brigade_name(&app_with(vec![]), 1),
            "\"Frozen at formation\""
        );

        // Nothing known at all: the number, and only then.
        let state = state_with_facts(vec![]);
        assert_eq!(state.brigade_name(&app_with(vec![]), 7), "brigade 7");
        // A stored name that is only whitespace is no name.
        let state = state_with_facts(vec![name_facts(7, " \n\t ", None)]);
        assert_eq!(state.brigade_name(&app_with(vec![]), 7), "brigade 7");
    }

    #[test]
    fn a_title_is_one_line_and_cut_by_display_width_with_an_ellipsis() {
        let name = |title: &str| {
            let state = state_with_facts(vec![name_facts(1, "x", Some("dir"))]);
            state.brigade_name(&app_with(vec![titled("dir", Some(title))]), 1)
        };
        let width = |text: &str| unicode_width::UnicodeWidthStr::width(text);

        // 200 characters: at most 28 columns inside the quotes, ending in an ellipsis.
        let long = name(&"a".repeat(200));
        assert_eq!(long, format!("\"{}\u{2026}\"", "a".repeat(27)));
        assert_eq!(width(long.trim_matches('"')), 28);
        // Exactly 28 stays whole.
        assert_eq!(name(&"b".repeat(28)), format!("\"{}\"", "b".repeat(28)));

        // Wide characters take two columns and are not split: 13 fit (26
        // columns) with the ellipsis making 27.
        let wide = name(&"\u{30c6}".repeat(40));
        let inner = wide.trim_matches('"');
        assert!(width(inner) <= 28, "{inner:?}");
        assert!(inner.ends_with('\u{2026}'));
        assert_eq!(inner.chars().filter(|c| *c == '\u{30c6}').count(), 13);

        // Newlines, tabs and runs of spaces collapse to single spaces.
        assert_eq!(name("fix\n\n the\ttab  problem"), "\"fix the tab problem\"");
    }

    /// The name `brigade_name` gives a Director titled `title`, a brigade
    /// stored as `stored`.
    fn named(title: &str, stored: &str) -> String {
        let state = state_with_facts(vec![name_facts(1, stored, Some("dir"))]);
        state.brigade_name(&app_with(vec![titled("dir", Some(title))]), 1)
    }

    #[test]
    fn a_title_and_a_stored_name_are_reduced_to_what_is_safe_to_print() {
        // Neither the title nor the stored name may carry a byte that steers the terminal.
        let hostile = "A\u{1b}[2Jb\u{1b}]0;x\u{7} c\u{9b}1m \u{202e}d\u{2066}\u{200b}e\u{200d}";
        let reduced = "\"A[2Jb]0;x c1m de\"";
        assert_eq!(named(hostile, "x"), reduced);
        assert_eq!(named("", hostile), reduced);
        let state = state_with_facts(vec![name_facts(1, hostile, None)]);
        assert_eq!(state.brigade_name(&app_with(vec![]), 1), reduced);
        for c in [
            '\u{1b}', '\u{7}', '\u{9b}', '\u{202e}', '\u{2066}', '\u{200b}', '\u{200d}',
        ] {
            assert!(!named(hostile, hostile).contains(c), "{c:?}");
        }
        // C0 and C1 controls on their own.
        assert_eq!(named("a\u{0}b\u{8}c\u{7f}d\u{85}e", "x"), "\"abcd e\"");
        // A joiner inside a word.
        assert_eq!(named("re\u{200d}lay", "x"), "\"relay\"");
    }

    #[test]
    fn a_name_that_reduces_to_nothing_falls_through_to_the_next_source() {
        // A title of only invisible or control characters is no title.
        assert_eq!(named("\u{1b}\u{200b}\u{202e}", "Frozen"), "\"Frozen\"");
        assert_eq!(named("   \t\n", "Frozen"), "\"Frozen\"");
        // A stored name that reduces to nothing too: the number.
        let state = state_with_facts(vec![name_facts(4, "\u{1b}\u{2067}", Some("dir"))]);
        assert_eq!(
            state.brigade_name(&app_with(vec![titled("dir", Some(" \u{200b} "))]), 4),
            "brigade 4"
        );
        // Of two candidate Director sessions the first with a usable title wins.
        let mut state = state_with_facts(vec![name_facts(1, "Frozen", Some("old"))]);
        let new = key_of("new");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(new.clone()),
            panes: vec![new],
            focused: 0,
        };
        let app = app_with(vec![
            titled("new", Some("\u{1b}")),
            titled("old", Some("Older but usable")),
        ]);
        assert_eq!(state.brigade_name(&app, 1), "\"Older but usable\"");
    }

    #[test]
    fn an_ascii_double_quote_inside_a_name_becomes_an_apostrophe() {
        assert_eq!(named(r#"say "hi" to it"#, "x"), "\"say 'hi' to it\"");
        assert_eq!(named("", r#""quoted""#), "\"'quoted'\"");
        // The outer quotes are the only double quotes left.
        assert_eq!(named(r#"a"b"c"#, "x").matches('"').count(), 2);
    }

    #[test]
    fn a_cut_name_never_strands_a_combining_mark_or_splits_a_flag_or_an_emoji() {
        let width = |text: &str| unicode_width::UnicodeWidthStr::width(text);
        // The 28th column is a letter with a mark after it: 27 letters fit
        // before the ellipsis, and the mark is not left at the head of what was cut.
        let marked = format!("{}e\u{301}tail", "a".repeat(26));
        let cut = named(&marked, "x");
        assert_eq!(cut, format!("\"{}e\u{301}\u{2026}\"", "a".repeat(26)));
        // A joiner-held family is reduced to its parts first, so the cut falls
        // between whole emoji, and the row never exceeds its columns.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}".repeat(8);
        let cut = named(&family, "x");
        let inner = cut.trim_matches('"');
        assert!(width(inner) <= 28, "{inner:?}");
        assert!(inner.ends_with('\u{2026}'));
        assert!(!inner.contains('\u{200d}'));
        // Flags and heart + selector.
        for text in [
            "\u{1f1ef}\u{1f1f5}".repeat(20),
            "\u{2764}\u{fe0f}".repeat(30),
        ] {
            let cut = named(&text, "x");
            let inner = cut.trim_matches('"');
            assert!(width(inner) <= 28, "{inner:?}");
            let body = inner.strip_suffix('\u{2026}').unwrap();
            if text.starts_with('\u{1f1ef}') {
                assert_eq!(body.chars().count() % 2, 0, "{inner:?}");
            }
        }
    }

    #[test]
    fn the_quoted_message_line_is_reduced_the_same_way() {
        // Defence in depth: a glance from an old capture was not reduced by the store.
        let status = nudge_status_with(Some(said_by(
            Some("worker-3"),
            "do\u{1b}[2J \u{202e}it\u{200b} now",
        )));
        assert!(
            status.ends_with("\u{2190} worker-3: do[2J it now"),
            "{status}"
        );
        // A line of nothing but invisible characters is no line.
        assert_eq!(
            nudge_status_with(Some(said_by(Some("worker-3"), "\u{1b}\u{200b}"))),
            "relay: nudged director of \"Fix the relay draft guard\""
        );
    }

    #[test]
    fn the_name_prefers_what_the_core_holds_about_the_director_over_a_stale_tick() {
        // The store (and so the last tick) still says the Director is `old`;
        // a compaction fork re-keyed the live pane to `new`, whose row has the
        // current title. State-first is what covers the tick's one-second gap.
        let mut state = state_with_facts(vec![name_facts(1, "Frozen", Some("old"))]);
        let new = key_of("new");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(new.clone()),
            panes: vec![new],
            focused: 0,
        };
        let app = app_with(vec![
            titled("old", Some("Old title")),
            titled("new", Some("Current title")),
        ]);
        assert_eq!(state.brigade_name(&app, 1), "\"Current title\"");

        // Off-stage with its panes alive: found through the member maps.
        let mut state = state_with_facts(vec![]);
        let director = key_of("offstage-dir");
        state
            .member_tokens
            .insert(director.clone(), DIRECTOR_TOKEN.to_string());
        state.member_brigades.insert(director, 2);
        let app = app_with(vec![titled("offstage-dir", Some("Quiet brigade"))]);
        assert_eq!(state.brigade_name(&app, 2), "\"Quiet brigade\"");
        // ...and another brigade's Director is not this one's.
        assert_eq!(state.brigade_name(&app, 3), "brigade 3");

        // Director pane closed (no key anywhere in core): only the store's
        // session id, from the tick, can say.
        let state = state_with_facts(vec![name_facts(4, "Frozen", Some("closed-dir"))]);
        let app = app_with(vec![titled("closed-dir", Some("Closed but known"))]);
        assert_eq!(state.brigade_name(&app, 4), "\"Closed but known\"");
    }

    #[test]
    fn a_brigade_formed_just_now_has_its_name_before_any_tick() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![titled("dir", Some("Ship the persistence stage"))]);
        let brigade = brigade_config();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::BrigadeFormed {
                director_row_id: "dir".to_string(),
                name: "Ship the persistence stage".to_string(),
                cwd: PathBuf::from("/work"),
                worker_agent: AgentKind::ClaudeCode,
                worker_model: String::new(),
                result: Ok((9, vec!["worker-1".to_string()])),
            },
            test_instant(),
        );

        assert_eq!(
            state.brigade_name(&app, 9),
            "\"Ship the persistence stage\""
        );
        // With the Director's row gone the frozen name still answers.
        assert_eq!(
            state.brigade_name(&app_with(vec![]), 9),
            "\"Ship the persistence stage\""
        );
    }

    #[test]
    fn a_tick_replaces_a_brigades_facts_and_the_name_follows_a_director_session_change() {
        let (mut state, _, brigade, now) = drag_fixture(1);
        let tick = |state: &mut EmporiumState, app: &mut App, facts: Vec<BrigadeNameFacts>| {
            update(
                state,
                app,
                &brigade,
                Event::Tick {
                    relay: vec![],
                    brigades: facts,
                },
                now,
            );
        };
        // Brigade 5 has no pane in core at all: only the tick knows it.
        let mut app = app_with(vec![
            titled("first", Some("Before the fork")),
            titled("second", Some("After the fork")),
        ]);
        tick(
            &mut state,
            &mut app,
            vec![name_facts(5, "Frozen", Some("first"))],
        );
        assert_eq!(state.brigade_name(&app, 5), "\"Before the fork\"");
        tick(
            &mut state,
            &mut app,
            vec![name_facts(5, "Frozen", Some("second"))],
        );
        assert_eq!(state.brigade_name(&app, 5), "\"After the fork\"");
        // A tick that lists nothing (a failed gather) keeps what was known.
        tick(&mut state, &mut app, vec![]);
        assert_eq!(state.brigade_name(&app, 5), "\"After the fork\"");
    }

    #[test]
    fn disbanding_forgets_the_brigades_name_facts() {
        let (mut state, mut app, brigade, now) = drag_fixture(1);
        state
            .brigade_names
            .insert(1, name_facts(1, "Gone soon", Some("dir")));
        state.brigade_names.insert(2, name_facts(2, "Stays", None));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );
        assert!(!state.brigade_names.contains_key(&1));
        assert!(state.brigade_names.contains_key(&2));
    }

    #[test]
    fn members_are_grouped_by_brigade_so_each_name_is_said_once() {
        let mut state = state_with_facts(vec![
            name_facts(1, "Alpha", None),
            name_facts(2, "Beta", None),
            name_facts(3, "Gamma", None),
        ]);
        state.brigade_names.get_mut(&3).unwrap().name = "Gamma".to_string();
        let app = app_with(vec![]);
        let m = |id: Option<BrigadeId>, token: &str| (id, token.to_string());

        // One member.
        assert_eq!(
            state.members_phrase(&app, &[m(Some(2), "worker-2")]),
            "worker-2 of \"Beta\""
        );
        // The Director is a member like any other.
        assert_eq!(
            state.members_phrase(&app, &[m(Some(1), "director")]),
            "director of \"Alpha\""
        );
        // Several of one brigade: in the order given.
        assert_eq!(
            state.members_phrase(&app, &[m(Some(1), "worker-3"), m(Some(1), "worker-1")]),
            "worker-3, worker-1 of \"Alpha\""
        );
        // Two brigades: grouped by first appearance, members in order.
        assert_eq!(
            state.members_phrase(
                &app,
                &[
                    m(Some(2), "worker-2"),
                    m(Some(1), "worker-1"),
                    m(Some(2), "worker-4")
                ]
            ),
            "worker-2, worker-4 of \"Beta\" and worker-1 of \"Alpha\""
        );
        // Three: separated by ';' since a group's own members use commas.
        assert_eq!(
            state.members_phrase(
                &app,
                &[
                    m(Some(1), "worker-1"),
                    m(Some(1), "worker-2"),
                    m(Some(2), "worker-1"),
                    m(Some(3), "director")
                ]
            ),
            "worker-1, worker-2 of \"Alpha\"; worker-1 of \"Beta\" and director of \"Gamma\""
        );
        // No brigade known for a pane: it stands alone, as before.
        assert_eq!(
            state.members_phrase(&app, &[m(None, "worker-1")]),
            "member worker-1"
        );
        // Nothing known about a brigade: its number, said once.
        let bare = state_with_facts(vec![]);
        assert_eq!(
            bare.members_phrase(&app, &[m(Some(8), "worker-1"), m(Some(8), "worker-2")]),
            "worker-1, worker-2 of brigade 8"
        );
    }

    #[test]
    fn the_kickoff_statuses_name_the_brigade_by_its_directors_title() {
        let mut state = state_with_facts(vec![
            name_facts(1, "Frozen A", Some("dir-a")),
            name_facts(2, "Frozen B", Some("dir-b")),
        ]);
        let mut app = app_with(vec![
            titled("dir-a", Some("Fix the relay draft guard")),
            titled("dir-b", Some("Second task")),
        ]);
        let brigade = brigade_config();
        let now = test_instant();
        let member = |state: &mut EmporiumState, key: &str, token: &str, brigade: BrigadeId| {
            let key = key_of(key);
            state.member_tokens.insert(key.clone(), token.to_string());
            state.member_brigades.insert(key.clone(), brigade);
            key
        };
        let (w1, w3, b1) = (
            member(&mut state, "w1", "worker-1", 1),
            member(&mut state, "w3", "worker-3", 1),
            member(&mut state, "b1", "worker-1", 2),
        );
        for key in [&w1, &b1, &w3] {
            state.pending_kickoffs.push(PendingKickoff {
                key: key.clone(),
                spawned_at: now,
                cwd: PathBuf::from("/work"),
                notified_untrusted: true,
                sent_at: None,
                attempts: 0,
            });
        }

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: vec![],
            },
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some(
                "kickoff: waiting for directory trust in worker-1, worker-3 of \"Fix the relay draft guard\" \
                 and worker-1 of \"Second task\""
            )
        );

        // The acknowledgement family (the trust one is gone from the list).
        state.status = None;
        state.status_set_at = None;
        for pending in &mut state.pending_kickoffs {
            pending.notified_untrusted = false;
            pending.attempts = KICKOFF_MAX_ATTEMPTS;
        }
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: vec![],
            },
            now + Duration::from_secs(1),
        );
        let status = state.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("kickoff: no acknowledgement after 3 attempts — worker-1, worker-3 of \"Fix the relay"),
            "{status}"
        );
        assert!(
            status.ends_with("and worker-1 of \"Second task\""),
            "{status}"
        );

        // A pane with no brigade keeps the old fallback.
        let lone = key_of("lone");
        state
            .member_tokens
            .insert(lone.clone(), "worker-9".to_string());
        assert_eq!(state.kickoff_label(&app, &lone), "member worker-9");
    }

    #[test]
    fn the_single_member_trust_notices_use_the_same_wording_for_codex_and_claude() {
        let mut state = state_with_facts(vec![name_facts(1, "Frozen", Some("dir"))]);
        let mut app = app_with(vec![titled("dir", Some("Fix the relay draft guard"))]);
        let brigade = brigade_config();
        let now = test_instant();
        let worker = key_of("w1");
        state
            .member_tokens
            .insert(worker.clone(), "worker-1".to_string());
        state.member_brigades.insert(worker.clone(), 1);
        state.pending_kickoffs.push(PendingKickoff {
            key: worker.clone(),
            spawned_at: now,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDirectoryTrustChecked {
                key: worker,
                trusted: false,
            },
            now,
        );
        assert_eq!(
            state
                .status
                .as_deref()
                .map(|s| s.split(" — ").next().unwrap()),
            Some(
                "worker-1 of \"Fix the relay draft guard\": Codex hasn't been told to trust this directory yet"
            )
        );

        let goinkyo = key_of("g1");
        state
            .member_tokens
            .insert(goinkyo.clone(), "goinkyo".to_string());
        state.member_brigades.insert(goinkyo.clone(), 1);
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: goinkyo.clone(),
            spawned_at: now,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: goinkyo,
                trusted: false,
            },
            now,
        );
        assert!(
            state.status.as_deref().is_some_and(|s| s
                .starts_with("goinkyo of \"Fix the relay draft guard\": Claude hasn't been told")),
            "{:?}",
            state.status
        );
    }

    #[test]
    fn the_relay_statuses_name_the_brigade_one_family_at_a_time() {
        // Nudge: the status right after a nudge is typed.
        let (mut state, _, brigade, director, start) = reading_fixture();
        state
            .brigade_names
            .insert(1, name_facts(1, "Frozen", Some("a-dir")));
        let mut app = app_with(vec![titled("a-dir", Some("Fix the relay draft guard"))]);
        let seen = Seen {
            agent: CLAUDE,
            idle: Some(true),
            has_unseen: true,
        };
        tick_obs(&mut state, &mut app, &brigade, &director, seen, start);
        tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            seen,
            start + Duration::from_secs(1),
        );
        assert_eq!(
            state.status.as_deref(),
            Some("relay: nudged director of \"Fix the relay draft guard\"")
        );

        // Cannot read the input line.
        let (mut state, _, brigade, director, start) = reading_fixture();
        state
            .brigade_names
            .insert(1, name_facts(1, "Frozen", Some("a-dir")));
        let mut app = app_with(vec![titled("a-dir", Some("Fix the relay draft guard"))]);
        let idle = Seen {
            agent: CLAUDE,
            idle: Some(true),
            has_unseen: false,
        };
        tick_obs(&mut state, &mut app, &brigade, &director, idle, start);
        tick_obs(
            &mut state,
            &mut app,
            &brigade,
            &director,
            idle,
            start + INPUT_LINE_UNREADABLE_AFTER,
        );
        assert_eq!(
            state.status.as_deref(),
            Some(
                "relay: cannot read the input line of director of \"Fix the relay draft guard\" — \
                 unsent-draft detection is back to keystroke tracking"
            )
        );
    }

    /// A tick for the fixture's Director carrying `glance`.
    fn tick_with_glance(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        key: &SessionKey,
        glance: Option<MemberGlance>,
        now: Instant,
    ) {
        update(
            state,
            app,
            brigade,
            Event::Tick {
                relay: vec![RelayObservation {
                    glance,
                    brigade_id: 1,
                    token: "director".to_string(),
                    key: key.clone(),
                    has_unseen: true,
                    is_idle_this_tick: Some(true),
                    agent: CLAUDE,
                }],
                brigades: Vec::new(),
            },
            now,
        );
    }

    fn said_by(sender: Option<&str>, line: &str) -> MemberGlance {
        MemberGlance {
            inbound: Some(MessageGlance {
                created_at_ms: 1,
                line: line.to_string(),
                from_token: sender.map(str::to_string),
            }),
            sent: None,
        }
    }

    /// The status the second of two idle ticks leaves, when that second
    /// tick's observation carries `glance`.
    fn nudge_status_with(glance: Option<MemberGlance>) -> String {
        let (mut state, _, brigade, director, start) = reading_fixture();
        state
            .brigade_names
            .insert(1, name_facts(1, "Frozen", Some("a-dir")));
        let mut app = app_with(vec![titled("a-dir", Some("Fix the relay draft guard"))]);
        tick_with_glance(&mut state, &mut app, &brigade, &director, None, start);
        tick_with_glance(
            &mut state,
            &mut app,
            &brigade,
            &director,
            glance,
            start + Duration::from_secs(1),
        );
        state.status.clone().unwrap_or_default()
    }

    #[test]
    fn the_nudge_status_quotes_the_sender_and_first_line_of_the_message_that_prompted_it() {
        let status = nudge_status_with(Some(said_by(
            Some("worker-3"),
            "stage 2 is done in the working tree",
        )));
        assert_eq!(
            status,
            "relay: nudged director of \"Fix the relay draft guard\" \u{2190} worker-3: stage 2 is done in the working tree"
        );
    }

    #[test]
    fn the_glance_of_the_very_tick_that_nudges_is_the_one_quoted() {
        // The member's facts are filled at the top of `update_tick`, before the
        // nudge decision: the first tick (idle streak 1) carried an older
        // message, the second (the nudging one) a newer, and the status quotes
        // the newer.
        let (mut state, _, brigade, director, start) = reading_fixture();
        let mut app = app_with(vec![]);
        tick_with_glance(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Some(said_by(Some("worker-1"), "the old one")),
            start,
        );
        tick_with_glance(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Some(said_by(Some("worker-2"), "the one that just arrived")),
            start + Duration::from_secs(1),
        );
        let status = state.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("relay: nudged director of brigade 1"),
            "{status}"
        );
        assert!(
            status.ends_with("\u{2190} worker-2: the one that just arrived"),
            "{status}"
        );
        // A tick that re-reads nothing (`glance: None`) quotes what the core held.
        let (mut state, _, brigade, director, start) = reading_fixture();
        let mut app = app_with(vec![]);
        tick_with_glance(
            &mut state,
            &mut app,
            &brigade,
            &director,
            Some(said_by(Some("worker-1"), "kept")),
            start,
        );
        tick_with_glance(
            &mut state,
            &mut app,
            &brigade,
            &director,
            None,
            start + Duration::from_secs(1),
        );
        assert!(
            state
                .status
                .clone()
                .unwrap_or_default()
                .ends_with("\u{2190} worker-1: kept")
        );
    }

    #[test]
    fn the_nudge_status_is_the_short_form_when_there_is_nothing_to_quote() {
        let short = "relay: nudged director of \"Fix the relay draft guard\"";
        // No glance at all.
        assert_eq!(nudge_status_with(None), short);
        // A glance with no inbound half.
        assert_eq!(nudge_status_with(Some(MemberGlance::default())), short);
        // A message whose first line is empty: never an arrow with nothing after it.
        assert_eq!(
            nudge_status_with(Some(said_by(Some("worker-3"), ""))),
            short
        );
        assert_eq!(
            nudge_status_with(Some(said_by(Some("worker-3"), "  \t "))),
            short
        );
        // A sender that is not recorded (older data): the line alone.
        assert_eq!(
            nudge_status_with(Some(said_by(None, "no sender here"))),
            format!("{short} \u{2190} no sender here")
        );
    }

    #[test]
    fn the_quoted_line_is_cut_by_display_width_to_sixty_columns() {
        let width = |text: &str| unicode_width::UnicodeWidthStr::width(text);
        let quoted = |line: &str| {
            let status = nudge_status_with(Some(said_by(Some("worker-3"), line)));
            let (_, tail) = status.split_once(" \u{2190} worker-3: ").expect("an arrow");
            tail.to_string()
        };

        let long = quoted(&"a".repeat(200));
        assert_eq!(width(&long), 60);
        assert_eq!(long, format!("{}\u{2026}", "a".repeat(59)));
        // Exactly 60 stays whole.
        assert_eq!(quoted(&"b".repeat(60)), "b".repeat(60));
        // Wide characters: two columns each, never split, ellipsis included.
        let wide = quoted(&"\u{30c6}".repeat(60));
        assert!(width(&wide) <= 60, "{wide:?}");
        assert!(wide.ends_with('\u{2026}'));
        assert_eq!(wide.chars().filter(|c| *c == '\u{30c6}').count(), 29);
    }

    #[test]
    fn a_glance_recorded_without_a_sender_still_parses() {
        let old: MessageGlance =
            serde_json::from_str(r#"{"created_at_ms":5,"line":"hello"}"#).unwrap();
        assert_eq!(old.from_token, None);
        let new = MessageGlance {
            created_at_ms: 5,
            line: "hi".to_string(),
            from_token: Some("worker-2".to_string()),
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<MessageGlance>(&json).unwrap(), new);
    }

    #[test]
    fn the_holding_status_groups_members_of_two_brigades_in_the_order_first_seen() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let panes: Vec<(SessionKey, &str, BrigadeId)> = vec![
            (key_of("a1"), "worker-1", 1),
            (key_of("b2"), "worker-2", 2),
            (key_of("a3"), "worker-3", 1),
        ];
        for (key, token, brigade_id) in &panes {
            state.screens.insert(key.clone(), Screen::new(24, 80));
            state.member_tokens.insert(key.clone(), token.to_string());
            state.member_brigades.insert(key.clone(), *brigade_id);
            state.unsent_operator_input.insert(key.clone());
        }
        state
            .brigade_names
            .insert(1, name_facts(1, "Frozen", Some("dir-a")));
        state
            .brigade_names
            .insert(2, name_facts(2, "Frozen", Some("dir-b")));
        let mut app = app_with(vec![
            titled("dir-a", Some("A task")),
            titled("dir-b", Some("B task")),
        ]);
        let brigade = brigade_config();
        let now = test_instant();
        let relay = |at: Duration| {
            let obs: Vec<RelayObservation> = panes
                .iter()
                .map(|(key, token, brigade_id)| RelayObservation {
                    glance: None,
                    brigade_id: *brigade_id,
                    token: token.to_string(),
                    key: key.clone(),
                    has_unseen: true,
                    is_idle_this_tick: Some(true),
                    agent: None,
                })
                .collect();
            (obs, now + at)
        };
        for second in 0..2 {
            let (obs, at) = relay(Duration::from_secs(second));
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Tick {
                    relay: obs,
                    brigades: vec![],
                },
                at,
            );
        }
        assert_eq!(
            state.status.as_deref(),
            Some(
                "relay: holding the nudge to worker-1, worker-3 of \"A task\" and worker-2 of \"B task\" \
                 — unsent input in the pane"
            )
        );
    }

    #[test]
    fn old_tick_captures_without_brigade_facts_still_parse_and_new_ones_round_trip() {
        let old: Event = serde_json::from_str(r#"{"Tick":{"relay":[]}}"#).unwrap();
        assert_eq!(
            old,
            Event::Tick {
                relay: vec![],
                brigades: vec![]
            }
        );

        let new = Event::Tick {
            relay: vec![],
            brigades: vec![
                name_facts(3, "A title", Some("dir")),
                name_facts(4, "No director", None),
            ],
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), new);
    }

    #[test]
    fn the_new_event_fields_round_trip_and_older_captures_still_parse() {
        let loaded = Event::SidebarWidthLoaded { width: Some(44) };
        let json = serde_json::to_string(&loaded).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), loaded);

        let resolved = Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, "director".to_string(), BrigadeRole::Director)),
            members: Some(vec![]),
            layout: Some(stored_arrangement()),
        };
        let json = serde_json::to_string(&resolved).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), resolved);

        // A capture from before `layout` existed.
        let old = r#"{"MembershipResolved":{"session_id":"dir","membership":null,"members":null}}"#;
        let Event::MembershipResolved { layout, .. } = serde_json::from_str::<Event>(old).unwrap()
        else {
            panic!("expected MembershipResolved");
        };
        assert_eq!(layout, None);

        // The intents round trip too (they sit in recorded Cmd histories).
        let intent = StoreIntent::SaveBrigadeLayout {
            brigade_id: 2,
            layout: stored_arrangement(),
        };
        let json = serde_json::to_string(&intent).unwrap();
        assert_eq!(serde_json::from_str::<StoreIntent>(&json).unwrap(), intent);
    }

    #[test]
    fn the_keys_m_z_and_capital_m_refuse_with_a_reason_where_they_make_no_sense() {
        // The sidebar.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        state.focus = Focus::Sidebar;
        for (key, mods, expected) in [
            ('m', Modifiers::NONE, "prefix: focus a tile to collapse it"),
            ('z', Modifiers::NONE, "prefix: focus a tile to solo it"),
        ] {
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char(key),
                mods,
                now,
            );
            assert_eq!(state.status.as_deref(), Some(expected));
        }
        // The master.
        focus_stack_slot(&mut state, 0);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prefix: the master tile can't be collapsed")
        );
        // The master takes a solo (and turns it off again): it has a member.
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("director"));
        assert!(layout_of(&state).minimized.is_empty());
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member, None);

        // A master with no member token has nothing to key a solo by.
        state.member_tokens.remove(&key_of("dir"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prefix: this tile has no member to solo")
        );
        assert_eq!(layout_of(&state).solo_member, None);

        // A solo (single-session) stage.
        let (mut state, mut app, brigade, now) = drag_fixture(0);
        state.stage = Stage::Solo(key_of("dir"));
        for key in ['m', 'z'] {
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char(key),
                Modifiers::NONE,
                now,
            );
            assert!(state.status.is_some(), "{key}");
        }
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('M'),
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(state.status.as_deref(), Some("prefix: no stack to restore"));
    }

    #[test]
    fn the_keys_m_and_z_work_on_the_focused_stack_tile_and_capital_m_with_or_without_shift() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 3);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('z'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-3"));
        assert_eq!(collapsed_names(&state), ["w1", "w2"]);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('m'),
            Modifiers::NONE,
            now,
        );
        assert_eq!(layout_of(&state).minimized, token_set(&["worker-3"]));
        assert_eq!(layout_of(&state).solo_member, None);
        for mods in [Modifiers::NONE, Modifiers::SHIFT] {
            set_collapse(&mut state, &["worker-1"], Some("worker-2"));
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char('M'),
                mods,
                now,
            );
            assert!(layout_of(&state).minimized.is_empty());
            assert_eq!(layout_of(&state).solo_member, None);
        }
    }

    #[test]
    fn dismissal_forgets_collapse_and_solo_and_disband_forgets_everything() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-3", "worker-1"], Some("worker-3"));
        // A foreign brigade's result does nothing.
        state.pending_dismiss = Some(key_of("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));
        update(&mut state, &mut app, &brigade, dismissed(2), now);
        assert_eq!(layout_of(&state).solo_member.as_deref(), Some("worker-3"));

        // (A result for any brigade spends `pending_dismiss`; ask again.)
        state.pending_dismiss = Some(key_of("w3"));
        update(&mut state, &mut app, &brigade, dismissed(1), now);
        assert_eq!(layout_of(&state).minimized, token_set(&["worker-1"]));
        assert_eq!(layout_of(&state).solo_member, None);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );
        assert!(!state.brigade_layouts.contains_key(&1));
    }

    #[test]
    fn a_pane_exit_and_a_restage_keep_the_collapse_preferences() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        set_collapse(&mut state, &["worker-2"], Some("worker-1"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w2"),
                reason: None,
            },
            now,
        );
        state.stage = Stage::Solo(key_of("elsewhere"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        let layout = layout_of(&state);
        assert_eq!(layout.minimized, token_set(&["worker-2"]));
        assert_eq!(layout.solo_member.as_deref(), Some("worker-1"));
    }

    // --- what a member was last told and last said ------------------------------

    fn glance(at_ms: i64, line: &str) -> MessageGlance {
        MessageGlance {
            created_at_ms: at_ms,
            line: line.to_string(),
            from_token: None,
        }
    }

    fn observe(
        name: &str,
        token: &str,
        agent: Option<AgentKind>,
        glance: Option<MemberGlance>,
    ) -> RelayObservation {
        RelayObservation {
            brigade_id: 1,
            token: token.to_string(),
            key: key_of(name),
            has_unseen: false,
            is_idle_this_tick: None,
            agent,
            glance,
        }
    }

    fn tick_with(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        relay: Vec<RelayObservation>,
        now: Instant,
    ) {
        update(
            state,
            app,
            brigade,
            Event::Tick {
                relay,
                brigades: Vec::new(),
            },
            now,
        );
    }

    #[test]
    fn a_relay_observation_fills_the_members_facts_and_the_tile_reads_them() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        assert_eq!(
            state.member_facts(&key_of("w1")),
            None,
            "nothing observed yet"
        );

        tick_with(
            &mut state,
            &mut app,
            &brigade,
            vec![observe(
                "w1",
                "worker-1",
                Some(AgentKind::Codex),
                Some(MemberGlance {
                    inbound: Some(glance(1_000, "run the tests")),
                    sent: Some(glance(2_000, "done")),
                }),
            )],
            now,
        );

        let facts = state.member_facts(&key_of("w1")).expect("observed");
        assert_eq!(facts.agent, Some(AgentKind::Codex));
        assert_eq!(facts.inbound, Some(glance(1_000, "run the tests")));
        assert_eq!(facts.sent, Some(glance(2_000, "done")));
        // Another member is not touched.
        assert_eq!(state.member_facts(&key_of("w2")), None);
    }

    #[test]
    fn no_glance_keeps_what_the_core_has_and_an_empty_one_clears_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(1);
        let full = MemberGlance {
            inbound: Some(glance(1, "in")),
            sent: Some(glance(2, "out")),
        };
        tick_with(
            &mut state,
            &mut app,
            &brigade,
            vec![observe(
                "w1",
                "worker-1",
                Some(AgentKind::ClaudeCode),
                Some(full),
            )],
            now,
        );

        // A tick the shell skipped the reads on, and one where the agent could
        // not be resolved: both leave the facts alone.
        tick_with(
            &mut state,
            &mut app,
            &brigade,
            vec![observe("w1", "worker-1", None, None)],
            now,
        );
        let facts = state.member_facts(&key_of("w1")).unwrap();
        assert_eq!(facts.agent, Some(AgentKind::ClaudeCode));
        assert_eq!(facts.inbound, Some(glance(1, "in")));
        assert_eq!(facts.sent, Some(glance(2, "out")));

        // A real, empty answer is "this member has no mail".
        tick_with(
            &mut state,
            &mut app,
            &brigade,
            vec![observe(
                "w1",
                "worker-1",
                None,
                Some(MemberGlance::default()),
            )],
            now,
        );
        let facts = state.member_facts(&key_of("w1")).unwrap();
        assert_eq!(facts.inbound, None);
        assert_eq!(facts.sent, None);
    }

    #[test]
    fn facts_are_forgotten_on_dismissal_and_disband_but_not_on_exit_or_restage() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let observed = |state: &mut EmporiumState, app: &mut App| {
            for (name, token) in [("w1", "worker-1"), ("w2", "worker-2")] {
                tick_with(
                    state,
                    app,
                    &brigade,
                    vec![observe(
                        name,
                        token,
                        Some(AgentKind::ClaudeCode),
                        Some(MemberGlance {
                            inbound: Some(glance(5, "in")),
                            sent: None,
                        }),
                    )],
                    now,
                );
            }
        };
        observed(&mut state, &mut app);

        // A pane exiting and the brigade going off-stage keep them.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: key_of("w1"),
                reason: None,
            },
            now,
        );
        assert!(
            state
                .member_facts
                .contains_key(&(1, "worker-1".to_string()))
        );
        assert!(
            state
                .member_facts
                .contains_key(&(1, "worker-2".to_string()))
        );

        // Dismissal forgets that member only (and only for its own brigade).
        state.pending_dismiss = Some(key_of("w2"));
        state.pending_dismiss_member = Some((1, "worker-2".to_string()));
        update(&mut state, &mut app, &brigade, dismissed(2), now);
        assert!(
            state
                .member_facts
                .contains_key(&(1, "worker-2".to_string())),
            "foreign result"
        );
        state.pending_dismiss = Some(key_of("w2"));
        update(&mut state, &mut app, &brigade, dismissed(1), now);
        assert!(
            !state
                .member_facts
                .contains_key(&(1, "worker-2".to_string()))
        );
        assert!(
            state
                .member_facts
                .contains_key(&(1, "worker-1".to_string()))
        );

        // Disband forgets the brigade's.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );
        assert!(state.member_facts.is_empty());
    }

    #[test]
    fn a_recorded_observation_round_trips_with_its_glance_and_old_streams_still_parse() {
        let with = observe(
            "w1",
            "worker-1",
            Some(AgentKind::Codex),
            Some(MemberGlance {
                inbound: Some(glance(7, "first line")),
                sent: None,
            }),
        );
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(
            serde_json::from_str::<RelayObservation>(&json).unwrap(),
            with
        );

        // A stream recorded before the field existed has no `glance`.
        let old = r#"{"brigade_id":1,"token":"worker-1","key":"w1","has_unseen":false,"is_idle_this_tick":null}"#;
        let parsed: RelayObservation = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.glance, None);
        assert_eq!(parsed.agent, None);
    }

    #[test]
    fn the_master_label_and_a_stack_of_one_start_no_move() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 2;
        }
        let master = state.geometry(DRAG_AREA).tiles[0].1;
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            master.x + 2,
            master.y,
            now,
        );
        assert!(state.move_drag.is_none() && state.drag.is_none());
        assert_eq!(
            focused_name(&state).as_deref(),
            Some("dir"),
            "it still focuses the master"
        );

        let (mut state, mut app, brigade, now) = drag_fixture(1);
        press_label(&mut state, &mut app, &brigade, "w1", now);
        assert!(state.move_drag.is_none(), "one tile has nowhere to go");
        assert_eq!(focused_name(&state).as_deref(), Some("w1"));
    }

    #[test]
    fn pressing_a_label_focuses_the_tile_exactly_as_before_and_starts_the_move() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        state.focus = Focus::Sidebar;
        let cmds = press_label(&mut state, &mut app, &brigade, "w3", now);
        assert!(cmds.is_empty());
        assert_eq!(state.focus, Focus::Pane);
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
        assert_eq!(
            state.move_drag,
            Some(MoveDrag {
                key: SessionKey::from_id("w3"),
                column: label_cell(&state, "w3").0,
                row: label_cell(&state, "w3").1,
                dragged: false,
            })
        );
    }

    /// A move in progress on `w2` of three Workers, the pointer on `w1`.
    fn moving_w2_up() -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let w1 = tile_of(&state, "w1");
        press_label(&mut state, &mut app, &brigade, "w2", now);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            60,
            w1.y + 1,
            now,
        );
        assert_eq!(state.move_drag.as_ref().map(|m| m.row), Some(w1.y + 1));
        (state, app, brigade, now)
    }

    /// The line a move preview should show for a tile moved down onto `name`:
    /// that tile's bottom edge, across the stack, as the geometry stands now.
    fn bottom_edge_of(state: &EmporiumState, name: &str) -> (Rect, Axis) {
        let tile = tile_of(state, name);
        (
            Rect::new(tile.x, tile.y + tile.height - 1, tile.width, 1),
            Axis::Horizontal,
        )
    }

    #[test]
    fn the_move_preview_follows_the_pointer_when_a_director_arrives_late_under_a_held_button() {
        // The Director has not been staged yet, so a Worker holds `panes[0]`
        // and w2 is the first stack tile.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let dir = SessionKey::from_id("dir");
        state.unstage(&dir);
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(dir.clone()),
            panes: ["w1", "w2", "w3"].map(SessionKey::from_id).to_vec(),
            focused: 0,
        };
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        // Low enough to be over w3 both with two stack tiles and with three.
        let row = 35;
        press_label(&mut state, &mut app, &brigade, "w2", now);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            60,
            row,
            now,
        );
        assert_eq!(overlay_line(&state), Some(bottom_edge_of(&state, "w3")));

        // The Director arrives and takes the front; every index shifts.
        state.pending_opens.insert(
            dir.clone(),
            PendingOpen::BrigadeMember {
                brigade_id: 1,
                needs_codex_kickoff: false,
                cwd: PathBuf::from("/work"),
            },
        );
        state
            .member_tokens
            .insert(dir.clone(), DIRECTOR_TOKEN.to_string());
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned { key: dir },
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3"]);
        assert!(
            state.move_drag.is_some(),
            "the gesture survives the arrival"
        );

        // The pointer has not moved: still over w3, so the line is still at
        // w3's (new) bottom edge, not gone and not on another tile.
        assert!(
            tile_of(&state, "w3").y <= row
                && row < tile_of(&state, "w3").y + tile_of(&state, "w3").height
        );
        assert_eq!(overlay_line(&state), Some(bottom_edge_of(&state, "w3")));

        // And the release lands where the pointer was.
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            60,
            row,
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w1", "w3", "w2"]);
    }

    #[test]
    fn the_move_preview_follows_the_pointer_when_a_pane_above_the_target_goes_away() {
        let (mut state, mut app, brigade, now) = drag_fixture(4);
        let row = tile_of(&state, "w4").y + 6;
        press_label(&mut state, &mut app, &brigade, "w2", now);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Drag(MouseButton::Left),
            60,
            row,
            now,
        );
        assert_eq!(overlay_line(&state), Some(bottom_edge_of(&state, "w4")));

        // w1, above the target, exits: the stack shrinks to three tiles, so
        // the slot index the pointer selected before no longer exists.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("w1"),
                reason: None,
            },
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w2", "w3", "w4"]);
        let w4 = tile_of(&state, "w4");
        assert!(w4.y <= row && row < w4.y + w4.height, "still over w4");
        assert_eq!(overlay_line(&state), Some(bottom_edge_of(&state, "w4")));

        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            60,
            row,
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w3", "w4", "w2"]);
    }

    #[test]
    fn a_resize_preview_is_clamped_to_the_limits_as_they_stand_after_the_pane_list_changes() {
        // Drag the w1/w2 boundary to its lowest allowed place, then a third
        // Worker arrives: w2 is shorter, so the boundary may go less far.
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let boundary = w1_over_w2();
        drag_to(&mut state, &mut app, &brigade, &boundary, 39, now);
        let stale = state.drag.as_ref().unwrap().preview;

        let w3 = SessionKey::from_id("w3");
        state.screens.insert(w3.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(w3.clone(), "worker-3".to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.push(w3);
        }
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        let handle = handle_of(&state, &boundary);
        assert!(
            stale > handle.max,
            "the limit moved under the stored preview"
        );
        let (line, axis) = overlay_line(&state).expect("the drag survives");
        assert_eq!(axis, Axis::Horizontal);
        assert_eq!(
            line.y,
            handle.max - 1,
            "drawn at the limit, on the border before it"
        );
    }

    #[test]
    fn every_cancel_leaves_the_order_untouched() {
        type Cancel = Box<dyn Fn(&mut EmporiumState, &mut App, &BrigadeConfig, Instant)>;
        let cancels: Vec<(&str, Cancel)> = vec![
            (
                "Esc",
                Box::new(|state, app, brigade, now| {
                    forward_key(state, app, brigade, KeyCode::Esc, Modifiers::NONE, now);
                }),
            ),
            (
                "a press of the left button",
                Box::new(|state, app, brigade, now| {
                    mouse_event(
                        state,
                        app,
                        brigade,
                        MouseEventKind::Down(MouseButton::Left),
                        60,
                        5,
                        now,
                    );
                }),
            ),
            (
                "a press of another button",
                Box::new(|state, app, brigade, now| {
                    mouse_event(
                        state,
                        app,
                        brigade,
                        MouseEventKind::Down(MouseButton::Right),
                        60,
                        5,
                        now,
                    );
                }),
            ),
            (
                "a move with no button held",
                Box::new(|state, app, brigade, now| {
                    mouse_event(state, app, brigade, MouseEventKind::Moved, 60, 5, now);
                }),
            ),
            (
                "the window losing focus",
                Box::new(|state, app, brigade, now| {
                    update(
                        state,
                        app,
                        brigade,
                        Event::WindowFocusChanged { focused: false },
                        now,
                    );
                }),
            ),
            (
                "a modal opening",
                Box::new(|state, app, brigade, now| {
                    app.open_new_session_modal();
                    update(
                        state,
                        app,
                        brigade,
                        Event::Tick {
                            relay: vec![],
                            brigades: Vec::new(),
                        },
                        now,
                    );
                }),
            ),
            (
                "a terminal resize",
                Box::new(|state, app, brigade, now| {
                    update(
                        state,
                        app,
                        brigade,
                        Event::Resized {
                            width: 120,
                            height: 40,
                        },
                        now,
                    );
                }),
            ),
            (
                "the tile being moved going away",
                Box::new(|state, app, brigade, now| {
                    state.unstage(&SessionKey::from_id("w2"));
                    update(
                        state,
                        app,
                        brigade,
                        Event::Tick {
                            relay: vec![],
                            brigades: Vec::new(),
                        },
                        now,
                    );
                }),
            ),
        ];
        for (what, cancel) in cancels {
            let (mut state, mut app, brigade, now) = moving_w2_up();
            let before = order_of(&state);
            let focus = (state.focus, focused_name(&state));

            cancel(&mut state, &mut app, &brigade, now);

            assert!(state.move_drag.is_none(), "{what} cancels");
            assert_eq!(state.displayed_status(), None, "{what}: the hint is gone");
            assert_eq!(
                state.drag_overlay(DRAG_AREA),
                None,
                "{what}: the mark and the ghost are gone"
            );
            assert!(
                state.brigade_layouts.is_empty(),
                "{what}: no preference recorded"
            );
            if what == "the tile being moved going away" {
                continue;
            }
            assert_eq!(order_of(&state), before, "{what} leaves the order alone");
            if what != "a modal opening" {
                assert_eq!((state.focus, focused_name(&state)), focus, "{what}: focus");
            }
        }
    }

    #[test]
    fn a_move_in_progress_never_writes_to_a_pane_it_crosses() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        for name in ["dir", "w1", "w2", "w3"] {
            state
                .screens
                .get_mut(&SessionKey::from_id(name))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        let mut cmds = press_label(&mut state, &mut app, &brigade, "w3", now);
        // Over the SGR-mouse master, a stack tile's interior, the sidebar,
        // and finally released over the first tile.
        let w1 = tile_of(&state, "w1");
        for (kind, column, row) in [
            (MouseEventKind::Drag(MouseButton::Left), 70, 12),
            (MouseEventKind::Drag(MouseButton::Left), 10, 12),
            (MouseEventKind::Drag(MouseButton::Left), w1.x + 4, w1.y + 3),
            (MouseEventKind::ScrollUp, w1.x + 4, w1.y + 3),
            (MouseEventKind::Up(MouseButton::Left), w1.x + 4, w1.y + 3),
        ] {
            cmds.extend(mouse_event(
                &mut state, &mut app, &brigade, kind, column, row, now,
            ));
        }
        // The only command is the preference write of the move that landed.
        assert!(writes_of(&cmds).is_empty(), "{cmds:?}");
        assert_eq!(layout_writes(&cmds).len(), 1, "{cmds:?}");
        assert_eq!(
            order_of(&state),
            ["dir", "w3", "w1", "w2"],
            "and the move did land"
        );
    }

    #[test]
    fn the_order_survives_a_rebuilt_pane_list_and_leaves_the_focus_on_its_key() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);
        assert_eq!(order_of(&state), ["dir", "w3", "w1", "w2"]);

        // Staged again in member order (what `stage_brigade` builds).
        let keys: Vec<SessionKey> = ["dir", "w1", "w2", "w3"].map(SessionKey::from_id).to_vec();
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(keys[0].clone()),
            panes: keys,
            focused: 2, // w2
        };
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(order_of(&state), ["dir", "w3", "w1", "w2"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));
    }

    #[test]
    fn the_order_survives_off_stage_and_applies_to_whatever_arrives() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);
        let layout_before = state.brigade_layouts[&1].clone();

        // Another session takes the stage; members unstage one by one.
        for name in ["w1", "w2", "w3", "dir"] {
            state.unstage(&SessionKey::from_id(name));
        }
        state.stage = Stage::Solo(SessionKey::from_id("elsewhere"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        assert_eq!(state.brigade_layouts[&1], layout_before);

        // The brigade comes back, members arriving through `Spawned` in the
        // opposite order to the preference.
        let dir = SessionKey::from_id("dir");
        state
            .member_tokens
            .insert(dir.clone(), DIRECTOR_TOKEN.to_string());
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(dir.clone()),
            panes: vec![dir],
            focused: 0,
        };
        for token in ["worker-1", "worker-2", "worker-3"] {
            open_worker(
                &mut state,
                1,
                token,
                std::path::Path::new("/work"),
                "",
                None,
                AgentKind::ClaudeCode,
            );
        }
        for token in ["worker-1", "worker-2", "worker-3"] {
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Spawned {
                    key: SessionKey::new_worker(1, token),
                },
                now,
            );
        }
        // The preference was w3, w1, w2.
        assert_eq!(
            order_of(&state),
            [
                "dir",
                "new-worker::1::worker-3",
                "new-worker::1::worker-1",
                "new-worker::1::worker-2"
            ]
        );
    }

    #[test]
    fn a_director_arriving_late_at_the_front_does_not_disturb_the_stack_order() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);

        // The Director is not staged yet: a Worker holds `panes[0]`.
        let dir = SessionKey::from_id("dir");
        state.unstage(&dir);
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(dir.clone()),
            panes: ["w1", "w2", "w3"].map(SessionKey::from_id).to_vec(),
            focused: 0,
        };
        state.pending_opens.insert(
            dir.clone(),
            PendingOpen::BrigadeMember {
                brigade_id: 1,
                needs_codex_kickoff: false,
                cwd: PathBuf::from("/work"),
            },
        );
        state
            .member_tokens
            .insert(dir.clone(), DIRECTOR_TOKEN.to_string());

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned { key: dir },
            now,
        );

        // The Director goes to the front, and the Worker it displaced joins
        // the stack in its ranked place.
        assert_eq!(order_of(&state), ["dir", "w3", "w1", "w2"]);
    }

    #[test]
    fn members_without_a_rank_land_after_the_ranked_ones_in_their_own_order() {
        let (mut state, mut app, brigade, now) = drag_fixture(4);
        let layout = BrigadeLayout {
            stack_order: vec!["worker-4".to_string(), "worker-2".to_string()],
            ..BrigadeLayout::default()
        };
        state.brigade_layouts.insert(1, layout);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(order_of(&state), ["dir", "w4", "w2", "w1", "w3"]);
    }

    #[test]
    fn a_move_keeps_an_off_stage_members_place_after_the_staged_ones() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let layout = BrigadeLayout {
            stack_order: vec!["worker-9".to_string(), "worker-8".to_string()],
            ..BrigadeLayout::default()
        };
        state.brigade_layouts.insert(1, layout);

        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);

        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-3", "worker-1", "worker-2", "worker-9", "worker-8"]
        );
    }

    #[test]
    fn dismissing_a_worker_forgets_its_slot_and_a_reused_token_does_not_inherit_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);
        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-3", "worker-1", "worker-2"]
        );

        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));
        update(&mut state, &mut app, &brigade, dismissed(1), now);
        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-1", "worker-2"]
        );
        assert_eq!(order_of(&state), ["dir", "w1", "w2"]);

        // add-worker numbers max + 1, so the next Worker is `worker-3` again;
        // put it at the front of the panes to see where it ends up.
        let again = SessionKey::from_id("w3-again");
        state.screens.insert(again.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(again.clone(), "worker-3".to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.insert(1, again);
        }
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3-again"]);
    }

    #[test]
    fn another_brigades_dismissal_result_does_not_forget_this_brigades_slot() {
        let (mut state, mut app, brigade, now) = two_brigades_with_a_worker_3_each();
        state.brigade_layouts.get_mut(&1).unwrap().stack_order =
            vec!["worker-3".to_string(), "worker-1".to_string()];
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(2), now);

        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-3", "worker-1"]
        );
    }

    #[test]
    fn disbanding_forgets_the_order_with_the_rest() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        move_onto(&mut state, &mut app, &brigade, "w2", "w1", now);
        assert!(!state.brigade_layouts[&1].stack_order.is_empty());

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );

        assert!(!state.brigade_layouts.contains_key(&1));
    }

    #[test]
    fn a_pane_exiting_keeps_its_slot() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("w1"),
                reason: None,
            },
            now,
        );

        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-3", "worker-1", "worker-2"]
        );
    }

    /// Arm the prefix and press `code`+`modifiers` after it.
    fn prefix_key(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        code: KeyCode,
        modifiers: Modifiers,
        now: Instant,
    ) -> Vec<Cmd> {
        state.prefix_armed = Some(now);
        forward_key(state, app, brigade, code, modifiers, now)
    }

    fn focus_stack_slot(state: &mut EmporiumState, slot: usize) {
        state.focus = Focus::Pane;
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = slot;
        }
    }

    #[test]
    fn prefix_shift_up_and_down_move_the_focused_tile_one_slot_and_keep_focus_on_it() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);

        focus_stack_slot(&mut state, 2); // w2
        let cmds = prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::SHIFT,
            now,
        );
        assert!(writes_of(&cmds).is_empty(), "nothing leaks into the child");
        assert_eq!(order_of(&state), ["dir", "w2", "w1", "w3"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));
        assert_eq!(state.focus, Focus::Pane);
        assert_eq!(state.prefix_armed, None);

        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::SHIFT,
            now,
        );
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(order_of(&state), ["dir", "w1", "w3", "w2"]);
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));
        assert_eq!(
            state.brigade_layouts[&1].stack_order,
            ["worker-1", "worker-3", "worker-2"]
        );
    }

    #[test]
    fn prefix_shift_arrows_refuse_with_a_reason_at_the_edges_the_master_and_the_sidebar() {
        let cases: [(&str, usize, KeyCode, &str); 4] = [
            (
                "top",
                1,
                KeyCode::Up,
                "prefix: already at the top of the stack",
            ),
            (
                "bottom",
                3,
                KeyCode::Down,
                "prefix: already at the bottom of the stack",
            ),
            (
                "master up",
                0,
                KeyCode::Up,
                "prefix: the master tile stays put",
            ),
            (
                "master down",
                0,
                KeyCode::Down,
                "prefix: the master tile stays put",
            ),
        ];
        for (what, slot, code, expected) in cases {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            focus_stack_slot(&mut state, slot);
            let cmds = prefix_key(&mut state, &mut app, &brigade, code, Modifiers::SHIFT, now);
            assert!(cmds.is_empty(), "{what}");
            assert_eq!(state.status.as_deref(), Some(expected), "{what}");
            assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3"], "{what}");
            assert!(state.brigade_layouts.is_empty(), "{what}");
            assert_eq!(state.focus, Focus::Pane, "{what}");
        }

        let (mut state, mut app, brigade, now) = drag_fixture(3);
        state.focus = Focus::Sidebar;
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prefix: focus a tile to move it")
        );
        assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3"]);

        // A solo session has no stack either.
        let (mut state, mut app, brigade, now) = drag_fixture(0);
        state.stage = Stage::Solo(SessionKey::from_id("dir"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::SHIFT,
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prefix: the master tile stays put")
        );
    }

    #[test]
    fn prefix_plain_arrows_still_move_focus_and_never_the_order() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        focus_stack_slot(&mut state, 2);
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w1"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::NONE,
            now,
        );
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Down,
            Modifiers::NONE,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w3"));
        // Any other modifier combination keeps resolving as it always did.
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::CONTROL,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w2"));
        prefix_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Up,
            Modifiers::ALT,
            now,
        );
        assert_eq!(focused_name(&state).as_deref(), Some("w1"));
        assert_eq!(order_of(&state), ["dir", "w1", "w2", "w3"]);
        assert!(state.brigade_layouts.is_empty());
    }

    #[test]
    fn the_number_keys_address_the_new_order() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        move_onto(&mut state, &mut app, &brigade, "w3", "w1", now);
        assert_eq!(order_of(&state), ["dir", "w3", "w1", "w2"]);

        for (digit, expected) in [('1', "dir"), ('2', "w3"), ('3', "w1"), ('4', "w2")] {
            prefix_key(
                &mut state,
                &mut app,
                &brigade,
                KeyCode::Char(digit),
                Modifiers::NONE,
                now,
            );
            assert_eq!(
                focused_name(&state).as_deref(),
                Some(expected),
                "prefix {digit}"
            );
        }
    }

    #[test]
    fn a_move_resizes_exactly_the_tiles_whose_content_size_changed() {
        let (mut state, mut app, brigade, now) = drag_fixture(4);
        let before: Vec<(String, (u16, u16))> = state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .map(|(key, rect)| {
                let content = pane_content(*rect);
                (key.as_str().to_string(), (content.height, content.width))
            })
            .collect();

        let cmds = move_onto(&mut state, &mut app, &brigade, "w4", "w1", now);

        let after: HashMap<String, (u16, u16)> = state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .map(|(key, rect)| {
                let content = pane_content(*rect);
                (key.as_str().to_string(), (content.height, content.width))
            })
            .collect();
        let mut expected: Vec<String> = before
            .iter()
            .filter(|(name, size)| after[name] != *size)
            .map(|(name, _)| name.clone())
            .collect();
        expected.sort();
        let resized: Vec<String> = resizes(&cmds)
            .into_iter()
            .map(|(key, _, _)| key.as_str().to_string())
            .collect();
        assert_eq!(resized, expected);
        // Equal weights over 39 rows are 10, 10, 9, 10: moving the last tile
        // to the top shifts the short row from third to fourth.
        assert_eq!(resized, ["w2", "w3"], "and the rest keep their size");
    }

    #[test]
    fn the_label_cells_follow_the_drawn_label_for_a_goinkyo_and_a_director_too() {
        let (mut state, _, _, _) = drag_fixture(2);
        // A Goinkyo in the lower slot reads "goinkyo", not its token.
        state.goinkyo_pane.insert(1, SessionKey::from_id("w2"));
        let label = state.tile_label(&SessionKey::from_id("w2"));
        assert_eq!(label, "goinkyo");
        let lower = state.geometry(DRAG_AREA).tiles[2].1;
        let handle = handle_of(
            &state,
            &Boundary::Stack {
                brigade: 1,
                above: SessionKey::from_id("w1"),
                below: SessionKey::from_id("w2"),
            },
        );
        assert_eq!(
            handle.hits()[1].x,
            lower.x + 1 + crate::layout::title_text("goinkyo").len() as u16,
            "the lower row's handle starts where the drawn title ends"
        );
        assert_eq!(state.tile_label(&SessionKey::from_id("dir")), "director");
    }

    #[test]
    fn a_press_on_a_handle_changes_no_focus_forwards_nothing_and_scrolls_nothing() {
        for boundary in [
            Boundary::Sidebar,
            Boundary::Master { brigade: 1 },
            Boundary::Stack {
                brigade: 1,
                above: SessionKey::from_id("w1"),
                below: SessionKey::from_id("w2"),
            },
        ] {
            let (mut state, mut app, brigade, now) = drag_fixture(2);
            // The pane under the pointer asks for mouse reports, so a stray
            // press would be forwarded to it.
            for key in ["dir", "w1", "w2"] {
                state
                    .screens
                    .get_mut(&SessionKey::from_id(key))
                    .unwrap()
                    .process(b"\x1b[?1000h\x1b[?1006h");
            }
            state.focus = Focus::Sidebar;
            let handle = handle_of(&state, &boundary);
            let (column, row) = grab_point(&handle);

            let cmds = mouse_event(
                &mut state,
                &mut app,
                &brigade,
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                now,
            );

            assert!(state.drag.is_some());
            assert_eq!(state.focus, Focus::Sidebar, "{boundary:?}");
            assert!(matches!(&state.stage, Stage::Brigade { focused: 0, .. }));
            assert!(writes_of(&cmds).is_empty(), "{boundary:?}: {cmds:?}");
        }
    }

    #[test]
    fn dragging_moves_only_the_preview_and_resizes_nothing() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let before = state.geometry(DRAG_AREA).tiles;
        let sizes_before = sizes(&state);
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;

        let cmds = drag_to(&mut state, &mut app, &brigade, &master, start + 10, now);

        assert!(resizes(&cmds).is_empty(), "{cmds:?}");
        assert_eq!(state.drag.as_ref().unwrap().preview, start + 10);
        assert_eq!(
            state.geometry(DRAG_AREA).tiles,
            before,
            "the real geometry holds still"
        );
        assert_eq!(state.sidebar_width, SIDEBAR_WIDTH);
        assert!(state.brigade_layouts.is_empty());
        let sizes_after: Vec<_> = sizes(&state);
        assert_eq!(sizes_after, sizes_before, "no screen was resized");
    }

    #[test]
    fn releasing_commits_each_boundary_and_resizes_exactly_the_tiles_that_changed() {
        // Master boundary: the master's width and every stack tile's width
        // change; no height does.
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start + 8, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &master, start + 8, now);
        assert!(state.drag.is_none());
        let tiles = state.geometry(DRAG_AREA).tiles;
        assert_eq!(tiles[0].1.width, 42 + 8);
        let keys: Vec<_> = resizes(&cmds)
            .into_iter()
            .map(|(k, _, _)| k.as_str().to_string())
            .collect();
        assert_eq!(keys, vec!["dir", "w1", "w2"]);

        // Stack boundary: only the two tiles either side of it change.
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let stack = Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        };
        let start = handle_of(&state, &stack).position;
        drag_to(&mut state, &mut app, &brigade, &stack, start + 3, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &stack, start + 3, now);
        let keys: Vec<_> = resizes(&cmds)
            .into_iter()
            .map(|(k, _, _)| k.as_str().to_string())
            .collect();
        assert_eq!(keys, vec!["w1", "w2"]);

        // Sidebar boundary: every tile's width changes.
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        drag_to(&mut state, &mut app, &brigade, &Boundary::Sidebar, 50, now);
        let cmds = release_at(&mut state, &mut app, &brigade, &Boundary::Sidebar, 50, now);
        assert_eq!(state.sidebar_width, 50);
        assert_eq!(state.geometry(DRAG_AREA).areas.pane.x, 50);
        let keys: Vec<_> = resizes(&cmds)
            .into_iter()
            .map(|(k, _, _)| k.as_str().to_string())
            .collect();
        assert_eq!(keys, vec!["dir", "w1", "w2"]);
    }

    #[test]
    fn a_stack_drag_rewrites_every_staged_members_weight_to_its_new_height() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let stack = Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        };
        let start = handle_of(&state, &stack).position;
        let before: Vec<u16> = state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .skip(1)
            .map(|(_, r)| r.height)
            .collect();
        drag_to(&mut state, &mut app, &brigade, &stack, start + 2, now);
        release_at(&mut state, &mut app, &brigade, &stack, start + 2, now);

        let after: Vec<u16> = state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .skip(1)
            .map(|(_, r)| r.height)
            .collect();
        assert_eq!(after, vec![before[0] + 2, before[1] - 2, before[2]]);
        let weights = &state.brigade_layouts[&1].row_weights;
        assert_eq!(weights["worker-1"], after[0]);
        assert_eq!(weights["worker-2"], after[1]);
        assert_eq!(
            weights["worker-3"], after[2],
            "even the tile that did not move"
        );
        assert_eq!(stack_height_sum(&state), 39);
    }

    #[test]
    fn a_press_and_release_on_the_same_cell_stores_nothing() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let stack = Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        };
        let handle = handle_of(&state, &stack);
        let (column, row) = grab_point(&handle);
        mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            now,
        );
        let cmds = mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Up(MouseButton::Left),
            column,
            row,
            now,
        );

        assert!(state.drag.is_none());
        assert!(
            state.brigade_layouts.is_empty(),
            "a click must not freeze row counts"
        );
        assert!(resizes(&cmds).is_empty());
    }

    #[test]
    fn a_drag_is_clamped_to_each_minimum() {
        // Sidebar: both ends.
        let (mut state, mut app, brigade, now) = drag_fixture(1);
        drag_to(&mut state, &mut app, &brigade, &Boundary::Sidebar, 0, now);
        assert_eq!(state.drag.as_ref().unwrap().preview, MIN_SIDEBAR_WIDTH);
        // A second press would cancel the first drag, so the far end gets a
        // fresh gesture.
        let (mut state, mut app, brigade, now) = drag_fixture(1);
        drag_to(&mut state, &mut app, &brigade, &Boundary::Sidebar, 119, now);
        assert_eq!(
            state.drag.as_ref().unwrap().preview,
            120 - MIN_PANE_AREA_WIDTH
        );
        release_at(&mut state, &mut app, &brigade, &Boundary::Sidebar, 119, now);
        assert_eq!(state.sidebar_width, 100);
        assert_eq!(
            state.geometry(DRAG_AREA).areas.pane.width,
            MIN_PANE_AREA_WIDTH
        );

        // Master boundary: master and stack each keep their minimum.
        for (target, expect_master) in [(0u16, MIN_MASTER_WIDTH), (119, 84 - MIN_STACK_WIDTH)] {
            let (mut state, mut app, brigade, now) = drag_fixture(1);
            let master = Boundary::Master { brigade: 1 };
            drag_to(&mut state, &mut app, &brigade, &master, target, now);
            release_at(&mut state, &mut app, &brigade, &master, target, now);
            let tiles = state.geometry(DRAG_AREA).tiles;
            assert_eq!(tiles[0].1.width, expect_master, "toward {target}");
            assert_eq!(tiles[1].1.width, 84 - expect_master);
        }

        // Stack boundary: each tile keeps MIN_STACK_TILE_HEIGHT rows.
        for target in [0u16, 39] {
            let (mut state, mut app, brigade, now) = drag_fixture(2);
            let stack = Boundary::Stack {
                brigade: 1,
                above: SessionKey::from_id("w1"),
                below: SessionKey::from_id("w2"),
            };
            drag_to(&mut state, &mut app, &brigade, &stack, target, now);
            release_at(&mut state, &mut app, &brigade, &stack, target, now);
            let heights: Vec<u16> = state
                .geometry(DRAG_AREA)
                .tiles
                .iter()
                .skip(1)
                .map(|(_, r)| r.height)
                .collect();
            assert!(
                heights.iter().all(|h| *h >= MIN_STACK_TILE_HEIGHT),
                "{heights:?}"
            );
            assert_eq!(heights.iter().sum::<u16>(), 39);
            assert!(
                heights.contains(&MIN_STACK_TILE_HEIGHT),
                "{heights:?}: dragged all the way"
            );
        }
    }

    /// Start a drag on the master boundary and move it, leaving the button
    /// down: the state every cancel test starts from.
    fn dragging_master() -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start + 9, now);
        assert!(state.drag.is_some());
        (state, app, brigade, now)
    }

    /// What every cancel must leave alone.
    fn layout_facts(state: &EmporiumState) -> (Vec<(SessionKey, Rect)>, u16, usize, Focus, usize) {
        let focused = match &state.stage {
            Stage::Brigade { focused, .. } => *focused,
            _ => 0,
        };
        (
            state.geometry(DRAG_AREA).tiles,
            state.sidebar_width,
            state.brigade_layouts.len(),
            state.focus,
            focused,
        )
    }

    #[test]
    fn every_cancel_leaves_the_geometry_and_focus_untouched() {
        type Cancel = Box<dyn Fn(&mut EmporiumState, &mut App, &BrigadeConfig, Instant)>;
        let cancels: Vec<(&str, Cancel)> = vec![
            (
                "Esc",
                Box::new(|state, app, brigade, now| {
                    update(
                        state,
                        app,
                        brigade,
                        Event::Input(InputEvent::Key(KeyEvent::new(
                            KeyCode::Esc,
                            Modifiers::NONE,
                        ))),
                        now,
                    );
                }),
            ),
            (
                "a press of the left button",
                Box::new(|state, app, brigade, now| {
                    mouse_event(
                        state,
                        app,
                        brigade,
                        MouseEventKind::Down(MouseButton::Left),
                        60,
                        20,
                        now,
                    );
                }),
            ),
            (
                "a press of another button",
                Box::new(|state, app, brigade, now| {
                    mouse_event(
                        state,
                        app,
                        brigade,
                        MouseEventKind::Down(MouseButton::Right),
                        60,
                        20,
                        now,
                    );
                }),
            ),
            (
                "a move with no button held",
                Box::new(|state, app, brigade, now| {
                    mouse_event(state, app, brigade, MouseEventKind::Moved, 60, 20, now);
                }),
            ),
            (
                "the window losing focus",
                Box::new(|state, app, brigade, now| {
                    update(
                        state,
                        app,
                        brigade,
                        Event::WindowFocusChanged { focused: false },
                        now,
                    );
                }),
            ),
            (
                "a modal opening",
                Box::new(|state, app, brigade, now| {
                    app.open_new_session_modal();
                    update(
                        state,
                        app,
                        brigade,
                        Event::Tick {
                            relay: vec![],
                            brigades: Vec::new(),
                        },
                        now,
                    );
                }),
            ),
            (
                "a terminal resize",
                Box::new(|state, app, brigade, now| {
                    update(
                        state,
                        app,
                        brigade,
                        Event::Resized {
                            width: 120,
                            height: 40,
                        },
                        now,
                    );
                }),
            ),
        ];
        for (what, cancel) in cancels {
            let (mut state, mut app, brigade, now) = dragging_master();
            let before = layout_facts(&state);

            cancel(&mut state, &mut app, &brigade, now);

            assert!(state.drag.is_none(), "{what} cancels");
            assert_eq!(layout_facts(&state), before, "{what} changes nothing");
            assert_eq!(state.displayed_status(), None, "{what}: the hint is gone");
        }
    }

    #[test]
    fn a_drag_whose_boundary_stops_existing_is_dropped() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let stack = Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        };
        let start = handle_of(&state, &stack).position;
        drag_to(&mut state, &mut app, &brigade, &stack, start + 2, now);

        // A pane closes: the stack has one tile left and no boundary.
        state.unstage(&SessionKey::from_id("w2"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert!(state.drag.is_none());
        assert!(state.brigade_layouts.is_empty());
    }

    #[test]
    fn a_drag_survives_events_that_do_not_touch_its_boundary() {
        let (mut state, mut app, brigade, now) = dragging_master();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: true },
            now,
        );
        assert!(state.drag.is_some());
    }

    #[test]
    fn while_dragging_every_mouse_event_belongs_to_the_drag() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        for key in ["dir", "w1", "w2"] {
            state
                .screens
                .get_mut(&SessionKey::from_id(key))
                .unwrap()
                .process(b"\x1b[?1000h\x1b[?1006h");
        }
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        let geometry = state.geometry(DRAG_AREA);
        let over_a_worker = (geometry.tiles[1].1.x + 6, geometry.tiles[1].1.y + 3);
        drag_to(&mut state, &mut app, &brigade, &master, start + 5, now);

        // The pointer crosses an SGR-mouse pane while the button is held.
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Up(MouseButton::Right),
        ] {
            let cmds = mouse_event(
                &mut state,
                &mut app,
                &brigade,
                kind,
                over_a_worker.0,
                over_a_worker.1,
                now,
            );
            assert!(cmds.is_empty(), "{kind:?}: {cmds:?}");
            assert!(state.drag.is_some(), "{kind:?} does not end the drag");
        }
        assert!(matches!(&state.stage, Stage::Brigade { focused: 0, .. }));
        assert_eq!(state.focus, Focus::Pane);

        // And the cancelling press is swallowed too, not delivered.
        let cmds = mouse_event(
            &mut state,
            &mut app,
            &brigade,
            MouseEventKind::Down(MouseButton::Left),
            over_a_worker.0,
            over_a_worker.1,
            now,
        );
        assert!(writes_of(&cmds).is_empty());
        assert!(state.drag.is_none());
    }

    #[test]
    fn esc_during_a_drag_is_consumed_and_esc_otherwise_still_reaches_the_child() {
        let (mut state, mut app, brigade, now) = dragging_master();
        let cmds = forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Esc,
            Modifiers::NONE,
            now,
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        assert!(state.drag.is_none());

        let cmds = forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Esc,
            Modifiers::NONE,
            now,
        );
        assert_eq!(
            writes_of(&cmds),
            vec![(SessionKey::from_id("dir"), vec![0x1b])],
            "with no drag the key goes to the focused pane as before"
        );
    }

    #[test]
    fn the_hint_and_the_preview_line_exist_exactly_as_long_as_the_drag() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        assert_eq!(state.displayed_status(), None);
        assert_eq!(overlay_line(&state), None);

        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start + 6, now);
        assert_eq!(
            state.displayed_status(),
            Some("resize: release to apply · Esc cancels")
        );
        let (line, axis) = overlay_line(&state).unwrap();
        assert_eq!(axis, Axis::Vertical);
        assert_eq!(
            line.x,
            start + 6 - 1,
            "on the last column before the boundary"
        );
        assert_eq!(line.height, 39);

        // It does not expire like a transient status, and a status set in the
        // meantime does not replace it.
        state.set_status("something else", now);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now + Duration::from_secs(60),
        );
        assert_eq!(
            state.displayed_status(),
            Some("resize: release to apply · Esc cancels")
        );

        release_at(&mut state, &mut app, &brigade, &master, start + 6, now);
        assert_eq!(overlay_line(&state), None);
        assert_ne!(
            state.displayed_status(),
            Some("resize: release to apply · Esc cancels")
        );
    }

    #[test]
    fn layout_preferences_come_back_when_the_same_brigade_is_staged_again() {
        let (mut state, mut app, brigade, now) = drag_fixture(3);
        let stack = Boundary::Stack {
            brigade: 1,
            above: SessionKey::from_id("w1"),
            below: SessionKey::from_id("w2"),
        };
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &stack).position;
        drag_to(&mut state, &mut app, &brigade, &stack, start + 3, now);
        release_at(&mut state, &mut app, &brigade, &stack, start + 3, now);
        let mstart = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, mstart - 7, now);
        release_at(&mut state, &mut app, &brigade, &master, mstart - 7, now);
        let committed = state.geometry(DRAG_AREA).tiles;

        // Another session takes the stage, then the brigade is staged again
        // with the same members (new key objects, same tokens).
        let panes = match &state.stage {
            Stage::Brigade { panes, .. } => panes.clone(),
            _ => unreachable!(),
        };
        state.stage = Stage::Solo(SessionKey::from_id("elsewhere"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(panes[0].clone()),
            panes,
            focused: 0,
        };

        assert_eq!(state.geometry(DRAG_AREA).tiles, committed);
    }

    #[test]
    fn a_member_with_no_weight_gets_the_mean_of_the_weights_present() {
        let (mut state, _, _, _) = drag_fixture(3);
        assert_eq!(
            state.stack_weights(),
            vec![1, 1, 1],
            "nothing stored: equal"
        );

        let mut layout = BrigadeLayout::default();
        layout.row_weights.insert("worker-1".to_string(), 10);
        layout.row_weights.insert("worker-2".to_string(), 20);
        state.brigade_layouts.insert(1, layout);
        assert_eq!(
            state.stack_weights(),
            vec![10, 20, 15],
            "a Worker added later is average"
        );

        // The unweighted member lands at an average height.
        let heights: Vec<u16> = state
            .geometry(DRAG_AREA)
            .tiles
            .iter()
            .skip(1)
            .map(|(_, r)| r.height)
            .collect();
        assert_eq!(heights.iter().sum::<u16>(), 39);
        assert!(
            heights[2] > heights[0] && heights[2] < heights[1],
            "{heights:?}"
        );
    }

    #[test]
    fn a_tiny_terminal_never_panics_through_a_whole_gesture() {
        for (width, height) in [(0u16, 0u16), (1, 1), (10, 5), (30, 8), (36, 12), (60, 10)] {
            let (mut state, mut app, brigade, now) = drag_fixture(3);
            update(
                &mut state,
                &mut app,
                &brigade,
                Event::Resized { width, height },
                now,
            );
            let area = Rect::new(0, 0, width, height);
            let handles: Vec<Handle> = state.geometry(area).handles().to_vec();
            for handle in handles {
                let (column, row) = match handle.axis {
                    Axis::Vertical => (handle.position, height / 2),
                    Axis::Horizontal => (
                        (handle.position.min(width.saturating_sub(1))).max(1),
                        handle.position.saturating_sub(1),
                    ),
                };
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Drag(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    mouse_event(&mut state, &mut app, &brigade, kind, column, row, now);
                }
                assert!(state.drag.is_none());
            }
            let _ = state.drag_overlay(area);
        }
    }

    #[test]
    fn the_stack_heights_always_sum_to_the_stack_height_after_any_commit() {
        for workers in 1..=4 {
            for target in [0u16, 7, 15, 25, 39] {
                let (mut state, mut app, brigade, now) = drag_fixture(workers);
                let boundaries: Vec<Boundary> = state
                    .geometry(DRAG_AREA)
                    .handles()
                    .iter()
                    .map(|h| h.boundary.clone())
                    .collect();
                for boundary in boundaries {
                    if boundary == Boundary::Sidebar {
                        continue;
                    }
                    drag_to(&mut state, &mut app, &brigade, &boundary, target, now);
                    release_at(&mut state, &mut app, &brigade, &boundary, target, now);
                    assert_eq!(
                        stack_height_sum(&state),
                        39,
                        "{workers} workers, {boundary:?} to {target}"
                    );
                }
            }
        }
    }

    #[test]
    fn disbanding_drops_the_brigades_layout_preferences() {
        let (mut state, mut app, brigade, now) = drag_fixture(2);
        let master = Boundary::Master { brigade: 1 };
        let start = handle_of(&state, &master).position;
        drag_to(&mut state, &mut app, &brigade, &master, start + 4, now);
        release_at(&mut state, &mut app, &brigade, &master, start + 4, now);
        assert!(state.brigade_layouts.contains_key(&1));

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Disbanded {
                brigade_id: 1,
                result: Ok((HashSet::new(), HashSet::new())),
            },
            now,
        );

        assert!(!state.brigade_layouts.contains_key(&1));
    }

    #[test]
    fn unstaging_a_pane_clears_its_unsent_mark() {
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        assert!(state.has_unsent_operator_input(&director));

        state.unstage(&director);

        assert!(!state.has_unsent_operator_input(&director));
    }

    #[test]
    fn a_pending_submit_is_not_flushed_while_the_pane_holds_a_draft() {
        // Input typed after a nudge already cancels its `\r`; this is the
        // draft that was there before the nudge was written.
        let (mut state, mut app, brigade, director, _worker, now) = two_pane_brigade_focused_on(0);
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            now,
        );
        state.pending_submits.push(PendingSubmit {
            key: director.clone(),
            nudged_at: now,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now + RELAY_SUBMIT_DELAY,
        );

        assert!(writes_of(&cmds).is_empty(), "{cmds:?}");
        assert!(
            state.pending_submits.is_empty(),
            "the entry is spent, not retried every tick"
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
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
        };

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
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
                brigades: Vec::new(),
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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

    // --- Goinkyo: update_goinkyo_awaiting_spawn (spawn-once guard) ---------

    fn goinkyo_candidate(brigade_id: BrigadeId) -> GoinkyoSpawnCandidate {
        GoinkyoSpawnCandidate {
            brigade_id,
            cwd: PathBuf::from("/work/alpha"),
            roster: Vec::new(),
        }
    }

    fn awaiting_spawn(brigade_id: BrigadeId) -> Event {
        Event::GoinkyoAwaitingSpawn {
            observation: GoinkyoObservation::AwaitingSpawn(goinkyo_candidate(brigade_id)),
        }
    }

    #[test]
    fn unchanged_means_no_cmd() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: GoinkyoObservation::Unchanged,
            },
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(state.goinkyo_pane.is_empty());
    }

    #[test]
    fn a_candidate_issues_an_open_embedded_carrying_the_goinkyo_model_effort_permission_mode_and_disallowed_tools()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        // `goinkyo_permission_mode`/`goinkyo_disallowed_tools` both left at
        // their own defaults ("auto"/"Edit,Write,NotebookEdit") on purpose:
        // this doubles as the "default value reaches argv" case for both,
        // the same way `goinkyo_model`/`goinkyo_effort` are overridden here
        // to prove an *explicit* value reaches them.
        let brigade = BrigadeConfig {
            goinkyo_model: "fable".to_string(),
            goinkyo_effort: "max".to_string(),
            ..BrigadeConfig::default()
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(7),
            test_instant(),
        );
        let [
            Cmd::OpenEmbedded {
                target,
                brigade: member,
                model,
                effort,
                permission_mode,
                disallowed_tools,
                ..
            },
        ] = cmds.as_slice()
        else {
            panic!("expected exactly one OpenEmbedded, got {cmds:?}");
        };
        assert_eq!(target.cwd, PathBuf::from("/work/alpha"));
        assert_eq!(
            member
                .as_ref()
                .map(|(id, token, role)| (*id, token.as_str(), *role)),
            Some((7, "goinkyo", BrigadeRole::Goinkyo))
        );
        assert_eq!(model.as_deref(), Some("fable"));
        assert_eq!(effort.as_deref(), Some("max"));
        assert_eq!(permission_mode.as_deref(), Some("auto"));
        assert_eq!(disallowed_tools.as_deref(), Some("Edit,Write,NotebookEdit"));
        assert!(state.goinkyo_pane.contains_key(&7));
    }

    #[test]
    fn an_empty_goinkyo_permission_mode_drops_the_field_entirely() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = BrigadeConfig {
            goinkyo_permission_mode: String::new(),
            ..BrigadeConfig::default()
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(7),
            test_instant(),
        );
        let [
            Cmd::OpenEmbedded {
                permission_mode, ..
            },
        ] = cmds.as_slice()
        else {
            panic!("expected exactly one OpenEmbedded, got {cmds:?}");
        };
        assert_eq!(permission_mode, &None);
    }

    #[test]
    fn an_empty_goinkyo_disallowed_tools_drops_the_field_entirely() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = BrigadeConfig {
            goinkyo_disallowed_tools: String::new(),
            ..BrigadeConfig::default()
        };

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(7),
            test_instant(),
        );
        let [
            Cmd::OpenEmbedded {
                disallowed_tools, ..
            },
        ] = cmds.as_slice()
        else {
            panic!("expected exactly one OpenEmbedded, got {cmds:?}");
        };
        assert_eq!(disallowed_tools, &None);
    }

    #[test]
    fn the_tick_equivalent_event_never_opens_the_same_goinkyo_twice() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        for _ in 0..5 {
            let cmds = update(
                &mut state,
                &mut app,
                &brigade,
                awaiting_spawn(3),
                test_instant(),
            );
            // Only the very first of these repeated firings may open a pane —
            // this is the invariant the whole guard exists for: a spawn
            // attempted at most once per consultation, no matter how many
            // times the tick that notices the candidate fires while it's
            // still pending.
            if cmds.is_empty() {
                continue;
            }
            assert!(
                matches!(cmds.as_slice(), [Cmd::OpenEmbedded { .. }]),
                "unexpected cmd on a repeat firing: {cmds:?}"
            );
        }
        let opens = (0..5)
            .map(|_| {
                update(
                    &mut state,
                    &mut app,
                    &brigade,
                    awaiting_spawn(3),
                    test_instant(),
                )
            })
            .filter(|cmds| !cmds.is_empty())
            .count();
        assert_eq!(
            opens, 0,
            "the guard must already be set from the loop above"
        );
    }

    #[test]
    fn a_spawn_failure_is_not_retried_on_the_next_tick() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(9),
            test_instant(),
        );
        let Some(Cmd::OpenEmbedded { key, .. }) = cmds.into_iter().next() else {
            panic!("expected an OpenEmbedded on the first firing");
        };
        assert!(state.pending_opens.contains_key(&key));

        // The spawn fails — `update_spawn_failed`'s own doc: this clears
        // `pending_opens` unconditionally, leaving exactly the same trace a
        // Goinkyo that was never even attempted has (no session id, no
        // pane). `goinkyo_pane` is the only thing left standing
        // that can tell the two apart.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::SpawnFailed {
                key: key.clone(),
                error: "boom".to_string(),
            },
            test_instant(),
        );
        assert!(!state.pending_opens.contains_key(&key));

        // The candidate is still shell-reported next tick (nothing about it
        // resolved), but the guard must refuse to spawn it again.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(9),
            test_instant(),
        );
        assert!(
            cmds.is_empty(),
            "a failed spawn must not auto-retry: {cmds:?}"
        );
    }

    #[test]
    fn an_already_open_pane_is_a_second_reason_not_to_spawn() {
        // Defense in depth alongside `goinkyo_pane`: if a pane for
        // this Goinkyo's synthetic key is already known open, the guard must
        // refuse regardless of the marker, since the marker is untouched
        // (never inserted before the call under test).
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::new_worker(4, "goinkyo");
        state.screens.insert(key, Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(4),
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(
            !state.goinkyo_pane.contains_key(&4),
            "the marker must not be inserted on a path that never actually spawned anything"
        );
    }

    #[test]
    fn an_in_flight_pending_open_is_also_a_second_reason_not_to_spawn() {
        // The other half of the same defense-in-depth check: a pane that
        // isn't open *yet* but already has an open in flight (between this
        // function issuing a `Cmd::OpenEmbedded` and the matching
        // `Event::Spawned`/`SpawnFailed` landing) must refuse too — covered
        // separately from the `screens` case above because either condition
        // alone must be sufficient, and a test that only ever sets both
        // together cannot tell that apart.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::new_worker(5, "goinkyo");
        state.pending_opens.insert(
            key,
            PendingOpen::BrigadeMember {
                brigade_id: 5,
                needs_codex_kickoff: false,
                cwd: PathBuf::from("/work/alpha"),
            },
        );
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(5),
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(
            !state.goinkyo_pane.contains_key(&5),
            "the marker must not be inserted on a path that never actually spawned anything"
        );
    }

    #[test]
    fn attempting_for_one_brigade_survives_another_brigade_being_attempted_in_between() {
        // A single `Option<BrigadeId>` slot would let staging brigade 2
        // overwrite brigade 1's own entry, so switching back to 1 (its
        // Goinkyo still unresolved) would look like "never tried" and
        // re-fire — exactly the bug `goinkyo_pane` being a `HashMap` fixes.
        // Two different brigades can each hold a Goinkyo row in the store
        // independently; only one is ever staged in the UI at a time, but
        // switching which one is staged must not disturb the other's guard.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let for_1 = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(1),
            test_instant(),
        );
        assert!(matches!(for_1.as_slice(), [Cmd::OpenEmbedded { .. }]));

        let for_2 = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(2),
            test_instant(),
        );
        assert!(matches!(for_2.as_slice(), [Cmd::OpenEmbedded { .. }]));

        let for_1_again = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(1),
            test_instant(),
        );
        assert!(
            for_1_again.is_empty(),
            "brigade 1's own marker must survive brigade 2 being attempted in between: {for_1_again:?}"
        );
        assert!(state.goinkyo_pane.contains_key(&1));
        assert!(state.goinkyo_pane.contains_key(&2));
    }

    #[test]
    fn the_goinkyo_row_disappearing_releases_the_guard_for_a_later_consultation() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        let first = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        assert!(matches!(first.as_slice(), [Cmd::OpenEmbedded { .. }]));
        assert!(state.goinkyo_pane.contains_key(&6));

        // Stand-in for the pane cleanup a real "consultation ended" path
        // would also do (`update_pty_exited` removes a closed pane from
        // `screens`; `update_spawn_failed` already removes a failed one from
        // `pending_opens`) — this test is only exercising the guard itself,
        // not that cleanup.
        let key = SessionKey::new_worker(6, "goinkyo");
        state.screens.remove(&key);
        state.pending_opens.remove(&key);

        // The consultation is dismissed and the shell reports the row gone.
        // Nothing here is on stage, which is exactly the case that used to
        // return without ending anything — see `GoinkyoObservation::
        // NoGoinkyo`'s own doc for why a pane off stage is still a process.
        let released = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: GoinkyoObservation::NoGoinkyo { brigade_id: 6 },
            },
            test_instant(),
        );
        assert!(
            matches!(released.as_slice(), [Cmd::KillPty { key }] if *key == SessionKey::new_worker(6, "goinkyo")),
            "the pane goes with the row, staged or not: {released:?}"
        );
        assert!(!state.goinkyo_pane.contains_key(&6));

        let second = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        assert!(
            matches!(second.as_slice(), [Cmd::OpenEmbedded { .. }]),
            "a released guard must allow exactly one more spawn: {second:?}"
        );
        assert!(state.goinkyo_pane.contains_key(&6));
    }

    #[test]
    fn end_to_end_consult_spawn_end_close_reconsult_spawns_exactly_once_more() {
        // The full round trip `update_goinkyo_awaiting_spawn`'s `NoGoinkyo`
        // arm exists for: a real pane actually staged, closed by the guard
        // release itself (not by some other event this test stages first —
        // see the sibling test above for that narrower case), and a second
        // consultation on the same brigade spawning again afterward.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 6,
            director: None,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        // Consult + spawn.
        let spawn_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        let [Cmd::OpenEmbedded { key, .. }] = spawn_cmds.as_slice() else {
            panic!("expected exactly one OpenEmbedded, got {spawn_cmds:?}");
        };
        let goinkyo_key = key.clone();
        assert_eq!(state.goinkyo_pane.get(&6), Some(&goinkyo_key));

        // The shell's own `PtyHandle::open` succeeded — same fold any other
        // spawn goes through, which is what actually pushes the pane into
        // `Stage::Brigade.panes`.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: goinkyo_key.clone(),
            },
            test_instant(),
        );
        assert!(state.screens.contains_key(&goinkyo_key));
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert!(panes.contains(&goinkyo_key));

        // End: the row is gone, the pane is still staged.
        let end_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: GoinkyoObservation::NoGoinkyo { brigade_id: 6 },
            },
            test_instant(),
        );
        assert!(
            matches!(&end_cmds[..], [Cmd::KillPty { key }] if *key == goinkyo_key),
            "expected exactly one KillPty for the Goinkyo's own pane, got {end_cmds:?}"
        );
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected the brigade to stay staged — only the one pane leaves");
        };
        assert!(
            !panes.contains(&goinkyo_key),
            "the pane must be unstaged the moment the row is gone, not wait for PtyExited"
        );
        assert!(!state.goinkyo_pane.contains_key(&6));

        // Reconsult before the killed pane has actually finished exiting:
        // `goinkyo_pane` no longer blocks it, but the old pane's `screens`
        // entry — under the very same synthetic key, since this Goinkyo was
        // never discovered — still does, the same defense-in-depth that
        // stops any other double-spawn under a key still in flight. A brief
        // delay, not a permanent block: the next tick's `PtyExited` clears
        // it.
        let too_soon = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        assert!(
            too_soon.is_empty(),
            "must not spawn onto a key the old pane hasn't actually vacated yet: {too_soon:?}"
        );

        // The shell confirms the killed pane actually exited.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: goinkyo_key.clone(),
                reason: None,
            },
            test_instant(),
        );
        assert!(!state.screens.contains_key(&goinkyo_key));

        // Reconsult: a fresh candidate for the same brigade spawns exactly
        // once more.
        let reconsult_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        assert!(
            matches!(reconsult_cmds.as_slice(), [Cmd::OpenEmbedded { .. }]),
            "a released guard must allow exactly one more spawn: {reconsult_cmds:?}"
        );
        assert!(state.goinkyo_pane.contains_key(&6));
    }

    #[test]
    fn a_row_swap_without_an_intervening_no_goinkyo_tears_down_the_stale_pane_first() {
        // The gap `NoGoinkyo`'s own guard release can miss entirely:
        // `dismiss_goinkyo` immediately followed by a fresh
        // `consult_goinkyo`, close enough together that the ~1Hz tick's one
        // `gather_goinkyo_observation` call never observes the row-less
        // moment in between — it only ever sees the *new* row, session-id-
        // less, which reads as an ordinary `AwaitingSpawn`. `goinkyo_pane`
        // still names the *old* pane's real key at that point, since
        // `NoGoinkyo` never fired to release it. Exercises
        // `update_goinkyo_awaiting_spawn`'s real-key branch, not the
        // synthetic-key attempt-once one the sibling tests above cover.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 6,
            director: None,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        let mut app = app_with(vec![]);
        let brigade = brigade_config();

        // Consult + spawn + land, same as the end-to-end test above.
        let spawn_cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );
        let [Cmd::OpenEmbedded { key, .. }] = spawn_cmds.as_slice() else {
            panic!("expected exactly one OpenEmbedded, got {spawn_cmds:?}");
        };
        let synthetic_key = key.clone();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: synthetic_key.clone(),
            },
            test_instant(),
        );
        assert!(state.screens.contains_key(&synthetic_key));

        // Discovery resolves it to a real id — `goinkyo_pane` now names a
        // *real* key, the precondition this whole test exists to exercise
        // (without this step, `goinkyo_pane` would still hold the synthetic
        // key, and the very next `awaiting_spawn` would just hit the
        // ordinary attempt-once branch the sibling tests above already
        // cover).
        let old_key = SessionKey::from_id("g1");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: synthetic_key,
                session_id: "g1".to_string(),
                member: Some((6, GOINKYO_TOKEN.to_string())),
            },
            test_instant(),
        );
        assert_eq!(state.goinkyo_pane.get(&6), Some(&old_key));
        assert!(state.screens.contains_key(&old_key));

        // Dismiss + a fresh consult, without ever going through `NoGoinkyo`:
        // the very next observation jumps straight to `AwaitingSpawn` for
        // the same brigade, as if a new, session-id-less row had simply
        // replaced the old one between two ticks.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            awaiting_spawn(6),
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: old_key.clone()
            }],
            "the stale pane must be torn down, not layered under a fresh spawn"
        );
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected the brigade to stay staged");
        };
        assert!(!panes.contains(&old_key), "the stale pane must be unstaged");
        assert!(
            !state.goinkyo_pane.contains_key(&6),
            "the guard must be released, not left pointing at the stale key"
        );
    }

    #[test]
    fn a_goinkyo_resumed_through_stage_brigade_is_torn_down_correctly_on_dismiss() {
        // Hole 2: before `stage_brigade` seeded `goinkyo_pane` on resume
        // too, a Goinkyo resumed this way — the only path reachable after a
        // banto restart, since `update_goinkyo_awaiting_spawn` was the
        // *only* other place that ever wrote to the guard — left it unset.
        // `NoGoinkyo`'s own `let Some(key) = state.goinkyo_pane.remove(...)
        // else { return }` found nothing, so a dismissed-but-still-alive
        // pane was never torn down at all.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir"), row("g1")]);
        let brigade = brigade_config();
        // Both already open (as a real restart would find them: banto's own
        // in-memory state is gone, but the PTY handles/screens the shell
        // rebuilds from the still-running processes are not) — the
        // Director's own pane is only here so removing the Goinkyo's below
        // doesn't collapse the whole stage to `Empty` as a side effect,
        // which isn't what this test is about.
        state
            .screens
            .insert(SessionKey::from_id("dir"), Screen::new(24, 80));
        state
            .screens
            .insert(SessionKey::from_id("g1"), Screen::new(24, 80));
        state.pending_membership = Some(PendingMembership::Activate);

        let roster = vec![
            (
                "director".to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "goinkyo".to_string(),
                BrigadeRole::Goinkyo,
                Some("g1".to_string()),
            ),
        ];
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: Some(roster),
                layout: None,
            },
            test_instant(),
        );
        assert_eq!(
            state.goinkyo_pane.get(&1),
            Some(&SessionKey::from_id("g1")),
            "the resume must seed the guard, not just stage the pane"
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoAwaitingSpawn {
                observation: GoinkyoObservation::NoGoinkyo { brigade_id: 1 },
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::KillPty {
                key: SessionKey::from_id("g1")
            }]
        );
        assert!(!state.goinkyo_pane.contains_key(&1));
        let Stage::Brigade { panes, .. } = &state.stage else {
            panic!("expected a staged brigade");
        };
        assert!(!panes.iter().any(|k| k.as_str() == "g1"));
    }

    // --- Goinkyo kickoff: PendingGoinkyoKickoff / CheckGoinkyoDirectoryTrust --

    #[test]
    fn spawning_a_goinkyo_queues_a_pending_kickoff() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(&mut state, &mut app, &brigade, awaiting_spawn(9), now);
        let [Cmd::OpenEmbedded { key, .. }] = cmds.as_slice() else {
            panic!("expected exactly one OpenEmbedded, got {cmds:?}");
        };

        assert_eq!(state.pending_goinkyo_kickoffs.len(), 1);
        assert_eq!(&state.pending_goinkyo_kickoffs[0].key, key);
        assert_eq!(state.pending_goinkyo_kickoffs[0].spawned_at, now);
        assert_eq!(
            state.pending_goinkyo_kickoffs[0].cwd,
            PathBuf::from("/work/alpha")
        );
        assert!(!state.pending_goinkyo_kickoffs[0].notified_untrusted);
    }

    #[test]
    fn a_claude_worker_never_queues_a_goinkyo_kickoff() {
        // The Goinkyo needs a kickoff because nobody sits at its keyboard —
        // a Worker's operator always gives it its own first turn, so
        // `open_worker` must never queue one of these regardless of
        // product.
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };

        open_worker(
            &mut state,
            1,
            "worker-1",
            &PathBuf::from("/work"),
            "",
            None,
            AgentKind::ClaudeCode,
        );

        assert!(state.pending_goinkyo_kickoffs.is_empty());
        // Sanity: the same spawn does queue an ordinary pending_open, so
        // this isn't just an empty-because-nothing-happened false negative.
        assert!(!state.pending_opens.is_empty());
    }

    #[test]
    fn tick_checks_goinkyo_directory_trust_once_quiet_long_enough_but_not_before() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, GOINKYO_TOKEN);
        let spawned_at = test_instant();
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: key.clone(),
            spawned_at,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            spawned_at + GOINKYO_KICKOFF_QUIET_PERIOD - Duration::from_millis(1),
        );
        assert!(cmds.is_empty(), "not quiet long enough yet");
        assert_eq!(state.pending_goinkyo_kickoffs.len(), 1);

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            spawned_at + GOINKYO_KICKOFF_QUIET_PERIOD,
        );
        assert_eq!(
            cmds,
            vec![Cmd::CheckGoinkyoDirectoryTrust {
                key: key.clone(),
                cwd: PathBuf::from("/work"),
            }]
        );
        // Never typed blind: the entry stays queued until the trust check
        // actually answers.
        assert_eq!(state.pending_goinkyo_kickoffs.len(), 1);
        assert!(state.pending_submits.is_empty());
    }

    #[test]
    fn a_trusted_goinkyo_directory_answer_types_the_kickoff_line_exactly_once() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, GOINKYO_TOKEN);
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: key.clone(),
                trusted: true,
            },
            test_instant(),
        );

        assert_eq!(
            cmds,
            vec![Cmd::WritePty {
                key: key.clone(),
                bytes: GOINKYO_KICKOFF_LINE.as_bytes().to_vec(),
            }]
        );
        assert_eq!(state.pending_goinkyo_kickoffs[0].attempts, 1);
        assert_eq!(state.pending_submits.len(), 1);
        assert_eq!(state.pending_submits[0].key, key);

        // A stale answer before the acknowledgement timeout cannot create a
        // duplicate turn.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked { key, trusted: true },
            test_instant(),
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn an_untrusted_goinkyo_directory_answer_keeps_waiting_and_notifies_once() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let key = SessionKey::new_worker(1, GOINKYO_TOKEN);
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: key.clone(),
                trusted: false,
            },
            test_instant(),
        );
        assert!(cmds.is_empty(), "never types blind on an untrusted answer");
        assert_eq!(state.pending_goinkyo_kickoffs.len(), 1);
        assert!(state.pending_goinkyo_kickoffs[0].notified_untrusted);
        let status = state.status.clone().unwrap();
        assert!(status.contains("Claude hasn't been told to trust"));

        // A second untrusted answer must not restamp the same notice.
        state.status = None;
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key,
                trusted: false,
            },
            test_instant(),
        );
        assert!(cmds.is_empty());
        assert!(state.status.is_none());
    }

    #[test]
    fn a_goinkyo_pane_unstaging_before_its_kickoff_fires_drops_the_pending_entry() {
        let mut state = EmporiumState::new(PrefixKey::default());
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![SessionKey::from_id("dir")],
            focused: 0,
        };
        let key = SessionKey::new_worker(1, GOINKYO_TOKEN);
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: key.clone(),
            spawned_at: test_instant(),
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        state.unstage(&key);

        assert!(state.pending_goinkyo_kickoffs.is_empty());
    }

    #[test]
    fn a_kickoff_queued_before_discovery_still_reaches_the_pane_after_it_renames() {
        // The regression this pins: discovery routinely resolves a Goinkyo's
        // synthetic key well inside `GOINKYO_KICKOFF_QUIET_PERIOD` (measured
        // live, ~123ms vs. 700ms — see `update_discovery_result`'s own doc
        // on its `pending_goinkyo_kickoffs` rename). Before that rename
        // existed, every step downstream kept addressing the dead synthetic
        // key, and `Cmd::WritePty` against a key nothing is listening on is
        // a silent no-op — the kickoff simply never landed, with nothing to
        // say why. `screens` stands in here for the shell's own `handles`
        // map (a different crate, unreachable from this test): both are
        // kept in lockstep by the same `Cmd::RekeyPty`, so a key present in
        // `screens` is a live key exactly when the shell's own handle is —
        // this is what makes "the Cmd was emitted" and "the Cmd's key is
        // actually still listening" two different, both-necessary claims.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let t0 = test_instant();

        let cmds = update(&mut state, &mut app, &brigade, awaiting_spawn(1), t0);
        let [Cmd::OpenEmbedded { key: syn_key, .. }] = cmds.as_slice() else {
            panic!("expected exactly one OpenEmbedded, got {cmds:?}");
        };
        let syn_key = syn_key.clone();
        assert_eq!(state.pending_goinkyo_kickoffs[0].key, syn_key);

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: syn_key.clone(),
            },
            t0,
        );
        assert!(state.screens.contains_key(&syn_key));

        let real_key = SessionKey::from_id("goinkyo-real-id");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: syn_key.clone(),
                session_id: "goinkyo-real-id".to_string(),
                member: Some((1, GOINKYO_TOKEN.to_string())),
            },
            t0,
        );
        assert_eq!(
            state.pending_goinkyo_kickoffs[0].key, real_key,
            "the pending kickoff must follow the rename, not stay on the dead synthetic key"
        );
        assert!(state.screens.contains_key(&real_key));
        assert!(!state.screens.contains_key(&syn_key));

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + GOINKYO_KICKOFF_QUIET_PERIOD,
        );
        assert_eq!(
            cmds,
            vec![Cmd::CheckGoinkyoDirectoryTrust {
                key: real_key.clone(),
                cwd: PathBuf::from("/work/alpha"),
            }],
            "the quiet-period check must ask about the renamed key"
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: real_key.clone(),
                trusted: true,
            },
            t0,
        );
        let [Cmd::WritePty { key: write_key, .. }] = cmds.as_slice() else {
            panic!("expected exactly one WritePty, got {cmds:?}");
        };
        assert_eq!(
            write_key, &real_key,
            "the kickoff must be written to the renamed key"
        );
        assert!(
            state.screens.contains_key(write_key),
            "the key the kickoff is written to must still be a live pane"
        );
    }

    #[test]
    fn a_kickoff_queued_before_an_auto_compaction_fork_still_reaches_the_pane_after_it_renames() {
        // Same guarantee as the discovery-side test above, for the other of
        // the two rename-following paths: a Goinkyo's own Claude session can
        // auto-compaction-fork before its kickoff ever fires, same as any
        // other member's can (`update_member_session_forked`'s own doc).
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let old_key = SessionKey::new_worker(1, GOINKYO_TOKEN);
        state.screens.insert(old_key.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir"), old_key.clone()],
            focused: 0,
        };
        let t0 = test_instant();
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: old_key.clone(),
            spawned_at: t0,
            cwd: PathBuf::from("/work/alpha"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        let new_key = SessionKey::from_id("goinkyo-forked-id");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: GOINKYO_TOKEN.to_string(),
                old_id: old_key.as_str().to_string(),
                new_id: "goinkyo-forked-id".to_string(),
            },
            t0,
        );
        assert_eq!(
            state.pending_goinkyo_kickoffs[0].key, new_key,
            "the pending kickoff must follow the fork's rename too"
        );
        assert!(state.screens.contains_key(&new_key));
        assert!(!state.screens.contains_key(&old_key));

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::GoinkyoDirectoryTrustChecked {
                key: new_key.clone(),
                trusted: true,
            },
            t0,
        );
        let [Cmd::WritePty { key: write_key, .. }] = cmds.as_slice() else {
            panic!("expected exactly one WritePty, got {cmds:?}");
        };
        assert_eq!(
            write_key, &new_key,
            "the kickoff must be written to the renamed key"
        );
        assert!(
            state.screens.contains_key(write_key),
            "the key the kickoff is written to must still be a live pane"
        );
    }

    #[test]
    fn pane_rekey_moves_every_session_key_indexed_fact() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let old = SessionKey::new_worker(7, "worker-1");
        let new = SessionKey::from_id("worker-real");
        let now = test_instant();
        state.screens.insert(old.clone(), Screen::new(24, 80));
        state.stage = Stage::Solo(old.clone());
        state.last_forwarded_input.insert(old.clone(), now);
        state.unsent_operator_input.insert(old.clone());
        state.last_output_at.insert(old.clone(), now);
        state.pending_opens.insert(old.clone(), PendingOpen::Solo);
        state.attention_panes.insert(old.clone());
        state.latched_attention_panes.insert(old.clone());
        state.ring_list_waiting.insert(old.clone());
        state.previous_ring_set = Some([old.clone()].into_iter().collect());
        state.expected_exits.insert(old.clone());
        state.last_relaunch.insert(old.clone(), now);
        state.pending_dismiss = Some(old.clone());
        state.last_focus_notified = Some(old.clone());
        state
            .member_tokens
            .insert(old.clone(), "worker-1".to_string());
        state.member_brigades.insert(old.clone(), 7);
        state.goinkyo_pane.insert(7, old.clone());
        state.pending_submits.push(PendingSubmit {
            key: old.clone(),
            nudged_at: now,
        });
        state.pending_kickoffs.push(PendingKickoff {
            key: old.clone(),
            spawned_at: now,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });
        state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
            key: old.clone(),
            spawned_at: now,
            cwd: PathBuf::from("/work"),
            notified_untrusted: false,
            sent_at: None,
            attempts: 0,
        });

        assert!(state.rekey_pane_state(&old, &new));
        assert!(!state.screens.contains_key(&old));
        assert!(!state.last_forwarded_input.contains_key(&old));
        assert!(!state.unsent_operator_input.contains(&old));
        assert!(!state.last_output_at.contains_key(&old));
        assert!(!state.pending_opens.contains_key(&old));
        assert!(!state.attention_panes.contains(&old));
        assert!(!state.latched_attention_panes.contains(&old));
        assert!(!state.ring_list_waiting.contains(&old));
        assert!(!state.previous_ring_set.as_ref().unwrap().contains(&old));
        assert!(!state.expected_exits.contains(&old));
        assert!(!state.last_relaunch.contains_key(&old));
        assert_ne!(state.pending_dismiss.as_ref(), Some(&old));
        assert_ne!(state.last_focus_notified.as_ref(), Some(&old));
        assert!(!state.member_tokens.contains_key(&old));
        assert!(!state.member_brigades.contains_key(&old));
        assert!(state.goinkyo_pane.values().all(|key| key != &old));
        assert!(
            state
                .pending_submits
                .iter()
                .all(|pending| pending.key != old)
        );
        assert!(
            state
                .pending_kickoffs
                .iter()
                .all(|pending| pending.key != old)
        );
        assert!(
            state
                .pending_goinkyo_kickoffs
                .iter()
                .all(|pending| pending.key != old)
        );
    }

    #[test]
    fn a_pending_submit_queued_before_discovery_still_reaches_the_pane_after_it_renames() {
        // Same bug class as the kickoff tests above, for `PendingSubmit`
        // (the delayed `\r` behind a relay nudge or either kickoff's own
        // phase two): `update_discovery_result` must rename it too, and
        // unconditionally — `member: None` here on purpose, to prove the
        // rename isn't gated on being a recognized brigade member the way
        // `goinkyo_pane`/`pending_goinkyo_kickoffs`'s own follow is.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let old_key = SessionKey::new_worker(1, "worker-1");
        state.screens.insert(old_key.clone(), Screen::new(24, 80));
        let t0 = test_instant();
        state.pending_submits.push(PendingSubmit {
            key: old_key.clone(),
            nudged_at: t0,
        });

        let new_key = SessionKey::from_id("w1-real");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::DiscoveryResult {
                key: old_key.clone(),
                session_id: "w1-real".to_string(),
                member: None,
            },
            t0,
        );
        assert_eq!(
            state.pending_submits[0].key, new_key,
            "the pending submit must follow the rename, not stay on the dead synthetic key"
        );
        assert!(state.screens.contains_key(&new_key));
        assert!(!state.screens.contains_key(&old_key));

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + RELAY_SUBMIT_DELAY,
        );
        let [
            Cmd::WritePty {
                key: write_key,
                bytes,
            },
        ] = cmds.as_slice()
        else {
            panic!("expected exactly one WritePty, got {cmds:?}");
        };
        assert_eq!(
            write_key, &new_key,
            "the submit must be written to the renamed key"
        );
        assert_eq!(bytes, b"\r");
        assert!(
            state.screens.contains_key(write_key),
            "the key the submit is written to must still be a live pane"
        );
    }

    #[test]
    fn a_pending_submit_queued_before_an_auto_compaction_fork_still_reaches_the_pane_after_it_renames()
     {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let old_key = SessionKey::from_id("w1-old");
        state.screens.insert(old_key.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(SessionKey::from_id("dir")),
            panes: vec![SessionKey::from_id("dir"), old_key.clone()],
            focused: 0,
        };
        let t0 = test_instant();
        state.pending_submits.push(PendingSubmit {
            key: old_key.clone(),
            nudged_at: t0,
        });

        let new_key = SessionKey::from_id("w1-new");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "worker-1".to_string(),
                old_id: "w1-old".to_string(),
                new_id: "w1-new".to_string(),
            },
            t0,
        );
        assert_eq!(
            state.pending_submits[0].key, new_key,
            "the pending submit must follow the fork's rename too"
        );
        assert!(state.screens.contains_key(&new_key));
        assert!(!state.screens.contains_key(&old_key));

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            t0 + RELAY_SUBMIT_DELAY,
        );
        let [
            Cmd::WritePty {
                key: write_key,
                bytes,
            },
        ] = cmds.as_slice()
        else {
            panic!("expected exactly one WritePty, got {cmds:?}");
        };
        assert_eq!(
            write_key, &new_key,
            "the submit must be written to the renamed key"
        );
        assert_eq!(bytes, b"\r");
        assert!(
            state.screens.contains_key(write_key),
            "the key the submit is written to must still be a live pane"
        );
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
            director: None,
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
            director: None,
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
            director: Some(director.clone()),
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
            director: Some(director.clone()),
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
    fn armed_x_on_the_director_pane_has_no_dismiss_choice_even_when_panes_are_reversed() {
        // The Worker staged solo first, ahead of the Director, so `panes[0]`
        // is the Worker — proves the dismissable check reads `director`,
        // not position.
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![worker, director],
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
                KeyCode::Char('y'),
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
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
                    effort: None,
                    permission_mode: None,
                    disallowed_tools: None,
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

    /// A brigade 1 stage (Director + `worker-1..=3`) whose stored layout
    /// gives every Worker a weight and the master edge a non-default place.
    fn weighted_three_worker_brigade() -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, app, brigade, now) = drag_fixture(3);
        let mut layout = BrigadeLayout {
            master_permille: 620,
            ..BrigadeLayout::default()
        };
        layout.row_weights.insert("worker-1".to_string(), 8);
        layout.row_weights.insert("worker-2".to_string(), 12);
        layout.row_weights.insert("worker-3".to_string(), 16);
        state.brigade_layouts.insert(1, layout);
        (state, app, brigade, now)
    }

    fn dismissed(brigade_id: BrigadeId) -> Event {
        Event::WorkerDismissed {
            brigade_id,
            result: Ok((HashSet::new(), HashSet::new())),
        }
    }

    #[test]
    fn dismissing_a_worker_forgets_its_row_weight_and_only_that() {
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        // The way the confirm dialog leaves things for a staged Worker.
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(1), now);

        let layout = &state.brigade_layouts[&1];
        assert_eq!(
            layout
                .row_weights
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["worker-1".to_string(), "worker-2".to_string()]),
        );
        assert_eq!(
            layout.master_permille, 620,
            "the brigade's other prefs stay"
        );
    }

    #[test]
    fn a_reused_token_does_not_inherit_the_dismissed_workers_height() {
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));
        update(&mut state, &mut app, &brigade, dismissed(1), now);
        assert_eq!(state.stack_weights(), vec![8, 12], "w3 left the stage");

        // add-worker numbers max + 1, so the next Worker is `worker-3` again.
        let again = SessionKey::from_id("w3-again");
        state.screens.insert(again.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(again.clone(), "worker-3".to_string());
        if let Stage::Brigade { panes, .. } = &mut state.stage {
            panes.push(again);
        }

        assert_eq!(
            state.stack_weights(),
            vec![8, 12, 10],
            "the mean of the others, not the 16 the dismissed one had"
        );
    }

    #[test]
    fn dismissing_a_worker_whose_pane_is_already_gone_still_forgets_its_weight() {
        // The pane exited earlier, so `member_tokens` has already lost it:
        // the token can only come from what the dismissal itself recorded.
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        let gone = SessionKey::from_id("w2");
        state.unstage(&gone);
        assert!(state.member_token_for(&gone).is_none());
        state.pending_dismiss = Some(gone);
        state.pending_dismiss_member = Some((1, "worker-2".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(1), now);

        let weights = &state.brigade_layouts[&1].row_weights;
        assert!(!weights.contains_key("worker-2"));
        assert!(weights.contains_key("worker-1") && weights.contains_key("worker-3"));
    }

    /// Brigade 1 staged with weights, brigade 2's preferences stored too, both
    /// with a `worker-3`.
    fn two_brigades_with_a_worker_3_each() -> (EmporiumState, App, BrigadeConfig, Instant) {
        let (mut state, app, brigade, now) = weighted_three_worker_brigade();
        let mut other = BrigadeLayout::default();
        other.row_weights.insert("worker-3".to_string(), 99);
        state.brigade_layouts.insert(2, other);
        (state, app, brigade, now)
    }

    #[test]
    fn another_brigades_dismissal_result_forgets_nothing_and_keeps_the_pending_member() {
        // Brigade 1's dismissal is pending; brigade 2's result arrives first.
        let (mut state, mut app, brigade, now) = two_brigades_with_a_worker_3_each();
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(2), now);

        assert_eq!(
            state.brigade_layouts[&2].row_weights["worker-3"], 99,
            "brigade 2's same-named token is not touched"
        );
        assert_eq!(state.brigade_layouts[&1].row_weights.len(), 3);
        assert_eq!(
            state.pending_dismiss_member,
            Some((1, "worker-3".to_string())),
            "the entry belongs to brigade 1 and stays"
        );
    }

    #[test]
    fn the_right_brigades_result_after_a_foreign_one_forgets_once_and_only_in_its_brigade() {
        // The other order: brigade 1's result lands with the member still
        // captured for it, whatever arrived before.
        let (mut state, mut app, brigade, now) = two_brigades_with_a_worker_3_each();
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        update(&mut state, &mut app, &brigade, dismissed(2), now);
        // `pending_dismiss` is spent by any result (see the function's own
        // doc); the member is not, so restate the pending dismissal the way
        // the confirm dialog would, as a second dismissal in brigade 1.
        state.pending_dismiss = Some(SessionKey::from_id("w3"));

        update(&mut state, &mut app, &brigade, dismissed(1), now);

        assert!(
            !state.brigade_layouts[&1]
                .row_weights
                .contains_key("worker-3")
        );
        assert_eq!(state.brigade_layouts[&1].row_weights.len(), 2);
        assert_eq!(state.brigade_layouts[&2].row_weights["worker-3"], 99);
        assert_eq!(state.pending_dismiss_member, None);
    }

    #[test]
    fn a_dismissal_for_brigade_2_then_a_result_for_brigade_1_forgets_nothing_in_either() {
        let (mut state, mut app, brigade, now) = two_brigades_with_a_worker_3_each();
        state.pending_dismiss = Some(SessionKey::from_id("x"));
        state.pending_dismiss_member = Some((2, "worker-3".to_string()));

        update(&mut state, &mut app, &brigade, dismissed(1), now);

        assert_eq!(state.brigade_layouts[&1].row_weights.len(), 3);
        assert_eq!(state.brigade_layouts[&2].row_weights["worker-3"], 99);
        assert_eq!(
            state.pending_dismiss_member,
            Some((2, "worker-3".to_string()))
        );
    }

    #[test]
    fn a_failed_dismissal_keeps_the_row_weight() {
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        state.pending_dismiss = Some(SessionKey::from_id("w3"));
        state.pending_dismiss_member = Some((1, "worker-3".to_string()));

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::WorkerDismissed {
                brigade_id: 1,
                result: Err("store is locked".to_string()),
            },
            now,
        );

        assert_eq!(state.brigade_layouts[&1].row_weights.len(), 3);
        assert!(state.pending_dismiss_member.is_none(), "nothing left stale");
    }

    #[test]
    fn the_confirm_dialog_records_which_member_a_dismissal_removes() {
        // Synthetic key (token in the key) and resolved key (token from the
        // membership round trip): both leave the member for `WorkerDismissed`.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![]);
        let key = SessionKey::new_worker(1, "worker-4");
        app.open_confirm_kill_modal(key.as_str().to_string(), "worker-4".to_string(), true);
        app.modal_select_next();
        let brigade = brigade_config();
        let now = test_instant();
        let enter = || {
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            )))
        };
        update(&mut state, &mut app, &brigade, enter(), now);
        assert_eq!(
            state.pending_dismiss_member,
            Some((1, "worker-4".to_string()))
        );

        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("w5")]);
        app.open_confirm_kill_modal("w5".to_string(), "w5".to_string(), true);
        app.modal_select_next();
        update(&mut state, &mut app, &brigade, enter(), now);
        assert_eq!(state.pending_dismiss_member, None, "unknown until resolved");
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "w5".to_string(),
                membership: Some((1, "worker-5".to_string(), BrigadeRole::Worker)),
                members: None,
                layout: None,
            },
            now,
        );
        assert_eq!(
            state.pending_dismiss_member,
            Some((1, "worker-5".to_string()))
        );
    }

    #[test]
    fn a_pane_exiting_keeps_its_row_weight() {
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        let before = state.brigade_layouts[&1].row_weights.clone();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: SessionKey::from_id("w2"),
                reason: None,
            },
            now,
        );

        assert_eq!(state.brigade_layouts[&1].row_weights, before);
    }

    #[test]
    fn a_member_leaving_the_stage_and_coming_back_keeps_its_row_weight() {
        let (mut state, mut app, brigade, now) = weighted_three_worker_brigade();
        let before = state.brigade_layouts[&1].row_weights.clone();

        state.stage = Stage::Empty;
        state.unstage(&SessionKey::from_id("w1"));
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: vec![],
                brigades: Vec::new(),
            },
            now,
        );

        assert_eq!(state.brigade_layouts[&1].row_weights, before);
        assert_eq!(state.brigade_layouts[&1].master_permille, 620);
    }

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
    fn kill_confirm_dismiss_on_a_synthetic_goinkyo_key_emits_dismiss_worker_directly() {
        // `SessionKey::worker_identity` never checked role to begin with
        // (see its own doc) — this pins that the synthetic-key path already
        // worked for a still-undiscovered Goinkyo before this round, same
        // as it does for a Worker.
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::new_worker(1, "goinkyo");
        let mut app = app_with(vec![]);
        app.open_confirm_kill_modal(key.as_str().to_string(), "goinkyo".to_string(), true);
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
                token: "goinkyo".to_string(),
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
                layout: None,
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
    fn kill_confirm_dismiss_on_a_resolved_goinkyo_key_works_the_same_way_as_a_worker() {
        // The actual bug this round fixes: `update_membership_resolved`
        // used to abort silently for anything but `BrigadeRole::Worker`,
        // which meant choosing Dismiss on a discovered Goinkyo's own pane
        // did nothing at all.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("g1")]);
        app.open_confirm_kill_modal("g1".to_string(), "g1".to_string(), true);
        app.modal_select_next(); // ClosePane -> Dismiss
        let brigade = brigade_config();
        let now = test_instant();

        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Input(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                Modifiers::NONE,
            ))),
            now,
        );
        assert_eq!(state.pending_dismiss, Some(SessionKey::from_id("g1")));

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "g1".to_string(),
                membership: Some((1, "goinkyo".to_string(), BrigadeRole::Goinkyo)),
                members: None,
                layout: None,
            },
            now,
        );

        assert_eq!(
            cmds,
            vec![Cmd::Store(StoreIntent::DismissWorker {
                brigade_id: 1,
                token: "goinkyo".to_string(),
            })]
        );
    }

    #[test]
    fn kill_confirm_dismiss_on_the_directors_own_resolved_key_aborts() {
        // The Director never even gets the Dismiss choice in the first
        // place (`dismissable` is false for it — see `PrefixAction::Kill`'s
        // own doc) — this proves the engine's own decision, independent of
        // the modal ever offering it, still refuses: dismissing a Director
        // is disband's job, not this flow's, no matter how the round trip
        // was reached.
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = app_with(vec![row("dir")]);
        state.pending_membership = Some(PendingMembership::DismissWorker);
        state.pending_dismiss = Some(SessionKey::from_id("dir"));
        let brigade = brigade_config();
        let now = test_instant();

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, "director".to_string(), BrigadeRole::Director)),
                members: None,
                layout: None,
            },
            now,
        );

        assert!(cmds.is_empty());
        assert!(state.pending_dismiss.is_none());
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
            director: Some(director.clone()),
            panes: vec![director.clone(), w1.clone(), w2.clone()],
            focused: 1,
        };
        state.pending_dismiss = Some(w1.clone());
        let mut app = app_with(vec![row("dir"), row("w1"), row("w2")])
            .with_hidden_member_ids(["w1".to_string(), "w2".to_string()].into_iter().collect());
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
            director: Some(director.clone()),
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
            director: Some(director.clone()),
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
    fn disband_while_staged_kills_workers_but_not_the_director_even_when_panes_are_reversed() {
        // The Worker staged solo first, ahead of the Director, so
        // `panes[0]` is a Worker — proves the "who survives" split reads
        // `director`, not position.
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let w1 = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![w1.clone(), director.clone()],
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

        assert_eq!(cmds, vec![Cmd::KillPty { key: w1 }]);
        match &state.stage {
            Stage::Solo(key) => assert_eq!(key, &director),
            _ => panic!("expected the director to remain staged solo"),
        }
    }

    #[test]
    fn disbanding_with_no_known_director_kills_nobody() {
        // Kill is irreversible: `director == None` (unresolved, or its own
        // pane already closed) must not fall back to guessing who's safe —
        // see `update_disbanded`'s own doc.
        let mut state = EmporiumState::new(PrefixKey::default());
        let w1 = SessionKey::from_id("w1");
        let w2 = SessionKey::from_id("w2");
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![w1, w2],
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

        assert!(cmds.is_empty(), "nobody may be killed: {cmds:?}");
        assert!(matches!(state.stage, Stage::Empty));
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
            director: None,
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
            director: None,
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
            director: None,
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
            cycle_forward(FocusSlot::Sidebar, pane_count, &[]),
            FocusSlot::Pane(0)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(0), pane_count, &[]),
            FocusSlot::Pane(1)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(1), pane_count, &[]),
            FocusSlot::Pane(2)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(2), pane_count, &[]),
            FocusSlot::Sidebar
        );
    }

    #[test]
    fn cycle_forward_on_a_solo_stage_toggles_between_sidebar_and_the_pane() {
        assert_eq!(
            cycle_forward(FocusSlot::Sidebar, 1, &[]),
            FocusSlot::Pane(0)
        );
        assert_eq!(
            cycle_forward(FocusSlot::Pane(0), 1, &[]),
            FocusSlot::Sidebar
        );
    }

    #[test]
    fn cycle_forward_on_an_empty_stage_is_a_noop() {
        assert_eq!(
            cycle_forward(FocusSlot::Sidebar, 0, &[]),
            FocusSlot::Sidebar
        );
    }

    #[test]
    fn arming_from_the_sidebar_then_o_lands_on_the_first_pane() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
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
                arrow_target(from, direction, pane_count, &[]),
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
            director: None,
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
            director: None,
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
            Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
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
            director: None,
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
            director: None,
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
    fn bell_for_selected_pane_while_away_latches_rings_and_clears_on_regain() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        // The selected pane is not effectively focused once banto sits behind
        // another window, even though its internal selection stays put.
        let seeded = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: false },
            now,
        );
        assert!(
            !seeded.iter().any(|cmd| matches!(cmd, Cmd::RingHostBell)),
            "startup seeds state but must not announce it: {seeded:?}"
        );

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"\x07\x07".to_vec(),
            },
            now + Duration::from_millis(1),
        );
        assert_eq!(cmds, vec![Cmd::RingHostBell]);
        assert!(state.attention_panes().contains(&key));
        assert_eq!(
            state.visible_waiting_count(),
            0,
            "BEL is not Claude Waiting"
        );

        // Regaining banto's OS focus makes this selected pane effectively
        // focused and clears its latch without a bespoke focus-edge record.
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::WindowFocusChanged { focused: true },
            now + Duration::from_millis(2),
        );
        assert!(cmds.is_empty());
        assert!(!state.attention_panes().contains(&key));
    }

    #[test]
    fn bell_for_selected_frontmost_pane_neither_latches_nor_rings() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let key = SessionKey::from_id("sess-1");
        state.stage = Stage::Solo(key.clone());
        state.focus = Focus::Pane;
        state.screens.insert(key.clone(), Screen::new(24, 80));
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();

        // Seed under the default frontmost assumption. A BEL from the pane
        // already visible to the operator is not a new attention mark.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
            now,
        );
        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: key.clone(),
                chunk: b"\x07".to_vec(),
            },
            now + Duration::from_millis(1),
        );
        assert!(cmds.is_empty());
        assert!(!state.attention_panes().contains(&key));
    }

    #[test]
    fn attention_edges_coalesce_and_respect_the_global_cooldown() {
        let mut state = EmporiumState::new(PrefixKey::default());
        let a = SessionKey::from_id("a");
        let b = SessionKey::from_id("b");
        let c = SessionKey::from_id("c");
        state.stage = Stage::Brigade {
            id: 1,
            director: None,
            panes: vec![a.clone(), b.clone(), c.clone()],
            focused: 0,
        };
        for key in [&a, &b, &c] {
            state.screens.insert(key.clone(), Screen::new(24, 80));
        }
        let mut app = app_with(vec![]);
        let brigade = brigade_config();
        let now = test_instant();
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Tick {
                relay: Vec::new(),
                brigades: Vec::new(),
            },
            now,
        );

        state.focus = Focus::Sidebar;
        let first = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: a,
                chunk: b"\x07".to_vec(),
            },
            now + Duration::from_millis(1),
        );
        assert_eq!(first, vec![Cmd::RingHostBell]);
        let suppressed = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: b,
                chunk: b"\x07".to_vec(),
            },
            now + Duration::from_millis(2),
        );
        assert!(suppressed.is_empty(), "the second edge is inside cooldown");
        let after_cooldown = update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyOutput {
                key: c,
                chunk: b"\x07".to_vec(),
            },
            now + ATTENTION_BELL_COOLDOWN + Duration::from_millis(1),
        );
        assert_eq!(after_cooldown, vec![Cmd::RingHostBell]);
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

    // --- a member's pane dying on its own (`relaunch_member`) -------------

    /// A staged brigade with one Director pane and one Worker pane, both
    /// already open, and the Worker carrying its member token the way
    /// `stage_brigade` leaves it.
    fn staged_brigade_with_worker() -> (EmporiumState, App) {
        let mut state = EmporiumState::new(PrefixKey::default());
        let director = SessionKey::from_id("dir");
        let worker = SessionKey::from_id("w1");
        state.screens.insert(director.clone(), Screen::new(24, 80));
        state.screens.insert(worker.clone(), Screen::new(24, 80));
        state
            .member_tokens
            .insert(director.clone(), DIRECTOR_TOKEN.to_string());
        state
            .member_tokens
            .insert(worker.clone(), "worker-1".to_string());
        state.member_brigades.insert(director.clone(), 1);
        state.member_brigades.insert(worker.clone(), 1);
        state.stage = Stage::Brigade {
            id: 1,
            director: Some(director.clone()),
            panes: vec![director, worker],
            focused: 0,
        };
        (state, app_with(vec![row("dir"), row("w1")]))
    }

    /// Losing a pane relayouts the tiles that remain, so every exit here
    /// also emits a `ResizePty` for the survivor. That is the stage doing
    /// its job, not an answer to "was this member put back".
    fn without_resizes(cmds: Vec<Cmd>) -> Vec<Cmd> {
        cmds.into_iter()
            .filter(|cmd| !matches!(cmd, Cmd::ResizePty { .. }))
            .collect()
    }

    fn worker_exits(
        state: &mut EmporiumState,
        app: &mut App,
        brigade: &BrigadeConfig,
        now: Instant,
    ) -> Vec<Cmd> {
        without_resizes(update(
            state,
            app,
            brigade,
            Event::PtyExited {
                key: SessionKey::from_id("w1"),
                reason: Some(PtyExitReason::Code(1)),
            },
            now,
        ))
    }

    #[test]
    fn a_worker_that_dies_on_its_own_is_restarted_in_place() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        match &cmds[..] {
            [
                Cmd::OpenEmbedded {
                    key,
                    target,
                    brigade: Some((id, token, role)),
                    ..
                },
            ] => {
                assert_eq!(key, &worker);
                assert_eq!(target.id, "w1", "the same session, resumed");
                assert_eq!(*id, 1);
                assert_eq!(token, "worker-1");
                assert_eq!(*role, BrigadeRole::Worker);
            }
            other => panic!("expected one OpenEmbedded, got {other:?}"),
        }
        assert!(
            matches!(&state.stage, Stage::Brigade { panes, .. } if panes.contains(&worker)),
            "the tile keeps its place while the pane comes back"
        );
        assert!(
            !state.screens.contains_key(&worker),
            "its screen is gone until the spawn lands"
        );
        assert!(state.status.unwrap().contains("restarting"));
    }

    #[test]
    fn an_offstage_worker_that_dies_is_restarted_in_its_own_brigade() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let visible = SessionKey::from_id("other-director");
        state.screens.insert(visible.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 2,
            director: Some(visible.clone()),
            panes: vec![visible],
            focused: 0,
        };

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        assert!(matches!(
            &cmds[..],
            [Cmd::OpenEmbedded {
                brigade: Some((1, token, BrigadeRole::Worker)),
                ..
            }] if token == "worker-1"
        ));
    }

    #[test]
    fn an_offstage_members_compaction_fork_rekeys_its_live_pane() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let visible = SessionKey::from_id("other-director");
        state.screens.insert(visible.clone(), Screen::new(24, 80));
        state.stage = Stage::Brigade {
            id: 2,
            director: Some(visible.clone()),
            panes: vec![visible],
            focused: 0,
        };
        let old_key = SessionKey::from_id("w1");
        let new_key = SessionKey::from_id("w1-new");

        let cmds = update(
            &mut state,
            &mut app,
            &brigade,
            Event::MemberSessionForked {
                brigade_id: 1,
                token: "worker-1".to_string(),
                old_id: "w1".to_string(),
                new_id: "w1-new".to_string(),
            },
            test_instant(),
        );

        assert!(state.screens.contains_key(&new_key));
        assert!(!state.screens.contains_key(&old_key));
        assert_eq!(state.member_brigades.get(&new_key), Some(&1));
        assert!(matches!(
            cmds.first(),
            Some(Cmd::RekeyPty { from, to }) if from == &old_key && to == &new_key
        ));
    }

    #[test]
    fn a_restarted_worker_carries_the_same_configured_permission_mode_as_a_fresh_spawn() {
        // `relaunch_member` is a separate code path from `stage_brigade`'s own
        // resume arm (see `stage_brigade_passes_the_configured_permission_
        // mode_to_a_resumed_worker_and_goinkyo_but_never_to_director`) — both
        // go through `RoleLaunch::of`, but a crash-triggered restart is
        // reachable only through here.
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = BrigadeConfig {
            worker_permission_mode: "acceptEdits".to_string(),
            ..BrigadeConfig::default()
        };

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        match &cmds[..] {
            [
                Cmd::OpenEmbedded {
                    permission_mode, ..
                },
            ] => {
                assert_eq!(permission_mode.as_deref(), Some("acceptEdits"));
            }
            other => panic!("expected one OpenEmbedded, got {other:?}"),
        }
    }

    #[test]
    fn a_restarted_worker_starts_with_no_unsent_draft() {
        // The process that held the draft is gone.
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");
        if let Stage::Brigade { focused, .. } = &mut state.stage {
            *focused = 1;
        }
        state.focus = Focus::Pane;
        forward_key(
            &mut state,
            &mut app,
            &brigade,
            KeyCode::Char('a'),
            Modifiers::NONE,
            test_instant(),
        );
        assert!(state.has_unsent_operator_input(&worker));

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        assert!(matches!(&cmds[..], [Cmd::OpenEmbedded { .. }]), "{cmds:?}");
        assert!(!state.has_unsent_operator_input(&worker));
    }

    #[test]
    fn a_worker_the_operator_killed_stays_dead() {
        // The plain `x` kill: the one deliberate exit that changes nothing
        // else observable, and so the one that needs `kill_pane`'s record to
        // be told from a crash at all.
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");
        app.open_confirm_kill_modal("w1".to_string(), "w1".to_string(), true);

        let kill = confirm_kill_modal(&mut state, &mut app);
        assert!(matches!(&kill[..], [Cmd::KillPty { key }] if *key == worker));

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        assert!(
            cmds.is_empty(),
            "a killed Worker must not come back: {cmds:?}"
        );
        assert!(
            matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&worker)),
            "and its tile goes with it"
        );
    }

    #[test]
    fn a_worker_dismissed_from_the_brigade_stays_dead() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");
        state.pending_dismiss = Some(worker.clone());

        let kill = update_worker_dismissed(
            &mut state,
            &mut app,
            1,
            Ok((HashSet::new(), HashSet::new())),
            test_instant(),
        );
        assert!(matches!(&kill[..], [Cmd::KillPty { key }] if *key == worker));

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        assert!(
            cmds.is_empty(),
            "a dismissed Worker must not come back: {cmds:?}"
        );
    }

    #[test]
    fn the_directors_own_pane_is_not_restarted_by_this() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let director = SessionKey::from_id("dir");

        let cmds = without_resizes(update(
            &mut state,
            &mut app,
            &brigade,
            Event::PtyExited {
                key: director,
                reason: None,
            },
            test_instant(),
        ));

        assert!(
            cmds.is_empty(),
            "the Director has its own reopen paths: {cmds:?}"
        );
        assert!(state.status.unwrap().contains("session ended"));
    }

    #[test]
    fn a_goinkyos_pane_is_not_restarted_by_this() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        // Same shape as a Worker as far as the stage is concerned; only
        // `goinkyo_pane` says otherwise, and that is what must be consulted.
        state.goinkyo_pane.insert(1, SessionKey::from_id("w1"));

        let cmds = worker_exits(&mut state, &mut app, &brigade, test_instant());

        assert!(
            cmds.is_empty(),
            "a Goinkyo's absence is answered against the store, not here: {cmds:?}"
        );
    }

    #[test]
    fn a_worker_that_cannot_stay_up_is_restarted_once_and_then_left_closed() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");
        let start = test_instant();

        assert_eq!(
            worker_exits(&mut state, &mut app, &brigade, start).len(),
            1,
            "the first death is restarted"
        );
        // The spawn landed and the child fell over again a second later.
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: worker.clone(),
            },
            start + Duration::from_secs(1),
        );
        let cmds = worker_exits(
            &mut state,
            &mut app,
            &brigade,
            start + Duration::from_secs(2),
        );

        assert!(
            cmds.is_empty(),
            "a member that cannot start must not be respawned as fast as it fails: {cmds:?}"
        );
        assert!(
            state.status.as_deref().unwrap().contains("left closed"),
            "and the operator is told why the tile went, not left guessing: {:?}",
            state.status
        );
    }

    #[test]
    fn a_worker_that_dies_again_much_later_is_restarted_again() {
        let (mut state, mut app) = staged_brigade_with_worker();
        let brigade = brigade_config();
        let worker = SessionKey::from_id("w1");
        let start = test_instant();

        worker_exits(&mut state, &mut app, &brigade, start);
        update(
            &mut state,
            &mut app,
            &brigade,
            Event::Spawned {
                key: worker.clone(),
            },
            start + Duration::from_secs(1),
        );

        let cmds = worker_exits(
            &mut state,
            &mut app,
            &brigade,
            start + RELAUNCH_COOLDOWN + Duration::from_secs(1),
        );

        assert_eq!(
            cmds.len(),
            1,
            "one death an hour is an agent updating itself, not a broken one: {cmds:?}"
        );
    }
}
