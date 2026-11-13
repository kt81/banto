//! What a hosted program says about its own state: the OSC 7501 report.
//!
//! The format is the Program Status Protocol
//! (<https://www.superlogical.com/rex/docs/build/program-status>), a published
//! protocol and not Codex's own:
//!
//! ```text
//! ESC ] 7501 ; key=value:key=value… ST        (ST is ESC \, or BEL)
//! ```
//!
//! at most 4096 bytes a sequence. `state` (`idle`, `working`, `done`, `blocked`,
//! `error`, `clear`) and `kind` (with `blocked`) are read; `id`, which names a
//! sub-record, is parsed and left for the caller to drop, since only the root
//! record (no `id`) is a pane's own state; every other key is ignored. The `?`
//! form is a program asking what the terminal supports, not a report, and banto
//! does not answer it. A payload the parser may have cut short, in which no state
//! survived, is [`StateReport::Cut`].
//!
//! What the mapping to idle and not idle relies on: `done` and `error` are what
//! Codex leaves after a turn, and it reverts them to `idle` only when the operator
//! interacts, so a pane can sit in either for good with nothing running; `working`
//! covers background work left running after a turn, which the rollout does not
//! show. Where that comes from (Codex's source, one measurement on Windows) and
//! what the report is not worth is in docs/REQUIREMENTS.md, "Codex data sources",
//! "The pane's own report (OSC 7501)".
//!
//! No scanner of its own: the pane's `vt100` parser already finds OSC sequences
//! across chunk boundaries and hands every one it does not implement to
//! `Callbacks::unhandled_osc`, so [`crate::screen::Screen`] only gives this module
//! the parameters of the one it cares about.

use crate::model::Quiescence;

/// The longest payload read at all: the protocol's limit for a sequence. A
/// longer one is discarded whole.
const MAX_PAYLOAD: usize = 4096;

/// How many OSC parameters the `vt100` layer's parser (vte 0.15.0,
/// `MAX_OSC_PARAMS`, private there) hands over: the number and fifteen more. It
/// drops the rest without a word, so a payload whose `;` run past that arrives
/// without its tail, indistinguishable from one that was that short. The text
/// after a payload's fifteenth `;` is what is lost. A conforming report has no
/// `;` outside `id` and unknown keys (the protocol makes `title` and `msg` base64
/// and restricts `app`), so this bites only on ones that are not. A test through
/// the real parser pins the number, so an upgrade that moves it shows up there.
const OSC_PARAM_CAP: usize = 16;

/// What an OSC 7501 report names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportedState {
    Idle,
    Working,
    /// An approval, question or verification prompt is on screen; the kind is
    /// `None` when the report names none this build knows.
    Blocked(Option<BlockedKind>),
    Done,
    Error,
}

/// Why a program reports itself blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    Permission,
    Question,
    Auth,
}

/// What one report says: a state, or the program saying the record has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateReport {
    State(ReportedState),
    Clear,
    /// The parser's parameter limit may have cut the payload and no usable state
    /// was in what arrived. A report that may have been cut could have been the
    /// one that ended a `working` or `blocked` state, and keeping the old state
    /// would hold the quit prompt and the bell on stale information, so the
    /// reader closes to the previous rule when unsure: this has the effect of
    /// `Clear`, and is told apart from it only because the program said no such
    /// thing.
    Cut,
}

/// One parsed report: which record it is about and what it says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The sub-record's path (`a/b`), or `None` for the root record — the pane's
    /// own state. An empty `id=` is not the root: it names a record this build
    /// does not keep.
    pub id: Option<String>,
    pub report: StateReport,
}

impl ReportedState {
    /// Whether ending the program now would lose nothing: `idle`, `done` and
    /// `error` are the after-turn states, in which nothing is running however long
    /// the pane stays in them; `working` (a turn, background terminals, an active
    /// goal) and `blocked` (waiting on the operator) are not.
    pub fn quiescence(self) -> Quiescence {
        match self {
            Self::Idle | Self::Done | Self::Error => Quiescence::Idle,
            Self::Working | Self::Blocked(_) => Quiescence::NotIdle,
        }
    }

    /// The program is waiting on the operator.
    pub fn is_blocked(self) -> bool {
        matches!(self, Self::Blocked(_))
    }
}

/// Read an OSC whose parameters (split on `;` by the parser, number first) are
/// `params`. `None` for any other sequence and for a payload that is not a state
/// report: the `?` feature-detection form, no `state=` field, an unknown state
/// value, text that is not UTF-8, one over the length limit. `app` is not
/// required, and a key this build does not know is ignored wherever it stands.
///
/// The one exception is a payload that arrived at the parser's parameter cap
/// ([`OSC_PARAM_CAP`]), so may be only part of what was sent, in which no usable
/// `state` was found: that is [`StateReport::Cut`], not "not a report". A `?`
/// query is never that. A `state` found in what arrived is applied as usual.
pub fn parse_osc(params: &[&[u8]]) -> Option<Report> {
    let (number, rest) = params.split_first()?;
    if *number != b"7501" || rest.is_empty() {
        return None;
    }
    // The parser splits on `;` too, and a value (an `id`, an unknown key) may hold
    // one: the payload is every parameter after the number, joined back.
    let payload = rest.join(&b';');
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let payload = std::str::from_utf8(&payload).ok()?;
    let mut state = None;
    let mut kind = None;
    let mut id = None;
    for field in payload.split(':') {
        match field.split_once('=') {
            Some(("state", value)) => state = Some(value),
            Some(("kind", value)) => kind = Some(value),
            Some(("id", value)) => id = Some(value.to_string()),
            _ => {}
        }
    }
    let kind = kind.and_then(|value| match value {
        "permission" => Some(BlockedKind::Permission),
        "question" => Some(BlockedKind::Question),
        "auth" => Some(BlockedKind::Auth),
        _ => None,
    });
    let report = match state {
        Some("idle") => StateReport::State(ReportedState::Idle),
        Some("working") => StateReport::State(ReportedState::Working),
        Some("blocked") => StateReport::State(ReportedState::Blocked(kind)),
        Some("done") => StateReport::State(ReportedState::Done),
        Some("error") => StateReport::State(ReportedState::Error),
        Some("clear") => StateReport::Clear,
        // No usable state: a payload that stopped at the parser's cap may have
        // lost it; a query, or a payload that was whole, simply is not a report.
        _ if params.len() >= OSC_PARAM_CAP && !payload.starts_with('?') => StateReport::Cut,
        _ => return None,
    };
    Some(Report { id, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(payload: &str) -> Option<Report> {
        parse_osc(&[b"7501", payload.as_bytes()])
    }

    /// The root record's report, for the tests that do not care about `id`.
    fn root(payload: &str) -> Option<StateReport> {
        let report = parse(payload)?;
        assert_eq!(report.id, None, "{payload}");
        Some(report.report)
    }

    #[test]
    fn every_state_is_read() {
        for (text, state) in [
            ("state=idle:app=codex", ReportedState::Idle),
            ("state=working:app=codex", ReportedState::Working),
            ("state=done:app=codex", ReportedState::Done),
            ("state=error:app=codex", ReportedState::Error),
            ("state=blocked:app=codex", ReportedState::Blocked(None)),
        ] {
            assert_eq!(root(text), Some(StateReport::State(state)), "{text}");
        }
    }

    #[test]
    fn blocked_carries_its_kind() {
        for (kind, expected) in [
            ("permission", Some(BlockedKind::Permission)),
            ("question", Some(BlockedKind::Question)),
            ("auth", Some(BlockedKind::Auth)),
            ("something-new", None),
        ] {
            assert_eq!(
                root(&format!("state=blocked:app=codex:kind={kind}")),
                Some(StateReport::State(ReportedState::Blocked(expected))),
                "{kind}"
            );
        }
    }

    #[test]
    fn fields_may_come_in_any_order_and_unknown_ones_are_ignored() {
        let blocked = Some(StateReport::State(ReportedState::Blocked(Some(
            BlockedKind::Question,
        ))));
        assert_eq!(root("kind=question:app=codex:state=blocked"), blocked);
        assert_eq!(root("app=codex:kind=question:state=blocked"), blocked);
        assert_eq!(
            root("state=blocked:kind=question:future=thing:app=codex:extra"),
            blocked
        );
        // Unknown keys first, last and between; the keys the protocol defines
        // but this build does not use are ignored the same way.
        assert_eq!(root("future=1:state=blocked:kind=question"), blocked);
        assert_eq!(root("state=blocked:kind=question:future=1"), blocked);
        assert_eq!(
            root("progress=40:title=Build:msg=aGVsbG8=:state=blocked:kind=question"),
            blocked
        );
        assert_eq!(
            root("state=working"),
            Some(StateReport::State(ReportedState::Working)),
            "no app field is fine"
        );
    }

    #[test]
    fn a_value_holding_the_separator_comes_back_whole() {
        // The parser splits the sequence on `;`; the payload is put back as it
        // was, so a title with one does not break the fields after it and an id
        // with one is the id that was sent.
        assert_eq!(
            parse_osc(&[b"7501", b"title=a", b"b:state=working"]),
            Some(Report {
                id: None,
                report: StateReport::State(ReportedState::Working)
            })
        );
        assert_eq!(
            parse_osc(&[b"7501", b"id=a", b"b:state=idle"]).and_then(|r| r.id),
            Some("a;b".to_string())
        );
    }

    #[test]
    fn a_state_given_twice_is_the_last_one() {
        // The protocol does not say what a repeated key means. Last wins is
        // what a map built from the pairs would give; nothing sends this.
        assert_eq!(
            root("state=idle:state=working"),
            Some(StateReport::State(ReportedState::Working))
        );
        assert_eq!(root("state=working:state=clear"), Some(StateReport::Clear));
        // An unusable later value does not fall back to the earlier one.
        assert_eq!(root("state=working:state=sleeping"), None);
    }

    #[test]
    fn clear_says_the_program_has_no_state_any_more() {
        assert_eq!(root("state=clear"), Some(StateReport::Clear));
        assert_eq!(root("state=clear:app=codex"), Some(StateReport::Clear));
    }

    #[test]
    fn a_report_names_the_record_it_is_about() {
        assert_eq!(
            parse("state=working:id=build/tests"),
            Some(Report {
                id: Some("build/tests".into()),
                report: StateReport::State(ReportedState::Working)
            })
        );
        assert_eq!(
            parse("id=build:state=clear"),
            Some(Report {
                id: Some("build".into()),
                report: StateReport::Clear
            })
        );
        // Not the root: an empty id names a record of its own.
        assert_eq!(
            parse("state=idle:id=").map(|r| r.id),
            Some(Some(String::new()))
        );
    }

    #[test]
    fn a_payload_at_the_parameter_cap_with_no_state_is_cut_and_only_then() {
        let at_cap: Vec<&[u8]> = std::iter::once(b"7501".as_slice())
            .chain(std::iter::repeat_n(b"x=1".as_slice(), OSC_PARAM_CAP - 1))
            .collect();
        let cut = |id: Option<&str>| {
            Some(Report {
                id: id.map(str::to_string),
                report: StateReport::Cut,
            })
        };
        assert_eq!(parse_osc(&at_cap), cut(None));
        // One fewer parameter lost nothing: just not a report.
        assert_eq!(parse_osc(&at_cap[..OSC_PARAM_CAP - 1]), None);
        // The record it names, if that arrived, is the one it is about.
        let mut named = at_cap.clone();
        named[1] = b"id=a/b:y=1";
        assert_eq!(parse_osc(&named), cut(Some("a/b")));
        // A state in what arrived is applied, an unusable one is not, a query is
        // never a cut.
        let mut stated = at_cap.clone();
        stated[1] = b"state=working:y=1";
        assert_eq!(
            parse_osc(&stated),
            Some(Report {
                id: None,
                report: StateReport::State(ReportedState::Working)
            })
        );
        stated[1] = b"state=sleeping:y=1";
        assert_eq!(parse_osc(&stated), cut(None));
        let mut query = at_cap.clone();
        query[1] = b"?";
        assert_eq!(parse_osc(&query), None);
    }

    #[test]
    fn what_is_not_a_state_report_is_ignored() {
        // The feature-detection form (bare and with its lists), a payload with
        // no state, an unknown state, an empty one, one that is not text.
        for payload in [
            "?",
            "?:states=idle,working,blocked:kinds=permission,question",
            "app=codex",
            "state=sleeping",
            "state=",
            "",
            "kind=question",
        ] {
            assert_eq!(parse(payload), None, "{payload:?}");
        }
        assert_eq!(parse_osc(&[b"7501", b"state=\xff\xfe"]), None);
    }

    #[test]
    fn other_sequences_and_shapes_are_ignored() {
        assert_eq!(parse_osc(&[b"7502", b"state=idle"]), None);
        assert_eq!(parse_osc(&[b"7501"]), None);
        assert_eq!(parse_osc(&[]), None);
    }

    #[test]
    fn a_payload_over_the_limit_is_not_read() {
        let padded = |len: usize| format!("state=idle:{}", "x".repeat(len - "state=idle:".len()));
        let at_limit = padded(MAX_PAYLOAD);
        assert_eq!(at_limit.len(), 4096);
        assert_eq!(
            root(&at_limit),
            Some(StateReport::State(ReportedState::Idle))
        );
        assert_eq!(parse(&padded(MAX_PAYLOAD + 1)), None);
        // The limit counts the whole payload, separators the parser took out
        // included.
        let split = [
            b"7501".as_slice(),
            b"state=idle:x",
            &[b'y'; MAX_PAYLOAD - 13],
        ];
        assert!(parse_osc(&split).is_some());
        let split = [
            b"7501".as_slice(),
            b"state=idle:x",
            &[b'y'; MAX_PAYLOAD - 12],
        ];
        assert_eq!(parse_osc(&split), None);
    }

    #[test]
    fn the_after_turn_states_are_idle_and_the_rest_are_not() {
        use Quiescence::{Idle, NotIdle};
        assert_eq!(ReportedState::Idle.quiescence(), Idle);
        assert_eq!(ReportedState::Done.quiescence(), Idle);
        assert_eq!(ReportedState::Error.quiescence(), Idle);
        assert_eq!(ReportedState::Working.quiescence(), NotIdle);
        assert_eq!(ReportedState::Blocked(None).quiescence(), NotIdle);
        assert!(ReportedState::Blocked(Some(BlockedKind::Auth)).is_blocked());
        assert!(!ReportedState::Working.is_blocked());
    }
}
