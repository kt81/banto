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
//! PID liveness sits behind [`ProcessProbe`] so tests can mock it (no real
//! processes in tests). Bucketing itself lives in `banto_core::status`, a
//! pure function of (mtime, now, thresholds).

mod live;
mod probe;

pub use live::{LiveSession, read_live_sessions};
pub use probe::{ProcessProbe, SysinfoProbe, ancestry_reaches};

use std::collections::HashSet;
use std::time::SystemTime;

use banto_core::model::{Activity, SessionMeta};
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
pub const LIVE_STATUS_BUSY: &str = "busy";
pub const LIVE_STATUS_WAITING: &str = "waiting";

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
}
