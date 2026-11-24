//! Live-session state and activity classification — the I/O half of status
//! (`banto_core::status` keeps the pure bucketing math: `AgeThresholds` /
//! `age_bucket`).
//!
//! Sources, in priority order (docs/REQUIREMENTS.md, "Activity indicator"):
//! 1. `<claude_home>/sessions/<pid>.json` + PID alive + status=busy -> Busy
//! 2. `<claude_home>/sessions/<pid>.json` + PID alive + status=waiting -> Waiting
//! 3. PID alive + status=shell (a background command still running) -> Busy
//! 4. PID alive, any other or unknown status -> Alive
//! 5. otherwise bucket the session file mtime into Today / ThisWeek / Older
//!
//! The status values belong to Claude Code, not banto. Unknown values must
//! degrade to [`Activity::Alive`], preserving today's behavior if upstream
//! adds another state.
//!
//! The same live file also answers a second, stricter question —
//! [`claude_quiescence`], "is this session known to be doing nothing?" — where
//! the degradation runs the other way: only the `idle` report is idle. It tells
//! `shell` apart from the rest (`Quiescence::Background`), because a tile and the
//! quit question say more about a background command than about other work.
//!
//! PID liveness sits behind [`ProcessProbe`] so tests can mock it (no real
//! processes in tests). Bucketing itself lives in `banto_core::status`, a
//! pure function of (mtime, now, thresholds).

mod live;
mod probe;

pub use live::{LiveSession, read_live_sessions};
pub use probe::{ProcessProbe, SysinfoProbe, ancestry_distance, ancestry_reaches};

use std::collections::HashSet;
use std::time::SystemTime;

use banto_core::model::{Activity, Quiescence, SessionMeta};
use banto_core::status::{AgeThresholds, age_bucket};

/// The two [`LiveSession::status`] values compared against anywhere in this
/// codebase — Claude Code's own vocabulary, not banto's, so an unrecognized
/// third value (this crate has only ever seen these two plus `"idle"`) must
/// keep degrading to [`Activity::Alive`] here and to `None` at every other
/// reader, never break. Observed set as of `claude` 2.1.222, 2026-08-06:
/// exactly `{"idle", "busy", "waiting"}`, live-polled at 1s resolution
/// through a real permission prompt and a real plan-mode prompt — `"idle"`
/// itself needs no constant, since anything that isn't one of the two below
/// already falls through to `Activity::Alive`'s catch-all.
///
/// A third way a session stops for a person was measured separately on
/// 2.1.224, 2026-08-07, because nothing said it had to behave like the other
/// two: an MCP server's `elicitation/create`, which Claude Code answers with
/// an interactive dialog. Polled through a real one — a scratch MCP server
/// that blocks until answered — the field read `"busy"` as the tool call
/// started, `"waiting"` for the whole thirteen seconds the dialog was on
/// screen, and `"idle"` once it was answered. So this needs no MCP-specific
/// handling, and the reason it needs none is written down rather than
/// assumed.
///
/// `banto::session::activity_tag` also emits the strings `"busy"` and
/// `"waiting"`, and that is a coincidence, not a shared fact: those are
/// banto's own `list`-subcommand output vocabulary, keyed off the already-
/// classified [`Activity`] enum, never off this raw field. Do not route it
/// through these constants — the two vocabularies are independent, agreeing
/// today only by coincidence, and keeping them separate is what stops a
/// change to one (upstream renaming this field's value, say) from silently
/// moving the other. Nothing here would catch that if they were unified —
/// renaming a shared constant compiles cleanly either way.
///
/// A fourth value, `"shell"`, was reported on 2.1.323, 2026-11-13, by another
/// member's measurement (not re-measured here): the turn has ended but a
/// background shell command of the session, or of one of its sub-agents, is
/// still running; when that work ends Claude is re-invoked (`"busy"`) and then
/// goes `"idle"`. Four readers, each with its own answer on purpose, so that
/// changing a word on screen cannot change who gets nudged or what a quit loses:
///
/// - The session list ([`classify_from_alive_pids`]) shows it as
///   [`Activity::Busy`]: a command is running.
/// - The quit question and the brigade watch ([`claude_quiescence`]) count it as
///   not idle for the same reason, but as `Quiescence::Background`, the one
///   reason a tile and the quit prompt name: the turn has ended and only a
///   background command runs, so a tile says `idle` with a mark and the prompt
///   says "running a background command". The watch holds its quiet stretch for
///   it exactly as for work.
/// - The relay ([`claude_nudgeable`]) counts it as idle enough to nudge, since
///   the turn has ended and the member can take a message.
/// - `brigade_status` prints the raw status, with `shell` spelled out.
pub const LIVE_STATUS_BUSY: &str = "busy";
pub const LIVE_STATUS_WAITING: &str = "waiting";
pub const LIVE_STATUS_IDLE: &str = "idle";
pub const LIVE_STATUS_SHELL: &str = "shell";

/// The one [`LiveSession::waiting_for`] value anything here acts on.
///
/// Measured 2026-11-13 on Claude Code 2.1.323, by sampling a live file from
/// outside: next to `status: "waiting"` the file can carry `waitingFor`. It read
/// `"input needed"` while the session sat in the picker for a question the
/// model had asked, and `"dialog open"` while the OPERATOR had a slash-command
/// picker (`/model`) open that he had opened himself; when he closed it the
/// status went back to `"idle"`. Not measured: the value for a permission
/// prompt, for a dialog the product opens on its own, and what `status` says
/// when a dialog is opened over a running turn. A table of reason strings was
/// read (not measured) in the Claude Code bundle: "permission prompt", "sandbox
/// request", "worker request", "goal proposal", "input needed", "dialog open".
///
/// Compared for equality and nothing else. "dialog open" is a catch-all, so a
/// dialog the product opens by itself reads the same, and every other value, or
/// none, is a `waiting` exactly as before.
pub const LIVE_WAITING_FOR_DIALOG_OPEN: &str = "dialog open";

/// Whether `entry` is a `waiting` session whose reason is exactly a dialog.
fn on_a_dialog(entry: &LiveSession) -> bool {
    entry.status.as_deref() == Some(LIVE_STATUS_WAITING)
        && entry.waiting_for.as_deref() == Some(LIVE_WAITING_FOR_DIALOG_OPEN)
}

/// Whether the session `session_id` is `waiting` only because a dialog is open
/// (`alive_pids` as in [`classify_from_alive_pids`]): it has a live process,
/// nothing busy, at least one `waiting` entry, and every `waiting` entry says
/// exactly [`LIVE_WAITING_FOR_DIALOG_OPEN`]. The list still calls such a session
/// Waiting ([`classify`]); this is what lets the bell tell it apart, and nothing
/// else.
pub fn waiting_on_a_dialog(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> bool {
    let mut waiting = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(session_id) || !alive_pids.contains(&entry.pid) {
            continue;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_BUSY) {
            return false;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_WAITING) {
            if !on_a_dialog(entry) {
                return false;
            }
            waiting = true;
        }
    }
    waiting
}

/// Whether the process a live-state file names is still the one that wrote it.
///
/// When the entry records `procStart`, the process must be alive **and** provably
/// the same one ([`ProcessProbe::is_alive_matching`]); without it, alive is all
/// there is to ask. The distinction matters for a leftover: Claude Code never
/// sweeps the `<pid>.json` of a process that died without getting to, and the
/// pid is later handed to an unrelated process, which a bare liveness check takes
/// for the session still running. One definition for a point query, shared by
/// the double-resume guard (`launch::is_live`) and the `brigade_status` reading;
/// [`SysinfoProbe::alive_live_pids`] decides the same thing for every entry at
/// once, and its answer is what the readers below take.
pub fn entry_is_alive(entry: &LiveSession, probe: &dyn ProcessProbe) -> bool {
    match &entry.proc_start {
        Some(proc_start) => probe.is_alive_matching(entry.pid, proc_start),
        None => probe.is_alive(entry.pid),
    }
}

/// Whether the relay may treat the Claude session `session_id` as idle enough to
/// nudge: `None` when no live entry names it, else `Some(true)` when at least one
/// entry naming it has a live process and none of those is busy or waiting.
///
/// Entries are not alternatives to pick between. Claude Code removes its own
/// `<pid>.json` on a normal exit but never sweeps the file of a process that died
/// without getting to, so a resumed session can be named by a dead leftover and
/// the live file together, in whatever order `read_dir` returned them. Three such
/// files from 2026-07-25 were still there four months later, and the one for the
/// Director's own session, listed first, read as a dead process: the relay never
/// nudged it and wrote nothing down. A dead entry therefore decides nothing, and
/// among the live ones busy or waiting anywhere wins, as in [`live_activity`]. An
/// entry whose process is dead is only ever the answer when it is the only kind
/// there is (`Some(false)`: a dead process is not idle).
///
/// `alive_pids` is [`SysinfoProbe::alive_live_pids`]'s answer, which leaves out a
/// leftover whose pid was recycled by an unrelated process: one saying `busy`
/// that stayed in would stop the relay for good.
///
/// A `waiting` session is not idle for the relay whatever it is waiting for,
/// `"dialog open"` included: a nudge is typed text and an Enter, and typed into
/// an open dialog its Enter would choose something, so its mail waits. This is
/// the only place the relay reads the raw status, so that rule cannot come apart
/// from the others. (What a dialog leaves unknown for the *quiet bell* is a
/// separate question, [`claude_quiescence`]'s.)
pub fn claude_nudgeable(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> Option<bool> {
    let mut named = false;
    let mut alive = false;
    for entry in live
        .iter()
        .filter(|entry| entry.session_id.as_deref() == Some(session_id))
    {
        named = true;
        if !alive_pids.contains(&entry.pid) {
            continue;
        }
        if matches!(
            entry.status.as_deref(),
            Some(LIVE_STATUS_BUSY) | Some(LIVE_STATUS_WAITING)
        ) {
            return Some(false);
        }
        alive = true;
    }
    named.then_some(alive)
}

/// Whether the Claude session `session_id` is known to be idle, from the live
/// files in `live` and the pids whose process is alive (`alive_pids` as in
/// [`classify_from_alive_pids`]).
///
/// `Idle` only when a live entry for the session has a live PID and every such
/// entry reports `"idle"` — the other values (`busy`, `waiting`, `shell`, an
/// absent or unrecognised one) are all not idle, because each means something
/// is running or waiting that ending the session would lose. No entry with a
/// live PID is `Unknown`: the caller asks about a pane whose process is
/// running, so an absent file is missing information, not idleness (a fresh
/// session publishes its file only a moment after it starts).
///
/// `shell` is `Background`, a kind of not idle that says why: a turn that has
/// ended with a background command still running. It is `Background` only when
/// that is the whole reason, so any entry that is busy, waiting, in a status
/// nothing here knows, or on a dialog beside it makes the answer `NotIdle`
/// (the dialog case is `NotIdle` as it always was, though a dialog alone is
/// `Unknown`): the extra word is for a session that is otherwise idle, and
/// must never soften a reading that is something else. The answer does not
/// depend on the order of the entries.
///
/// One exception, `waiting` with exactly [`LIVE_WAITING_FOR_DIALOG_OPEN`]: that
/// is `Unknown`, not `NotIdle` (unless another entry is busy or waiting for
/// something else, which wins). Read as not idle, a dialog the operator opens in
/// a member's pane and closes again would count as work that ended, arm a brigade
/// that had been quiet, and ring its quiet bell a settle after he closes it; and
/// `Unknown` is the honest value, since it is not known what status a dialog
/// opened over a running turn reports, so a dialog proves neither that the
/// session is idle nor that it is busy. Unknown still asks at quit and holds a
/// brigade's quiet stretch, so a brigade with a dialog left open cannot go quiet
/// until it is closed.
pub fn claude_quiescence(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> Quiescence {
    let mut seen = false;
    let mut dialog = false;
    let mut shell = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(session_id) || !alive_pids.contains(&entry.pid) {
            continue;
        }
        if on_a_dialog(entry) {
            dialog = true;
            continue;
        }
        match entry.status.as_deref() {
            Some(LIVE_STATUS_IDLE) => seen = true,
            Some(LIVE_STATUS_SHELL) => shell = true,
            _ => return Quiescence::NotIdle,
        }
    }
    // A background command beside a dialog is not told apart: the dialog is
    // what the session is stopped on, and the answer there was always `NotIdle`.
    if shell && dialog {
        Quiescence::NotIdle
    } else if shell {
        Quiescence::Background
    } else if dialog || !seen {
        Quiescence::Unknown
    } else {
        Quiescence::Idle
    }
}

/// Classify one session's activity from live state and file age.
///
/// A live entry matches `meta` when its `session_id` equals `meta.id.0`.
/// Precedence: a matching entry whose PID is alive with status `"busy"` wins
/// ([`Activity::Busy`]), then `"waiting"` ([`Activity::Waiting`]), then
/// `"shell"` (a background command still running, also [`Activity::Busy`]), then
/// any other matching entry with a live PID ([`Activity::Alive`]); otherwise the
/// session is [`Activity::Idle`] with an [`age_bucket`] computed from
/// `meta.mtime`.
pub fn classify(
    meta: &SessionMeta,
    live: &[LiveSession],
    probe: &dyn ProcessProbe,
    now: SystemTime,
    thresholds: &AgeThresholds,
) -> Activity {
    let alive_pids = live
        .iter()
        .filter(|entry| entry.session_id.as_deref() == Some(meta.id.0.as_str()))
        .filter(|entry| entry_is_alive(entry, probe))
        .map(|entry| entry.pid)
        .collect();
    classify_from_alive_pids(meta, live, &alive_pids, now, thresholds)
}

/// The one activity decision, supplied with the pids whose process is alive.
///
/// The set is taken rather than probed here so one list pass can obtain it
/// through a single batched system refresh: [`SysinfoProbe::alive_live_pids`],
/// which leaves out a leftover file whose pid was recycled by an unrelated
/// process, so one that says `busy` cannot make a session look busy.
/// [`classify`] is the point-query compatibility shim for existing callers and
/// mocks.
pub fn classify_from_alive_pids(
    meta: &SessionMeta,
    live: &[LiveSession],
    alive_pids: &HashSet<u32>,
    now: SystemTime,
    thresholds: &AgeThresholds,
) -> Activity {
    live_activity(live, &meta.id.0, alive_pids)
        .unwrap_or_else(|| Activity::Idle(age_bucket(meta.mtime, now, thresholds)))
}

/// What the live files say a session is doing now (`alive_pids` as in
/// [`classify_from_alive_pids`]), or `None` when no live entry with a running
/// process names it. The whole of the list's classification but
/// its fallback to the session file's age, and so the one decision for anything
/// that shows a session's state without a list row (a member that has not run its
/// first turn has no transcript, hence no row, but does have a live file).
///
/// Precedence: `"busy"` wins ([`Activity::Busy`]), then `"waiting"` (any reason,
/// [`Activity::Waiting`]; whether it is a dialog is [`waiting_on_a_dialog`]'s),
/// then `"shell"` ([`Activity::Busy`], see [`LIVE_STATUS_SHELL`]), then any other
/// entry with a live process ([`Activity::Alive`]).
pub fn live_activity(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> Option<Activity> {
    let mut any_alive = false;
    let mut any_waiting = false;
    let mut any_shell = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(session_id) {
            continue;
        }
        if !alive_pids.contains(&entry.pid) {
            continue;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_BUSY) {
            // Busy wins over Waiting: multiple live entries are anomalous, and
            // under-reporting a stale waiting entry preserves today's behavior
            // while over-reporting would train the operator to distrust it.
            return Some(Activity::Busy);
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_WAITING) {
            any_waiting = true;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_SHELL) {
            any_shell = true;
        }
        any_alive = true;
    }
    if any_waiting {
        // A person is needed, which outranks a command still running.
        Some(Activity::Waiting)
    } else if any_shell {
        Some(Activity::Busy)
    } else if any_alive {
        Some(Activity::Alive)
    } else {
        None
    }
}

/// Whether some live entry with a running process names `session_id` and says,
/// in so many words, `busy`: a turn is running. Neither [`live_activity`] nor
/// [`claude_nudgeable`] can answer that. The first reads `shell` (a background
/// command after the turn ended) as [`Activity::Busy`] too, and an Esc written
/// then would cut nothing; the second says only "not nudgeable", which `waiting`
/// is as well. The interrupt acts on this reading and no other, and so does the
/// confirmation that a Claude Goinkyo's first prompt started a turn (the other
/// confirmation, a first user message in its session row, is not a reading of
/// this file).
pub fn live_reports_busy(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> bool {
    live.iter().any(|entry| {
        entry.session_id.as_deref() == Some(session_id)
            && alive_pids.contains(&entry.pid)
            && entry.status.as_deref() == Some(LIVE_STATUS_BUSY)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;

    use banto_core::model::{AgeBucket, AgentKind, SessionId};

    use super::*;

    /// Mock probe reporting only an explicit set of PIDs as alive.
    struct MockProbe {
        alive: HashSet<u32>,
        /// Alive pids that now belong to an unrelated process: `is_alive_matching`
        /// is `false` for them whatever `proc_start` it is given.
        recycled: HashSet<u32>,
    }

    impl MockProbe {
        fn with_alive(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
                recycled: HashSet::new(),
            }
        }

        /// `alive` pids are all running; those also in `recycled` are running
        /// as somebody else.
        fn with_recycled(alive: &[u32], recycled: &[u32]) -> Self {
            Self {
                recycled: recycled.iter().copied().collect(),
                ..Self::with_alive(alive)
            }
        }
    }

    impl ProcessProbe for MockProbe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn parent_pid(&self, _pid: u32) -> Option<u32> {
            None
        }

        fn is_alive_matching(&self, pid: u32, _proc_start: &str) -> bool {
            self.is_alive(pid) && !self.recycled.contains(&pid)
        }
    }

    const SESSION_ID: &str = "00000000-0000-4000-8000-0000000000aa";

    fn fixed_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    /// Synthetic session metadata whose mtime is `age` before [`fixed_now`].
    fn meta_with_age(age: Duration) -> SessionMeta {
        SessionMeta {
            id: SessionId(SESSION_ID.to_string()),
            agent: AgentKind::ClaudeCode,
            title: Some("synthetic".to_string()),
            title_is_custom: false,
            cwd: None,
            source_path: PathBuf::from("synthetic.jsonl"),
            mtime: fixed_now() - age,
            size: 0,
            is_agent: false,
            preview: None,
            source_archived: false,
        }
    }

    fn live_entry(pid: u32, session_id: Option<&str>, status: Option<&str>) -> LiveSession {
        LiveSession {
            pid,
            session_id: session_id.map(str::to_string),
            cwd: None,
            status: status.map(str::to_string),
            waiting_for: None,
            kind: None,
            name: None,
            proc_start: None,
            version: None,
        }
    }

    fn classify_default(meta: &SessionMeta, live: &[LiveSession], probe: &MockProbe) -> Activity {
        classify(meta, live, probe, fixed_now(), &AgeThresholds::default())
    }

    #[test]
    fn matching_alive_busy_is_busy() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some("busy"))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Busy);
    }

    #[test]
    fn matching_alive_waiting_is_waiting() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some("waiting"))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Waiting);
    }

    #[test]
    fn matching_alive_non_busy_is_alive() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some("idle"))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Alive);
    }

    #[test]
    fn matching_alive_without_status_is_alive() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), None)];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Alive);
    }

    #[test]
    fn busy_wins_over_alive_regardless_of_order() {
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100, 200]);

        let idle_first = [
            live_entry(100, Some(SESSION_ID), Some("idle")),
            live_entry(200, Some(SESSION_ID), Some("busy")),
        ];
        assert_eq!(classify_default(&meta, &idle_first, &probe), Activity::Busy);

        let busy_first = [
            live_entry(200, Some(SESSION_ID), Some("busy")),
            live_entry(100, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(classify_default(&meta, &busy_first, &probe), Activity::Busy);
    }

    #[test]
    fn busy_wins_over_waiting_regardless_of_order() {
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100, 200]);

        let waiting_first = [
            live_entry(100, Some(SESSION_ID), Some("waiting")),
            live_entry(200, Some(SESSION_ID), Some("busy")),
        ];
        assert_eq!(
            classify_default(&meta, &waiting_first, &probe),
            Activity::Busy
        );

        let busy_first = [
            live_entry(200, Some(SESSION_ID), Some("busy")),
            live_entry(100, Some(SESSION_ID), Some("waiting")),
        ];
        assert_eq!(classify_default(&meta, &busy_first, &probe), Activity::Busy);
    }

    #[test]
    fn matching_alive_unknown_status_is_alive() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(
            100,
            Some(SESSION_ID),
            Some("upstream-future-state"),
        )];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Alive);
    }

    #[test]
    fn dead_pid_falls_through_to_age_bucket() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some("busy"))];
        let probe = MockProbe::with_alive(&[]); // PID 100 is dead
        assert_eq!(
            classify_default(&meta, &live, &probe),
            Activity::Idle(AgeBucket::Today)
        );
    }

    #[test]
    fn dead_waiting_pid_falls_through_to_age_bucket() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some("waiting"))];
        let probe = MockProbe::with_alive(&[]);
        assert_eq!(
            classify_default(&meta, &live, &probe),
            Activity::Idle(AgeBucket::Today)
        );
    }

    #[test]
    fn non_matching_session_id_falls_through_to_age_bucket() {
        let meta = meta_with_age(Duration::from_secs(2 * 24 * 60 * 60));
        let live = [live_entry(
            100,
            Some("00000000-0000-4000-8000-0000000000bb"),
            Some("busy"),
        )];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(
            classify_default(&meta, &live, &probe),
            Activity::Idle(AgeBucket::ThisWeek)
        );
    }

    #[test]
    fn entry_without_session_id_never_matches() {
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, None, Some("busy"))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(
            classify_default(&meta, &live, &probe),
            Activity::Idle(AgeBucket::Today)
        );
    }

    #[test]
    fn no_live_entries_buckets_by_age() {
        let probe = MockProbe::with_alive(&[]);

        let today = meta_with_age(Duration::from_secs(3600));
        assert_eq!(
            classify_default(&today, &[], &probe),
            Activity::Idle(AgeBucket::Today)
        );

        let this_week = meta_with_age(Duration::from_secs(3 * 24 * 60 * 60));
        assert_eq!(
            classify_default(&this_week, &[], &probe),
            Activity::Idle(AgeBucket::ThisWeek)
        );

        let older = meta_with_age(Duration::from_secs(30 * 24 * 60 * 60));
        assert_eq!(
            classify_default(&older, &[], &probe),
            Activity::Idle(AgeBucket::Older)
        );
    }

    #[test]
    fn dead_busy_and_alive_idle_mix_is_alive() {
        // A stale busy entry with a dead PID must not shadow a live idle one.
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [
            live_entry(100, Some(SESSION_ID), Some("busy")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        let probe = MockProbe::with_alive(&[200]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Alive);
    }

    // -- claude_quiescence ------------------------------------------------

    fn quiescence_of(live: &[LiveSession], alive: &[u32]) -> Quiescence {
        let alive: HashSet<u32> = alive.iter().copied().collect();
        claude_quiescence(live, SESSION_ID, &alive)
    }

    #[test]
    fn only_the_idle_report_of_a_live_session_is_idle() {
        let idle = [live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_IDLE))];
        assert_eq!(quiescence_of(&idle, &[100]), Quiescence::Idle);
    }

    #[test]
    fn every_other_status_of_a_live_session_is_not_idle() {
        for status in [
            Some(LIVE_STATUS_BUSY),
            Some(LIVE_STATUS_WAITING),
            Some("some-state-from-a-later-release"),
            Some(""),
            None,
        ] {
            let live = [live_entry(100, Some(SESSION_ID), status)];
            assert_eq!(
                quiescence_of(&live, &[100]),
                Quiescence::NotIdle,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn no_usable_live_entry_is_unknown_never_idle() {
        // No file at all, a file for another session, a file with no session
        // id, and a file whose process is gone: nothing was read about a
        // session whose pane is running.
        assert_eq!(quiescence_of(&[], &[]), Quiescence::Unknown);
        let other = [live_entry(100, Some("another-session"), Some("idle"))];
        assert_eq!(quiescence_of(&other, &[100]), Quiescence::Unknown);
        let anonymous = [live_entry(100, None, Some("idle"))];
        assert_eq!(quiescence_of(&anonymous, &[100]), Quiescence::Unknown);
        let dead = [live_entry(100, Some(SESSION_ID), Some("idle"))];
        assert_eq!(quiescence_of(&dead, &[]), Quiescence::Unknown);
    }

    #[test]
    fn a_stale_entry_with_a_dead_pid_cannot_make_a_live_one_look_idle_or_busy() {
        let live = [
            live_entry(100, Some(SESSION_ID), Some("busy")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(quiescence_of(&live, &[200]), Quiescence::Idle);
        assert_eq!(quiescence_of(&live, &[100, 200]), Quiescence::NotIdle);
    }

    #[test]
    fn shell_is_busy_in_the_list_and_background_for_quiescence() {
        // The turn has ended but a background command is still running: the list
        // still shows work in progress, and the quit question and the quiet watch
        // still count it ([`Quiescence::is_working`]) while being told why.
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Busy);
        assert_eq!(quiescence_of(&live, &[100]), Quiescence::Background);
        assert!(Quiescence::Background.is_working());
    }

    #[test]
    fn shell_is_background_only_when_it_is_the_whole_reason_the_session_is_not_idle() {
        // Beside an idle entry it is the only reason; beside anything that is
        // itself a reason to ask (busy, waiting for anything, a status nothing here
        // knows, none) it is just not idle, and a dialog stays not idle with it
        // exactly as before. A dead entry says nothing.
        let entry = |pid, status: Option<&str>, waiting_for: Option<&str>| LiveSession {
            waiting_for: waiting_for.map(str::to_string),
            ..live_entry(pid, Some(SESSION_ID), status)
        };
        let shell = entry(100, Some(LIVE_STATUS_SHELL), None);
        for (other, expected) in [
            (
                entry(200, Some(LIVE_STATUS_IDLE), None),
                Quiescence::Background,
            ),
            (
                entry(200, Some(LIVE_STATUS_SHELL), None),
                Quiescence::Background,
            ),
            (
                entry(200, Some(LIVE_STATUS_BUSY), None),
                Quiescence::NotIdle,
            ),
            (
                entry(200, Some(LIVE_STATUS_WAITING), None),
                Quiescence::NotIdle,
            ),
            (
                entry(200, Some(LIVE_STATUS_WAITING), Some("input needed")),
                Quiescence::NotIdle,
            ),
            (
                entry(
                    200,
                    Some(LIVE_STATUS_WAITING),
                    Some(LIVE_WAITING_FOR_DIALOG_OPEN),
                ),
                Quiescence::NotIdle,
            ),
            (
                entry(200, Some("a status nobody has seen"), None),
                Quiescence::NotIdle,
            ),
            (entry(200, None, None), Quiescence::NotIdle),
        ] {
            let status = other.status.clone();
            assert_eq!(
                quiescence_of(&[shell.clone(), other.clone()], &[100, 200]),
                expected,
                "shell beside {status:?}"
            );
            assert_eq!(
                quiescence_of(&[other, shell.clone()], &[100, 200]),
                expected,
                "{status:?} beside shell, the other way round"
            );
        }
        let dead = entry(300, Some(LIVE_STATUS_SHELL), None);
        assert_eq!(quiescence_of(&[dead], &[]), Quiescence::Unknown);
        let idle = entry(200, Some(LIVE_STATUS_IDLE), None);
        assert_eq!(
            quiescence_of(&[shell, idle], &[200]),
            Quiescence::Idle,
            "a dead shell entry beside a live idle one is idle"
        );
    }

    #[test]
    fn every_claude_status_has_one_display_word() {
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100]);
        for (status, expected) in [
            (Some("busy"), Activity::Busy),
            (Some("waiting"), Activity::Waiting),
            (Some("shell"), Activity::Busy),
            (Some("idle"), Activity::Alive),
            (Some("a status nobody has seen"), Activity::Alive),
            (Some(""), Activity::Alive),
            (None, Activity::Alive),
        ] {
            let live = [live_entry(100, Some(SESSION_ID), status)];
            assert_eq!(
                classify_default(&meta, &live, &probe),
                expected,
                "status {status:?}"
            );
        }
    }

    #[test]
    fn a_shell_entry_does_not_outrank_a_busy_or_waiting_one_for_the_same_session() {
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100, 200]);
        let shell = live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL));
        let waiting = live_entry(200, Some(SESSION_ID), Some("waiting"));
        let busy = live_entry(200, Some(SESSION_ID), Some("busy"));
        assert_eq!(
            classify_default(&meta, &[shell.clone(), waiting], &probe),
            Activity::Waiting,
            "a person is needed"
        );
        assert_eq!(
            classify_default(&meta, &[shell, busy], &probe),
            Activity::Busy
        );
        // A dead shell entry says nothing.
        let dead = live_entry(300, Some(SESSION_ID), Some(LIVE_STATUS_SHELL));
        assert_eq!(
            classify_default(&meta, &[dead], &probe),
            Activity::Idle(AgeBucket::Today)
        );
    }

    fn waiting_entry(pid: u32, waiting_for: Option<&str>) -> LiveSession {
        LiveSession {
            waiting_for: waiting_for.map(str::to_string),
            ..live_entry(pid, Some(SESSION_ID), Some("waiting"))
        }
    }

    #[test]
    fn a_waiting_session_is_waiting_in_the_list_whatever_its_reason_and_a_dialog_is_told_apart() {
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100]);
        for (reason, dialog) in [
            (None, false),
            (Some("input needed"), false),
            (Some("permission prompt"), false),
            (Some("a reason nobody has seen"), false),
            (Some(""), false),
            (Some("Dialog Open"), false),
            (Some("dialog open "), false),
            (Some("dialog open"), true),
        ] {
            let live = [waiting_entry(100, reason)];
            assert_eq!(
                classify_default(&meta, &live, &probe),
                Activity::Waiting,
                "{reason:?}: still Waiting in the list"
            );
            assert_eq!(
                waiting_on_a_dialog(&live, SESSION_ID, &[100].into_iter().collect()),
                dialog,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn only_a_waiting_session_with_a_live_process_and_nothing_else_to_say_is_on_a_dialog() {
        let alive: HashSet<u32> = [100, 200].into_iter().collect();
        let dialog = |live: &[LiveSession]| waiting_on_a_dialog(live, SESSION_ID, &alive);
        // The reason is only read beside `waiting`.
        for status in [Some("busy"), Some("idle"), Some("shell"), None] {
            let entry = LiveSession {
                waiting_for: Some("dialog open".to_string()),
                ..live_entry(100, Some(SESSION_ID), status)
            };
            assert!(!dialog(&[entry]), "{status:?}");
        }
        // A dead process, another session's entry, no entry.
        assert!(!waiting_on_a_dialog(
            &[waiting_entry(100, Some("dialog open"))],
            SESSION_ID,
            &HashSet::new()
        ));
        assert!(!waiting_on_a_dialog(
            &[waiting_entry(100, Some("dialog open"))],
            "another-session",
            &alive
        ));
        assert!(!dialog(&[]));
        // Two live entries for one session: one that is really waiting, or busy,
        // is the whole story.
        assert!(dialog(&[
            waiting_entry(100, Some("dialog open")),
            waiting_entry(200, Some("dialog open"))
        ]));
        assert!(!dialog(&[
            waiting_entry(100, Some("dialog open")),
            waiting_entry(200, Some("input needed"))
        ]));
        assert!(!dialog(&[
            waiting_entry(100, Some("dialog open")),
            live_entry(200, Some(SESSION_ID), Some("busy"))
        ]));
    }

    #[test]
    fn a_dialog_open_session_is_unknown_for_quiescence_and_every_other_waiting_is_not_idle() {
        for (reason, expected) in [
            (None, Quiescence::NotIdle),
            (Some("input needed"), Quiescence::NotIdle),
            (Some("a reason nobody has seen"), Quiescence::NotIdle),
            (Some("dialog open"), Quiescence::Unknown),
        ] {
            assert_eq!(
                quiescence_of(&[waiting_entry(100, reason)], &[100]),
                expected,
                "{reason:?}"
            );
        }
        // Whatever else is running wins over what a dialog leaves unknown.
        let busy_too = [
            waiting_entry(100, Some("dialog open")),
            live_entry(200, Some(SESSION_ID), Some("busy")),
        ];
        assert_eq!(quiescence_of(&busy_too, &[100, 200]), Quiescence::NotIdle);
        // And an idle entry beside a dialog does not make the session idle.
        let idle_too = [
            waiting_entry(100, Some("dialog open")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(quiescence_of(&idle_too, &[100, 200]), Quiescence::Unknown);
    }

    #[test]
    fn the_relay_never_sees_a_waiting_session_as_idle_whatever_it_is_waiting_for() {
        let alive: HashSet<u32> = [100].into_iter().collect();
        let nudgeable = |entry: LiveSession| claude_nudgeable(&[entry], SESSION_ID, &alive);
        for reason in [
            None,
            Some("input needed"),
            Some("permission prompt"),
            Some("a reason nobody has seen"),
            Some("dialog open"),
        ] {
            assert_eq!(
                nudgeable(waiting_entry(100, reason)),
                Some(false),
                "{reason:?}"
            );
        }
        assert_eq!(
            nudgeable(live_entry(100, Some(SESSION_ID), Some("busy"))),
            Some(false)
        );
        assert_eq!(
            nudgeable(live_entry(100, Some(SESSION_ID), Some("idle"))),
            Some(true)
        );
        assert_eq!(
            nudgeable(live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL))),
            Some(true),
            "shell is nudgeable, as before"
        );
        assert_eq!(
            claude_nudgeable(
                &[live_entry(300, Some(SESSION_ID), Some("idle"))],
                SESSION_ID,
                &alive
            ),
            Some(false),
            "a dead process is not idle"
        );
        assert_eq!(claude_nudgeable(&[], SESSION_ID, &alive), None);
    }

    fn nudgeable_of(live: &[LiveSession], alive: &[u32]) -> Option<bool> {
        let alive: HashSet<u32> = alive.iter().copied().collect();
        claude_nudgeable(live, SESSION_ID, &alive)
    }

    /// A live file as Claude Code writes it, with the start time it recorded.
    fn started(pid: u32, status: &str, proc_start: Option<&str>) -> LiveSession {
        LiveSession {
            proc_start: proc_start.map(str::to_string),
            ..live_entry(pid, Some(SESSION_ID), Some(status))
        }
    }

    /// The alive set built the way the real one is, with `probe` standing in for
    /// the operating system: an entry that recorded its start is settled by
    /// `is_alive_matching`, the rest go to the one batch of liveness.
    fn alive_set(live: &[LiveSession], probe: &MockProbe) -> HashSet<u32> {
        probe::alive_from_entries(
            live,
            |pid, start| Some(probe.is_alive_matching(pid, start)),
            |pids| {
                pids.into_iter()
                    .filter(|pid| probe.is_alive(*pid))
                    .collect()
            },
        )
    }

    #[test]
    fn an_entry_is_alive_when_its_process_is_the_one_that_wrote_it() {
        let probe = MockProbe::with_recycled(&[100, 200], &[200]);
        assert!(entry_is_alive(&started(100, "idle", Some("1")), &probe));
        assert!(
            !entry_is_alive(&started(200, "idle", Some("1")), &probe),
            "the pid is running, as somebody else"
        );
        assert!(!entry_is_alive(&started(300, "idle", Some("1")), &probe));
        // Without a recorded start only the pid can be asked, so a recycled one
        // cannot be told apart; the check is no weaker than it was.
        assert!(entry_is_alive(&started(200, "idle", None), &probe));
        assert!(!entry_is_alive(&started(300, "idle", None), &probe));
    }

    #[test]
    fn the_alive_set_leaves_out_a_recycled_pid_and_a_dead_one_and_keeps_the_rest() {
        let probe = MockProbe::with_recycled(&[100, 200, 400], &[100]);
        let live = [
            started(100, "busy", Some("1")),
            started(200, "idle", Some("2")),
            started(300, "idle", Some("3")),
            // No recorded start: counted by pid, recycled or not.
            started(400, "idle", None),
        ];
        let set = alive_set(&live, &probe);
        assert_eq!(set, [200, 400].into_iter().collect::<HashSet<u32>>());
    }

    #[test]
    fn a_non_idle_leftover_on_a_recycled_pid_is_not_what_any_reader_says_of_the_session() {
        let recycled = MockProbe::with_recycled(&[100, 200], &[100]);
        // The control: the same files with pid 100 the session's own process.
        let own = MockProbe::with_alive(&[100, 200]);
        let thresholds = AgeThresholds::default();
        let meta = meta_with_age(Duration::from_secs(60));
        for status in ["busy", "waiting", "shell"] {
            // What the session's file says when it is not its own process any
            // more, beside the file of the session as it runs (idle, pid 200).
            let mut stale = started(100, status, Some("1"));
            if status == "waiting" {
                stale.waiting_for = Some(LIVE_WAITING_FOR_DIALOG_OPEN.to_string());
            }
            let current = started(200, "idle", Some("2"));
            // `read_dir` promises no order.
            for live in [
                [stale.clone(), current.clone()],
                [current.clone(), stale.clone()],
            ] {
                let alive = alive_set(&live, &recycled);
                let held = alive_set(&live, &own);
                let ctx = format!("{status}, first file pid {}", live[0].pid);

                assert_eq!(
                    claude_nudgeable(&live, SESSION_ID, &alive),
                    Some(true),
                    "{ctx}"
                );
                assert_eq!(
                    claude_quiescence(&live, SESSION_ID, &alive),
                    Quiescence::Idle,
                    "{ctx}"
                );
                assert_eq!(
                    live_activity(&live, SESSION_ID, &alive),
                    Some(Activity::Alive),
                    "{ctx}"
                );
                assert!(!waiting_on_a_dialog(&live, SESSION_ID, &alive), "{ctx}");
                assert_eq!(
                    classify_from_alive_pids(&meta, &live, &alive, fixed_now(), &thresholds),
                    Activity::Alive,
                    "{ctx}"
                );

                // With the pid the session's own, every reader says what the
                // leftover says, so the answers above are the recycling's doing.
                let (nudge, quiet, activity) = match status {
                    "shell" => (Some(true), Quiescence::Background, Activity::Busy),
                    "busy" => (Some(false), Quiescence::NotIdle, Activity::Busy),
                    // A dialog proves neither idle nor busy (`claude_quiescence`).
                    _ => (Some(false), Quiescence::Unknown, Activity::Waiting),
                };
                assert_eq!(claude_nudgeable(&live, SESSION_ID, &held), nudge, "{ctx}");
                assert_eq!(claude_quiescence(&live, SESSION_ID, &held), quiet, "{ctx}");
                assert_eq!(
                    live_activity(&live, SESSION_ID, &held),
                    Some(activity),
                    "{ctx}"
                );
                assert_eq!(
                    waiting_on_a_dialog(&live, SESSION_ID, &held),
                    status == "waiting",
                    "{ctx}"
                );
                assert_eq!(
                    classify_from_alive_pids(&meta, &live, &held, fixed_now(), &thresholds),
                    activity,
                    "{ctx}"
                );
            }
        }
    }

    #[test]
    fn a_recycled_pid_alone_is_a_dead_session_and_a_dead_session_is_not_idle() {
        let probe = MockProbe::with_recycled(&[100], &[100]);
        let live = [started(100, "idle", Some("1"))];
        let alive = alive_set(&live, &probe);
        assert_eq!(claude_nudgeable(&live, SESSION_ID, &alive), Some(false));
        assert_eq!(
            claude_quiescence(&live, SESSION_ID, &alive),
            Quiescence::Unknown
        );
        assert_eq!(live_activity(&live, SESSION_ID, &alive), None);
    }

    #[test]
    fn a_dead_leftover_entry_listed_before_the_live_one_does_not_hide_it() {
        let live = [
            live_entry(100, Some(SESSION_ID), Some("idle")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(nudgeable_of(&live, &[200]), Some(true));
    }

    #[test]
    fn a_live_entry_listed_before_a_dead_leftover_is_still_nudgeable() {
        let live = [
            live_entry(200, Some(SESSION_ID), Some("idle")),
            live_entry(100, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(nudgeable_of(&live, &[200]), Some(true));
    }

    #[test]
    fn entries_that_are_all_dead_are_not_idle_but_are_not_unknown() {
        let live = [
            live_entry(100, Some(SESSION_ID), Some("idle")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(nudgeable_of(&live, &[]), Some(false));
        assert_eq!(nudgeable_of(&live[..1], &[]), Some(false));
    }

    #[test]
    fn a_busy_or_waiting_live_entry_wins_over_an_idle_one_in_either_order() {
        for busy_status in ["busy", "waiting"] {
            let busy_first = [
                live_entry(100, Some(SESSION_ID), Some(busy_status)),
                live_entry(200, Some(SESSION_ID), Some("idle")),
            ];
            let idle_first = [
                live_entry(200, Some(SESSION_ID), Some("idle")),
                live_entry(100, Some(SESSION_ID), Some(busy_status)),
            ];
            assert_eq!(nudgeable_of(&busy_first, &[100, 200]), Some(false));
            assert_eq!(nudgeable_of(&idle_first, &[100, 200]), Some(false));
        }
    }

    #[test]
    fn a_dead_busy_leftover_does_not_hold_a_live_idle_entry_back() {
        let live = [
            live_entry(100, Some(SESSION_ID), Some("busy")),
            live_entry(200, Some(SESSION_ID), Some("idle")),
        ];
        assert_eq!(nudgeable_of(&live, &[200]), Some(true));
    }

    #[test]
    fn entries_for_other_sessions_are_not_this_sessions_answer() {
        let live = [
            live_entry(100, Some("another-session"), Some("idle")),
            live_entry(200, None, Some("idle")),
        ];
        assert_eq!(nudgeable_of(&live, &[100, 200]), None);
        assert_eq!(nudgeable_of(&[], &[]), None);
    }

    #[test]
    fn what_the_live_files_say_of_a_session_is_the_lists_classification_without_the_age() {
        let alive: HashSet<u32> = [100, 200].into_iter().collect();
        let meta = meta_with_age(Duration::from_secs(60));
        let probe = MockProbe::with_alive(&[100, 200]);
        for (status, reason, expected) in [
            (Some("busy"), None, Some(Activity::Busy)),
            (Some("waiting"), None, Some(Activity::Waiting)),
            (
                Some("waiting"),
                Some("dialog open"),
                Some(Activity::Waiting),
            ),
            (
                Some("waiting"),
                Some("input needed"),
                Some(Activity::Waiting),
            ),
            (Some("shell"), None, Some(Activity::Busy)),
            (Some("idle"), None, Some(Activity::Alive)),
            (
                Some("a status nobody has seen"),
                None,
                Some(Activity::Alive),
            ),
            (None, None, Some(Activity::Alive)),
        ] {
            let live = [LiveSession {
                waiting_for: reason.map(str::to_string),
                ..live_entry(100, Some(SESSION_ID), status)
            }];
            assert_eq!(
                live_activity(&live, SESSION_ID, &alive),
                expected,
                "{status:?} {reason:?}"
            );
            // The one decision: the list's classification is this, or its age.
            assert_eq!(
                classify_default(&meta, &live, &probe),
                expected.unwrap(),
                "{status:?} {reason:?}"
            );
        }
        // Nothing live to say: no entry, another session's, a dead process.
        assert_eq!(live_activity(&[], SESSION_ID, &alive), None);
        let other = [live_entry(100, Some("another-session"), Some("busy"))];
        assert_eq!(live_activity(&other, SESSION_ID, &alive), None);
        let dead = [live_entry(300, Some(SESSION_ID), Some("busy"))];
        assert_eq!(live_activity(&dead, SESSION_ID, &alive), None);
        assert_eq!(
            classify_default(&meta, &dead, &probe),
            Activity::Idle(AgeBucket::Today),
            "where the list falls back to the file's age"
        );
    }

    #[test]
    fn only_a_literal_busy_from_a_live_process_is_a_running_turn() {
        let alive: HashSet<u32> = [100].into_iter().collect();
        let reports = |status: Option<&str>| {
            live_reports_busy(
                &[live_entry(100, Some(SESSION_ID), status)],
                SESSION_ID,
                &alive,
            )
        };
        assert!(reports(Some("busy")));
        // The statuses `live_activity` or the relay lump in with busy.
        for status in [
            Some("shell"),
            Some("waiting"),
            Some("idle"),
            Some("other"),
            None,
        ] {
            assert!(!reports(status), "{status:?}");
        }
        // Another session's entry, a dead process, no entry at all.
        let other = [live_entry(100, Some("another-session"), Some("busy"))];
        assert!(!live_reports_busy(&other, SESSION_ID, &alive));
        let dead = [live_entry(300, Some(SESSION_ID), Some("busy"))];
        assert!(!live_reports_busy(&dead, SESSION_ID, &alive));
        assert!(!live_reports_busy(&[], SESSION_ID, &alive));
        // One busy entry among others is enough, as in `live_activity`.
        let several = [
            live_entry(100, Some(SESSION_ID), Some("idle")),
            live_entry(100, Some(SESSION_ID), Some("busy")),
        ];
        assert!(live_reports_busy(&several, SESSION_ID, &alive));
    }

    #[test]
    fn live_activity_has_the_same_precedence_as_the_list_among_several_entries() {
        let alive: HashSet<u32> = [100, 200].into_iter().collect();
        let shell = live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL));
        let waiting = live_entry(200, Some(SESSION_ID), Some("waiting"));
        let busy = live_entry(200, Some(SESSION_ID), Some("busy"));
        let idle = live_entry(200, Some(SESSION_ID), Some("idle"));
        assert_eq!(
            live_activity(&[shell.clone(), waiting], SESSION_ID, &alive),
            Some(Activity::Waiting)
        );
        assert_eq!(
            live_activity(&[shell.clone(), busy], SESSION_ID, &alive),
            Some(Activity::Busy)
        );
        assert_eq!(
            live_activity(&[shell, idle], SESSION_ID, &alive),
            Some(Activity::Busy)
        );
    }
}
