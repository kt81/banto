//! Live-session state and activity classification — the I/O half of status
//! (`banto_core::status` keeps the pure bucketing math: `AgeThresholds` /
//! `age_bucket`).
//!
//! Sources, in priority order (docs/REQUIREMENTS.md, "Activity indicator"):
//! 1. `<claude_home>/sessions/<pid>.json` + PID alive + status=busy -> Busy
//! 2. `<claude_home>/sessions/<pid>.json` + PID alive + status=waiting -> Waiting
//! 3. PID alive, any other or unknown status -> Alive
//! 4. otherwise bucket the session file mtime into Today / ThisWeek / Older
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
/// goes `"idle"`. The list keeps drawing it as [`Activity::Alive`] — the
/// catch-all it already fell into — and the relay still counts it as idle
/// enough to nudge, since it is neither busy nor waiting for the operator; only
/// [`claude_quiescence`] tells it apart from `"idle"`.
pub const LIVE_STATUS_BUSY: &str = "busy";
pub const LIVE_STATUS_WAITING: &str = "waiting";
pub const LIVE_STATUS_IDLE: &str = "idle";
pub const LIVE_STATUS_SHELL: &str = "shell";

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
pub fn claude_quiescence(
    live: &[LiveSession],
    session_id: &str,
    alive_pids: &HashSet<u32>,
) -> Quiescence {
    let mut seen = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(session_id) || !alive_pids.contains(&entry.pid) {
            continue;
        }
        if entry.status.as_deref() != Some(LIVE_STATUS_IDLE) {
            return Quiescence::NotIdle;
        }
        seen = true;
    }
    if seen {
        Quiescence::Idle
    } else {
        Quiescence::Unknown
    }
}

/// Classify one session's activity from live state and file age.
///
/// A live entry matches `meta` when its `session_id` equals `meta.id.0`.
/// Precedence: a matching entry whose PID is alive with status `"busy"` wins
/// ([`Activity::Busy`]), then `"waiting"` ([`Activity::Waiting`]), then any
/// other matching entry with a live PID ([`Activity::Alive`]); otherwise the
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
    let mut any_alive = false;
    let mut any_waiting = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(meta.id.0.as_str()) {
            continue;
        }
        if !alive_pids.contains(&entry.pid) {
            continue;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_BUSY) {
            // Busy wins over Waiting: multiple live entries are anomalous, and
            // under-reporting a stale waiting entry preserves today's behavior
            // while over-reporting would train the operator to distrust it.
            return Activity::Busy;
        }
        if entry.status.as_deref() == Some(LIVE_STATUS_WAITING) {
            any_waiting = true;
        }
        any_alive = true;
    }
    if any_waiting {
        Activity::Waiting
    } else if any_alive {
        Activity::Alive
    } else {
        Activity::Idle(age_bucket(meta.mtime, now, thresholds))
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
    fn shell_is_alive_in_the_list_and_not_idle_for_quiescence() {
        // What the session list draws for a `shell` session is what it drew
        // for any status it does not name: the catch-all `Alive`.
        let meta = meta_with_age(Duration::from_secs(60));
        let live = [live_entry(100, Some(SESSION_ID), Some(LIVE_STATUS_SHELL))];
        let probe = MockProbe::with_alive(&[100]);
        assert_eq!(classify_default(&meta, &live, &probe), Activity::Alive);
        assert_eq!(quiescence_of(&live, &[100]), Quiescence::NotIdle);
    }
}
