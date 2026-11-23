//! Whether hosted sessions are known to be idle ([`Event::QuiescenceSampled`])
//! as the operator meets it: a quit that asks only when something might be
//! lost, and a bell when a brigade has gone quiet.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use crate::model::Quiescence::{Background, Idle, NotIdle, Unknown};
use crate::screen::Screen;

use super::*;

#[allow(clippy::disallowed_methods)]
fn t0() -> Instant {
    Instant::now()
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, at)
}

/// The brigades the store would list this second: every one the hosted panes
/// belong to, except one that has been disbanded.
fn listed(state: &EmporiumState) -> Vec<BrigadeNameFacts> {
    let mut ids: Vec<BrigadeId> = state.member_brigades.values().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .filter(|id| !state.disbanding.contains(id))
        .map(|brigade_id| BrigadeNameFacts {
            brigade_id,
            name: "cell".to_string(),
            director_session: None,
            member_names: Vec::new(),
            request: stored_request(brigade_id),
            interrupts: None,
        })
        .collect()
}

thread_local! {
    /// What the store holds as each brigade's open request, as the shell would
    /// read it into every tick: its sequence. Per test thread.
    static STORED_REQUESTS: std::cell::RefCell<HashMap<BrigadeId, i64>> =
        std::cell::RefCell::new(HashMap::new());
}

fn stored_request(brigade_id: BrigadeId) -> Option<RequestFact> {
    STORED_REQUESTS.with(|stored| {
        stored
            .borrow()
            .get(&brigade_id)
            .map(|seq| RequestFact { seq: *seq })
    })
}

/// A Director's `request_decision`: the store holds a request under a new number.
fn store_request(brigade_id: BrigadeId) -> i64 {
    STORED_REQUESTS.with(|stored| {
        let mut stored = stored.borrow_mut();
        let seq = stored.get(&brigade_id).copied().unwrap_or(0) + 1;
        stored.insert(brigade_id, seq);
        seq
    })
}

/// What the shell does with the `Cmd`s that reach the store: a conditional clear.
fn run_store(cmds: &[Cmd]) {
    for cmd in cmds {
        if let Cmd::Store(StoreIntent::ClearRequest { brigade_id, seq }) = cmd {
            STORED_REQUESTS.with(|stored| {
                let mut stored = stored.borrow_mut();
                if stored.get(brigade_id).is_some_and(|held| held == seq) {
                    stored.remove(brigade_id);
                }
            });
        }
    }
}

fn forget_stored_requests() {
    STORED_REQUESTS.with(|stored| stored.borrow_mut().clear());
}

/// What the relay observes in a tick when the roster reads fine: every hosted
/// member of a brigade that still exists, with nothing unread, the entries of
/// `relay` replacing the default for the same pane. A disbanded brigade is not
/// observed: the store no longer has it.
fn observed(state: &EmporiumState, relay: Vec<RelayObservation>) -> Vec<RelayObservation> {
    let mut all: Vec<RelayObservation> = state
        .screens
        .keys()
        .filter_map(|pane| {
            let brigade_id = *state.member_brigades.get(pane)?;
            if state.disbanding.contains(&brigade_id) {
                return None;
            }
            Some(RelayObservation {
                live: None,
                glance: None,
                brigade_id,
                token: state.member_tokens.get(pane)?.clone(),
                key: pane.clone(),
                has_unseen: false,
                is_idle_this_tick: Some(true),
                agent: None,
                briefed_since_launch: None,
            })
        })
        .filter(|default| !relay.iter().any(|given| given.key == default.key))
        .collect();
    all.extend(relay);
    all
}

fn tick(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    let brigades = listed(state);
    let relay = observed(state, Vec::new());
    send(state, app, Event::Tick { relay, brigades }, at)
}

/// What the shell sends every second: a sample, then the tick that carries the
/// relay's observations — in that order, at the same instant.
fn sample_with_relay(
    state: &mut EmporiumState,
    app: &mut App,
    samples: &[(&SessionKey, Quiescence)],
    relay: Vec<RelayObservation>,
    at: Instant,
) -> Vec<Cmd> {
    let samples = samples
        .iter()
        .map(|(key, q)| ((*key).clone(), *q))
        .collect::<Vec<_>>();
    let mut cmds = send(state, app, Event::QuiescenceSampled { samples }, at);
    let brigades = listed(state);
    let relay = observed(state, relay);
    cmds.extend(send(state, app, Event::Tick { relay, brigades }, at));
    cmds
}

fn sample(
    state: &mut EmporiumState,
    app: &mut App,
    samples: &[(&SessionKey, Quiescence)],
    at: Instant,
) -> Vec<Cmd> {
    sample_with_relay(state, app, samples, Vec::new(), at)
}

fn press(state: &mut EmporiumState, app: &mut App, code: KeyCode, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Input(InputEvent::Key(KeyEvent::new(code, Modifiers::NONE))),
        at,
    )
}

fn rang(cmds: &[Cmd]) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, Cmd::RingHostBell { .. }))
}

fn key(id: &str) -> SessionKey {
    SessionKey::from_id(id)
}

/// A hosted pane of `brigade` under `token`, with a screen, whose process is
/// taken to have run (and idled) before the test begins: started well before
/// [`PROCESS_START_GRACE`] could matter, and already read idle.
fn host(state: &mut EmporiumState, brigade: BrigadeId, token: &str, id: &str) -> SessionKey {
    let pane = host_fresh(state, brigade, token, id);
    let process = state.spawn_order[&pane];
    state.seen_idle.insert(process);
    if let Some(long_ago) = t0().checked_sub(2 * PROCESS_START_GRACE) {
        state.process_started.insert(pane.clone(), long_ago);
    }
    pane
}

/// A hosted pane whose process has only just started: nothing is known about it.
fn host_fresh(state: &mut EmporiumState, brigade: BrigadeId, token: &str, id: &str) -> SessionKey {
    let pane = key(id);
    // As `update_spawned` does: a screen comes with the number of the process
    // that holds it.
    state.screens.insert(pane.clone(), Screen::new(24, 80));
    state.spawn_order.insert(pane.clone(), state.next_spawn);
    state.next_spawn += 1;
    state.process_started.insert(pane.clone(), t0());
    state.member_tokens.insert(pane.clone(), token.to_string());
    state.member_brigades.insert(pane.clone(), brigade);
    pane
}

/// The pane's process ends and its screen goes, as `update_pty_exited` does for
/// a member that is not relaunched.
fn unhost(state: &mut EmporiumState, pane: &SessionKey) {
    state.screens.remove(pane);
    state.spawn_order.remove(pane);
}

/// Put `brigade` on stage by assigning it, leaving the focus where it was.
///
/// For a brigade that is on stage already with the operator back in the list
/// (F2): that state is reachable. NOT for one being staged: the product stages
/// through Enter on the Director's row and puts the focus on the Director's tile
/// with the pane focused, which clears that pane's latch in the same update; use
/// [`OffStage::stage_by_enter`] for that.
fn stage(
    state: &mut EmporiumState,
    brigade: BrigadeId,
    director: &SessionKey,
    rest: &[&SessionKey],
) {
    let mut panes = vec![director.clone()];
    panes.extend(rest.iter().map(|pane| (*pane).clone()));
    state.stage = Stage::Brigade {
        id: brigade,
        director: Some(director.clone()),
        panes,
        focused: 0,
    };
}

/// A hosted pane that belongs to no brigade.
fn host_solo(state: &mut EmporiumState, id: &str) -> SessionKey {
    let pane = key(id);
    state.screens.insert(pane.clone(), Screen::new(24, 80));
    pane
}

// --- the quit confirmation --------------------------------------------------

/// Two plain hosted sessions, the list focused, and a first tick (the
/// attention display seeds itself silently on the first update it sees).
fn quit_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    let b = host_solo(&mut state, "b");
    state.focus = Focus::Sidebar;
    let at = t0();
    tick(&mut state, &mut app, at);
    (state, app, a, b, at)
}

#[test]
fn with_every_session_known_idle_one_quit_key_quits() {
    for code in [KeyCode::Esc, KeyCode::Char('q')] {
        let (mut state, mut app, a, b, at) = quit_fixture();
        sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
        press(&mut state, &mut app, code, at);
        assert!(app.should_quit(), "{code:?}");
        assert_eq!(state.displayed_status(), None);
    }
}

#[test]
fn one_session_that_is_not_idle_among_idle_ones_arms_the_prompt_with_a_count_of_one() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, NotIdle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: b (busy)")
    );
    press(
        &mut state,
        &mut app,
        KeyCode::Char('q'),
        at + Duration::from_secs(1),
    );
    assert!(app.should_quit(), "the second key confirms");
}

#[test]
fn a_session_nothing_is_known_about_asks_like_one_that_is_busy() {
    let (mut state, mut app, a, _b, at) = quit_fixture();
    // `b` is left out of the sample, `a` is read as unknown: both count.
    sample(&mut state, &mut app, &[(&a, Unknown)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 2 not idle: a (unknown), b (unknown)")
    );
}

#[test]
fn before_any_sample_every_hosted_session_asks() {
    let (mut state, mut app, _a, _b, at) = quit_fixture();
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 2 not idle: a (unknown), b (unknown)")
    );
}

#[test]
fn a_session_whose_process_has_exited_is_idle_for_the_question() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, NotIdle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(state.displayed_status().is_some());

    // The busy one exits during the window: nothing is left to interrupt, so
    // the prompt goes with it ...
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: b.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(1),
    );
    assert!(
        !state
            .displayed_status()
            .is_some_and(|text| text.starts_with("Esc again")),
        "the quit prompt is gone"
    );
    // ... and a quit key now quits at once.
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + Duration::from_secs(2),
    );
    assert!(app.should_quit());
}

#[test]
fn a_quit_key_after_the_only_busy_session_exited_quits_on_the_first_press() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, NotIdle)], at);
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: b,
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    );
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(app.should_quit());
}

#[test]
fn the_prompt_follows_the_samples_and_goes_when_the_count_reaches_zero() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, NotIdle), (&b, NotIdle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 2 not idle: a (busy), b (busy)")
    );

    sample(
        &mut state,
        &mut app,
        &[(&a, Idle), (&b, NotIdle)],
        at + Duration::from_secs(1),
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: b (busy)"),
        "rebuilt from the new sample, not stale"
    );

    sample(
        &mut state,
        &mut app,
        &[(&a, Idle), (&b, Idle)],
        at + Duration::from_secs(2),
    );
    assert_eq!(state.displayed_status(), None, "nothing left to ask about");
    // So the next key is a first press, and it quits.
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + Duration::from_secs(2),
    );
    assert!(app.should_quit());
}

#[test]
fn a_sample_replaces_the_last_one_as_a_whole() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    // The next sample says nothing about `b`: it is unknown again, not idle
    // by memory.
    sample(
        &mut state,
        &mut app,
        &[(&a, Idle)],
        at + Duration::from_secs(1),
    );
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + Duration::from_secs(1),
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: b (unknown)")
    );
}

#[test]
fn a_pane_opened_after_the_last_sample_asks_until_the_next_one() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    let fresh = key("fresh");
    // A pane that held this key before was read as idle; the new process is a
    // different matter.
    state.quiescence.insert(fresh.clone(), Idle);
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: fresh.clone(),
            agent: None,
        },
        at,
    );
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: fresh (unknown)")
    );
}

#[test]
fn a_pane_whose_key_changes_keeps_its_sample() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    state.rekey_pane_state(&a, &key("a-after-discovery"));
    assert_eq!(state.quit_counts(&app, at).not_idle(), 0);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(app.should_quit());
}

// --- a background command in the quit question --------------------------------

/// The text the first quit key arms with, given what each of `a` and `b` was read as.
fn prompt_for(a_is: Quiescence, b_is: Quiescence) -> Option<String> {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, a_is), (&b, b_is)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    if app.should_quit() {
        return None;
    }
    state.displayed_status().map(str::to_string)
}

#[test]
fn the_quit_prompt_says_a_background_command_apart_from_work_that_is_not_idle() {
    let dash = "\u{2014}";
    for (a, b, expected) in [
        // Only work that is not idle: the words it has always had, then who.
        (
            NotIdle,
            Idle,
            format!("Esc again to quit {dash} 1 not idle: a (busy)"),
        ),
        (
            Unknown,
            NotIdle,
            format!("Esc again to quit {dash} 2 not idle: a (unknown), b (busy)"),
        ),
        // Only background commands: said as that.
        (
            Background,
            Idle,
            format!("Esc again to quit {dash} 1 in background: a"),
        ),
        (
            Background,
            Background,
            format!("Esc again to quit {dash} 2 in background: a, b"),
        ),
        // Both, and either order of the panes says the same.
        (
            NotIdle,
            Background,
            format!("Esc again to quit {dash} 1 not idle, 1 in background: a (busy), b (bg)"),
        ),
        (
            Background,
            NotIdle,
            format!("Esc again to quit {dash} 1 not idle, 1 in background: b (busy), a (bg)"),
        ),
        // A pane nothing is known about is not idle, as it always was.
        (
            Unknown,
            Background,
            format!("Esc again to quit {dash} 1 not idle, 1 in background: a (unknown), b (bg)"),
        ),
    ] {
        assert_eq!(
            prompt_for(a, b).as_deref(),
            Some(expected.as_str()),
            "{a:?} {b:?}"
        );
    }
    // And a second key confirms it as it confirms the others.
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Background), (&b, Idle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(!app.should_quit());
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert!(app.should_quit());
}

#[test]
fn a_background_command_still_makes_the_quit_ask_and_idle_alone_does_not() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(app.should_quit(), "nothing running: one key");
    assert!(prompt_for(Idle, Background).is_some(), "it asks");
}

#[test]
fn the_prompt_follows_a_background_command_through_to_its_end() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&a, Background), (&b, Background)],
        at,
    );
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 2 in background: a, b")
    );
    // One of them ends its command, the other starts a turn: rebuilt each time.
    sample(
        &mut state,
        &mut app,
        &[(&a, Idle), (&b, NotIdle)],
        at + SECOND,
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: b (busy)")
    );
    sample(
        &mut state,
        &mut app,
        &[(&a, Background), (&b, NotIdle)],
        at + 2 * SECOND,
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle, 1 in background: b (busy), a (bg)")
    );
    sample(
        &mut state,
        &mut app,
        &[(&a, Idle), (&b, Idle)],
        at + 3 * SECOND,
    );
    assert_eq!(state.displayed_status(), None, "nothing left to ask about");
    press(&mut state, &mut app, KeyCode::Esc, at + 3 * SECOND);
    assert!(app.should_quit());
}

#[test]
fn a_background_pane_that_was_just_typed_into_counts_as_not_idle_for_the_window() {
    // A turn that input started may not show in the sample yet, and its being
    // in the background says nothing about that: the pane is one that took
    // input, so it is counted with the work, and goes back to the background
    // count once the window is over.
    for (after, expected) in [
        (
            QUIT_RECENT_INPUT_WINDOW - Duration::from_millis(1),
            "Esc again to quit \u{2014} 1 not idle: a (input)",
        ),
        (
            QUIT_RECENT_INPUT_WINDOW,
            "Esc again to quit \u{2014} 1 in background: a",
        ),
    ] {
        let (mut state, mut app, a, b, at) = quit_fixture();
        state.stage = Stage::Solo(a.clone());
        state.focus = Focus::Pane;
        sample(&mut state, &mut app, &[(&a, Background), (&b, Idle)], at);
        press(&mut state, &mut app, KeyCode::Char('x'), at);
        back_to_the_list(&mut state, &mut app, at);
        press(&mut state, &mut app, KeyCode::Esc, at + after);
        assert!(!app.should_quit());
        assert_eq!(state.displayed_status(), Some(expected), "{after:?}");
    }
}

#[test]
fn a_pane_is_counted_once_whatever_it_is_read_as() {
    // Not idle and in the background are two answers about one pane, never both:
    // a background pane that took input is the not-idle one only.
    let (mut state, mut app, a, b, at) = quit_fixture();
    state.stage = Stage::Solo(a.clone());
    state.focus = Focus::Pane;
    sample(
        &mut state,
        &mut app,
        &[(&a, Background), (&b, Background)],
        at,
    );
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    back_to_the_list(&mut state, &mut app, at);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle, 1 in background: a (input), b (bg)")
    );
}

#[test]
fn the_prompt_window_and_its_drop_rules_are_the_same_for_a_background_command() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Background), (&b, Idle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(state.displayed_status().is_some());
    // The window runs out: the prompt goes and the next key is a first press.
    sample(
        &mut state,
        &mut app,
        &[(&a, Background), (&b, Idle)],
        at + QUIT_CONFIRM_WINDOW,
    );
    assert_eq!(state.displayed_status(), None);
    press(&mut state, &mut app, KeyCode::Esc, at + QUIT_CONFIRM_WINDOW);
    assert!(!app.should_quit());
    assert!(state.displayed_status().is_some(), "armed again");
    // Leaving the list drops it too.
    state.focus = Focus::Pane;
    sample(
        &mut state,
        &mut app,
        &[(&a, Background), (&b, Idle)],
        at + QUIT_CONFIRM_WINDOW + SECOND,
    );
    assert_eq!(state.displayed_status(), None);
}

// --- the quit question names what it counts ------------------------------------

/// A session row whose list title is `title`.
fn titled(id: &str, title: &str) -> SessionRow {
    SessionRow {
        title: Some(title.to_string()),
        ..row_for(id)
    }
}

/// The question the first quit key arms with, `None` when it quit at once.
fn asked(state: &mut EmporiumState, app: &mut App, at: Instant) -> Option<String> {
    press(state, app, KeyCode::Esc, at);
    if app.should_quit() {
        return None;
    }
    state.displayed_status().map(str::to_string)
}

#[test]
fn the_quit_question_names_each_session_it_counts_and_the_reason() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    let c = host_solo(&mut state, "c");
    app.replace_rows(vec![
        titled("a", "review"),
        titled("b", "build"),
        titled("c", "docs"),
    ]);
    sample(
        &mut state,
        &mut app,
        &[(&a, Unknown), (&b, NotIdle), (&c, Idle)],
        at,
    );
    // Idle `c` is not named: the question is about what a quit would end.
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some("Esc again to quit \u{2014} 2 not idle: review (unknown), build (busy)")
    );
}

#[test]
fn a_session_that_took_input_is_named_by_the_input_not_by_what_the_sample_read() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    app.replace_rows(vec![titled("a", "typed into"), titled("b", "working")]);
    // Both are read as busy, but only `a` was typed into: quit_counts counts it
    // by the input first, so the input is the reason it names.
    sample(&mut state, &mut app, &[(&a, NotIdle), (&b, NotIdle)], at);
    state.last_given_input.insert(a.clone(), at);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             typed into (input), working (busy)"
        )
    );
}

#[test]
fn a_pane_banto_typed_into_is_named_nudged_even_when_idle() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    state.last_prompted.insert(b.clone(), at);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some("Esc again to quit \u{2014} 1 not idle: b (nudged)")
    );
}

#[test]
fn the_operators_input_wins_over_banto_own_write_inside_the_same_window() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    app.replace_rows(vec![titled("a", "both"), titled("b", "banto only")]);
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    state.last_prompted.insert(a.clone(), at);
    state.last_given_input.insert(a.clone(), at);
    state.last_prompted.insert(b.clone(), at);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some("Esc again to quit \u{2014} 2 not idle: both (input), banto only (nudged)")
    );

    // An old operator key does not hide a fresh write of banto's: the window is
    // read per record.
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    let later = at + QUIT_RECENT_INPUT_WINDOW + SECOND;
    state.last_given_input.insert(a.clone(), at);
    state.last_prompted.insert(a.clone(), later);
    assert_eq!(
        asked(&mut state, &mut app, later).as_deref(),
        Some("Esc again to quit \u{2014} 1 not idle: a (nudged)")
    );
}

#[test]
fn a_background_session_is_named_in_its_own_group() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    let c = host_solo(&mut state, "c");
    app.replace_rows(vec![titled("a", "impl"), titled("b", "docs")]);
    sample(
        &mut state,
        &mut app,
        &[(&a, NotIdle), (&b, Background), (&c, Background)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 1 not idle, 2 in background: impl (busy), c (bg), docs (bg)"
        )
    );
    // Only background: the group stands alone.
    let (mut state, mut app, a, b, at) = quit_fixture();
    app.replace_rows(vec![titled("a", "docs")]);
    sample(&mut state, &mut app, &[(&a, Background), (&b, Idle)], at);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some("Esc again to quit \u{2014} 1 in background: docs")
    );
}

#[test]
fn a_brigade_member_is_named_by_its_display_name_else_its_token() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let named = host(&mut state, 1, "worker-1", "w1");
    let bare = host(&mut state, 1, "worker-2", "w2");
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "dir");
    // The list's own title for a Worker is not what a brigade calls it.
    app.replace_rows(vec![titled("w1", "a long transcript title")]);
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(
        &mut state,
        &mut app,
        &[(&named, NotIdle), (&bare, Unknown), (&director, NotIdle)],
        at,
    );
    // After the tick: it carries the store's roster, which replaces what was stated.
    state.state_member_name(1, "worker-1", Some("impl"));
    state.resolve_member_names(1);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 3 not idle: \
             worker-2 (unknown), director (busy), impl (busy)"
        )
    );
}

#[test]
fn a_pane_the_list_has_no_row_for_is_named_by_its_id_and_a_placeholder_as_a_new_session() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    let fresh = SessionKey::new_worker(1, "worker-3");
    state.screens.insert(fresh.clone(), Screen::new(24, 80));
    // Not the Worker's own name: the pane belongs to no brigade here.
    assert!(fresh.is_synthetic());
    sample(
        &mut state,
        &mut app,
        &[(&a, NotIdle), (&b, Idle), (&fresh, Unknown)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             new session (unknown), a (busy)"
        )
    );
}

#[test]
fn a_long_or_wide_name_is_cut_to_a_budget_and_a_long_list_is_not_cut() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    let long = "x".repeat(80);
    app.replace_rows(vec![titled("a", &long), titled("b", "\u{5b9f}\u{88c5}")]);
    let mut samples = vec![(a.clone(), NotIdle), (b.clone(), NotIdle)];
    for n in 0..4 {
        let pane = host_solo(&mut state, &format!("more-{n}"));
        samples.push((pane, NotIdle));
    }
    let borrowed: Vec<(&SessionKey, Quiescence)> = samples.iter().map(|(k, q)| (k, *q)).collect();
    sample(&mut state, &mut app, &borrowed, at);
    let text = asked(&mut state, &mut app, at).expect("asks");
    // Sorted by name: the four `more-N` ids, the cut title, then the wide one.
    // Every session is named: the status bar clips, the question does not.
    assert_eq!(
        text,
        "Esc again to quit \u{2014} 6 not idle: more-0 (busy), more-1 (busy), \
         more-2 (busy), more-3 (busy), xxxxxxxxxx\u{2026} (busy), \u{5b9f}\u{88c5} (busy)"
    );
    // The cut itself: one name alone shows the ellipsis within the budget.
    let (mut state, mut app, a, b, at) = quit_fixture();
    app.replace_rows(vec![titled("a", &long)]);
    sample(&mut state, &mut app, &[(&a, NotIdle), (&b, Idle)], at);
    let text = asked(&mut state, &mut app, at).expect("asks");
    let name = text
        .strip_prefix("Esc again to quit \u{2014} 1 not idle: ")
        .and_then(|rest| rest.strip_suffix(" (busy)"))
        .expect("one named session");
    assert!(name.ends_with('\u{2026}'), "{name:?}");
    assert!(
        unicode_width::UnicodeWidthStr::width(name) <= super::QUIT_NAME_COLUMNS,
        "{name:?}"
    );
}

#[test]
fn the_names_are_in_name_order_whichever_order_the_panes_were_hosted() {
    let mut texts = Vec::new();
    for ids in [["a", "b", "c"], ["c", "b", "a"], ["b", "c", "a"]] {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(Vec::new());
        let panes: Vec<SessionKey> = ids.iter().map(|id| host_solo(&mut state, id)).collect();
        // Two panes with one title are told apart by their ids, whichever came first.
        app.replace_rows(vec![
            titled("a", "same"),
            titled("b", "same"),
            titled("c", "alpha"),
        ]);
        state.focus = Focus::Sidebar;
        let at = t0();
        tick(&mut state, &mut app, at);
        let samples: Vec<(&SessionKey, Quiescence)> = panes.iter().map(|k| (k, NotIdle)).collect();
        sample(&mut state, &mut app, &samples, at);
        texts.push(asked(&mut state, &mut app, at).expect("asks"));
    }
    assert!(texts.windows(2).all(|pair| pair[0] == pair[1]), "{texts:?}");
    assert_eq!(
        texts[0],
        "Esc again to quit \u{2014} 3 not idle: \
         alpha (busy), same #a (busy), same #b (busy)"
    );
}

#[test]
fn the_names_are_in_order_of_diagnostic_value_unknown_input_nudged_busy_background() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    let c = host_solo(&mut state, "c");
    let d = host_solo(&mut state, "d");
    let e = host_solo(&mut state, "e");
    let f = host_solo(&mut state, "f");
    let g = host_solo(&mut state, "g");
    app.replace_rows(vec![
        titled("g", "g-nudged"),
        titled("a", "a-busy"),
        titled("b", "b-unknown"),
        titled("c", "c-bg"),
        titled("d", "d-input"),
        titled("e", "e-unknown"),
        titled("f", "f-busy"),
    ]);
    sample(
        &mut state,
        &mut app,
        &[
            (&a, NotIdle),
            (&b, Unknown),
            (&c, Background),
            (&d, Idle),
            (&e, Unknown),
            (&f, NotIdle),
            (&g, Idle),
        ],
        at,
    );
    state.last_given_input.insert(d.clone(), at);
    state.last_prompted.insert(g.clone(), at);
    // Both counts first, so a clipped line still says how much is at stake; then
    // one list, the least explained first, by name inside a reason.
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 6 not idle, 1 in background: b-unknown (unknown), \
             e-unknown (unknown), d-input (input), g-nudged (nudged), a-busy (busy), \
             f-busy (busy), c-bg (bg)"
        )
    );
}

#[test]
fn a_directors_row_title_names_it_and_a_director_without_a_row_is_named_by_its_token() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let titled_director = host(&mut state, 1, DIRECTOR_TOKEN, "dir1");
    let bare_director = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    app.replace_rows(vec![titled("dir1", "fix relay")]);
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(
        &mut state,
        &mut app,
        &[(&titled_director, NotIdle), (&bare_director, NotIdle)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             director (busy), fix relay (busy)"
        )
    );
}

#[test]
fn two_brigades_whose_directors_both_count_never_read_the_same() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let first = host(&mut state, 1, DIRECTOR_TOKEN, "dir1");
    let second = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let at = t0();
    state.focus = Focus::Sidebar;
    // Both sessions carry one title, as two Directors started from one prompt do.
    app.replace_rows(vec![titled("dir1", "same job"), titled("dir2", "same job")]);
    sample(
        &mut state,
        &mut app,
        &[(&first, NotIdle), (&second, Unknown)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             same job #2 (unknown), same job #1 (busy)"
        )
    );

    // No rows: both read `director`, and the brigade number tells them apart.
    let mut app = App::new(Vec::new());
    state.pending_quit = None;
    sample(
        &mut state,
        &mut app,
        &[(&first, NotIdle), (&second, NotIdle)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             director #1 (busy), director #2 (busy)"
        )
    );
}

#[test]
fn members_of_different_brigades_with_one_token_are_told_apart_by_the_brigade_number() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let one = host(&mut state, 1, "worker-1", "w1");
    let two = host(&mut state, 2, "worker-1", "w2");
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(
        &mut state,
        &mut app,
        &[(&one, NotIdle), (&two, NotIdle)],
        at,
    );
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             worker-1 #1 (busy), worker-1 #2 (busy)"
        )
    );
}

#[test]
fn where_the_brigade_number_would_not_tell_two_apart_the_session_id_does() {
    // One brigade's Director, titled `x`, and its Worker, named `x`: the same
    // name and the same brigade number.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "director-session-1234");
    let worker = host(&mut state, 1, "worker-1", "worker-session-5678");
    app.replace_rows(vec![titled("director-session-1234", "x")]);
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, NotIdle)],
        at,
    );
    state.state_member_name(1, "worker-1", Some("x"));
    state.resolve_member_names(1);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 2 not idle: \
             x #1234 (busy), x #5678 (busy)"
        )
    );
}

#[test]
fn a_suffix_fits_inside_the_name_budget_by_cutting_the_name_shorter() {
    // Two titles as wide as a name may be, and two longer ones that read the same
    // once cut: each pair collides, and the suffix is paid for out of the name.
    for (first, second) in [
        (
            "n".repeat(super::QUIT_NAME_COLUMNS),
            "n".repeat(super::QUIT_NAME_COLUMNS),
        ),
        ("n".repeat(30), "n".repeat(40)),
    ] {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(Vec::new());
        let a = host_solo(&mut state, "a");
        let b = host_solo(&mut state, "b");
        app.replace_rows(vec![titled("a", &first), titled("b", &second)]);
        state.focus = Focus::Sidebar;
        let at = t0();
        sample(&mut state, &mut app, &[(&a, NotIdle), (&b, NotIdle)], at);
        let text = asked(&mut state, &mut app, at).expect("asks");
        let cut = "n".repeat(super::QUIT_NAME_COLUMNS - " #a".len() - 1);
        assert_eq!(
            text,
            format!(
                "Esc again to quit \u{2014} 2 not idle: \
                 {cut}\u{2026} #a (busy), {cut}\u{2026} #b (busy)"
            )
        );
        let list = text
            .strip_prefix("Esc again to quit \u{2014} 2 not idle: ")
            .unwrap();
        for entry in list.split(", ") {
            let name = entry.strip_suffix(" (busy)").expect("a reason");
            assert!(
                unicode_width::UnicodeWidthStr::width(name) <= super::QUIT_NAME_COLUMNS,
                "{name:?}"
            );
        }
    }
}

#[test]
fn a_suffix_never_makes_a_name_read_like_another_sessions_untouched_name() {
    // Two brigades' Directors are both titled `review`, and an unrelated session is
    // already titled `review #1`: the brigade-number suffix would give the first
    // Director that very name.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let first = host(&mut state, 1, DIRECTOR_TOKEN, "dir1");
    let second = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let plain = host_solo(&mut state, "plain");
    app.replace_rows(vec![
        titled("dir1", "review"),
        titled("dir2", "review"),
        titled("plain", "review #1"),
    ]);
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(
        &mut state,
        &mut app,
        &[(&first, NotIdle), (&second, NotIdle), (&plain, NotIdle)],
        at,
    );
    // The session that was already called `review #1` keeps its name; the one
    // that would have collided with it moves on to its id.
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some(
            "Esc again to quit \u{2014} 3 not idle: \
             review #1 (busy), review #2 (busy), revi… #dir1 (busy)"
        )
    );
}

#[test]
fn the_armed_question_follows_what_the_next_sample_says_about_the_same_sessions() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    app.replace_rows(vec![titled("a", "review")]);
    sample(&mut state, &mut app, &[(&a, Unknown), (&b, Idle)], at);
    assert_eq!(
        asked(&mut state, &mut app, at).as_deref(),
        Some("Esc again to quit \u{2014} 1 not idle: review (unknown)")
    );
    sample(
        &mut state,
        &mut app,
        &[(&a, NotIdle), (&b, Idle)],
        at + SECOND,
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: review (busy)")
    );
}

// --- the closing line -------------------------------------------------------

#[test]
fn the_closing_text_counts_every_hosted_session_idle_or_not() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, NotIdle)], at);
    assert_eq!(
        state.closing_text(),
        "Closing \u{2014} ending 2 session(s)\u{2026}"
    );
    let empty = EmporiumState::new(PrefixKey::default());
    assert_eq!(empty.closing_text(), "Closing\u{2026}");
}

// --- a brigade that has gone quiet -------------------------------------------

/// Brigade 1 on stage — Director and one Worker — with the list focused (the
/// operator is not in the Director's pane), and the attention display seeded.
fn quiet_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "dir");
    let worker = host(&mut state, 1, "worker-1", "w1");
    stage(&mut state, 1, &director, &[&worker]);
    state.focus = Focus::Sidebar;
    let at = t0();
    tick(&mut state, &mut app, at);
    (state, app, director, worker, at)
}

const SECOND: Duration = Duration::from_secs(1);

/// The brigade is busy at `at`, then idle from `at + 1 s`: the stretch the
/// settle is counted from starts there.
fn busy_then_idle(
    state: &mut EmporiumState,
    app: &mut App,
    members: &[&SessionKey],
    at: Instant,
) -> Instant {
    let busy: Vec<_> = members
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, if i == 0 { NotIdle } else { Idle }))
        .collect();
    sample(state, app, &busy, at);
    let idle_from = at + SECOND;
    let idle: Vec<_> = members.iter().map(|k| (*k, Idle)).collect();
    sample(state, app, &idle, idle_from);
    idle_from
}

#[test]
fn the_brigade_rings_once_when_it_has_been_idle_for_the_settle() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::QuietRecorded {
            brigade: 1,
            director: director.clone()
        }],
        "a brigade whose Director is not focused: a record"
    );
    assert!(
        state.attention_panes().contains(&director),
        "the Director's tile carries the mark"
    );
    assert!(!state.attention_panes().contains(&worker));

    // Idle samples go on: nothing fires again.
    let later = idle_from + BRIGADE_QUIET_SETTLE + 10 * SECOND;
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        later,
    );
    assert!(!rang(&cmds), "{cmds:?}");
}

#[test]
fn it_does_not_ring_before_the_settle_has_passed() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE - Duration::from_millis(1),
    );
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().is_empty());
}

#[test]
fn a_member_that_turns_busy_inside_the_settle_starts_it_again() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    // The report reaches the Director: it turns busy before the settle ran out.
    let nudged = idle_from + 3 * SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle)],
        nudged,
    );
    // Idle again from the next second: the count starts over from there.
    let idle_again = nudged + SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_again,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_again + BRIGADE_QUIET_SETTLE - Duration::from_millis(1),
    );
    assert!(
        !rang(&cmds),
        "five seconds from the first idle would have fired"
    );

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_again + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn it_does_not_ring_again_until_someone_has_been_busy() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let fired_at = idle_from + BRIGADE_QUIET_SETTLE;
    let all_idle = [(&director, Idle), (&worker, Idle)];
    assert!(rang(&sample(&mut state, &mut app, &all_idle, fired_at)));

    // Unknown for a while, then idle again for far longer than the settle: a
    // brigade nobody has used since the last ring does not ring again.
    sample(
        &mut state,
        &mut app,
        &[(&director, Unknown), (&worker, Idle)],
        fired_at + SECOND,
    );
    for secs in 2..60 {
        let cmds = sample(&mut state, &mut app, &all_idle, fired_at + secs * SECOND);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    // Spend the mark so the check below sees a new one.
    state.latched_attention_panes.clear();
    state.attention.clear();

    // Someone works and the brigade goes quiet again: it rings again.
    let again = fired_at + 70 * SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        again,
    );
    sample(&mut state, &mut app, &all_idle, again + SECOND);
    let cmds = sample(
        &mut state,
        &mut app,
        &all_idle,
        again + SECOND + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn a_brigade_that_was_never_busy_does_not_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    for secs in 0..30 {
        let cmds = sample(&mut state, &mut app, &all_idle, at + secs * SECOND);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    assert!(state.attention_panes().is_empty());
}

#[test]
fn a_member_nothing_is_known_about_blocks_the_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle)],
        at,
    );
    // The Director is idle, the Worker is unreadable: for as long as that
    // holds, however long, nothing fires.
    for secs in 1..30 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Unknown)],
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    // It breaks the stretch too: the settle counts from when the Worker is
    // read as idle, not from before.
    let readable = at + 30 * SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        readable,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        readable + BRIGADE_QUIET_SETTLE - Duration::from_millis(1),
    );
    assert!(!rang(&cmds), "{cmds:?}");
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        readable + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn a_pane_without_any_sample_blocks_the_ring_like_an_unknown_one() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle)],
        at,
    );
    // The Worker is left out of every later sample.
    for secs in 1..20 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle)],
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
}

#[test]
fn with_the_directors_pane_focused_there_is_no_mark_and_no_bell() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.focus = Focus::Pane;
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().is_empty());

    // The ring was spent, not saved for when the operator leaves the pane.
    state.focus = Focus::Sidebar;
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE + 5 * SECOND,
    );
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().is_empty());
}

#[test]
fn another_pane_of_the_brigade_being_focused_does_not_stop_the_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.focus = Focus::Pane;
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(director.clone()),
        panes: vec![director.clone(), worker.clone()],
        focused: 1,
    };
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().contains(&director));
}

#[test]
fn the_mark_clears_when_the_operator_goes_to_the_directors_pane() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let fired = idle_from + BRIGADE_QUIET_SETTLE;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fired,
    );
    assert!(state.attention_panes().contains(&director));

    state.focus = Focus::Pane;
    tick(&mut state, &mut app, fired + SECOND);
    assert!(state.attention_panes().is_empty());
}

#[test]
fn a_waiting_member_counts_as_busy_so_the_brigade_rings_when_it_is_answered_and_quiet() {
    // The shell reads a waiting Claude as not idle, so to the brigade it is
    // activity like any other.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        at,
    );
    let idle_from = at + SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

// --- a background command is not idle for the watch, the marks or the relay ---

#[test]
fn a_background_command_holds_the_quiet_stretch_and_arms_the_ring_as_work_does() {
    for read_as in [NotIdle, Background] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        // The only thing the brigade ever did, held for much longer than the
        // settle: nothing rings while it lasts.
        let mut heard = Vec::new();
        for second in 0..SETTLED {
            heard.extend(sample(
                &mut state,
                &mut app,
                &[(&director, Idle), (&worker, read_as)],
                at + second * SECOND,
            ));
        }
        assert!(!rang(&heard), "{read_as:?}: {heard:?}");
        // It ends: the settle is counted from there, and the brigade then rings
        // once, for the stretch that background work armed.
        let idle_from = at + SETTLED * SECOND;
        let idle = [(&director, Idle), (&worker, Idle)];
        let cmds = sample(&mut state, &mut app, &idle, idle_from);
        assert!(!rang(&cmds), "{read_as:?}");
        let cmds = sample(
            &mut state,
            &mut app,
            &idle,
            idle_from + BRIGADE_QUIET_SETTLE - SECOND,
        );
        assert!(!rang(&cmds), "{read_as:?}: not before the settle");
        let cmds = sample(
            &mut state,
            &mut app,
            &idle,
            idle_from + BRIGADE_QUIET_SETTLE,
        );
        assert_eq!(
            bell_causes(&cmds),
            vec![BellCause::QuietRecorded {
                brigade: 1,
                director: director.clone()
            }],
            "{read_as:?}"
        );
    }
}

#[test]
fn a_background_command_turning_up_in_a_settling_brigade_starts_the_settle_again() {
    // The Worker's turn ends and the brigade begins to settle; then the Worker
    // reads as having a background command: the stretch is broken as by work.
    for read_as in [NotIdle, Background] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
        let idle = [(&director, Idle), (&worker, Idle)];
        sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, read_as)],
            idle_from + BRIGADE_QUIET_SETTLE - SECOND,
        );
        let cmds = sample(
            &mut state,
            &mut app,
            &idle,
            idle_from + BRIGADE_QUIET_SETTLE,
        );
        assert!(!rang(&cmds), "{read_as:?}: counted from here");
        let cmds = sample(
            &mut state,
            &mut app,
            &idle,
            idle_from + 2 * BRIGADE_QUIET_SETTLE,
        );
        assert!(rang(&cmds), "{read_as:?}");
    }
}

#[test]
fn a_request_spends_the_same_quiet_whether_the_stretch_was_work_or_a_background_command() {
    for read_as in [NotIdle, Background] {
        let (mut state, mut app, director, worker, at) = request_fixture();
        focus_the_director(&mut state);
        toggle_bell(&mut state, &mut app, at);
        let works = |state: &mut EmporiumState, app: &mut App, from: Instant, secs: u32| {
            let mut heard = Vec::new();
            for second in 0..secs {
                heard.extend(sample(
                    state,
                    app,
                    &[(&director, read_as), (&worker, Idle)],
                    from + second * SECOND,
                ));
            }
            heard
        };
        let mut heard = works(&mut state, &mut app, at + SECOND, 2);
        store_request(1);
        heard.extend(works(&mut state, &mut app, at + 3 * SECOND, 4));
        heard.extend(all_idle_for(
            &mut state,
            &mut app,
            &director,
            &worker,
            at + 7 * SECOND,
            SETTLED,
        ));
        assert_eq!(
            bell_causes(&heard),
            vec![the_request_ring(1, &director)],
            "{read_as:?}: one ring, for the request: {heard:?}"
        );
        forget_stored_requests();
    }
}

#[test]
fn a_background_command_raises_no_mark_count_or_bell_that_work_does_not() {
    let observed_with = |read_as| {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, read_as)],
            at,
        );
        (
            rang(&cmds),
            state.attention_panes().is_empty(),
            app.has_brigade_mark("dir"),
            app.is_waiting_id("w1"),
            state.displayed_activity(&app, &worker),
        )
    };
    let nothing = (false, true, false, false, None);
    assert_eq!(observed_with(NotIdle), nothing);
    assert_eq!(observed_with(Background), nothing);
}

#[test]
fn a_tile_word_is_idle_with_a_mark_only_for_a_background_command() {
    use crate::model::Activity::{Alive, Busy};
    for (read_as, word, background) in [
        (Idle, Some(Alive), false),
        (NotIdle, Some(Busy), false),
        (Background, Some(Alive), true),
        (Unknown, None, false),
    ] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, read_as)],
            at,
        );
        assert_eq!(state.pane_word(&app, &worker), word, "{read_as:?}");
        assert_eq!(
            state.pane_background(&app, &worker),
            background,
            "{read_as:?}"
        );
    }
}

#[test]
fn a_member_that_is_waiting_is_waiting_and_never_in_the_background() {
    // The live file says waiting this tick, and the sample (which cannot tell a
    // wait from work) says there is more: the word is waiting, and no mark.
    use crate::model::Activity::Waiting;
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let waiting = RelayObservation {
        live: Some(LiveStatus {
            activity: Waiting,
            dialog_open: false,
            status_busy: false,
        }),
        has_unseen: false,
        ..mail_for(1, "worker-1", &worker)
    };
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Background)],
        vec![waiting],
        at,
    );
    assert_eq!(state.pane_word(&app, &worker), Some(Waiting));
    assert!(!state.pane_background(&app, &worker));
}

/// The first second at which the relay types into the Worker's pane, with the
/// Worker's sample read as `read_as`.
fn first_nudge_when_read_as(read_as: Quiescence) -> Option<u64> {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.size = (120, 40);
    stage(&mut state, 1, &director, &[&worker]);
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    state.focus = Focus::Pane;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, read_as)],
        at,
    );
    (0..=RELAY_INPUT_QUIET_PERIOD.as_secs() + 5).find(|secs| {
        let cmds = send(
            &mut state,
            &mut app,
            Event::Tick {
                relay: vec![mail_for(1, "worker-1", &worker)],
                brigades: Vec::new(),
            },
            at + Duration::from_secs(*secs),
        );
        typed_into(&cmds, &worker)
    })
}

#[test]
fn the_relay_nudges_a_member_whatever_the_sample_says_including_a_background_command() {
    // What is idle enough to take a message is the live file's own word, decided
    // by the shell and carried in the observation; the sample the quit question
    // reads is not read for it.
    for read_as in [Idle, NotIdle, Background] {
        let at = first_nudge_when_read_as(read_as);
        assert!(at.is_some_and(|secs| secs < 5), "{read_as:?}: {at:?}");
    }
}

#[test]
fn two_brigades_settle_and_fire_independently() {
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");

    // Brigade 1 works and goes idle; brigade 2 works for longer.
    sample(
        &mut state,
        &mut app,
        &[(&d1, NotIdle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)],
        at,
    );
    let idle_1 = at + SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)],
        idle_1,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)],
        idle_1 + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "brigade 1 is quiet: {cmds:?}");
    assert!(state.attention.contains_key(&1));
    assert!(
        !state.attention.contains_key(&2),
        "brigade 2 is still working"
    );

    // Brigade 2 goes quiet five seconds after its own last busy sample.
    let idle_2 = idle_1 + BRIGADE_QUIET_SETTLE + SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)],
        idle_2,
    );
    assert!(!state.attention.contains_key(&2));
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)],
        idle_2 + BRIGADE_QUIET_SETTLE,
    );
    assert!(state.attention.contains_key(&2));
}

// --- an off-stage brigade, staged the way the product stages it ----------------------------------
//
// The only way to bring a brigade onto the stage is Enter (or a double click) on
// its Director's row: the shell is asked who the row is, answers with
// `Event::MembershipResolved`, and `stage_brigade` puts the focus on the first
// tile, the Director's, with the pane focused. The helpers below do exactly
// that; `stage()` above assigns the stage and leaves the focus where it was,
// which is right for a brigade that is already on stage with the operator back
// in the list (F2) and wrong for one that is being staged.

/// What an off-stage brigade (2: Director `dir2`, Worker `w2`) has to say before
/// the operator stages it.
#[derive(Clone, Copy, Debug)]
enum OffStageSays {
    /// The brigade went quiet after work: its Director's latch.
    QuietFire,
    /// A BEL from the target pane: that pane's latch.
    Bell,
    /// The target's session file says `waiting`.
    ClaudeWaiting,
    /// The target is Codex and reports `blocked`.
    CodexBlocked,
    /// The target's session file says `waiting` because the operator has a
    /// dialog open in it.
    ClaudeDialog,
    /// Brigade 1's Worker, which is on stage, starts waiting: it rings on its
    /// own tile, before anything is off stage.
    OnStageWait,
}

struct OffStage {
    says: OffStageSays,
    state: EmporiumState,
    app: App,
    d2: SessionKey,
    w2: SessionKey,
    target: SessionKey,
    at: Instant,
    /// Bells rung while the brigade was being set up off stage.
    rings_before: usize,
    /// Why each of them rang.
    causes_before: Vec<BellCause>,
}

fn rings(cmds: &[Cmd]) -> usize {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::RingHostBell { .. }))
        .count()
}

/// Why each bell in `cmds` was asked for, in order.
fn bell_causes(cmds: &[Cmd]) -> Vec<BellCause> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::RingHostBell { cause } => Some(cause.clone()),
            _ => None,
        })
        .collect()
}

impl OffStage {
    /// Brigade 1 on stage with the list focused, brigade 2 hosted off stage with
    /// only its Director's row in the list (its Worker is hidden, as members are),
    /// and `says` about `target`: the Director when `on_director`, else the Worker.
    fn new(says: OffStageSays, on_director: bool) -> Self {
        Self::new_with(says, on_director, false)
    }

    /// As [`Self::new`], with the operator `working` in brigade 1: the pane
    /// focus on its Director's tile and banto's window focused.
    fn new_with(says: OffStageSays, on_director: bool, working: bool) -> Self {
        use crate::model::Activity;
        let (mut state, mut app, d1, w1, at) = quiet_fixture();
        if working {
            state.focus = Focus::Pane;
            state.window_focused = true;
        }
        let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
        let w2 = host(&mut state, 2, "worker-1", "w2");
        let target = match says {
            OffStageSays::OnStageWait => w1.clone(),
            _ if on_director => d2.clone(),
            _ => w2.clone(),
        };
        let mut rows = vec![
            row_for("dir"),
            row_for("w1"),
            row_for("dir2"),
            row_for("w2"),
        ];
        for row in rows.iter_mut().filter(|row| row.id == target.as_str()) {
            match says {
                OffStageSays::CodexBlocked => row.agent = crate::model::AgentKind::Codex,
                OffStageSays::ClaudeWaiting => row.activity = Activity::Waiting,
                OffStageSays::ClaudeDialog => {
                    row.activity = Activity::Waiting;
                    row.dialog_open = true;
                }
                OffStageSays::OnStageWait => row.activity = Activity::Waiting,
                OffStageSays::QuietFire | OffStageSays::Bell => {}
            }
        }
        app.replace_rows(rows);
        app.set_hidden_member_ids(
            ["dir", "w1", "w2"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
        app.set_viewport_height(20);
        let all_idle = [(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)];
        let mut rings_before = 0;
        let mut causes_before = Vec::new();
        match says {
            OffStageSays::QuietFire => {
                let working = [(&d1, Idle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)];
                let mut heard = sample(&mut state, &mut app, &working, at);
                let idle_from = at + SECOND;
                heard.extend(sample(&mut state, &mut app, &all_idle, idle_from));
                let settled = idle_from + BRIGADE_QUIET_SETTLE;
                heard.extend(sample(&mut state, &mut app, &all_idle, settled));
                rings_before += rings(&heard);
                causes_before.extend(bell_causes(&heard));
            }
            OffStageSays::Bell => {
                let heard = said(&mut state, &mut app, &target, b"\x07", at + SECOND);
                rings_before += rings(&heard);
                causes_before.extend(bell_causes(&heard));
            }
            OffStageSays::ClaudeWaiting
            | OffStageSays::ClaudeDialog
            | OffStageSays::OnStageWait => {
                let heard = sample(&mut state, &mut app, &all_idle, at + SECOND);
                rings_before += rings(&heard);
                causes_before.extend(bell_causes(&heard));
            }
            OffStageSays::CodexBlocked => {
                let mut heard = sample(&mut state, &mut app, &all_idle, at + SECOND);
                heard.extend(said(
                    &mut state,
                    &mut app,
                    &target,
                    REPORT_BLOCKED,
                    at + 2 * SECOND,
                ));
                rings_before += rings(&heard);
                causes_before.extend(bell_causes(&heard));
            }
        }
        Self {
            says,
            state,
            app,
            d2,
            w2,
            target,
            at,
            rings_before,
            causes_before,
        }
    }

    /// What the one bell rung while this was set up must say it was for.
    fn expected_cause(&self) -> BellCause {
        let on_director = self.target == self.d2;
        match self.says {
            OffStageSays::QuietFire => BellCause::QuietRecorded {
                brigade: 2,
                director: self.d2.clone(),
            },
            OffStageSays::Bell => BellCause::OffStageMemberBell {
                member: self.target.clone(),
                brigade: 2,
                others: 0,
            },
            // A Director's own row is in the list, which is what rang.
            OffStageSays::ClaudeWaiting | OffStageSays::CodexBlocked if on_director => {
                BellCause::ListRowWaiting {
                    session: self.target.clone(),
                    others: 0,
                }
            }
            OffStageSays::ClaudeWaiting | OffStageSays::CodexBlocked => {
                BellCause::OffStageMemberWait {
                    member: self.target.clone(),
                    brigade: 2,
                    others: 0,
                }
            }
            OffStageSays::OnStageWait => BellCause::StagedPaneWait {
                pane: self.target.clone(),
                others: 0,
            },
            OffStageSays::ClaudeDialog => unreachable!("a dialog does not ring"),
        }
    }

    fn assert_it_rang_once_for_its_cause(&self, what: &str) {
        assert_eq!(
            self.causes_before,
            vec![self.expected_cause()],
            "{what}: why it rang"
        );
    }

    /// When the operator stages it: long after the setup, so the bell's cooldown
    /// is never what keeps a ring away.
    fn staged_at(&self) -> Instant {
        self.at + 60 * SECOND
    }

    /// Enter on the Director's row with the list focused, and the shell's answer:
    /// the update that stages the brigade, which is returned.
    fn stage_by_enter(&mut self, window_focused: bool, f4: bool) -> Vec<Cmd> {
        let roster = vec![
            (
                DIRECTOR_TOKEN.to_string(),
                BrigadeRole::Director,
                Some("dir2".to_string()),
            ),
            (
                "worker-1".to_string(),
                BrigadeRole::Worker,
                Some("w2".to_string()),
            ),
        ];
        self.stage_with_roster(window_focused, f4, roster)
    }

    fn stage_with_roster(
        &mut self,
        window_focused: bool,
        f4: bool,
        roster: Vec<(MemberToken, BrigadeRole, Option<String>)>,
    ) -> Vec<Cmd> {
        let t = self.staged_at();
        self.state.focus = Focus::Sidebar;
        self.state.bell_when_focused = f4;
        let asked = press(&mut self.state, &mut self.app, KeyCode::Enter, t);
        assert!(
            asked.iter().any(|cmd| matches!(
                cmd,
                Cmd::Store(StoreIntent::ResolveMembership { session_id }) if session_id == "dir2"
            )),
            "Enter asks who the row is: {asked:?}"
        );
        self.state.window_focused = window_focused;
        send(
            &mut self.state,
            &mut self.app,
            Event::MembershipResolved {
                session_id: "dir2".to_string(),
                membership: Some((2, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
                members: Some(roster),
                layout: None,
                names: Vec::new(),
            },
            t,
        )
    }

    /// The brigade is on stage with the Director's tile focused and the pane
    /// focused: where staging puts it, whatever was focused before.
    fn assert_focus_landed_on_the_director(&self) {
        match &self.state.stage {
            Stage::Brigade {
                id: 2,
                panes,
                focused: 0,
                ..
            } => assert_eq!(panes.first(), Some(&self.d2)),
            other => panic!("brigade 2 is not staged with focus 0: {other:?}"),
        }
        assert_eq!(self.state.focus, Focus::Pane);
    }

    fn marks(&self) -> Vec<String> {
        let mut marks: Vec<String> = self
            .state
            .attention_panes()
            .iter()
            .map(|key| key.as_str().to_string())
            .collect();
        marks.sort();
        marks
    }

    /// What is outstanding for the operator: the brigade's record for a quiet
    /// fire, the target pane's latch for a BEL.
    fn latched(&self) -> bool {
        match self.says {
            OffStageSays::QuietFire => self.state.attention.contains_key(&2),
            _ => self.state.latched_attention_panes.contains(&self.target),
        }
    }

    /// An ordinary tick `secs` after staging.
    fn tick_after_staging(&mut self, secs: u32) -> Vec<Cmd> {
        let at = self.staged_at() + secs * SECOND;
        tick(&mut self.state, &mut self.app, at)
    }

    fn window_regains_focus(&mut self, secs: u32) -> Vec<Cmd> {
        let at = self.staged_at() + secs * SECOND;
        send(
            &mut self.state,
            &mut self.app,
            Event::WindowFocusChanged { focused: true },
            at,
        )
    }
}

/// What an off-stage brigade's attention on its Director does, and what staging
/// does to it. Off stage it rings when it happens and marks the Director's row
/// (the row is in the list whether or not the brigade is on stage), and counts.
/// Staging it with the window focused reaches the Director's pane: the mark is
/// over in the update that stages it, nothing is drawn on the tile and nothing
/// rings again. Staging it with the window unfocused does not reach the pane, so
/// the mark stays (now on the tile) and nothing rings again, and it goes quietly
/// when the window regains focus. (This used to be a known defect: nothing
/// reached the operator off stage, and staging cleared the latch unseen.)
fn assert_an_off_stage_directors_attention_rings_at_once_and_ends_at_staging(says: OffStageSays) {
    for f4 in [false, true] {
        let mut off_stage = OffStage::new(says, true);
        assert!(off_stage.latched(), "{says:?}: outstanding off stage");
        assert!(off_stage.marks().is_empty(), "no tile is drawn for it");
        assert_eq!(
            off_stage.rings_before, 1,
            "{says:?}: it rang when it happened"
        );
        off_stage.assert_it_rang_once_for_its_cause(&format!("{says:?}"));
        assert!(
            off_stage.app.is_waiting_id("dir2"),
            "{says:?}: its row is marked"
        );
        assert_eq!(
            off_stage.state.visible_waiting_count(),
            1,
            "{says:?}: counted"
        );

        let staged = off_stage.stage_by_enter(true, f4);
        off_stage.assert_focus_landed_on_the_director();
        assert!(off_stage.marks().is_empty(), "{says:?} f4 {f4}: no mark");
        assert_eq!(rings(&staged), 0, "{says:?} f4 {f4}: no second bell");
        assert!(!off_stage.latched(), "{says:?} f4 {f4}: ended by staging");
        assert!(
            !off_stage.app.is_waiting_id("dir2"),
            "{says:?} f4 {f4}: row clear"
        );
        assert_eq!(off_stage.state.visible_waiting_count(), 0);
        for secs in [1, 30] {
            let cmds = off_stage.tick_after_staging(secs);
            assert!(off_stage.marks().is_empty());
            assert_eq!(rings(&cmds), 0, "{says:?} f4 {f4}: later tick +{secs}s");
        }
    }
    for f4 in [false, true] {
        let mut off_stage = OffStage::new(says, true);
        let staged = off_stage.stage_by_enter(false, f4);
        off_stage.assert_focus_landed_on_the_director();
        assert_eq!(off_stage.marks(), ["dir2"], "{says:?} f4 {f4}, unfocused");
        assert_eq!(rings(&staged), 0, "{says:?} f4 {f4}: it rang already");
        assert!(off_stage.latched());
        assert_eq!(rings(&off_stage.tick_after_staging(1)), 0, "once");
        let regained = off_stage.window_regains_focus(2);
        assert_eq!(rings(&regained), 0, "it goes quietly");
        assert!(off_stage.marks().is_empty());
        assert!(!off_stage.latched());
    }
}

#[test]
fn an_off_stage_brigades_quiet_fire_rings_at_once_and_staging_ends_its_mark() {
    assert_an_off_stage_directors_attention_rings_at_once_and_ends_at_staging(
        OffStageSays::QuietFire,
    );
}

#[test]
fn a_bell_latch_on_an_off_stage_director_rings_at_once_and_staging_ends_its_mark() {
    assert_an_off_stage_directors_attention_rings_at_once_and_ends_at_staging(OffStageSays::Bell);
}

#[test]
fn a_bell_latch_on_an_off_stage_worker_shows_and_rings_once_at_staging_and_stays_until_it_is_focused()
 {
    for window_focused in [true, false] {
        for f4 in [false, true] {
            let what = format!("window focused {window_focused}, f4 {f4}");
            let mut off_stage = OffStage::new(OffStageSays::Bell, false);
            assert!(off_stage.latched());
            assert!(off_stage.marks().is_empty());
            // Off stage it has no tile, so the Director's row speaks for it: marked,
            // counted, and rung for when the BEL came.
            assert_eq!(off_stage.rings_before, 1);
            off_stage.assert_it_rang_once_for_its_cause(&what);
            assert!(off_stage.app.is_waiting_id("dir2"));
            assert_eq!(off_stage.state.visible_waiting_count(), 1);

            let staged = off_stage.stage_by_enter(window_focused, f4);
            off_stage.assert_focus_landed_on_the_director();
            assert_eq!(off_stage.marks(), ["w2"], "{what}");
            // It rang at the BEL, through the Director's row; staging moves the
            // mark onto the tile and does not ring for the same BEL again.
            assert_eq!(rings(&staged), 0, "{what}");
            assert!(off_stage.latched(), "{what}");
            assert_eq!(rings(&off_stage.tick_after_staging(1)), 0, "{what}: once");
            assert_eq!(rings(&off_stage.tick_after_staging(30)), 0, "{what}: once");
            // The Director's pane has the focus, not the Worker's, so the window
            // coming back does not clear it either.
            off_stage.window_regains_focus(31);
            assert_eq!(off_stage.marks(), ["w2"], "{what}: still marked");

            // F3 moves the focus to the Worker's tile: seen, so cleared, quietly.
            let at = off_stage.staged_at() + 40 * SECOND;
            let cmds = press(&mut off_stage.state, &mut off_stage.app, KeyCode::F(3), at);
            assert!(off_stage.marks().is_empty(), "{what}: focused, so cleared");
            assert!(!off_stage.latched());
            assert_eq!(rings(&cmds), 0);
        }
    }
}

#[test]
fn an_off_stage_worker_that_is_waiting_or_blocked_shows_and_rings_once_at_staging() {
    for says in [OffStageSays::ClaudeWaiting, OffStageSays::CodexBlocked] {
        for window_focused in [true, false] {
            for f4 in [false, true] {
                let what = format!("{says:?}, window focused {window_focused}, f4 {f4}");
                let mut off_stage = OffStage::new(says, false);
                assert!(
                    off_stage.app.is_waiting_id("w2"),
                    "{what}: the row knows off stage"
                );
                // A hidden member is not in the list, and nothing draws it: its
                // Director's row speaks for it, marked, counted and rung for.
                assert!(off_stage.marks().is_empty(), "{what}");
                assert_eq!(off_stage.rings_before, 1, "{what}");
                off_stage.assert_it_rang_once_for_its_cause(&what);
                assert!(off_stage.app.is_waiting_id("dir2"), "{what}");
                assert_eq!(off_stage.state.visible_waiting_count(), 1, "{what}");

                let staged = off_stage.stage_by_enter(window_focused, f4);
                off_stage.assert_focus_landed_on_the_director();
                assert_eq!(off_stage.marks(), ["w2"], "{what}");
                assert_eq!(off_stage.state.visible_waiting_count(), 1, "{what}");
                // Rang when it began, through the Director's row, not again here.
                assert_eq!(rings(&staged), 0, "{what}");
                assert_eq!(rings(&off_stage.tick_after_staging(1)), 0, "{what}: once");
                assert_eq!(rings(&off_stage.tick_after_staging(30)), 0, "{what}: once");
            }
        }
    }
}

#[test]
fn an_off_stage_director_that_is_waiting_or_blocked_rang_from_the_list_and_shows_at_staging_without_a_second_bell()
 {
    for says in [OffStageSays::ClaudeWaiting, OffStageSays::CodexBlocked] {
        for window_focused in [true, false] {
            for f4 in [false, true] {
                let what = format!("{says:?}, window focused {window_focused}, f4 {f4}");
                let mut off_stage = OffStage::new(says, true);
                // The Director's row is in the list, so the list rang when it
                // turned up; nothing is drawn on stage for it yet.
                assert_eq!(off_stage.rings_before, 1, "{what}");
                off_stage.assert_it_rang_once_for_its_cause(&what);
                assert!(off_stage.marks().is_empty(), "{what}");

                let staged = off_stage.stage_by_enter(window_focused, f4);
                off_stage.assert_focus_landed_on_the_director();
                // Not a latch: the mark stays under the focus, as for any
                // waiting session, and it is the same attention, so no bell.
                assert_eq!(off_stage.marks(), ["dir2"], "{what}");
                assert_eq!(off_stage.state.visible_waiting_count(), 1, "{what}");
                assert_eq!(rings(&staged), 0, "{what}");
                assert_eq!(rings(&off_stage.tick_after_staging(1)), 0, "{what}");
                assert_eq!(rings(&off_stage.tick_after_staging(30)), 0, "{what}");
            }
        }
    }
}

#[test]
fn a_hosted_director_the_roster_has_no_session_for_is_still_staged_first_and_its_latch_still_cleared()
 {
    // `stage_brigade` reuses a member's live pane before it looks at the row or
    // the recorded session, so a Director that is hosted is staged whatever the
    // roster knows of it, first (the roster lists it first) and focused. The
    // brigade-quiet latch exists only for a hosted Director (a brigade whose
    // Director is not hosted is not watched), so there is no case of focus
    // landing on a Worker with the Director's latch outstanding.
    let mut off_stage = OffStage::new(OffStageSays::QuietFire, true);
    assert!(off_stage.latched());
    let roster = vec![
        (DIRECTOR_TOKEN.to_string(), BrigadeRole::Director, None),
        (
            "worker-1".to_string(),
            BrigadeRole::Worker,
            Some("w2".to_string()),
        ),
    ];
    let staged = off_stage.stage_with_roster(true, false, roster);
    off_stage.assert_focus_landed_on_the_director();
    assert!(
        matches!(
            &off_stage.state.stage,
            Stage::Brigade { director: None, .. }
        ),
        "no session recorded: no Director named on the stage"
    );
    assert!(off_stage.marks().is_empty());
    assert_eq!(rings(&staged), 0);
    assert!(!off_stage.latched());
    assert!(off_stage.w2 != off_stage.d2);
}

#[test]
fn a_brigade_whose_director_is_not_hosted_is_not_watched() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        at,
    );
    // The Director's process ends; the Worker goes idle and stays so.
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: director.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + SECOND,
    );
    for secs in 2..20 {
        let cmds = sample(&mut state, &mut app, &[(&worker, Idle)], at + secs * SECOND);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    assert!(state.latched_attention_panes.is_empty() && state.attention.is_empty());
}

// --- a Director's request for a decision --------------------------------------------

/// A staged brigade 1 (the operator in the list) after the first tick that visits
/// it, with nothing stored: the state a running banto is in when a Director calls
/// `request_decision`.
fn request_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    forget_stored_requests();
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir"), row_for("w1")]);
    app.set_viewport_height(20);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at,
    );
    (state, app, director, worker, at)
}

/// The operator is inside the Director's pane and banto's window is focused.
fn focus_the_director(state: &mut EmporiumState) {
    state.focus = Focus::Pane;
    state.window_focused = true;
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 0;
    }
}

fn request_off() -> BrigadeConfig {
    BrigadeConfig {
        request_attention: false,
        ..BrigadeConfig::default()
    }
}

fn tick_under(
    state: &mut EmporiumState,
    app: &mut App,
    config: &BrigadeConfig,
    at: Instant,
) -> Vec<Cmd> {
    let brigades = listed(state);
    let relay = observed(state, Vec::new());
    update(state, app, config, Event::Tick { relay, brigades }, at)
}

fn the_request_ring(brigade: BrigadeId, director: &SessionKey) -> BellCause {
    BellCause::Request {
        brigade,
        director: director.clone(),
    }
}

#[test]
fn the_first_sight_of_a_stored_request_draws_it_and_rings_and_marks_nothing() {
    // Restart: the request was stored by the last run, and the first tick that
    // visits the brigade is the one that reads it. Nothing happened just now.
    forget_stored_requests();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "dir");
    let worker = host(&mut state, 1, "worker-1", "w1");
    stage(&mut state, 1, &director, &[&worker]);
    state.focus = Focus::Sidebar;
    app.replace_rows(vec![row_for("dir"), row_for("w1")]);
    app.set_viewport_height(20);
    let at = t0();
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + SECOND);
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.has_request(&director));
    assert!(state.attention_panes().is_empty(), "no mark");
    assert!(!app.has_brigade_mark("dir"));
    assert_eq!(state.visible_waiting_count(), 0);
    // And it is not a new request on the next tick either.
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().is_empty());
}

#[test]
fn a_brigade_first_seen_with_no_request_does_not_make_a_later_one_a_first_sight() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert_eq!(bell_causes(&cmds), vec![the_request_ring(1, &director)]);
}

#[test]
fn a_new_request_marks_a_staged_directors_tile_rings_and_shows_the_tag() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert_eq!(
        bell_causes(&cmds),
        vec![the_request_ring(1, &director)],
        "{cmds:?}"
    );
    assert!(
        state.attention_panes().contains(&director),
        "the tile's mark"
    );
    assert!(app.has_brigade_mark("dir"), "and the row's");
    assert_eq!(state.visible_waiting_count(), 1);
    assert!(state.has_request(&director));
}

/// Brigade 1 on stage with the list focused, brigade 2 hosted off stage with only
/// its Director's row visible, both first seen with no request.
fn off_stage_request_fixture() -> (EmporiumState, App, SessionKey, Instant) {
    forget_stored_requests();
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");
    app.replace_rows(vec![
        row_for("dir"),
        row_for("w1"),
        row_for("dir2"),
        row_for("w2"),
    ]);
    app.set_hidden_member_ids(
        ["dir", "w1", "w2"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    app.set_viewport_height(20);
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)],
        at,
    );
    tick(&mut state, &mut app, at + SECOND);
    (state, app, d2, at + SECOND)
}

#[test]
fn a_request_from_a_brigade_off_stage_marks_its_row_and_rings_wherever_the_operator_is() {
    let (mut state, mut app, d2, at) = off_stage_request_fixture();
    store_request(2);
    let cmds = tick(&mut state, &mut app, at + SECOND);
    assert_eq!(
        bell_causes(&cmds),
        vec![the_request_ring(2, &d2)],
        "{cmds:?}"
    );
    assert!(app.has_brigade_mark("dir2"), "the Director's row is marked");
    assert_eq!(state.visible_waiting_count(), 1);
    assert!(state.has_request(&d2));
}

#[test]
fn a_request_while_the_operator_is_in_that_directors_pane_makes_no_mark_and_rings_only_with_f4() {
    for f4 in [false, true] {
        let (mut state, mut app, director, _worker, at) = request_fixture();
        focus_the_director(&mut state);
        if f4 {
            toggle_bell(&mut state, &mut app, at);
        }
        tick(&mut state, &mut app, at + SECOND);
        store_request(1);
        let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
        assert!(state.attention_panes().is_empty(), "f4 {f4}: no mark");
        assert!(!app.has_brigade_mark("dir"), "f4 {f4}");
        assert_eq!(state.visible_waiting_count(), 0, "f4 {f4}");
        assert_eq!(
            bell_causes(&cmds),
            if f4 {
                vec![the_request_ring(1, &director)]
            } else {
                Vec::new()
            },
            "f4 {f4}"
        );
        // The tag is shown all the same.
        assert!(state.has_request(&director), "f4 {f4}");
    }
}

#[test]
fn the_mark_ends_at_the_directors_pane_and_the_tag_stays_until_the_answer() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(app.has_brigade_mark("dir"));
    // The operator reaches the Director's pane: seen.
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 3 * SECOND);
    assert!(!app.has_brigade_mark("dir"), "looked at");
    assert!(state.attention_panes().is_empty());
    assert_eq!(
        state.visible_waiting_count(),
        0,
        "the tag alone is not counted"
    );
    assert!(state.has_request(&director), "not answered");
}

/// The samples of a stretch of work in progress: the Director not idle, the Worker
/// idle (a Director calls `request_decision` in the middle of a turn), one a second
/// from `from` for `secs` seconds. Returns every ring heard.
fn director_works(
    state: &mut EmporiumState,
    app: &mut App,
    director: &SessionKey,
    worker: &SessionKey,
    from: Instant,
    secs: u32,
) -> Vec<Cmd> {
    let mut heard = Vec::new();
    for second in 0..secs {
        heard.extend(sample(
            state,
            app,
            &[(director, NotIdle), (worker, Idle)],
            from + second * SECOND,
        ));
    }
    heard
}

/// Everything idle, one sample a second from `from` for `secs` seconds. Returns
/// every ring heard.
fn all_idle_for(
    state: &mut EmporiumState,
    app: &mut App,
    director: &SessionKey,
    worker: &SessionKey,
    from: Instant,
    secs: u32,
) -> Vec<Cmd> {
    let mut heard = Vec::new();
    for second in 0..secs {
        heard.extend(sample(
            state,
            app,
            &[(director, Idle), (worker, Idle)],
            from + second * SECOND,
        ));
    }
    heard
}

/// The brigade's settle, with margin.
const SETTLED: u32 = BRIGADE_QUIET_SETTLE.as_secs() as u32 + 4;

#[test]
fn a_request_made_in_the_middle_of_a_turn_swallows_the_quiet_that_ends_that_stretch() {
    // The operator is in the Director's pane with the bell switch on, so the
    // request rings (no mark). The Director then stays busy for several ticks, and
    // when everything is idle that is the quiet the request was about: not a
    // second ring.
    let (mut state, mut app, director, worker, at) = request_fixture();
    focus_the_director(&mut state);
    toggle_bell(&mut state, &mut app, at);
    let mut heard = director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    heard.extend(director_works(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 3 * SECOND,
        4,
    ));
    heard.extend(all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 7 * SECOND,
        SETTLED,
    ));
    assert_eq!(
        bell_causes(&heard),
        vec![the_request_ring(1, &director)],
        "one ring, for the request: {heard:?}"
    );
}

#[test]
fn a_request_whose_mark_was_looked_at_before_the_quiet_still_swallows_it() {
    // Off stage or not on it, the operator saw the mark and left: the quiet that
    // follows is the same stretch of work and is not announced again.
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    let cmds = director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    assert_eq!(bell_causes(&cmds), vec![the_request_ring(1, &director)]);
    assert!(app.has_brigade_mark("dir"));
    // The operator reaches the Director's pane (the mark ends) and leaves again.
    focus_the_director(&mut state);
    director_works(&mut state, &mut app, &director, &worker, at + 5 * SECOND, 2);
    state.focus = Focus::Sidebar;
    assert!(!app.has_brigade_mark("dir"));
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 7 * SECOND,
        SETTLED,
    );
    assert!(!rang(&heard), "swallowed: {heard:?}");
    assert!(!app.has_brigade_mark("dir"), "and no record");
    assert!(state.attention_panes().is_empty());
}

#[test]
fn once_the_operator_answers_the_next_stretch_is_his_and_its_end_rings() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    // He answers: an Enter into the Director's pane, then back to the list.
    focus_the_director(&mut state);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, at + 5 * SECOND);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Store(StoreIntent::ClearRequest { .. })))
    );
    state.focus = Focus::Sidebar;
    // The Director works on what he said and stops: that quiet rings.
    director_works(&mut state, &mut app, &director, &worker, at + 6 * SECOND, 3);
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 9 * SECOND,
        SETTLED,
    );
    assert_eq!(
        bell_causes(&heard),
        vec![BellCause::QuietRecorded {
            brigade: 1,
            director: director.clone()
        }],
        "{heard:?}"
    );
}

#[test]
fn a_key_the_operator_types_into_a_worker_ends_the_spending_too() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 5 * SECOND);
    // He tells a Worker something: from here the stretch is his.
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    press(&mut state, &mut app, KeyCode::Char('x'), at + 6 * SECOND);
    state.focus = Focus::Sidebar;
    director_works(&mut state, &mut app, &director, &worker, at + 7 * SECOND, 2);
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 9 * SECOND,
        SETTLED,
    );
    assert!(rang(&heard), "{heard:?}");
}

#[test]
fn what_banto_itself_types_into_a_worker_does_not_end_the_spending() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 5 * SECOND);
    state.focus = Focus::Sidebar;
    // The relay nudges the idle Worker (mail for it), and it works and stops.
    let mail = |state: &mut EmporiumState, app: &mut App, when: Instant| {
        send(
            state,
            app,
            Event::Tick {
                relay: vec![mail_for(1, "worker-1", &key("w1"))],
                brigades: listed(state),
            },
            when,
        )
    };
    mail(&mut state, &mut app, at + 6 * SECOND);
    let nudged = mail(&mut state, &mut app, at + 7 * SECOND);
    assert!(typed_into(&nudged, &worker), "{nudged:?}");
    let mut heard = Vec::new();
    for second in 0..3 {
        heard.extend(sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, NotIdle)],
            at + (8 + second) * SECOND,
        ));
    }
    heard.extend(all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 11 * SECOND,
        SETTLED,
    ));
    assert!(!rang(&heard), "still swallowed: {heard:?}");
}

#[test]
fn a_request_made_when_everything_is_already_idle_spends_only_the_next_quiet_point() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    // Long idle, nothing in progress.
    let heard = all_idle_for(&mut state, &mut app, &director, &worker, at + SECOND, 30);
    assert!(!rang(&heard));
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 31 * SECOND);
    assert_eq!(bell_causes(&cmds), vec![the_request_ring(1, &director)]);
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 32 * SECOND);
    state.focus = Focus::Sidebar;
    // The next quiet point passes in silence ...
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 33 * SECOND,
        SETTLED,
    );
    assert!(!rang(&heard), "{heard:?}");
    // ... and an unrelated stretch of work later rings as usual: nothing lingers.
    director_works(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 60 * SECOND,
        3,
    );
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 63 * SECOND,
        SETTLED,
    );
    assert_eq!(
        bell_causes(&heard),
        vec![BellCause::QuietRecorded {
            brigade: 1,
            director: director.clone()
        }],
        "{heard:?}"
    );
}

#[test]
fn a_second_request_spends_again() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    // The first quiet point is swallowed ...
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 5 * SECOND,
        SETTLED,
    );
    assert!(!rang(&heard), "{heard:?}");
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 20 * SECOND);
    state.focus = Focus::Sidebar;
    // ... and a request made in the next stretch swallows that one too.
    director_works(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 21 * SECOND,
        2,
    );
    store_request(1);
    director_works(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 23 * SECOND,
        2,
    );
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 25 * SECOND,
        SETTLED,
    );
    assert!(!rang(&heard), "{heard:?}");
}

#[test]
fn with_the_setting_off_the_tag_is_shown_and_the_quiet_rings_as_it_always_did() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    let config = request_off();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    let cmds = tick_under(&mut state, &mut app, &config, at + 3 * SECOND);
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(!app.has_brigade_mark("dir"));
    assert!(state.attention_panes().is_empty());
    assert_eq!(state.visible_waiting_count(), 0);
    assert!(state.has_request(&director));
    director_works(&mut state, &mut app, &director, &worker, at + 4 * SECOND, 2);
    let mut rang_quiet = false;
    for second in 0..SETTLED {
        let at_secs = at + (6 + second) * SECOND;
        update(
            &mut state,
            &mut app,
            &config,
            Event::QuiescenceSampled {
                samples: vec![(director.clone(), Idle), (worker.clone(), Idle)],
            },
            at_secs,
        );
        rang_quiet |= rang(&tick_under(&mut state, &mut app, &config, at_secs));
    }
    assert!(rang_quiet, "nothing was spent");
}

#[test]
fn a_request_from_a_brigade_whose_director_is_not_hosted_spends_nothing_that_could_linger() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    unhost(&mut state, &director);
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + SECOND);
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.request_spent.is_empty());
    // The Director is hosted again and a stretch of work goes quiet: it rings.
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "dir");
    director_works(&mut state, &mut app, &director, &worker, at + 5 * SECOND, 3);
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 8 * SECOND,
        SETTLED,
    );
    assert!(rang(&heard), "{heard:?}");
}

#[test]
fn a_directors_pane_ending_takes_the_spending_with_it_and_the_next_stretch_rings() {
    // Not a disband: the Director's process exits (a Director is never relaunched
    // by banto), and the operator reopens it from the list some time later.
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    assert!(state.request_spent.contains(&1), "spent");
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: director.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + 5 * SECOND,
    );
    assert!(!state.screens.contains_key(&director), "its pane is gone");
    sample(&mut state, &mut app, &[(&worker, Idle)], at + 6 * SECOND);
    assert!(
        state.request_spent.is_empty(),
        "no Director hosted, nothing is spent"
    );
    // Reopened later, it works and stops: that quiet is the operator's to hear.
    let director = host(&mut state, 1, DIRECTOR_TOKEN, "dir");
    director_works(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 60 * SECOND,
        3,
    );
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 63 * SECOND,
        SETTLED,
    );
    assert_eq!(
        bell_causes(&heard),
        vec![BellCause::QuietRecorded {
            brigade: 1,
            director: director.clone()
        }],
        "{heard:?}"
    );
}

#[test]
fn a_workers_pane_being_relaunched_keeps_the_spending_while_the_director_stays_hosted() {
    // The paths that drop a pane's screen: a process that ends (a Worker is then
    // relaunched under its key, a Director is not), a kill, a dismissal. A Worker
    // relaunch leaves the Director's pane hosted throughout, so the spending stays.
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    let cmds = send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + 5 * SECOND,
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { key, .. } if key == &worker)),
        "relaunched: {cmds:?}"
    );
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: worker.clone(),
            agent: None,
        },
        at + 5 * SECOND,
    );
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle)],
        at + 6 * SECOND,
    );
    assert!(state.request_spent.contains(&1), "still spent");
}

#[test]
fn disbanding_drops_what_a_request_spent() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    director_works(&mut state, &mut app, &director, &worker, at + SECOND, 2);
    store_request(1);
    director_works(&mut state, &mut app, &director, &worker, at + 3 * SECOND, 2);
    send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Ok((Default::default(), Default::default())),
        },
        at + 5 * SECOND,
    );
    assert!(state.request_spent.is_empty());
}

#[test]
fn a_request_made_before_the_first_tick_of_a_brigade_formed_in_this_run_is_a_new_one() {
    // The brigade is formed through the real path; its Director calls the tool
    // before the first gathering tick visits it. Only a request that already stood
    // when this run first saw the brigade is silent, and a brigade formed now had
    // none.
    forget_stored_requests();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row_for("dir")]);
    let config = BrigadeConfig::default();
    let at = t0();
    let mut cmds = update(
        &mut state,
        &mut app,
        &config,
        Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: std::path::PathBuf::from("/work"),
            worker_agent: crate::model::AgentKind::ClaudeCode,
            worker_model: "sonnet".to_string(),
            worker_effort: "high".to_string(),
            result: Ok((1, vec!["worker-1".to_string()])),
        },
        at,
    );
    while let Some(cmd) = cmds.pop() {
        if let Cmd::OpenEmbedded { key, .. } = cmd {
            cmds.extend(update(
                &mut state,
                &mut app,
                &config,
                Event::Spawned { key, agent: None },
                at,
            ));
        }
    }
    assert!(state.member_brigades.values().any(|id| *id == 1), "formed");
    state.focus = Focus::Sidebar;
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + SECOND);
    assert_eq!(
        bell_causes(&cmds),
        vec![the_request_ring(1, &key("dir"))],
        "{cmds:?}"
    );
    assert!(app.has_brigade_mark("dir"), "marked");
}

#[test]
fn a_second_request_rings_again_and_a_repeat_of_the_same_does_not() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert_eq!(bell_causes(&cmds), vec![the_request_ring(1, &director)]);
    // The same stored request seen again is not a new one.
    let cmds = tick(&mut state, &mut app, at + 3 * SECOND);
    assert!(!rang(&cmds));
    // A second call, with the mark still standing, is a new request and rings
    // again once the shared cooldown has passed.
    store_request(1);
    let cmds = tick(
        &mut state,
        &mut app,
        at + 2 * SECOND + ATTENTION_BELL_COOLDOWN,
    );
    assert_eq!(
        bell_causes(&cmds),
        vec![the_request_ring(1, &director)],
        "{cmds:?}"
    );
    assert!(state.has_request(&director));
    assert!(app.has_brigade_mark("dir"));
}

#[test]
fn a_quiet_fire_while_a_request_is_unanswered_but_looked_at_rings_as_usual() {
    let (mut state, mut app, director, worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    tick(&mut state, &mut app, at + 2 * SECOND);
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 3 * SECOND);
    assert!(!app.has_brigade_mark("dir"));
    // The operator goes back to the list. The brigade was idle when the request was
    // made, so the quiet point that comes first is the one the request spent.
    state.focus = Focus::Sidebar;
    let heard = all_idle_for(
        &mut state,
        &mut app,
        &director,
        &worker,
        at + 4 * SECOND,
        SETTLED,
    );
    assert!(!rang(&heard), "{heard:?}");
    // Then the brigade works and goes quiet: that rings as usual.
    let idle_from = busy_then_idle(
        &mut state,
        &mut app,
        &[&worker, &director],
        at + 20 * SECOND,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::QuietRecorded {
            brigade: 1,
            director: director.clone()
        }]
    );
    assert!(state.has_request(&director), "still unanswered");
}

/// A request the operator has looked at: the tag stands, the mark is gone.
fn a_request_that_stands_unanswered() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let (mut state, mut app, director, worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    tick(&mut state, &mut app, at + 2 * SECOND);
    focus_the_director(&mut state);
    tick(&mut state, &mut app, at + 3 * SECOND);
    (state, app, director, worker, at + 3 * SECOND)
}

#[test]
fn only_an_enter_into_the_directors_pane_answers_it_and_clears_the_store() {
    let (mut state, mut app, director, _worker, at) = a_request_that_stands_unanswered();
    let cmds = press(&mut state, &mut app, KeyCode::Enter, at + SECOND);
    assert!(wrote_to(&cmds, &director), "the Enter went to the pane");
    assert!(!state.has_request(&director), "answered");
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::ClearRequest {
                brigade_id: 1,
                seq: 1
            })
        )),
        "{cmds:?}"
    );
    run_store(&cmds);
    // The store was cleared, and the next tick has nothing to bring back.
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(!rang(&cmds));
    assert!(!state.has_request(&director));
}

#[test]
fn nothing_but_an_enter_answers_it() {
    // A letter, a paste, Shift+Enter (the child receives a line feed, not a send),
    // the wheel, a click, a focus change, and banto's own nudge with its Enter.
    // What the operator (or banto) does in the Director's pane.
    type Act = fn(&mut EmporiumState, &mut App, &SessionKey, Instant);
    let cases: [(&str, Act); 6] = [
        ("a key", |state, app, _, at| {
            press(state, app, KeyCode::Char('x'), at);
        }),
        ("a paste", |state, app, _, at| {
            send(
                state,
                app,
                Event::Input(InputEvent::Paste("one\ntwo\r\n".to_string())),
                at,
            );
        }),
        ("shift+enter", |state, app, _, at| {
            press_with(state, app, KeyCode::Enter, Modifiers::SHIFT, at);
        }),
        ("the pointer", |state, app, director, at| {
            state.size = (120, 40);
            said(state, app, director, MOUSE_REPORTING_ON, at);
            let over = inside(state, director);
            for kind in [
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
            ] {
                mouse(state, app, kind, over, at);
            }
        }),
        ("a focus change", |state, app, _, at| {
            state.focus = Focus::Sidebar;
            tick(state, app, at);
            focus_the_director(state);
            send(state, app, Event::WindowFocusChanged { focused: false }, at);
            send(state, app, Event::WindowFocusChanged { focused: true }, at);
        }),
        ("banto's nudge", |state, app, director, at| {
            // Mail waits for the Director; two idle ticks nudge it, and the Enter
            // that submits the nudge follows a moment later.
            let mail = |state: &mut EmporiumState, app: &mut App, when: Instant| {
                send(
                    state,
                    app,
                    Event::Tick {
                        relay: vec![mail_for(1, DIRECTOR_TOKEN, director)],
                        brigades: listed(state),
                    },
                    when,
                )
            };
            // The operator is not typing into it: the nudge is free to go out.
            state.focus = Focus::Sidebar;
            mail(state, app, at);
            let nudged = mail(state, app, at + SECOND);
            assert!(typed_into(&nudged, director), "{nudged:?}");
            let submitted = mail(state, app, at + 2 * SECOND);
            assert!(typed_into(&submitted, director), "its Enter: {submitted:?}");
        }),
    ];
    for (what, act) in cases {
        let (mut state, mut app, director, _worker, at) = a_request_that_stands_unanswered();
        act(&mut state, &mut app, &director, at + SECOND);
        assert!(state.has_request(&director), "{what} does not answer it");
        assert_eq!(
            stored_request(1).map(|fact| fact.seq),
            Some(1),
            "{what}: the store was not cleared"
        );
    }
}

#[test]
fn an_enter_into_another_pane_does_not_answer_the_directors_request() {
    let (mut state, mut app, director, worker, at) = a_request_that_stands_unanswered();
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    let cmds = press(&mut state, &mut app, KeyCode::Enter, at + SECOND);
    assert!(wrote_to(&cmds, &worker));
    assert!(state.has_request(&director));
}

#[test]
fn a_late_or_failed_clear_cannot_bring_an_answered_request_back_and_a_newer_one_still_counts() {
    let (mut state, mut app, director, _worker, at) = a_request_that_stands_unanswered();
    let cmds = press(&mut state, &mut app, KeyCode::Enter, at + SECOND);
    // The clear never reached the store (not run): the next tick still carries
    // the request it had, the one that was answered.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Store(StoreIntent::ClearRequest { .. })))
    );
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(!rang(&cmds));
    assert!(!state.has_request(&director), "not resurrected");
    assert!(!app.has_brigade_mark("dir"));
    // A request made after it is a new one, whatever happened to the clear.
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 3 * SECOND);
    assert!(state.has_request(&director));
    // The window is focused on the Director, so no mark; and with F4 off no bell.
    assert!(!rang(&cmds), "{cmds:?}");
}

#[test]
fn disbanding_the_brigade_drops_its_request_and_everything_kept_for_it() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    store_request(1);
    tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(app.has_brigade_mark("dir"));
    send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Ok((Default::default(), Default::default())),
        },
        at + 3 * SECOND,
    );
    assert!(!state.has_request(&director));
    assert!(!app.has_brigade_mark("dir"));
    assert!(state.attention.is_empty());
    assert!(state.requests.is_empty() && state.request_seen.is_empty());
}

#[test]
fn a_request_from_a_brigade_whose_director_is_not_hosted_is_the_tag_alone() {
    let (mut state, mut app, director, _worker, at) = request_fixture();
    tick(&mut state, &mut app, at + SECOND);
    unhost(&mut state, &director);
    store_request(1);
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention.is_empty());
}

fn request_on_row(app: &App, id: &str) -> bool {
    app.visible().into_iter().any(|line| match line {
        crate::app::ListLine::Row(row) => row.row.id == id && row.request,
        _ => false,
    })
}

/// Stage brigade 2 from the list, the way the operator does: Enter on the
/// Director's row, then the membership the engine resolves for it.
fn stage_brigade_two(state: &mut EmporiumState, app: &mut App, at: Instant) {
    let roster = vec![
        (
            DIRECTOR_TOKEN.to_string(),
            BrigadeRole::Director,
            Some("dir2".to_string()),
        ),
        (
            "worker-1".to_string(),
            BrigadeRole::Worker,
            Some("w2".to_string()),
        ),
    ];
    state.focus = Focus::Sidebar;
    press(state, app, KeyCode::Enter, at);
    send(
        state,
        app,
        Event::MembershipResolved {
            session_id: "dir2".to_string(),
            membership: Some((2, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
            members: Some(roster),
            layout: None,
            names: Vec::new(),
        },
        at,
    );
}

#[test]
fn a_directors_row_carries_the_tag_whether_its_brigade_is_on_stage_or_not() {
    let (mut state, mut app, d2, at) = off_stage_request_fixture();
    store_request(2);
    tick(&mut state, &mut app, at + SECOND);
    assert!(request_on_row(&app, "dir2"), "off stage: on its row");
    assert!(app.has_brigade_mark("dir2"), "off stage: marked");
    assert_eq!(state.visible_waiting_count(), 1, "off stage: counted");
    // Brigade 1 is on stage and has no request; staging brigade 2 puts the tag on
    // its tile and the row keeps it: the list is the overview of every brigade, so
    // what a row says does not depend on which one is on stage.
    stage_brigade_two(&mut state, &mut app, at + 2 * SECOND);
    assert!(state.has_request(&d2), "still unanswered");
    assert!(request_on_row(&app, "dir2"), "staged: still on its row");
    // The tag is not a mark and is not counted: the staged pane speaks through its
    // own tile, so the row's mark and the `waiting N` count are gone as they were.
    assert!(!app.has_brigade_mark("dir2"), "staged: no mark");
    assert_eq!(state.visible_waiting_count(), 0, "staged: not counted");
    assert_eq!(app.filtered_waiting_count(), 0);
}

#[test]
fn a_solo_staged_pane_of_a_requesting_brigade_leaves_the_row_as_it_was() {
    let (mut state, mut app, d2, at) = off_stage_request_fixture();
    store_request(2);
    tick(&mut state, &mut app, at + SECOND);
    assert!(request_on_row(&app, "dir2"));
    // The Director's session opened on its own, with no brigade on stage.
    state.stage = Stage::Solo(d2.clone());
    tick(&mut state, &mut app, at + 2 * SECOND);
    assert!(request_on_row(&app, "dir2"), "solo: still on its row");
    assert!(state.has_request(&d2));
}

#[test]
fn a_row_hidden_by_a_search_has_no_tag_to_draw_and_gets_it_back_when_the_search_ends() {
    let (mut state, mut app, _d2, at) = off_stage_request_fixture();
    store_request(2);
    tick(&mut state, &mut app, at + SECOND);
    assert!(request_on_row(&app, "dir2"));
    state.focus = Focus::Sidebar;
    press(&mut state, &mut app, KeyCode::Char('/'), at + 2 * SECOND);
    for c in "zzz".chars() {
        press(&mut state, &mut app, KeyCode::Char(c), at + 2 * SECOND);
    }
    assert_eq!(app.filtered_len(), 0, "the search hides every row");
    assert!(
        !request_on_row(&app, "dir2"),
        "hidden: nothing to draw it on"
    );
    press(&mut state, &mut app, KeyCode::Esc, at + 3 * SECOND);
    assert!(request_on_row(&app, "dir2"), "search over: back on its row");
}

#[test]
fn a_brigade_nothing_hosts_yet_shows_its_stored_request_on_its_directors_row_with_no_effect() {
    // Restart: the brigade is in the store and nothing is hosted. The roster names
    // the Director's session, which is the row.
    forget_stored_requests();
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row_for("dir9")]);
    app.set_viewport_height(20);
    store_request(9);
    let facts = BrigadeNameFacts {
        brigade_id: 9,
        name: "cell".to_string(),
        director_session: Some("dir9".to_string()),
        member_names: Vec::new(),
        request: stored_request(9),
        interrupts: None,
    };
    let cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts],
        },
        t0(),
    );
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(request_on_row(&app, "dir9"));
    assert!(!app.has_brigade_mark("dir9"));
    assert_eq!(app.filtered_waiting_count(), 0);
}

#[test]
fn the_request_ring_reads_as_one_line_for_the_bell_log() {
    assert_eq!(
        the_request_ring(7, &key("dir")).to_string(),
        "request brigade=7 director=dir"
    );
    let hostile = BellCause::Request {
        brigade: 7,
        director: SessionKey::from_id("a\nb"),
    }
    .to_string();
    assert_eq!(hostile.lines().count(), 1, "{hostile:?}");
}

// --- what a bell says it was for ----------------------------------------------------

#[test]
fn waits_that_begin_in_one_update_ring_once_naming_the_lowest_id_and_counting_the_rest() {
    use crate::model::Activity;
    // Off stage: two members of brigade 2 begin waiting in the same reload.
    let (mut state, mut app, _d1, _w1, at) = quiet_fixture();
    host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    host(&mut state, 2, "worker-1", "w3");
    host(&mut state, 2, "worker-2", "w2");
    let rows = |activity| {
        let mut rows = vec![
            row_for("dir"),
            row_for("w1"),
            row_for("dir2"),
            row_for("w2"),
            row_for("w3"),
        ];
        for row in rows.iter_mut().filter(|r| r.id == "w2" || r.id == "w3") {
            row.activity = activity;
        }
        rows
    };
    app.replace_rows(rows(Activity::Alive));
    app.set_hidden_member_ids(
        ["dir", "w1", "w2", "w3"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    app.set_viewport_height(20);
    tick(&mut state, &mut app, at + SECOND);
    app.replace_rows(rows(Activity::Waiting));
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::OffStageMemberWait {
            member: key("w2"),
            brigade: 2,
            others: 1
        }],
        "{cmds:?}"
    );

    // In the list: two rows nothing hosts begin waiting in the same reload.
    let (mut state, mut app, _d1, _w1, at) = quiet_fixture();
    app.replace_rows(vec![row_for("x2"), row_for("x1")]);
    app.set_viewport_height(20);
    tick(&mut state, &mut app, at + SECOND);
    let mut waiting = vec![row_for("x2"), row_for("x1")];
    for row in &mut waiting {
        row.activity = Activity::Waiting;
    }
    app.replace_rows(waiting);
    let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::ListRowWaiting {
            session: key("x1"),
            others: 1
        }],
        "{cmds:?}"
    );
}

#[test]
fn a_new_plain_session_is_named_in_a_cause_without_its_working_directory() {
    // The placeholder key of a session opened new carries its cwd; the log line
    // keeps its kind and counter and nothing else of it.
    let cwd = std::path::Path::new("/work/secret\nproject::7");
    let pane = SessionKey::new_plain(cwd, 3);
    assert_eq!(pane.log_id(), "new::3");

    // Through real events: that session is on stage, the window is away, and the
    // child rings BEL.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    state.screens.insert(pane.clone(), Screen::new(24, 80));
    state.stage = Stage::Solo(pane.clone());
    state.focus = Focus::Pane;
    let at = t0();
    send(
        &mut state,
        &mut app,
        Event::WindowFocusChanged { focused: false },
        at,
    );
    let cmds = said(&mut state, &mut app, &pane, b"\x07", at + SECOND);
    let causes = bell_causes(&cmds);
    assert_eq!(
        causes,
        vec![BellCause::StagedPaneBell {
            pane: pane.clone(),
            others: 0
        }]
    );
    let text = causes[0].to_string();
    assert_eq!(text, "staged_pane_bell pane=new::3 others=0");
    assert!(!text.contains("secret") && !text.contains('/'), "{text}");

    // The same for a wait.
    let wait = BellCause::StagedPaneWait { pane, others: 0 };
    assert_eq!(wait.to_string(), "staged_pane_wait pane=new::3 others=0");
}

#[test]
fn nothing_a_key_can_contain_puts_a_line_break_or_a_control_character_in_a_cause() {
    let hostile = [
        "a\nb",
        "a\r\nb",
        "tab\there",
        "nul\0here",
        "esc\x1b[31mred",
        "next\u{85}line",
        "sep\u{2028}line",
        "new::1\nnext",
        "new-worker::1::wor\nker",
        "new::/work/\n::x",
    ];
    for id in hostile {
        let pane = SessionKey::from_id(id);
        let causes = [
            BellCause::QuietFocused {
                brigade: 1,
                director: pane.clone(),
            },
            BellCause::QuietRecorded {
                brigade: 1,
                director: pane.clone(),
            },
            BellCause::StagedPaneBell {
                pane: pane.clone(),
                others: 0,
            },
            BellCause::StagedPaneWait {
                pane: pane.clone(),
                others: 0,
            },
            BellCause::ListRowWaiting {
                session: pane.clone(),
                others: 0,
            },
            BellCause::OffStageMemberBell {
                member: pane.clone(),
                brigade: 1,
                others: 0,
            },
            BellCause::OffStageMemberWait {
                member: pane.clone(),
                brigade: 1,
                others: 0,
            },
        ];
        for cause in causes {
            let line = cause.to_string();
            assert!(
                !line
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() && c != ' '),
                "{id:?} -> {line:?}"
            );
            assert_eq!(line.lines().count(), 1, "{id:?}");
        }
    }
    // A placeholder whose counter is not one is not trusted to be a counter.
    let odd = SessionKey::new_plain(std::path::Path::new("/work/x"), 1);
    assert_eq!(odd.log_id(), "new::1");
    let mangled = SessionKey::from_id("new::/work/x::notanumber");
    assert_eq!(mangled.log_id(), "new::?");
    // And nothing makes a line longer than an id has any need of.
    let long = SessionKey::from_id(&"x".repeat(500));
    assert_eq!(long.log_id().len(), 80);
}

#[test]
fn every_cause_reads_as_one_line_of_ids_and_counts() {
    let dir = key("dir");
    let lines: Vec<String> = [
        BellCause::QuietFocused {
            brigade: 4,
            director: dir.clone(),
        },
        BellCause::QuietRecorded {
            brigade: 4,
            director: dir.clone(),
        },
        BellCause::StagedPaneBell {
            pane: dir.clone(),
            others: 2,
        },
        BellCause::StagedPaneWait {
            pane: dir.clone(),
            others: 0,
        },
        BellCause::ListRowWaiting {
            session: dir.clone(),
            others: 1,
        },
        BellCause::OffStageMemberBell {
            member: dir.clone(),
            brigade: 4,
            others: 0,
        },
        BellCause::OffStageMemberWait {
            member: dir.clone(),
            brigade: 4,
            others: 3,
        },
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        lines,
        [
            "quiet_focused brigade=4 director=dir",
            "quiet_recorded brigade=4 director=dir",
            "staged_pane_bell pane=dir others=2",
            "staged_pane_wait pane=dir others=0",
            "list_row_waiting session=dir others=1",
            "off_stage_member_bell member=dir brigade=4 others=0",
            "off_stage_member_wait member=dir brigade=4 others=3",
        ]
    );
}

#[test]
fn the_event_survives_a_recording_round_trip() {
    let event = Event::QuiescenceSampled {
        samples: vec![(key("a"), Idle), (key("b"), NotIdle), (key("c"), Unknown)],
    };
    let text = serde_json::to_string(&event).unwrap();
    assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
}

// --- the bell switch ------------------------------------------------------------

fn press_with(
    state: &mut EmporiumState,
    app: &mut App,
    code: KeyCode,
    mods: Modifiers,
    at: Instant,
) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Input(InputEvent::Key(KeyEvent::new(code, mods))),
        at,
    )
}

/// F4, as the operator presses it.
fn toggle_bell(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    press_with(state, app, KeyCode::F(4), Modifiers::NONE, at)
}

fn saves(cmds: &[Cmd]) -> Vec<bool> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::Store(StoreIntent::SaveBellWhenFocused { on }) => Some(*on),
            _ => None,
        })
        .collect()
}

/// The quiet fixture with the operator inside the Director's pane, and the
/// brigade busy then idle: a sample at the returned instant fires it.
fn focused_director_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.focus = Focus::Pane;
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    (
        state,
        app,
        director,
        worker,
        idle_from + BRIGADE_QUIET_SETTLE,
    )
}

#[test]
fn the_bell_switch_is_off_by_default() {
    let state = EmporiumState::new(PrefixKey::default());
    assert!(!state.bell_when_focused());
}

#[test]
fn f4_flips_the_switch_from_the_list_or_a_pane_and_says_so_in_words() {
    for focus in [Focus::Sidebar, Focus::Pane] {
        let (mut state, mut app, _director, _worker, at) = quiet_fixture();
        state.focus = focus;

        toggle_bell(&mut state, &mut app, at);
        assert!(state.bell_when_focused(), "{focus:?}");
        let on = state.displayed_status().unwrap_or_default().to_string();
        assert!(on.starts_with("brigade bell: on"), "{on:?}");

        toggle_bell(&mut state, &mut app, at + SECOND);
        assert!(!state.bell_when_focused(), "{focus:?}");
        let off = state.displayed_status().unwrap_or_default().to_string();
        assert!(off.starts_with("brigade bell: off"), "{off:?}");
    }
}

#[test]
fn f4_is_taken_by_banto_and_never_reaches_the_pane() {
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    state.focus = Focus::Pane;
    for mods in [Modifiers::NONE, Modifiers::SHIFT] {
        let cmds = press_with(&mut state, &mut app, KeyCode::F(4), mods, at);
        assert!(
            !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            "{mods:?}: {cmds:?}"
        );
    }
    assert!(!state.bell_when_focused(), "two presses: on, then off");
}

#[test]
fn prefix_r_no_longer_toggles_and_is_an_unbound_prefix_key() {
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    state.focus = Focus::Pane;
    press_with(
        &mut state,
        &mut app,
        KeyCode::Char('b'),
        Modifiers::CONTROL,
        at,
    );
    let cmds = press_with(
        &mut state,
        &mut app,
        KeyCode::Char('r'),
        Modifiers::NONE,
        at,
    );
    assert!(!state.bell_when_focused());
    assert_eq!(state.displayed_status(), Some("unbound prefix key"));
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
        "swallowed, not forwarded: {cmds:?}"
    );
    assert!(saves(&cmds).is_empty());
}

#[test]
fn a_modal_the_search_box_or_an_armed_prefix_takes_f4_first_as_it_does_f2() {
    // A modal open.
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    app.open_confirm_kill_modal("w1".to_string(), "w1".to_string(), None);
    toggle_bell(&mut state, &mut app, at);
    assert!(!state.bell_when_focused(), "modal");
    assert!(app.modal().is_some(), "and the modal is still up");

    // The search box open.
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    app.enter_search();
    toggle_bell(&mut state, &mut app, at);
    assert!(!state.bell_when_focused(), "search");

    // The prefix armed: F4 is a prefix follow-up like any other key, so it is
    // unbound there.
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    press_with(
        &mut state,
        &mut app,
        KeyCode::Char('b'),
        Modifiers::CONTROL,
        at,
    );
    toggle_bell(&mut state, &mut app, at);
    assert!(!state.bell_when_focused(), "armed prefix");
    assert_eq!(state.displayed_status(), Some("unbound prefix key"));
}

#[test]
fn toggling_writes_the_preference_once_per_change() {
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    assert_eq!(saves(&toggle_bell(&mut state, &mut app, at)), vec![true]);
    // Nothing further to write while nothing changes.
    assert!(saves(&tick(&mut state, &mut app, at + SECOND)).is_empty());
    assert_eq!(
        saves(&toggle_bell(&mut state, &mut app, at + 2 * SECOND)),
        vec![false]
    );
}

#[test]
fn a_loaded_preference_is_applied_and_not_written_back() {
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    let cmds = send(
        &mut state,
        &mut app,
        Event::BellWhenFocusedLoaded { on: true },
        at,
    );
    assert!(state.bell_when_focused());
    assert!(saves(&cmds).is_empty(), "{cmds:?}");
    assert!(saves(&tick(&mut state, &mut app, at + SECOND)).is_empty());
    // And turning it off afterwards is a change like any other.
    assert_eq!(
        saves(&toggle_bell(&mut state, &mut app, at + 2 * SECOND)),
        vec![false]
    );
}

#[test]
fn with_the_switch_on_a_fire_with_the_director_focused_rings_and_leaves_no_mark() {
    let (mut state, mut app, director, worker, fire_at) = focused_director_fixture();
    send(
        &mut state,
        &mut app,
        Event::BellWhenFocusedLoaded { on: true },
        fire_at,
    );
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fire_at,
    );
    assert!(rang(&cmds), "{cmds:?}");
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::QuietFocused {
            brigade: 1,
            director: director.clone()
        }],
        "the switch is on and the Director is focused"
    );
    assert!(
        state.attention_panes().is_empty(),
        "a focused pane carries no mark"
    );
    assert!(state.latched_attention_panes.is_empty() && state.attention.is_empty());
}

#[test]
fn with_the_switch_off_the_same_fire_is_silent() {
    let (mut state, mut app, director, worker, fire_at) = focused_director_fixture();
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fire_at,
    );
    assert!(!rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().is_empty());
}

#[test]
fn with_the_director_not_focused_the_switch_changes_nothing() {
    let mut outcomes = Vec::new();
    for on in [false, true] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        send(
            &mut state,
            &mut app,
            Event::BellWhenFocusedLoaded { on },
            at,
        );
        let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Idle)],
            idle_from + BRIGADE_QUIET_SETTLE,
        );
        assert_eq!(
            cmds.iter()
                .filter(|cmd| matches!(cmd, Cmd::RingHostBell { .. }))
                .count(),
            1,
            "switch {on}: one bell: {cmds:?}"
        );
        outcomes.push(state.attention_panes().contains(&director));
    }
    assert_eq!(outcomes, vec![true, true], "mark in both");
}

#[test]
fn the_switched_on_bell_respects_the_cooldown() {
    let (mut state, mut app, director, worker, fire_at) = focused_director_fixture();
    send(
        &mut state,
        &mut app,
        Event::BellWhenFocusedLoaded { on: true },
        fire_at,
    );
    // Another attention bell rang a moment ago.
    state.last_attention_bell = Some(fire_at - ATTENTION_BELL_COOLDOWN + Duration::from_millis(1));
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fire_at,
    );
    assert!(!rang(&cmds), "inside the cooldown: {cmds:?}");
}

#[test]
fn the_switch_leaves_a_panes_own_attention_alone() {
    // A BEL from a pane the operator is not in marks it and rings; a BEL from
    // the pane the operator is in does neither — with the switch either way.
    for on in [false, true] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        send(
            &mut state,
            &mut app,
            Event::BellWhenFocusedLoaded { on },
            at,
        );
        state.focus = Focus::Pane; // the Director's pane is the focused one

        let cmds = send(
            &mut state,
            &mut app,
            Event::PtyOutput {
                key: director.clone(),
                chunk: b"\x07".to_vec(),
            },
            at + SECOND,
        );
        assert!(!rang(&cmds), "switch {on}, focused pane: {cmds:?}");
        assert!(state.attention_panes().is_empty(), "switch {on}");

        let cmds = send(
            &mut state,
            &mut app,
            Event::PtyOutput {
                key: worker.clone(),
                chunk: b"\x07".to_vec(),
            },
            at + 10 * SECOND,
        );
        assert!(rang(&cmds), "switch {on}, other pane: {cmds:?}");
        assert!(state.attention_panes().contains(&worker), "switch {on}");
    }
}

// --- what "effectively focused" means for the fire ---------------------------------

#[test]
fn a_window_that_lost_focus_leaves_the_directors_pane_unfocused_so_the_fire_marks_and_rings() {
    let (mut state, mut app, director, worker, fire_at) = focused_director_fixture();
    // The operator is in another window; inside banto the Director's pane is
    // still the selected one.
    send(
        &mut state,
        &mut app,
        Event::WindowFocusChanged { focused: false },
        fire_at - SECOND,
    );
    assert_eq!(state.focus, Focus::Pane);
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fire_at,
    );
    assert!(rang(&cmds), "{cmds:?}");
    assert!(state.attention_panes().contains(&director));
}

#[test]
fn before_any_focus_event_the_window_counts_as_focused() {
    // `window_focused` starts true: until crossterm reports otherwise banto
    // assumes it is frontmost, so a selected Director pane is a focused one.
    let (state, ..) = focused_director_fixture();
    assert!(state.window_focused);
    assert_eq!(effective_focused_pane(&state), Some(key("dir")));
}

// --- input banto has just written ------------------------------------------------------

/// Two hosted sessions, one of them staged and focused for typing, both read
/// as idle, and the list reachable with F2.
fn typing_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let (mut state, mut app, a, b, at) = quit_fixture();
    state.stage = Stage::Solo(a.clone());
    state.focus = Focus::Pane;
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    (state, app, a, b, at)
}

fn back_to_the_list(state: &mut EmporiumState, app: &mut App, at: Instant) {
    press(state, app, KeyCode::F(2), at);
    assert_eq!(state.focus, Focus::Sidebar);
}

#[test]
fn a_key_just_forwarded_to_a_pane_counts_it_as_not_idle_whatever_its_sample_says() {
    let (mut state, mut app, _a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    back_to_the_list(&mut state, &mut app, at);

    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: a (input)")
    );
}

#[test]
fn after_the_window_the_sample_alone_decides_again() {
    // Just inside the window it still asks ...
    let (mut state, mut app, _a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    back_to_the_list(&mut state, &mut app, at);
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + QUIT_RECENT_INPUT_WINDOW - Duration::from_millis(1),
    );
    assert!(!app.should_quit());

    // ... and once it is over one press quits.
    let (mut state, mut app, _a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    back_to_the_list(&mut state, &mut app, at);
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + QUIT_RECENT_INPUT_WINDOW,
    );
    assert!(app.should_quit());
}

#[test]
fn a_paste_into_a_pane_counts_too() {
    let (mut state, mut app, _a, _b, at) = typing_fixture();
    send(
        &mut state,
        &mut app,
        Event::Input(InputEvent::Paste("hello".to_string())),
        at,
    );
    back_to_the_list(&mut state, &mut app, at);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: a (input)")
    );
}

#[test]
fn the_prefix_sent_through_literally_counts_too() {
    // Prefix, then `b`: the prefix key itself goes to the child.
    let (mut state, mut app, a, _b, at) = typing_fixture();
    press_with(
        &mut state,
        &mut app,
        KeyCode::Char('b'),
        Modifiers::CONTROL,
        at,
    );
    let cmds = press(&mut state, &mut app, KeyCode::Char('b'), at);
    assert!(wrote_to(&cmds, &a), "{cmds:?}");
    back_to_the_list(&mut state, &mut app, at);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: a (input)")
    );
}

// --- which of the pointer's traffic counts as input ------------------------------------

fn mouse(
    state: &mut EmporiumState,
    app: &mut App,
    kind: MouseEventKind,
    (column, row): (u16, u16),
    at: Instant,
) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Input(InputEvent::Mouse(MouseEvent { kind, column, row })),
        at,
    )
}

/// A cell in the middle of `pane`'s tile, clear of its title row and borders.
fn inside(state: &EmporiumState, pane: &SessionKey) -> (u16, u16) {
    let geometry = state.geometry(Rect::new(0, 0, state.size.0, state.size.1));
    let (_, rect) = geometry
        .tiles
        .iter()
        .find(|(key, _)| key == pane)
        .expect("the pane has a tile");
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

/// A cell inside the list.
fn in_the_list(state: &EmporiumState) -> (u16, u16) {
    let sidebar = state
        .geometry(Rect::new(0, 0, state.size.0, state.size.1))
        .areas
        .sidebar;
    (sidebar.x + 2, sidebar.y + 2)
}

/// What a child that asked for every mouse report ends up with.
const MOUSE_REPORTING_ON: &[u8] = b"\x1b[?1003h\x1b[?1006h";

fn wrote_to(cmds: &[Cmd], pane: &SessionKey) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, Cmd::WritePty { key, .. } if key == pane))
}

/// The pointer does `kind` over the focused pane, the operator clicks the list
/// and presses Esc a second later: whether the pane's child was written to, and
/// what the quit key then did.
fn quit_after_pointer(kind: MouseEventKind) -> (bool, bool, Option<String>) {
    let (mut state, mut app, a, _b, at) = typing_fixture();
    state.size = (120, 40);
    said(&mut state, &mut app, &a, MOUSE_REPORTING_ON, at);
    assert!(
        state.screens[&a].wants_sgr_mouse(),
        "the child asked for reports"
    );
    let over = inside(&state, &a);
    let forwarded = wrote_to(&mouse(&mut state, &mut app, kind, over, at), &a);
    let list = in_the_list(&state);
    mouse(
        &mut state,
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        list,
        at,
    );
    assert_eq!(state.focus, Focus::Sidebar);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    (
        forwarded,
        app.should_quit(),
        state.displayed_status().map(str::to_string),
    )
}

#[test]
fn no_pointer_traffic_asks_the_quit_question() {
    // Reading a pane's scrollback, hovering, clicking it to focus it or finishing
    // a gesture neither starts nor continues a turn: the operator who then leaves
    // quits at once. Everything here but a bare move is still written to the child
    // (a move has no SGR form here and never was).
    for (kind, forwarded) in [
        (MouseEventKind::Moved, false),
        (MouseEventKind::Down(MouseButton::Left), true),
        (MouseEventKind::Down(MouseButton::Middle), true),
        (MouseEventKind::Down(MouseButton::Right), true),
        (MouseEventKind::Drag(MouseButton::Left), true),
        (MouseEventKind::Up(MouseButton::Left), true),
        (MouseEventKind::ScrollUp, true),
        (MouseEventKind::ScrollDown, true),
    ] {
        let (written, quit, status) = quit_after_pointer(kind);
        assert_eq!(written, forwarded, "{kind:?}");
        assert!(quit, "{kind:?}: asked {status:?}");
    }
}

#[test]
fn clicking_into_a_pane_and_back_to_the_list_then_leaving_quits_at_once() {
    // The field sequence: the operator moves between the pane and the list by
    // clicking, both ways, with nothing working anywhere.
    let (mut state, mut app, a, _b, at) = quit_fixture();
    state.stage = Stage::Solo(a.clone());
    sample(&mut state, &mut app, &[(&a, Idle), (&_b, Idle)], at);
    state.size = (120, 40);
    said(&mut state, &mut app, &a, MOUSE_REPORTING_ON, at);
    let over = inside(&state, &a);
    let cmds = mouse(
        &mut state,
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        over,
        at,
    );
    assert!(wrote_to(&cmds, &a), "the click reached the child");
    assert_eq!(state.focus, Focus::Pane, "the click focused the pane");
    let list = in_the_list(&state);
    mouse(
        &mut state,
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        list,
        at + SECOND / 2,
    );
    assert_eq!(state.focus, Focus::Sidebar);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert!(app.should_quit(), "asked {:?}", state.displayed_status());
}

/// The Worker's pane focused for the operator, whose child asked for mouse
/// reports, unread mail waiting for it and both members idle. The operator does
/// `kind` over it, if anything; then the relay ticks once a second, and this
/// gives the second after which it first writes a nudge into the Worker.
fn first_nudge_after(kind: Option<MouseEventKind>) -> Option<u64> {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.size = (120, 40);
    stage(&mut state, 1, &director, &[&worker]);
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    state.focus = Focus::Pane;
    said(&mut state, &mut app, &worker, MOUSE_REPORTING_ON, at);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at,
    );
    if let Some(kind) = kind {
        let over = inside(&state, &worker);
        assert!(wrote_to(
            &mouse(&mut state, &mut app, kind, over, at),
            &worker
        ));
    }
    (0..=RELAY_INPUT_QUIET_PERIOD.as_secs() + 5).find(|secs| {
        let cmds = send(
            &mut state,
            &mut app,
            Event::Tick {
                relay: vec![mail_for(1, "worker-1", &worker)],
                brigades: Vec::new(),
            },
            at + Duration::from_secs(*secs),
        );
        typed_into(&cmds, &worker)
    })
}

#[test]
fn every_pointer_report_forwarded_to_a_pane_still_holds_the_relays_nudge() {
    // The relay stays out of a pane the operator is doing something in, and
    // reading a transcript with the wheel is doing something: a nudge typed into
    // it would land in the middle. That is unlike the quit question, which asks
    // about work given and does not count the wheel.
    let free = first_nudge_after(None).expect("with no input the nudge goes out");
    assert!(free < 5, "{free}");
    for kind in [
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown,
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Left),
    ] {
        let at = first_nudge_after(Some(kind)).expect("it goes out once the quiet period ends");
        assert!(at >= RELAY_INPUT_QUIET_PERIOD.as_secs(), "{kind:?}: {at}");
    }
}

#[test]
fn the_relay_and_the_quit_question_read_two_different_records_of_pointer_traffic() {
    let (mut state, mut app, a, _b, at) = typing_fixture();
    state.size = (120, 40);
    said(&mut state, &mut app, &a, MOUSE_REPORTING_ON, at);
    let over = inside(&state, &a);
    for kind in [
        MouseEventKind::ScrollUp,
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
    ] {
        mouse(&mut state, &mut app, kind, over, at);
        assert!(state.last_forwarded_input.contains_key(&a), "{kind:?}");
        assert!(!state.last_given_input.contains_key(&a), "{kind:?}");
        state.last_forwarded_input.clear();
    }
    // A key is both.
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    assert!(state.last_forwarded_input.contains_key(&a));
    assert!(state.last_given_input.contains_key(&a));
}

/// Brigade 1 on stage with the operator inside the Director's pane and the bell
/// switch on, both processes only just started (nothing is known about them),
/// the Director's child asking for mouse reports. After `act` the Director
/// reads busy for a second and then idle for as long as the settle: whether the
/// quiet watch was armed by that, and the brigade rang.
fn a_freshly_started_brigade_after(
    act: impl FnOnce(&mut EmporiumState, &mut App, &SessionKey, Instant),
) -> bool {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let director = host_fresh(&mut state, 1, DIRECTOR_TOKEN, "dir");
    let worker = host_fresh(&mut state, 1, "worker-1", "w1");
    stage(&mut state, 1, &director, &[&worker]);
    state.focus = Focus::Pane;
    state.size = (120, 40);
    let at = t0();
    tick(&mut state, &mut app, at);
    toggle_bell(&mut state, &mut app, at);
    assert!(state.bell_when_focused());
    said(&mut state, &mut app, &director, MOUSE_REPORTING_ON, at);
    act(&mut state, &mut app, &director, at + SECOND);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&director, &worker], at + 2 * SECOND);
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    rang(&cmds)
}

#[test]
fn a_pane_the_operator_only_pointed_at_was_not_given_work_so_its_first_busy_reading_arms_nothing() {
    // The field report: after a restart the operator staged the brigade, the bell
    // rang once shortly after the resume, and nothing was being worked on. This
    // pins one way a process could be taken for "given work" that it was not:
    // pointer traffic over a freshly resumed pane. What a resumed pane reports at
    // its start is not measured, so this says nothing about whether it was the
    // cause there.
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::ScrollUp,
        MouseEventKind::ScrollDown,
    ] {
        let rang = a_freshly_started_brigade_after(|state, app, director, at| {
            let over = inside(state, director);
            mouse(state, app, kind, over, at);
        });
        assert!(!rang, "{kind:?}");
    }
}

#[test]
fn a_pane_that_was_typed_into_was_given_work_and_its_busy_reading_arms_the_watch() {
    assert!(a_freshly_started_brigade_after(|state, app, _, at| {
        press(state, app, KeyCode::Char('x'), at);
    }));
    assert!(a_freshly_started_brigade_after(|state, app, _, at| {
        send(
            state,
            app,
            Event::Input(InputEvent::Paste("hello".to_string())),
            at,
        );
    }));
}

// --- a process's start-up is not work ------------------------------------------------

const MS: Duration = Duration::from_millis(1);

/// A brigade resumed by Enter on its Director's row, as in the operator's trial:
/// the Director and two Claude Workers, and a Codex Worker whose pane reports its
/// own state. Every pane spawned at `spawned`, the bell switch on, the operator
/// inside the Director's pane.
struct Resumed {
    state: EmporiumState,
    app: App,
    director: SessionKey,
    codex: SessionKey,
    claude: Vec<SessionKey>,
    start: Instant,
    spawned: Instant,
}

impl Resumed {
    fn new() -> Self {
        let mut state = EmporiumState::new(PrefixKey::default());
        let mut app = App::new(vec![
            row_for("dir"),
            codex_row("cx"),
            row_for("c1"),
            row_for("c2"),
        ]);
        app.set_hidden_member_ids(["cx", "c1", "c2"].into_iter().map(str::to_string).collect());
        app.set_viewport_height(20);
        let start = t0();
        tick(&mut state, &mut app, start);
        state.focus = Focus::Sidebar;
        let asked = press(&mut state, &mut app, KeyCode::Enter, start);
        assert!(
            asked.iter().any(|cmd| matches!(
                cmd,
                Cmd::Store(StoreIntent::ResolveMembership { session_id }) if session_id == "dir"
            )),
            "{asked:?}"
        );
        let roster = vec![
            (
                DIRECTOR_TOKEN.to_string(),
                BrigadeRole::Director,
                Some("dir".to_string()),
            ),
            (
                "worker-1".to_string(),
                BrigadeRole::Worker,
                Some("cx".to_string()),
            ),
            (
                "worker-2".to_string(),
                BrigadeRole::Worker,
                Some("c1".to_string()),
            ),
            (
                "worker-3".to_string(),
                BrigadeRole::Worker,
                Some("c2".to_string()),
            ),
        ];
        let mut cmds = send(
            &mut state,
            &mut app,
            Event::MembershipResolved {
                session_id: "dir".to_string(),
                membership: Some((1, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
                members: Some(roster),
                layout: None,
                names: Vec::new(),
            },
            start,
        );
        // The shell reports every spawn it was asked for, 741 ms in.
        let spawned = start + 741 * MS;
        let mut opened = 0;
        while let Some(cmd) = cmds.pop() {
            if let Cmd::OpenEmbedded { key, .. } = cmd {
                opened += 1;
                cmds.extend(send(
                    &mut state,
                    &mut app,
                    Event::Spawned { key, agent: None },
                    spawned,
                ));
            }
        }
        assert_eq!(opened, 4, "the whole brigade was opened");
        assert_eq!(state.focus, Focus::Pane);
        toggle_bell(&mut state, &mut app, spawned);
        assert!(state.bell_when_focused());
        let director = key("dir");
        let codex = key("cx");
        said(&mut state, &mut app, &codex, b"", spawned);
        Self {
            state,
            app,
            director,
            codex,
            claude: vec![key("c1"), key("c2")],
            start,
            spawned,
        }
    }

    /// One sample and relay tick at `ms` after the run started. `codex` and
    /// `claude` are what the shell read for the Codex Worker and for the Claude
    /// panes (the Director among them) at that moment.
    fn sample_at(&mut self, ms: u64, codex: Quiescence, claude: Quiescence) -> Vec<Cmd> {
        let mut readings = vec![(self.codex.clone(), codex), (self.director.clone(), claude)];
        readings.extend(self.claude.iter().map(|pane| (pane.clone(), claude)));
        let readings: Vec<(&SessionKey, Quiescence)> =
            readings.iter().map(|(pane, q)| (pane, *q)).collect();
        sample(
            &mut self.state,
            &mut self.app,
            &readings,
            self.start + Duration::from_millis(ms),
        )
    }

    fn codex_reports(&mut self, ms: u64, report: &[u8]) {
        said(
            &mut self.state,
            &mut self.app,
            &self.codex,
            report,
            self.start + Duration::from_millis(ms),
        );
    }
}

#[test]
fn a_codex_workers_start_up_report_of_working_does_not_arm_the_watch_and_nothing_rings() {
    // The measured sequence (offsets from the run start; the spawns at 741 ms): the
    // Codex pane reads idle on the shell's first sample, then reports `working`
    // by itself while it starts, then `idle`; the Claude panes were never read
    // idle before they settled. Nobody was given anything to do.
    let mut run = Resumed::new();
    let mut rang_at = None;
    let mut note = |ms: u64, cmds: Vec<Cmd>| {
        if rang(&cmds) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    };
    note(900, run.sample_at(900, Unknown, Unknown));
    note(1053, run.sample_at(1053, Idle, Unknown));
    run.codex_reports(2000, REPORT_WORKING);
    note(2061, run.sample_at(2061, Idle, Unknown));
    run.codex_reports(2136, REPORT_IDLE);
    note(3100, run.sample_at(3100, Idle, Unknown));
    note(4141, run.sample_at(4141, Idle, Idle));
    for ms in (5141..=15_141).step_by(1000) {
        note(ms, run.sample_at(ms, Idle, Idle));
    }
    assert_eq!(rang_at, None, "a start-up is not work");
}

#[test]
fn the_directors_own_start_up_is_covered_like_any_members() {
    // Read idle at 1.5 s, busy at 3 s (a start-up that took its time), idle again:
    // inside the grace, so nothing was begun and nothing rings.
    let mut run = Resumed::new();
    run.sample_at(1500, Idle, Idle);
    run.sample_at(3000, Idle, NotIdle);
    let mut rang_at = None;
    for ms in (4000..=14_000).step_by(1000) {
        if rang(&run.sample_at(ms, Idle, Idle)) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    }
    assert_eq!(rang_at, None);
}

#[test]
fn the_same_working_report_after_the_grace_arms_the_watch_and_the_brigade_rings() {
    // Not a start-up any more: the process was read idle, then does something on
    // its own at 11 s (more than the grace after it was spawned), and goes idle
    // again. That is the watch's ordinary case.
    let mut run = Resumed::new();
    run.sample_at(1053, Idle, Unknown);
    run.sample_at(4141, Idle, Idle);
    assert!(PROCESS_START_GRACE < 11 * SECOND);
    run.codex_reports(11_741, REPORT_WORKING);
    run.sample_at(11_800, Idle, Idle);
    run.codex_reports(14_000, REPORT_IDLE);
    let mut rang_at = None;
    for ms in (15_000..=25_000).step_by(1000) {
        if rang(&run.sample_at(ms, Idle, Idle)) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    }
    assert!(rang_at.is_some(), "the brigade went quiet after work");
}

#[test]
fn work_that_is_still_going_when_the_grace_ends_arms_at_the_first_reading_past_it() {
    let mut run = Resumed::new();
    run.sample_at(1053, Idle, Unknown);
    run.codex_reports(2000, REPORT_WORKING);
    run.sample_at(2061, Idle, Unknown);
    // Still working at the first sample past the grace.
    let past = (PROCESS_START_GRACE.as_millis() as u64) + 741;
    run.sample_at(past, Idle, Unknown);
    run.codex_reports(past + 500, REPORT_IDLE);
    let mut rang_at = None;
    for ms in (past + 1000..=past + 12_000).step_by(1000) {
        if rang(&run.sample_at(ms, Idle, Idle)) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    }
    assert!(rang_at.is_some(), "self-started work past the grace arms");
}

#[test]
fn what_the_operator_or_banto_gave_a_process_arms_the_watch_inside_the_grace() {
    // A key typed into the Director's pane, then it reads busy: given work, at any
    // age.
    let mut run = Resumed::new();
    press(
        &mut run.state,
        &mut run.app,
        KeyCode::Char('x'),
        run.spawned + 300 * MS,
    );
    run.sample_at(1500, Idle, NotIdle);
    let mut rang_at = None;
    for ms in (2500..=12_000).step_by(1000) {
        if rang(&run.sample_at(ms, Idle, Idle)) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    }
    assert!(rang_at.is_some(), "a key arms inside the grace");

    // A nudge banto typed into a Worker, which then reads busy.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.process_started.insert(worker.clone(), at);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at,
    );
    let tick_with_mail = |state: &mut EmporiumState, app: &mut App, when: Instant| {
        send(
            state,
            app,
            Event::Tick {
                relay: vec![mail_for(1, "worker-1", &worker)],
                brigades: Vec::new(),
            },
            when,
        )
    };
    tick_with_mail(&mut state, &mut app, at + SECOND);
    let cmds = tick_with_mail(&mut state, &mut app, at + 2 * SECOND);
    assert!(typed_into(&cmds, &worker), "the nudge went out: {cmds:?}");
    let busy = [(&director, Idle), (&worker, NotIdle)];
    sample(&mut state, &mut app, &busy, at + 3 * SECOND);
    let idle = [(&director, Idle), (&worker, Idle)];
    let mut rang_at = None;
    for secs in 4..=14 {
        if rang(&sample(&mut state, &mut app, &idle, at + secs * SECOND)) && rang_at.is_none() {
            rang_at = Some(secs);
        }
    }
    assert!(rang_at.is_some(), "a nudge arms inside the grace");
}

#[test]
fn a_relaunched_pane_gets_a_new_grace() {
    let mut run = Resumed::new();
    run.sample_at(1053, Idle, Idle);
    // Long after, the Worker's process is replaced under the same key.
    let relaunched = run.start + 40 * SECOND;
    send(
        &mut run.state,
        &mut run.app,
        Event::Spawned {
            key: run.codex.clone(),
            agent: None,
        },
        relaunched,
    );
    said(
        &mut run.state,
        &mut run.app,
        &run.codex.clone(),
        REPORT_IDLE,
        relaunched + 100 * MS,
    );
    run.sample_at(40_500, Idle, Idle);
    run.codex_reports(41_000, REPORT_WORKING);
    run.sample_at(41_100, Idle, Idle);
    run.codex_reports(41_500, REPORT_IDLE);
    let mut rang_at = None;
    for ms in (42_000..=52_000).step_by(1000) {
        if rang(&run.sample_at(ms, Idle, Idle)) && rang_at.is_none() {
            rang_at = Some(ms);
        }
    }
    assert_eq!(rang_at, None, "its own start-up is not work either");
}

#[test]
fn a_relay_nudge_just_typed_into_a_pane_counts_it() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at,
    );
    let observation = || RelayObservation {
        live: None,
        glance: None,
        brigade_id: 1,
        token: "worker-1".to_string(),
        key: worker.clone(),
        has_unseen: true,
        is_idle_this_tick: Some(true),
        agent: None,
        briefed_since_launch: None,
    };
    let tick_with_mail = |state: &mut EmporiumState, app: &mut App, when: Instant| {
        send(
            state,
            app,
            Event::Tick {
                relay: vec![observation()],
                brigades: Vec::new(),
            },
            when,
        )
    };
    tick_with_mail(&mut state, &mut app, at);
    let cmds = tick_with_mail(&mut state, &mut app, at + SECOND);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::WritePty { key, .. } if key == &worker)),
        "the nudge went out: {cmds:?}"
    );
    // The operator's own input record is not touched by it: the relay reads
    // that one to stay out of the way, and a nudge must not silence the next.
    assert!(!state.last_forwarded_input.contains_key(&worker));

    // Both samples say idle; the nudge is a second old.
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at + 2 * SECOND,
    );
    press(&mut state, &mut app, KeyCode::Esc, at + 2 * SECOND);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: worker-1 (nudged)")
    );
}

#[test]
fn recent_input_changes_neither_the_brigade_bell_nor_what_the_samples_say() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // The operator types into the Worker's pane one second before the settle
    // would end.
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(director.clone()),
        panes: vec![director.clone(), worker.clone()],
        focused: 1,
    };
    state.focus = Focus::Pane;
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let typed_at = idle_from + BRIGADE_QUIET_SETTLE - SECOND;
    press(&mut state, &mut app, KeyCode::Char('x'), typed_at);
    assert!(state.took_input_recently(&worker, typed_at));

    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(
        rang(&cmds),
        "the bell decides from the samples alone: {cmds:?}"
    );
}

#[test]
fn a_panes_input_record_follows_its_key_and_goes_with_the_pane() {
    // The operator's key, with nothing banto wrote: what the quit question reads
    // for the operator's input must follow the rename on its own.
    let (mut state, mut app, a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    let renamed = key("a-after-discovery");
    state.rekey_pane_state(&a, &renamed);
    assert!(state.took_input_recently(&renamed, at));
    assert!(!state.took_input_recently(&a, at));
    assert!(state.was_given_work(&renamed));
    assert!(state.last_forwarded_input.contains_key(&renamed));
    state.unstage(&renamed);
    assert!(!state.last_forwarded_input.contains_key(&renamed));
    assert!(!state.last_given_input.contains_key(&renamed));

    // And banto's own write.
    let (mut state, _app, a, _b, at) = typing_fixture();
    state.last_prompted.insert(a.clone(), at);
    state.rekey_pane_state(&a, &renamed);
    assert!(state.took_input_recently(&renamed, at));
    state.unstage(&renamed);
    assert!(!state.last_prompted.contains_key(&renamed));
}

// --- the quiet stretch belongs to the members that were watched ---------------------

#[test]
fn a_member_hosted_late_in_a_stretch_does_not_borrow_the_time_the_others_were_idle() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let almost = idle_from + BRIGADE_QUIET_SETTLE - SECOND;
    let two = [(&director, Idle), (&worker, Idle)];
    assert!(!rang(&sample(&mut state, &mut app, &two, almost)));

    // A second Worker appears and reads idle at once, when the first two would
    // have been quiet for the whole settle.
    let late = host(&mut state, 1, "worker-2", "w2");
    let three = [(&director, Idle), (&worker, Idle), (&late, Idle)];
    let at_old_deadline = idle_from + BRIGADE_QUIET_SETTLE;
    let cmds = sample(&mut state, &mut app, &three, at_old_deadline);
    assert!(!rang(&cmds), "{cmds:?}");

    // Five seconds after it was first seen idle, the whole brigade has been.
    let cmds = sample(
        &mut state,
        &mut app,
        &three,
        at_old_deadline + BRIGADE_QUIET_SETTLE - Duration::from_millis(1),
    );
    assert!(!rang(&cmds), "{cmds:?}");
    let cmds = sample(
        &mut state,
        &mut app,
        &three,
        at_old_deadline + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn a_member_that_leaves_mid_stretch_restarts_the_count() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let extra = host(&mut state, 1, "worker-2", "w2");
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle), (&extra, Idle)],
        at,
    );
    let idle_from = at + SECOND;
    let three = [(&director, Idle), (&worker, Idle), (&extra, Idle)];
    sample(&mut state, &mut app, &three, idle_from);
    let almost = idle_from + BRIGADE_QUIET_SETTLE - SECOND;
    assert!(!rang(&sample(&mut state, &mut app, &three, almost)));

    // One Worker ends. The members that are left are a different set: their
    // stretch starts from the first sample that sees them alone.
    unhost(&mut state, &extra);
    let two = [(&director, Idle), (&worker, Idle)];
    let after_leaving = idle_from + BRIGADE_QUIET_SETTLE;
    let cmds = sample(&mut state, &mut app, &two, after_leaving);
    assert!(!rang(&cmds), "{cmds:?}");
    let cmds = sample(
        &mut state,
        &mut app,
        &two,
        after_leaving + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn a_member_relaunched_under_the_same_key_is_a_new_member() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let almost = idle_from + BRIGADE_QUIET_SETTLE - SECOND;
    let both = [(&director, Idle), (&worker, Idle)];
    sample(&mut state, &mut app, &both, almost);

    // Its process was replaced between two samples; the key is the same.
    unhost(&mut state, &worker);
    host(&mut state, 1, "worker-1", "w1");
    let cmds = sample(
        &mut state,
        &mut app,
        &both,
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(!rang(&cmds), "{cmds:?}");
}

#[test]
fn a_pane_whose_key_changes_keeps_the_stretch_it_was_part_of() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let almost = idle_from + BRIGADE_QUIET_SETTLE - SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        almost,
    );

    // Discovery gives the same live process its real session id.
    let renamed = key("w1-discovered");
    state.rekey_pane_state(&worker, &renamed);
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&renamed, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "same process, same stretch: {cmds:?}");
}

#[test]
fn a_new_member_that_was_never_busy_neither_arms_nor_disarms_a_fire() {
    // It does not arm: a brigade that was never busy, joined by an idle member,
    // still does not ring.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let all_idle = |extra: &SessionKey| {
        vec![
            (director.clone(), Idle),
            (worker.clone(), Idle),
            (extra.clone(), Idle),
        ]
    };
    let joined = host(&mut state, 1, "worker-2", "w2");
    for secs in 0..20 {
        let samples = all_idle(&joined);
        let samples: Vec<_> = samples.iter().map(|(k, q)| (k, *q)).collect();
        let cmds = sample(&mut state, &mut app, &samples, at + secs * SECOND);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }

    // It does not disarm: someone was busy, then a member joins during the
    // stretch; the brigade rings once the new set has been quiet for the settle.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let joined = host(&mut state, 1, "worker-2", "w2");
    let three = [(&director, Idle), (&worker, Idle), (&joined, Idle)];
    sample(&mut state, &mut app, &three, idle_from + SECOND);
    let cmds = sample(
        &mut state,
        &mut app,
        &three,
        idle_from + SECOND + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
}

#[test]
fn a_brigade_that_is_gone_leaves_nothing_for_the_next_one() {
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&w1, &d1], at);
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle)],
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "the first brigade fired");

    // It is disbanded: its panes end. A new brigade is formed and sits idle
    // from the start.
    unhost(&mut state, &d1);
    unhost(&mut state, &w1);
    let gone = idle_from + BRIGADE_QUIET_SETTLE + SECOND;
    sample(&mut state, &mut app, &[], gone);
    assert!(state.brigade_quiet.is_empty());
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");
    for secs in 0..20 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&d2, Idle), (&w2, Idle)],
            gone + SECOND + secs * SECOND,
        );
        assert!(!rang(&cmds), "never busy: at +{secs}s: {cmds:?}");
    }
}

// --- the kickoff retry's Enter is banto's own write too ---------------------------------

#[test]
fn the_retry_enter_of_a_kickoff_counts_the_pane_for_the_quit_question() {
    use super::tests::{draw_prompt_on, goinkyo_kickoff_retry_state, goinkyo_kickoff_tick};
    let setup = || {
        goinkyo_kickoff_retry_state(
            |screen| draw_prompt_on(screen, Some(GOINKYO_KICKOFF_LINE)),
            true,
        )
    };

    // The sentence was typed 15 s ago and is still sitting unsubmitted: the
    // retry presses Enter alone.
    let (mut state, mut app, brigade, pane, t0) = setup();
    state.focus = Focus::Sidebar;
    let enter_at = t0 + KICKOFF_ACK_TIMEOUT;
    let cmds = goinkyo_kickoff_tick(&mut state, &mut app, &brigade, enter_at);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::WritePty { bytes, .. } if bytes == b"\r")),
        "the Enter went out: {cmds:?}"
    );
    // The pane's own sample still says idle, as it would a moment after Enter.
    sample(&mut state, &mut app, &[(&pane, Idle)], enter_at);

    // A quit key inside the window asks, with that pane counted.
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        enter_at + QUIT_RECENT_INPUT_WINDOW - Duration::from_millis(1),
    );
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: goinkyo-re… (nudged)")
    );

    // After the window the sample alone decides again.
    let (mut state, mut app, brigade, pane, t0) = setup();
    state.focus = Focus::Sidebar;
    let enter_at = t0 + KICKOFF_ACK_TIMEOUT;
    goinkyo_kickoff_tick(&mut state, &mut app, &brigade, enter_at);
    sample(&mut state, &mut app, &[(&pane, Idle)], enter_at);
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        enter_at + QUIT_RECENT_INPUT_WINDOW,
    );
    assert!(app.should_quit());
}

// --- the stretch counts from when banto last prompted a member -------------------------

/// The relay's view of `member` with unread mail, idle this tick.
fn mail_for(brigade: BrigadeId, token: &str, member: &SessionKey) -> RelayObservation {
    RelayObservation {
        live: None,
        glance: None,
        brigade_id: brigade,
        token: token.to_string(),
        key: member.clone(),
        has_unseen: true,
        is_idle_this_tick: Some(true),
        agent: None,
        briefed_since_launch: None,
    }
}

fn typed_into(cmds: &[Cmd], pane: &SessionKey) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, Cmd::WritePty { key, bytes } if key == pane && !bytes.is_empty()))
}

/// One tick a second from `from` up to and including `until` (all members idle,
/// no new mail), returning the instant of the first ring, if any.
fn tick_each_second_until(
    state: &mut EmporiumState,
    app: &mut App,
    members: &[(&SessionKey, Quiescence)],
    from: Instant,
    until: Instant,
) -> Option<Instant> {
    let mut at = from;
    while at <= until {
        if rang(&sample(state, app, members, at)) {
            return Some(at);
        }
        at += SECOND;
    }
    None
}

/// [`tick_each_second_until`] with the relay's observations of each tick.
fn tick_each_second_until_seeing(
    state: &mut EmporiumState,
    app: &mut App,
    members: &[(&SessionKey, Quiescence)],
    relay: impl Fn() -> Vec<RelayObservation>,
    from: Instant,
    until: Instant,
) -> Option<Instant> {
    let mut at = from;
    while at <= until {
        if rang(&sample_with_relay(state, app, members, relay(), at)) {
            return Some(at);
        }
        at += SECOND;
    }
    None
}

#[test]
fn the_settle_counts_from_the_nudge_not_from_when_the_worker_went_idle() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];

    // The Worker reported and went idle at `idle_from`; the relay reads the
    // Director's mail on that tick and the next, and types the nudge on the
    // second.
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &director)];
    let cmds = sample_with_relay(&mut state, &mut app, &all_idle, mail(), idle_from);
    assert!(!typed_into(&cmds, &director));
    let nudged_at = idle_from + SECOND;
    let cmds = sample_with_relay(&mut state, &mut app, &all_idle, mail(), nudged_at);
    assert!(typed_into(&cmds, &director), "the nudge went out: {cmds:?}");

    // The Director reads its mail but still samples idle (the sample lags, or
    // there was nothing to answer). The submitting Enter goes out on the next
    // tick, and the settle runs from that last write: five seconds after the
    // Worker went idle is not enough, and neither is five seconds after the
    // nudge line.
    let enter_at = nudged_at + SECOND;
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(false, &director),
        nudged_at + SECOND,
        enter_at + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(enter_at + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_nudge_typed_on_the_tick_that_would_complete_the_stretch_does_not_ring_on_that_tick() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &director)];

    // The relay's first look at the mail is a tick before the settle ends; its
    // second, at the very tick the settle would end, types the nudge.
    sample_with_relay(
        &mut state,
        &mut app,
        &all_idle,
        mail(),
        idle_from + BRIGADE_QUIET_SETTLE - SECOND,
    );
    let deadline = idle_from + BRIGADE_QUIET_SETTLE;
    let cmds = sample_with_relay(&mut state, &mut app, &all_idle, mail(), deadline);
    assert!(typed_into(&cmds, &director), "the nudge went out: {cmds:?}");
    assert!(
        !rang(&cmds),
        "a ring just before the brigade resumes: {cmds:?}"
    );

    // If the Director reads its mail and wakes to nothing, the brigade is quiet
    // five seconds after its last write (the submitting Enter, a tick later).
    let enter_at = deadline + SECOND;
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(false, &director),
        deadline + SECOND,
        enter_at + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(enter_at + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_nudge_held_back_by_a_draft_holds_the_ring_for_as_long_as_it_is_held() {
    // The recipient has an unsent draft: the relay holds the nudge and nothing
    // is written, but the mail is still to be delivered, so the brigade is not
    // quiet however long that takes.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    state.unsent_operator_input.insert(director.clone());
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &director)];

    for secs in 0..BRIGADE_QUIET_SETTLE.as_secs() as u32 {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            mail(),
            idle_from + secs * SECOND,
        );
        assert!(!typed_into(&cmds, &director), "held at +{secs}s: {cmds:?}");
    }
    let cmds = sample_with_relay(
        &mut state,
        &mut app,
        &all_idle,
        mail(),
        idle_from + BRIGADE_QUIET_SETTLE,
    );
    assert!(!typed_into(&cmds, &director));
    assert!(!rang(&cmds), "mail is still waiting: {cmds:?}");
}

#[test]
fn a_nudge_in_one_brigade_does_not_restart_the_other() {
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");
    let busy = [(&d1, NotIdle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)];
    sample(&mut state, &mut app, &busy, at);
    let idle_from = at + SECOND;
    let all_idle = [(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)];
    sample(&mut state, &mut app, &all_idle, idle_from);

    // Brigade 1's Director is nudged on the tick brigade 2 would complete.
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &d1)];
    sample_with_relay(
        &mut state,
        &mut app,
        &all_idle,
        mail(),
        idle_from + BRIGADE_QUIET_SETTLE - SECOND,
    );
    let deadline = idle_from + BRIGADE_QUIET_SETTLE;
    let cmds = sample_with_relay(&mut state, &mut app, &all_idle, mail(), deadline);
    assert!(typed_into(&cmds, &d1));
    assert!(
        state.attention.contains_key(&2),
        "brigade 2 went quiet on time"
    );
    assert!(
        !state.attention.contains_key(&1),
        "brigade 1 was just prompted"
    );
}

#[test]
fn every_write_banto_makes_to_start_a_turn_is_stamped_when_it_is_written() {
    // The nudge line, and then its delayed submitting Enter: each moves the
    // stamp to the moment of its own write.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &director)];
    sample_with_relay(&mut state, &mut app, &[], mail(), at);
    let nudged_at = at + SECOND;
    let cmds = sample_with_relay(&mut state, &mut app, &[], mail(), nudged_at);
    assert!(typed_into(&cmds, &director));
    assert_eq!(state.last_prompted.get(&director), Some(&nudged_at));

    let enter_at = nudged_at + RELAY_SUBMIT_DELAY;
    let cmds = tick(&mut state, &mut app, enter_at);
    assert!(
        cmds.iter().any(
            |cmd| matches!(cmd, Cmd::WritePty { key, bytes } if key == &director && bytes == b"\r")
        ),
        "the Enter went out: {cmds:?}"
    );
    assert_eq!(state.last_prompted.get(&director), Some(&enter_at));
    let _ = worker;
}

// --- a brigade does not ring while banto still owes a member its kickoff ----------------

fn row_for(id: &str) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent: crate::model::AgentKind::ClaudeCode,
        title: None,
        cwd: None,
        dialog_open: false,
        activity: crate::model::Activity::Alive,
        is_agent: false,
        preview: None,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        size: 0,
        source_archived: false,
        archived: false,
    }
}

/// The quiet fixture plus a Goinkyo pane that is hosted and has its kickoff to
/// receive: its boot output quiet since `spawned_after` past the fixture's start.
fn goinkyo_fixture(
    spawned_after: Duration,
) -> (
    EmporiumState,
    App,
    SessionKey,
    SessionKey,
    SessionKey,
    Instant,
) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let goinkyo = host(&mut state, 1, GOINKYO_TOKEN, "gk");
    state.pending_goinkyo_kickoffs.push(PendingGoinkyoKickoff {
        key: goinkyo.clone(),
        spawned_at: at + spawned_after,
        notified_not_at_prompt: false,
        turn_seen: false,
        stuck: false,
        sent_at: None,
        attempts: 0,
    });
    // At its empty prompt: the quiet period is what the kickoff waits for.
    super::tests::draw_prompt_on(state.screens.get_mut(&goinkyo).unwrap(), None);
    app.replace_rows(Vec::new());
    (state, app, director, worker, goinkyo, at)
}

#[test]
fn a_goinkyo_kickoff_typed_on_the_tick_that_would_complete_the_stretch_does_not_ring() {
    // The Goinkyo's boot output has been quiet for exactly long enough that its
    // kickoff becomes due on the tick the all-idle stretch would complete.
    let (mut state, mut app, director, worker, goinkyo, at) =
        goinkyo_fixture(SECOND + BRIGADE_QUIET_SETTLE - GOINKYO_KICKOFF_QUIET_PERIOD);
    let deadline = at + SECOND + BRIGADE_QUIET_SETTLE;
    tick(&mut state, &mut app, at);
    let busy = [(&director, NotIdle), (&worker, Idle), (&goinkyo, Idle)];
    sample(&mut state, &mut app, &busy, at);
    let all_idle = [(&director, Idle), (&worker, Idle), (&goinkyo, Idle)];
    let idle_from = at + SECOND;
    let rang_before = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        deadline - SECOND,
    );
    assert_eq!(rang_before, None);

    let cmds = sample(&mut state, &mut app, &all_idle, deadline);
    assert!(
        typed_into(&cmds, &goinkyo),
        "the kickoff was typed on this tick: {cmds:?}"
    );
    assert!(!rang(&cmds), "rang first, typed into afterwards: {cmds:?}");

    // The Goinkyo's first turn shows up as a session row, and only then does
    // the all-idle stretch run again.
    let acknowledged = deadline + 2 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        deadline + SECOND,
        acknowledged - SECOND,
    );
    assert_eq!(rang_at, None, "still waiting for its acknowledgement");
    app.replace_rows(vec![SessionRow {
        preview: Some("[banto] first message".to_string()),
        ..row_for("gk")
    }]);
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        acknowledged,
        acknowledged + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(acknowledged + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_goinkyo_spawned_off_stage_holds_the_ring_until_it_has_had_its_kickoff() {
    // Spawned through the real path for a brigade that is not on stage: from
    // the placeholder key on, the quiet watch counts the pane (it needs both
    // ownership maps), so its owed kickoff keeps the brigade from ringing.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    state.stage = Stage::Empty;
    let cmds = update_goinkyo_awaiting_spawn(
        &mut state,
        &BrigadeConfig::default(),
        GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
            brigade_id: 1,
            cwd: PathBuf::from("/work"),
            roster: Vec::new(),
            agent: None,
            director_agent: None,
        }),
        at,
    );
    let [Cmd::OpenEmbedded { key: goinkyo, .. }] = cmds.as_slice() else {
        panic!("expected exactly one OpenEmbedded, got {cmds:?}");
    };
    let goinkyo = goinkyo.clone();
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: goinkyo.clone(),
            agent: None,
        },
        at,
    );

    let busy = [(&director, NotIdle), (&worker, Idle), (&goinkyo, Idle)];
    sample(&mut state, &mut app, &busy, at);
    let all_idle = [(&director, Idle), (&worker, Idle), (&goinkyo, Idle)];
    let idle_from = at + SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(
        rang_at, None,
        "the Goinkyo's kickoff is still owed, so the brigade is not quiet"
    );
}

#[test]
fn a_goinkyo_spawned_off_stage_that_waits_is_shown_on_its_director() {
    // The brigade marks find a pane through the ownership maps too: without
    // the brigade written at the spawn, the wait of an off-stage Goinkyo has
    // no Director to be shown on until discovery has re-keyed it.
    let (mut state, mut app, director, _worker, at) = quiet_fixture();
    state.stage = Stage::Empty;
    let cmds = update_goinkyo_awaiting_spawn(
        &mut state,
        &BrigadeConfig::default(),
        GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
            brigade_id: 1,
            cwd: PathBuf::from("/work"),
            roster: Vec::new(),
            agent: None,
            director_agent: None,
        }),
        at,
    );
    let [Cmd::OpenEmbedded { key: goinkyo, .. }] = cmds.as_slice() else {
        panic!("expected exactly one OpenEmbedded, got {cmds:?}");
    };
    let goinkyo = goinkyo.clone();
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: goinkyo.clone(),
            agent: None,
        },
        at,
    );
    state.latched_attention_panes.insert(goinkyo);

    tick(&mut state, &mut app, at + SECOND);
    assert!(
        state.marked_directors.contains(&director),
        "the wait is shown on its Director"
    );
}

#[test]
fn a_codex_worker_kickoff_typed_on_the_tick_that_would_complete_the_stretch_does_not_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let deadline = at + SECOND + BRIGADE_QUIET_SETTLE;
    let codex = host(&mut state, 1, "worker-2", "w2");
    state.pending_kickoffs.push(PendingKickoff {
        key: codex.clone(),
        spawned_at: deadline - CODEX_KICKOFF_QUIET_PERIOD,
        notified_not_at_prompt: false,
        turn_seen: false,
        stuck: false,
        sent_at: None,
        attempts: 0,
    });
    // At its prompt and idle: the quiet period is what the kickoff waits for.
    super::tests::paint_codex_prompt(&mut state, &codex, Some("idle"));
    tick(&mut state, &mut app, at);
    let busy = [(&director, NotIdle), (&worker, Idle), (&codex, Idle)];
    sample(&mut state, &mut app, &busy, at);
    let all_idle = [(&director, Idle), (&worker, Idle), (&codex, Idle)];
    let rang_before = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + SECOND,
        deadline - SECOND,
    );
    assert_eq!(rang_before, None);

    let cmds = sample(&mut state, &mut app, &all_idle, deadline);
    assert!(
        typed_into(&cmds, &codex),
        "the kickoff was typed on this tick: {cmds:?}"
    );
    assert!(!rang(&cmds), "{cmds:?}");
    // Not acknowledged for a long while (well past the settle): still blocked.
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        deadline + SECOND,
        deadline + BRIGADE_QUIET_SETTLE + 3 * SECOND,
    );
    assert_eq!(rang_at, None);
    // Its session is discovered: the kickoff is acknowledged.
    let acknowledged = deadline + BRIGADE_QUIET_SETTLE + 4 * SECOND;
    state.pending_kickoffs.clear();
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        acknowledged,
        acknowledged + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(acknowledged + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_goinkyo_that_needs_eight_seconds_to_become_ready_holds_the_ring_until_its_kickoff_is_done() {
    let (mut state, mut app, director, worker, goinkyo, at) = goinkyo_fixture(Duration::ZERO);
    tick(&mut state, &mut app, at);
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle), (&worker, Idle), (&goinkyo, Idle)],
        at,
    );
    let all_idle = [(&director, Idle), (&worker, Idle), (&goinkyo, Idle)];
    // The Goinkyo is still printing its boot output for eight seconds, reading
    // idle in between; everyone else is idle from the first second.
    for secs in 1..=8 {
        let when = at + secs * SECOND;
        send(
            &mut state,
            &mut app,
            Event::PtyOutput {
                key: goinkyo.clone(),
                chunk: b"booting".to_vec(),
            },
            when,
        );
        let cmds = sample(&mut state, &mut app, &all_idle, when);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    // Quiet at last, and the prompt is up (the boot output covered it until
    // now): the kickoff is typed, then acknowledged.
    super::tests::draw_prompt_on(state.screens.get_mut(&goinkyo).unwrap(), None);
    let checked = at + 9 * SECOND;
    let cmds = sample(&mut state, &mut app, &all_idle, checked);
    assert!(typed_into(&cmds, &goinkyo), "{cmds:?}");
    assert!(!rang(&cmds));
    app.replace_rows(vec![SessionRow {
        preview: Some("[banto] first message".to_string()),
        ..row_for("gk")
    }]);
    let acknowledged = checked + SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        acknowledged,
        acknowledged + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(acknowledged + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_kickoff_that_banto_has_given_up_on_no_longer_holds_the_ring() {
    let (mut state, mut app, director, worker, goinkyo, at) = goinkyo_fixture(Duration::ZERO);
    // Typed KICKOFF_MAX_ATTEMPTS times, the last one's acknowledgement window
    // long over: banto reports the failure and the brigade is really waiting.
    state.pending_goinkyo_kickoffs[0].attempts = KICKOFF_MAX_ATTEMPTS;
    state.pending_goinkyo_kickoffs[0].sent_at = Some(at - 2 * KICKOFF_ACK_TIMEOUT);
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director, &goinkyo], at);
    let all_idle = [(&director, Idle), (&worker, Idle), (&goinkyo, Idle)];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_kickoff_held_for_the_operator_does_not_hold_the_ring() {
    // A Codex Worker whose pane is not at its prompt (a question, a menu): the
    // first due tick finds that out and says so, nothing is typed until the
    // operator answers what the pane shows, and the brigade is really waiting
    // on them.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let codex = host(&mut state, 1, "worker-2", "w2");
    state.pending_kickoffs.push(PendingKickoff {
        key: codex.clone(),
        spawned_at: at,
        notified_not_at_prompt: false,
        turn_seen: false,
        stuck: false,
        sent_at: None,
        attempts: 0,
    });
    state
        .screens
        .get_mut(&codex)
        .unwrap()
        .process(b"[2J[1;1HDo you trust the contents of this directory?");
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director, &codex], at);
    assert!(state.pending_kickoffs[0].notified_not_at_prompt);
    assert!(state.pending_kickoffs[0].sent_at.is_none());
    let all_idle = [(&director, Idle), (&worker, Idle), (&codex, Idle)];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));

    // The same when the operator has a draft in the pane: the retry waits for
    // them, not the other way round.
    let (mut state, mut app, director, worker, goinkyo, at) = goinkyo_fixture(Duration::ZERO);
    let all_idle = [(&director, Idle), (&worker, Idle), (&goinkyo, Idle)];
    // The draft is on the pane's input line, where the screen reader sees it.
    super::tests::draw_prompt_on(
        state.screens.get_mut(&goinkyo).unwrap(),
        Some("a note for later"),
    );
    state.unsent_operator_input.insert(goinkyo.clone());
    tick(&mut state, &mut app, at);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director, &goinkyo], at);
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

// --- a member's start-up does not arm the brigade -------------------------------------------

/// A brigade that has just been opened: a Director, a Codex Worker and two
/// Claude Workers, all freshly started processes. The operator has the switch
/// on and is in the Director's pane (the trial's setup), so a ring would be
/// heard whichever pane it is for.
fn opened_brigade() -> (EmporiumState, App, [SessionKey; 4], Instant) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let director = host_fresh(&mut state, 1, DIRECTOR_TOKEN, "dir");
    let codex = host_fresh(&mut state, 1, "worker-1", "cx");
    let claude_a = host_fresh(&mut state, 1, "worker-2", "ca");
    let claude_b = host_fresh(&mut state, 1, "worker-3", "cb");
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(director.clone()),
        panes: vec![
            director.clone(),
            codex.clone(),
            claude_a.clone(),
            claude_b.clone(),
        ],
        focused: 0,
    };
    state.focus = Focus::Pane;
    let at = t0();
    send(
        &mut state,
        &mut app,
        Event::BellWhenFocusedLoaded { on: true },
        at,
    );
    tick(&mut state, &mut app, at);
    (state, app, [director, codex, claude_a, claude_b], at)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Feed a timeline of `(offset, [dir, codex, claude a, claude b])` samples and
/// report the first ring, if any.
fn replay_timeline(
    state: &mut EmporiumState,
    app: &mut App,
    members: &[SessionKey; 4],
    at: Instant,
    timeline: &[(u64, [Quiescence; 4])],
) -> Option<Duration> {
    for (offset, states) in timeline {
        let samples: Vec<_> = members.iter().zip(states.iter().copied()).collect();
        let cmds = sample(state, app, &samples, at + ms(*offset));
        if rang(&cmds) {
            return Some(ms(*offset));
        }
    }
    None
}

/// Idle for every second of `from..from+secs` after the timeline, one sample a
/// second, to show nothing rings however long the brigade then sits.
fn sit_idle(
    state: &mut EmporiumState,
    app: &mut App,
    members: &[SessionKey; 4],
    at: Instant,
    from_ms: u64,
    secs: u64,
) -> Option<Duration> {
    let timeline: Vec<(u64, [Quiescence; 4])> = (0..secs)
        .map(|s| (from_ms + s * 1000, [Idle, Idle, Idle, Idle]))
        .collect();
    replay_timeline(state, app, members, at, &timeline)
}

/// The first recorded opening: the Claude sessions read unknown, then not idle
/// for a second or two while they start, then idle.
const RUN_A: [(u64, [Quiescence; 4]); 4] = [
    (900, [Unknown, Idle, Unknown, Unknown]),
    (2000, [Idle, Idle, NotIdle, NotIdle]),
    (3000, [Idle, Idle, Idle, NotIdle]),
    (4000, [Idle, Idle, Idle, Idle]),
];

/// The second: the Director too reads not idle while it starts.
const RUN_B: [(u64, [Quiescence; 4]); 3] = [
    (200, [Unknown, Idle, Unknown, Unknown]),
    (1200, [NotIdle, Idle, NotIdle, NotIdle]),
    (2300, [Idle, Idle, Idle, Idle]),
];

#[test]
fn opening_a_brigade_rings_nothing_when_nobody_did_any_work() {
    for (name, run) in [("A", &RUN_A[..]), ("B", &RUN_B[..])] {
        let (mut state, mut app, members, at) = opened_brigade();
        assert_eq!(
            replay_timeline(&mut state, &mut app, &members, at, run),
            None,
            "run {name}: during start-up"
        );
        let last = run.last().unwrap().0;
        assert_eq!(
            sit_idle(&mut state, &mut app, &members, at, last + 1000, 60),
            None,
            "run {name}: after it, however long the brigade sits idle"
        );
    }
}

#[test]
fn a_start_up_reading_still_blocks_a_ring_it_does_not_arm() {
    // Not arming is not the same as not counting: a member reading not idle
    // keeps the brigade from being quiet and breaks the stretch, so a ring that
    // real work armed waits for it.
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    // Real work first: the Director is typed into and works.
    press(&mut state, &mut app, KeyCode::Char('x'), at + ms(100));
    sample(
        &mut state,
        &mut app,
        &[
            (&director, NotIdle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + ms(1000),
    );
    // A sibling is still starting up (not idle) for ten seconds; everyone else
    // is idle: no ring, however long.
    for secs in 2..12 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[
                (&director, Idle),
                (&codex, Idle),
                (&claude_a, Idle),
                (&claude_b, NotIdle),
            ],
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    // It settles: the ring (armed by the Director's real work) comes five
    // seconds after.
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 12 * SECOND,
        at + 12 * SECOND + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(at + 12 * SECOND + BRIGADE_QUIET_SETTLE));
}

#[test]
fn real_work_after_a_quiet_start_still_rings_five_seconds_after() {
    let (mut state, mut app, members, at) = opened_brigade();
    assert_eq!(
        replay_timeline(&mut state, &mut app, &members, at, &RUN_A),
        None
    );
    // The brigade has been quiet since 4.0 s. Now the operator types into the
    // Director, which works for a while and finishes.
    let [director, codex, claude_a, claude_b] = members.clone();
    let typed_at = at + 20 * SECOND;
    press(&mut state, &mut app, KeyCode::Char('x'), typed_at);
    let working = [
        (&director, NotIdle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    assert!(!rang(&sample(
        &mut state,
        &mut app,
        &working,
        typed_at + SECOND
    )));
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    let idle_from = typed_at + 3 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_nudge_right_after_start_up_arms_the_brigade() {
    // (a) A Claude Worker nudged before it was ever read idle.
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    let mail = || vec![mail_for(1, "worker-2", &claude_a)];
    sample_with_relay(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, Unknown),
            (&claude_b, Unknown),
        ],
        mail(),
        at + SECOND,
    );
    let cmds = sample_with_relay(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, Unknown),
            (&claude_b, Unknown),
        ],
        mail(),
        at + 2 * SECOND,
    );
    assert!(typed_into(&cmds, &claude_a), "the nudge went out: {cmds:?}");
    // Its first reading is not idle — because of the nudge, which it reads.
    sample_with_relay(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, NotIdle),
            (&claude_b, Idle),
        ],
        vec![{
            let mut read = mail_for(1, "worker-2", &claude_a);
            read.has_unseen = false;
            read
        }],
        at + 3 * SECOND,
    );
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 4 * SECOND,
        at + 30 * SECOND,
    );
    assert!(rang_at.is_some(), "armed by the nudge");
}

#[test]
fn a_kickoff_typed_into_a_fresh_codex_worker_arms_the_brigade() {
    // (b) Its first known state is not idle because its kickoff was typed.
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    state.last_prompted.insert(codex.clone(), at + SECOND);
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, NotIdle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + 2 * SECOND,
    );
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 3 * SECOND,
        at + 30 * SECOND,
    );
    assert!(rang_at.is_some(), "armed by the kickoff");
}

#[test]
fn operator_input_into_the_director_before_it_was_ever_idle_arms_the_brigade() {
    // (c)
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    press(&mut state, &mut app, KeyCode::Char('x'), at + ms(500));
    sample(
        &mut state,
        &mut app,
        &[
            (&director, NotIdle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + SECOND,
    );
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 2 * SECOND,
        at + 30 * SECOND,
    );
    assert!(rang_at.is_some(), "armed by the operator's input");
}

#[test]
fn a_resumed_member_nobody_touched_that_stays_busy_on_its_own_never_arms() {
    // (d) Thirty seconds of its own start-up work, no input anywhere.
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    for secs in 0..30 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[
                (&director, Idle),
                (&codex, Idle),
                (&claude_a, NotIdle),
                (&claude_b, Idle),
            ],
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    // Idle at last: not armed, so nothing rings.
    assert_eq!(
        tick_each_second_until(
            &mut state,
            &mut app,
            &all_idle,
            at + 30 * SECOND,
            at + 60 * SECOND
        ),
        None
    );
    // From then on it counts as a member that has been idle, and its next work
    // arms the brigade like anyone's.
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, NotIdle),
            (&claude_b, Idle),
        ],
        at + 61 * SECOND,
    );
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 62 * SECOND,
        at + 100 * SECOND,
    );
    assert!(rang_at.is_some());
}

#[test]
fn a_brigade_armed_by_real_work_stays_armed_when_a_new_member_starts_up() {
    // (e)
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    press(&mut state, &mut app, KeyCode::Char('x'), at + ms(500));
    sample(
        &mut state,
        &mut app,
        &[
            (&director, NotIdle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + SECOND,
    );
    // A new Worker joins and starts up: not idle for a few seconds.
    let newcomer = host_fresh(&mut state, 1, "worker-4", "cn");
    for secs in 2..6 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[
                (&director, Idle),
                (&codex, Idle),
                (&claude_a, Idle),
                (&claude_b, Idle),
                (&newcomer, NotIdle),
            ],
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
        (&newcomer, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 6 * SECOND,
        at + 30 * SECOND,
    );
    assert_eq!(rang_at, Some(at + 6 * SECOND + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_member_relaunched_under_the_same_key_starts_its_start_up_over() {
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    // The Worker was seen idle: its later work would arm the brigade ...
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + SECOND,
    );
    // ... but its process is replaced, and the new one reads not idle while it
    // starts: that is start-up again.
    unhost(&mut state, &claude_a);
    let claude_a = host_fresh(&mut state, 1, "worker-2", "ca");
    for secs in 2..6 {
        sample(
            &mut state,
            &mut app,
            &[
                (&director, Idle),
                (&codex, Idle),
                (&claude_a, NotIdle),
                (&claude_b, Idle),
            ],
            at + secs * SECOND,
        );
    }
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&claude_a, Idle),
        (&claude_b, Idle),
    ];
    assert_eq!(
        tick_each_second_until(
            &mut state,
            &mut app,
            &all_idle,
            at + 6 * SECOND,
            at + 60 * SECOND
        ),
        None
    );
}

#[test]
fn a_pane_whose_key_changes_keeps_what_its_process_had_seen() {
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + SECOND,
    );
    // Discovery gives the same process its real id; its next work, past the
    // start-up grace, arms.
    let renamed = key("ca-discovered");
    state.rekey_pane_state(&claude_a, &renamed);
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&renamed, NotIdle),
            (&claude_b, Idle),
        ],
        at + 12 * SECOND,
    );
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&renamed, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 13 * SECOND,
        at + 40 * SECOND,
    );
    assert!(rang_at.is_some());
}

#[test]
fn a_pane_whose_key_changes_keeps_the_start_up_grace_of_its_process() {
    // The same rename inside the grace: the process was spawned a second ago, so
    // its work reading is still its start-up, whatever its key is called now.
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&claude_a, Idle),
            (&claude_b, Idle),
        ],
        at + SECOND,
    );
    let renamed = key("ca-discovered");
    state.rekey_pane_state(&claude_a, &renamed);
    sample(
        &mut state,
        &mut app,
        &[
            (&director, Idle),
            (&codex, Idle),
            (&renamed, NotIdle),
            (&claude_b, Idle),
        ],
        at + 2 * SECOND,
    );
    let all_idle = [
        (&director, Idle),
        (&codex, Idle),
        (&renamed, Idle),
        (&claude_b, Idle),
    ];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        at + 3 * SECOND,
        at + 9 * SECOND,
    );
    assert_eq!(rang_at, None);
}

/// The Resumed brigade with the operator inside Claude Worker `c1`'s pane, every
/// process long past its start-up and read idle.
fn resumed_with_the_operator_in_a_worker() -> Resumed {
    let mut run = Resumed::new();
    if let Stage::Brigade { panes, focused, .. } = &mut run.state.stage {
        *focused = panes.iter().position(|pane| pane.as_str() == "c1").unwrap();
    }
    run.sample_at(15_000, Idle, Idle);
    run
}

/// After a not-idle reading of `c1` one second after its relaunch, everyone idle:
/// whether the brigade rang.
fn rang_after_the_relaunched_worker_reads_busy(run: &mut Resumed, relaunched_ms: u64) -> bool {
    let at = |ms: u64| run.start + Duration::from_millis(ms);
    let c1 = key("c1");
    let busy_at = at(relaunched_ms + 1000);
    let readings = [
        (&run.director, Idle),
        (&run.codex, Idle),
        (&c1, NotIdle),
        (&run.claude[1], Idle),
    ];
    sample(&mut run.state, &mut run.app, &readings, busy_at);
    let idle = [
        (&run.director, Idle),
        (&run.codex, Idle),
        (&c1, Idle),
        (&run.claude[1], Idle),
    ];
    (2..=9).any(|secs| {
        rang(&sample(
            &mut run.state,
            &mut run.app,
            &idle,
            at(relaunched_ms + 1000 + secs * 1000),
        ))
    })
}

#[test]
fn input_given_to_a_process_that_was_replaced_is_not_work_given_to_the_new_one() {
    // A key typed into the Worker's pane; the pane exits; the member is relaunched
    // under the same key, later. Through real events, the state a real relaunch
    // keeps. The new process reads busy inside its start-up: not armed.
    let mut run = resumed_with_the_operator_in_a_worker();
    let typed = run.start + 30 * SECOND;
    press(&mut run.state, &mut run.app, KeyCode::Char('x'), typed);
    assert!(run.state.last_given_input.contains_key(&key("c1")));
    let relaunched = typed + 40 * SECOND;
    let cmds = send(
        &mut run.state,
        &mut run.app,
        Event::PtyExited {
            key: key("c1"),
            reason: Some(PtyExitReason::Code(0)),
        },
        relaunched,
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { key, .. } if key.as_str() == "c1")),
        "relaunched: {cmds:?}"
    );
    send(
        &mut run.state,
        &mut run.app,
        Event::Spawned {
            key: key("c1"),
            agent: None,
        },
        relaunched,
    );
    assert!(!run.state.was_given_work(&key("c1")));
    assert!(
        !rang_after_the_relaunched_worker_reads_busy(&mut run, 70_000),
        "the old process's key is not work given to the new one"
    );
}

#[test]
fn a_key_in_the_same_drain_as_the_relaunch_is_not_work_given_to_the_new_process() {
    // One drain of the event loop stamps every event with the same instant, and
    // the exit, a key typed into the dying pane and the `Spawned` that answers the
    // relaunch can all be in it, in that order. The key was given to the process
    // that was already gone.
    let mut run = resumed_with_the_operator_in_a_worker();
    let at = run.start + 30 * SECOND;
    send(
        &mut run.state,
        &mut run.app,
        Event::PtyExited {
            key: key("c1"),
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    );
    press(&mut run.state, &mut run.app, KeyCode::Char('x'), at);
    send(
        &mut run.state,
        &mut run.app,
        Event::Spawned {
            key: key("c1"),
            agent: None,
        },
        at,
    );
    assert!(!run.state.was_given_work(&key("c1")));
    // The relay's own record of the operator's input is as it was: a nudge is
    // still held back for a pane the operator was just typing into.
    assert!(run.state.last_forwarded_input.contains_key(&key("c1")));
    assert!(
        !rang_after_the_relaunched_worker_reads_busy(&mut run, 30_000),
        "its start-up reading is not armed by a key meant for the old pane"
    );
}

#[test]
fn what_banto_wrote_to_the_old_process_in_the_same_drain_is_not_work_given_to_the_new_one() {
    // The same for banto's own writes: a nudge typed at the old process, and its
    // delayed Enter written by a tick at the instant of the exit and of the new
    // process's `Spawned`, all in one drain.
    let mut run = resumed_with_the_operator_in_a_worker();
    let at = run.start + 30 * SECOND;
    let c1 = key("c1");
    let mail = |run: &mut Resumed, when: Instant| {
        send(
            &mut run.state,
            &mut run.app,
            Event::Tick {
                relay: vec![mail_for(1, "worker-2", &key("c1"))],
                brigades: Vec::new(),
            },
            when,
        )
    };
    mail(&mut run, at - 3 * SECOND);
    let nudged = mail(&mut run, at - 2 * SECOND);
    assert!(
        typed_into(&nudged, &c1),
        "the nudge went to the old process"
    );
    // Its delayed submitting Enter goes out in the tick at the very instant the
    // process is replaced.
    let submitted = mail(&mut run, at);
    assert!(typed_into(&submitted, &c1), "and so did its Enter");
    assert!(run.state.last_prompted.contains_key(&c1));
    send(
        &mut run.state,
        &mut run.app,
        Event::PtyExited {
            key: c1.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    );
    send(
        &mut run.state,
        &mut run.app,
        Event::Spawned {
            key: c1.clone(),
            agent: None,
        },
        at,
    );
    assert!(!run.state.was_given_work(&c1));
    assert!(
        !rang_after_the_relaunched_worker_reads_busy(&mut run, 30_000),
        "its start-up reading is not armed by banto's nudge to the old process"
    );
}

// --- a brigade is not quiet while mail the relay will deliver is unread ------------------

/// The Director's relay entry: mail waiting (or, with `unread` false, read).
fn director_mail(unread: bool, director: &SessionKey) -> Vec<RelayObservation> {
    let mut observation = mail_for(1, DIRECTOR_TOKEN, director);
    observation.has_unseen = unread;
    vec![observation]
}

#[test]
fn a_brigade_with_unread_mail_held_back_by_the_operator_does_not_ring_until_it_is_delivered() {
    // The trial's sequence: a Worker's report is waiting for the Director, every
    // member reads idle, and the nudge is held for thirty seconds because the
    // operator is at the Director's pane.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        director_mail(true, &director),
        at,
    );
    state.unsent_operator_input.insert(director.clone());
    for secs in 1..=30 {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            at + secs * SECOND,
        );
        assert!(!typed_into(&cmds, &director), "held at +{secs}s: {cmds:?}");
        assert!(
            !rang(&cmds),
            "mail is waiting, not quiet: at +{secs}s: {cmds:?}"
        );
    }

    // The hold ends; the nudge is typed (and its Enter a tick later) ...
    state.unsent_operator_input.remove(&director);
    let mut typed_at = None;
    for secs in 31..=34 {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            at + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
        if typed_at.is_none() && typed_into(&cmds, &director) {
            typed_at = Some(secs);
        }
    }
    assert!(typed_at.is_some(), "the nudge was delivered");
    // ... the Director reads its mail and works, then goes idle again.
    let working = [(&director, NotIdle), (&worker, Idle)];
    sample_with_relay(
        &mut state,
        &mut app,
        &working,
        director_mail(false, &director),
        at + 35 * SECOND,
    );
    let idle_from = at + 37 * SECOND;
    let mut rang_at = None;
    let mut when = idle_from;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(false, &director),
            when,
        );
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_relay_that_has_given_up_on_the_mail_no_longer_holds_the_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    // Three nudges were tried on this batch and none was answered (set after the
    // ticks above, whose observations had no mail and so cleared the relay state).
    state.relay_states.insert(
        (1, DIRECTOR_TOKEN.to_string()),
        RelayState {
            idle_streak: 9,
            nudge: NudgeState {
                last_nudge: Some(at),
                attempts: RELAY_MAX_ATTEMPTS,
            },
        },
    );
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let mut rang_at = None;
    let mut when = idle_from;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            when,
        );
        assert!(
            !typed_into(&cmds, &director),
            "the relay does not try again"
        );
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn mail_the_relay_will_not_deliver_because_it_is_off_does_not_hold_the_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let manual = BrigadeConfig {
        relay: crate::config::RelayMode::Manual,
        ..BrigadeConfig::default()
    };
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let mut rang_at = None;
    let mut when = idle_from;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE {
        let samples = all_idle
            .iter()
            .map(|(k, q)| ((*k).clone(), *q))
            .collect::<Vec<_>>();
        update(
            &mut state,
            &mut app,
            &manual,
            Event::QuiescenceSampled { samples },
            when,
        );
        let brigades = listed(&state);
        let relay = observed(&state, director_mail(true, &director));
        let cmds = update(
            &mut state,
            &mut app,
            &manual,
            Event::Tick { relay, brigades },
            when,
        );
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn unread_mail_in_another_brigade_or_for_a_member_nobody_hosts_does_not_hold_this_one() {
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");
    let busy = [(&d1, Idle), (&w1, NotIdle), (&d2, Idle), (&w2, NotIdle)];
    sample(&mut state, &mut app, &busy, at);
    let idle_from = at + SECOND;
    let all_idle = [(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)];

    // Brigade 2's Director has mail waiting (held by a draft); a member that is
    // not hosted at all also has some.
    state.unsent_operator_input.insert(d2.clone());
    let ghost = key("ghost");
    let mail = || {
        let mut ghost_mail = mail_for(1, "worker-9", &ghost);
        ghost_mail.has_unseen = true;
        vec![mail_for(2, DIRECTOR_TOKEN, &d2), ghost_mail]
    };
    let mut rang_at = None;
    let mut when = idle_from;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE {
        let cmds = sample_with_relay(&mut state, &mut app, &all_idle, mail(), when);
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(
        rang_at,
        Some(idle_from + BRIGADE_QUIET_SETTLE),
        "brigade 1 on time"
    );
    assert!(state.attention.contains_key(&1));
    assert!(
        !state.attention.contains_key(&2),
        "brigade 2 still has mail to deliver"
    );
}

// --- a brigade that has been disbanded is not watched ----------------------------------------

/// The disband of `brigade` as the shell reports it after the store deleted the
/// brigade: the Director's pane stays open on its own, the other panes are
/// killed and then report their exit.
fn disband(
    state: &mut EmporiumState,
    app: &mut App,
    brigade: BrigadeId,
    others: &[&SessionKey],
    at: Instant,
) {
    let cmds = send(
        state,
        app,
        Event::Disbanded {
            brigade_id: brigade,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        at,
    );
    for other in others {
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::HangUpPty { key } if key == *other)),
            "{other:?} is hung up: {cmds:?}"
        );
        send(
            state,
            app,
            Event::PtyExited {
                key: (*other).clone(),
                reason: Some(PtyExitReason::Code(0)),
            },
            at,
        );
    }
}

#[test]
fn a_former_director_left_open_on_its_own_does_not_ring_for_a_brigade_that_no_longer_exists() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // Real work arms the brigade.
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);

    // The brigade is disbanded while the Director's pane stays open.
    disband(&mut state, &mut app, 1, &[&worker], idle_from);
    assert!(matches!(&state.stage, Stage::Solo(key) if key == &director));

    // The Director now sits idle on its own for far longer than the settle.
    let alone = [(&director, Idle)];
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &alone,
        idle_from + SECOND,
        idle_from + 60 * SECOND,
    );
    assert_eq!(rang_at, None, "a brigade that is gone is not quiet");
    assert!(state.latched_attention_panes.is_empty() && state.attention.is_empty());
    assert!(state.brigade_quiet.is_empty(), "its watch went with it");
}

#[test]
fn a_disband_that_fails_leaves_the_membership_and_the_watch_as_they_were() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];

    // The operator confirmed the disband: while it is in flight, even for longer
    // than the settle, nothing rings for a brigade that is about to be gone, and
    // its watch is kept.
    state.disbanding.insert(1);
    for secs in 1..=7 {
        let cmds = sample(&mut state, &mut app, &all_idle, idle_from + secs * SECOND);
        assert!(!rang(&cmds), "in flight, at +{secs}s: {cmds:?}");
    }
    assert!(state.brigade_quiet.contains_key(&1));

    // The store refused: the brigade lives on, staged as it was, and its stretch
    // carries on from where it was rather than starting over.
    let refused_at = idle_from + 8 * SECOND;
    let cmds = send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Err("database is locked".to_string()),
        },
        refused_at,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(matches!(&state.stage, Stage::Brigade { id: 1, .. }));
    assert_eq!(state.member_brigades.get(&director), Some(&1));
    assert_eq!(
        state.member_tokens.get(&director).map(String::as_str),
        Some(DIRECTOR_TOKEN)
    );
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        refused_at,
        refused_at + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(
        rang_at,
        Some(refused_at),
        "the old stretch had long matured"
    );
}

#[test]
fn a_brigade_formed_again_around_the_same_director_starts_with_a_fresh_watch() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // The first brigade did real work and was disbanded; the Director stays.
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    disband(&mut state, &mut app, 1, &[&worker], idle_from);

    // A new brigade (a new id) is formed around the same Director session.
    let newcomer = host(&mut state, 2, "worker-1", "w2");
    state.member_brigades.insert(director.clone(), 2);
    stage(&mut state, 2, &director, &[&newcomer]);
    state.focus = Focus::Sidebar;
    let formed = idle_from + 2 * SECOND;
    let both = [(&director, Idle), (&newcomer, Idle)];
    // Nothing the first brigade did arms it: quiet from the start is not quiet
    // after work.
    let rang_at = tick_each_second_until(&mut state, &mut app, &both, formed, formed + 30 * SECOND);
    assert_eq!(rang_at, None);

    // Its own work does.
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&newcomer, NotIdle)],
        formed + 31 * SECOND,
    );
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        formed + 32 * SECOND,
        formed + 60 * SECOND,
    );
    assert_eq!(rang_at, Some(formed + 32 * SECOND + BRIGADE_QUIET_SETTLE));
}

#[test]
fn an_armed_brigade_stays_armed_when_a_member_is_relaunched() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // Real work arms it ...
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    // ... then a Worker's process is replaced and reads not idle while it starts.
    unhost(&mut state, &worker);
    let worker = host_fresh(&mut state, 1, "worker-1", "w1");
    for secs in 1..=3 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, NotIdle)],
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "start-up at +{secs}s: {cmds:?}");
    }
    // It idles: the brigade rings five seconds after, armed by the earlier work.
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let idle_again = idle_from + 4 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        idle_again,
        idle_again + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(idle_again + BRIGADE_QUIET_SETTLE));
}

#[test]
fn nothing_the_brigade_kept_outlives_its_disband() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    assert!(state.brigade_quiet.contains_key(&1));
    disband(&mut state, &mut app, 1, &[&worker], idle_from + SECOND);
    assert!(state.brigade_quiet.is_empty());
    // The pane itself is untouched: still hosted, still staged on its own.
    assert!(state.screens.contains_key(&director));
    assert!(matches!(&state.stage, Stage::Solo(key) if key == &director));
    // The quit question is per screen and does not care about the brigade: the
    // former Director counts by its own sample.
    assert_eq!(
        state.quit_counts(&app, idle_from + 60 * SECOND).not_idle(),
        0
    );
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle)],
        idle_from + 61 * SECOND,
    );
    assert_eq!(
        state.quit_counts(&app, idle_from + 61 * SECOND).not_idle(),
        1
    );
}

// --- the bell decides from what was observed in this tick ----------------------------------

/// A tick carrying exactly the relay observations given (none, for a roster
/// that could not be read) and no more.
fn raw_tick(
    state: &mut EmporiumState,
    app: &mut App,
    samples: &[(&SessionKey, Quiescence)],
    relay: Vec<RelayObservation>,
    at: Instant,
) -> Vec<Cmd> {
    let samples = samples
        .iter()
        .map(|(key, q)| ((*key).clone(), *q))
        .collect::<Vec<_>>();
    let mut cmds = send(state, app, Event::QuiescenceSampled { samples }, at);
    let brigades = listed(state);
    cmds.extend(send(state, app, Event::Tick { relay, brigades }, at));
    cmds
}

/// A tick as the shell sends it when the roster could not be read.
fn blind_tick(
    state: &mut EmporiumState,
    app: &mut App,
    samples: &[(&SessionKey, Quiescence)],
    at: Instant,
) -> Vec<Cmd> {
    raw_tick(state, app, samples, Vec::new(), at)
}

#[test]
fn during_a_roster_outage_no_brigade_rings_and_the_stretch_starts_over_afterwards() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    // Observed fine at the start of the stretch ...
    sample_with_relay(
        &mut state,
        &mut app,
        &all_idle,
        director_mail(false, &director),
        idle_from,
    );
    // ... then the roster cannot be read, for much longer than the settle, with
    // every member sampling idle: nothing is known about the store, so nothing
    // rings.
    let outage_end = idle_from + 4 * BRIGADE_QUIET_SETTLE;
    let mut when = idle_from + SECOND;
    while when < outage_end {
        let cmds = blind_tick(&mut state, &mut app, &all_idle, when);
        assert!(
            !rang(&cmds),
            "blind at +{}s: {cmds:?}",
            (when - idle_from).as_secs()
        );
        when += SECOND;
    }

    // Observations come back, the mail was read meanwhile: the stretch starts
    // over from here (the brigade is still armed) and rings a settle later.
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(false, &director),
        outage_end,
        outage_end + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(outage_end + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_brigade_that_stops_appearing_in_the_observations_never_rings() {
    // The shape of a brigade deleted from the store by something else: its panes
    // are still hosted and still sample idle, but no tick observes it any more.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    sample_with_relay(
        &mut state,
        &mut app,
        &all_idle,
        director_mail(false, &director),
        idle_from,
    );
    for secs in 1..=60 {
        // Other brigades are still observed: the store is being read.
        let elsewhere = vec![mail_for(7, DIRECTOR_TOKEN, &key("elsewhere"))];
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &all_idle,
            elsewhere,
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
}

#[test]
fn mail_observed_before_an_outage_holds_nothing_afterwards_unless_it_is_observed_again() {
    let at_start = || {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
        let all_idle = [(&director, Idle), (&worker, Idle)];
        // Mail is waiting and the nudge is held by a draft.
        state.unsent_operator_input.insert(director.clone());
        sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            idle_from,
        );
        // The roster cannot be read for three settles.
        let outage_end = idle_from + 3 * BRIGADE_QUIET_SETTLE;
        let mut when = idle_from + SECOND;
        while when < outage_end {
            let cmds = blind_tick(&mut state, &mut app, &all_idle, when);
            assert!(!rang(&cmds), "blind: {cmds:?}");
            when += SECOND;
        }
        (state, app, director, worker, outage_end)
    };

    // It comes back and the mail was read meanwhile: the old record decides
    // nothing, and the brigade rings a settle after the observations returned.
    let (mut state, mut app, director, worker, outage_end) = at_start();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(false, &director),
        outage_end,
        outage_end + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(outage_end + BRIGADE_QUIET_SETTLE));

    // It comes back and the mail is observed unread: pending again, from this
    // tick's own observation.
    let (mut state, mut app, director, worker, outage_end) = at_start();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(true, &director),
        outage_end,
        outage_end + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, None);
}

#[test]
fn unanswered_mail_holds_the_ring_through_the_relays_retries_and_releases_it_afterwards() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    // The Director never reads its mail: every tick observes it unread while
    // every member samples idle. The relay nudges at the idle streak, again after
    // each cooldown, and gives up at RELAY_MAX_ATTEMPTS.
    let mut nudge_lines = Vec::new();
    let mut rang_at = None;
    let mut when = idle_from;
    while when <= idle_from + 400 * SECOND {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            when,
        );
        if cmds.iter().any(|cmd| {
            matches!(cmd, Cmd::WritePty { key, bytes } if key == &director && bytes.as_slice() == RELAY_NUDGE_LINE.as_bytes())
        }) {
            nudge_lines.push(when);
        }
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(
        nudge_lines.len() as u32,
        RELAY_MAX_ATTEMPTS,
        "{nudge_lines:?}"
    );
    // The last attempt's submitting Enter is typed on the tick after its line,
    // and the brigade rings a settle after that last write.
    let last_enter = *nudge_lines.last().unwrap() + SECOND;
    assert_eq!(rang_at, Some(last_enter + BRIGADE_QUIET_SETTLE));
}

#[test]
fn switching_the_relay_to_manual_in_the_middle_of_a_hold_releases_the_ring_after_a_settle() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    state.unsent_operator_input.insert(director.clone());
    for secs in 0..20 {
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &all_idle,
            director_mail(true, &director),
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "held at +{secs}s: {cmds:?}");
    }

    // The relay is switched off: the mail will not be delivered by banto any
    // more, so the hold ends and the stretch starts from this tick.
    let manual = BrigadeConfig {
        relay: crate::config::RelayMode::Manual,
        ..BrigadeConfig::default()
    };
    let switched = idle_from + 20 * SECOND;
    let mut rang_at = None;
    let mut when = switched;
    while when <= switched + 2 * BRIGADE_QUIET_SETTLE {
        let samples = all_idle
            .iter()
            .map(|(k, q)| ((*k).clone(), *q))
            .collect::<Vec<_>>();
        update(
            &mut state,
            &mut app,
            &manual,
            Event::QuiescenceSampled { samples },
            when,
        );
        let brigades = listed(&state);
        let relay = observed(&state, director_mail(true, &director));
        let cmds = update(
            &mut state,
            &mut app,
            &manual,
            Event::Tick { relay, brigades },
            when,
        );
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(switched + BRIGADE_QUIET_SETTLE));
}

#[test]
fn unread_mail_belongs_to_the_pane_the_observation_names_after_a_rekey_or_a_relaunch() {
    // Rekey: discovery gives the same process its real key; the observation under
    // the new key says whether mail is waiting.
    for (unread_after, rings) in [(true, false), (false, true)] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
        let renamed = key("dir-discovered");
        state.rekey_pane_state(&director, &renamed);
        let all_idle = [(&renamed, Idle), (&worker, Idle)];
        let mut observation = mail_for(1, DIRECTOR_TOKEN, &renamed);
        observation.has_unseen = unread_after;
        let rang_at = tick_each_second_until_seeing(
            &mut state,
            &mut app,
            &all_idle,
            || vec![observation.clone()],
            idle_from,
            idle_from + 2 * BRIGADE_QUIET_SETTLE,
        );
        assert_eq!(
            rang_at.is_some(),
            rings,
            "unread under the new key: {unread_after}"
        );
    }

    // Relaunch under the same key: the new process's own observation decides, and
    // the brigade is not quiet while it still has mail to receive.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    unhost(&mut state, &worker);
    let worker = host(&mut state, 1, "worker-1", "w1");
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || vec![mail_for(1, "worker-1", &worker)],
        idle_from,
        idle_from + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, None, "mail for the new process is pending");
}

// --- an observation counts only for the pane the core holds for that member -----------------

/// Armed brigade 1 (Director `dir`, Worker `w1`), every member idle from the
/// returned instant, and an unrelated pane of brigade 2.
fn armed_with_a_stranger() -> (
    EmporiumState,
    App,
    SessionKey,
    SessionKey,
    SessionKey,
    Instant,
) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let stranger = host(&mut state, 2, "worker-1", "stranger");
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle), (&stranger, Idle)],
        idle_from,
    );
    (state, app, director, worker, stranger, idle_from)
}

fn claim(brigade: BrigadeId, token: &str, pane: &SessionKey, unread: bool) -> RelayObservation {
    let mut observation = mail_for(brigade, token, pane);
    observation.has_unseen = unread;
    observation
}

#[test]
fn an_observation_that_names_a_director_under_another_pane_does_not_count_as_observing_it() {
    let nothing_hosts_this = key("nothing-hosts-this");
    for case in 0..3 {
        let (mut state, mut app, director, worker, stranger, idle_from) = armed_with_a_stranger();
        let members = [(&director, Idle), (&worker, Idle), (&stranger, Idle)];
        // The only observations name brigade 1's Director token — under the
        // Worker's pane, under a pane of another brigade, under a key nothing
        // hosts — never under the Director's own pane.
        let claimed = match case {
            0 => worker.clone(),
            1 => stranger.clone(),
            _ => nothing_hosts_this.clone(),
        };
        for secs in 1..=60 {
            let cmds = raw_tick(
                &mut state,
                &mut app,
                &members,
                vec![claim(1, DIRECTOR_TOKEN, &claimed, false)],
                idle_from + secs * SECOND,
            );
            assert!(!rang(&cmds), "case {case}, at +{secs}s: {cmds:?}");
        }
        // The stretch was broken as in any tick without an observation of the
        // Director: once the Director is observed, a whole settle has to pass.
        let back = idle_from + 61 * SECOND;
        let rang_at = tick_each_second_until(
            &mut state,
            &mut app,
            &members,
            back,
            back + 2 * BRIGADE_QUIET_SETTLE,
        );
        assert_eq!(rang_at, Some(back + BRIGADE_QUIET_SETTLE), "case {case}");
    }
}

#[test]
fn an_unread_bit_applies_only_to_the_pane_that_is_that_member() {
    let (mut state, mut app, director, worker, stranger, idle_from) = armed_with_a_stranger();
    let members = [(&director, Idle), (&worker, Idle), (&stranger, Idle)];
    // The genuine observations carry no mail. Two stray ones say the Director's
    // pane has mail "for worker-9" and the Worker's pane has mail "for the
    // Director": neither is what the core holds for that pane. (They report the
    // member busy so the relay itself has nothing to type for them.)
    let stray = |brigade, token: &str, pane: &SessionKey| {
        let mut observation = claim(brigade, token, pane, true);
        observation.is_idle_this_tick = Some(false);
        observation
    };
    let mut rang_at = None;
    let mut when = idle_from + SECOND;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE {
        let mut relay = observed(&state, Vec::new());
        relay.push(stray(1, "worker-9", &director));
        relay.push(stray(1, DIRECTOR_TOKEN, &worker));
        let cmds = raw_tick(&mut state, &mut app, &members, relay, when);
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(idle_from + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_director_hosted_without_a_session_id_is_not_observed_so_its_brigade_is_not_judged() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    // The store records no session for the Director yet (a fresh formation before
    // discovery), so the relay observes the Worker and nobody else of the brigade.
    for secs in 0..=30 {
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &all_idle,
            vec![claim(1, "worker-1", &worker, false)],
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    // Discovery records it: observed from here, a settle later it rings.
    let found = idle_from + 31 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        found,
        found + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(found + BRIGADE_QUIET_SETTLE));
}

#[test]
fn switching_the_relay_back_to_automatic_resumes_the_hold_on_pending_mail() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    state.unsent_operator_input.insert(director.clone());
    let manual = BrigadeConfig {
        relay: crate::config::RelayMode::Manual,
        ..BrigadeConfig::default()
    };
    let auto = BrigadeConfig::default();
    let step = |state: &mut EmporiumState, app: &mut App, config: &BrigadeConfig, when| {
        let samples = all_idle
            .iter()
            .map(|(k, q)| ((*k).clone(), *q))
            .collect::<Vec<_>>();
        update(
            state,
            app,
            config,
            Event::QuiescenceSampled { samples },
            when,
        );
        let brigades = listed(state);
        let relay = observed(state, director_mail(true, &director));
        update(state, app, config, Event::Tick { relay, brigades }, when)
    };

    // Held under Auto, then Manual for a few seconds (the hold ends, a stretch
    // starts), then Auto again before it has run a settle: the hold resumes and
    // the stretch is broken again, so nothing rings however long it lasts.
    for secs in 0..10 {
        let cmds = step(&mut state, &mut app, &auto, idle_from + secs * SECOND);
        assert!(!rang(&cmds), "auto at +{secs}s: {cmds:?}");
    }
    for secs in 10..13 {
        let cmds = step(&mut state, &mut app, &manual, idle_from + secs * SECOND);
        assert!(!rang(&cmds), "manual at +{secs}s: {cmds:?}");
    }
    for secs in 13..60 {
        let cmds = step(&mut state, &mut app, &auto, idle_from + secs * SECOND);
        assert!(!rang(&cmds), "auto again at +{secs}s: {cmds:?}");
    }
    // The mail is finally read: a settle after that, it rings.
    let read_at = idle_from + 60 * SECOND;
    let rang_at = tick_each_second_until_seeing(
        &mut state,
        &mut app,
        &all_idle,
        || director_mail(false, &director),
        read_at,
        read_at + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(read_at + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_tick_gathered_before_a_rekey_is_ignored_and_the_next_one_is_not() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    // Discovery has renamed the Director's pane; the store still says the old id.
    let renamed = key("dir-discovered");
    state.rekey_pane_state(&director, &renamed);
    let members = [(&renamed, Idle), (&worker, Idle)];
    for secs in 0..=3 {
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &members,
            vec![
                claim(1, DIRECTOR_TOKEN, &director, false),
                claim(1, "worker-1", &worker, false),
            ],
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "stale at +{secs}s: {cmds:?}");
    }
    // The store caught up: observed under the new key, a settle from here.
    let caught_up = idle_from + 4 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &members,
        caught_up,
        caught_up + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(caught_up + BRIGADE_QUIET_SETTLE));
}

// --- legitimate transients of the observation match ---------------------------------------------

#[test]
fn a_member_added_to_an_armed_brigade_only_delays_the_ring_and_its_own_rules_apply() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let both = [(&director, Idle), (&worker, Idle)];

    // A Codex Worker is being added: its membership and its kickoff are
    // installed before its pane exists.
    let added = key("w2");
    state
        .member_tokens
        .insert(added.clone(), "worker-2".to_string());
    state.member_brigades.insert(added.clone(), 1);
    state.pending_kickoffs.push(PendingKickoff {
        key: added.clone(),
        spawned_at: idle_from,
        notified_not_at_prompt: false,
        turn_seen: false,
        stuck: false,
        sent_at: None,
        attempts: 0,
    });
    // Until the screen is hosted an observation naming it is not about a pane
    // the core holds, and is ignored; the brigade is still judged through its
    // Director and has not matured yet.
    for secs in 1..=2 {
        let mut relay = observed(&state, Vec::new());
        relay.push(claim(1, "worker-2", &added, true));
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &both,
            relay,
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "not hosted yet, at +{secs}s: {cmds:?}");
    }

    // The pane is hosted: a new process joins the set, restarting the stretch,
    // reads busy while it starts, then idle — but banto still owes it its kickoff.
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: added.clone(),
            agent: None,
        },
        idle_from + 3 * SECOND,
    );
    // The pane is at its prompt and reports idle: it is typed when the quiet
    // period is over, and owed until the turn is acknowledged.
    super::tests::paint_codex_prompt(&mut state, &added, Some("idle"));
    let with_added =
        |added_state: Quiescence| [(&director, Idle), (&worker, Idle), (&added, added_state)];
    for secs in 3..=4 {
        let relay = observed(&state, Vec::new());
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &with_added(NotIdle),
            relay,
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "starting up, at +{secs}s: {cmds:?}");
    }
    for secs in 5..40 {
        let relay = observed(&state, Vec::new());
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &with_added(Idle),
            relay,
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "kickoff owed, at +{secs}s: {cmds:?}");
    }

    // The kickoff is acknowledged: the brigade, still armed by its earlier work,
    // rings a settle after the newcomer stopped being owed.
    state.pending_kickoffs.clear();
    let acknowledged = idle_from + 40 * SECOND;
    let mut rang_at = None;
    let mut when = acknowledged;
    while when <= acknowledged + 2 * BRIGADE_QUIET_SETTLE {
        let relay = observed(&state, Vec::new());
        let cmds = raw_tick(&mut state, &mut app, &with_added(Idle), relay, when);
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    assert_eq!(rang_at, Some(acknowledged + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_brigade_formed_again_around_the_same_director_session_hands_over_without_a_stale_ring() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    disband(&mut state, &mut app, 1, &[&worker], idle_from);

    // The new brigade (id 2) takes over the Director's membership and hosts its
    // Worker, but the relay gathers for a few ticks from the old formation: the
    // old brigade's Director under the same pane.
    let newcomer = host(&mut state, 2, "worker-1", "w2");
    state.member_brigades.insert(director.clone(), 2);
    stage(&mut state, 2, &director, &[&newcomer]);
    let both = [(&director, Idle), (&newcomer, Idle)];
    for secs in 1..=10 {
        let cmds = raw_tick(
            &mut state,
            &mut app,
            &both,
            vec![claim(1, DIRECTOR_TOKEN, &director, false)],
            idle_from + secs * SECOND,
        );
        assert!(!rang(&cmds), "hand-off, at +{secs}s: {cmds:?}");
    }
    assert!(
        !state.brigade_quiet.contains_key(&1),
        "nothing for the old brigade"
    );

    // Gathered from the new formation they match: a fresh watch, never armed by
    // the old brigade's work, so a long quiet rings nothing ...
    let reopened = idle_from + 11 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        reopened,
        reopened + 40 * SECOND,
    );
    assert_eq!(rang_at, None);
    // ... and its own work does.
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&newcomer, NotIdle)],
        reopened + 41 * SECOND,
    );
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        reopened + 42 * SECOND,
        reopened + 70 * SECOND,
    );
    assert_eq!(rang_at, Some(reopened + 42 * SECOND + BRIGADE_QUIET_SETTLE));
}

// --- the relay and the bell disagreeing about an observation ---------------------------------------

fn nudged(cmds: &[Cmd], pane: &SessionKey) -> bool {
    cmds.iter().any(|cmd| {
        matches!(cmd, Cmd::WritePty { key, bytes } if key == pane && bytes.as_slice() == RELAY_NUDGE_LINE.as_bytes())
    })
}

#[test]
fn a_tuple_the_bell_rejects_still_makes_the_relay_type_and_the_write_only_postpones() {
    let (mut state, mut app, director, worker, stranger, idle_from) = armed_with_a_stranger();
    let members = [(&director, Idle), (&worker, Idle), (&stranger, Idle)];

    // An observation names the Director's token (and so brigade 1) under the
    // Worker's pane, unread, with the relay's conditions met. The relay takes it
    // as given and types the nudge into the pane it names; the bell ignores it,
    // so the Director is not observed and nothing is judged.
    let tuple = || vec![claim(1, DIRECTOR_TOKEN, &worker, true)];
    let mut nudge_at = None;
    for secs in 1..=10 {
        let when = idle_from + secs * SECOND;
        let cmds = raw_tick(&mut state, &mut app, &members, tuple(), when);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
        if nudged(&cmds, &worker) {
            nudge_at.get_or_insert(when);
        }
    }
    let nudge_at = nudge_at.expect("the relay typed the nudge into the pane the tuple names");
    // The write stamped the pane it went to: a member of brigade 1.
    assert!(state.last_prompted.contains_key(&worker));
    assert!(!state.last_prompted.contains_key(&director));

    // Properly observed again, the brigade rings a settle after the LATER of the
    // tick it is observed again and the last write — never earlier than either.
    let back = idle_from + 11 * SECOND;
    let last_write = state.last_prompted[&worker];
    assert!(last_write >= nudge_at);
    let rang_at = tick_each_second_until(&mut state, &mut app, &members, back, back + 90 * SECOND);
    assert_eq!(rang_at, Some(back.max(last_write) + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_tuple_naming_a_pane_outside_the_brigade_leaves_the_brigades_stretch_alone() {
    let nowhere = key("hosted-by-nobody");
    for target_name in ["another brigade's pane", "an unhosted key"] {
        let (mut state, mut app, director, worker, stranger, idle_from) = armed_with_a_stranger();
        let members = [(&director, Idle), (&worker, Idle), (&stranger, Idle)];
        let target = if target_name.starts_with("another") {
            stranger.clone()
        } else {
            nowhere.clone()
        };
        let mut wrote = false;
        for secs in 1..=10 {
            let cmds = raw_tick(
                &mut state,
                &mut app,
                &members,
                vec![claim(1, DIRECTOR_TOKEN, &target, true)],
                idle_from + secs * SECOND,
            );
            assert!(!rang(&cmds), "{target_name}, at +{secs}s: {cmds:?}");
            wrote |= nudged(&cmds, &target);
        }
        assert!(wrote, "{target_name}: the relay wrote where the tuple says");
        // Nothing in brigade 1 was written to: its stretch is only broken by the
        // missing Director observation, so a settle after it is observed again it
        // rings, whatever the write did elsewhere.
        assert!(!state.last_prompted.contains_key(&director));
        assert!(!state.last_prompted.contains_key(&worker));
        let back = idle_from + 11 * SECOND;
        let rang_at =
            tick_each_second_until(&mut state, &mut app, &members, back, back + 30 * SECOND);
        assert_eq!(rang_at, Some(back + BRIGADE_QUIET_SETTLE), "{target_name}");
    }
}

#[test]
fn a_write_from_a_rejected_tuple_postpones_a_brigade_that_is_observed_properly() {
    let (mut state, mut app, director, worker, stranger, idle_from) = armed_with_a_stranger();
    let members = [(&director, Idle), (&worker, Idle), (&stranger, Idle)];
    // Every member of brigade 1 is observed as usual; in addition a stray tuple
    // (a token no member has) puts unread mail on the Worker's pane. The bell
    // ignores the stray; the relay types a nudge into the Worker's pane on the
    // second tick and its Enter on the third.
    let mut rang_at = None;
    let mut nudge_at = None;
    let mut when = idle_from + SECOND;
    while when <= idle_from + 2 * BRIGADE_QUIET_SETTLE + 5 * SECOND {
        let mut relay = observed(&state, Vec::new());
        relay.push(claim(1, "worker-3", &worker, true));
        let cmds = raw_tick(&mut state, &mut app, &members, relay, when);
        if nudged(&cmds, &worker) {
            nudge_at.get_or_insert(when);
        }
        if rang(&cmds) {
            rang_at = Some(when);
            break;
        }
        when += SECOND;
    }
    let nudge_at = nudge_at.expect("the relay typed into the pane the stray tuple names");
    // The brigade would have matured at idle_from + settle on its own; the write
    // (the nudge line, then its Enter a tick later) pushes it back to a settle
    // after the last of them, never earlier.
    let enter_at = nudge_at + SECOND;
    assert_eq!(rang_at, Some(enter_at + BRIGADE_QUIET_SETTLE));
    assert!(enter_at + BRIGADE_QUIET_SETTLE > idle_from + BRIGADE_QUIET_SETTLE);
}

// --- what a hosted Codex pane says about itself (OSC 7501) ----------------------------------------

const REPORT_WORKING: &[u8] = b"\x1b]7501;state=working:app=codex\x1b\\";
const REPORT_IDLE: &[u8] = b"\x1b]7501;state=idle:app=codex\x1b\\";
const REPORT_DONE: &[u8] = b"\x1b]7501;state=done:app=codex\x1b\\";
const REPORT_ERROR: &[u8] = b"\x1b]7501;state=error:app=codex\x1b\\";
const REPORT_BLOCKED: &[u8] = b"\x1b]7501;state=blocked:app=codex:kind=permission\x1b\\";
const REPORT_CLEAR: &[u8] = b"\x1b]7501;state=clear\x1b\\";
const QUERY_FROM_CLAUDE: &[u8] = b"\x1b]7501;?\x07";

fn codex_row(id: &str) -> SessionRow {
    SessionRow {
        agent: crate::model::AgentKind::Codex,
        ..row_for(id)
    }
}

fn said(
    state: &mut EmporiumState,
    app: &mut App,
    pane: &SessionKey,
    bytes: &[u8],
    at: Instant,
) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::PtyOutput {
            key: pane.clone(),
            chunk: bytes.to_vec(),
        },
        at,
    )
}

/// One hosted session, staged on its own with the list focused, whose row says
/// which product it is, and the first sample that lets the core place it.
fn solo_fixture(agent_row: SessionRow) -> (EmporiumState, App, SessionKey, Instant) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let pane = key(&agent_row.id);
    let mut app = App::new(vec![agent_row]);
    app.set_viewport_height(20);
    state.screens.insert(pane.clone(), Screen::new(24, 80));
    state.stage = Stage::Solo(pane.clone());
    state.focus = Focus::Sidebar;
    let at = t0();
    sample(&mut state, &mut app, &[(&pane, Idle)], at);
    (state, app, pane, at)
}

fn row_activity(app: &App, id: &str) -> crate::model::Activity {
    app.visible()
        .into_iter()
        .find_map(|line| match line {
            crate::app::ListLine::Row(row) if row.row.id == id => Some(row.row.activity),
            _ => None,
        })
        .expect("the row is in the list")
}

#[test]
fn a_codex_pane_that_has_reported_decides_its_own_quiescence_whatever_the_sample_says() {
    for (bytes, expected) in [
        (REPORT_IDLE, Idle),
        (REPORT_DONE, Idle),
        (REPORT_ERROR, Idle),
        (REPORT_WORKING, NotIdle),
        (REPORT_BLOCKED, NotIdle),
    ] {
        for sampled in [Idle, NotIdle, Unknown] {
            let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
            said(&mut state, &mut app, &pane, bytes, at + SECOND);
            sample(&mut state, &mut app, &[(&pane, sampled)], at + 2 * SECOND);
            assert_eq!(
                state.quiescence_of(&app, &pane),
                expected,
                "{} with a sample of {sampled:?}",
                String::from_utf8_lossy(bytes).escape_debug()
            );
        }
    }
}

#[test]
fn without_a_report_or_after_clear_the_shells_sample_decides_as_before() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    for sampled in [Idle, NotIdle, Unknown] {
        sample(&mut state, &mut app, &[(&pane, sampled)], at + SECOND);
        assert_eq!(state.quiescence_of(&app, &pane), sampled, "no report");
    }
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + 2 * SECOND);
    assert_eq!(state.quiescence_of(&app, &pane), NotIdle);
    said(&mut state, &mut app, &pane, REPORT_CLEAR, at + 3 * SECOND);
    sample(&mut state, &mut app, &[(&pane, Idle)], at + 3 * SECOND);
    assert_eq!(
        state.quiescence_of(&app, &pane),
        Idle,
        "cleared: the sample again"
    );
    sample(&mut state, &mut app, &[(&pane, NotIdle)], at + 4 * SECOND);
    assert_eq!(state.quiescence_of(&app, &pane), NotIdle);
}

#[test]
fn where_the_report_and_the_rollout_disagree_the_report_wins() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    // A turn killed with its process left a start with no end: the rollout reads
    // mid-turn for good, while Codex itself says idle. Nothing is running.
    said(&mut state, &mut app, &pane, REPORT_IDLE, at + SECOND);
    sample(&mut state, &mut app, &[(&pane, NotIdle)], at + 2 * SECOND);
    assert_eq!(state.quiescence_of(&app, &pane), Idle);
    // A turn ended with a command still running: the rollout says the turn is
    // over, Codex says it is working. Ending the pane would lose the command.
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + 3 * SECOND);
    sample(&mut state, &mut app, &[(&pane, Idle)], at + 4 * SECOND);
    assert_eq!(state.quiescence_of(&app, &pane), NotIdle);
    // A prompt waiting for the operator, which the rollout never shows.
    said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + 5 * SECOND);
    assert_eq!(state.quiescence_of(&app, &pane), NotIdle);
    // The report arrives the moment a turn ends; the rollout is read a second
    // later at worst.
    said(&mut state, &mut app, &pane, REPORT_DONE, at + 6 * SECOND);
    assert_eq!(
        state.quiescence_of(&app, &pane),
        Idle,
        "before the next sample"
    );
}

#[test]
fn a_report_from_a_pane_that_is_not_codex_is_ignored_until_it_is_known_to_be() {
    // A Claude pane: the query it sends is no report, and even a state report
    // would not be read for it.
    let (mut state, mut app, pane, at) = solo_fixture(row_for("cl"));
    said(&mut state, &mut app, &pane, QUERY_FROM_CLAUDE, at + SECOND);
    assert_eq!(state.screens[&pane].reported_state(), None);
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + 2 * SECOND);
    assert!(state.screens[&pane].reported_state().is_some(), "taken in");
    assert_eq!(
        state.quiescence_of(&app, &pane),
        Idle,
        "but not read for Claude"
    );
    assert!(!app.is_waiting_id("cl"));

    // A pane the core cannot place yet (a fresh session under a placeholder
    // key, no row): kept, not applied.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let fresh = key("new::/work::0");
    state.screens.insert(fresh.clone(), Screen::new(24, 80));
    sample(&mut state, &mut app, &[(&fresh, Idle)], at);
    said(&mut state, &mut app, &fresh, REPORT_WORKING, at + SECOND);
    assert_eq!(state.quiescence_of(&app, &fresh), Idle, "agent unknown");
    // Discovery gives it its real id, and a row says it is Codex: from the next
    // sample on, what it reported counts.
    let found = key("found-id");
    state.rekey_pane_state(&fresh, &found);
    app.replace_rows(vec![codex_row("found-id")]);
    sample(&mut state, &mut app, &[(&found, Idle)], at + 2 * SECOND);
    assert_eq!(state.quiescence_of(&app, &found), NotIdle);
}

#[test]
fn a_reports_life_follows_its_process_and_its_pane() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + SECOND);

    // Rekeyed (discovery, a session move): the report stays with the pane.
    let renamed = key("cx-renamed");
    state.rekey_pane_state(&pane, &renamed);
    app.replace_rows(vec![codex_row("cx-renamed")]);
    sample(&mut state, &mut app, &[(&renamed, Idle)], at + 2 * SECOND);
    assert_eq!(state.quiescence_of(&app, &renamed), NotIdle);
    assert!(state.screens[&renamed].reported_state().is_some());

    // A new process under the same key starts with nothing reported.
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: renamed.clone(),
            agent: None,
        },
        at + 3 * SECOND,
    );
    assert_eq!(state.screens[&renamed].reported_state(), None);
    sample(&mut state, &mut app, &[(&renamed, Idle)], at + 4 * SECOND);
    assert_eq!(state.quiescence_of(&app, &renamed), Idle);

    // Gone with the pane.
    said(
        &mut state,
        &mut app,
        &renamed,
        REPORT_WORKING,
        at + 5 * SECOND,
    );
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: renamed.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + 6 * SECOND,
    );
    assert!(!state.screens.contains_key(&renamed));
    assert_eq!(state.quiescence_of(&app, &renamed), Unknown);
}

#[test]
fn a_report_survives_a_recording_round_trip() {
    let event = Event::PtyOutput {
        key: key("cx"),
        chunk: REPORT_BLOCKED.to_vec(),
    };
    let text = serde_json::to_string(&event).unwrap();
    let replayed: Event = serde_json::from_str(&text).unwrap();
    assert_eq!(replayed, event);
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    send(&mut state, &mut app, replayed, at + SECOND);
    assert!(
        state.screens[&pane]
            .reported_state()
            .is_some_and(|s| s.is_blocked())
    );
}

// --- the brigade bell and a Codex member's report -------------------------------------------------

/// Brigade 1: a Claude Director and a Codex Worker whose pane is `cx`, both
/// rows known, armed by earlier work and idle at the returned instant.
fn brigade_with_a_codex_worker() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let (mut state, mut app, director, codex, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir"), codex_row("w1")]);
    let _ = (&director, &codex);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&codex, &director], at);
    (state, app, director, codex, idle_from)
}

#[test]
fn work_left_running_after_a_codex_turn_holds_the_brigade_until_it_reports_idle() {
    let (mut state, mut app, director, codex, idle_from) = brigade_with_a_codex_worker();
    let both = [(&director, Idle), (&codex, Idle)];
    // The turn is over for the rollout (the sample is idle) but Codex says it is
    // still working: a command it started is running.
    said(&mut state, &mut app, &codex, REPORT_WORKING, idle_from);
    for secs in 0..30 {
        let cmds = sample(&mut state, &mut app, &both, idle_from + secs * SECOND);
        assert!(!rang(&cmds), "working at +{secs}s: {cmds:?}");
    }
    // The command ends and Codex goes idle: a settle later the brigade rings.
    let reported_idle = idle_from + 30 * SECOND;
    said(&mut state, &mut app, &codex, REPORT_IDLE, reported_idle);
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        reported_idle,
        reported_idle + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(rang_at, Some(reported_idle + BRIGADE_QUIET_SETTLE));
}

#[test]
fn a_codex_workers_start_up_report_working_then_idle_does_not_arm_the_brigade() {
    // The start of a hosted Codex pane: working, then idle (what that
    // `working` is: docs/REQUIREMENTS.md, "The pane's own report"). It is a
    // not-idle reading before any idle one, so the start-up rule already keeps
    // it from arming the bell.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row_for("dir"), codex_row("w1")]);
    let director = host_fresh(&mut state, 1, DIRECTOR_TOKEN, "dir");
    let codex = host_fresh(&mut state, 1, "worker-1", "w1");
    stage(&mut state, 1, &director, &[&codex]);
    state.focus = Focus::Sidebar;
    let at = t0();
    tick(&mut state, &mut app, at);
    let both = [(&director, Idle), (&codex, Idle)];
    said(&mut state, &mut app, &codex, REPORT_WORKING, at);
    for secs in 1..=3 {
        let cmds = sample(&mut state, &mut app, &both, at + secs * SECOND);
        assert!(!rang(&cmds), "starting up, at +{secs}s: {cmds:?}");
    }
    said(&mut state, &mut app, &codex, REPORT_IDLE, at + 4 * SECOND);
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        at + 4 * SECOND,
        at + 90 * SECOND,
    );
    assert_eq!(rang_at, None, "nobody did any work");
}

#[test]
fn the_relay_keeps_the_rollout_for_a_codex_member_whatever_it_reports() {
    // A Codex member whose turn has ended is nudgeable even while it reports
    // working: the relay reads the observation the shell built from the rollout,
    // never the report.
    let (mut state, mut app, director, codex, idle_from) = brigade_with_a_codex_worker();
    // At its empty prompt (the relay types only there), yet reporting `working`.
    state.size = (160, 50);
    resize_staged_tiles(&mut state);
    let rows = state.screens.get(&codex).unwrap().size().0;
    let prompt = format!(
        "\x1b[2J\x1b[{row};1H\x1b[1m\u{203a}\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m\x1b[{row};3H",
        row = rows - 3
    );
    said(&mut state, &mut app, &codex, prompt.as_bytes(), idle_from);
    said(&mut state, &mut app, &codex, REPORT_WORKING, idle_from);
    let both = [(&director, Idle), (&codex, Idle)];
    let mut nudged_codex = false;
    for secs in 0..4 {
        let mut mail = mail_for(1, "worker-1", &codex);
        mail.agent = Some(crate::model::AgentKind::Codex);
        let cmds = sample_with_relay(
            &mut state,
            &mut app,
            &both,
            vec![mail],
            idle_from + secs * SECOND,
        );
        nudged_codex |= typed_into(&cmds, &codex);
    }
    assert!(
        nudged_codex,
        "the relay nudged a member whose rollout says idle"
    );
}

// --- a Codex pane blocked on the operator shows as waiting ------------------------------------------

fn waiting_marks(state: &EmporiumState, app: &App, pane: &SessionKey) -> (bool, bool, usize) {
    (
        app.is_waiting_id(pane.as_str()),
        state.attention_panes().contains(pane),
        state.visible_waiting_count(),
    )
}

#[test]
fn a_blocked_codex_pane_is_marked_counted_and_rung_for_once_and_clears_with_the_state() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    assert_eq!(waiting_marks(&state, &app, &pane), (false, false, 0));

    let cmds = said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + SECOND);
    assert_eq!(waiting_marks(&state, &app, &pane), (true, true, 1));
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Waiting);
    assert_eq!(
        cmds.iter()
            .filter(|c| matches!(c, Cmd::RingHostBell { .. }))
            .count(),
        1
    );
    assert_eq!(
        bell_causes(&cmds),
        vec![BellCause::StagedPaneWait {
            pane: pane.clone(),
            others: 0
        }],
        "a pane on its own tile that reports blocked"
    );

    // A level: it holds while the state does, through a reload of the list ...
    app.replace_rows(vec![codex_row("cx")]);
    sample(&mut state, &mut app, &[(&pane, Idle)], at + 20 * SECOND);
    assert_eq!(waiting_marks(&state, &app, &pane), (true, true, 1));
    // ... and goes when it changes (working is shown as busy).
    let cmds = said(
        &mut state,
        &mut app,
        &pane,
        REPORT_WORKING,
        at + 30 * SECOND,
    );
    assert_eq!(waiting_marks(&state, &app, &pane), (false, false, 0));
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    assert!(!rang(&cmds));
    said(&mut state, &mut app, &pane, REPORT_DONE, at + 40 * SECOND);
    assert_eq!(
        row_activity(&app, "cx"),
        crate::model::Activity::Alive,
        "as today"
    );

    // Blocked again: a new mark, a new ring.
    let cmds = said(
        &mut state,
        &mut app,
        &pane,
        REPORT_BLOCKED,
        at + 50 * SECOND,
    );
    assert_eq!(waiting_marks(&state, &app, &pane), (true, true, 1));
    assert!(rang(&cmds));
}

#[test]
fn a_blocked_pane_is_treated_exactly_as_a_claude_session_that_is_waiting() {
    // Claude: the row itself says waiting (the live file, read by the shell).
    // Codex: the pane says blocked. Whatever that mechanism does with the pane
    // being focused or not, both do the same, in the marks and in the bell.
    for focused in [false, true] {
        let (mut state, mut app, pane, at) = solo_fixture(row_for("cl"));
        if focused {
            state.focus = Focus::Pane;
        }
        let mut waiting = row_for("cl");
        waiting.activity = crate::model::Activity::Waiting;
        app.replace_rows(vec![waiting]);
        let cmds = tick(&mut state, &mut app, at + SECOND);
        let claude = (waiting_marks(&state, &app, &pane), rang(&cmds));

        let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
        if focused {
            state.focus = Focus::Pane;
        }
        let cmds = said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + SECOND);
        let codex = (waiting_marks(&state, &app, &pane), rang(&cmds));
        assert_eq!(codex, claude, "pane focused: {focused}");
    }
}

#[test]
fn a_bel_and_a_blocked_report_for_the_same_pane_ring_once() {
    for blocked_first in [true, false] {
        let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
        let mut rings = 0;
        let mut step = |state: &mut EmporiumState, app: &mut App, bytes: &[u8], secs: u32| {
            let cmds = said(state, app, &pane, bytes, at + secs * SECOND);
            rings += cmds
                .iter()
                .filter(|c| matches!(c, Cmd::RingHostBell { .. }))
                .count();
        };
        // Codex rings BEL for the same approval it reports; 10 s apart, beyond
        // the bell cooldown, so the cooldown is not what keeps the second quiet.
        if blocked_first {
            step(&mut state, &mut app, REPORT_BLOCKED, 1);
            step(&mut state, &mut app, b"\x07", 11);
        } else {
            step(&mut state, &mut app, b"\x07", 1);
            step(&mut state, &mut app, REPORT_BLOCKED, 11);
        }
        assert_eq!(rings, 1, "blocked first: {blocked_first}");
        assert!(state.attention_panes().contains(&pane));
    }
}

#[test]
fn what_nobody_reported_and_what_is_not_hosted_is_unchanged() {
    // A hosted Codex pane that reported nothing, another hosted pane, and a
    // Codex session that is not hosted at all.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![
        codex_row("cx"),
        codex_row("quiet"),
        codex_row("not-hosted"),
    ]);
    app.set_viewport_height(20);
    let cx = key("cx");
    let quiet = key("quiet");
    state.screens.insert(cx.clone(), Screen::new(24, 80));
    state.screens.insert(quiet.clone(), Screen::new(24, 80));
    state.stage = Stage::Solo(cx.clone());
    let at = t0();
    sample(&mut state, &mut app, &[(&cx, Idle), (&quiet, Idle)], at);
    said(&mut state, &mut app, &cx, REPORT_BLOCKED, at + SECOND);
    assert!(app.is_waiting_id("cx"));
    assert!(!app.is_waiting_id("quiet"));
    assert!(!app.is_waiting_id("not-hosted"));
    assert_eq!(row_activity(&app, "quiet"), crate::model::Activity::Alive);
    assert_eq!(
        row_activity(&app, "not-hosted"),
        crate::model::Activity::Alive
    );
    assert_eq!(state.visible_waiting_count(), 1);
}

// --- a report counts for a pane that is Codex now ---------------------------------------------------

fn rows_loaded(rows: Vec<SessionRow>) -> Event {
    Event::RowsLoaded {
        rows,
        hidden: HashSet::new(),
        directors: HashSet::new(),
    }
}

/// What the relay observes of a member in a tick, with the product it resolved.
fn observed_as(
    state: &EmporiumState,
    pane: &SessionKey,
    agent: Option<crate::model::AgentKind>,
) -> RelayObservation {
    RelayObservation {
        live: None,
        glance: None,
        brigade_id: state.member_brigades[pane],
        token: state.member_tokens[pane].clone(),
        key: pane.clone(),
        has_unseen: false,
        is_idle_this_tick: Some(true),
        agent,
        briefed_since_launch: None,
    }
}

/// Whether the list-focused `q` ends banto at once (nothing is known to be
/// running) or asks for a second key.
fn quits_at_once(state: &mut EmporiumState, app: &mut App, at: Instant) -> bool {
    press(state, app, KeyCode::Char('q'), at);
    app.should_quit()
}

#[test]
fn a_reload_that_drops_a_codex_panes_row_ends_what_its_report_decides_in_that_update() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + SECOND);
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);

    // The row is gone, nothing else says what the pane is, and the operator
    // presses quit before the next sample: the sample (idle) decides, as it did
    // before the pane ever reported.
    send(
        &mut state,
        &mut app,
        rows_loaded(Vec::new()),
        at + 2 * SECOND,
    );
    assert!(quits_at_once(&mut state, &mut app, at + 2 * SECOND));
    // And nothing stays laid over the list: the row, when it is back as a
    // product that is not Codex, shows its own activity.
    app.replace_rows(vec![row_for("cx")]);
    assert_ne!(row_activity(&app, "cx"), crate::model::Activity::Busy);
}

#[test]
fn a_reload_that_lists_the_pane_as_another_product_does_the_same() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + SECOND);
    assert!(app.is_waiting_id("cx"));

    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("cx")]),
        at + 2 * SECOND,
    );
    assert_ne!(row_activity(&app, "cx"), crate::model::Activity::Waiting);
    assert!(!app.is_waiting_id("cx"));
    assert!(quits_at_once(&mut state, &mut app, at + 2 * SECOND));
}

#[test]
fn the_row_coming_back_as_codex_makes_the_kept_report_count_again() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + SECOND);
    send(
        &mut state,
        &mut app,
        rows_loaded(Vec::new()),
        at + 2 * SECOND,
    );
    assert!(state.screens[&pane].reported_state().is_some());

    // It is still that process's last report: not forgotten while the row was
    // away, believed again as soon as the pane is known to be Codex.
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![codex_row("cx")]),
        at + 3 * SECOND,
    );
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    assert!(!quits_at_once(&mut state, &mut app, at + 3 * SECOND));
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: cx (busy)")
    );
}

#[test]
fn a_member_the_relay_observes_as_codex_is_read_without_a_row_and_one_nobody_knows_is_not() {
    // No row for the Worker in the list (a reload dropped it, or it was never
    // there), but the relay resolved it as Codex this very tick: believed, even
    // though no sample has come since.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let relay = vec![observed_as(
        &state,
        &worker,
        Some(crate::model::AgentKind::Codex),
    )];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        relay,
        at + SECOND,
    );
    said(
        &mut state,
        &mut app,
        &worker,
        REPORT_WORKING,
        at + 2 * SECOND,
    );
    assert!(app.row_for_id("w1").is_none());
    assert!(!quits_at_once(&mut state, &mut app, at + 2 * SECOND));

    // The same pane with nothing saying what it is: the sample decides.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at + SECOND,
    );
    said(
        &mut state,
        &mut app,
        &worker,
        REPORT_WORKING,
        at + 2 * SECOND,
    );
    assert!(quits_at_once(&mut state, &mut app, at + 2 * SECOND));
}

#[test]
fn where_the_row_and_the_relay_disagree_about_the_product_the_report_is_not_believed() {
    use crate::model::AgentKind::{ClaudeCode as Claude, Codex};
    for (row_agent, relay_agent) in [(Claude, Codex), (Codex, Claude)] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        app.replace_rows(vec![SessionRow {
            agent: row_agent,
            ..row_for("w1")
        }]);
        app.set_viewport_height(20);
        let relay = vec![observed_as(&state, &worker, Some(relay_agent))];
        sample_with_relay(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Idle)],
            relay,
            at + SECOND,
        );
        said(
            &mut state,
            &mut app,
            &worker,
            REPORT_WORKING,
            at + 2 * SECOND,
        );
        assert!(
            quits_at_once(&mut state, &mut app, at + 2 * SECOND),
            "row {row_agent:?}, relay {relay_agent:?}: the sample decides"
        );
        assert_ne!(row_activity(&app, "w1"), crate::model::Activity::Busy);
    }
    // Where they agree on Codex, or only one says anything, it is believed.
    for rows_say in [true, false] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        if rows_say {
            app.replace_rows(vec![codex_row("w1")]);
        }
        let relay = vec![observed_as(&state, &worker, Some(Codex))];
        sample_with_relay(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Idle)],
            relay,
            at + SECOND,
        );
        said(
            &mut state,
            &mut app,
            &worker,
            REPORT_WORKING,
            at + 2 * SECOND,
        );
        assert!(
            !quits_at_once(&mut state, &mut app, at + 2 * SECOND),
            "row says Codex: {rows_say}"
        );
    }
}

#[test]
fn a_blocked_codex_panes_mark_and_ring_do_not_depend_on_the_bell_switch_or_on_focus() {
    // F4 ("BELL ALWAYS") is only about the brigade-quiet bell of a focused
    // Director. A pane's own blocked mark is not that, and neither is it
    // cleared by the pane being the focused one: the same as a Claude session
    // whose live file says waiting.
    let mut outcomes = Vec::new();
    for bell_when_focused in [false, true] {
        for focused in [false, true] {
            let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
            state.bell_when_focused = bell_when_focused;
            if focused {
                state.focus = Focus::Pane;
            }
            let cmds = said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + SECOND);
            let outcome = (waiting_marks(&state, &app, &pane), rang(&cmds));
            assert_eq!(
                outcome,
                ((true, true, 1), true),
                "switch {bell_when_focused}, pane focused {focused}"
            );
            outcomes.push(outcome);
        }
    }
    assert!(outcomes.windows(2).all(|pair| pair[0] == pair[1]));
}

// --- a report that may have been cut short, and what the output path recomputes -------------------

#[test]
fn a_cut_report_ends_what_the_pane_reported_before_and_the_sample_decides_again() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + SECOND);
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    // A report the parser's parameter limit cut before its state: it may have
    // been the one that said the work is over, so the old `working` is not kept.
    let cut = format!("\x1b]7501;x{}:state=idle\x07", ";".repeat(20));
    said(&mut state, &mut app, &pane, cut.as_bytes(), at + 2 * SECOND);
    assert_eq!(state.screens[&pane].reported_state(), None);
    assert_ne!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    assert!(quits_at_once(&mut state, &mut app, at + 2 * SECOND));
}

#[test]
fn an_output_chunk_with_no_new_report_does_not_recompute_the_overlay_and_one_with_a_report_does() {
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    // A terminal size, so the pane has room for the text below.
    send(
        &mut state,
        &mut app,
        Event::Resized {
            width: 120,
            height: 40,
        },
        at,
    );
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + SECOND);
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);

    // The rows change behind the core's back (a real reload arrives as
    // `RowsLoaded`, which recomputes): the pane is a Claude one now. Ordinary
    // output does not look at the rows, so the overlay is still what it was.
    // This is how the test sees that the output path did no work; the next
    // event that is not output puts it right.
    app.replace_rows(vec![row_for("cx")]);
    said(
        &mut state,
        &mut app,
        &pane,
        b"plain output\r\n",
        at + 2 * SECOND,
    );
    said(&mut state, &mut app, &pane, REPORT_WORKING, at + 2 * SECOND);
    assert_eq!(
        row_activity(&app, "cx"),
        crate::model::Activity::Busy,
        "no new report: nothing recomputed"
    );
    tick(&mut state, &mut app, at + 3 * SECOND);
    assert_ne!(row_activity(&app, "cx"), crate::model::Activity::Busy);

    // A chunk that changes the pane's report is acted on in that same update,
    // in both directions.
    app.replace_rows(vec![codex_row("cx")]);
    tick(&mut state, &mut app, at + 4 * SECOND);
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    said(&mut state, &mut app, &pane, REPORT_IDLE, at + 5 * SECOND);
    assert_ne!(row_activity(&app, "cx"), crate::model::Activity::Busy);
    said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + 6 * SECOND);
    assert_eq!(row_activity(&app, "cx"), crate::model::Activity::Waiting);
    said(&mut state, &mut app, &pane, REPORT_CLEAR, at + 7 * SECOND);
    assert!(!app.is_waiting_id("cx"));
}

// --- every surface that names a pane's activity agrees with the report -----------------------------

/// What the surfaces of the core read for `id`: the list row (None when the
/// list does not show it), the summary panel's row, and the row the collapsed
/// tile's strip is drawn from.
fn surfaces_of(
    app: &mut App,
    id: &str,
) -> (
    Option<crate::model::Activity>,
    Option<crate::model::Activity>,
    Option<crate::model::Activity>,
) {
    let in_list = app.visible().into_iter().find_map(|line| match line {
        crate::app::ListLine::Row(row) if row.row.id == id => Some(row.row.activity),
        _ => None,
    });
    let in_summary = app
        .selected_row()
        .filter(|row| row.id == id)
        .map(|row| row.activity);
    let for_strip = app.row_for_id(id).map(|row| row.activity);
    (in_list, in_summary, for_strip)
}

#[test]
fn a_codex_panes_reported_activity_reaches_the_list_the_summary_and_the_strip() {
    use crate::model::Activity::{Alive, Busy, Waiting};
    // A Codex Worker of the staged brigade (a member: never in the list, at any
    // reveal level) and a Codex session hosted on its own (in the list).
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let solo = host_solo(&mut state, "cx");
    app.replace_rows(vec![row_for("dir"), codex_row("w1"), codex_row("cx")]);
    app.set_hidden_member_ids(["w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    let all = [(&director, Idle), (&worker, Idle), (&solo, Idle)];
    sample(&mut state, &mut app, &all, at + SECOND);

    // Selected, so that the summary panel shows it.
    while app.selected_row().map(|row| row.id.as_str()) != Some("cx") {
        app.select_next();
    }
    // The rollout says idle (the samples above); Codex says what it is doing.
    for (bytes, expected) in [
        (REPORT_WORKING, Busy),
        (REPORT_BLOCKED, Waiting),
        (REPORT_CLEAR, Alive),
    ] {
        said(&mut state, &mut app, &worker, bytes, at + 2 * SECOND);
        said(&mut state, &mut app, &solo, bytes, at + 2 * SECOND);
        let what = String::from_utf8_lossy(bytes).escape_debug().to_string();
        assert_eq!(
            surfaces_of(&mut app, "cx"),
            (Some(expected), Some(expected), Some(expected)),
            "listed pane after {what}"
        );
        // The member is not in the list, but the strip reads its row.
        assert_eq!(
            surfaces_of(&mut app, "w1"),
            (None, None, Some(expected)),
            "member after {what}"
        );
    }
    // Not at any reveal level either: a member is never listed.
    for _ in 0..3 {
        app.cycle_reveal();
        assert_eq!(surfaces_of(&mut app, "w1").0, None);
    }
}

// --- a dialog the operator opens in a member's pane --------------------------------------------

#[test]
fn a_member_that_reads_not_idle_for_a_moment_arms_an_idle_brigade_that_has_seen_it_idle() {
    // What a `waiting` session was sampled as before a dialog was told apart from
    // a question: not idle. The operator opens a picker in the Worker's pane and
    // closes it; nothing ran, yet the brigade counts it as work that ended.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    // Quiet from the start: every member has been seen idle, nothing armed.
    let rang_at = tick_each_second_until(&mut state, &mut app, &all_idle, at, at + 30 * SECOND);
    assert_eq!(
        rang_at, None,
        "a brigade that did nothing is not quiet news"
    );

    // The dialog: three seconds of not idle, then idle again.
    let opened = at + 31 * SECOND;
    for secs in 0..3 {
        sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, NotIdle)],
            opened + secs * SECOND,
        );
    }
    let closed = opened + 3 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        closed,
        closed + 2 * BRIGADE_QUIET_SETTLE,
    );
    assert_eq!(
        rang_at,
        Some(closed + BRIGADE_QUIET_SETTLE),
        "the false bell: a settle after the dialog closes"
    );
}

/// A row as the shell reports a Claude session: `waiting` for a person, and
/// whether the live file says the reason is a dialog.
fn waiting_row(id: &str, dialog_open: bool) -> SessionRow {
    SessionRow {
        activity: crate::model::Activity::Waiting,
        dialog_open,
        ..row_for(id)
    }
}

/// A reload of the list with exactly `rows`, as the shell sends it.
fn reload(state: &mut EmporiumState, app: &mut App, rows: Vec<SessionRow>, at: Instant) -> usize {
    rings(&send(state, app, rows_loaded(rows), at))
}

/// Two ways a session reaches the operator: a session that is in the list
/// (`cx`, hosted alone and staged), and a brigade Worker (`w1`) that is only a
/// tile. Each is staged, so a mark can be drawn for it.
fn dialog_fixture() -> (EmporiumState, App, Instant) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let solo = host_solo(&mut state, "cx");
    app.replace_rows(vec![row_for("dir"), row_for("w1"), row_for("cx")]);
    app.set_hidden_member_ids(["dir".to_string(), "w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    stage(&mut state, 1, &director, &[&worker, &solo]);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle), (&solo, Idle)],
        at + SECOND,
    );
    (state, app, at + 10 * SECOND)
}

fn plain_rows() -> Vec<SessionRow> {
    vec![row_for("dir"), row_for("w1"), row_for("cx")]
}

/// The rows with `id` waiting, for the reason a dialog is (or is not).
fn rows_waiting(id: &str, dialog_open: bool) -> Vec<SessionRow> {
    plain_rows()
        .into_iter()
        .map(|row| {
            if row.id == id {
                waiting_row(id, dialog_open)
            } else {
                row
            }
        })
        .collect()
}

#[test]
fn a_dialog_open_session_is_shown_and_counted_as_waiting_like_any_other() {
    for id in ["cx", "w1"] {
        let (mut state, mut app, at) = dialog_fixture();
        reload(&mut state, &mut app, rows_waiting(id, true), at);
        let pane = key(id);
        assert!(app.is_waiting_id(id), "{id}: the row is Waiting");
        assert!(
            state.attention_panes().contains(&pane),
            "{id}: its tile is marked"
        );
        assert_eq!(state.visible_waiting_count(), 1, "{id}: counted");
        let activity = app.row_for_id(id).map(|row| row.activity);
        assert_eq!(activity, Some(crate::model::Activity::Waiting), "{id}");
    }
}

#[test]
fn a_dialog_open_session_does_not_ring_when_it_appears_nor_when_it_closes_and_reopens() {
    for id in ["cx", "w1"] {
        let (mut state, mut app, at) = dialog_fixture();
        let mut rung = 0;
        // Appears, closes, reopens, closes.
        for (step, dialog) in [Some(true), None, Some(true), None, Some(true)]
            .into_iter()
            .enumerate()
        {
            let rows = match dialog {
                Some(dialog) => rows_waiting(id, dialog),
                None => plain_rows(),
            };
            rung += reload(&mut state, &mut app, rows, at + (step as u32) * 10 * SECOND);
            let marked = state.attention_panes().contains(&key(id));
            assert_eq!(marked, dialog.is_some(), "{id}: step {step}");
        }
        assert_eq!(rung, 0, "{id}: a dialog the operator opened never rings");
    }
}

#[test]
fn a_dialog_that_turns_into_a_question_while_waiting_rings_once_at_that_moment() {
    for id in ["cx", "w1"] {
        let (mut state, mut app, at) = dialog_fixture();
        assert_eq!(reload(&mut state, &mut app, rows_waiting(id, true), at), 0);
        // Still `waiting`, the reason is something else now.
        let later = at + 10 * SECOND;
        assert_eq!(
            reload(&mut state, &mut app, rows_waiting(id, false), later),
            1,
            "{id}: rings when the reason changes"
        );
        // And only once.
        assert_eq!(
            reload(
                &mut state,
                &mut app,
                rows_waiting(id, false),
                later + 10 * SECOND
            ),
            0,
            "{id}"
        );
        assert!(state.attention_panes().contains(&key(id)));
        // Back to a dialog: it stays marked and does not ring again.
        assert_eq!(
            reload(
                &mut state,
                &mut app,
                rows_waiting(id, true),
                later + 20 * SECOND
            ),
            0,
            "{id}"
        );
        assert!(state.attention_panes().contains(&key(id)));
    }
}

#[test]
fn a_waiting_session_whose_reason_is_not_a_dialog_rings_as_before() {
    // A question, a permission prompt, a reason nobody has seen, or none: the row
    // does not say "dialog", and each rings once when it appears.
    for id in ["cx", "w1"] {
        let (mut state, mut app, at) = dialog_fixture();
        assert_eq!(
            reload(&mut state, &mut app, rows_waiting(id, false), at),
            1,
            "{id}"
        );
        assert!(state.attention_panes().contains(&key(id)));
        assert_eq!(state.visible_waiting_count(), 1);
    }
}

#[test]
fn a_dialog_open_session_still_asks_at_quit_and_is_not_idle_for_the_relay() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // The shell samples a dialog as Unknown: not known idle, so quitting asks.
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Unknown)],
        at + SECOND,
    );
    press(&mut state, &mut app, KeyCode::Char('q'), at + 2 * SECOND);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: worker-1 (unknown)")
    );
}

#[test]
fn a_dialog_alone_never_arms_a_quiet_brigade() {
    // The operator opens a picker in a member's pane and closes it: the shell
    // samples Unknown while it is open. The brigade, which had never done any
    // work, rings nothing then or later.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    assert_eq!(
        tick_each_second_until(&mut state, &mut app, &all_idle, at, at + 30 * SECOND),
        None
    );
    let opened = at + 31 * SECOND;
    for secs in 0..3 {
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Unknown)],
            opened + secs * SECOND,
        );
        assert!(!rang(&cmds));
    }
    let closed = opened + 3 * SECOND;
    assert_eq!(
        tick_each_second_until(
            &mut state,
            &mut app,
            &all_idle,
            closed,
            closed + 3 * BRIGADE_QUIET_SETTLE
        ),
        None,
        "no bell after the dialog closes"
    );
}

#[test]
fn a_dialog_during_a_real_quiet_stretch_holds_it_and_the_bell_comes_after_it_closes() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    // Real work, armed by it, then idle for a moment.
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let all_idle = [(&director, Idle), (&worker, Idle)];
    let dialog_open = [(&director, Idle), (&worker, Unknown)];
    // Two seconds in, a dialog opens and stays open far past the settle.
    sample(&mut state, &mut app, &all_idle, idle_from + SECOND);
    let mut rang_during = false;
    for secs in 2..=40 {
        rang_during |= rang(&sample(
            &mut state,
            &mut app,
            &dialog_open,
            idle_from + secs * SECOND,
        ));
    }
    assert!(!rang_during, "an open dialog holds the quiet stretch");
    // It closes; the stretch starts over and rings a settle later.
    let closed = idle_from + 41 * SECOND;
    assert_eq!(
        tick_each_second_until(
            &mut state,
            &mut app,
            &all_idle,
            closed,
            closed + 3 * BRIGADE_QUIET_SETTLE
        ),
        Some(closed + BRIGADE_QUIET_SETTLE)
    );
}

// --- a staged Codex pane the session list has no row for -------------------------------------------

/// Brigade 1 staged with a Claude Director (row in the list) and a Worker whose
/// pane the relay has resolved as Codex but that has no row yet: a member the
/// session list has not picked up, or a reload window.
fn rowless_codex_worker() -> (EmporiumState, App, SessionKey, Instant) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir")]);
    app.set_viewport_height(20);
    let relay = vec![observed_as(
        &state,
        &worker,
        Some(crate::model::AgentKind::Codex),
    )];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        relay,
        at + SECOND,
    );
    assert!(app.row_for_id("w1").is_none());
    (state, app, worker, at + 10 * SECOND)
}

#[test]
fn a_staged_codex_pane_with_no_row_that_reports_blocked_is_marked_counted_and_rung_for_once() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    let cmds = said(&mut state, &mut app, &worker, REPORT_BLOCKED, at);
    assert!(state.attention_panes().contains(&worker), "marked");
    assert_eq!(state.visible_waiting_count(), 1, "counted");
    assert_eq!(rings(&cmds), 1, "rung for");
    assert_eq!(
        rings(&tick(&mut state, &mut app, at + 10 * SECOND)),
        0,
        "once"
    );

    // The row turning up changes nothing visible and does not ring again.
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), codex_row("w1")]),
        at + 20 * SECOND,
    );
    assert_eq!(rings(&cmds), 0);
    assert!(state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 1);
    assert_eq!(rings(&tick(&mut state, &mut app, at + 30 * SECOND)), 0);

    // And it clears with the report, row or not.
    said(&mut state, &mut app, &worker, REPORT_IDLE, at + 40 * SECOND);
    assert!(!state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 0);
}

#[test]
fn a_staged_codex_pane_with_no_row_that_reports_working_is_not_idle_and_not_waiting() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    let cmds = said(&mut state, &mut app, &worker, REPORT_WORKING, at);
    assert_eq!(rings(&cmds), 0);
    assert!(!state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 0);
    assert_eq!(state.quiescence_of(&app, &worker), NotIdle);
}

#[test]
fn a_staged_pane_with_neither_a_row_nor_a_known_product_is_not_read_as_codex() {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir")]);
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        at,
    );
    let cmds = said(&mut state, &mut app, &worker, REPORT_BLOCKED, at + SECOND);
    assert!(!state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 0);
    assert_eq!(rings(&cmds), 0);
    assert_eq!(state.quiescence_of(&app, &worker), Idle);
}

// --- a row that disagrees about the product, and the report it leaves unbelieved -------------------
//
// The rule these pin: a pane's report is believed only while the pane is known to
// be Codex, and where the session list's row and the relay's facts name different
// products it is not known to be either (`EmporiumState::pane_agent`). So a row
// that agrees (Codex), or no row, leaves everything as it was; a row naming
// another product makes the report stop deciding on every surface at once.

/// (marked, waiting count, `quiescence_of`) for `pane`.
fn surfaces(state: &EmporiumState, app: &App, pane: &SessionKey) -> (bool, usize, Quiescence) {
    (
        state.attention_panes().contains(pane),
        state.visible_waiting_count(),
        state.quiescence_of(app, pane),
    )
}

fn claude_row(id: &str) -> SessionRow {
    row_for(id)
}

#[test]
fn a_row_naming_another_product_ends_what_a_row_less_codex_pane_reported_on_every_surface_at_once()
{
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    let cmds = said(&mut state, &mut app, &worker, REPORT_BLOCKED, at);
    assert_eq!(rings(&cmds), 1);
    assert_eq!(surfaces(&state, &app, &worker), (true, 1, NotIdle));

    // A row for the same id arrives, and says Claude: row and facts disagree.
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), claude_row("w1")]),
        at + 10 * SECOND,
    );
    assert_eq!(
        surfaces(&state, &app, &worker),
        (false, 0, Idle),
        "mark, count and quit all let go in the update that brought the row"
    );
    assert_eq!(rings(&cmds), 0, "the change rings nothing");
    // What the pane shows is now the row's own, and the report is still held.
    assert_eq!(
        app.row_for_id("w1").map(|row| row.activity),
        Some(crate::model::Activity::Alive)
    );
    assert!(state.screens[&worker].reported_state().is_some());
    assert_eq!(state.reported_activity(&app, &worker), None);
    assert_eq!(rings(&tick(&mut state, &mut app, at + 20 * SECOND)), 0);
}

#[test]
fn the_conflicting_row_going_away_gives_the_kept_report_back_and_it_rings_again() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    assert_eq!(
        rings(&said(&mut state, &mut app, &worker, REPORT_BLOCKED, at)),
        1
    );
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), claude_row("w1")]),
        at + 10 * SECOND,
    );
    assert_eq!(surfaces(&state, &app, &worker), (false, 0, Idle));

    // The row is gone again: the pane is Codex by the relay's facts alone, and
    // what it reported counts again. It is a new gain of the mark on a surface the
    // operator has seen nothing on in between, so it rings once.
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir")]),
        at + 20 * SECOND,
    );
    assert_eq!(surfaces(&state, &app, &worker), (true, 1, NotIdle));
    assert_eq!(rings(&cmds), 1, "re-gained, rung once");
    assert_eq!(
        rings(&tick(&mut state, &mut app, at + 30 * SECOND)),
        0,
        "and no more"
    );
}

#[test]
fn the_conflicting_row_coming_back_as_codex_gives_the_kept_report_back_and_it_rings_again() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    assert_eq!(
        rings(&said(&mut state, &mut app, &worker, REPORT_BLOCKED, at)),
        1
    );
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), claude_row("w1")]),
        at + 10 * SECOND,
    );
    assert_eq!(surfaces(&state, &app, &worker), (false, 0, Idle));

    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), codex_row("w1")]),
        at + 20 * SECOND,
    );
    assert_eq!(surfaces(&state, &app, &worker), (true, 1, NotIdle));
    assert_eq!(rings(&cmds), 1, "re-gained, rung once");
    assert_eq!(rings(&tick(&mut state, &mut app, at + 30 * SECOND)), 0);
    assert_eq!(rings(&tick(&mut state, &mut app, at + 40 * SECOND)), 0);
}

#[test]
fn a_row_removed_while_the_codex_pane_is_blocked_changes_nothing_visible_and_rings_nothing() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    // It has a Codex row first (agreeing with the facts).
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), codex_row("w1")]),
        at,
    );
    let cmds = said(&mut state, &mut app, &worker, REPORT_BLOCKED, at + SECOND);
    assert_eq!(rings(&cmds), 1);
    assert_eq!(surfaces(&state, &app, &worker), (true, 1, NotIdle));

    // The row goes; the member facts still say Codex.
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir")]),
        at + 10 * SECOND,
    );
    assert!(app.row_for_id("w1").is_none());
    assert_eq!(surfaces(&state, &app, &worker), (true, 1, NotIdle));
    assert_eq!(rings(&cmds), 0);
    assert_eq!(rings(&tick(&mut state, &mut app, at + 20 * SECOND)), 0);
}

#[test]
fn a_report_that_clears_ends_the_mark_and_the_count_with_or_without_a_row_and_rings_nothing() {
    for (with_row, clear) in [
        (false, REPORT_IDLE),
        (false, REPORT_CLEAR),
        (false, REPORT_DONE),
        (true, REPORT_IDLE),
        (true, REPORT_CLEAR),
        (true, REPORT_DONE),
    ] {
        let (mut state, mut app, worker, at) = rowless_codex_worker();
        if with_row {
            send(
                &mut state,
                &mut app,
                rows_loaded(vec![row_for("dir"), codex_row("w1")]),
                at,
            );
        }
        assert_eq!(
            rings(&said(
                &mut state,
                &mut app,
                &worker,
                REPORT_BLOCKED,
                at + SECOND
            )),
            1
        );
        let cmds = said(&mut state, &mut app, &worker, clear, at + 10 * SECOND);
        let what = String::from_utf8_lossy(clear).escape_debug().to_string();
        assert!(
            !state.attention_panes().contains(&worker),
            "row {with_row}, {what}: mark gone"
        );
        assert_eq!(state.visible_waiting_count(), 0, "row {with_row}, {what}");
        assert_eq!(rings(&cmds), 0, "row {with_row}, {what}");
        assert_eq!(rings(&tick(&mut state, &mut app, at + 20 * SECOND)), 0);
    }
}

#[test]
fn a_pane_waiting_in_the_rows_and_by_its_report_is_counted_once() {
    // As a brigade member: hidden from the list, a tile only.
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    let mut waiting = codex_row("w1");
    waiting.activity = crate::model::Activity::Waiting;
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), waiting.clone()]),
        at,
    );
    assert_eq!(state.visible_waiting_count(), 1, "the row alone");
    assert_eq!(rings(&cmds), 1, "its tile is marked: one bell");
    let cmds = said(&mut state, &mut app, &worker, REPORT_BLOCKED, at + SECOND);
    assert_eq!(state.visible_waiting_count(), 1, "the row and the report");
    assert!(state.attention_panes().contains(&worker));
    assert_eq!(rings(&cmds), 0, "nothing new to ring for");

    // As a session in the list: it is a row of the list, a tile and a report.
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    let solo = host_solo(&mut state, "cx");
    let mut waiting = codex_row("cx");
    waiting.activity = crate::model::Activity::Waiting;
    app.replace_rows(vec![row_for("dir"), row_for("w1"), waiting]);
    app.set_hidden_member_ids(["dir".to_string(), "w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    state.stage = Stage::Solo(solo.clone());
    state.focus = Focus::Sidebar;
    sample(&mut state, &mut app, &[(&solo, Idle)], at);
    assert_eq!(
        state.visible_waiting_count(),
        1,
        "the list row and the tile"
    );
    said(&mut state, &mut app, &solo, REPORT_BLOCKED, at + SECOND);
    assert_eq!(app.filtered_waiting_count(), 1);
    assert_eq!(state.visible_waiting_count(), 1, "still one");
}

#[test]
fn the_ring_edge_of_a_row_less_blocked_pane_is_one_bell_on_gaining_and_none_after() {
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    let mut rung = rings(&said(&mut state, &mut app, &worker, REPORT_BLOCKED, at));
    assert_eq!(rung, 1, "on gaining");
    for secs in 1..=30 {
        rung += rings(&tick(&mut state, &mut app, at + secs * SECOND));
    }
    assert_eq!(rung, 1, "none on later ticks");
    // The same report again changes nothing.
    rung += rings(&said(
        &mut state,
        &mut app,
        &worker,
        REPORT_BLOCKED,
        at + 31 * SECOND,
    ));
    // The row then appearing as Codex adds no bell either.
    rung += rings(&send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), codex_row("w1")]),
        at + 40 * SECOND,
    ));
    assert_eq!(rung, 1);
}

#[test]
fn the_brigade_watch_and_the_quit_question_follow_the_same_decision_when_a_conflicting_row_arrives()
{
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir")]);
    let relay = vec![observed_as(
        &state,
        &worker,
        Some(crate::model::AgentKind::Codex),
    )];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        relay,
        at,
    );
    // Real work, armed by it, then idle by the rollout.
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at + SECOND);
    let both = [(&director, Idle), (&worker, Idle)];
    // The report says working: the rollout's idle does not count, the brigade
    // is held, and quitting asks.
    said(&mut state, &mut app, &worker, REPORT_WORKING, idle_from);
    for secs in 0..10 {
        assert!(!rang(&sample(
            &mut state,
            &mut app,
            &both,
            idle_from + secs * SECOND
        )));
    }
    press(
        &mut state,
        &mut app,
        KeyCode::Char('q'),
        idle_from + 10 * SECOND,
    );
    assert!(!app.should_quit(), "the report holds the quit question");

    // A row naming another product arrives: the report stops deciding for the
    // watch and the quit question alike, and the rollout-based sample (idle)
    // does, so the stretch can complete.
    let conflict_at = idle_from + 11 * SECOND;
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), claude_row("w1")]),
        conflict_at,
    );
    assert_eq!(state.quiescence_of(&app, &worker), Idle);
    assert_eq!(state.quit_counts(&app, conflict_at).not_idle(), 0);
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &both,
        conflict_at,
        conflict_at + 3 * BRIGADE_QUIET_SETTLE,
    );
    assert!(rang_at.is_some(), "the watch completes on the sample");
}

// --- a Claude member that has no row yet shows what its live file says -------------------------------

/// Brigade 1 staged with a Claude Director (a row) and a Worker with no row,
/// whose live file the relay read this tick as `live`.
fn rowless_claude_worker(
    live: Option<crate::engine::LiveStatus>,
) -> (EmporiumState, App, SessionKey, Instant) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir")]);
    app.set_viewport_height(20);
    let relay = vec![RelayObservation {
        live,
        ..observed_as(&state, &worker, Some(crate::model::AgentKind::ClaudeCode))
    }];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        relay,
        at + SECOND,
    );
    assert!(app.row_for_id("w1").is_none());
    (state, app, worker, at + 10 * SECOND)
}

fn live(activity: crate::model::Activity, dialog_open: bool) -> Option<crate::engine::LiveStatus> {
    Some(crate::engine::LiveStatus {
        activity,
        dialog_open,
        status_busy: false,
    })
}

/// A tick that observes the Worker as `live` (or not at all with `None` and an
/// empty relay), as the shell sends it.
fn observe(
    state: &mut EmporiumState,
    app: &mut App,
    worker: &SessionKey,
    live: Option<crate::engine::LiveStatus>,
    at: Instant,
) -> Vec<Cmd> {
    let relay = live
        .map(|live| {
            vec![RelayObservation {
                live: Some(live),
                ..observed_as(state, worker, Some(crate::model::AgentKind::ClaudeCode))
            }]
        })
        .unwrap_or_default();
    let brigades = listed(state);
    send(state, app, Event::Tick { relay, brigades }, at)
}

#[test]
fn a_row_less_claude_member_shows_the_word_its_live_file_gives_it() {
    use crate::model::Activity::{Alive, Busy, Waiting};
    for (live, expected) in [
        (live(Alive, false), Some(Alive)),
        (live(Busy, false), Some(Busy)),
        (live(Waiting, false), Some(Waiting)),
        (live(Waiting, true), Some(Waiting)),
        (None, None),
    ] {
        let (state, app, worker, _) = rowless_claude_worker(live);
        assert_eq!(
            state.displayed_activity(&app, &worker),
            expected,
            "{live:?}"
        );
    }
}

#[test]
fn a_row_less_claude_member_that_is_waiting_is_marked_counted_once_and_rung_for_once() {
    use crate::model::Activity::Waiting;
    let (mut state, mut app, worker, at) = rowless_claude_worker(None);
    let cmds = observe(&mut state, &mut app, &worker, live(Waiting, false), at);
    assert!(state.attention_panes().contains(&worker), "marked");
    assert_eq!(state.visible_waiting_count(), 1, "counted once");
    assert_eq!(rings(&cmds), 1, "rung for");
    for secs in 1..=5 {
        let cmds = observe(
            &mut state,
            &mut app,
            &worker,
            live(Waiting, false),
            at + secs * SECOND,
        );
        assert_eq!(rings(&cmds), 0, "once");
    }
    // It stops waiting: the mark and the count go.
    observe(
        &mut state,
        &mut app,
        &worker,
        live(crate::model::Activity::Alive, false),
        at + 10 * SECOND,
    );
    assert!(!state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 0);
}

#[test]
fn a_row_less_member_on_a_dialog_is_shown_waiting_and_not_rung_until_the_reason_changes() {
    use crate::model::Activity::Waiting;
    let (mut state, mut app, worker, at) = rowless_claude_worker(None);
    let cmds = observe(&mut state, &mut app, &worker, live(Waiting, true), at);
    assert!(state.attention_panes().contains(&worker), "marked");
    assert_eq!(state.visible_waiting_count(), 1, "counted");
    assert_eq!(rings(&cmds), 0, "a dialog does not ring");
    let cmds = observe(
        &mut state,
        &mut app,
        &worker,
        live(Waiting, false),
        at + 10 * SECOND,
    );
    assert_eq!(rings(&cmds), 1, "the reason changed while waiting: once");
}

#[test]
fn a_row_arriving_later_for_a_waiting_row_less_member_changes_nothing_and_rings_nothing() {
    use crate::model::Activity::{Alive, Waiting};
    let (mut state, mut app, worker, at) = rowless_claude_worker(None);
    assert_eq!(
        rings(&observe(
            &mut state,
            &mut app,
            &worker,
            live(Waiting, false),
            at
        )),
        1
    );
    // The row says waiting too (the same live file, read by the list).
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), waiting_row("w1", false)]),
        at + 10 * SECOND,
    );
    assert_eq!(rings(&cmds), 0);
    assert!(state.attention_panes().contains(&worker));
    assert_eq!(state.visible_waiting_count(), 1);
    // And a row that says otherwise is the one that counts, as for any pane.
    let cmds = send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), row_for("w1")]),
        at + 20 * SECOND,
    );
    assert_eq!(rings(&cmds), 0);
    assert!(!state.attention_panes().contains(&worker));
    assert_eq!(state.displayed_activity(&app, &worker), Some(Alive));
}

// --- the word a hosted pane's tile carries -----------------------------------------------
//
// What the operator wants from a tile is whether the session is working, waiting
// on him or idle, the same way for either product. "Alive" is a word in contrast
// to "dead", and a hosted pane is never that: so the word is `busy`, `waiting` or
// `idle`, from what banto has read of the pane, and nothing before it has read
// anything.

fn row_that_says(row: SessionRow, activity: crate::model::Activity) -> SessionRow {
    SessionRow { activity, ..row }
}

#[test]
fn a_codex_pane_that_reports_idle_reads_idle_whatever_its_row_says() {
    use crate::model::Activity::{Alive, Idle as Dormant};
    use crate::model::AgeBucket::{Older, Today};
    // The incident: a hosted Codex pane at its prompt read "idle" for a minute,
    // meaning "no live process proven", and "alive" once a reload found its pid.
    for report in [REPORT_IDLE, REPORT_DONE, REPORT_ERROR] {
        for row in [Alive, Dormant(Today), Dormant(Older)] {
            let (mut state, mut app, pane, at) = solo_fixture(row_that_says(codex_row("cx"), row));
            said(&mut state, &mut app, &pane, report, at + SECOND);
            assert_eq!(
                state.pane_word(&app, &pane),
                Some(Alive),
                "{row:?}, report {report:?}"
            );
        }
    }
}

#[test]
fn a_codex_pane_that_never_reports_reads_what_the_sample_says() {
    use crate::model::Activity::{Alive, Busy};
    for (reading, word) in [(Idle, Some(Alive)), (NotIdle, Some(Busy)), (Unknown, None)] {
        let (mut state, mut app, pane, at) = solo_fixture(row_that_says(codex_row("cx"), Alive));
        sample(&mut state, &mut app, &[(&pane, reading)], at + SECOND);
        assert_eq!(state.pane_word(&app, &pane), word, "{reading:?}");
    }
}

#[test]
fn a_claude_pane_reads_what_its_live_file_was_sampled_as_not_what_its_row_was() {
    use crate::model::Activity::{Alive, Busy};
    // A live file that says idle, or anything this build does not know (the row
    // says Alive for both), is sampled Idle or NotIdle, and so is the word.
    for (reading, word) in [(Idle, Some(Alive)), (NotIdle, Some(Busy))] {
        let (mut state, mut app, pane, at) = solo_fixture(row_that_says(row_for("cl"), Alive));
        sample(&mut state, &mut app, &[(&pane, reading)], at + SECOND);
        assert_eq!(state.pane_word(&app, &pane), word, "{reading:?}");
    }
    // `shell` and busy: the row says Busy, and the sample is not idle.
    let (mut state, mut app, pane, at) = solo_fixture(row_that_says(row_for("cl"), Busy));
    sample(&mut state, &mut app, &[(&pane, NotIdle)], at + SECOND);
    assert_eq!(state.pane_word(&app, &pane), Some(Busy));
}

#[test]
fn a_member_with_no_row_reads_the_sample_of_its_live_file() {
    use crate::model::Activity::{Alive, Busy};
    // No row yet (no transcript before the first turn): what the tick's sample
    // made of the member's live file is the word.
    let (mut state, mut app, worker, at) = rowless_claude_worker(live(Alive, false));
    let director = key("dir");
    assert_eq!(state.pane_word(&app, &worker), Some(Alive), "idle");
    // The live file turns busy: the next tick samples it not idle.
    let relay = vec![RelayObservation {
        live: live(Busy, false),
        ..observed_as(&state, &worker, Some(crate::model::AgentKind::ClaudeCode))
    }];
    sample_with_relay(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        relay,
        at + SECOND,
    );
    assert_eq!(state.pane_word(&app, &worker), Some(Busy), "busy");
    // A tick that read nothing of it leaves nothing to say.
    sample(&mut state, &mut app, &[], at + 2 * SECOND);
    assert_eq!(state.pane_word(&app, &worker), None, "nothing read");
}

#[test]
fn a_member_whose_live_file_has_turned_waiting_reads_waiting_before_the_list_reloads() {
    use crate::model::Activity::{Alive, Busy, Waiting};
    // The row was last loaded as Alive and no reload has come since; the relay's
    // tick has read the live file, which now waits. The word says so. The marks
    // and the bell still read `displayed_activity`, row first, so for this short
    // window the tile says `waiting` before the mark appears.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir"), row_for("w1")]);
    app.set_viewport_height(20);
    let tick = |state: &mut EmporiumState,
                app: &mut App,
                live: Option<crate::engine::LiveStatus>,
                reading,
                secs: u32| {
        let relay = live
            .map(|live| {
                vec![RelayObservation {
                    live: Some(live),
                    ..observed_as(state, &worker, Some(crate::model::AgentKind::ClaudeCode))
                }]
            })
            .unwrap_or_default();
        sample_with_relay(
            state,
            app,
            &[(&director, Idle), (&worker, reading)],
            relay,
            at + secs * SECOND,
        )
    };
    let status = |activity, dialog_open| crate::engine::LiveStatus {
        activity,
        dialog_open,
        status_busy: false,
    };
    tick(&mut state, &mut app, Some(status(Alive, false)), Idle, 1);
    assert_eq!(state.pane_word(&app, &worker), Some(Alive));

    tick(
        &mut state,
        &mut app,
        Some(status(Waiting, false)),
        NotIdle,
        2,
    );
    assert_eq!(
        state.pane_word(&app, &worker),
        Some(Waiting),
        "the live file waits"
    );
    assert_eq!(
        state.displayed_activity(&app, &worker),
        Some(Alive),
        "the decision the marks read is the row's, as before"
    );
    assert!(!state.attention_panes().contains(&worker), "no mark yet");

    // A dialog the operator opened is still waiting, as ruled.
    tick(
        &mut state,
        &mut app,
        Some(status(Waiting, true)),
        NotIdle,
        3,
    );
    assert_eq!(state.pane_word(&app, &worker), Some(Waiting), "a dialog");

    // The live file stops waiting: the word follows the sample.
    tick(&mut state, &mut app, Some(status(Busy, false)), NotIdle, 4);
    assert_eq!(state.pane_word(&app, &worker), Some(Busy));
    tick(&mut state, &mut app, Some(status(Alive, false)), Idle, 5);
    assert_eq!(state.pane_word(&app, &worker), Some(Alive));

    // And a status cannot outlive its tick: one that read nothing of the member
    // takes `waiting` away, whatever was seen before.
    tick(
        &mut state,
        &mut app,
        Some(status(Waiting, false)),
        NotIdle,
        6,
    );
    assert_eq!(state.pane_word(&app, &worker), Some(Waiting));
    tick(&mut state, &mut app, None, NotIdle, 7);
    assert_eq!(
        state.pane_word(&app, &worker),
        Some(Busy),
        "a stale wait is gone"
    );
}

#[test]
fn what_waits_on_the_operator_reads_waiting_whatever_the_sample_says() {
    use crate::model::Activity::Waiting;
    // A Claude row that says waiting (a permission prompt, or a dialog the
    // operator opened), and a Codex pane that reports blocked.
    for dialog in [false, true] {
        let mut row = row_that_says(row_for("cl"), Waiting);
        row.dialog_open = dialog;
        let (mut state, mut app, pane, at) = solo_fixture(row);
        sample(&mut state, &mut app, &[(&pane, NotIdle)], at + SECOND);
        assert_eq!(
            state.pane_word(&app, &pane),
            Some(Waiting),
            "dialog {dialog}"
        );
    }
    let (mut state, mut app, pane, at) = solo_fixture(codex_row("cx"));
    sample(&mut state, &mut app, &[(&pane, Idle)], at + SECOND);
    said(&mut state, &mut app, &pane, REPORT_BLOCKED, at + 2 * SECOND);
    assert_eq!(state.pane_word(&app, &pane), Some(Waiting));
}

#[test]
fn nothing_is_said_of_a_pane_before_banto_has_read_it_and_again_after_a_respawn() {
    use crate::model::Activity::Alive;
    let (mut state, mut app, pane, at) = solo_fixture(row_that_says(row_for("cl"), Alive));
    assert_eq!(state.pane_word(&app, &pane), Some(Alive), "read once");
    // The process is replaced under the same key: nothing is known about the new
    // one, and the row of the old one says nothing about it.
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: pane.clone(),
            agent: None,
        },
        at + SECOND,
    );
    assert_eq!(state.pane_word(&app, &pane), None, "just spawned");
    sample(&mut state, &mut app, &[(&pane, Idle)], at + 2 * SECOND);
    assert_eq!(state.pane_word(&app, &pane), Some(Alive));
}

#[test]
fn a_key_typed_into_a_pane_does_not_make_its_word_busy_for_the_quit_windows_three_seconds() {
    use crate::model::Activity::Alive;
    let (mut state, mut app, pane, at) = solo_fixture(row_that_says(row_for("cl"), Alive));
    state.focus = Focus::Pane;
    press(&mut state, &mut app, KeyCode::Char('x'), at + SECOND);
    // The quit question asks (it counts a pane that just took input) ...
    assert_eq!(state.quit_counts(&app, at + SECOND).not_idle(), 1);
    // ... and the word does not follow it.
    assert_eq!(state.pane_word(&app, &pane), Some(Alive));
}

#[test]
fn the_word_is_not_what_the_marks_and_the_bell_read() {
    // A pane whose row has not caught up (a rename window): the word reads the
    // sample, but `displayed_activity` still says nothing is known, which is what
    // keeps an announced wait from ringing a second time as the row arrives.
    let (mut state, mut app, _director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir")]);
    sample(&mut state, &mut app, &[(&worker, NotIdle)], at + SECOND);
    assert!(app.row_for_id("w1").is_none());
    assert_eq!(state.displayed_activity(&app, &worker), None);
    assert_eq!(
        state.pane_word(&app, &worker),
        Some(crate::model::Activity::Busy)
    );
}

#[test]
fn a_hosted_rows_dot_says_live_and_goes_when_the_pane_does() {
    use crate::model::Activity::Alive;
    use crate::model::AgeBucket::Today;
    // Laid over the list, as what a pane reports is: a pane banto hosts is not a
    // row with no live process. A reload cannot lose it, and ending the pane
    // gives the row back its own word.
    let (mut state, mut app, pane, at) = solo_fixture(row_that_says(
        codex_row("cx"),
        crate::model::Activity::Idle(Today),
    ));
    send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        },
        at + SECOND,
    );
    assert_eq!(row_activity(&app, "cx"), Alive);
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_that_says(
            codex_row("cx"),
            crate::model::Activity::Idle(Today),
        )]),
        at + 2 * SECOND,
    );
    assert_eq!(row_activity(&app, "cx"), Alive, "a reload keeps it");
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: pane.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + 3 * SECOND,
    );
    assert_eq!(
        row_activity(&app, "cx"),
        crate::model::Activity::Idle(Today),
        "the pane is gone"
    );
}

#[test]
fn the_word_comes_from_the_report_then_the_row_then_the_live_file() {
    use crate::model::Activity::{Alive, Busy, Waiting};
    // A Codex pane's believed report beats its row, which beats the facts.
    let (mut state, mut app, worker, at) = rowless_claude_worker(live(Waiting, false));
    assert_eq!(
        state.displayed_activity(&app, &worker),
        Some(Waiting),
        "facts"
    );
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), row_for("w1")]),
        at,
    );
    assert_eq!(state.displayed_activity(&app, &worker), Some(Alive), "row");
    // A Codex member: its report decides over both.
    let (mut state, mut app, worker, at) = rowless_codex_worker();
    send(
        &mut state,
        &mut app,
        rows_loaded(vec![row_for("dir"), codex_row("w1")]),
        at,
    );
    said(&mut state, &mut app, &worker, REPORT_WORKING, at + SECOND);
    assert_eq!(
        state.displayed_activity(&app, &worker),
        Some(Busy),
        "report"
    );
}

#[test]
fn a_tick_that_did_not_observe_the_member_leaves_no_word_for_it() {
    use crate::model::Activity::Waiting;
    let (mut state, mut app, worker, at) = rowless_claude_worker(live(Waiting, false));
    assert_eq!(state.displayed_activity(&app, &worker), Some(Waiting));
    assert!(state.attention_panes().contains(&worker));
    // A roster that could not be read: the tick observes nobody. What the last
    // tick saw is not carried over; the tile shows no word until one is seen.
    observe(&mut state, &mut app, &worker, None, at + 10 * SECOND);
    assert_eq!(state.displayed_activity(&app, &worker), None);
    assert!(!state.attention_panes().contains(&worker));
    // The agent it was resolved as is kept, as before.
    assert_eq!(
        state.member_facts(&worker).and_then(|facts| facts.agent),
        Some(crate::model::AgentKind::ClaudeCode)
    );
    // And the next good tick brings it back.
    observe(
        &mut state,
        &mut app,
        &worker,
        live(Waiting, false),
        at + 20 * SECOND,
    );
    assert_eq!(state.displayed_activity(&app, &worker), Some(Waiting));
}

#[test]
fn the_live_word_of_a_member_never_feeds_the_quit_question_or_the_watch() {
    use crate::model::Activity::Waiting;
    // The word is display; idleness is the sample's: a row-less Worker whose
    // file says waiting but whose sample says idle is idle for both.
    let (state, app, worker, _) = rowless_claude_worker(live(Waiting, false));
    assert_eq!(state.quiescence_of(&app, &worker), Idle);
    assert_eq!(state.quit_counts(&app, t0()).not_idle(), 0);
}

// --- S1: an off-stage brigade speaks through its Director's row --------------------------------

#[test]
fn an_off_stage_brigades_quiet_fire_rings_and_marks_its_directors_row_and_staging_clears_it_without_a_second_ring()
 {
    // Through the real staging path (`MembershipResolved`): two brigades hosted,
    // the operator on brigade 1, brigade 2 goes quiet after work.
    let off_stage = OffStage::new(OffStageSays::QuietFire, true);
    assert_eq!(
        off_stage.rings_before, 1,
        "the fire rings while the brigade is off stage"
    );
    assert!(
        off_stage.app.is_waiting_id("dir2"),
        "its Director's row is marked"
    );
    assert!(off_stage.app.is_filtered_waiting_id("dir2"));
    assert_eq!(off_stage.app.filtered_waiting_count(), 1);
    assert_eq!(off_stage.state.visible_waiting_count(), 1, "`waiting 1`");

    // Staging it reaches the Director's pane: the mark is over, and nothing
    // rings again, with the window focused (F4 off or on).
    for f4 in [false, true] {
        let mut staged = OffStage::new(OffStageSays::QuietFire, true);
        let cmds = staged.stage_by_enter(true, f4);
        staged.assert_focus_landed_on_the_director();
        assert_eq!(rings(&cmds), 0, "f4 {f4}: no second ring");
        assert!(!staged.app.is_waiting_id("dir2"), "f4 {f4}: mark ended");
        assert_eq!(staged.app.filtered_waiting_count(), 0);
        assert_eq!(staged.state.visible_waiting_count(), 0);
        for secs in [1, 30] {
            assert_eq!(rings(&staged.tick_after_staging(secs)), 0, "+{secs}s");
        }
    }
}

#[test]
fn a_fire_rings_while_the_operator_is_working_in_another_brigade() {
    for says in [
        OffStageSays::QuietFire,
        OffStageSays::ClaudeWaiting,
        OffStageSays::Bell,
    ] {
        let off_stage = OffStage::new_with(says, matches!(says, OffStageSays::QuietFire), true);
        assert_eq!(effective_focused_pane(&off_stage.state), Some(key("dir")));
        assert_eq!(off_stage.rings_before, 1, "{says:?}");
        assert!(off_stage.app.is_waiting_id("dir2"), "{says:?}");
        assert_eq!(off_stage.state.visible_waiting_count(), 1, "{says:?}");
    }
}

#[test]
fn an_off_stage_member_waiting_on_a_dialog_neither_marks_nor_rings() {
    let off_stage = OffStage::new(OffStageSays::ClaudeDialog, false);
    assert_eq!(off_stage.rings_before, 0);
    assert!(!off_stage.app.is_waiting_id("dir2"));
    assert_eq!(off_stage.state.visible_waiting_count(), 0);
}

#[test]
fn a_staged_brigades_waiting_member_does_not_also_mark_its_directors_row() {
    // Its tiles speak for it, and marking the row too would count one waiting
    // Worker twice.
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    let mut rows = vec![row_for("dir"), row_for("w1")];
    rows[1].activity = crate::model::Activity::Waiting;
    app.replace_rows(rows);
    app.set_hidden_member_ids(["w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    tick(&mut state, &mut app, at + SECOND);
    assert!(
        !app.is_waiting_id("dir"),
        "the Director's row is not marked"
    );
    assert!(app.is_waiting_id("w1"));
    assert_eq!(
        state.visible_waiting_count(),
        1,
        "counted once, as its tile"
    );
}

#[test]
fn a_hidden_directors_row_does_not_silence_the_bell() {
    // A search that hides the Director's row takes it out of the list's own
    // Waiting sets; the ring is the mark's, not the list's.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row_for("dir2"), row_for("w2")]);
    app.set_hidden_member_ids(["w2".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    app.enter_search();
    for c in "zzzz".chars() {
        app.push_char(c);
    }
    assert_eq!(app.filtered_len(), 0, "the row is filtered out");
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");
    let at = t0();
    let working = [(&d2, NotIdle), (&w2, Idle)];
    let idle = [(&d2, Idle), (&w2, Idle)];
    sample(&mut state, &mut app, &working, at);
    sample(&mut state, &mut app, &idle, at + SECOND);
    let cmds = sample(
        &mut state,
        &mut app,
        &idle,
        at + SECOND + BRIGADE_QUIET_SETTLE,
    );
    assert!(rang(&cmds), "{cmds:?}");
    assert!(app.has_brigade_mark("dir2"));
}

// --- the mark is display only ---------------------------------------------------------------

/// Brigade 1 staged, the list focused, with rows for both members, and a quiet
/// fire that has marked the Director's row (not yet looked at).
fn marked_fixture() -> (EmporiumState, App, SessionKey, SessionKey, Instant) {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    app.replace_rows(vec![row_for("dir"), row_for("w1")]);
    app.set_hidden_member_ids(["w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
    let fired_at = idle_from + BRIGADE_QUIET_SETTLE;
    let cmds = sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, Idle)],
        fired_at,
    );
    assert!(rang(&cmds), "it fired");
    assert!(app.has_brigade_mark("dir"), "and its row is marked");
    (state, app, director, worker, fired_at)
}

#[test]
fn a_marked_directors_row_does_not_make_its_brigade_not_idle_for_the_quit_question() {
    for code in [KeyCode::Esc, KeyCode::Char('q')] {
        let (mut state, mut app, director, _worker, fired_at) = marked_fixture();
        assert_eq!(state.quiescence_of(&app, &director), Idle);
        assert_eq!(state.quit_counts(&app, fired_at + SECOND).not_idle(), 0);
        press(&mut state, &mut app, code, fired_at + SECOND);
        assert!(
            app.should_quit(),
            "{code:?}: one press quits an idle brigade whose row is marked"
        );
    }
}

#[test]
fn a_marked_directors_row_neither_holds_nor_rearms_its_own_watch() {
    let (mut state, mut app, director, worker, fired_at) = marked_fixture();
    let all_idle = [(&director, Idle), (&worker, Idle)];
    // Spent by the fire: a marked row does not re-arm it, however long it stays.
    assert!(!state.brigade_quiet[&1].active);
    for secs in 1..60 {
        let cmds = sample(&mut state, &mut app, &all_idle, fired_at + secs * SECOND);
        assert!(!rang(&cmds), "at +{secs}s: {cmds:?}");
    }
    assert!(!state.brigade_quiet[&1].active);
    assert!(app.has_brigade_mark("dir"), "still not looked at");

    // And it does not hold it: work, then quiet, fires as it would without a mark.
    let again = fired_at + 70 * SECOND;
    sample(
        &mut state,
        &mut app,
        &[(&director, Idle), (&worker, NotIdle)],
        again,
    );
    assert!(state.brigade_quiet[&1].active, "armed by the work");
    sample(&mut state, &mut app, &all_idle, again + SECOND);
    let cmds = sample(
        &mut state,
        &mut app,
        &all_idle,
        again + SECOND + BRIGADE_QUIET_SETTLE,
    );
    assert!(
        !state.brigade_quiet[&1].active,
        "fired again, with the old mark standing"
    );
    assert!(
        !rang(&cmds),
        "a second fire under a standing mark is silent: {cmds:?}"
    );
}

#[test]
fn a_marked_directors_row_does_not_change_what_the_relay_decides() {
    // The same brigade, once with a standing mark and once without, given the
    // same unread mail: the same nudge goes out.
    let nudges = |marked: bool| {
        let (mut state, mut app, director, worker, at) = if marked {
            marked_fixture()
        } else {
            let (state, app, director, worker, at) = quiet_fixture();
            (state, app, director, worker, at + 40 * SECOND)
        };
        let observation = || RelayObservation {
            live: None,
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
            briefed_since_launch: None,
        };
        let idle = [(&director, Idle), (&worker, Idle)];
        let mut written = Vec::new();
        for secs in 20..30 {
            let cmds = sample_with_relay(
                &mut state,
                &mut app,
                &idle,
                vec![observation()],
                at + secs * SECOND,
            );
            written.extend(
                cmds.into_iter()
                    .filter(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            );
        }
        written
    };
    let with_mark = nudges(true);
    let without = nudges(false);
    assert!(!with_mark.is_empty(), "a nudge went out");
    assert_eq!(with_mark, without);
}

#[test]
fn a_marked_directors_word_on_its_tile_stays_its_own_state() {
    // The mark is colour and a dot, never the session's state: what the tile's
    // title and the summary say is read from the row's own activity.
    for says in [OffStageSays::QuietFire, OffStageSays::ClaudeWaiting] {
        let off_stage = OffStage::new(says, matches!(says, OffStageSays::QuietFire));
        assert!(off_stage.app.is_waiting_id("dir2"), "{says:?}: marked");
        let row = off_stage.app.row_for_id("dir2").unwrap();
        assert_eq!(
            row.activity,
            crate::model::Activity::Alive,
            "{says:?}: the row's own state"
        );
        assert_eq!(
            off_stage
                .state
                .displayed_activity(&off_stage.app, &off_stage.d2),
            Some(crate::model::Activity::Alive),
            "{says:?}: the word the tile shows"
        );
    }
}

// --- one event, one ring ------------------------------------------------------------------

#[test]
fn an_off_stage_members_wait_rings_once_across_the_event_and_staging_and_the_mark_moves_to_its_tile()
 {
    for says in [
        OffStageSays::Bell,
        OffStageSays::ClaudeWaiting,
        OffStageSays::CodexBlocked,
    ] {
        for window_focused in [true, false] {
            for f4 in [false, true] {
                let what = format!("{says:?}, window focused {window_focused}, f4 {f4}");
                let mut off_stage = OffStage::new(says, false);
                let staged = off_stage.stage_by_enter(window_focused, f4);
                off_stage.assert_focus_landed_on_the_director();
                assert_eq!(
                    off_stage.rings_before + rings(&staged),
                    1,
                    "{what}: one ring for the event, through the row, none at staging"
                );
                assert_eq!(off_stage.marks(), ["w2"], "{what}: the mark is on the tile");
                // A BEL's mark is not a Waiting row, so it is not counted.
                let counted = usize::from(!matches!(says, OffStageSays::Bell));
                assert_eq!(off_stage.state.visible_waiting_count(), counted, "{what}");
                for secs in [1, 30] {
                    assert_eq!(rings(&off_stage.tick_after_staging(secs)), 0, "{what}");
                }
            }
        }
    }
}

#[test]
fn a_new_wait_after_staging_rings_as_before() {
    use crate::model::Activity;
    // A BEL: seen (the focus goes to the Worker's tile, F3), then a second BEL
    // is a new wait and rings.
    let mut off_stage = OffStage::new(OffStageSays::Bell, false);
    off_stage.stage_by_enter(true, false);
    let at = off_stage.staged_at() + 40 * SECOND;
    press(&mut off_stage.state, &mut off_stage.app, KeyCode::F(3), at);
    assert!(off_stage.marks().is_empty(), "seen, so cleared");
    // Back on the Director's tile, then the Worker's child rings again.
    press(
        &mut off_stage.state,
        &mut off_stage.app,
        KeyCode::F(3),
        at + SECOND,
    );
    let w2 = off_stage.w2.clone();
    let cmds = said(
        &mut off_stage.state,
        &mut off_stage.app,
        &w2,
        b"\x07",
        at + 10 * SECOND,
    );
    assert_eq!(rings(&cmds), 1, "a new wait rings: {cmds:?}");

    // A waiting session: the wait ends, then another begins after staging.
    let mut off_stage = OffStage::new(OffStageSays::ClaudeWaiting, false);
    off_stage.stage_by_enter(true, false);
    let set_w2 = |off_stage: &mut OffStage, activity: Activity| {
        let rows: Vec<_> = ["dir", "w1", "dir2", "w2"]
            .iter()
            .map(|id| {
                let mut row = row_for(id);
                if *id == "w2" {
                    row.activity = activity;
                }
                row
            })
            .collect();
        off_stage.app.replace_rows(rows);
    };
    set_w2(&mut off_stage, Activity::Alive);
    tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 70 * SECOND,
    );
    set_w2(&mut off_stage, Activity::Waiting);
    let cmds = tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 80 * SECOND,
    );
    assert_eq!(rings(&cmds), 1, "the second wait is new: {cmds:?}");
}

#[test]
fn a_member_never_announced_through_the_row_still_rings_when_its_wait_becomes_real_after_staging() {
    use crate::model::Activity;
    // Off stage it is waiting only on a dialog the operator opened: not marked,
    // not rung for.
    let mut off_stage = OffStage::new(OffStageSays::ClaudeDialog, false);
    assert_eq!(off_stage.rings_before, 0);
    let staged = off_stage.stage_by_enter(true, false);
    assert_eq!(
        rings(&staged),
        0,
        "a dialog does not ring at staging either"
    );
    // The reason changes to a real wait: it rings now, as it always did.
    let rows: Vec<_> = ["dir", "w1", "dir2", "w2"]
        .iter()
        .map(|id| {
            let mut row = row_for(id);
            if *id == "w2" {
                row.activity = Activity::Waiting;
            }
            row
        })
        .collect();
    off_stage.app.replace_rows(rows);
    let cmds = tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 80 * SECOND,
    );
    assert_eq!(rings(&cmds), 1, "{cmds:?}");
}

// --- a pane that is on stage speaks through its own tile --------------------------------------

#[test]
fn a_waiting_worker_whose_tile_is_soloed_is_counted_once_and_does_not_mark_its_directors_row() {
    // The tile's solo button (prefix `z`) is a layout preference inside the staged
    // brigade: every pane stays staged, so the rule that a pane on stage speaks
    // through its own tile is what keeps this at one. (No real path puts a hidden
    // Worker in `Stage::Solo`: that comes from opening a session on its own, which
    // a brigade member's hidden row cannot be, and from a disband, which leaves
    // the Director.)
    let (mut state, mut app, _director, _worker, at) = quiet_fixture();
    let mut rows = vec![row_for("dir"), row_for("w1")];
    rows[1].activity = crate::model::Activity::Waiting;
    app.replace_rows(rows);
    app.set_hidden_member_ids(["w1".to_string()].into_iter().collect());
    app.set_viewport_height(20);
    state.focus = Focus::Pane;
    press(&mut state, &mut app, KeyCode::F(3), at + SECOND);
    state.prefix_armed = Some(at + 2 * SECOND);
    press(&mut state, &mut app, KeyCode::Char('z'), at + 2 * SECOND);
    assert_eq!(
        state.brigade_layouts[&1].solo_member.as_deref(),
        Some("worker-1"),
        "the Worker's tile is soloed"
    );
    assert!(
        matches!(&state.stage, Stage::Brigade { id: 1, .. }),
        "still staged"
    );
    tick(&mut state, &mut app, at + 3 * SECOND);
    assert!(app.is_waiting_id("w1"), "its own tile is marked");
    assert!(
        !app.is_waiting_id("dir"),
        "not also through the Director's row"
    );
    assert_eq!(state.visible_waiting_count(), 1, "`waiting 1`, not 2");
}

#[test]
fn taking_the_brigade_off_stage_again_marks_its_row_for_a_standing_wait_without_a_new_ring() {
    let mut off_stage = OffStage::new(OffStageSays::ClaudeWaiting, false);
    off_stage.stage_by_enter(true, false);
    assert!(
        !off_stage.app.is_waiting_id("dir2"),
        "on stage: the tile speaks"
    );
    // Off stage again — the operator opens a plain session on its own, as Enter on
    // its row does — with the Worker still waiting: the row speaks again, quietly,
    // because it is the same wait, already announced.
    let mut rows = vec![
        row_for("dir"),
        row_for("w1"),
        row_for("dir2"),
        row_for("w2"),
    ];
    rows[3].activity = crate::model::Activity::Waiting;
    off_stage.app.replace_rows(rows);
    let later = off_stage.at + 90 * SECOND;
    let cmds = open_a_plain_session_solo(&mut off_stage, later, "w2");
    assert!(off_stage.app.is_waiting_id("dir2"));
    assert_eq!(rings(&cmds), 0, "{cmds:?}");
}

// --- review fixes: one event, one ring, per event --------------------------------------------

/// Leave brigade 1 (hosted, off stage from now on) by opening a plain session
/// solo, the way Enter on its row does: the shell's `MembershipResolved` finds
/// no brigade, `open_solo` asks for a spawn, the spawn lands.
fn open_a_plain_session_solo(off_stage: &mut OffStage, at: Instant, waiting: &str) -> Vec<Cmd> {
    let mut rows = vec![
        row_for("dir"),
        row_for("w1"),
        row_for("dir2"),
        row_for("w2"),
    ];
    // The Worker the test is about is still waiting.
    for row in rows.iter_mut().filter(|row| row.id == waiting) {
        row.activity = crate::model::Activity::Waiting;
    }
    rows.push(row_for("plain"));
    off_stage.app.replace_rows(rows);
    off_stage.state.pending_membership = Some((PendingMembership::Activate, "plain".to_string()));
    let mut cmds = send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::MembershipResolved {
            session_id: "plain".to_string(),
            membership: None,
            members: None,
            layout: None,
            names: Vec::new(),
        },
        at,
    );
    let opened: Vec<SessionKey> = cmds
        .iter()
        .filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(opened, [key("plain")], "{cmds:?}");
    cmds.extend(send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::Spawned {
            key: key("plain"),
            agent: None,
        },
        at,
    ));
    assert!(matches!(&off_stage.state.stage, Stage::Solo(solo) if *solo == key("plain")));
    cmds
}

#[test]
fn a_wait_that_began_on_stage_does_not_ring_again_when_its_brigade_leaves_the_stage() {
    // R1. Brigade 1 is on stage and its Worker starts waiting: it rings on its
    // own tile. Then the operator stages brigade 2 (Enter on its Director's row),
    // or opens a plain session solo: brigade 1 is off stage with the wait still
    // standing, and its Director's row now speaks for it — without a second ring.
    for f4 in [false, true] {
        let mut off_stage = OffStage::new(OffStageSays::OnStageWait, false);
        assert_eq!(off_stage.rings_before, 1, "it rang on its own tile");
        off_stage.assert_it_rang_once_for_its_cause("on its own tile");
        assert!(
            !off_stage.app.is_waiting_id("dir"),
            "on stage: the tile speaks"
        );
        let staged = off_stage.stage_by_enter(true, f4);
        assert_eq!(rings(&staged), 0, "f4 {f4}: staging another brigade");
        assert!(
            off_stage.app.is_waiting_id("dir"),
            "brigade 1 is off stage: its Director's row speaks"
        );
        for secs in [1, 30] {
            assert_eq!(rings(&off_stage.tick_after_staging(secs)), 0, "+{secs}s");
        }
    }
    let mut off_stage = OffStage::new(OffStageSays::OnStageWait, false);
    let at = off_stage.staged_at();
    let cmds = open_a_plain_session_solo(&mut off_stage, at, "w1");
    assert_eq!(rings(&cmds), 0, "opening a session solo");
    assert!(off_stage.app.is_waiting_id("dir"));
    assert_eq!(rings(&off_stage.tick_after_staging(5)), 0);
}

#[test]
fn a_director_pane_that_is_renamed_keeps_its_mark_without_ringing_again() {
    // R2. An off-stage brigade fires and rings once. Its Director's session moves
    // (`/clear`) or is discovered: its pane and its row change key, and the next
    // tick must not take the mark for a new one.
    let mut off_stage = OffStage::new(OffStageSays::QuietFire, true);
    assert_eq!(off_stage.rings_before, 1);
    let at = off_stage.at + 30 * SECOND;
    let mut rung = 0;
    rung += rings(&send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::MemberSessionForked {
            brigade_id: 2,
            token: DIRECTOR_TOKEN.to_string(),
            old_id: "dir2".to_string(),
            new_id: "dir2b".to_string(),
            held_by_row: false,
        },
        at,
    ));
    rung += rings(&send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::RowsLoaded {
            rows: vec![
                row_for("dir"),
                row_for("w1"),
                row_for("dir2b"),
                row_for("w2"),
            ],
            hidden: ["dir", "w1", "w2"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            directors: ["dir", "dir2b"].into_iter().map(str::to_string).collect(),
        },
        at,
    ));
    assert_eq!(rung, 0, "the rename is not a new event");
    assert!(
        off_stage.app.is_waiting_id("dir2b"),
        "the mark followed the row"
    );
    for secs in [1, 2, 10] {
        let cmds = tick(&mut off_stage.state, &mut off_stage.app, at + secs * SECOND);
        assert_eq!(rings(&cmds), 0, "+{secs}s: {cmds:?}");
    }
    assert!(off_stage.app.is_waiting_id("dir2b"));
}

#[test]
fn a_members_wait_rings_when_it_is_announced_even_while_the_row_is_already_marked() {
    // R3. The brigade went quiet (row marked, rang). Later a Worker of it starts
    // waiting: that is another event and rings, though the row was already marked.
    let mut off_stage = OffStage::new(OffStageSays::QuietFire, true);
    assert_eq!(off_stage.rings_before, 1);
    let mut rows = vec![
        row_for("dir"),
        row_for("w1"),
        row_for("dir2"),
        row_for("w2"),
    ];
    rows[3].activity = crate::model::Activity::Waiting;
    off_stage.app.replace_rows(rows);
    let cmds = tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 60 * SECOND,
    );
    assert_eq!(rings(&cmds), 1, "{cmds:?}");
    // Still the one wait: no more rings while it stands.
    let cmds = tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 70 * SECOND,
    );
    assert_eq!(rings(&cmds), 0, "{cmds:?}");
    // A second quiet fire under a standing mark stays silent.
    assert!(off_stage.app.is_waiting_id("dir2"));
}

#[test]
fn a_hosted_member_that_is_not_on_the_stage_does_not_make_staging_ring_for_its_directors_mark() {
    // A member that is hosted but not among the staged brigade's panes speaks
    // through its Director's row; staging the brigade puts the Director's tile
    // under the same mark, and that is not a new event.
    let mut off_stage = OffStage::new(OffStageSays::ClaudeDialog, false);
    let w3 = host(&mut off_stage.state, 2, "worker-2", "w3");
    assert_ne!(w3, off_stage.w2);
    let mut rows = vec![
        row_for("dir"),
        row_for("w1"),
        row_for("dir2"),
        row_for("w2"),
    ];
    let mut third = row_for("w3");
    third.activity = crate::model::Activity::Waiting;
    rows.push(third);
    off_stage.app.replace_rows(rows);
    off_stage.app.set_hidden_member_ids(
        ["dir", "w1", "w2", "w3"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    );
    let announced = tick(
        &mut off_stage.state,
        &mut off_stage.app,
        off_stage.at + 10 * SECOND,
    );
    assert_eq!(
        rings(&announced),
        1,
        "its wait is announced through the row"
    );
    // The roster the store lists has no w3, so it is not among the panes.
    let staged = off_stage.stage_by_enter(true, false);
    off_stage.assert_focus_landed_on_the_director();
    assert!(
        !matches!(&off_stage.state.stage, Stage::Brigade { panes, .. } if panes.contains(&w3)),
        "w3 is hosted but not staged"
    );
    assert_eq!(
        off_stage.marks(),
        ["dir2"],
        "the Director's tile carries the mark"
    );
    assert_eq!(rings(&staged), 0, "and that is not a second event");
}

// --- a Goinkyo that ends or is replaced leaves nothing of itself on the next one --------------

fn glance_line(line: &str) -> crate::model::MessageGlance {
    crate::model::MessageGlance {
        created_at_ms: 1,
        line: line.to_string(),
        from_token: Some(GOINKYO_TOKEN.to_string()),
    }
}

fn facts_of_goinkyo(state: &EmporiumState) -> Option<&MemberFacts> {
    state.member_facts.get(&(1, GOINKYO_TOKEN.to_string()))
}

#[test]
fn a_goinkyos_glance_lines_do_not_outlive_its_consultation() {
    // Observed with lines while it lives.
    let (mut state, mut app, ..) = quiet_fixture();
    let g = host(&mut state, 1, GOINKYO_TOKEN, "g1");
    let line = glance_line("a verdict");
    let observation = RelayObservation {
        live: None,
        glance: Some(MemberGlance {
            inbound: None,
            sent: Some(line),
        }),
        brigade_id: 1,
        token: GOINKYO_TOKEN.to_string(),
        key: g.clone(),
        has_unseen: false,
        is_idle_this_tick: Some(true),
        agent: None,
        briefed_since_launch: None,
    };
    let at = t0();
    let brigades = listed(&state);
    send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: vec![observation],
            brigades,
        },
        at,
    );
    assert!(
        facts_of_goinkyo(&state).is_some_and(|facts| facts.sent.is_some()),
        "its sent line is shown while it lives"
    );

    // The consultation ends: the next gather finds no Goinkyo row.
    send(
        &mut state,
        &mut app,
        Event::GoinkyoAwaitingSpawn {
            observation: GoinkyoObservation::NoGoinkyo { brigade_id: 1 },
        },
        at + SECOND,
    );
    assert!(
        facts_of_goinkyo(&state).is_none(),
        "its lines are gone with it: a new Goinkyo under the token starts empty"
    );
}

#[test]
fn a_new_goinkyo_hosted_before_it_has_a_session_starts_with_no_facts_even_if_the_end_was_never_seen()
 {
    // Dismiss and consult again inside one tick: the gather never sees a
    // row-less moment, so no `NoGoinkyo`; the new consultation's spawn must clear
    // what the old one left.
    let (mut state, ..) = quiet_fixture();
    state.member_facts.insert(
        (1, GOINKYO_TOKEN.to_string()),
        MemberFacts {
            sent: Some(glance_line("old line")),
            ..Default::default()
        },
    );
    let cmds = update_goinkyo_awaiting_spawn(
        &mut state,
        &BrigadeConfig::default(),
        GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
            brigade_id: 1,
            cwd: std::path::PathBuf::from("/work"),
            roster: Vec::new(),
            agent: None,
            director_agent: None,
        }),
        t0(),
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
        "{cmds:?}"
    );
    assert!(facts_of_goinkyo(&state).is_none());
}

#[test]
fn a_members_pane_that_is_renamed_keeps_its_announcement_and_does_not_ring_again() {
    // The same one-event rule for a member: an off-stage Worker's wait rang
    // through its Director's row; its session moves, its pane and row change key.
    let mut off_stage = OffStage::new(OffStageSays::ClaudeWaiting, false);
    assert_eq!(off_stage.rings_before, 1);
    let at = off_stage.at + 30 * SECOND;
    let mut rung = rings(&send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::MemberSessionForked {
            brigade_id: 2,
            token: "worker-1".to_string(),
            old_id: "w2".to_string(),
            new_id: "w2b".to_string(),
            held_by_row: false,
        },
        at,
    ));
    let mut renamed = row_for("w2b");
    renamed.activity = crate::model::Activity::Waiting;
    rung += rings(&send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::RowsLoaded {
            rows: vec![row_for("dir"), row_for("w1"), row_for("dir2"), renamed],
            hidden: ["dir", "w1", "w2b"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            directors: ["dir", "dir2"].into_iter().map(str::to_string).collect(),
        },
        at,
    ));
    rung += rings(&tick(
        &mut off_stage.state,
        &mut off_stage.app,
        at + 5 * SECOND,
    ));
    assert_eq!(rung, 0, "the same wait under a new key is not a new wait");
    assert!(off_stage.app.is_waiting_id("dir2"));
}

#[test]
fn a_wait_shown_on_its_tile_keeps_its_announcement_when_its_pane_is_renamed_on_stage() {
    // Brigade 1's Worker waits on stage (rang on its own tile), its session moves
    // and its pane and row change key, and then the brigade leaves the stage:
    // still the one wait, still no second ring.
    let mut off_stage = OffStage::new(OffStageSays::OnStageWait, false);
    assert_eq!(off_stage.rings_before, 1);
    let at = off_stage.at + 10 * SECOND;
    let mut renamed = row_for("w1b");
    renamed.activity = crate::model::Activity::Waiting;
    // (While the row still carries the old id the staged pane's wait reads as
    // gone and then back, so its own tile rings once more: that is the staged
    // pane's edge as it always was, and not what this test is about.)
    send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::MemberSessionForked {
            brigade_id: 1,
            token: "worker-1".to_string(),
            old_id: "w1".to_string(),
            new_id: "w1b".to_string(),
            held_by_row: false,
        },
        at,
    );
    send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::RowsLoaded {
            rows: vec![row_for("dir"), renamed, row_for("dir2"), row_for("w2")],
            hidden: ["dir", "w1b", "w2"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            directors: ["dir", "dir2"].into_iter().map(str::to_string).collect(),
        },
        at,
    );
    let mut rung = rings(&off_stage.stage_by_enter(true, false));
    rung += rings(&off_stage.tick_after_staging(5));
    assert_eq!(rung, 0, "no second ring after the rename and the move");
    assert!(
        off_stage.app.is_waiting_id("dir"),
        "brigade 1's row speaks now"
    );
}

// --- an announcement ends with its pane and with its latch --------------------------------------

/// An off-stage brigade (2) with one more hosted member, `w3` (`worker-2`), that
/// has no list row and no live information: nothing is known about it but what
/// its child says.
fn off_stage_with_an_unknown_member() -> (OffStage, SessionKey) {
    let mut off_stage = OffStage::new(OffStageSays::ClaudeDialog, false);
    let w3 = host(&mut off_stage.state, 2, "worker-2", "w3");
    (off_stage, w3)
}

fn pane_exits(off_stage: &mut OffStage, pane: &SessionKey, at: Instant) {
    send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::PtyExited {
            key: pane.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    );
}

#[test]
fn an_announcement_does_not_outlive_its_pane() {
    // N1 a: a waiting member that was announced, whose pane then exits, leaves
    // nothing behind in the sets for the rest of the run.
    let mut off_stage = OffStage::new(OffStageSays::ClaudeWaiting, false);
    let w2 = off_stage.w2.clone();
    assert_eq!(off_stage.rings_before, 1);
    assert!(off_stage.state.row_announced_waits.contains(&w2));
    let later = off_stage.at + 20 * SECOND;
    pane_exits(&mut off_stage, &w2, later);
    assert!(
        !off_stage.state.row_announced_waits.contains(&w2),
        "{:?}",
        off_stage.state.row_announced_waits
    );
    assert!(!off_stage.state.tile_announced_waits.contains(&w2));

    // The same for a wait that stood on its own tile.
    let mut off_stage = OffStage::new(OffStageSays::OnStageWait, false);
    let w1 = key("w1");
    assert!(off_stage.state.tile_announced_waits.contains(&w1));
    let later = off_stage.at + 20 * SECOND;
    pane_exits(&mut off_stage, &w1, later);
    assert!(!off_stage.state.tile_announced_waits.contains(&w1));
}

#[test]
fn a_new_pane_under_a_key_that_was_announced_rings_for_its_own_wait() {
    // N1 b1: a hosted member nothing is known about rings a BEL (one ring); its
    // pane dies and another is hosted under the same key, and its BEL is a new
    // event.
    let (mut off_stage, w3) = off_stage_with_an_unknown_member();
    let at = off_stage.at + 10 * SECOND;
    let first = said(&mut off_stage.state, &mut off_stage.app, &w3, b"\x07", at);
    assert_eq!(rings(&first), 1, "{first:?}");
    pane_exits(&mut off_stage, &w3, at + 5 * SECOND);
    let again = host(&mut off_stage.state, 2, "worker-2", "w3");
    assert_eq!(again, w3);
    let second = said(
        &mut off_stage.state,
        &mut off_stage.app,
        &w3,
        b"\x07",
        at + 20 * SECOND,
    );
    assert_eq!(
        rings(&second),
        1,
        "a new pane's BEL is a new event: {second:?}"
    );
}

#[test]
fn a_bell_rings_again_after_the_operator_has_looked_at_it_and_left() {
    // N1 b2: BEL (one ring); the operator stages the brigade and focuses the
    // member's tile, which clears the latch; he leaves for another session; a
    // second BEL is a new event even though nothing is known about the member's
    // activity.
    let (mut off_stage, w3) = off_stage_with_an_unknown_member();
    let at = off_stage.at + 10 * SECOND;
    let first = said(&mut off_stage.state, &mut off_stage.app, &w3, b"\x07", at);
    assert_eq!(rings(&first), 1, "{first:?}");

    let roster = vec![
        (
            DIRECTOR_TOKEN.to_string(),
            BrigadeRole::Director,
            Some("dir2".to_string()),
        ),
        (
            "worker-1".to_string(),
            BrigadeRole::Worker,
            Some("w2".to_string()),
        ),
        (
            "worker-2".to_string(),
            BrigadeRole::Worker,
            Some("w3".to_string()),
        ),
    ];
    off_stage.stage_with_roster(true, false, roster);
    // Focus the member's tile (F3 steps through them).
    let mut t = off_stage.staged_at();
    for _ in 0..3 {
        if effective_focused_pane(&off_stage.state).as_ref() == Some(&w3) {
            break;
        }
        t += SECOND;
        press(&mut off_stage.state, &mut off_stage.app, KeyCode::F(3), t);
    }
    assert_eq!(
        effective_focused_pane(&off_stage.state),
        Some(w3.clone()),
        "its tile is focused, so its latch is cleared"
    );
    assert!(!off_stage.state.latched_attention_panes.contains(&w3));

    // He leaves: a plain session takes the stage and brigade 2 is off stage.
    open_a_plain_session_solo(&mut off_stage, t + 10 * SECOND, "none");
    let second = said(
        &mut off_stage.state,
        &mut off_stage.app,
        &w3,
        b"\x07",
        t + 40 * SECOND,
    );
    assert_eq!(rings(&second), 1, "a second BEL is a new event: {second:?}");
}

#[test]
fn a_bell_latched_member_renamed_keeps_its_latch_and_its_announcement() {
    // The rename window: the pane is renamed before its row exists. A latch is
    // carried by the rename, so the wait still stands, still announced, and the
    // row catching up (there is no row to catch up here) changes nothing.
    let (mut off_stage, w3) = off_stage_with_an_unknown_member();
    let at = off_stage.at + 10 * SECOND;
    let first = said(&mut off_stage.state, &mut off_stage.app, &w3, b"\x07", at);
    assert_eq!(rings(&first), 1);
    let mut rung = rings(&send(
        &mut off_stage.state,
        &mut off_stage.app,
        Event::MemberSessionForked {
            brigade_id: 2,
            token: "worker-2".to_string(),
            old_id: "w3".to_string(),
            new_id: "w3b".to_string(),
            held_by_row: false,
        },
        at + 5 * SECOND,
    ));
    rung += rings(&tick(
        &mut off_stage.state,
        &mut off_stage.app,
        at + 10 * SECOND,
    ));
    assert_eq!(rung, 0, "the same BEL under a new key is not a new event");
    let renamed = key("w3b");
    assert!(off_stage.state.latched_attention_panes.contains(&renamed));
    assert!(off_stage.state.row_announced_waits.contains(&renamed));
}

// --- a solo pane whose process moved to another session ---------------------------------

fn session_moved(old_id: &str, new_id: &str) -> Event {
    Event::PaneSessionMoved {
        old_id: old_id.to_string(),
        new_id: new_id.to_string(),
        held_by_row: false,
    }
}

#[test]
fn a_clear_in_a_solo_pane_renames_it_and_the_quit_question_reads_the_new_id() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    // After `/clear` no live file names `a` any more: the pane reads unknown,
    // and the quit confirmation says so.
    sample(&mut state, &mut app, &[(&a, Unknown), (&b, Idle)], at);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 not idle: a (unknown)")
    );
    state.focus = Focus::Sidebar;
    state.pending_quit = None;
    state.status = None;

    let cmds = send(&mut state, &mut app, session_moved("a", "a2"), at);

    // The pane is renamed and nothing is written: a solo pane has no store row.
    assert!(
        matches!(cmds.as_slice(), [Cmd::RekeyPty { from, to }] if *from == a && *to == key("a2")),
        "{cmds:?}"
    );
    assert!(!state.screens.contains_key(&a));
    assert!(state.screens.contains_key(&key("a2")));

    // The next sample reads it under the new id, and the count is right.
    let a2 = key("a2");
    sample(
        &mut state,
        &mut app,
        &[(&a2, Idle), (&b, Idle)],
        at + Duration::from_secs(1),
    );
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + Duration::from_secs(1),
    );
    assert!(app.should_quit(), "nothing is left to ask about");
}

#[test]
fn a_branch_in_a_solo_pane_is_followed_with_its_screen_and_its_place_on_the_stage() {
    // `/branch` moves the process to a copy of the conversation under a new id:
    // to the pane that is the same event as `/clear`.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    state.stage = Stage::Solo(a.clone());
    let at = t0();

    send(&mut state, &mut app, session_moved("a", "a-branch"), at);

    let branch = key("a-branch");
    assert!(matches!(&state.stage, Stage::Solo(k) if *k == branch));
    assert!(state.screens.contains_key(&branch));
    assert!(!state.screens.contains_key(&a));
    // A repeat of the observation (the shell sends a move again every few ticks
    // while it still sees it) finds no pane under the old key and does nothing.
    let again = send(&mut state, &mut app, session_moved("a", "a-branch"), at);
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn an_in_pane_resume_onto_a_session_open_elsewhere_is_refused_and_said_once() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    let b = host_solo(&mut state, "b");
    let at = t0();

    let first = send(&mut state, &mut app, session_moved("a", "b"), at);

    // Renaming `a` onto `b` would put two panes on one key, and a resume of a
    // conversation another pane is running is the double resume.
    assert!(first.is_empty(), "{first:?}");
    assert!(state.screens.contains_key(&a) && state.screens.contains_key(&b));
    let said = state.status.clone().expect("the refusal is said");
    assert!(said.contains("already in use"), "{said}");

    state.status = None;
    let second = send(
        &mut state,
        &mut app,
        session_moved("a", "b"),
        at + Duration::from_secs(1),
    );
    assert!(second.is_empty());
    assert_eq!(state.status, None, "not said again every tick");

    // A different id for the same pane is a new fact and is said.
    state.screens.insert(key("c"), Screen::new(24, 80));
    send(
        &mut state,
        &mut app,
        session_moved("a", "c"),
        at + Duration::from_secs(2),
    );
    assert!(state.status.is_some());
}

#[test]
fn an_in_pane_resume_onto_a_session_that_is_being_opened_is_refused_and_said_once() {
    // The operator pressed Enter on the row of `p` (an open is in flight, no pane
    // yet) and then resumed `p` from inside pane `a`: renaming `a` onto `p` would
    // meet the spawn's own pane under the same key.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    state.pending_opens.insert(key("p"), PendingOpen::Solo);
    let at = t0();

    let first = send(&mut state, &mut app, session_moved("a", "p"), at);

    assert!(first.is_empty(), "{first:?}");
    assert!(state.screens.contains_key(&a) && !state.screens.contains_key(&key("p")));
    assert!(state.status.is_some(), "the refusal is said");
    state.status = None;
    send(
        &mut state,
        &mut app,
        session_moved("a", "p"),
        at + Duration::from_secs(1),
    );
    assert_eq!(state.status, None, "not said again every tick");
}

fn session_moved_onto_a_row(old_id: &str, new_id: &str) -> Event {
    Event::PaneSessionMoved {
        old_id: old_id.to_string(),
        new_id: new_id.to_string(),
        held_by_row: true,
    }
}

#[test]
fn a_solo_pane_is_not_renamed_onto_a_session_another_members_row_holds() {
    // The member's pane is closed, so no pane holds the id: only the roster does.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    let at = t0();

    let first = send(
        &mut state,
        &mut app,
        session_moved_onto_a_row("a", "held"),
        at,
    );

    assert!(first.is_empty(), "{first:?}");
    assert!(state.screens.contains_key(&a) && !state.screens.contains_key(&key("held")));
    assert!(state.status.is_some(), "the refusal is said");
    state.status = None;
    send(
        &mut state,
        &mut app,
        session_moved_onto_a_row("a", "held"),
        at + Duration::from_secs(1),
    );
    assert_eq!(state.status, None, "not said again every tick");
}

#[test]
fn a_member_whose_process_moved_onto_a_session_another_members_row_holds_writes_nothing() {
    // `SetMemberSession` moves an id between rows, so writing it would silently
    // clear the link of the member that has it.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let w = host(&mut state, 1, "worker-1", "w1");
    let at = t0();
    let moved = || Event::MemberSessionForked {
        brigade_id: 1,
        token: "worker-1".to_string(),
        old_id: "w1".to_string(),
        new_id: "held".to_string(),
        held_by_row: true,
    };

    let first = send(&mut state, &mut app, moved(), at);

    assert!(first.is_empty(), "no rename and no store write: {first:?}");
    assert!(state.screens.contains_key(&w));
    assert!(state.status.is_some(), "the refusal is said");
    state.status = None;
    send(&mut state, &mut app, moved(), at + Duration::from_secs(1));
    assert_eq!(state.status, None, "not said again every tick");
}

#[test]
fn a_member_whose_process_moved_onto_a_session_being_opened_writes_nothing() {
    // The in-pane switch lands on a thread the operator just pressed Enter on.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let w = host(&mut state, 1, "worker-1", "w1");
    state.pending_opens.insert(key("held"), PendingOpen::Solo);

    let refused = send(&mut state, &mut app, member_moved("held", false), t0());

    assert!(
        refused.is_empty(),
        "no rename and no store write: {refused:?}"
    );
    assert!(state.screens.contains_key(&w));
    assert!(state.status.is_some(), "the refusal is said");
}

fn member_moved(new_id: &str, held_by_row: bool) -> Event {
    Event::MemberSessionForked {
        brigade_id: 1,
        token: "worker-1".to_string(),
        old_id: "w1".to_string(),
        new_id: new_id.to_string(),
        held_by_row,
    }
}

#[test]
fn a_refused_solo_move_is_renamed_when_the_open_that_held_it_fails() {
    // Enter on row `p`, then an in-pane `/resume p` in pane `a`: refused while
    // the open is in flight. The open fails (it says "already running
    // elsewhere"), the process stays on `p`, and the shell sends the move again.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    state.pending_opens.insert(key("p"), PendingOpen::Solo);
    let at = t0();

    let refused = send(&mut state, &mut app, session_moved("a", "p"), at);
    assert!(refused.is_empty(), "{refused:?}");
    assert!(state.screens.contains_key(&a));
    state.status = None;

    state.pending_opens.remove(&key("p"));
    let renamed = send(
        &mut state,
        &mut app,
        session_moved("a", "p"),
        at + Duration::from_secs(5),
    );

    assert!(
        matches!(renamed.as_slice(), [Cmd::RekeyPty { from, to }] if *from == a && *to == key("p")),
        "{renamed:?}"
    );
    assert!(state.screens.contains_key(&key("p")) && !state.screens.contains_key(&a));
    assert_eq!(state.status, None, "the retry that succeeds says nothing");
}

#[test]
fn a_refused_member_move_is_renamed_and_written_when_the_other_pane_closes() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let w = host(&mut state, 1, "worker-1", "w1");
    let other = host_solo(&mut state, "other");
    let at = t0();

    let refused = send(&mut state, &mut app, member_moved("other", false), at);
    assert!(refused.is_empty(), "no rename, no store write: {refused:?}");
    state.status = None;

    state.screens.remove(&other);
    let applied = send(
        &mut state,
        &mut app,
        member_moved("other", false),
        at + Duration::from_secs(5),
    );

    assert!(
        matches!(
            applied.as_slice(),
            [
                Cmd::RekeyPty { from, to },
                Cmd::Store(StoreIntent::SetMemberSession { session_id, .. }),
            ] if *from == w && *to == key("other") && session_id == "other"
        ),
        "{applied:?}"
    );
    assert_eq!(state.status, None);
}

#[test]
fn a_member_move_is_renamed_when_the_row_that_held_it_lets_go() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let w = host(&mut state, 1, "worker-1", "w1");
    let at = t0();

    let refused = send(&mut state, &mut app, member_moved("held", true), at);
    assert!(refused.is_empty(), "{refused:?}");
    assert!(state.screens.contains_key(&w));

    let applied = send(
        &mut state,
        &mut app,
        member_moved("held", false),
        at + Duration::from_secs(5),
    );
    assert_eq!(applied.len(), 2, "{applied:?}");
    assert!(state.screens.contains_key(&key("held")));
}

#[test]
fn a_member_whose_pane_is_gone_is_not_written_onto_a_session_something_holds() {
    // The store-only branch: no pane under the old key, only the row to move.
    // `SetMemberSession` would take the id from whoever holds it.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let at = t0();

    let by_row = send(&mut state, &mut app, member_moved("held", true), at);
    assert!(by_row.is_empty(), "a row holds it: {by_row:?}");

    let other = host_solo(&mut state, "other");
    let by_pane = send(&mut state, &mut app, member_moved("other", false), at);
    assert!(by_pane.is_empty(), "a pane holds it: {by_pane:?}");
    state.screens.remove(&other);

    state.pending_opens.insert(key("p"), PendingOpen::Solo);
    let by_open = send(&mut state, &mut app, member_moved("p", false), at);
    assert!(by_open.is_empty(), "an open holds it: {by_open:?}");

    let free = send(&mut state, &mut app, member_moved("free", false), at);
    assert!(
        matches!(
            free.as_slice(),
            [Cmd::Store(StoreIntent::SetMemberSession { session_id, .. })] if session_id == "free"
        ),
        "{free:?}"
    );
}

#[test]
fn a_repeated_member_move_after_the_rename_still_writes_the_row() {
    // The rename took the id for this member's own pane; the same fact arriving
    // again (before the row follows) must not read that pane as another holder.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    host(&mut state, 1, "worker-1", "w1");
    let at = t0();
    send(&mut state, &mut app, member_moved("w1-new", false), at);

    let again = send(&mut state, &mut app, member_moved("w1-new", false), at);

    assert!(
        matches!(
            again.as_slice(),
            [Cmd::Store(StoreIntent::SetMemberSession { session_id, .. })] if session_id == "w1-new"
        ),
        "{again:?}"
    );
}

#[test]
fn a_refusal_latch_goes_with_its_pane_at_the_next_tick() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let a = host_solo(&mut state, "a");
    host_solo(&mut state, "b");
    let at = t0();
    send(&mut state, &mut app, session_moved("a", "b"), at);
    assert_eq!(state.refused_session_moves.len(), 1);

    // The pane exits; no further refusal comes along to prune the latch.
    state.screens.remove(&a);
    tick(&mut state, &mut app, at + Duration::from_secs(1));

    assert!(state.refused_session_moves.is_empty());
}

#[test]
fn a_session_move_never_renames_a_brigade_members_pane() {
    // A member is followed by `MemberSessionForked`, which also writes the store;
    // the solo event must not rename it behind the roster's back.
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let w = host(&mut state, 1, "worker-1", "w1");

    let cmds = send(&mut state, &mut app, session_moved("w1", "w1-new"), t0());

    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(state.screens.contains_key(&w));
}

// --- a brigade being closed makes no news --------------------------------------
//
// Each of these is reachable only in the moments between the operator's confirm
// and the last pane's exit, and each runs the scenario both ways: the control
// without a close is what proves the test could see the news it says is absent.

#[test]
fn a_brigade_that_goes_quiet_while_being_closed_rings_no_bell_and_marks_nothing() {
    for closing in [false, true] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let idle_from = busy_then_idle(&mut state, &mut app, &[&worker, &director], at);
        if closing {
            state.closing.insert(1, idle_from);
        }
        let cmds = sample(
            &mut state,
            &mut app,
            &[(&director, Idle), (&worker, Idle)],
            idle_from + BRIGADE_QUIET_SETTLE,
        );
        assert_eq!(rang(&cmds), !closing, "closing {closing}: {cmds:?}");
        assert_eq!(
            state.attention_panes().contains(&director),
            !closing,
            "closing {closing}"
        );
    }
}

#[test]
fn an_off_stage_worker_that_waits_while_its_brigade_is_closed_marks_no_row_and_rings_nothing() {
    for closing in [false, true] {
        let (mut state, mut app, _director, _worker, at) = quiet_fixture();
        let mut rows = vec![row_for("dir"), row_for("w1")];
        rows[1].activity = crate::model::Activity::Waiting;
        app.replace_rows(rows);
        app.set_hidden_member_ids(["w1".to_string()].into_iter().collect());
        app.set_viewport_height(20);
        state.stage = Stage::Empty;
        if closing {
            state.closing.insert(1, at);
        }
        let cmds = tick(&mut state, &mut app, at + SECOND);
        assert_eq!(rings(&cmds), usize::from(!closing), "closing {closing}");
        assert_eq!(app.has_brigade_mark("dir"), !closing, "closing {closing}");
    }
}

#[test]
fn the_relay_types_nothing_into_a_pane_of_a_brigade_being_closed() {
    for closing in [false, true] {
        let (mut state, mut app, director, worker, at) = quiet_fixture();
        let start = at + 20 * SECOND;
        if closing {
            state.closing.insert(1, start);
        }
        let observation = || RelayObservation {
            live: None,
            glance: None,
            brigade_id: 1,
            token: "worker-1".to_string(),
            key: worker.clone(),
            has_unseen: true,
            is_idle_this_tick: Some(true),
            agent: None,
            briefed_since_launch: None,
        };
        let idle = [(&director, Idle), (&worker, Idle)];
        let mut written = Vec::new();
        // Inside the close's own limit, which would end the closing and with it
        // the guard under test.
        for secs in 20..28 {
            let cmds = sample_with_relay(
                &mut state,
                &mut app,
                &idle,
                vec![observation()],
                at + secs * SECOND,
            );
            written.extend(
                cmds.into_iter()
                    .filter(|cmd| matches!(cmd, Cmd::WritePty { .. })),
            );
        }
        assert_eq!(
            written.is_empty(),
            closing,
            "closing {closing}: {written:?}"
        );
    }
}

#[test]
fn a_request_that_lands_while_its_brigade_is_being_closed_is_kept_and_makes_no_noise() {
    for closing in [false, true] {
        let (mut state, mut app, director, _worker, at) = request_fixture();
        tick(&mut state, &mut app, at + SECOND);
        if closing {
            state.closing.insert(1, at + SECOND);
        }
        store_request(1);
        let cmds = tick(&mut state, &mut app, at + 2 * SECOND);
        assert_eq!(rings(&cmds), usize::from(!closing), "closing {closing}");
        assert_eq!(
            state.attention_panes().contains(&director),
            !closing,
            "closing {closing}"
        );
        assert!(state.has_request(&director), "recorded either way");
        assert_eq!(
            state.request_spent.contains(&1),
            !closing,
            "closing {closing}: nothing is spent for a brigade on its way out"
        );
    }
}

// --- the quit question's "input" reason with nothing typed by the operator ---------------
//
// From an operator report: the question named a pane "(input)" with nothing typed,
// the warning went away by itself, and the same quit key a moment later quit at
// once. banto's own writes are now named "(nudged)"; the operator's are "(input)".

#[test]
fn an_input_only_question_goes_when_the_stamp_is_three_seconds_old_not_when_its_own_window_is() {
    // What makes the warning "disappear" and the retry quit: the reason is a
    // stamp's age, not the question's age. The stamp is 3 s old at `at + 3 s`;
    // the question was armed at `at + 1 s`, so its own window runs to `at + 4 s`.
    let (mut state, mut app, _a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    back_to_the_list(&mut state, &mut app, at);
    press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
    assert!(state.displayed_status().is_some());

    tick(
        &mut state,
        &mut app,
        at + QUIT_RECENT_INPUT_WINDOW - Duration::from_millis(1),
    );
    assert!(state.displayed_status().is_some(), "still inside");
    tick(&mut state, &mut app, at + QUIT_RECENT_INPUT_WINDOW);
    assert_eq!(
        state.displayed_status(),
        None,
        "gone with its own window (until at + 4 s) still running"
    );

    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        at + QUIT_RECENT_INPUT_WINDOW,
    );
    assert!(app.should_quit(), "the retry counts nothing and quits");
}

#[test]
fn keys_that_reach_a_child_as_nothing_still_stamp_the_operators_input() {
    // `F1` and `F5`..`F12` encode to no bytes: the child is told nothing, yet
    // the stamp the quit question reads is set (see `took_input_recently`).
    for code in [KeyCode::F(1), KeyCode::F(5), KeyCode::F(12)] {
        let (mut state, mut app, a, _b, at) = typing_fixture();
        let cmds = press(&mut state, &mut app, code, at);
        assert!(!typed_into(&cmds, &a), "{code:?}: wrote {cmds:?}");
        assert!(state.last_given_input.contains_key(&a), "{code:?}");
        back_to_the_list(&mut state, &mut app, at);
        press(&mut state, &mut app, KeyCode::Esc, at + SECOND);
        assert_eq!(
            state.displayed_status(),
            Some("Esc again to quit \u{2014} 1 not idle: a (input)"),
            "{code:?}"
        );
    }
}

#[test]
fn leaving_the_pane_does_not_stamp_input_by_any_route_the_operator_has() {
    // Candidates that were read in the code and do not stamp: F2, the prefix
    // chord with `s` / Esc / `o` / a number, and the terminal's own focus
    // reports. (A mouse click is covered by
    // `the_relay_and_the_quit_question_read_two_different_records_of_pointer_traffic`.)
    let (mut state, mut app, a, _b, at) = typing_fixture();
    let ctrl_b = |state: &mut EmporiumState, app: &mut App| {
        press_with(state, app, KeyCode::Char('b'), Modifiers::CONTROL, at)
    };

    back_to_the_list(&mut state, &mut app, at);
    state.focus = Focus::Pane;
    for follow in [KeyCode::Char('s'), KeyCode::Esc, KeyCode::Char('o')] {
        ctrl_b(&mut state, &mut app);
        let cmds = press(&mut state, &mut app, follow, at);
        assert!(!typed_into(&cmds, &a), "{follow:?}: {cmds:?}");
        state.focus = Focus::Pane;
    }
    for focused in [false, true] {
        send(
            &mut state,
            &mut app,
            Event::WindowFocusChanged { focused },
            at,
        );
    }
    assert!(!state.last_given_input.contains_key(&a));
    assert!(!state.last_prompted.contains_key(&a));
    assert!(!state.took_input_recently(&a, at));
}

#[test]
fn a_nudge_the_operator_never_typed_is_named_nudged_for_the_window_after_its_delayed_enter() {
    // The only record of banto's own write is `last_prompted`: it is set at the
    // nudge and again at the delayed Enter 300 ms later, so "(nudged)" is shown up
    // to 3.3 s after the nudge, for a pane the operator never touched.
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let mail = || vec![mail_for(1, DIRECTOR_TOKEN, &director)];
    sample_with_relay(&mut state, &mut app, &[], mail(), at);
    let nudged_at = at + SECOND;
    let cmds = sample_with_relay(&mut state, &mut app, &[], mail(), nudged_at);
    assert!(typed_into(&cmds, &director));
    tick(&mut state, &mut app, nudged_at + RELAY_SUBMIT_DELAY);
    assert!(!state.last_given_input.contains_key(&director));

    let late = nudged_at + RELAY_SUBMIT_DELAY + QUIT_RECENT_INPUT_WINDOW;
    // Both panes read idle, so the only reason left to ask is the stamp.
    let idle = [(&director, Idle), (&worker, Idle)];
    sample(&mut state, &mut app, &idle, late - Duration::from_millis(1));
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        late - Duration::from_millis(1),
    );
    assert!(!app.should_quit());
    let asked = state.displayed_status().expect("it asked").to_string();
    assert!(asked.contains("(nudged)"), "{asked}");
    assert!(!asked.contains("(input)"), "{asked}");
    assert!(
        late - nudged_at > QUIT_RECENT_INPUT_WINDOW,
        "asked more than three seconds after the nudge itself"
    );
}

#[test]
fn the_pane_log_facts_cannot_see_banto_own_writes_though_the_quit_question_counts_them() {
    // `input_ago_ms` of a pane-log line comes from `last_given_input` alone. A
    // pane counted for banto's own write has `input_ago_ms=none` there, which is
    // how a log tells that the "(nudged)" was not the operator's.
    let (mut state, app, a, _b, at) = typing_fixture();
    state.last_prompted.insert(a.clone(), at);
    assert!(state.took_input_recently(&a, at + SECOND));
    assert_eq!(state.pane_facts(&app, &a, at + SECOND).input_ago_ms, None);

    state.last_given_input.insert(a.clone(), at);
    assert_eq!(
        state.pane_facts(&app, &a, at + SECOND).input_ago_ms,
        Some(1000)
    );
}
