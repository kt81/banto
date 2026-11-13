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
//! the degradation runs the other way: only the `idle` report is idle.
//!
//! PID liveness sits behind [`ProcessProbe`] so tests can mock it (no real
//! processes in tests). Bucketing itself lives in `banto_core::status`, a
//! pure function of (mtime, now, thresholds).

mod live;
mod probe;

pub use live::{LiveSession, read_live_sessions};
pub use probe::{ProcessProbe, SysinfoProbe, ancestry_reaches};

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
/// goes `"idle"`. The list shows it as [`Activity::Busy`], which is what the quit
/// question and the brigade watch already counted it as (not idle: a command is
/// running that ending the session would lose), so what is drawn and what is
/// decided say the same. The relay is the one reader that does NOT follow: it
/// still counts `shell` as idle enough to nudge ([`claude_nudgeable`]), since the
/// turn has ended and the member can take a message. The two readings are
/// separate on purpose — display here in [`classify_from_alive_pids`], the relay
/// in [`claude_nudgeable`] — so that changing a word on screen cannot change who
/// gets nudged.
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

/// Whether the session `session_id` is `waiting` only because a dialog is open:
/// it has a live process, nothing busy, at least one `waiting` entry, and every
/// `waiting` entry says exactly [`LIVE_WAITING_FOR_DIALOG_OPEN`]. The list still
/// calls such a session Waiting ([`classify`]); this is what lets the bell tell it
/// apart, and nothing else.
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

/// Whether the relay may treat the Claude session `session_id` as idle enough to
/// nudge: `None` when no live entry names it, else whether its first entry has a
/// live process and is neither busy nor waiting.
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
    live.iter()
        .find(|entry| entry.session_id.as_deref() == Some(session_id))
        .map(|entry| {
            alive_pids.contains(&entry.pid)
                && !matches!(
                    entry.status.as_deref(),
                    Some(LIVE_STATUS_BUSY) | Some(LIVE_STATUS_WAITING)
                )
        })
}

/// Whether the Claude session `session_id` is known to be idle, from the live
/// files in `live` and the set of PIDs that are alive.
///
/// `Idle` only when a live entry for the session has a live PID and every such
/// entry reports `"idle"` — the other values (`busy`, `waiting`, `shell`, an
/// absent or unrecognised one) are all `NotIdle`, because each means something
/// is running or waiting that ending the session would lose. No entry with a
/// live PID is `Unknown`: the caller asks about a pane whose process is
/// running, so an absent file is missing information, not idleness (a fresh
/// session publishes its file only a moment after it starts).
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
    for entry in live {
        if entry.session_id.as_deref() != Some(session_id) || !alive_pids.contains(&entry.pid) {
            continue;
        }
        if on_a_dialog(entry) {
            dialog = true;
            continue;
        }
        if entry.status.as_deref() != Some(LIVE_STATUS_IDLE) {
            return Quiescence::NotIdle;
        }
        seen = true;
    }
    if dialog || !seen {
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
        .filter(|entry| probe.is_alive(entry.pid))
        .map(|entry| entry.pid)
        .collect();
    classify_from_alive_pids(meta, live, &alive_pids, now, thresholds)
}

/// The one activity decision, supplied with a snapshot of alive PIDs.
///
/// The set is taken rather than probed here so one list pass can obtain it
/// through a single batched system refresh.  [`classify`] is the point-query
/// compatibility shim for existing callers and mocks.
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

/// What the live files say a session is doing now, or `None` when no live entry
/// with a running process names it. The whole of the list's classification but
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
    }

    impl MockProbe {
        fn with_alive(pids: &[u32]) -> Self {
            Self {
                alive: pids.iter().copied().collect(),
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
            self.is_alive(pid)
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
            cwd: None,
            source_path: PathBuf::from("synthetic.jsonl"),
            mtime: fixed_now() - age,
            size: 0,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
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
            Some(LIVE_STATUS_SHELL),
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
    fn shell_is_busy_in_the_list_and_not_idle_for_quiescence() {
        // The turn has ended but a background command is still running: what the
        // list shows is what the quit question already counts, work in progress.
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Busy);
        assert_eq!(quiescence_of(&live, &[100]), Quiescence::NotIdle);
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
