//! Live-session state and activity classification — the I/O half of status
//! (`banto_core::status` keeps the pure bucketing math: `AgeThresholds` /
//! `age_bucket`).
//!
//! Sources, in priority order (docs/REQUIREMENTS.md, "Activity indicator"):
//! 1. `<claude_home>/sessions/<pid>.json` + PID alive + status=busy -> Busy
//! 2. PID alive, not busy -> Alive
//! 3. otherwise bucket the session file mtime into Today / ThisWeek / Older
//!
//! PID liveness sits behind [`ProcessProbe`] so tests can mock it (no real
//! processes in tests). Bucketing itself lives in `banto_core::status`, a
//! pure function of (mtime, now, thresholds).

mod live;
mod probe;

pub use live::{LiveSession, read_live_sessions};
pub use probe::{ProcessProbe, SysinfoProbe, ancestry_reaches};

use std::time::SystemTime;

use banto_core::model::{Activity, SessionMeta};
use banto_core::status::{AgeThresholds, age_bucket};

/// Classify one session's activity from live state and file age.
///
/// A live entry matches `meta` when its `session_id` equals `meta.id.0`.
/// Precedence: a matching entry whose PID is alive with status `"busy"` wins
/// ([`Activity::Busy`]), then any other matching entry with a live PID
/// ([`Activity::Alive`]); otherwise the session is [`Activity::Idle`] with an
/// [`age_bucket`] computed from `meta.mtime`.
pub fn classify(
    meta: &SessionMeta,
    live: &[LiveSession],
    probe: &dyn ProcessProbe,
    now: SystemTime,
    thresholds: &AgeThresholds,
) -> Activity {
    let mut any_alive = false;
    for entry in live {
        if entry.session_id.as_deref() != Some(meta.id.0.as_str()) {
            continue;
        }
        if !probe.is_alive(entry.pid) {
            continue;
        }
        if entry.status.as_deref() == Some("busy") {
            // Busy wins over Alive even when multiple entries match.
            return Activity::Busy;
        }
        any_alive = true;
    }
    if any_alive {
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

    use banto_core::model::{AgeBucket, SessionId};

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
            provider: "claude-code".to_string(),
            title: Some("synthetic".to_string()),
            cwd: None,
            source_path: PathBuf::from("synthetic.jsonl"),
            mtime: fixed_now() - age,
            size: 0,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
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
