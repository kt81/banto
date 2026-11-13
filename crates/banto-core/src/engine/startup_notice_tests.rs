//! What the shell found wrong at startup ([`Event::StartupNotices`]) as the
//! operator sees it: status messages, one after another, never one over
//! another unseen, and never in the way of input.

use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{InputEvent, KeyCode, KeyEvent, Modifiers};
use crate::model::{Activity, AgentKind, SessionRow};

use super::*;

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
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

fn notices(texts: &[&str]) -> Event {
    Event::StartupNotices {
        notices: texts.iter().map(|text| text.to_string()).collect(),
    }
}

fn fresh() -> (EmporiumState, App) {
    (
        EmporiumState::new(PrefixKey::default()),
        App::new(Vec::new()),
    )
}

#[test]
fn nothing_is_shown_when_there_is_nothing_to_say() {
    let (mut state, mut app) = fresh();
    let t = now();
    send(&mut state, &mut app, notices(&[]), t);
    assert_eq!(state.displayed_status(), None);
    tick(&mut state, &mut app, t + Duration::from_secs(10));
    assert_eq!(state.displayed_status(), None);
}

#[test]
fn a_notice_is_shown_at_once_like_any_status_message() {
    let (mut state, mut app) = fresh();
    send(
        &mut state,
        &mut app,
        notices(&["codex: approve the hook"]),
        now(),
    );
    assert_eq!(state.displayed_status(), Some("codex: approve the hook"));
}

#[test]
fn two_notices_are_each_shown_in_full_the_second_when_the_first_has_had_its_time() {
    let (mut state, mut app) = fresh();
    let t = now();
    send(&mut state, &mut app, notices(&["first", "second"]), t);
    assert_eq!(state.displayed_status(), Some("first"));
    // Still the first while it has time left, however often the tick runs.
    tick(
        &mut state,
        &mut app,
        t + STATUS_TIMEOUT - Duration::from_millis(1),
    );
    assert_eq!(state.displayed_status(), Some("first"));
    // The tick that clears the first puts the second up on the same pass.
    tick(&mut state, &mut app, t + STATUS_TIMEOUT);
    assert_eq!(state.displayed_status(), Some("second"));
    // And the second gets its full time, counted from when it went up.
    tick(
        &mut state,
        &mut app,
        t + 2 * STATUS_TIMEOUT - Duration::from_millis(1),
    );
    assert_eq!(state.displayed_status(), Some("second"));
    tick(&mut state, &mut app, t + 2 * STATUS_TIMEOUT);
    assert_eq!(state.displayed_status(), None);
}

#[test]
fn a_notice_waits_for_a_status_line_that_is_in_use_instead_of_replacing_it() {
    let (mut state, mut app) = fresh();
    let t = now();
    state.set_status("something else is showing", t);
    send(&mut state, &mut app, notices(&["the notice"]), t);
    assert_eq!(state.displayed_status(), Some("something else is showing"));
    tick(&mut state, &mut app, t + STATUS_TIMEOUT);
    assert_eq!(state.displayed_status(), Some("the notice"));
}

#[test]
fn notices_arriving_in_two_events_queue_behind_each_other() {
    let (mut state, mut app) = fresh();
    let t = now();
    send(&mut state, &mut app, notices(&["a"]), t);
    send(&mut state, &mut app, notices(&["b"]), t);
    assert_eq!(state.displayed_status(), Some("a"));
    tick(&mut state, &mut app, t + STATUS_TIMEOUT);
    assert_eq!(state.displayed_status(), Some("b"));
}

#[test]
fn a_notice_on_screen_blocks_no_input_and_asks_for_no_effects() {
    let rows: Vec<SessionRow> = ["a", "b", "c"]
        .iter()
        .map(|id| SessionRow {
            id: id.to_string(),
            agent: AgentKind::ClaudeCode,
            title: Some(id.to_string()),
            cwd: None,
            activity: Activity::Alive,
            is_agent: false,
            preview: None,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
            archived: false,
        })
        .collect();
    let mut app = App::new(rows);
    let mut state = EmporiumState::new(PrefixKey::default());
    let t = now();
    let cmds = send(
        &mut state,
        &mut app,
        notices(&["codex: approve the hook"]),
        t,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    let before = app.selected_row().map(|row| row.id.clone());
    send(
        &mut state,
        &mut app,
        Event::Input(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('j'),
            Modifiers::NONE,
        ))),
        t + Duration::from_millis(10),
    );
    assert_ne!(
        app.selected_row().map(|row| row.id.clone()),
        before,
        "`j` must still move the selection while a notice is up"
    );
}

#[test]
fn the_event_survives_a_recording_round_trip() {
    let event = notices(&["one", "two"]);
    let text = serde_json::to_string(&event).unwrap();
    assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), event);
}
