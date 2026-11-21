//! How a pane that banto ends leaves: hung up first, killed only if it is still
//! there when `PANE_END_GRACE` runs out. Driven through real `Event`s, as
//! `close_brigade_tests` is.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::model::{Activity, AgentKind, PtyExitReason, SessionRow};
use crate::screen::Screen;

use super::*;

fn row(id: &str) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent: AgentKind::ClaudeCode,
        title: Some(id.to_string()),
        cwd: Some(PathBuf::from("/work/alpha")),
        dialog_open: false,
        activity: Activity::Alive,
        is_agent: false,
        preview: None,
        mtime: std::time::SystemTime::UNIX_EPOCH,
        size: 0,
        source_archived: false,
        archived: false,
    }
}

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

fn key(id: &str) -> SessionKey {
    SessionKey::from_id(id)
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, at)
}

fn tick(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        },
        at,
    )
}

fn press(state: &mut EmporiumState, app: &mut App, code: KeyCode, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Input(InputEvent::Key(KeyEvent::new(code, Modifiers::NONE))),
        at,
    )
}

fn only(cmds: &[Cmd], wanted: fn(&Cmd) -> Option<&SessionKey>) -> Vec<SessionKey> {
    cmds.iter().filter_map(wanted).cloned().collect()
}

fn hang_ups(cmds: &[Cmd]) -> Vec<SessionKey> {
    only(cmds, |cmd| match cmd {
        Cmd::HangUpPty { key } => Some(key),
        _ => None,
    })
}

fn kills(cmds: &[Cmd]) -> Vec<SessionKey> {
    only(cmds, |cmd| match cmd {
        Cmd::KillPty { key } => Some(key),
        _ => None,
    })
}

/// A single session on stage with its confirmed end asked for at `at`. Returns
/// what that update produced.
fn solo_ended_at(at: Instant) -> (EmporiumState, App, Vec<Cmd>) {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    state.focus = Focus::Pane;
    let mut app = App::new(vec![row("solo")]);
    state.prefix_armed = Some(at);
    press(&mut state, &mut app, KeyCode::Char('x'), at);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, at);
    (state, app, cmds)
}

/// Brigade 1: the Director `dir` and Workers `w1`, `w2`, hosted and staged.
fn staged_brigade() -> (EmporiumState, App) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let panes: Vec<SessionKey> = [
        ("dir", DIRECTOR_TOKEN),
        ("w1", "worker-1"),
        ("w2", "worker-2"),
    ]
    .into_iter()
    .map(|(id, token)| {
        let pane = key(id);
        state.screens.insert(pane.clone(), Screen::new(24, 80));
        state.member_tokens.insert(pane.clone(), token.to_string());
        state.member_brigades.insert(pane.clone(), 1);
        pane
    })
    .collect();
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(key("dir")),
        panes,
        focused: 0,
    };
    state.focus = Focus::Pane;
    (state, App::new(vec![row("dir")]))
}

#[test]
fn an_end_is_a_hang_up_and_not_a_kill() {
    let t = now();
    let (state, _app, cmds) = solo_ended_at(t);
    assert_eq!(hang_ups(&cmds), vec![key("solo")]);
    assert!(kills(&cmds).is_empty(), "{cmds:?}");
    assert!(state.expected_exits.contains_key(&key("solo")));
}

#[test]
fn a_tick_before_the_deadline_does_nothing_and_one_at_it_kills_once() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);

    let early = tick(
        &mut state,
        &mut app,
        t + PANE_END_GRACE - Duration::from_millis(1),
    );
    assert!(
        kills(&early).is_empty() && hang_ups(&early).is_empty(),
        "{early:?}"
    );

    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("solo")], "{due:?}");
    assert!(hang_ups(&due).is_empty());

    let after = tick(&mut state, &mut app, t + PANE_END_GRACE * 3);
    assert!(kills(&after).is_empty(), "killed once only: {after:?}");
    // What banto asked for stays until the exit, so the exit is still its own.
    assert!(state.expected_exits.contains_key(&key("solo")));
}

#[test]
fn the_deadline_is_counted_from_the_event_that_asked_not_from_the_next_tick() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);
    // The first event after the request comes late; the grace still began at `t`.
    let late = tick(&mut state, &mut app, t + Duration::from_secs(3));
    assert!(kills(&late).is_empty());
    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("solo")]);
}

#[test]
fn an_exit_before_the_deadline_is_never_killed_and_drops_the_deadline() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);
    assert!(state.hangups.contains_key(&key("solo")));

    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("solo"),
            reason: Some(PtyExitReason::Code(129)),
        },
        t + Duration::from_millis(800),
    );
    assert!(state.hangups.is_empty());

    let after = tick(&mut state, &mut app, t + PANE_END_GRACE * 2);
    assert!(kills(&after).is_empty(), "{after:?}");
}

#[test]
fn a_second_request_is_no_second_hang_up_and_does_not_move_the_deadline() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);

    let again = kill_pane(&mut state, key("solo"));
    assert!(again.is_none(), "{again:?}");
    // Ending it again later keeps the first deadline.
    tick(&mut state, &mut app, t + Duration::from_secs(4));
    assert!(kill_pane(&mut state, key("solo")).is_none());
    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("solo")]);
}

#[test]
fn a_new_process_under_the_key_is_not_ended_by_the_old_ones_deadline() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);
    state.pending_opens.insert(key("solo"), PendingOpen::Solo);
    send(
        &mut state,
        &mut app,
        Event::Spawned {
            key: key("solo"),
            agent: None,
        },
        t + Duration::from_secs(1),
    );
    let due = tick(&mut state, &mut app, t + PANE_END_GRACE * 2);
    assert!(kills(&due).is_empty(), "{due:?}");
}

#[test]
fn a_pane_whose_output_was_lost_is_hung_up_and_still_killed_at_the_deadline() {
    let t = now();
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    let mut app = App::new(vec![row("solo")]);

    let lost = send(
        &mut state,
        &mut app,
        Event::PtyOutputLost { key: key("solo") },
        t,
    );
    assert_eq!(hang_ups(&lost), vec![key("solo")], "{lost:?}");
    assert!(
        !state.screens.contains_key(&key("solo")),
        "no screen is left to hang the deadline on"
    );

    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("solo")], "{due:?}");
}

#[test]
fn closing_a_brigade_hangs_every_pane_up_then_kills_the_stragglers_only() {
    let t = now();
    let (mut state, mut app) = staged_brigade();
    state.prefix_armed = Some(t);
    press(&mut state, &mut app, KeyCode::Char('x'), t);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    let mut ended = hang_ups(&cmds);
    ended.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(ended, vec![key("dir"), key("w1"), key("w2")], "{cmds:?}");
    assert!(kills(&cmds).is_empty());

    // Two leave within the grace; one does not.
    for pane in ["dir", "w2"] {
        send(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: key(pane),
                reason: Some(PtyExitReason::Code(129)),
            },
            t + Duration::from_millis(800),
        );
    }
    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("w1")], "{due:?}");

    // The force's own exit settles the close well inside the give-up limit.
    send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("w1"),
            reason: Some(PtyExitReason::Code(1)),
        },
        t + PANE_END_GRACE + Duration::from_millis(200),
    );
    assert!(!state.closing.contains_key(&1));
    assert!(
        !state
            .status
            .clone()
            .unwrap_or_default()
            .contains("did not end"),
        "{:?}",
        state.status
    );
}

#[test]
fn a_re_key_carries_the_deadline_to_the_new_key() {
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);
    state.rekey_pane_state(&key("solo"), &key("sess-2"));
    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("sess-2")], "{due:?}");
}

#[test]
fn the_output_ending_because_of_the_hang_up_adds_no_second_request_and_keeps_the_deadline() {
    // Closing the console closes the pane's output as well, and a child slower
    // than the shell's output-loss wait is reported as lost output while it is
    // still shutting down. Its tile is gone then; the process is not.
    let t = now();
    let (mut state, mut app, _) = solo_ended_at(t);

    let lost = send(
        &mut state,
        &mut app,
        Event::PtyOutputLost { key: key("solo") },
        t + Duration::from_millis(1100),
    );
    assert!(
        hang_ups(&lost).is_empty() && kills(&lost).is_empty(),
        "{lost:?}"
    );
    assert!(state.lost_output_waiting_for_exit.contains(&key("solo")));

    let due = tick(&mut state, &mut app, t + PANE_END_GRACE);
    assert_eq!(kills(&due), vec![key("solo")], "{due:?}");
}
