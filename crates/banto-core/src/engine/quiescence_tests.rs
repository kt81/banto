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
                live: None,
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
    assert_eq!(state.not_idle_count(&app, at), 0);
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
}

struct OffStage {
    state: EmporiumState,
    app: App,
    d2: SessionKey,
    w2: SessionKey,
    target: SessionKey,
    at: Instant,
    /// Bells rung while the brigade was being set up off stage.
    rings_before: usize,
}

fn rings(cmds: &[Cmd]) -> usize {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::RingHostBell))
        .count()
}

impl OffStage {
    /// Brigade 1 on stage with the list focused, brigade 2 hosted off stage with
    /// only its Director's row in the list (its Worker is hidden, as members are),
    /// and `says` about `target`: the Director when `on_director`, else the Worker.
    fn new(says: OffStageSays, on_director: bool) -> Self {
        use crate::model::Activity;
        let (mut state, mut app, d1, w1, at) = quiet_fixture();
        let d2 = host(&mut state, 2, DIRECTOR_TOKEN, "dir2");
        let w2 = host(&mut state, 2, "worker-1", "w2");
        let target = if on_director { d2.clone() } else { w2.clone() };
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
        match says {
            OffStageSays::QuietFire => {
                let working = [(&d1, Idle), (&w1, Idle), (&d2, NotIdle), (&w2, Idle)];
                rings_before += rings(&sample(&mut state, &mut app, &working, at));
                let idle_from = at + SECOND;
                rings_before += rings(&sample(&mut state, &mut app, &all_idle, idle_from));
                let settled = idle_from + BRIGADE_QUIET_SETTLE;
                rings_before += rings(&sample(&mut state, &mut app, &all_idle, settled));
            }
            OffStageSays::Bell => {
                rings_before += rings(&said(&mut state, &mut app, &target, b"\x07", at + SECOND));
            }
            OffStageSays::ClaudeWaiting => {
                rings_before += rings(&sample(&mut state, &mut app, &all_idle, at + SECOND));
            }
            OffStageSays::CodexBlocked => {
                rings_before += rings(&sample(&mut state, &mut app, &all_idle, at + SECOND));
                rings_before += rings(&said(
                    &mut state,
                    &mut app,
                    &target,
                    REPORT_BLOCKED,
                    at + 2 * SECOND,
                ));
            }
        }
        Self {
            state,
            app,
            d2,
            w2,
            target,
            at,
            rings_before,
        }
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

    fn latched(&self) -> bool {
        self.state.latched_attention_panes.contains(&self.target)
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

/// What staging an off-stage brigade does to a latch on its Director: with the
/// window focused (F4 off or on) the Director's pane is the focused one, so the
/// latch is cleared in the very update that stages the brigade — nothing is drawn
/// in any frame and the bell never rings; with the window unfocused it is not
/// effectively focused, so the mark is drawn and the bell rings once, and the
/// mark goes silently when the window regains focus.
fn assert_a_director_latch_is_lost_at_staging(says: OffStageSays) {
    for f4 in [false, true] {
        let mut off_stage = OffStage::new(says, true);
        assert!(off_stage.latched(), "{says:?}: latched off stage");
        assert!(off_stage.marks().is_empty(), "no tile is drawn for it");
        assert_eq!(
            off_stage.rings_before, 0,
            "no bell for a mark nobody can see"
        );

        let staged = off_stage.stage_by_enter(true, f4);
        off_stage.assert_focus_landed_on_the_director();
        assert!(off_stage.marks().is_empty(), "{says:?} f4 {f4}: no mark");
        assert_eq!(rings(&staged), 0, "{says:?} f4 {f4}: no bell");
        assert!(!off_stage.latched(), "{says:?} f4 {f4}: cleared by staging");
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
        assert_eq!(rings(&staged), 1);
        assert!(off_stage.latched());
        assert_eq!(rings(&off_stage.tick_after_staging(1)), 0, "once");
        let regained = off_stage.window_regains_focus(2);
        assert_eq!(rings(&regained), 0, "it goes quietly");
        assert!(off_stage.marks().is_empty());
        assert!(!off_stage.latched());
    }
}

// KNOWN DEFECT, pinned: an off-stage brigade's quiet fire does not reach the
// operator. The fire latches the Director's mark, no tile is drawn for it off
// stage, and staging the brigade clears the latch in the same update because
// staging focuses the Director's pane (docs/REQUIREMENTS.md, "The quiet
// brigade's bell"). This test says what the product does today, so that the
// change which gives off-stage brigades a surface has a test to turn around.
#[test]
fn an_off_stage_brigades_quiet_fire_is_lost_when_it_is_staged_unless_the_window_is_unfocused() {
    assert_a_director_latch_is_lost_at_staging(OffStageSays::QuietFire);
}

#[test]
fn a_bell_latch_on_an_off_stage_director_is_lost_at_staging_like_the_quiet_fire() {
    assert_a_director_latch_is_lost_at_staging(OffStageSays::Bell);
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
            assert_eq!(off_stage.rings_before, 0);

            let staged = off_stage.stage_by_enter(window_focused, f4);
            off_stage.assert_focus_landed_on_the_director();
            assert_eq!(off_stage.marks(), ["w2"], "{what}");
            assert_eq!(rings(&staged), 1, "{what}");
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
                // A hidden member is not in the list, and nothing draws it.
                assert!(off_stage.marks().is_empty(), "{what}");
                assert_eq!(off_stage.rings_before, 0, "{what}");

                let staged = off_stage.stage_by_enter(window_focused, f4);
                off_stage.assert_focus_landed_on_the_director();
                assert_eq!(off_stage.marks(), ["w2"], "{what}");
                assert_eq!(off_stage.state.visible_waiting_count(), 1, "{what}");
                assert_eq!(rings(&staged), 1, "{what}");
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
        live: None,
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
        live: None,
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
    assert_eq!(state.not_idle_count(&app, idle_from + 60 * SECOND), 0);
    sample(
        &mut state,
        &mut app,
        &[(&director, NotIdle)],
        idle_from + 61 * SECOND,
    );
    assert_eq!(state.not_idle_count(&app, idle_from + 61 * SECOND), 1);
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

    // Rekeyed (discovery, a compaction fork): the report stays with the pane.
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
            .filter(|c| matches!(c, Cmd::RingHostBell))
            .count(),
        1
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
                .filter(|c| matches!(c, Cmd::RingHostBell))
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
        superseded: HashSet::new(),
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
        Some("Esc again to quit \u{2014} 1 session(s) not idle")
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
    assert_eq!(state.not_idle_count(&app, conflict_at), 0);
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
    assert_eq!(state.not_idle_count(&app, t0()), 0);
}
