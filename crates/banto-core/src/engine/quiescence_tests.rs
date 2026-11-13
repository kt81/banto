//! Whether hosted sessions are known to be idle ([`Event::QuiescenceSampled`])
//! as the operator meets it: a quit that asks only when something might be
//! lost, and a bell when a brigade has gone quiet.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use crate::model::Quiescence::{Idle, NotIdle, Unknown};
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
        })
        .collect()
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
                glance: None,
                brigade_id,
                token: state.member_tokens.get(pane)?.clone(),
                key: pane.clone(),
                has_unseen: false,
                is_idle_this_tick: Some(true),
                agent: None,
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
    cmds.iter().any(|cmd| matches!(cmd, Cmd::RingHostBell))
}

fn key(id: &str) -> SessionKey {
    SessionKey::from_id(id)
}

/// A hosted pane of `brigade` under `token`, with a screen, whose process is
/// taken to have run (and idled) before the test begins.
fn host(state: &mut EmporiumState, brigade: BrigadeId, token: &str, id: &str) -> SessionKey {
    let pane = host_fresh(state, brigade, token, id);
    let process = state.spawn_order[&pane];
    state.seen_idle.insert(process);
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

/// Show `brigade` on stage with its Director focused.
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
        Some("Esc again to quit \u{2014} 2 session(s) not idle")
    );
}

#[test]
fn before_any_sample_every_hosted_session_asks() {
    let (mut state, mut app, _a, _b, at) = quit_fixture();
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(!app.should_quit());
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 2 session(s) not idle")
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
        Some("Esc again to quit \u{2014} 2 session(s) not idle")
    );

    sample(
        &mut state,
        &mut app,
        &[(&a, Idle), (&b, NotIdle)],
        at + Duration::from_secs(1),
    );
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 session(s) not idle"),
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
        Event::Spawned { key: fresh.clone() },
        at,
    );
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert_eq!(
        state.displayed_status(),
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
    );
}

#[test]
fn a_pane_whose_key_changes_keeps_its_sample() {
    let (mut state, mut app, a, b, at) = quit_fixture();
    sample(&mut state, &mut app, &[(&a, Idle), (&b, Idle)], at);
    state.rekey_pane_state(&a, &key("a-after-discovery"));
    assert_eq!(state.not_idle_count(at), 0);
    press(&mut state, &mut app, KeyCode::Esc, at);
    assert!(app.should_quit());
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
    assert!(state.latched_attention_panes.contains(&d1));
    assert!(
        !state.latched_attention_panes.contains(&d2),
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
    assert!(!state.latched_attention_panes.contains(&d2));
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)],
        idle_2 + BRIGADE_QUIET_SETTLE,
    );
    assert!(state.latched_attention_panes.contains(&d2));
}

#[test]
fn an_off_stage_brigade_latches_its_mark_but_shows_it_only_once_it_is_staged() {
    let (mut state, mut app, d1, w1, at) = quiet_fixture();
    let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
    let w2 = host(&mut state, 2, "worker-1", "w2");

    // Brigade 2 is not the one on stage, and it goes quiet.
    sample(
        &mut state,
        &mut app,
        &[(&d1, Idle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)],
        at,
    );
    let idle_from = at + SECOND;
    let all = [(&d1, Idle), (&w1, Idle), (&d2, Idle), (&w2, Idle)];
    sample(&mut state, &mut app, &all, idle_from);
    let cmds = sample(&mut state, &mut app, &all, idle_from + BRIGADE_QUIET_SETTLE);
    assert!(state.latched_attention_panes.contains(&d2));
    assert!(
        !state.attention_panes().contains(&d2),
        "no tile is drawn for it"
    );
    assert!(!rang(&cmds), "no bell for a mark nobody can see: {cmds:?}");

    // Staging it shows the mark, and the bell follows then.
    stage(&mut state, 2, &d2, &[&w2]);
    let cmds = tick(
        &mut state,
        &mut app,
        idle_from + BRIGADE_QUIET_SETTLE + SECOND,
    );
    assert!(state.attention_panes().contains(&d2));
    assert!(rang(&cmds), "{cmds:?}");
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
    assert!(state.latched_attention_panes.is_empty());
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
    assert!(
        state.attention_panes().is_empty(),
        "a focused pane carries no mark"
    );
    assert!(state.latched_attention_panes.is_empty());
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
                .filter(|cmd| matches!(cmd, Cmd::RingHostBell))
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
    );
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
        glance: None,
        brigade_id: 1,
        token: "worker-1".to_string(),
        key: worker.clone(),
        has_unseen: true,
        is_idle_this_tick: Some(true),
        agent: None,
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
    let (mut state, mut app, a, _b, at) = typing_fixture();
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    state.last_prompted.insert(a.clone(), at);
    let renamed = key("a-after-discovery");
    state.rekey_pane_state(&a, &renamed);
    assert!(state.took_input_recently(&renamed, at));
    assert!(!state.took_input_recently(&a, at));

    state.unstage(&renamed);
    assert!(!state.last_prompted.contains_key(&renamed));
    assert!(!state.last_forwarded_input.contains_key(&renamed));
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
    use super::tests::{draw_prompt_on, goinkyo_kickoff_retry_state, goinkyo_trust_answer};
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
    let cmds = goinkyo_trust_answer(&mut state, &mut app, &brigade, &pane, enter_at);
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
    );

    // After the window the sample alone decides again.
    let (mut state, mut app, brigade, pane, t0) = setup();
    state.focus = Focus::Sidebar;
    let enter_at = t0 + KICKOFF_ACK_TIMEOUT;
    goinkyo_trust_answer(&mut state, &mut app, &brigade, &pane, enter_at);
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
        glance: None,
        brigade_id: brigade,
        token: token.to_string(),
        key: member.clone(),
        has_unseen: true,
        is_idle_this_tick: Some(true),
        agent: None,
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
        state.latched_attention_panes.contains(&d2),
        "brigade 2 went quiet on time"
    );
    assert!(
        !state.latched_attention_panes.contains(&d1),
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
        cwd: PathBuf::from("/work"),
        notified_untrusted: false,
        sent_at: None,
        attempts: 0,
    });
    app.replace_rows(Vec::new());
    (state, app, director, worker, goinkyo, at)
}

fn trust_answer(state: &mut EmporiumState, app: &mut App, pane: &SessionKey, at: Instant) {
    let cmds = send(
        state,
        app,
        Event::GoinkyoDirectoryTrustChecked {
            key: pane.clone(),
            trusted: true,
        },
        at,
    );
    assert!(typed_into(&cmds, pane), "the kickoff was typed: {cmds:?}");
}

#[test]
fn a_goinkyo_kickoff_check_going_out_on_the_tick_that_would_complete_the_stretch_does_not_ring() {
    // The Goinkyo's boot output has been quiet for exactly long enough that its
    // trust check becomes due on the tick the all-idle stretch would complete.
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
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::CheckGoinkyoDirectoryTrust { .. })),
        "the check went out on this tick: {cmds:?}"
    );
    assert!(!rang(&cmds), "rang first, typed into afterwards: {cmds:?}");

    // The shell's answer arrives after the tick: the kickoff is typed, the
    // Goinkyo's first turn shows up as a session row, and only then does the
    // all-idle stretch run again.
    trust_answer(&mut state, &mut app, &goinkyo, deadline);
    let acknowledged = deadline + 2 * SECOND;
    let rang_at = tick_each_second_until(
        &mut state,
        &mut app,
        &all_idle,
        deadline + SECOND,
        acknowledged - SECOND,
    );
    assert_eq!(rang_at, None, "still waiting for its acknowledgement");
    app.replace_rows(vec![row_for("gk")]);
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
fn a_codex_worker_kickoff_check_going_out_on_the_tick_that_would_complete_the_stretch_does_not_ring()
 {
    let (mut state, mut app, director, worker, at) = quiet_fixture();
    let deadline = at + SECOND + BRIGADE_QUIET_SETTLE;
    let codex = host(&mut state, 1, "worker-2", "w2");
    state.pending_kickoffs.push(PendingKickoff {
        key: codex.clone(),
        spawned_at: deadline - CODEX_KICKOFF_QUIET_PERIOD,
        cwd: PathBuf::from("/work"),
        notified_untrusted: false,
        sent_at: None,
        attempts: 0,
    });
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
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::CheckWorkerDirectoryTrust { .. })),
        "the check went out on this tick: {cmds:?}"
    );
    assert!(!rang(&cmds), "{cmds:?}");

    let cmds = send(
        &mut state,
        &mut app,
        Event::WorkerDirectoryTrustChecked {
            key: codex.clone(),
            trusted: true,
        },
        deadline,
    );
    assert!(typed_into(&cmds, &codex), "{cmds:?}");
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
    // Quiet at last: the check goes out, the kickoff is typed, then acknowledged.
    let checked = at + 9 * SECOND;
    let cmds = sample(&mut state, &mut app, &all_idle, checked);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::CheckGoinkyoDirectoryTrust { .. })),
        "{cmds:?}"
    );
    assert!(!rang(&cmds));
    trust_answer(&mut state, &mut app, &goinkyo, checked);
    app.replace_rows(vec![row_for("gk")]);
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
    // The directory was reported untrusted: nothing is typed until the operator
    // answers the prompt in the pane, and a bell is what tells them.
    let (mut state, mut app, director, worker, goinkyo, at) = goinkyo_fixture(Duration::ZERO);
    state.pending_goinkyo_kickoffs[0].notified_untrusted = true;
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

    // The same when the operator has a draft in the pane: the retry waits for
    // them, not the other way round.
    let (mut state, mut app, director, worker, goinkyo, at) = goinkyo_fixture(Duration::ZERO);
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
    // Discovery gives the same process its real id; its next work arms.
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
        at + 30 * SECOND,
    );
    assert!(rang_at.is_some());
}

#[test]
fn input_written_before_a_process_started_is_not_work_it_was_given() {
    let (mut state, mut app, members, at) = opened_brigade();
    let [director, codex, claude_a, claude_b] = members.clone();
    // The operator typed into the Worker's previous process ...
    state.last_forwarded_input.insert(claude_a.clone(), at);
    // ... which was then replaced; the new process started a second later.
    unhost(&mut state, &claude_a);
    let claude_a = host_fresh(&mut state, 1, "worker-2", "ca");
    state.process_started.insert(claude_a.clone(), at + SECOND);
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
        None,
        "the new process's start-up does not arm"
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
    assert!(state.latched_attention_panes.contains(&d1));
    assert!(
        !state.latched_attention_panes.contains(&d2),
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
                .any(|cmd| matches!(cmd, Cmd::KillPty { key } if key == *other)),
            "{other:?} is killed: {cmds:?}"
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
    assert!(state.latched_attention_panes.is_empty());
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
    assert_eq!(state.not_idle_count(idle_from + 60 * SECOND), 0);
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle)],
        idle_from + 61 * SECOND,
    );
    assert_eq!(state.not_idle_count(idle_from + 61 * SECOND), 1);
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
        cwd: PathBuf::from("/work"),
        notified_untrusted: false,
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
        Event::Spawned { key: added.clone() },
        idle_from + 3 * SECOND,
    );
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
