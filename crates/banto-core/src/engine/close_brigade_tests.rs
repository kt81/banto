//! Closing a brigade: every process it hosts ends, nothing in the store does.
//! The two gestures (prefix-`x` on the Director's tile, `x` on a list row), the
//! dialog's reading, the close itself and what it leaves behind, and opening the
//! brigade again. Driven through real `Event`s, as `member_exit_tests` is.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::model::{Activity, AgentKind, BrigadeRole, PtyExitReason, Quiescence, SessionRow};
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

fn host_goinkyo(state: &mut EmporiumState, brigade_id: BrigadeId) -> SessionKey {
    let key = SessionKey::new_worker(brigade_id, GOINKYO_TOKEN);
    state.screens.insert(key.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(key.clone(), GOINKYO_TOKEN.to_string());
    state.member_brigades.insert(key.clone(), brigade_id);
    state.goinkyo_pane.insert(brigade_id, key.clone());
    key
}

/// Brigade 1 as staging leaves it: the Director `dir`, Workers `w1` and `w2`,
/// all hosted and staged, the focus on `w1`. The list has a row for `dir`.
fn staged() -> (EmporiumState, App) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let panes: Vec<SessionKey> = [
        ("dir", DIRECTOR_TOKEN),
        ("w1", "worker-1"),
        ("w2", "worker-2"),
    ]
    .into_iter()
    .map(|(id, token)| host(&mut state, id, token, 1))
    .collect();
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(key("dir")),
        panes,
        focused: 1,
    };
    state.focus = Focus::Pane;
    (state, App::new(vec![row("dir")]))
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, at)
}

fn press(state: &mut EmporiumState, app: &mut App, code: KeyCode, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::Input(InputEvent::Key(KeyEvent::new(code, Modifiers::NONE))),
        at,
    )
}

fn prefix_x_on(state: &mut EmporiumState, app: &mut App, id: &str, at: Instant) {
    if let Stage::Brigade { panes, focused, .. } = &mut state.stage {
        *focused = panes.iter().position(|pane| *pane == key(id)).unwrap();
    }
    state.prefix_armed = Some(at);
    press(state, app, KeyCode::Char('x'), at);
}

fn exits(state: &mut EmporiumState, app: &mut App, key: &SessionKey, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::PtyExited {
            key: key.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    )
}

fn without_resizes(cmds: Vec<Cmd>) -> Vec<Cmd> {
    cmds.into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::ResizePty { .. }))
        .collect()
}

fn kills(cmds: &[Cmd]) -> Vec<SessionKey> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::HangUpPty { key } => Some(key.clone()),
            _ => None,
        })
        .collect()
}

fn is_store_or_reload(cmd: &Cmd) -> bool {
    matches!(cmd, Cmd::Store(_) | Cmd::Reload)
}

fn closing_modal(app: &App) -> Option<i64> {
    match app.modal() {
        Some(Modal::ConfirmCloseBrigade { brigade_id, .. }) => Some(*brigade_id),
        _ => None,
    }
}

fn status(state: &EmporiumState) -> String {
    state.status.clone().unwrap_or_default()
}

/// Open the dialog with prefix-`x` on the Director and confirm it.
fn close_staged(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    prefix_x_on(state, app, "dir", at);
    assert_eq!(closing_modal(app), Some(1));
    press(state, app, KeyCode::Enter, at)
}

fn resolved(session_id: &str, membership: Option<(BrigadeId, &str, BrigadeRole)>) -> Event {
    let members = membership
        .as_ref()
        .map(|(_, token, role)| vec![(token.to_string(), *role, Some(session_id.to_string()))]);
    Event::MembershipResolved {
        session_id: session_id.to_string(),
        membership: membership.map(|(id, token, role)| (id, token.to_string(), role)),
        members,
        layout: None,
        names: Vec::new(),
    }
}

/// `x` on the selected row, then the store's answer about it.
fn list_x(
    state: &mut EmporiumState,
    app: &mut App,
    answer: Option<(BrigadeId, &str, BrigadeRole)>,
    at: Instant,
) -> Vec<Cmd> {
    state.focus = Focus::Sidebar;
    let id = app.selected_row().unwrap().id.clone();
    let asked = without_resizes(press(state, app, KeyCode::Char('x'), at));
    assert_eq!(
        asked,
        vec![Cmd::Store(StoreIntent::ResolveMembership {
            session_id: id.clone()
        })],
        "the row's role is the store's to say"
    );
    assert!(app.modal().is_none(), "nothing opens before the answer");
    without_resizes(send(state, app, resolved(&id, answer), at))
}

// --- gestures ------------------------------------------------------------------

#[test]
fn prefix_x_on_the_director_asks_to_close_even_with_no_director_pane_known_to_the_stage() {
    // The decision is the ownership maps', not `Stage::director`: a brigade
    // staged around a Director with no known pane still has one on its tile.
    let (mut state, mut app) = staged();
    if let Stage::Brigade { director, .. } = &mut state.stage {
        *director = None;
    }
    prefix_x_on(&mut state, &mut app, "dir", now());
    assert_eq!(closing_modal(&app), Some(1));
}

#[test]
fn prefix_x_on_a_worker_or_the_goinkyo_is_still_a_dismissal() {
    let (mut state, mut app) = staged();
    let goinkyo = host_goinkyo(&mut state, 1);
    if let Stage::Brigade { panes, .. } = &mut state.stage {
        panes.push(goinkyo);
    }
    for (id, who) in [("w1", "worker-1"), ("w2", "worker-2")] {
        prefix_x_on(&mut state, &mut app, id, now());
        assert!(
            matches!(
                app.modal(),
                Some(Modal::ConfirmKill { dismisses: Some(name), .. }) if name == who
            ),
            "{id}: not a dismissal dialog"
        );
        app.close_modal();
    }
    prefix_x_on(
        &mut state,
        &mut app,
        SessionKey::new_worker(1, GOINKYO_TOKEN).as_str(),
        now(),
    );
    assert!(matches!(
        app.modal(),
        Some(Modal::ConfirmKill {
            dismisses: Some(_),
            ..
        })
    ));
}

#[test]
fn prefix_x_on_a_single_session_is_still_a_plain_kill() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    state.focus = Focus::Pane;
    let mut app = App::new(vec![row("solo")]);
    let t = now();
    state.prefix_armed = Some(t);
    press(&mut state, &mut app, KeyCode::Char('x'), t);
    assert!(matches!(
        app.modal(),
        Some(Modal::ConfirmKill {
            dismisses: None,
            ..
        })
    ));
    assert_eq!(
        press(&mut state, &mut app, KeyCode::Enter, t),
        vec![Cmd::HangUpPty { key: key("solo") }]
    );
}

#[test]
fn the_surviving_director_of_a_disbanded_brigade_is_a_single_session_to_prefix_x() {
    // A disband leaves the Director running with the maps it was staged with;
    // closing a brigade that is being deleted would be a second, wrong dialog.
    let (mut state, mut app) = staged();
    state.disbanding.insert(1);
    prefix_x_on(&mut state, &mut app, "dir", now());
    assert!(matches!(
        app.modal(),
        Some(Modal::ConfirmKill {
            dismisses: None,
            ..
        })
    ));
}

#[test]
fn x_on_a_hosted_single_sessions_row_asks_the_plain_kill_and_confirming_ends_it() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    let mut app = App::new(vec![row("solo")]);
    let t = now();
    let cmds = list_x(&mut state, &mut app, None, t);
    assert!(cmds.is_empty());
    assert!(matches!(
        app.modal(),
        Some(Modal::ConfirmKill {
            dismisses: None,
            ..
        })
    ));
    assert_eq!(
        press(&mut state, &mut app, KeyCode::Enter, t),
        vec![Cmd::HangUpPty { key: key("solo") }]
    );
}

#[test]
fn x_on_a_row_with_nothing_hosted_says_so_and_opens_nothing() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("solo")]);
    let cmds = list_x(&mut state, &mut app, None, now());
    assert!(cmds.is_empty());
    assert!(app.modal().is_none());
    assert!(
        status(&state).contains("no open pane"),
        "{}",
        status(&state)
    );
}

#[test]
fn x_on_a_directors_row_asks_to_close_whether_the_brigade_is_staged_or_not() {
    // Staged.
    let (mut state, mut app) = staged();
    list_x(
        &mut state,
        &mut app,
        Some((1, DIRECTOR_TOKEN, BrigadeRole::Director)),
        now(),
    );
    assert_eq!(closing_modal(&app), Some(1));

    // Off stage: another session is what is showing.
    let (mut state, mut app) = staged();
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    list_x(
        &mut state,
        &mut app,
        Some((1, DIRECTOR_TOKEN, BrigadeRole::Director)),
        now(),
    );
    assert_eq!(closing_modal(&app), Some(1));
}

#[test]
fn x_on_a_directors_row_reaches_a_brigade_whose_director_pane_is_already_gone() {
    let (mut state, mut app) = staged();
    let t = now();
    let dir = key("dir");
    state.expected_exits.insert(dir.clone(), None);
    exits(&mut state, &mut app, &dir, t);
    assert!(!state.screens.contains_key(&dir));
    list_x(
        &mut state,
        &mut app,
        Some((1, DIRECTOR_TOKEN, BrigadeRole::Director)),
        t,
    );
    assert_eq!(closing_modal(&app), Some(1), "its Workers are still hosted");
}

#[test]
fn x_on_a_directors_row_with_nothing_hosted_is_a_status_line_and_no_dialog() {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![row("dir")]);
    let cmds = list_x(
        &mut state,
        &mut app,
        Some((1, DIRECTOR_TOKEN, BrigadeRole::Director)),
        now(),
    );
    assert!(cmds.is_empty());
    assert!(app.modal().is_none());
    assert!(status(&state).contains("nothing of"), "{}", status(&state));
}

#[test]
fn x_on_a_workers_or_goinkyos_row_never_dismisses_and_points_to_its_tile() {
    for role in [BrigadeRole::Worker, BrigadeRole::Goinkyo] {
        let (mut state, mut app) = staged();
        let mut app_rows = App::new(vec![row("w1")]);
        std::mem::swap(&mut app, &mut app_rows);
        let cmds = list_x(&mut state, &mut app, Some((1, "worker-1", role)), now());
        assert!(cmds.is_empty(), "{role:?}: {cmds:?}");
        assert!(app.modal().is_none());
        assert!(state.pending_dismiss.is_none());
        assert!(status(&state).contains("Prefix+x"), "{}", status(&state));
    }
}

#[test]
fn x_types_into_the_search_box_and_does_nothing_while_a_dialog_is_open() {
    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.enter_search();
    let cmds = without_resizes(press(&mut state, &mut app, KeyCode::Char('x'), now()));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(app.query(), "x");
    assert!(state.pending_membership.is_none());

    let (mut state, mut app) = staged();
    state.focus = Focus::Sidebar;
    app.open_confirm_disband_modal(1, "dir".to_string());
    let cmds = without_resizes(press(&mut state, &mut app, KeyCode::Char('x'), now()));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(state.pending_membership.is_none());
}

// --- the dialog's reading --------------------------------------------------------

#[test]
fn the_reading_counts_every_hosted_pane_and_names_the_not_idle_by_the_quit_questions_reasons() {
    let (mut state, app) = staged();
    let goinkyo = host_goinkyo(&mut state, 1);
    let other = host(&mut state, "other", "worker-1", 2);
    let t = now();
    state.quiescence.insert(key("dir"), Quiescence::Idle);
    state.quiescence.insert(key("w1"), Quiescence::NotIdle);
    state.quiescence.insert(key("w2"), Quiescence::Background);
    state.quiescence.insert(goinkyo, Quiescence::Unknown);
    state.quiescence.insert(other, Quiescence::NotIdle);

    let reading = state.close_reading(&app, 1, t);
    assert_eq!(reading.ending, 4, "the other brigade's pane is not counted");
    assert_eq!(
        reading.not_idle,
        vec![
            "goinkyo (unknown)".to_string(),
            "worker-1 (busy)".to_string()
        ],
        "the quit question's order and words"
    );
    assert_eq!(reading.background, vec!["worker-2".to_string()]);

    // A pane that was just given input is `input`, whatever the sample says.
    state.last_given_input.insert(key("dir"), t);
    let reading = state.close_reading(&app, 1, t);
    assert!(reading.not_idle.contains(&"dir (input)".to_string()));

    // And the reading follows the panes, not a list taken earlier.
    state.screens.remove(&key("w1"));
    state.member_brigades.remove(&key("w1"));
    let reading = state.close_reading(&app, 1, t + Duration::from_secs(60));
    assert_eq!(reading.ending, 3);
    assert!(
        !reading
            .not_idle
            .iter()
            .any(|name| name.contains("worker-1"))
    );
}

#[test]
fn the_reading_is_the_quit_questions_own_count_narrowed_to_the_brigade() {
    // Same panes, same classification: with only this brigade hosted, the two
    // name the same panes in the same order.
    let (mut state, app) = staged();
    let t = now();
    state.quiescence.insert(key("dir"), Quiescence::NotIdle);
    state.quiescence.insert(key("w2"), Quiescence::Unknown);
    let reading = state.close_reading(&app, 1, t);
    let counts = state.quit_counts(&app, t);
    let from_counts: Vec<String> = counts
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, QuitKind::NotIdle(_)))
        .map(|entry| match entry.kind {
            QuitKind::NotIdle(reason) => format!("{} ({})", entry.name, reason.word()),
            QuitKind::Background => unreachable!(),
        })
        .collect();
    assert_eq!(reading.not_idle, from_counts);
}

// --- the close -------------------------------------------------------------------

#[test]
fn a_staged_close_ends_every_pane_empties_the_stage_hands_the_focus_back_and_writes_nothing() {
    let (mut state, mut app) = staged();
    let goinkyo = host_goinkyo(&mut state, 1);
    let t = now();
    let cmds = close_staged(&mut state, &mut app, t);

    assert!(app.modal().is_none());
    assert!(state.closing.contains_key(&1));
    let mut ended = kills(&cmds);
    ended.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut expected = vec![key("dir"), key("w1"), key("w2"), goinkyo.clone()];
    expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(ended, expected, "every hosted pane, once");
    assert_eq!(kills(&cmds).len(), 4);
    assert!(
        !cmds.iter().any(is_store_or_reload),
        "nothing is written and nothing reloaded: {cmds:?}"
    );
    assert!(matches!(state.stage, Stage::Empty));
    assert_eq!(state.focus, Focus::Sidebar);
    // Ends banto asked for, each remembered with its member.
    for key in kills(&cmds) {
        assert!(state.expected_exits.contains_key(&key), "{key:?}");
    }
    assert!(state.expected_exits[&key("w1")].is_some());
}

#[test]
fn an_offstage_close_leaves_the_stage_and_the_focus_alone() {
    let (mut state, mut app) = staged();
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    state.focus = Focus::Pane;
    app.open_confirm_close_brigade_modal(1, "dir".to_string());
    let cmds = press(&mut state, &mut app, KeyCode::Enter, now());
    assert_eq!(kills(&cmds).len(), 3);
    assert!(matches!(&state.stage, Stage::Solo(k) if *k == key("solo")));
    assert_eq!(state.focus, Focus::Pane);
}

#[test]
fn a_close_with_the_directors_pane_gone_ends_what_is_left() {
    let (mut state, mut app) = staged();
    let t = now();
    state.expected_exits.insert(key("dir"), None);
    exits(&mut state, &mut app, &key("dir"), t);
    app.open_confirm_close_brigade_modal(1, "dir".to_string());
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    let mut ended = kills(&cmds);
    ended.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(ended, vec![key("w1"), key("w2")]);
}

#[test]
fn the_confirm_closes_what_is_hosted_when_enter_is_pressed_not_when_the_dialog_opened() {
    let (mut state, mut app) = staged();
    let t = now();
    app.open_confirm_close_brigade_modal(1, "dir".to_string());
    // Everything ends while the dialog is up.
    for id in ["dir", "w1", "w2"] {
        state.expected_exits.insert(key(id), None);
        exits(&mut state, &mut app, &key(id), t);
    }
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(app.modal().is_none());
    assert!(!state.closing.contains_key(&1), "nothing was left to close");
}

#[test]
fn escape_cancels_and_a_second_enter_is_no_second_close() {
    let (mut state, mut app) = staged();
    let t = now();
    prefix_x_on(&mut state, &mut app, "dir", t);
    assert!(press(&mut state, &mut app, KeyCode::Esc, t).is_empty());
    assert!(app.modal().is_none());
    assert!(state.closing.is_empty());

    let first = close_staged(&mut state, &mut app, t);
    assert_eq!(kills(&first).len(), 3);
    let again = press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(kills(&again).is_empty(), "{again:?}");
}

#[test]
fn a_brigade_already_closing_or_being_disbanded_is_refused_with_a_line_not_a_dialog() {
    let (mut state, mut app) = staged();
    state.closing.insert(1, now());
    prefix_x_on(&mut state, &mut app, "dir", now());
    assert!(app.modal().is_none());
    assert!(status(&state).contains("already closing"));

    let (mut state, mut app) = staged();
    state.disbanding.insert(1);
    state.focus = Focus::Sidebar;
    list_x(
        &mut state,
        &mut app,
        Some((1, DIRECTOR_TOKEN, BrigadeRole::Director)),
        now(),
    );
    assert!(app.modal().is_none());
    assert!(status(&state).contains("being disbanded"));
}

// --- the exits ---------------------------------------------------------------------

#[test]
fn the_exits_of_a_closed_brigade_are_one_status_line_and_remove_and_reopen_nobody() {
    let (mut state, mut app) = staged();
    let goinkyo = host_goinkyo(&mut state, 1);
    state.brigade_names.insert(
        1,
        BrigadeNameFacts {
            brigade_id: 1,
            name: "alpha".to_string(),
            director_session: Some("dir".to_string()),
            member_names: Vec::new(),
            request: None,
        },
    );
    let name = state.brigade_name(&app, 1);
    let t = now();
    close_staged(&mut state, &mut app, t);
    state.status = None;

    let order = [key("w2"), goinkyo, key("dir"), key("w1")];
    for (i, pane) in order.iter().enumerate() {
        let cmds = without_resizes(exits(&mut state, &mut app, pane, t));
        assert!(cmds.is_empty(), "{pane:?}: {cmds:?}");
        assert!(
            !status(&state).contains("session ended"),
            "one line for the brigade, not one per pane: {}",
            status(&state)
        );
        if i + 1 < order.len() {
            assert!(
                state.closing.contains_key(&1),
                "still closing after {pane:?}"
            );
        }
    }
    assert!(!state.closing.contains_key(&1), "cleared at the last exit");
    assert_eq!(status(&state), format!("{name} closed"));
    assert!(state.expected_exits.is_empty());
    assert!(state.screens.is_empty());
}

#[test]
fn a_director_that_exits_on_its_own_is_the_same_as_ever() {
    let (mut state, mut app) = staged();
    let t = now();
    let cmds = without_resizes(exits(&mut state, &mut app, &key("dir"), t));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(
        status(&state).contains("session ended"),
        "{}",
        status(&state)
    );
    assert!(state.closing.is_empty());
    assert!(
        matches!(&state.stage, Stage::Brigade { director: None, panes, .. } if panes.len() == 2)
    );
}

#[test]
fn a_worker_that_dies_on_its_own_during_a_close_is_not_relaunched_or_removed_from_the_store() {
    let (mut state, _) = staged();
    // A Worker with a session of its own is the one the store could dismiss or
    // the shell could relaunch.
    let mut app = App::new(vec![row("dir"), row("w1")]);
    let t = now();
    close_staged(&mut state, &mut app, t);
    // Not through `kill_pane`'s record: as if it had died by itself first.
    state.expected_exits.remove(&key("w1"));
    let cmds = without_resizes(exits(&mut state, &mut app, &key("w1"), t));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn a_worker_of_a_closing_brigade_is_never_one_that_left_on_its_own() {
    // The store-writing path's own condition, whatever handles the exit before
    // it: closing deletes nobody.
    let (mut state, app) = staged();
    let w1 = key("w1");
    assert_eq!(
        unrequested_worker_exit(&state, &app, &w1, false),
        Some((1, "worker-1".to_string()))
    );
    state.closing.insert(1, now());
    assert_eq!(unrequested_worker_exit(&state, &app, &w1, false), None);
}

#[test]
fn a_closing_brigade_cannot_be_staged_and_can_once_the_last_pane_has_gone() {
    let (mut state, _) = staged();
    let mut app = App::new(vec![row("dir"), row("w1"), row("w2")]);
    let t = now();
    close_staged(&mut state, &mut app, t);
    let members = vec![
        (
            DIRECTOR_TOKEN.to_string(),
            BrigadeRole::Director,
            Some("dir".to_string()),
        ),
        (
            "worker-1".to_string(),
            BrigadeRole::Worker,
            Some("w1".to_string()),
        ),
    ];

    // While any pane is still ending: refused, with a line, opening nothing.
    exits(&mut state, &mut app, &key("w2"), t);
    let refused = stage_brigade(&mut state, &app, 1, &members, &BrigadeConfig::default());
    assert!(refused.is_empty(), "{refused:?}");
    assert!(
        status(&state).contains("still closing"),
        "{}",
        status(&state)
    );
    assert!(matches!(state.stage, Stage::Empty));

    // Enter on the Director's row goes the same way (membership resolved).
    state.focus = Focus::Sidebar;
    state.pending_membership = Some((PendingMembership::Activate, "dir".to_string()));
    let cmds = send(
        &mut state,
        &mut app,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((1, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
            members: Some(members.clone()),
            layout: None,
            names: Vec::new(),
        },
        t,
    );
    assert!(
        cmds.iter()
            .all(|cmd| !matches!(cmd, Cmd::OpenEmbedded { .. }))
    );

    for id in ["dir", "w1"] {
        exits(&mut state, &mut app, &key(id), t);
    }
    assert!(!state.closing.contains_key(&1));

    // Closed: Enter on the Director's row opens everyone, each exactly once.
    let opened = stage_brigade(&mut state, &app, 1, &members, &BrigadeConfig::default());
    let mut targets: Vec<String> = opened
        .iter()
        .filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded { target, .. } => Some(target.id.clone()),
            _ => None,
        })
        .collect();
    targets.sort();
    assert_eq!(targets, vec!["dir".to_string(), "w1".to_string()]);
}

#[test]
fn a_worker_listed_as_a_member_of_a_closing_brigade_is_not_shown_solo() {
    let (mut state, mut app) = staged();
    let t = now();
    close_staged(&mut state, &mut app, t);
    let mut app = App::new(vec![row("w1")]);
    state.focus = Focus::Sidebar;
    state.pending_membership = Some((PendingMembership::Activate, "w1".to_string()));
    let cmds = send(
        &mut state,
        &mut app,
        resolved("w1", Some((1, "worker-1", BrigadeRole::Worker))),
        t,
    );
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(matches!(state.stage, Stage::Empty));
    assert!(status(&state).contains("still closing"));
}

// --- what the close leaves behind -------------------------------------------------

#[test]
fn closing_drops_the_queued_and_counted_state_of_its_panes_and_only_theirs() {
    let (mut state, mut app) = staged();
    let other = host(&mut state, "other", "worker-1", 2);
    let t = now();
    for pane in [key("w1"), other.clone()] {
        state.pending_submits.push(PendingSubmit {
            key: pane,
            nudged_at: t,
        });
    }
    for brigade_id in [1, 2] {
        state
            .prompt_held_since
            .insert((brigade_id, "worker-1".to_string()), t);
        state
            .unreadable_since
            .insert((brigade_id, "worker-1".to_string()), t);
        state
            .unreadable_noticed
            .insert((brigade_id, "worker-1".to_string()));
    }
    let held = open_worker(
        &mut state,
        1,
        "worker-3",
        &PathBuf::from("/work/alpha"),
        WorkerChoice {
            agent: AgentKind::Codex,
            model: "some-model",
            effort: "",
        },
        RoleLaunch::of(
            BrigadeRole::Worker,
            AgentKind::Codex,
            &BrigadeConfig::default(),
        ),
    );
    assert!(matches!(held.as_slice(), [Cmd::ReadCodexCatalog { .. }]));
    assert_eq!(state.codex_opens_awaiting_catalog.len(), 1);

    close_staged(&mut state, &mut app, t);

    assert_eq!(
        state
            .pending_submits
            .iter()
            .map(|pending| pending.key.clone())
            .collect::<Vec<_>>(),
        vec![other],
        "a nudge's delayed Enter would be written into a dead pane"
    );
    assert!(state.codex_opens_awaiting_catalog.is_empty());
    for map in [&state.prompt_held_since, &state.unreadable_since] {
        assert_eq!(map.keys().map(|(id, _)| *id).collect::<Vec<_>>(), vec![2]);
    }
    assert_eq!(
        state
            .unreadable_noticed
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn a_creation_held_for_a_closing_brigade_is_no_longer_wanted() {
    let (mut state, _app) = staged();
    state.member_order.insert(1, vec!["worker-9".to_string()]);
    assert!(creation_still_wanted(&state, 1, "worker-9"));
    state.closing.insert(1, now());
    assert!(!creation_still_wanted(&state, 1, "worker-9"));
}

#[test]
fn a_spawn_reported_after_the_close_is_ended_at_once_and_holds_the_brigade_closing_until_it_exits()
{
    let (mut state, mut app) = staged();
    let t = now();
    // A Worker and a Director asked for before the close, spawned after it.
    let worker = SessionKey::new_worker(1, "worker-9");
    state
        .member_tokens
        .insert(worker.clone(), "worker-9".to_string());
    state.member_brigades.insert(worker.clone(), 1);
    state.pending_opens.insert(
        worker.clone(),
        PendingOpen::BrigadeMember {
            brigade_id: 1,
            needs_codex_kickoff: false,
        },
    );
    let director = key("dir-new");
    state.pending_opens.insert(
        director.clone(),
        PendingOpen::BrigadeDirector {
            brigade_id: 1,
            worker_tokens: vec!["worker-1".to_string()],
            cwd: PathBuf::from("/work/alpha"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: String::new(),
            worker_effort: String::new(),
        },
    );
    let ended = kills(&close_staged(&mut state, &mut app, t));
    assert!(!ended.contains(&worker), "no screen yet, nothing to kill");

    for pane in [key("dir"), key("w1"), key("w2")] {
        exits(&mut state, &mut app, &pane, t);
    }
    assert!(
        state.closing.contains_key(&1),
        "two panes are still being opened"
    );

    for pane in [&worker, &director] {
        let cmds = send(
            &mut state,
            &mut app,
            Event::Spawned {
                key: pane.clone(),
                agent: None,
            },
            t,
        );
        assert_eq!(kills(&cmds), vec![pane.clone()], "{pane:?}: {cmds:?}");
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
            "a Director spawned after the close opens no Workers"
        );
        assert!(matches!(state.stage, Stage::Empty));
    }
    assert!(state.closing.contains_key(&1), "both are still ending");
    for pane in [&worker, &director] {
        assert!(without_resizes(exits(&mut state, &mut app, pane, t)).is_empty());
    }
    assert!(!state.closing.contains_key(&1));
    assert!(status(&state).ends_with(" closed"), "{}", status(&state));
}

// --- the Goinkyo -------------------------------------------------------------------

#[test]
fn a_goinkyo_is_not_spawned_for_a_closing_brigade_and_stays_a_member() {
    let (mut state, mut app) = staged();
    let goinkyo = host_goinkyo(&mut state, 1);
    let t = now();
    let cmds = close_staged(&mut state, &mut app, t);
    assert!(kills(&cmds).contains(&goinkyo));
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::DismissWorker { .. }))),
        "ending a Goinkyo's pane is not dismissing it: {cmds:?}"
    );

    // The tick's observation, before and after the exits: the roster lists the
    // Goinkyo, the Director's pane is still hosted, no Goinkyo pane is.
    let candidate = || GoinkyoSpawnCandidate {
        brigade_id: 1,
        cwd: PathBuf::from("/work/alpha"),
        roster: Vec::new(),
    };
    let awaiting = || Event::GoinkyoAwaitingSpawn {
        observation: GoinkyoObservation::AwaitingSpawn(candidate()),
    };
    exits(&mut state, &mut app, &goinkyo, t);
    let cmds = send(&mut state, &mut app, awaiting(), t);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(!state.goinkyo_pane.contains_key(&1));
    for pane in [key("dir"), key("w1"), key("w2")] {
        exits(&mut state, &mut app, &pane, t);
    }
    assert!(state.closing.is_empty());
    // Closed and cleared: it is an ordinary missing Goinkyo again, which the
    // shell only observes while the Director is hosted, so this event does not
    // arise; if it did, the brigade is no longer refused.
    let cmds = send(&mut state, &mut app, awaiting(), t);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
        "{cmds:?}"
    );
}

/// Any event does: [`settle_closing`] runs at the end of every update.
fn quiet_event(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::PtyOutput {
            key: key("w1"),
            chunk: Vec::new(),
        },
        at,
    )
}

#[test]
fn a_close_whose_panes_never_end_gives_up_after_the_limit_and_counts_what_is_left() {
    let (mut state, mut app) = staged();
    let t = now();
    close_staged(&mut state, &mut app, t);
    // Two of the three exit; the third's kill did not take.
    exits(&mut state, &mut app, &key("dir"), t);
    exits(&mut state, &mut app, &key("w2"), t);
    assert!(state.closing.contains_key(&1));

    quiet_event(
        &mut state,
        &mut app,
        t + CLOSE_GIVE_UP - Duration::from_millis(1),
    );
    assert!(state.closing.contains_key(&1), "not before the limit");
    assert!(!status(&state).contains("did not end"));

    let cmds = quiet_event(&mut state, &mut app, t + CLOSE_GIVE_UP);
    assert!(!state.closing.contains_key(&1));
    assert!(
        status(&state).contains("1 pane did not end"),
        "{}",
        status(&state)
    );
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::HangUpPty { .. } | Cmd::KillPty { .. })),
        "giving up ends nothing again: {cmds:?}"
    );
    // The pane's record stays, so an exit that does come is still one banto
    // asked for, and the brigade can be closed or staged once more.
    assert!(state.expected_exits.contains_key(&key("w1")));
    let later = exits(&mut state, &mut app, &key("w1"), t + CLOSE_GIVE_UP);
    assert!(
        !later.iter().any(|cmd| matches!(cmd, Cmd::Store(_))),
        "{later:?}"
    );
    assert!(!refuse_while_closing(&mut state, &app, 1));
}

#[test]
fn giving_up_names_every_pane_that_is_left_and_a_normal_close_never_says_it() {
    let (mut state, mut app) = staged();
    let t = now();
    close_staged(&mut state, &mut app, t);
    quiet_event(&mut state, &mut app, t + CLOSE_GIVE_UP);
    assert!(
        status(&state).contains("3 panes did not end"),
        "{}",
        status(&state)
    );

    let (mut state, mut app) = staged();
    close_staged(&mut state, &mut app, t);
    for pane in ["dir", "w1", "w2"] {
        exits(
            &mut state,
            &mut app,
            &key(pane),
            t + Duration::from_millis(150),
        );
    }
    quiet_event(&mut state, &mut app, t + CLOSE_GIVE_UP * 3);
    assert!(status(&state).ends_with("closed"), "{}", status(&state));
}

// --- a Worker added while the brigade is closing ---------------------------------

fn worker_added(brigade_id: BrigadeId, token: &str) -> Event {
    Event::WorkerAdded {
        brigade_id,
        cwd: PathBuf::from("/work/alpha"),
        worker_agent: AgentKind::ClaudeCode,
        worker_model: String::new(),
        worker_effort: String::new(),
        result: Ok(token.to_string()),
    }
}

#[test]
fn a_worker_whose_addition_lands_during_a_close_is_recorded_and_not_spawned() {
    for closing in [false, true] {
        let (mut state, mut app) = staged();
        let t = now();
        if closing {
            close_staged(&mut state, &mut app, t);
        }
        let cmds = send(&mut state, &mut app, worker_added(1, "worker-3"), t);
        let pane = SessionKey::new_worker(1, "worker-3");
        assert_eq!(
            state.pending_opens.contains_key(&pane),
            !closing,
            "closing {closing}: {cmds:?}"
        );
        assert_eq!(
            cmds.iter()
                .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
            !closing,
            "closing {closing}: {cmds:?}"
        );
        // The store wrote the member row either way, so the roster has it: a
        // Worker with nothing to resume is created afresh when the brigade is
        // next opened.
        assert!(
            state.member_order[&1]
                .iter()
                .any(|token| token == "worker-3"),
            "closing {closing}"
        );
    }
}

// --- one membership question at a time -------------------------------------------

#[test]
fn an_answer_about_one_row_is_never_taken_for_the_question_asked_of_another() {
    let (mut state, _) = staged();
    let mut app = App::new(vec![row("a"), row("b")]);
    app.set_viewport_height(20);
    state.focus = Focus::Sidebar;
    host(&mut state, "dir-2", DIRECTOR_TOKEN, 2);
    let t = now();

    // Enter on `a`, then `x` on `b`, before the first answer is back.
    let asked_a = without_resizes(press(&mut state, &mut app, KeyCode::Enter, t));
    assert_eq!(
        asked_a,
        vec![Cmd::Store(StoreIntent::ResolveMembership {
            session_id: "a".to_string()
        })]
    );
    press(&mut state, &mut app, KeyCode::Down, t);
    let asked_b = without_resizes(press(&mut state, &mut app, KeyCode::Char('x'), t));
    assert_eq!(
        asked_b,
        vec![Cmd::Store(StoreIntent::ResolveMembership {
            session_id: "b".to_string()
        })]
    );

    // `a`'s answer comes first: it is brigade 1's Director, and it must neither
    // stage that brigade nor open a close dialog for it.
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        resolved("a", Some((1, DIRECTOR_TOKEN, BrigadeRole::Director))),
        t,
    ));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(closing_modal(&app), None);
    assert!(matches!(&state.stage, Stage::Brigade { id: 1, .. }));
    assert!(!state.closing.contains_key(&1));

    // `b`'s own answer still finds its question.
    send(
        &mut state,
        &mut app,
        resolved("b", Some((2, DIRECTOR_TOKEN, BrigadeRole::Director))),
        t,
    );
    assert_eq!(closing_modal(&app), Some(2));
    assert!(state.pending_membership.is_none());
}
