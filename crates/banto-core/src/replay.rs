//! Record/replay: the event-stream fixture format `docs/DISCIPLINE.md` §8
//! calls for — a recorded sequence of [`engine::Event`]s (with timestamps),
//! replayed through [`engine::update`] to reproduce a session's `State`/
//! `Cmd` history deterministically. The recorder that *writes* this format
//! lives in the `banto` bin crate (`BANTO_RECORD_EVENTS`, a shell/io
//! concern, `docs/DISCIPLINE.md` §6.2's diagnostic-bypass relaxation); this
//! module is the pure half — the format, parsing it, and driving it through
//! `update`.
//!
//! # Format
//!
//! JSONL (one JSON value per line). The first line is a version header:
//! ```text
//! {"banto_event_stream": 1}
//! ```
//! Every line after that is one recorded event, `offset_ms` milliseconds
//! after the recording began (never a wall-clock timestamp — replay
//! fabricates its own base `Instant` and adds each offset to it, so a
//! stream never claims a real moment in time, only a relative ordering)
//! paired with the [`Event`] observed at that moment, serialized via
//! `Event`'s own `Serialize`/`Deserialize` derive in serde_json's default
//! externally-tagged shape:
//! ```text
//! {"offset_ms": 1234, "event": {"Input": {"Key": {"code": {"Char": "a"}, "modifiers": {"ctrl": false, "alt": false, "shift": false}}}}}
//! ```
//!
//! An unrecognized `banto_event_stream` version is rejected outright, not
//! best-effort parsed: per §8, "a replay format change is an explicit
//! migration, not silent breakage." Likewise a malformed event line is a
//! hard, named error carrying its line number — unlike the *leniently*
//! parsed upstream formats elsewhere in banto (session jsonl,
//! `sessions/<pid>.json`), a stream file is banto's own artifact, and a
//! fixture that silently drops a line it can't parse is a fixture that lies
//! about what it tested.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::engine::{self, Cmd, EmporiumState, Event, PrefixKey};

/// The only stream format version this build understands.
pub const STREAM_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct StreamHeader {
    banto_event_stream: u32,
}

/// One recorded event: `offset_ms` milliseconds after the recording began,
/// paired with the [`Event`] observed at that moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedEvent {
    pub offset_ms: u64,
    pub event: Event,
}

/// Why a stream failed to parse. Deliberately not `anyhow`-wrapped —
/// `banto-core` stays free of I/O-flavored dependencies (`docs/
/// DISCIPLINE.md` §2); a caller that wants richer context wraps this
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The input had no lines at all — not even a header.
    MissingHeader,
    /// The first line did not parse as `{"banto_event_stream": <version>}`.
    MalformedHeader(String),
    /// The header parsed, but named a version this build doesn't know how
    /// to replay.
    UnknownVersion(u32),
    /// Line `line` (1-based, counting the header as line 1) did not parse
    /// as `{"offset_ms": <u64>, "event": <Event>}`.
    MalformedEvent { line: usize, message: String },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::MissingHeader => write!(f, "empty stream: missing the version header"),
            ReplayError::MalformedHeader(message) => {
                write!(f, "malformed stream header: {message}")
            }
            ReplayError::UnknownVersion(version) => write!(
                f,
                "unknown stream version {version} (this build understands {STREAM_VERSION})"
            ),
            ReplayError::MalformedEvent { line, message } => {
                write!(f, "malformed event on line {line}: {message}")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// Parse a `docs/DISCIPLINE.md` §8 event stream — see the module doc for
/// the format. Strict: an unknown version or a line that fails to parse is
/// a named error, never silently skipped.
pub fn parse_stream(input: &str) -> Result<Vec<TimedEvent>, ReplayError> {
    let mut lines = input.lines();
    let header_line = lines.next().ok_or(ReplayError::MissingHeader)?;
    let header: StreamHeader = serde_json::from_str(header_line)
        .map_err(|err| ReplayError::MalformedHeader(err.to_string()))?;
    if header.banto_event_stream != STREAM_VERSION {
        return Err(ReplayError::UnknownVersion(header.banto_event_stream));
    }

    let mut events = Vec::new();
    for (index, line) in lines.enumerate() {
        let line_number = index + 2; // 1-based; the header already claimed line 1.
        let timed: TimedEvent =
            serde_json::from_str(line).map_err(|err| ReplayError::MalformedEvent {
                line: line_number,
                message: err.to_string(),
            })?;
        events.push(timed);
    }
    Ok(events)
}

/// What driving a recorded stream through [`update`](engine::update)
/// produced.
pub struct ReplayOutcome {
    pub state: EmporiumState,
    pub app: App,
    /// Every `Cmd` `update` returned, tagged with the `offset_ms` of the
    /// event that produced it — not the event's own index, since one event
    /// can produce zero, one, or several `Cmd`s.
    pub cmds: Vec<(u64, Cmd)>,
}

/// Drive `events` through [`engine::update`], from a fresh `EmporiumState`
/// (the default prefix chord — replay never depends on a particular
/// `keys.prefix`) and `App::new(vec![])` (a fixture supplies rows itself,
/// via its own `Event::RowsLoaded`).
///
/// `base` is the synthetic `Instant` that `offset_ms == 0` maps to, supplied
/// by the caller rather than read here: `banto-core` must never call
/// `Instant::now()` itself (`docs/DISCIPLINE.md` §3's "no clock access" is
/// unconditional — a "just this once, for a synthetic anchor" reading of it
/// is exactly the kind of judgment call §0 rules out). A caller typically
/// just passes `Instant::now()`. Every duration-based behavior in `update`
/// (the relay's debounce/cooldown, the prefix-arm timeout, status expiry)
/// then replays deterministically, because `now` is always
/// `base + Duration::from_millis(offset_ms)`, never wall-clock time.
pub fn replay(events: &[TimedEvent], brigade: &BrigadeConfig, base: Instant) -> ReplayOutcome {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let mut cmds = Vec::new();
    for timed in events {
        let now = base + Duration::from_millis(timed.offset_ms);
        let produced = engine::update(&mut state, &mut app, brigade, timed.event.clone(), now);
        cmds.extend(produced.into_iter().map(|cmd| (timed.offset_ms, cmd)));
    }
    ReplayOutcome { state, app, cmds }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Focus, Stage};
    use crate::input::{InputEvent, KeyCode, KeyEvent, Modifiers};

    /// See `app::tests::test_instant`'s doc for why this exists and why it's
    /// not the clock access DISCIPLINE.md §3 forbids.
    #[allow(clippy::disallowed_methods)]
    fn test_instant() -> Instant {
        Instant::now()
    }

    fn header_line() -> String {
        format!("{{\"banto_event_stream\":{STREAM_VERSION}}}")
    }

    fn stream(events: &[TimedEvent]) -> String {
        let mut lines = vec![header_line()];
        for event in events {
            lines.push(serde_json::to_string(event).unwrap());
        }
        lines.join("\n")
    }

    // --- parse_stream: format/error handling ------------------------------

    #[test]
    fn round_trips_a_few_events_through_serialize_and_parse() {
        let events = vec![
            TimedEvent {
                offset_ms: 0,
                event: Event::Input(InputEvent::Key(KeyEvent::new(
                    KeyCode::Enter,
                    Modifiers::NONE,
                ))),
            },
            TimedEvent {
                offset_ms: 42,
                event: Event::Resized {
                    width: 80,
                    height: 24,
                },
            },
            TimedEvent {
                offset_ms: 100,
                event: Event::Tick { relay: vec![] },
            },
        ];
        let text = stream(&events);
        let parsed = parse_stream(&text).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn an_empty_input_is_a_missing_header_error() {
        assert_eq!(parse_stream(""), Err(ReplayError::MissingHeader));
    }

    #[test]
    fn a_header_that_is_not_json_is_a_malformed_header_error() {
        let err = parse_stream("not json").unwrap_err();
        assert!(
            matches!(err, ReplayError::MalformedHeader(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn an_unrecognized_version_is_rejected() {
        let text = "{\"banto_event_stream\":999}";
        assert_eq!(parse_stream(text), Err(ReplayError::UnknownVersion(999)));
    }

    #[test]
    fn a_malformed_event_line_carries_its_line_number() {
        let text = format!(
            "{}\n{{\"offset_ms\":0,\"event\":{{\"NotARealVariant\":{{}}}}}}",
            header_line()
        );
        let err = parse_stream(&text).unwrap_err();
        assert!(
            matches!(err, ReplayError::MalformedEvent { line: 2, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_later_malformed_line_reports_its_own_line_number() {
        let good = TimedEvent {
            offset_ms: 0,
            event: Event::Resized {
                width: 1,
                height: 1,
            },
        };
        let text = format!(
            "{}\n{}\n{{not json}}",
            header_line(),
            serde_json::to_string(&good).unwrap()
        );
        let err = parse_stream(&text).unwrap_err();
        assert!(
            matches!(err, ReplayError::MalformedEvent { line: 3, .. }),
            "got {err:?}"
        );
    }

    // --- replay: deterministic time -----------------------------------

    #[test]
    fn a_tick_driven_status_expiry_lands_exactly_where_the_offset_says() {
        let brigade = BrigadeConfig::default();
        let base = test_instant();
        let archived = |offset_ms| TimedEvent {
            offset_ms,
            event: Event::ArchiveDone {
                title: "demo".to_string(),
                result: Ok(()),
            },
        };
        let tick = |offset_ms| TimedEvent {
            offset_ms,
            event: Event::Tick { relay: vec![] },
        };

        // STATUS_TIMEOUT is exactly 5000ms: 1ms short of it, the status
        // must still be there.
        let just_before = replay(&[archived(0), tick(4999)], &brigade, base);
        assert_eq!(just_before.state.status.as_deref(), Some("archived demo"));

        // At exactly 5000ms, it must be gone.
        let at_timeout = replay(&[archived(0), tick(5000)], &brigade, base);
        assert_eq!(at_timeout.state.status, None);
    }

    // --- the canonical-flow fixture ---------------------------------------
    //
    // A hand-authored recording of the ordinary "click a session, it spawns,
    // it later exits" flow, walking `parse_stream` and `replay` together end
    // to end. One synthetic row (never real session data — repo invariant
    // 2): Enter activates it, membership resolves to "not a brigade member"
    // (a plain solo open), the shell answers that it spawned, then that it
    // exited. Every JSON line here is the real `Event`/`TimedEvent`
    // `Serialize` output for the equivalent Rust values (generated once,
    // then frozen as text) — this is what a real `BANTO_RECORD_EVENTS`
    // capture of the same flow looks like.
    const CANONICAL_FIXTURE: &str = concat!(
        "{\"banto_event_stream\":1}\n",
        "{\"offset_ms\":0,\"event\":{\"RowsLoaded\":{\"rows\":[{\"id\":\"row-1\",\"agent\":\"ClaudeCode\",\"title\":\"Demo Session\",\"cwd\":\"/tmp/demo\",\"activity\":{\"Idle\":\"Today\"},\"is_agent\":false,\"preview\":null,\"mtime\":{\"secs_since_epoch\":1700000000,\"nanos_since_epoch\":0},\"size\":1234}],\"hidden\":[],\"directors\":[]}}}\n",
        "{\"offset_ms\":100,\"event\":{\"Input\":{\"Key\":{\"code\":\"Enter\",\"modifiers\":{\"ctrl\":false,\"alt\":false,\"shift\":false}}}}}\n",
        "{\"offset_ms\":200,\"event\":{\"MembershipResolved\":{\"session_id\":\"row-1\",\"membership\":null,\"members\":null}}}\n",
        "{\"offset_ms\":300,\"event\":{\"Spawned\":{\"key\":\"row-1\"}}}\n",
        "{\"offset_ms\":400,\"event\":{\"PtyExited\":{\"key\":\"row-1\"}}}\n",
    );

    #[test]
    fn canonical_flow_parses_and_replays_the_activate_spawn_exit_sequence() {
        let events = parse_stream(CANONICAL_FIXTURE).expect("the fixture is well-formed");
        assert_eq!(events.len(), 5);
        let expected_key = engine::SessionKey::from_id("row-1");
        let brigade = BrigadeConfig::default();
        let base = test_instant();

        // Through `MembershipResolved` (offset 200) and `Spawned` (offset
        // 300): the row wasn't a brigade member, so `MembershipResolved`
        // opens it solo, and `Spawned` stages that pane and focuses it.
        let mid = replay(&events[..4], &brigade, base);
        let open_embedded: Vec<_> = mid
            .cmds
            .iter()
            .filter(|(_, cmd)| matches!(cmd, Cmd::OpenEmbedded { .. }))
            .collect();
        assert_eq!(open_embedded.len(), 1);
        assert_eq!(open_embedded[0].0, 200, "tagged with its producing offset");
        let Cmd::OpenEmbedded {
            key,
            target,
            brigade: cmd_brigade,
            model,
            effort,
        } = &open_embedded[0].1
        else {
            unreachable!("just matched OpenEmbedded above");
        };
        assert_eq!(key, &expected_key);
        assert_eq!(target.id, "row-1");
        assert!(cmd_brigade.is_none());
        assert!(model.is_none());
        assert!(effort.is_none());
        assert!(
            matches!(&mid.state.stage, Stage::Solo(k) if k == &expected_key),
            "expected Solo({expected_key:?}), got {:?}",
            mid.state.stage
        );
        assert_eq!(mid.state.focus, Focus::Pane);

        // The full sequence, `PtyExited` (offset 400) included: the pane
        // collapses back to Empty and the session-ended status lands.
        let outcome = replay(&events, &brigade, base);
        assert!(
            matches!(outcome.state.stage, Stage::Empty),
            "expected Empty, got {:?}",
            outcome.state.stage
        );
        assert_eq!(
            outcome.state.status.as_deref(),
            Some("session ended: Demo Session")
        );
    }

    #[test]
    fn canonical_fixtures_own_pty_exited_line_has_no_reason_field_and_that_is_the_point() {
        // `PtyExited`'s `reason` field (added after `CANONICAL_FIXTURE` was
        // frozen) is `#[serde(default)]` exactly so this fixture's own
        // pre-existing JSON line — "{"PtyExited":{"key":"row-1"}}", no
        // `reason` key at all — keeps parsing as a stream recorded before
        // the field existed, deserializing to `reason: None`. The test just
        // above already exercises this end to end (the fixture parses, and
        // its final status is the short, reason-less wording); this pins
        // the same fact at the JSON layer directly, so a future edit that
        // makes the field non-optional fails here with a clear message
        // instead of a confusing status-string mismatch three functions
        // away.
        let events = parse_stream(CANONICAL_FIXTURE).expect("the fixture is well-formed");
        let exited = events
            .iter()
            .find(|timed| matches!(timed.event, Event::PtyExited { .. }))
            .expect("the fixture has a PtyExited line");
        assert!(matches!(
            &exited.event,
            Event::PtyExited { reason: None, .. }
        ));
    }

    #[test]
    fn a_freshly_recorded_pty_exited_with_a_reason_round_trips_and_replays_into_the_richer_status()
    {
        let events = vec![
            TimedEvent {
                offset_ms: 0,
                event: Event::RowsLoaded {
                    rows: vec![],
                    hidden: Default::default(),
                    directors: Default::default(),
                    superseded: Default::default(),
                },
            },
            TimedEvent {
                offset_ms: 100,
                event: Event::PtyExited {
                    key: engine::SessionKey::from_id("row-1"),
                    reason: Some(crate::model::PtyExitReason::Code(1)),
                },
            },
        ];
        let text = stream(&events);
        let parsed = parse_stream(&text).expect("a freshly serialized stream must parse");
        assert_eq!(parsed, events);

        let brigade = BrigadeConfig::default();
        let outcome = replay(&parsed, &brigade, test_instant());
        assert_eq!(
            outcome.state.status.as_deref(),
            Some("session ended: row-1 — exited with code 1")
        );
    }

    // --- the director-fork fixture -----------------------------------------
    //
    // Same authoring discipline as `CANONICAL_FIXTURE` above, scoped to the
    // one rename-following path that a real recording can actually produce:
    // a brigade forms with zero Workers, the Director's own pane spawns,
    // then its Claude session auto-compaction-forks in place
    // (`Event::MemberSessionForked`) — pinning that `Stage::Brigade`'s
    // `director` field follows the rename, not just `panes`.
    // `update_discovery_result`'s own version of this same follow (see its
    // doc) has no equivalent here: it only fires for a key
    // `SessionKey::is_synthetic()` still calls true, and the Director's key
    // is never synthetic in a real recording (`BrigadeFormed`'s
    // `director_row_id` always names an already-known session) — a fixture
    // built to exercise it would have to fake a shape the shell can't
    // actually produce, unlike this one. It stays pinned as the direct
    // `engine.rs` unit test
    // `discovery_result_on_the_directors_own_key_renames_the_director_field`
    // instead.
    const DIRECTOR_FORK_FIXTURE: &str = concat!(
        "{\"banto_event_stream\":1}\n",
        "{\"offset_ms\":0,\"event\":{\"RowsLoaded\":{\"rows\":[{\"id\":\"dir-old\",\"agent\":\"ClaudeCode\",\"title\":\"Demo Director\",\"cwd\":\"/tmp/demo\",\"activity\":{\"Idle\":\"Today\"},\"is_agent\":false,\"preview\":null,\"mtime\":{\"secs_since_epoch\":1700000000,\"nanos_since_epoch\":0},\"size\":1234,\"source_archived\":false}],\"hidden\":[],\"directors\":[],\"superseded\":[]}}}\n",
        "{\"offset_ms\":100,\"event\":{\"BrigadeFormed\":{\"director_row_id\":\"dir-old\",\"name\":\"cell\",\"cwd\":\"/tmp/demo\",\"worker_agent\":\"ClaudeCode\",\"worker_model\":\"\",\"result\":{\"Ok\":[1,[]]}}}}\n",
        "{\"offset_ms\":200,\"event\":{\"Spawned\":{\"key\":\"dir-old\"}}}\n",
        "{\"offset_ms\":300,\"event\":{\"MemberSessionForked\":{\"brigade_id\":1,\"token\":\"director\",\"old_id\":\"dir-old\",\"new_id\":\"dir-new\"}}}\n",
    );

    #[test]
    fn director_fork_fixture_parses_and_replays_the_form_spawn_fork_sequence() {
        let events = parse_stream(DIRECTOR_FORK_FIXTURE).expect("the fixture is well-formed");
        assert_eq!(events.len(), 4);
        let brigade = BrigadeConfig::default();
        let base = test_instant();

        // Through `Spawned` (offset 200): the brigade stages with the
        // Director's original key, alone.
        let mid = replay(&events[..3], &brigade, base);
        let old_key = engine::SessionKey::from_id("dir-old");
        assert!(
            matches!(
                &mid.state.stage,
                Stage::Brigade { director, panes, .. }
                    if director.as_ref() == Some(&old_key) && panes == std::slice::from_ref(&old_key)
            ),
            "expected a staged brigade directed by dir-old, got {:?}",
            mid.state.stage
        );

        // The full sequence, `MemberSessionForked` (offset 300) included:
        // both `director` and `panes` follow the rename.
        let outcome = replay(&events, &brigade, base);
        let new_key = engine::SessionKey::from_id("dir-new");
        assert!(
            matches!(
                &outcome.state.stage,
                Stage::Brigade { director, panes, .. }
                    if director.as_ref() == Some(&new_key) && panes == std::slice::from_ref(&new_key)
            ),
            "expected the director rename to follow into both fields, got {:?}",
            outcome.state.stage
        );
    }
}
