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
    /// Who the store lists on the brigade's roster.
    roster: Vec<MemberToken>,
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
        roster: vec![DIRECTOR_TOKEN.to_string(), WORKER.to_string()],
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
        undecided: true,
        word: None,
        age_ms: 0,
        line: String::new(),
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
        roster: rig.roster.clone(),
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
    // What it arms ends when the Worker reads the message, so a request found after
    // a restart is armed like any other, whatever its age.
    let mut rig = rig();
    let obs = claude(&rig, live(Activity::Alive));
    let old = InterruptFact {
        age_ms: 86_400_000,
        ..request(1)
    };
    let cmds = tick(&mut rig, vec![obs], vec![old]);
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

// --- an interrupt that sits unread while its Worker works -----------------------------------

const ARMED: Option<InterruptOutcome> = Some(InterruptOutcome::Armed);

/// A request `age_ms` old that is still unread, carrying `word`. A row with any word
/// but the gate's is one the UI decided, which is what the store says of it too.
fn unread(message_id: i64, age_ms: i64, word: Option<InterruptOutcome>) -> InterruptFact {
    InterruptFact {
        message_id,
        age_ms,
        word,
        undecided: matches!(
            word,
            None | Some(InterruptOutcome::Gated | InterruptOutcome::Capped)
        ),
        ..request(message_id)
    }
}

/// The rig's tick with the Worker observed as `idle` and the one request listed.
fn tick_unread(rig: &mut Rig, idle: Option<bool>, interrupts: Vec<InterruptFact>) {
    let obs = RelayObservation {
        is_idle_this_tick: idle,
        ..claude(rig, running_turn())
    };
    tick(rig, vec![obs], interrupts);
}

/// What the notice said for one word of a request a minute old, or nothing.
fn notice_for(word: Option<InterruptOutcome>, obs: impl Fn(&Rig) -> RelayObservation) -> String {
    let mut rig = rig();
    let obs = obs(&rig);
    // A request the UI has not decided is decided on the first tick, and that line
    // has the status line for its five seconds; the notice waits for it.
    let mut said = String::new();
    for _ in 0..7 {
        tick(&mut rig, vec![obs.clone()], vec![unread(5, 61_000, word)]);
        if said.is_empty() && status(&rig).contains("interrupt unread") {
            said = status(&rig);
        }
    }
    said
}

#[test]
fn an_interrupt_is_told_unread_from_thirty_seconds_on_and_not_before() {
    let mut rig = rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 29_999, ARMED)]);
    assert_eq!(status(&rig), "");

    let mut rig = self::rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 30_000, ARMED)]);
    assert!(
        status(&rig).contains("interrupt unread for 30s"),
        "{}",
        status(&rig)
    );
}

#[test]
fn an_interrupt_is_told_only_while_the_worker_is_seen_working() {
    // Working: said. Idle: nothing to stop. Unknown: no claim either way.
    for (idle, said) in [(Some(false), true), (Some(true), false), (None, false)] {
        let mut rig = rig();
        tick_unread(&mut rig, idle, vec![unread(5, 45_000, ARMED)]);
        assert_eq!(status(&rig).contains("interrupt unread"), said, "{idle:?}");
    }

    // A Worker whose pane is not observed this tick is not working as far as the
    // tick knows.
    let mut rig = self::rig();
    tick(&mut rig, Vec::new(), vec![unread(5, 45_000, ARMED)]);
    assert!(
        !status(&rig).contains("interrupt unread"),
        "{}",
        status(&rig)
    );
}

#[test]
fn each_word_says_only_what_is_true_of_it() {
    use InterruptOutcome::*;
    let claude_pane = |rig: &Rig| claude(rig, running_turn());
    let said = |word| notice_for(word, claude_pane);
    let head = "relay: worker-1 of \"cell\": interrupt unread for 1m";
    for (word, tail) in [
        (None, ", no tool call since"),
        (Some(Armed), ", no tool call since"),
        (Some(Gated), ", a tool call was refused"),
        (Some(Capped), ", went on past the refusals"),
        (Some(RelayManual), ", relay is manual"),
        // The word says nothing about tools, so there is nothing to tell.
        (Some(NoPane), ""),
    ] {
        assert_eq!(said(word), format!("{head}{tail}"), "{word:?}");
    }
    assert_eq!(
        notice_for(Some(Codex), codex),
        format!("{head}, a Codex turn cannot be interrupted")
    );
    // No key is named, whatever the word.
    for word in [None, Some(Armed), Some(Gated), Some(Capped)] {
        assert!(!said(word).to_lowercase().contains("ctrl"), "{word:?}");
    }
}

#[test]
fn the_notice_quotes_the_interrupts_own_first_line_not_the_newest_message() {
    let mut rig = rig();
    // The Director sent something else after the interrupt: the member's newest
    // message is that one.
    let obs = RelayObservation {
        glance: Some(MemberGlance {
            inbound: Some(MessageGlance {
                created_at_ms: 2,
                line: "carry on".to_string(),
                from_token: Some(DIRECTOR_TOKEN.to_string()),
            }),
            sent: None,
        }),
        ..claude(&rig, running_turn())
    };
    let fact = InterruptFact {
        line: "stop the migration".to_string(),
        ..unread(5, 45_000, ARMED)
    };
    tick(&mut rig, vec![obs], vec![fact]);
    assert!(
        status(&rig).ends_with("\u{2190} director: stop the migration"),
        "{}",
        status(&rig)
    );
}

#[test]
fn the_decision_line_quotes_the_interrupts_own_first_line_too() {
    let mut rig = rig();
    let obs = RelayObservation {
        glance: Some(MemberGlance {
            inbound: Some(MessageGlance {
                created_at_ms: 2,
                line: "carry on".to_string(),
                from_token: Some(DIRECTOR_TOKEN.to_string()),
            }),
            sent: None,
        }),
        ..claude(&rig, running_turn())
    };
    let fact = InterruptFact {
        line: "stop the migration".to_string(),
        ..request(5)
    };
    tick(&mut rig, vec![obs], vec![fact]);
    assert!(
        status(&rig).ends_with("got an interrupt \u{2190} director: stop the migration"),
        "{}",
        status(&rig)
    );
}

#[test]
fn several_unread_interrupts_are_told_one_per_showing_oldest_first() {
    let mut rig = rig();
    let listed = |age: i64| {
        vec![
            unread(5, age + 20_000, ARMED),
            unread(6, age + 10_000, ARMED),
            unread(7, age, ARMED),
        ]
    };
    // The age is as of the showing: 50s, 40s, 30s are what the three are at first.
    tick_unread(&mut rig, Some(false), listed(30_000));
    assert!(status(&rig).contains("for 50s"), "{}", status(&rig));

    // Five seconds on the line, then twenty-five of nothing, then the next one:
    // the oldest not yet shown, with its age as of now.
    for second in 1..30 {
        tick_unread(&mut rig, Some(false), listed(30_000 + second * 1000));
        let line = status(&rig);
        assert_eq!(
            line.contains("interrupt unread"),
            second < 5,
            "{second}: {line}"
        );
    }
    tick_unread(&mut rig, Some(false), listed(60_000));
    assert!(status(&rig).contains("for 1m"), "{}", status(&rig));
    assert_eq!(rig.state.unread_interrupts_shown.len(), 2);

    // The third, and then the first again: the one shown longest ago.
    for _ in 0..30 {
        tick_unread(&mut rig, Some(false), listed(90_000));
    }
    assert_eq!(rig.state.unread_interrupts_shown.len(), 3);
    for _ in 0..30 {
        tick_unread(&mut rig, Some(false), listed(120_000));
    }
    let shown = &rig.state.unread_interrupts_shown;
    assert!(shown[&5].last_shown > shown[&6].last_shown, "{shown:?}");
}

/// Every different line the status shows over `seconds` ticks of the Worker working,
/// the store listing `interrupts` each time.
fn lines_over(rig: &mut Rig, seconds: u32, interrupts: Vec<InterruptFact>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for _ in 0..seconds {
        tick_unread(rig, Some(false), interrupts.clone());
        let line = status(rig);
        if !line.is_empty() && lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    lines
}

const READ_LINE: &str = "relay: worker-1 of \"cell\" read the interrupt";

#[test]
fn an_interrupt_that_is_then_read_is_said_to_have_been_read_but_only_after_a_showing() {
    // Shown, then read: said once, when the line is free, and not again.
    let mut rig = rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 45_000, ARMED)]);
    assert!(status(&rig).contains("interrupt unread"));
    let lines = lines_over(&mut rig, 60, Vec::new());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains("interrupt unread"), "{lines:?}");
    assert_eq!(lines[1], READ_LINE);
    assert!(rig.state.unread_interrupts_shown.is_empty());

    // Read before it was ever told: nothing to take back.
    let mut rig = self::rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 10_000, ARMED)]);
    assert_eq!(lines_over(&mut rig, 60, Vec::new()), Vec::<String>::new());

    // The cursor passing the message, the word kept, and the Worker gone idle in
    // the same tick: still read.
    let mut rig = self::rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 45_000, ARMED)]);
    let now_read = InterruptFact {
        read: true,
        ..unread(5, 46_000, ARMED)
    };
    for _ in 0..8 {
        tick_unread(&mut rig, Some(true), vec![now_read.clone()]);
    }
    assert_eq!(status(&rig), READ_LINE);
}

#[test]
fn an_interrupt_whose_worker_left_the_roster_is_dropped_without_a_word() {
    let mut rig = rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 45_000, ARMED)]);
    rig.roster.retain(|token| token != WORKER);
    let lines = lines_over(&mut rig, 60, Vec::new());
    assert!(
        lines.iter().all(|line| line.contains("interrupt unread")),
        "{lines:?}"
    );
    assert!(rig.state.unread_interrupts_shown.is_empty());
}

#[test]
fn a_list_that_could_not_be_read_says_and_ends_nothing() {
    let mut rig = rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 45_000, ARMED)]);
    assert!(status(&rig).contains("interrupt unread"));

    // Not an empty list: the interrupt is neither read nor gone, so nothing more is
    // said of it, no `read` either, and it is still remembered as shown.
    for second in 1..=60 {
        let obs = RelayObservation {
            is_idle_this_tick: Some(false),
            ..claude(&rig, running_turn())
        };
        tick_reading(&mut rig, vec![obs], None);
        if second >= 5 {
            assert_eq!(status(&rig), "", "{second}");
        }
    }
    assert!(rig.state.unread_interrupts_shown.contains_key(&5));
    assert!(rig.state.interrupt_read_notices.is_empty());
}

#[test]
fn an_interrupt_the_ui_already_decided_is_not_decided_again_for_being_listed() {
    // Armed and unread is listed for the notice, and only for it.
    let mut rig = rig();
    let obs = claude(&rig, running_turn());
    let armed = unread(5, 45_000, Some(InterruptOutcome::Armed));
    let cmds = tick(&mut rig, vec![obs], vec![armed]);
    assert!(recorded(&cmds).is_empty(), "{cmds:?}");
    assert!(
        rig.state.interrupt_decided.is_empty(),
        "{:?}",
        rig.state.interrupt_decided
    );
}

/// One tick of several brigades at once, each with the list the store gave for it.
fn tick_brigades(
    rig: &mut Rig,
    relay: Vec<RelayObservation>,
    brigades: Vec<(BrigadeId, Vec<InterruptFact>, Vec<MemberToken>)>,
) {
    rig.at += SECOND;
    let brigades = brigades
        .into_iter()
        .map(|(brigade_id, interrupts, roster)| BrigadeNameFacts {
            brigade_id,
            name: format!("cell {brigade_id}"),
            director_session: None,
            member_names: Vec::new(),
            request: None,
            interrupts: Some(interrupts),
            roster,
        })
        .collect();
    update(
        &mut rig.state,
        &mut rig.app,
        &rig.config,
        Event::Tick { relay, brigades },
        rig.at,
    );
}

#[test]
fn an_interrupt_already_read_is_not_told_unread_even_if_still_listed() {
    // Listed because the UI has not decided it (a restart, say), and its Worker
    // has read the message since.
    let mut rig = rig();
    let fact = InterruptFact {
        read: true,
        ..unread(5, 45_000, None)
    };
    for _ in 0..3 {
        tick_unread(&mut rig, Some(false), vec![fact.clone()]);
        assert!(
            !status(&rig).contains("interrupt unread"),
            "{}",
            status(&rig)
        );
    }
}

#[test]
fn another_member_working_does_not_make_an_idle_worker_unread() {
    // The Director is always busy; the Worker the interrupt is for is idle.
    let mut rig = rig();
    let director = RelayObservation {
        token: DIRECTOR_TOKEN.to_string(),
        key: key("dir"),
        is_idle_this_tick: Some(false),
        ..claude(&rig, running_turn())
    };
    let worker = RelayObservation {
        is_idle_this_tick: Some(true),
        ..claude(&rig, running_turn())
    };
    for _ in 0..3 {
        tick(
            &mut rig,
            vec![director.clone(), worker.clone()],
            vec![unread(5, 45_000, ARMED)],
        );
        assert!(
            !status(&rig).contains("interrupt unread"),
            "{}",
            status(&rig)
        );
    }
}

#[test]
fn a_member_working_in_another_brigade_does_not_make_this_ones_unread() {
    // worker-1 of brigade 2 works; worker-1 of brigade 1 is idle.
    let mut rig = rig();
    let idle_here = RelayObservation {
        is_idle_this_tick: Some(true),
        ..claude(&rig, running_turn())
    };
    let busy_there = RelayObservation {
        brigade_id: 2,
        is_idle_this_tick: Some(false),
        ..claude(&rig, running_turn())
    };
    for _ in 0..3 {
        tick_brigades(
            &mut rig,
            vec![idle_here.clone(), busy_there.clone()],
            vec![(
                BRIGADE,
                vec![unread(5, 45_000, ARMED)],
                vec![WORKER.to_string()],
            )],
        );
        assert!(
            !status(&rig).contains("interrupt unread"),
            "{}",
            status(&rig)
        );
    }
}

#[test]
fn another_brigades_list_does_not_end_this_brigades_shown_interrupt() {
    // Two brigades share the token `worker-1`. Brigade 1's interrupt was shown and
    // is still unread; brigade 2's list is empty. Reading brigade 2's list must not
    // say brigade 1's was read.
    let mut rig = rig();
    tick_unread(&mut rig, Some(false), vec![unread(5, 45_000, ARMED)]);
    assert!(status(&rig).contains("interrupt unread"));
    let obs = RelayObservation {
        is_idle_this_tick: Some(false),
        ..claude(&rig, running_turn())
    };
    for _ in 0..40 {
        tick_brigades(
            &mut rig,
            vec![obs.clone()],
            vec![
                (
                    BRIGADE,
                    vec![unread(5, 45_000, ARMED)],
                    vec![WORKER.to_string()],
                ),
                (2, Vec::new(), vec![WORKER.to_string()]),
            ],
        );
        let line = status(&rig);
        assert!(
            line.is_empty() || line.contains("interrupt unread"),
            "{line}"
        );
    }
    assert!(rig.state.unread_interrupts_shown.contains_key(&5));
    assert!(rig.state.interrupt_read_notices.is_empty());
}

#[test]
fn the_notice_waits_for_a_line_that_is_already_showing() {
    let mut rig = rig();
    let at = rig.at;
    rig.state.set_status("something failed".to_string(), at);
    let obs = RelayObservation {
        is_idle_this_tick: Some(false),
        ..claude(&rig, running_turn())
    };
    for second in 1..5 {
        tick(&mut rig, vec![obs.clone()], vec![unread(5, 45_000, ARMED)]);
        assert_eq!(status(&rig), "something failed", "{second}");
    }
    // Once it has run out, the notice that waited goes up, and is not lost.
    let mut said = false;
    for _ in 0..3 {
        tick(&mut rig, vec![obs.clone()], vec![unread(5, 45_000, ARMED)]);
        said |= status(&rig).contains("interrupt unread");
    }
    assert!(said);
}

#[test]
fn interrupts_of_equal_age_are_told_smaller_id_first() {
    let mut rig = rig();
    tick_unread(
        &mut rig,
        Some(false),
        vec![unread(6, 45_000, ARMED), unread(5, 45_000, ARMED)],
    );
    let shown: Vec<i64> = rig.state.unread_interrupts_shown.keys().copied().collect();
    assert_eq!(shown, [5]);
}

#[test]
fn the_age_is_told_in_its_two_largest_units_and_truncated() {
    for (ms, told) in [
        (0, "0s"),
        (45_999, "45s"),
        (60_000, "1m"),
        (90_000, "1m"),
        (120_000, "2m"),
        (3_599_999, "59m"),
        (3_600_000, "1h"),
        (3_900_000, "1h 5m"),
        (7_260_000, "2h 1m"),
        (-5, "0s"),
    ] {
        assert_eq!(unread_age(ms), told, "{ms}");
    }
}
