//! A brigade member leaving: prefix-`x` on a member other than the Director
//! dismisses it (one confirmation), and a Worker whose pane ends on its own and
//! cannot be relaunched leaves the brigade too — a member with no pane is an
//! invisible one. Everything here is driven through real `Event`s.

use std::path::PathBuf;
use std::time::Instant;

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::model::{Activity, AgentKind, BrigadeRole, PtyExitReason, SessionRow};
use crate::screen::Screen;

use super::*;

fn row(id: &str) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent: AgentKind::ClaudeCode,
        title: Some(id.to_string()),
        cwd: Some(PathBuf::from("/work/alpha")),
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

/// A staged brigade 1: the Director `dir`, a Worker `w1` (worker-1) and a Worker
/// `w2` (worker-2), every pane with a screen and its member token, the way
/// `stage_brigade` leaves them. `spoken` says which sessions have a row in the
/// session list (a Worker that never spoke has none).
fn brigade(spoken: &[&str]) -> (EmporiumState, App) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let panes = [
        ("dir", DIRECTOR_TOKEN),
        ("w1", "worker-1"),
        ("w2", "worker-2"),
    ];
    for (id, token) in panes {
        state.screens.insert(key(id), Screen::new(24, 80));
        state.member_tokens.insert(key(id), token.to_string());
        state.member_brigades.insert(key(id), 1);
    }
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(key("dir")),
        panes: panes.iter().map(|(id, _)| key(id)).collect(),
        focused: 1,
    };
    state.focus = Focus::Pane;
    let mut rows = vec![row("dir")];
    rows.extend(spoken.iter().map(|id| row(id)));
    (state, App::new(rows))
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

/// Prefix-`x`, as the operator types it: arm the prefix, then `x`.
fn prefix_x(state: &mut EmporiumState, app: &mut App, at: Instant) {
    state.prefix_armed = Some(at);
    press(state, app, KeyCode::Char('x'), at);
}

fn exits(state: &mut EmporiumState, app: &mut App, id: &str, at: Instant) -> Vec<Cmd> {
    send(
        state,
        app,
        Event::PtyExited {
            key: key(id),
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

fn removal(brigade_id: BrigadeId, token: &str, repeated: bool) -> Cmd {
    Cmd::Store(StoreIntent::DismissExitedWorker {
        brigade_id,
        token: token.to_string(),
        repeated,
    })
}

fn answered(removed: bool) -> Result<(bool, HashSet<String>, HashSet<String>), String> {
    Ok((removed, HashSet::new(), HashSet::new()))
}

fn dismissed_answer(
    state: &mut EmporiumState,
    app: &mut App,
    token: &str,
    repeated: bool,
    result: Result<(bool, HashSet<String>, HashSet<String>), String>,
    at: Instant,
) {
    send(
        state,
        app,
        Event::ExitedWorkerDismissed {
            brigade_id: 1,
            token: token.to_string(),
            repeated,
            result,
        },
        at,
    );
}

fn status(state: &EmporiumState) -> String {
    state.displayed_status().unwrap_or_default().to_string()
}

fn modal_dismisses(app: &App) -> Option<Option<String>> {
    match app.modal() {
        Some(Modal::ConfirmKill { dismisses, .. }) => Some(dismisses.clone()),
        _ => None,
    }
}

// --- prefix-x ------------------------------------------------------------------

#[test]
fn prefix_x_on_a_worker_with_a_real_key_dismisses_it_after_one_confirmation() {
    let (mut state, mut app) = brigade(&["w1", "w2"]);
    let t = now();

    prefix_x(&mut state, &mut app, t);
    assert_eq!(modal_dismisses(&app), Some(Some("worker-1".to_string())));

    // Enter: the real key carries no brigade, so one membership round trip ...
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert_eq!(
        cmds,
        vec![Cmd::Store(StoreIntent::ResolveMembership {
            session_id: "w1".to_string()
        })]
    );
    assert!(app.modal().is_none(), "one confirmation, nothing more");
    // ... whose answer is the DismissWorker ...
    state.pending_membership = Some(PendingMembership::DismissWorker);
    let cmds = send(
        &mut state,
        &mut app,
        Event::MembershipResolved {
            session_id: "w1".to_string(),
            membership: Some((1, "worker-1".to_string(), BrigadeRole::Worker)),
            members: None,
            layout: None,
            names: Vec::new(),
        },
        t,
    );
    assert_eq!(
        cmds,
        vec![Cmd::Store(StoreIntent::DismissWorker {
            brigade_id: 1,
            token: "worker-1".to_string()
        })]
    );
    // ... and the store's answer takes the tile away and ends the pane.
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        Event::WorkerDismissed {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        t,
    ));
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("w1") }]);
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&key("w1"))));
    assert!(
        status(&state).ends_with("dismissed from the brigade"),
        "{}",
        status(&state)
    );
    // The pane's own exit, which banto asked for, removes nobody a second time.
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn prefix_x_on_a_worker_with_a_synthetic_key_dismisses_it_directly() {
    let (mut state, mut app) = brigade(&[]);
    // A Worker still under its placeholder key.
    let synthetic = SessionKey::new_worker(1, "worker-4");
    state.screens.insert(synthetic.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(synthetic.clone(), "worker-4".to_string());
    state.member_brigades.insert(synthetic.clone(), 1);
    if let Stage::Brigade { panes, focused, .. } = &mut state.stage {
        panes.push(synthetic.clone());
        *focused = panes.len() - 1;
    }
    let t = now();

    prefix_x(&mut state, &mut app, t);
    assert_eq!(modal_dismisses(&app), Some(Some("worker-4".to_string())));
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert_eq!(
        cmds,
        vec![Cmd::Store(StoreIntent::DismissWorker {
            brigade_id: 1,
            token: "worker-4".to_string()
        })]
    );
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        Event::WorkerDismissed {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        t,
    ));
    assert_eq!(
        cmds,
        vec![Cmd::KillPty {
            key: synthetic.clone()
        }]
    );
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&synthetic)));
}

#[test]
fn the_dialog_names_a_named_member_by_its_quoted_name() {
    let (mut state, mut app) = brigade(&["w1"]);
    state.state_member_name(1, "worker-1", Some("impl"));
    state.resolve_member_names(1);
    prefix_x(&mut state, &mut app, now());
    assert_eq!(modal_dismisses(&app), Some(Some("\"impl\"".to_string())));
}

#[test]
fn escape_cancels_and_nothing_happens() {
    let (mut state, mut app) = brigade(&["w1"]);
    let t = now();
    prefix_x(&mut state, &mut app, t);
    let cmds = press(&mut state, &mut app, KeyCode::Esc, t);
    assert!(cmds.is_empty());
    assert!(app.modal().is_none());
    assert_eq!(state.pending_dismiss, None);
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if panes.contains(&key("w1"))));
}

#[test]
fn prefix_x_on_the_goinkyo_dismisses_it_the_same_way() {
    let (mut state, mut app) = brigade(&[]);
    let goinkyo = SessionKey::new_worker(1, GOINKYO_TOKEN);
    state.screens.insert(goinkyo.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(goinkyo.clone(), GOINKYO_TOKEN.to_string());
    state.member_brigades.insert(goinkyo.clone(), 1);
    state.goinkyo_pane.insert(1, goinkyo.clone());
    if let Stage::Brigade { panes, focused, .. } = &mut state.stage {
        panes.push(goinkyo.clone());
        *focused = panes.len() - 1;
    }
    let t = now();
    prefix_x(&mut state, &mut app, t);
    assert_eq!(modal_dismisses(&app), Some(Some("goinkyo".to_string())));
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert_eq!(
        cmds,
        vec![Cmd::Store(StoreIntent::DismissWorker {
            brigade_id: 1,
            token: GOINKYO_TOKEN.to_string()
        })]
    );
}

#[test]
fn prefix_x_on_the_director_only_ends_the_pane_and_keeps_the_member() {
    let (mut state, mut app) = brigade(&["w1"]);
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 0;
    }
    let t = now();
    prefix_x(&mut state, &mut app, t);
    assert_eq!(modal_dismisses(&app), Some(None));
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("dir") }]);
    assert_eq!(state.pending_dismiss, None);
    // Its exit (asked for) is the Director's, and removes nobody.
    let cmds = without_resizes(exits(&mut state, &mut app, "dir", t));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn prefix_x_on_a_single_session_only_ends_it() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    state.focus = Focus::Pane;
    let mut app = App::new(vec![row("solo")]);
    let t = now();
    prefix_x(&mut state, &mut app, t);
    assert_eq!(modal_dismisses(&app), Some(None));
    let cmds = press(&mut state, &mut app, KeyCode::Enter, t);
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("solo") }]);
}

#[test]
fn a_worker_is_dismissable_even_when_the_directors_pane_is_unknown() {
    // The Director's pane closed (or never resolved): `director` is None. The
    // focused pane is plainly a Worker by its token, so it is dismissable; the
    // Director's own token, if it were focused, still is not.
    let (mut state, mut app) = brigade(&["w1"]);
    if let Stage::Brigade { director, .. } = &mut state.stage {
        *director = None;
    }
    prefix_x(&mut state, &mut app, now());
    assert_eq!(modal_dismisses(&app), Some(Some("worker-1".to_string())));
    app.close_modal();

    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 0;
    }
    prefix_x(&mut state, &mut app, now());
    assert_eq!(modal_dismisses(&app), Some(None), "the Director's token");
}

#[test]
fn a_brigade_pane_with_no_known_membership_keeps_the_plain_kill() {
    let (mut state, mut app) = brigade(&["w1"]);
    state.member_tokens.remove(&key("w1"));
    prefix_x(&mut state, &mut app, now());
    assert_eq!(modal_dismisses(&app), Some(None));
}

#[test]
fn an_answer_that_is_no_member_to_dismiss_says_so_instead_of_doing_nothing() {
    for (membership, expected) in [
        (None, "could not tell which brigade member this pane is"),
        (
            Some((1, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
            "the Director cannot be dismissed",
        ),
    ] {
        let (mut state, mut app) = brigade(&["w1"]);
        let t = now();
        prefix_x(&mut state, &mut app, t);
        press(&mut state, &mut app, KeyCode::Enter, t);
        let cmds = send(
            &mut state,
            &mut app,
            Event::MembershipResolved {
                session_id: "w1".to_string(),
                membership,
                members: None,
                layout: None,
                names: Vec::new(),
            },
            t,
        );
        assert!(cmds.is_empty(), "{cmds:?}");
        assert!(status(&state).contains(expected), "{}", status(&state));
        assert_eq!(state.pending_dismiss, None);
        assert!(
            matches!(&state.stage, Stage::Brigade { panes, .. } if panes.contains(&key("w1"))),
            "nothing was dismissed, the pane stays"
        );
    }
}

// --- a Worker that ends on its own ------------------------------------------------

#[test]
fn a_worker_that_never_spoke_leaves_the_brigade_when_it_ends_on_its_own() {
    // No session row for w1: nothing to resume.
    let (mut state, mut app) = brigade(&[]);
    state.pending_kickoffs.clear();
    let t = now();

    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert_eq!(cmds, vec![removal(1, "worker-1", false)]);
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&key("w1"))));

    dismissed_answer(&mut state, &mut app, "worker-1", false, answered(true), t);
    assert_eq!(
        status(&state),
        "worker-1 exited \u{2014} removed from the brigade"
    );
    assert!(state.removing_exited.is_empty());
}

#[test]
fn a_named_worker_is_called_by_its_quoted_name_in_the_removal_status() {
    let (mut state, mut app) = brigade(&[]);
    state.state_member_name(1, "worker-1", Some("impl"));
    state.resolve_member_names(1);
    let t = now();
    exits(&mut state, &mut app, "w1", t);
    dismissed_answer(&mut state, &mut app, "worker-1", false, answered(true), t);
    assert_eq!(
        status(&state),
        "\"impl\" exited \u{2014} removed from the brigade"
    );
    // And what the core kept for it is forgotten with it.
    assert_eq!(state.member_display_name(1, "worker-1"), None);
}

#[test]
fn a_worker_with_a_session_to_resume_is_relaunched_and_stays_a_member() {
    let (mut state, mut app) = brigade(&["w1"]);
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", now()));
    assert!(matches!(&cmds[..], [Cmd::OpenEmbedded { .. }]), "{cmds:?}");
    assert!(state.removing_exited.is_empty());
    assert!(state.status.as_deref().unwrap().contains("restarting"));
}

#[test]
fn a_worker_that_dies_again_inside_the_cooldown_leaves_the_brigade() {
    let (mut state, mut app) = brigade(&["w1"]);
    let start = now();
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", start));
    assert!(matches!(&cmds[..], [Cmd::OpenEmbedded { .. }]));
    send(
        &mut state,
        &mut app,
        Event::Spawned { key: key("w1") },
        start + Duration::from_secs(1),
    );

    let cmds = without_resizes(exits(
        &mut state,
        &mut app,
        "w1",
        start + Duration::from_secs(2),
    ));
    assert_eq!(cmds, vec![removal(1, "worker-1", true)]);
    assert!(status(&state).contains("left closed"));
    dismissed_answer(
        &mut state,
        &mut app,
        "worker-1",
        true,
        answered(true),
        start + Duration::from_secs(2),
    );
    assert_eq!(
        status(&state),
        "worker-1 exited again \u{2014} removed from the brigade"
    );
}

#[test]
fn a_worker_that_dies_again_after_the_cooldown_is_relaunched_again() {
    let (mut state, mut app) = brigade(&["w1"]);
    let start = now();
    exits(&mut state, &mut app, "w1", start);
    send(
        &mut state,
        &mut app,
        Event::Spawned { key: key("w1") },
        start + Duration::from_secs(1),
    );
    let cmds = without_resizes(exits(
        &mut state,
        &mut app,
        "w1",
        start + RELAUNCH_COOLDOWN + Duration::from_secs(1),
    ));
    assert!(matches!(&cmds[..], [Cmd::OpenEmbedded { .. }]), "{cmds:?}");
}

#[test]
fn an_exit_banto_asked_for_removes_nobody_whatever_asked() {
    // Every kill goes through `kill_pane`; none of them is a Worker leaving.
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    let kill = kill_pane(&mut state, key("w1"));
    assert!(matches!(kill, Cmd::KillPty { .. }));
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");

    // Disbanding with the Workers' panes alive: the staged ones are killed ...
    let (mut state, mut app) = brigade(&[]);
    state.pending_dismiss = None;
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        t,
    ));
    assert!(
        cmds.iter().all(|cmd| matches!(cmd, Cmd::KillPty { .. })),
        "{cmds:?}"
    );
    for id in ["w1", "w2"] {
        let cmds = without_resizes(exits(&mut state, &mut app, id, t));
        assert!(cmds.is_empty(), "{id}: {cmds:?}");
    }
}

#[test]
fn panes_of_a_brigade_that_is_being_disbanded_or_gone_leave_nobody() {
    // The confirm was given; the result has not come back yet.
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    app.open_confirm_disband_modal(1, "cell".to_string());
    press(&mut state, &mut app, KeyCode::Enter, t);
    assert!(state.disbanding.contains(&1));
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");

    // A disbanded brigade whose pane was off-stage (nothing killed it): it ends
    // later, and nobody is removed from a brigade that is gone.
    let (mut state, mut app) = brigade(&[]);
    state.stage = Stage::Empty;
    send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Ok((HashSet::new(), HashSet::new())),
        },
        t,
    );
    let cmds = without_resizes(exits(&mut state, &mut app, "w2", t));
    assert!(cmds.is_empty(), "{cmds:?}");

    // A disband that failed: the brigade lives, so a Worker ending is a Worker ending.
    let (mut state, mut app) = brigade(&[]);
    state.disbanding.insert(1);
    send(
        &mut state,
        &mut app,
        Event::Disbanded {
            brigade_id: 1,
            result: Err("busy".to_string()),
        },
        t,
    );
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert_eq!(cmds, vec![removal(1, "worker-1", false)]);
}

#[test]
fn nobody_is_removed_while_banto_is_quitting() {
    // The loop drains what is queued after the quit key; a pane that ends in
    // that queue ends because banto is closing.
    let (mut state, mut app) = brigade(&[]);
    app.request_quit();
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", now()));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(state.removing_exited.is_empty());
}

#[test]
fn a_workers_second_exit_event_or_one_during_an_operator_dismissal_removes_nobody_twice() {
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert_eq!(cmds, vec![removal(1, "worker-1", false)]);
    // The same pane's exit again: the screen is gone, nothing to remove.
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");

    // An operator's dismissal in flight for the other Worker: its pane ends
    // before the store answers, and the dismissal already on its way is enough.
    state.pending_dismiss = Some(key("w2"));
    state.pending_dismiss_member = Some((1, "worker-2".to_string()));
    let cmds = without_resizes(exits(&mut state, &mut app, "w2", t));
    assert!(cmds.is_empty(), "{cmds:?}");

    // The same before the real key's membership round trip has answered: only the
    // pane is known to be leaving, not yet which member it is.
    let (mut state, mut app) = brigade(&[]);
    state.pending_dismiss = Some(key("w1"));
    state.pending_dismiss_member = None;
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn a_worker_of_a_brigade_that_is_not_staged_leaves_the_same_way() {
    let (mut state, mut app) = brigade(&[]);
    state.stage = Stage::Empty;
    let cmds = without_resizes(exits(&mut state, &mut app, "w2", now()));
    assert_eq!(cmds, vec![removal(1, "worker-2", false)]);
}

#[test]
fn the_goinkyo_and_the_director_keep_the_outcome_they_always_had() {
    // The Goinkyo: un-staged, its pane entry dropped, the row kept (a later
    // staging resumes it); nothing is removed.
    let (mut state, mut app) = brigade(&[]);
    let goinkyo = SessionKey::new_worker(1, GOINKYO_TOKEN);
    state.screens.insert(goinkyo.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(goinkyo.clone(), GOINKYO_TOKEN.to_string());
    state.member_brigades.insert(goinkyo.clone(), 1);
    state.goinkyo_pane.insert(1, goinkyo.clone());
    if let Stage::Brigade { panes, .. } = &mut state.stage {
        panes.push(goinkyo.clone());
    }
    let t = now();
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: goinkyo.clone(),
            reason: None,
        },
        t,
    ));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(state.goinkyo_pane.is_empty());
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&goinkyo)));

    // The Director: un-staged, `director` cleared, the row kept.
    let cmds = without_resizes(exits(&mut state, &mut app, "dir", t));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(matches!(
        &state.stage,
        Stage::Brigade { director: None, .. }
    ));
    assert!(
        state
            .status
            .as_deref()
            .unwrap()
            .starts_with("session ended")
    );
}

#[test]
fn a_store_failure_says_so_and_leaves_the_member_where_it_is() {
    let (mut state, mut app) = brigade(&[]);
    state.state_member_name(1, "worker-1", Some("impl"));
    state.resolve_member_names(1);
    let t = now();
    exits(&mut state, &mut app, "w1", t);
    dismissed_answer(
        &mut state,
        &mut app,
        "worker-1",
        false,
        Err("database is locked".to_string()),
        t,
    );
    assert_eq!(
        status(&state),
        "\"impl\" exited \u{2014} could not be removed from the brigade: database is locked"
    );
    // Still a member as far as the core knows: its name is kept, and the removal
    // is no longer in flight.
    assert_eq!(state.member_display_name(1, "worker-1"), Some("impl"));
    assert!(state.removing_exited.is_empty());
}

#[test]
fn a_row_that_was_already_gone_is_forgotten_without_a_word() {
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    exits(&mut state, &mut app, "w1", t);
    let before = status(&state);
    dismissed_answer(&mut state, &mut app, "worker-1", false, answered(false), t);
    assert_eq!(
        status(&state),
        before,
        "nothing was removed, so nothing is claimed"
    );
}

#[test]
fn two_workers_ending_together_are_each_removed_under_their_own_token() {
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    let first = without_resizes(exits(&mut state, &mut app, "w1", t));
    let second = without_resizes(exits(&mut state, &mut app, "w2", t));
    assert_eq!(first, vec![removal(1, "worker-1", false)]);
    assert_eq!(second, vec![removal(1, "worker-2", false)]);
    // The answers arrive in either order and carry their own tokens.
    dismissed_answer(&mut state, &mut app, "worker-2", false, answered(true), t);
    assert_eq!(
        status(&state),
        "worker-2 exited \u{2014} removed from the brigade"
    );
    dismissed_answer(&mut state, &mut app, "worker-1", false, answered(true), t);
    assert_eq!(
        status(&state),
        "worker-1 exited \u{2014} removed from the brigade"
    );
    assert!(state.removing_exited.is_empty());
}

// --- output lost is not an exit --------------------------------------------------
//
// The pane's output stream ending (a reader error) says nothing about the
// process: only the shell's exit report does. These pin what core does with the
// first fact when the second is missing.

fn output_lost(state: &mut EmporiumState, app: &mut App, id: &str, at: Instant) -> Vec<Cmd> {
    send(state, app, Event::PtyOutputLost { key: key(id) }, at)
}

fn opens_a_pane(cmds: &[Cmd]) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
}

fn removes_a_member(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|cmd| {
        matches!(
            cmd,
            Cmd::Store(StoreIntent::DismissExitedWorker { .. } | StoreIntent::DismissWorker { .. })
        )
    })
}

#[test]
fn lost_output_never_removes_a_worker_that_has_no_session_to_resume() {
    // The exact case an exit removes: a Worker that never spoke.
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    let cmds = without_resizes(output_lost(&mut state, &mut app, "w1", t));
    assert!(!removes_a_member(&cmds), "{cmds:?}");
    assert!(state.removing_exited.is_empty());
    // The unreadable pane is closed, and its process asked to end.
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("w1") }]);
    assert!(matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&key("w1"))));
    assert!(!state.screens.contains_key(&key("w1")));
    assert_eq!(
        status(&state),
        "lost the output of worker-1 \u{2014} pane closed; it stays in the brigade"
    );
}

#[test]
fn lost_output_does_not_relaunch_a_worker_that_has_a_session_to_resume() {
    // An exit relaunches this one; lost output must not start a second process
    // while the first may still be running.
    let (mut state, mut app) = brigade(&["w1"]);
    let cmds = without_resizes(output_lost(&mut state, &mut app, "w1", now()));
    assert!(!opens_a_pane(&cmds), "{cmds:?}");
    assert!(!removes_a_member(&cmds), "{cmds:?}");
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("w1") }]);
    assert!(!state.last_relaunch.contains_key(&key("w1")));
}

#[test]
fn lost_output_is_said_by_the_members_quoted_name() {
    let (mut state, mut app) = brigade(&[]);
    state.state_member_name(1, "worker-1", Some("impl"));
    state.resolve_member_names(1);
    output_lost(&mut state, &mut app, "w1", now());
    assert_eq!(
        status(&state),
        "lost the output of \"impl\" \u{2014} pane closed; it stays in the brigade"
    );
}

#[test]
fn the_exit_that_follows_lost_output_removes_nobody_either() {
    // The process may yet end (it was asked to): that report is the asked-for
    // one, not a Worker leaving on its own.
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    output_lost(&mut state, &mut app, "w1", t);
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(state.removing_exited.is_empty());
}

#[test]
fn lost_output_of_the_director_closes_its_pane_and_keeps_the_member() {
    let (mut state, mut app) = brigade(&["w1"]);
    let cmds = without_resizes(output_lost(&mut state, &mut app, "dir", now()));
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("dir") }]);
    assert!(matches!(
        &state.stage,
        Stage::Brigade { director: None, .. }
    ));
    assert!(
        status(&state).starts_with("lost the output of"),
        "{}",
        status(&state)
    );
}

#[test]
fn lost_output_of_the_goinkyo_closes_its_pane_and_removes_nobody() {
    let (mut state, mut app) = brigade(&[]);
    let goinkyo = SessionKey::new_worker(1, GOINKYO_TOKEN);
    state.screens.insert(goinkyo.clone(), Screen::new(24, 80));
    state
        .member_tokens
        .insert(goinkyo.clone(), GOINKYO_TOKEN.to_string());
    state.member_brigades.insert(goinkyo.clone(), 1);
    state.goinkyo_pane.insert(1, goinkyo.clone());
    if let Stage::Brigade { panes, .. } = &mut state.stage {
        panes.push(goinkyo.clone());
    }
    let cmds = without_resizes(send(
        &mut state,
        &mut app,
        Event::PtyOutputLost {
            key: goinkyo.clone(),
        },
        now(),
    ));
    assert_eq!(cmds, vec![Cmd::KillPty { key: goinkyo }]);
    assert!(state.goinkyo_pane.is_empty());
}

#[test]
fn lost_output_of_a_single_session_just_closes_it() {
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("solo"), Screen::new(24, 80));
    state.stage = Stage::Solo(key("solo"));
    let mut app = App::new(vec![row("solo")]);
    let cmds = without_resizes(output_lost(&mut state, &mut app, "solo", now()));
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("solo") }]);
    assert!(!state.screens.contains_key(&key("solo")));
    assert_eq!(
        status(&state),
        "lost the output of solo \u{2014} pane closed"
    );
}

#[test]
fn lost_output_of_a_pane_banto_asked_to_end_is_an_ordinary_asked_for_end() {
    // Dismissal in flight: the pane was killed on purpose, and the kill reached
    // banto as a lost stream. No second kill, no claim that anything was lost.
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    let kill = kill_pane(&mut state, key("w1"));
    assert!(matches!(kill, Cmd::KillPty { .. }));
    let cmds = without_resizes(output_lost(&mut state, &mut app, "w1", t));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(!state.screens.contains_key(&key("w1")));
    assert!(
        !status(&state).starts_with("lost the output"),
        "{}",
        status(&state)
    );
}

#[test]
fn lost_output_of_a_pane_that_is_already_gone_does_nothing_harmful() {
    let (mut state, mut app) = brigade(&[]);
    let t = now();
    output_lost(&mut state, &mut app, "w1", t);
    let cmds = without_resizes(output_lost(&mut state, &mut app, "w1", t));
    assert!(!removes_a_member(&cmds), "{cmds:?}");
    assert!(!opens_a_pane(&cmds), "{cmds:?}");
}

#[test]
fn a_confirmed_exit_still_removes_the_worker_lost_output_would_have_spared() {
    // The pair, side by side: the same Worker, the same state; only the fact
    // reported differs.
    let (mut state, mut app) = brigade(&[]);
    let cmds = without_resizes(exits(&mut state, &mut app, "w1", now()));
    assert!(removes_a_member(&cmds), "{cmds:?}");
}

#[test]
fn lost_output_of_a_director_being_reopened_continues_the_reopen_and_cannot_hang_it() {
    // The reopen waits for the pane to be gone; the pane it killed may reach
    // banto as a lost stream instead of an exit report.
    let mut state = EmporiumState::new(PrefixKey::default());
    state.screens.insert(key("dir"), Screen::new(24, 80));
    let mut app = App::new(vec![row("dir")]);
    app.open_confirm_director_reopen_modal("dir".to_string(), "dir".to_string());
    let cmds = confirm_director_reopen_modal(&mut state, &mut app);
    assert_eq!(cmds, vec![Cmd::KillPty { key: key("dir") }]);

    let cmds = output_lost(&mut state, &mut app, "dir", now());
    assert!(!state.screens.contains_key(&key("dir")));
    assert!(state.pending_director_reopen.is_none());
    assert!(
        matches!(
            cmds.as_slice(),
            [Cmd::Store(StoreIntent::FormBrigade { .. })]
        ),
        "{cmds:?}"
    );
}
