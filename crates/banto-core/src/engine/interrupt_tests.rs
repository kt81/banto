//! `interrupt_worker` as the tick decides it: which word each request is given, that
//! deciding writes nothing into any pane, and that arming a request restarts the
//! nudge clock of the Worker it concerns. Driven through real `Event::Tick`s, as
//! the shell sends them.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::{BrigadeConfig, RelayMode};
use crate::model::{Activity, AgentKind, InterruptOutcome, InterruptWhy};
use crate::screen::Screen;

use super::*;

const SECOND: Duration = Duration::from_secs(1);
const BRIGADE: BrigadeId = 1;
/// Wide enough for the nudge sentence to fit on one row: a longer line wraps
/// onto the block's bottom rule, and the reader then finds no input box.
const COLS: u16 = 200;
const WORKER: &str = "worker-1";

#[allow(clippy::disallowed_methods)]
fn t0() -> Instant {
    Instant::now()
}

fn key(id: &str) -> SessionKey {
    SessionKey::from_id(id)
}

struct Rig {
    state: EmporiumState,
    app: App,
    config: BrigadeConfig,
    worker: SessionKey,
    at: Instant,
}

/// A Director and a Worker hosted in brigade 1, no screens drawn yet.
fn rig() -> Rig {
    let mut state = EmporiumState::new(PrefixKey::default());
    host(&mut state, DIRECTOR_TOKEN, "dir");
    let worker = host(&mut state, WORKER, "w1");
    Rig {
        state,
        app: App::new(Vec::new()),
        config: BrigadeConfig::default(),
        worker,
        at: t0(),
    }
}

fn host(state: &mut EmporiumState, token: &str, id: &str) -> SessionKey {
    let pane = key(id);
    state.screens.insert(pane.clone(), Screen::new(24, COLS));
    state.spawn_order.insert(pane.clone(), state.next_spawn);
    state.next_spawn += 1;
    state.member_tokens.insert(pane.clone(), token.to_string());
    state.member_brigades.insert(pane.clone(), BRIGADE);
    pane
}

fn running_turn() -> Option<LiveStatus> {
    Some(LiveStatus {
        activity: Activity::Busy,
        dialog_open: false,
        status_busy: true,
    })
}

fn live(activity: Activity) -> Option<LiveStatus> {
    Some(LiveStatus {
        activity,
        dialog_open: false,
        status_busy: false,
    })
}

fn claude(rig: &Rig, live: Option<LiveStatus>) -> RelayObservation {
    RelayObservation {
        live,
        glance: None,
        brigade_id: BRIGADE,
        token: WORKER.to_string(),
        key: rig.worker.clone(),
        has_unseen: true,
        is_idle_this_tick: Some(false),
        agent: Some(AgentKind::ClaudeCode),
        briefed_since_launch: None,
    }
}

fn codex(rig: &Rig) -> RelayObservation {
    RelayObservation {
        agent: Some(AgentKind::Codex),
        ..claude(rig, running_turn())
    }
}

fn request(message_id: i64) -> InterruptFact {
    InterruptFact {
        message_id,
        member: WORKER.to_string(),
        read: false,
    }
}

/// One tick a second after the last, as the shell sends it. `interrupts` is what
/// the store lists as undecided that second.
fn tick(rig: &mut Rig, relay: Vec<RelayObservation>, interrupts: Vec<InterruptFact>) -> Vec<Cmd> {
    tick_reading(rig, relay, Some(interrupts))
}

/// A tick whose store read of the requests may have failed (`None`).
fn tick_reading(
    rig: &mut Rig,
    relay: Vec<RelayObservation>,
    interrupts: Option<Vec<InterruptFact>>,
) -> Vec<Cmd> {
    rig.at += SECOND;
    let brigades = vec![BrigadeNameFacts {
        brigade_id: BRIGADE,
        name: "cell".to_string(),
        director_session: None,
        member_names: Vec::new(),
        request: None,
        interrupts,
    }];
    update(
        &mut rig.state,
        &mut rig.app,
        &rig.config,
        Event::Tick { relay, brigades },
        rig.at,
    )
}

/// Every outcome the tick recorded, in order: message, outcome, folded reason.
fn recorded(cmds: &[Cmd]) -> Vec<(i64, InterruptOutcome, Option<InterruptWhy>)> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::Store(StoreIntent::RecordInterruptOutcome {
                message_id,
                outcome,
                why,
                ..
            }) => Some((*message_id, *outcome, *why)),
            _ => None,
        })
        .collect()
}

fn typed_bytes(cmds: &[Cmd], pane: &SessionKey) -> Vec<Vec<u8>> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::WritePty { key, bytes } if key == pane => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

fn status(rig: &Rig) -> String {
    rig.state.status.clone().unwrap_or_default()
}

// --- what a request earns -------------------------------------------------------------------

#[test]
fn a_claude_worker_under_an_automatic_relay_is_armed_once_and_nothing_is_typed() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert_eq!(
        recorded(&cmds),
        [(7, InterruptOutcome::Armed, None)],
        "{cmds:?}"
    );
    assert!(
        typed_bytes(&cmds, &rig.worker).is_empty(),
        "an interrupt writes nothing into the pane: {cmds:?}"
    );
    // Not "refused": nothing has been, and a request seen again later may be past
    // the point where anything is.
    assert!(status(&rig).contains("an interrupt"), "{}", status(&rig));
    assert!(!status(&rig).contains("refused"), "{}", status(&rig));

    // The store still lists the request (the write has not landed, or failed):
    // it is not decided a second time.
    let cmds = tick(&mut rig, vec![obs], vec![request(7)]);
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");
}

#[test]
fn a_store_read_that_fails_does_not_make_the_run_forget_what_it_decided() {
    // Request 7 is decided and its outcome write fails, so the store still lists it
    // as undecided. The next tick cannot read the store at all: that is not an
    // empty list, and must not drop the run's memory of 7. The tick after reads 7
    // again, still undecided: deciding it a second time would be the bug.
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert_eq!(recorded(&cmds).len(), 1, "{cmds:?}");

    let cmds = tick_reading(&mut rig, vec![obs.clone()], None);
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");

    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");

    // An empty list that *was* read still forgets it: the outcome is recorded, and
    // the id may go. (A later request has a new id, so this loses nothing.)
    tick(&mut rig, vec![obs], Vec::new());
    assert!(rig.state.interrupt_decided.is_empty());
}

#[test]
fn a_request_is_decided_however_old_it_is() {
    // The age of a request is not a fact the core is given: what it arms ends when
    // the Worker reads the message, so a request found after a restart is armed
    // like any other.
    let mut rig = rig();
    let obs = claude(&rig, live(Activity::Alive));
    let cmds = tick(&mut rig, vec![obs], vec![request(1)]);
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Armed, None)]);
}

#[test]
fn a_pane_that_cannot_be_read_yet_is_no_pane() {
    use InterruptOutcome::NoPane;
    // A member whose product is not resolved is a pane still starting: it is not
    // known to carry a gate, and "armed" would be a claim about a pane banto
    // cannot read.
    let mut rig = rig();
    let obs = RelayObservation {
        agent: None,
        ..claude(&rig, running_turn())
    };
    let cmds = tick(&mut rig, vec![obs], vec![request(4)]);
    assert_eq!(recorded(&cmds), [(4, NoPane, Some(InterruptWhy::Starting))]);
}

#[test]
fn overlapping_facts_are_decided_in_the_documented_order() {
    use InterruptOutcome::*;
    // read beats everything, including a missing pane.
    let mut rig = rig();
    let read = InterruptFact {
        read: true,
        ..request(1)
    };
    let cmds = tick(&mut rig, Vec::new(), vec![read]);
    assert_eq!(recorded(&cmds), [(1, Read, None)]);

    // A brigade being closed beats a manual relay.
    let mut rig = self::rig();
    rig.config.relay = RelayMode::Manual;
    rig.state.closing.insert(BRIGADE, rig.at);
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(2)]);
    assert_eq!(recorded(&cmds), [(2, NoPane, Some(InterruptWhy::Closing))]);

    // no-pane (a hang-up queued) beats relay-manual, which beats codex.
    let mut rig = self::rig();
    rig.config.relay = RelayMode::Manual;
    let codex = codex(&rig);
    let cmds = tick(&mut rig, vec![codex.clone()], vec![request(3)]);
    assert_eq!(recorded(&cmds), [(3, RelayManual, None)]);
    rig.state.hangups.insert(rig.worker.clone(), None);
    let cmds = tick(&mut rig, vec![codex.clone()], vec![request(4)]);
    assert_eq!(recorded(&cmds), [(4, NoPane, Some(InterruptWhy::Ending))]);
    rig.config.relay = RelayMode::Auto;
    rig.state.hangups.clear();
    let cmds = tick(&mut rig, vec![codex], vec![request(6)]);
    assert_eq!(recorded(&cmds), [(6, Codex, None)]);
}

#[test]
fn a_message_already_read_a_codex_worker_and_manual_relay_are_recorded_and_not_armed() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let read = InterruptFact {
        read: true,
        ..request(1)
    };
    let cmds = tick(&mut rig, vec![obs], vec![read]);
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Read, None)]);

    let mut rig = self::rig();
    let obs = codex(&rig);
    let cmds = tick(&mut rig, vec![obs], vec![request(2)]);
    assert_eq!(recorded(&cmds), [(2, InterruptOutcome::Codex, None)]);

    let mut rig = self::rig();
    rig.config.relay = RelayMode::Manual;
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(3)]);
    assert_eq!(recorded(&cmds), [(3, InterruptOutcome::RelayManual, None)]);

    // None of these says anything to the operator: only an armed request does.
    assert!(!status(&rig).contains("refused"), "{}", status(&rig));
}

#[test]
fn a_member_with_no_live_pane_is_answered_with_the_reason() {
    use InterruptOutcome::NoPane;
    use InterruptWhy::*;

    // Nothing hosts it.
    let mut rig = rig();
    rig.state.member_tokens.remove(&rig.worker);
    rig.state.member_brigades.remove(&rig.worker);
    let cmds = tick(&mut rig, Vec::new(), vec![request(1)]);
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(NotRunning))]);

    // A pane is open for it and has no session yet, so there is no observation.
    let mut rig = self::rig();
    let cmds = tick(&mut rig, Vec::new(), vec![request(1)]);
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(Starting))]);

    // Its pane is being ended.
    let mut rig = self::rig();
    rig.state.hangups.insert(rig.worker.clone(), Some(rig.at));
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1)]);
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(Ending))]);

    // Its brigade is being closed.
    let mut rig = self::rig();
    rig.state.closing.insert(BRIGADE, rig.at);
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1)]);
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(Closing))]);
}

#[test]
fn two_requests_for_one_pane_in_one_tick_are_both_answered() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1), request(2)]);
    assert_eq!(
        recorded(&cmds),
        [
            (1, InterruptOutcome::Armed, None),
            (2, InterruptOutcome::Armed, None)
        ]
    );
    assert!(typed_bytes(&cmds, &rig.worker).is_empty(), "{cmds:?}");
}

// --- what arming does to the nudge ----------------------------------------------------------

#[test]
fn arming_a_request_restarts_the_nudge_clock() {
    let mut rig = rig();
    let member = (BRIGADE, WORKER.to_string());
    rig.state.relay_states.insert(
        member.clone(),
        RelayState {
            idle_streak: 5,
            nudge: NudgeState {
                last_nudge: Some(rig.at),
                attempts: RELAY_MAX_ATTEMPTS,
            },
        },
    );
    let obs = claude(&rig, running_turn());
    tick(&mut rig, vec![obs], vec![request(1)]);
    let after = rig.state.relay_states[&member];
    assert_eq!(after.nudge.attempts, 0);
    assert!(after.nudge.last_nudge.is_none());
}

#[test]
fn a_request_that_is_not_armed_leaves_the_nudge_clock_alone() {
    let mut rig = rig();
    rig.config.relay = RelayMode::Manual;
    let member = (BRIGADE, WORKER.to_string());
    let before = RelayState {
        idle_streak: 5,
        nudge: NudgeState {
            last_nudge: Some(rig.at),
            attempts: RELAY_MAX_ATTEMPTS,
        },
    };
    rig.state.relay_states.insert(member.clone(), before);
    let obs = claude(&rig, running_turn());
    tick(&mut rig, vec![obs], vec![request(1)]);
    let after = rig.state.relay_states[&member];
    assert_eq!(after.idle_streak, before.idle_streak);
    assert_eq!(after.nudge.attempts, RELAY_MAX_ATTEMPTS);
    assert_eq!(after.nudge.last_nudge, before.nudge.last_nudge);
}

/// The empty prompt block of a Claude pane.
fn empty_prompt() -> String {
    let rule = "\u{2500}".repeat(COLS as usize);
    format!(
        "\x1b[2J\x1b[19;1H{rule}\x1b[20;1H\u{276f}\u{a0}\x1b[2mTry something\x1b[0m\x1b[21;1H{rule}\
         \x1b[22;1H  ? for shortcuts\x1b[20;3H"
    )
}

fn draw(rig: &mut Rig, bytes: &str) {
    rig.state
        .screens
        .get_mut(&rig.worker)
        .expect("the worker is hosted")
        .process(bytes.as_bytes());
}

fn at_rest(rig: &Rig) -> RelayObservation {
    RelayObservation {
        is_idle_this_tick: Some(true),
        ..claude(rig, live(Activity::Alive))
    }
}

/// Ticks of a Worker at rest with unread mail until something is written to it,
/// at most `limit`.
fn writes_within(rig: &mut Rig, limit: u32) -> Vec<Vec<u8>> {
    for _ in 0..limit {
        let obs = at_rest(rig);
        let cmds = tick(rig, vec![obs], Vec::new());
        let written = typed_bytes(&cmds, &rig.worker);
        if !written.is_empty() {
            return written;
        }
    }
    Vec::new()
}

#[test]
fn a_worker_whose_turn_ends_without_a_tool_call_is_woken_without_waiting_out_the_cooldown() {
    // A Worker that calls no tool is never refused anything, so the nudge is what
    // reaches it. One sent just before the Director's message was written must not
    // hold the next one back.
    let mut rig = rig();
    draw(&mut rig, &empty_prompt());
    let member = (BRIGADE, WORKER.to_string());
    rig.state.relay_states.insert(
        member,
        RelayState {
            idle_streak: 0,
            nudge: NudgeState {
                last_nudge: Some(rig.at),
                attempts: 1,
            },
        },
    );
    let busy = claude(&rig, running_turn());
    tick(&mut rig, vec![busy], vec![request(1)]);
    let written = writes_within(&mut rig, 6);
    assert_eq!(written, [RELAY_NUDGE_LINE.as_bytes().to_vec()]);
}
