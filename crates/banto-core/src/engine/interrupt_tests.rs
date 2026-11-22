//! `interrupt_worker` as the tick decides it: which requests earn an Esc, what is
//! recorded for the rest, how a press is read afterwards, and the Enter-only repair
//! that delivers the nudge an Esc put back on the input line. Driven through real
//! `Event::Tick`s, as the shell sends them.

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

fn request(message_id: i64) -> InterruptFact {
    InterruptFact {
        message_id,
        member: WORKER.to_string(),
        age_ms: 400,
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

fn presses(cmds: &[Cmd], pane: &SessionKey) -> usize {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::InterruptPty { key } if key == pane))
        .count()
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
fn a_running_turn_is_pressed_once_and_the_request_is_not_decided_again() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert_eq!(presses(&cmds, &rig.worker), 1, "{cmds:?}");
    assert_eq!(
        recorded(&cmds),
        [(7, InterruptOutcome::Pressed, None)],
        "{cmds:?}"
    );
    assert!(status(&rig).contains("interrupting"), "{}", status(&rig));

    // The store still lists the request (the write has not landed, or failed):
    // it is not decided a second time and no second Esc is written.
    let cmds = tick(&mut rig, vec![obs], vec![request(7)]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");
}

#[test]
fn a_store_read_that_fails_does_not_make_the_run_forget_what_it_decided() {
    // Request 7 is pressed and its outcome write fails, so the store still lists it
    // as undecided. The next tick cannot read the store at all: that is not an
    // empty list, and must not drop the run's memory of 7. The tick after reads 7
    // again, still undecided, the pane still busy and the request still young: a
    // second Esc would be the bug.
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert_eq!(presses(&cmds, &rig.worker), 1, "{cmds:?}");

    let cmds = tick_reading(&mut rig, vec![obs.clone()], None);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");

    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(7)]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");

    // An empty list that *was* read still forgets it: the outcome is recorded, and
    // the id may go. (A later request has a new id, so this loses nothing.)
    tick(&mut rig, vec![obs], Vec::new());
    assert!(rig.state.interrupt_decided.is_empty());
}

#[test]
fn a_request_exactly_at_the_age_limit_is_acted_on_and_one_past_it_is_expired() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let at_limit = InterruptFact {
        age_ms: INTERRUPT_STALE_AFTER.as_millis() as u64,
        ..request(1)
    };
    let cmds = tick(&mut rig, vec![obs.clone()], vec![at_limit]);
    assert_eq!(presses(&cmds, &rig.worker), 1);

    let mut rig = self::rig();
    let past = InterruptFact {
        age_ms: INTERRUPT_STALE_AFTER.as_millis() as u64 + 1,
        ..request(1)
    };
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![past]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Expired, None)]);
}

#[test]
fn what_is_not_a_running_turn_is_recorded_with_its_reason_and_no_esc() {
    use InterruptOutcome::NotMidTurn;
    use InterruptWhy::*;
    // `shell` is a background command after the turn ended: an Esc cuts nothing.
    let cases = [
        (live(Activity::Busy), BackgroundCommand),
        (live(Activity::Alive), Idle),
    ];
    for (live, why) in cases {
        let mut rig = rig();
        let obs = claude(&rig, live);
        let cmds = tick(&mut rig, vec![obs], vec![request(3)]);
        assert_eq!(presses(&cmds, &rig.worker), 0, "{why:?}: {cmds:?}");
        assert_eq!(recorded(&cmds), [(3, NotMidTurn, Some(why))], "{why:?}");
    }
}

#[test]
fn a_pane_that_cannot_be_read_yet_is_no_pane_not_a_pane_between_turns() {
    use InterruptOutcome::NoPane;
    // A Claude process that has published no live file, and a member whose
    // product is not resolved, are both a pane still starting: neither says a turn
    // is not running, and "idle" would be a claim about a pane banto cannot read.
    let mut rig = rig();
    let obs = claude(&rig, None);
    let cmds = tick(&mut rig, vec![obs], vec![request(3)]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert_eq!(recorded(&cmds), [(3, NoPane, Some(InterruptWhy::Starting))]);

    let mut rig = self::rig();
    let obs = RelayObservation {
        agent: None,
        ..claude(&rig, running_turn())
    };
    let cmds = tick(&mut rig, vec![obs], vec![request(4)]);
    assert_eq!(presses(&cmds, &rig.worker), 0);
    assert_eq!(recorded(&cmds), [(4, NoPane, Some(InterruptWhy::Starting))]);
}

#[test]
fn overlapping_facts_are_decided_in_the_documented_order() {
    use InterruptOutcome::*;
    // read beats everything, including age and a missing pane.
    let mut rig = rig();
    let old_and_read = InterruptFact {
        age_ms: INTERRUPT_STALE_AFTER.as_millis() as u64 + 1,
        read: true,
        ..request(1)
    };
    let cmds = tick(&mut rig, Vec::new(), vec![old_and_read]);
    assert_eq!(recorded(&cmds), [(1, Read, None)]);

    // expired beats no-pane.
    let mut rig = self::rig();
    let old = InterruptFact {
        age_ms: INTERRUPT_STALE_AFTER.as_millis() as u64 + 1,
        ..request(2)
    };
    let cmds = tick(&mut rig, Vec::new(), vec![old]);
    assert_eq!(recorded(&cmds), [(2, Expired, None)]);

    // no-pane (a hang-up queued) beats relay-manual, which beats codex.
    let mut rig = self::rig();
    rig.config.relay = RelayMode::Manual;
    let codex = RelayObservation {
        agent: Some(AgentKind::Codex),
        ..claude(&rig, running_turn())
    };
    let cmds = tick(&mut rig, vec![codex.clone()], vec![request(3)]);
    assert_eq!(recorded(&cmds), [(3, RelayManual, None)]);
    rig.state.hangups.insert(rig.worker.clone(), None);
    let cmds = tick(&mut rig, vec![codex.clone()], vec![request(4)]);
    assert_eq!(recorded(&cmds), [(4, NoPane, Some(InterruptWhy::Ending))]);
    rig.config.relay = RelayMode::Auto;
    rig.state.hangups.clear();
    let cmds = tick(&mut rig, vec![codex], vec![request(6)]);
    assert_eq!(recorded(&cmds), [(6, Codex, None)]);

    // waiting beats a line of banto's own in flight.
    let mut rig = self::rig();
    rig.state.pending_submits.push(PendingSubmit {
        key: rig.worker.clone(),
        nudged_at: rig.at,
    });
    let obs = claude(&rig, live(Activity::Waiting));
    let cmds = tick(&mut rig, vec![obs], vec![request(5)]);
    assert_eq!(recorded(&cmds), [(5, Waiting, None)]);
}

#[test]
fn a_line_banto_is_writing_into_the_pane_is_not_cut() {
    let mut rig = rig();
    rig.state.pending_submits.push(PendingSubmit {
        key: rig.worker.clone(),
        nudged_at: rig.at,
    });
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(3)]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert_eq!(
        recorded(&cmds),
        [(3, InterruptOutcome::NotMidTurn, Some(InterruptWhy::OwnLine))]
    );
}

#[test]
fn a_pane_waiting_on_a_prompt_is_not_pressed_and_the_operator_is_told_once() {
    let mut rig = rig();
    let obs = claude(&rig, live(Activity::Waiting));
    let cmds = tick(&mut rig, vec![obs.clone()], vec![request(5)]);
    assert_eq!(presses(&cmds, &rig.worker), 0);
    assert_eq!(recorded(&cmds), [(5, InterruptOutcome::Waiting, None)]);
    assert!(status(&rig).contains("waiting"), "{}", status(&rig));

    rig.state.status = None;
    rig.state.status_set_at = None;
    let cmds = tick(&mut rig, vec![obs], vec![request(5)]);
    assert!(recorded(&cmds).is_empty());
    assert!(!status(&rig).contains("waiting on a prompt"));
}

#[test]
fn a_message_already_read_a_codex_worker_and_manual_relay_are_recorded_and_not_pressed() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let read = InterruptFact {
        read: true,
        ..request(1)
    };
    let cmds = tick(&mut rig, vec![obs], vec![read]);
    assert_eq!(presses(&cmds, &rig.worker), 0);
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Read, None)]);

    let mut rig = self::rig();
    let obs = RelayObservation {
        agent: Some(AgentKind::Codex),
        ..claude(&rig, running_turn())
    };
    let cmds = tick(&mut rig, vec![obs], vec![request(2)]);
    assert_eq!(presses(&cmds, &rig.worker), 0);
    assert_eq!(recorded(&cmds), [(2, InterruptOutcome::Codex, None)]);

    let mut rig = self::rig();
    rig.config.relay = RelayMode::Manual;
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(3)]);
    assert_eq!(presses(&cmds, &rig.worker), 0);
    assert_eq!(recorded(&cmds), [(3, InterruptOutcome::RelayManual, None)]);
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
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(Ending))]);

    // Its brigade is being closed.
    let mut rig = self::rig();
    rig.state.closing.insert(BRIGADE, rig.at);
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1)]);
    assert_eq!(presses(&cmds, &rig.worker), 0, "{cmds:?}");
    assert_eq!(recorded(&cmds), [(1, NoPane, Some(Closing))]);
}

#[test]
fn an_unsent_draft_does_not_hold_the_esc() {
    let mut rig = rig();
    rig.state.unsent_operator_input.insert(rig.worker.clone());
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1)]);
    assert_eq!(presses(&cmds, &rig.worker), 1, "{cmds:?}");
    // The Esc is banto's key, not the operator's: it must not read as the operator
    // submitting (which clears the mark) or typing (which sets it).
    assert!(rig.state.has_unsent_operator_input(&rig.worker));

    let mut rig = self::rig();
    let obs = claude(&rig, running_turn());
    tick(&mut rig, vec![obs], vec![request(1)]);
    assert!(!rig.state.has_unsent_operator_input(&rig.worker));
}

#[test]
fn two_requests_for_one_pane_in_one_tick_share_one_esc_and_both_are_answered() {
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs], vec![request(1), request(2)]);
    assert_eq!(presses(&cmds, &rig.worker), 1, "{cmds:?}");
    assert_eq!(
        recorded(&cmds),
        [
            (1, InterruptOutcome::Pressed, None),
            (2, InterruptOutcome::Pressed, None)
        ]
    );

    // Both are refined together when the pane comes to rest.
    let rest = RelayObservation {
        is_idle_this_tick: Some(true),
        ..claude(&rig, live(Activity::Alive))
    };
    let cmds = tick(&mut rig, vec![rest], Vec::new());
    assert_eq!(
        recorded(&cmds),
        [
            (1, InterruptOutcome::Stopped, None),
            (2, InterruptOutcome::Stopped, None)
        ]
    );
}

#[test]
fn a_press_restarts_the_nudge_clock_and_the_idle_streak() {
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
    assert_eq!(after.idle_streak, 0, "the pane has a line in flight");
}

// --- what a press did ------------------------------------------------------------------------

/// The empty prompt block of a Claude pane.
fn empty_prompt() -> String {
    let rule = "\u{2500}".repeat(COLS as usize);
    format!(
        "\x1b[2J\x1b[19;1H{rule}\x1b[20;1H\u{276f}\u{a0}\x1b[2mTry something\x1b[0m\x1b[21;1H{rule}\
         \x1b[22;1H  ? for shortcuts\x1b[20;3H"
    )
}

/// The same block with `text` typed on the prompt row.
fn prompt_with(text: &str) -> String {
    empty_prompt() + &format!("\x1b[20;1H\x1b[K\u{276f}\u{a0}{text}")
}

fn draw(rig: &mut Rig, bytes: &str) {
    rig.state
        .screens
        .get_mut(&rig.worker)
        .expect("the worker is hosted")
        .process(bytes.as_bytes());
}

fn press(rig: &mut Rig) {
    let obs = claude(rig, running_turn());
    let cmds = tick(rig, vec![obs], vec![request(1)]);
    assert_eq!(presses(&cmds, &rig.worker), 1, "{cmds:?}");
}

fn at_rest(rig: &Rig) -> RelayObservation {
    RelayObservation {
        is_idle_this_tick: Some(true),
        ..claude(rig, live(Activity::Alive))
    }
}

#[test]
fn a_pane_at_rest_with_an_empty_line_after_a_press_is_stopped() {
    let mut rig = rig();
    draw(&mut rig, &empty_prompt());
    press(&mut rig);
    let obs = at_rest(&rig);
    let cmds = tick(&mut rig, vec![obs], Vec::new());
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Stopped, None)]);
    assert!(status(&rig).contains("interrupted"), "{}", status(&rig));
}

#[test]
fn the_nudge_the_esc_put_back_on_the_line_is_not_another_authors_text() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    press(&mut rig);
    let obs = at_rest(&rig);
    let cmds = tick(&mut rig, vec![obs], Vec::new());
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::Stopped, None)]);
}

#[test]
fn other_text_on_the_line_is_reported_without_quoting_it_and_names_both_ways_out() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with("SECRET-DRAFT-TEXT"));
    press(&mut rig);
    let obs = at_rest(&rig);
    let cmds = tick(&mut rig, vec![obs], Vec::new());
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::OtherText, None)]);
    let said = status(&rig);
    assert!(!said.contains("SECRET"), "{said}");
    assert!(said.contains("did not type"), "{said}");
    assert!(said.contains("clear the line"), "{said}");
    assert!(said.contains("without having read why"), "{said}");
}

#[test]
fn a_pane_that_never_comes_to_rest_is_still_busy_after_the_settle_ticks() {
    let mut rig = rig();
    draw(&mut rig, &empty_prompt());
    press(&mut rig);
    for tick_number in 1..INTERRUPT_SETTLE_TICKS {
        let obs = claude(&rig, running_turn());
        let cmds = tick(&mut rig, vec![obs], Vec::new());
        assert!(recorded(&cmds).is_empty(), "tick {tick_number}: {cmds:?}");
    }
    let obs = claude(&rig, running_turn());
    let cmds = tick(&mut rig, vec![obs.clone()], Vec::new());
    assert_eq!(recorded(&cmds), [(1, InterruptOutcome::StillBusy, None)]);
    assert!(status(&rig).contains("still busy"), "{}", status(&rig));

    // Said once: the watch is over.
    let cmds = tick(&mut rig, vec![obs], Vec::new());
    assert!(recorded(&cmds).is_empty());
}

#[test]
fn a_press_on_a_pane_that_goes_away_is_left_as_pressed() {
    let mut rig = rig();
    draw(&mut rig, &empty_prompt());
    press(&mut rig);
    let cmds = tick(&mut rig, Vec::new(), Vec::new());
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");
    // The pane's end clears what was waiting on it.
    rig.state.unstage(&rig.worker.clone());
    assert!(rig.state.interrupt_watches.is_empty());
    assert!(rig.state.interrupt_repairs.is_empty());
}

// --- the Enter-only repair -------------------------------------------------------------------

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
fn the_nudge_an_esc_restored_is_delivered_by_its_enter_alone() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    press(&mut rig);
    let written = writes_within(&mut rig, 6);
    assert_eq!(
        written,
        [b"\r".to_vec()],
        "Enter alone, not the sentence again"
    );
    assert!(status(&rig).contains("Enter only"), "{}", status(&rig));
    // Not ended by sending the Enter: a lost one has to be sent again.
    assert!(
        rig.state
            .interrupt_repairs
            .contains(&(BRIGADE, WORKER.to_string()))
    );
}

#[test]
fn neither_the_esc_nor_the_enter_only_repair_touches_the_unsent_draft_mark() {
    // The operator typed something earlier, so the pane is marked. The Esc restored
    // the nudge onto the line; its Enter goes out. The Enter is banto's own, as the
    // nudge's has always been: only an Enter the operator forwards clears the mark.
    let mut rig = rig();
    rig.state.unsent_operator_input.insert(rig.worker.clone());
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    press(&mut rig);
    assert!(rig.state.has_unsent_operator_input(&rig.worker));
    let written = writes_within(&mut rig, 6);
    assert_eq!(written, [b"\r".to_vec()], "the Enter went out");
    assert!(
        rig.state.has_unsent_operator_input(&rig.worker),
        "banto's Enter must leave the operator's mark alone"
    );

    // And the other way: no mark before, none after.
    let mut rig = self::rig();
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    press(&mut rig);
    let written = writes_within(&mut rig, 6);
    assert_eq!(written, [b"\r".to_vec()]);
    assert!(!rig.state.has_unsent_operator_input(&rig.worker));
}

#[test]
fn the_repair_ends_when_the_member_has_no_unread_mail() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    press(&mut rig);
    let read = RelayObservation {
        has_unseen: false,
        ..at_rest(&rig)
    };
    tick(&mut rig, vec![read], Vec::new());
    assert!(rig.state.interrupt_repairs.is_empty());
}

#[test]
fn text_that_is_not_the_nudge_still_holds_the_nudge() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with("a half-written thought"));
    press(&mut rig);
    let written = writes_within(&mut rig, 6);
    assert!(written.is_empty(), "{written:?}");
}

#[test]
fn an_empty_line_gets_the_ordinary_nudge_with_its_sentence() {
    let mut rig = rig();
    draw(&mut rig, &empty_prompt());
    press(&mut rig);
    let written = writes_within(&mut rig, 6);
    assert_eq!(written, [RELAY_NUDGE_LINE.as_bytes().to_vec()]);
}

#[test]
fn without_a_press_the_nudge_on_the_line_is_a_draft_as_before() {
    let mut rig = rig();
    draw(&mut rig, &prompt_with(RELAY_NUDGE_LINE));
    let written = writes_within(&mut rig, 6);
    assert!(written.is_empty(), "{written:?}");
}
