//! The F5 reading screen as the engine drives it: which brigade it opens on, the
//! round trip that fills it, what makes it read again, and what Enter, Esc and
//! the scroll keys do on it. Driven through real `Event`s, as `close_brigade_tests`
//! is.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::model::{Activity, AgentKind, SessionRow};
use crate::progress::{
    BrigadeReading, DecisionReading, Progress, ProgressGlance, ProgressReading, ProgressTask,
    TaskState,
};
use crate::reading::ReadingBody;
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

fn host(state: &mut EmporiumState, id: &str, token: &str, brigade_id: BrigadeId) -> SessionKey {
    let key = key(id);
    state.screens.insert(key.clone(), Screen::new(24, 80));
    state.member_tokens.insert(key.clone(), token.to_string());
    state.member_brigades.insert(key.clone(), brigade_id);
    key
}

fn facts(brigade_id: BrigadeId, director: &str, progress_seq: Option<i64>) -> BrigadeNameFacts {
    BrigadeNameFacts {
        brigade_id,
        name: format!("brigade-{brigade_id}"),
        director_session: Some(director.to_string()),
        member_names: Vec::new(),
        request: None,
        interrupts: None,
        roster: Vec::new(),
        progress: progress_seq.map(|seq| ProgressGlance {
            seq,
            stage: format!("stage {seq}"),
        }),
    }
}

/// Brigade 1 staged with the Director `dir` and one Worker, the focus on the
/// Director's pane; the list has rows for `dir` and for `other`, the Director of
/// brigade 2, which nothing hosts.
fn staged() -> (EmporiumState, App) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let panes: Vec<SessionKey> = [("dir", DIRECTOR_TOKEN), ("w1", "worker-1")]
        .into_iter()
        .map(|(id, token)| host(&mut state, id, token, 1))
        .collect();
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(key("dir")),
        panes,
        focused: 0,
    };
    state.focus = Focus::Pane;
    state.size = (100, 30);
    state.brigade_names.insert(1, facts(1, "dir", None));
    state.brigade_names.insert(2, facts(2, "other", None));
    let mut app = App::new(vec![row("dir"), row("other"), row("plain")]);
    app.set_viewport_height(20);
    (state, app)
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, at)
}

fn press(state: &mut EmporiumState, app: &mut App, code: KeyCode, at: Instant) -> Vec<Cmd> {
    press_with(state, app, code, Modifiers::NONE, at)
}

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

fn read_cmds(cmds: &[Cmd]) -> Vec<BrigadeId> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::Store(StoreIntent::ReadBrigadeReading { brigade_id }) => Some(*brigade_id),
            _ => None,
        })
        .collect()
}

fn wrote_to_a_pane(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. }))
}

fn open_on(app: &App) -> Option<i64> {
    app.reading().map(|reading| reading.brigade_id())
}

fn decision(seq: i64, question: Option<&str>) -> DecisionReading {
    DecisionReading {
        seq,
        question: question.map(str::to_string),
        age_ms: Some(2_000),
    }
}

fn progress_reading(seq: i64, tasks: usize) -> ProgressReading {
    ProgressReading {
        progress: Progress {
            stage: "working".to_string(),
            tasks: (0..tasks)
                .map(|i| ProgressTask {
                    id: format!("t{i}"),
                    state: TaskState::Todo,
                    note: format!("task number {i}"),
                })
                .collect(),
            off_plan: Vec::new(),
        },
        seq,
        age_ms: 5_000,
    }
}

fn loaded(
    state: &mut EmporiumState,
    app: &mut App,
    brigade_id: BrigadeId,
    reading: BrigadeReading,
    at: Instant,
) {
    send(
        state,
        app,
        Event::BrigadeReadingRead {
            brigade_id,
            result: Ok(reading),
        },
        at,
    );
}

fn with_decision(question: Option<&str>) -> BrigadeReading {
    BrigadeReading {
        decision: Some(decision(4, question)),
        progress: Some(progress_reading(2, 1)),
    }
}

// --- opening ------------------------------------------------------------------------

#[test]
fn f5_in_a_pane_opens_the_staged_brigade_asks_the_store_and_reaches_no_pane() {
    let (mut state, mut app) = staged();
    let cmds = press(&mut state, &mut app, KeyCode::F(5), now());
    assert_eq!(open_on(&app), Some(1));
    assert_eq!(read_cmds(&cmds), vec![1]);
    assert!(!wrote_to_a_pane(&cmds), "{cmds:?}");
    assert_eq!(app.reading().unwrap().body(), &ReadingBody::Loading);
    // The name the rest of the UI calls the brigade by: the Director's row title.
    assert_eq!(app.reading().unwrap().name(), state.brigade_name(&app, 1));
}

#[test]
fn f5_works_whatever_modifiers_are_held() {
    for mods in [Modifiers::SHIFT, Modifiers::CONTROL] {
        let (mut state, mut app) = staged();
        let cmds = press_with(&mut state, &mut app, KeyCode::F(5), mods, now());
        assert_eq!(open_on(&app), Some(1), "{mods:?}");
        assert!(!wrote_to_a_pane(&cmds), "{mods:?}");
    }
}

#[test]
fn f5_from_the_list_reads_the_selected_directors_brigade_without_staging_anything() {
    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.select_next();
    assert_eq!(app.selected_row().unwrap().id, "other");
    let stage_before = format!("{:?}", state.stage);
    let cmds = press(&mut state, &mut app, KeyCode::F(5), now());
    assert_eq!(open_on(&app), Some(2));
    assert_eq!(read_cmds(&cmds), vec![2]);
    assert_eq!(format!("{:?}", state.stage), stage_before, "nothing staged");
    assert!(
        !cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::OpenEmbedded { .. } | Cmd::Store(StoreIntent::ResolveMembership { .. })
        )),
        "nobody is resumed: {cmds:?}"
    );
}

#[test]
fn f5_from_the_list_on_a_row_outside_any_brigade_falls_back_to_the_staged_one() {
    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.select_next();
    app.select_next();
    assert_eq!(app.selected_row().unwrap().id, "plain");
    press(&mut state, &mut app, KeyCode::F(5), now());
    assert_eq!(open_on(&app), Some(1));
}

#[test]
fn f5_from_the_list_on_a_hosted_members_row_reads_that_members_brigade() {
    let (mut state, _) = staged();
    state.focus = Focus::Sidebar;
    // `w1` is hosted in brigade 1 and has a row (members are normally hidden, which
    // is the moment before a reload hides one).
    let mut app = App::new(vec![row("w1"), row("other")]);
    app.set_viewport_height(20);
    press(&mut state, &mut app, KeyCode::F(5), now());
    assert_eq!(open_on(&app), Some(1));
}

#[test]
fn with_nothing_to_read_f5_says_so_and_opens_no_screen() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("plain")]);
    state.focus = Focus::Sidebar;
    let cmds = press(&mut state, &mut app, KeyCode::F(5), now());
    assert!(app.modal().is_none());
    assert!(read_cmds(&cmds).is_empty());
    assert!(
        state
            .displayed_status()
            .is_some_and(|status| status.contains("no brigade to read")),
        "{:?}",
        state.displayed_status()
    );
}

#[test]
fn a_solo_pane_has_no_brigade_so_f5_there_is_the_same_notice() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("plain")]);
    let solo = host(&mut state, "plain", "x", 9);
    state.member_brigades.remove(&solo);
    state.stage = Stage::Solo(solo);
    state.focus = Focus::Pane;
    let cmds = press(&mut state, &mut app, KeyCode::F(5), now());
    assert!(app.modal().is_none());
    assert!(!wrote_to_a_pane(&cmds), "F5 is never the child's");
}

#[test]
fn a_dialog_that_is_open_keeps_f5_and_the_search_box_does_too() {
    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.open_confirm_archive_modal();
    assert!(app.modal().is_some());
    press(&mut state, &mut app, KeyCode::F(5), now());
    assert!(app.reading().is_none(), "the dialog took the key");

    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.enter_search();
    press(&mut state, &mut app, KeyCode::F(5), now());
    assert!(app.reading().is_none(), "the search box took the key");
}

// --- the body and its refresh --------------------------------------------------------

#[test]
fn the_answer_fills_the_screen_and_one_for_another_brigade_or_a_closed_screen_is_dropped() {
    let (mut state, mut app) = staged();
    let t = now();
    // No screen open: dropped.
    loaded(&mut state, &mut app, 1, with_decision(Some("q?")), t);
    assert!(app.modal().is_none());

    press(&mut state, &mut app, KeyCode::F(5), t);
    loaded(&mut state, &mut app, 2, with_decision(Some("wrong")), t);
    assert_eq!(app.reading().unwrap().body(), &ReadingBody::Loading);

    loaded(&mut state, &mut app, 1, with_decision(Some("q?")), t);
    assert!(app.reading().unwrap().decision_open());

    send(
        &mut state,
        &mut app,
        Event::BrigadeReadingRead {
            brigade_id: 1,
            result: Err("database is locked".to_string()),
        },
        t,
    );
    assert_eq!(
        app.reading().unwrap().body(),
        &ReadingBody::Unreadable("database is locked".to_string())
    );
    assert!(!app.reading().unwrap().decision_open());
}

#[test]
fn it_reads_again_only_while_open_and_only_when_a_counter_moved() {
    let (mut state, mut app) = staged();
    let t = now();

    // Closed: ticks that move the counters cost nothing.
    let mut cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts(1, "dir", Some(1))],
        },
        t,
    );
    assert!(read_cmds(&cmds).is_empty());

    press(&mut state, &mut app, KeyCode::F(5), t);
    assert_eq!(app.reading().unwrap().asked(), (None, Some(1)));

    // The same counters: nothing.
    cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts(1, "dir", Some(1))],
        },
        t + Duration::from_secs(1),
    );
    assert!(read_cmds(&cmds).is_empty());

    // A Progress write: one read.
    cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts(1, "dir", Some(2))],
        },
        t + Duration::from_secs(2),
    );
    assert_eq!(read_cmds(&cmds), vec![1]);

    // Another brigade's write is not this screen's business.
    cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts(1, "dir", Some(2)), facts(2, "other", Some(9))],
        },
        t + Duration::from_secs(3),
    );
    assert!(read_cmds(&cmds).is_empty());

    // A new request: one read; the request going away (answered): one more.
    let mut with_request = facts(1, "dir", Some(2));
    with_request.request = Some(RequestFact { seq: 7 });
    cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![with_request],
        },
        t + Duration::from_secs(4),
    );
    assert_eq!(read_cmds(&cmds), vec![1]);
    assert_eq!(app.reading().unwrap().asked(), (Some(7), Some(2)));

    // Closed again: back to costing nothing.
    press(
        &mut state,
        &mut app,
        KeyCode::Esc,
        t + Duration::from_secs(5),
    );
    cmds = send(
        &mut state,
        &mut app,
        Event::Tick {
            relay: Vec::new(),
            brigades: vec![facts(1, "dir", Some(3))],
        },
        t + Duration::from_secs(6),
    );
    assert!(read_cmds(&cmds).is_empty());
}

#[test]
fn a_read_that_lands_after_the_operator_answered_does_not_show_that_decision() {
    let (mut state, mut app) = staged();
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    state.request_answered.insert(1, 4);
    loaded(&mut state, &mut app, 1, with_decision(Some("old")), t);
    assert!(!app.reading().unwrap().decision_open());
    // A newer request than the answered one is still shown.
    let newer = BrigadeReading {
        decision: Some(decision(5, Some("new"))),
        progress: None,
    };
    loaded(&mut state, &mut app, 1, newer, t);
    assert!(app.reading().unwrap().decision_open());
}

// --- keys ------------------------------------------------------------------------------

#[test]
fn esc_closes_and_nothing_else_closes_it_or_reaches_a_pane() {
    let (mut state, mut app) = staged();
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    for code in [
        KeyCode::Char('x'),
        KeyCode::Char('q'),
        KeyCode::Tab,
        KeyCode::Backspace,
        KeyCode::F(5),
        KeyCode::F(2),
    ] {
        let cmds = press(&mut state, &mut app, code, t);
        assert!(app.reading().is_some(), "{code:?} closed it");
        assert!(!wrote_to_a_pane(&cmds), "{code:?}: {cmds:?}");
    }
    // A Ctrl chord that would swap a dialog in does nothing here.
    press_with(
        &mut state,
        &mut app,
        KeyCode::Char('r'),
        Modifiers::CONTROL,
        t,
    );
    assert!(app.reading().is_some());
    press(&mut state, &mut app, KeyCode::Esc, t);
    assert!(app.modal().is_none());
}

#[test]
fn enter_with_a_decision_open_closes_the_screen_and_goes_where_enter_on_the_directors_row_goes() {
    let (mut state, mut app) = staged();
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    loaded(&mut state, &mut app, 1, with_decision(Some("ship?")), t);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(app.modal().is_none());
    assert_eq!(
        cmds.iter()
            .filter_map(|cmd| match cmd {
                Cmd::Store(StoreIntent::ResolveMembership { session_id }) => {
                    Some(session_id.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["dir"]
    );
    assert!(!wrote_to_a_pane(&cmds), "Enter is not typed into the pane");
}

#[test]
fn enter_goes_to_an_unstaged_brigades_director_by_the_session_the_roster_names() {
    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.select_next();
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    loaded(&mut state, &mut app, 2, with_decision(None), t);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::Store(StoreIntent::ResolveMembership { session_id }) if session_id == "other"
    )));
}

#[test]
fn enter_with_no_decision_open_does_nothing() {
    let (mut state, mut app) = staged();
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    // Still loading.
    assert!(press(&mut state, &mut app, KeyCode::Enter, t).is_empty());
    assert!(app.reading().is_some());
    // Loaded with Progress only.
    loaded(
        &mut state,
        &mut app,
        1,
        BrigadeReading {
            decision: None,
            progress: Some(progress_reading(1, 1)),
        },
        t,
    );
    assert!(press(&mut state, &mut app, KeyCode::Enter, t).is_empty());
    assert!(app.reading().is_some());
}

#[test]
fn enter_says_so_when_the_directors_row_is_not_in_the_list() {
    let (mut state, _) = staged();
    state.focus = Focus::Sidebar;
    state.stage = Stage::Empty;
    state.brigade_names.insert(2, facts(2, "gone", None));
    let mut app = App::new(vec![row("plain")]);
    app.open_reading_modal(crate::reading::ReadingState::new(
        2,
        "b".to_string(),
        (None, None),
    ));
    let t = now();
    loaded(&mut state, &mut app, 2, with_decision(None), t);
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(cmds.is_empty());
    assert!(
        state
            .displayed_status()
            .is_some_and(|status| status.contains("not in the list")),
        "{:?}",
        state.displayed_status()
    );
}

#[test]
fn the_scroll_keys_stay_inside_the_text() {
    let (mut state, mut app) = staged();
    // A short terminal and a long Progress.
    state.size = (80, 14);
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    loaded(
        &mut state,
        &mut app,
        1,
        BrigadeReading {
            decision: None,
            progress: Some(progress_reading(1, 30)),
        },
        t,
    );
    let max = crate::reading::reading_max_scroll(app.reading().unwrap(), t, state.size);
    assert!(max > 10, "the text must overflow for this test: {max}");

    press(&mut state, &mut app, KeyCode::Up, t);
    assert_eq!(app.reading().unwrap().scroll(), 0, "not above the top");
    press(&mut state, &mut app, KeyCode::Down, t);
    press(&mut state, &mut app, KeyCode::Char('j'), t);
    assert_eq!(app.reading().unwrap().scroll(), 2);
    press(&mut state, &mut app, KeyCode::Char('k'), t);
    assert_eq!(app.reading().unwrap().scroll(), 1);
    press(&mut state, &mut app, KeyCode::PageDown, t);
    assert!(app.reading().unwrap().scroll() > 2);
    press(&mut state, &mut app, KeyCode::End, t);
    assert_eq!(app.reading().unwrap().scroll(), max);
    press(&mut state, &mut app, KeyCode::Down, t);
    assert_eq!(app.reading().unwrap().scroll(), max, "not below the end");
    // One press up from the end moves at once: no overshoot to undo.
    press(&mut state, &mut app, KeyCode::Up, t);
    assert_eq!(app.reading().unwrap().scroll(), max - 1);
    press(&mut state, &mut app, KeyCode::Home, t);
    assert_eq!(app.reading().unwrap().scroll(), 0);
    press(&mut state, &mut app, KeyCode::PageUp, t);
    assert_eq!(app.reading().unwrap().scroll(), 0);
}

#[test]
fn a_refresh_that_shortens_the_text_pulls_the_scroll_back_inside_it() {
    let (mut state, mut app) = staged();
    state.size = (80, 14);
    let t = now();
    press(&mut state, &mut app, KeyCode::F(5), t);
    loaded(
        &mut state,
        &mut app,
        1,
        BrigadeReading {
            decision: None,
            progress: Some(progress_reading(1, 30)),
        },
        t,
    );
    press(&mut state, &mut app, KeyCode::End, t);
    loaded(
        &mut state,
        &mut app,
        1,
        BrigadeReading {
            decision: None,
            progress: Some(progress_reading(2, 2)),
        },
        t,
    );
    let max = crate::reading::reading_max_scroll(app.reading().unwrap(), t, state.size);
    assert_eq!(app.reading().unwrap().scroll(), max);
}

#[test]
fn a_recording_that_predates_progress_still_parses() {
    let old = r#"{"brigade_id":1,"name":"alpha","director_session":null}"#;
    let facts: BrigadeNameFacts = serde_json::from_str(old).unwrap();
    assert_eq!(facts.progress, None);
}
