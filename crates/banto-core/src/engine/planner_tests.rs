//! The Planner's pane: launched by the tick's observation, kept through an exit it
//! did not ask for, brought back by staging the brigade again, and closed from its
//! own tile. Everything here is driven through real `Event`s, with the stack
//! fixture of `stack_order_tests`.
//!
//! The invariant under test is the one the Plan depends on: the Planner's member
//! row and its Plan leave together, in the transaction that closes the Plan. So an
//! exit nobody asked for never removes the row (a Worker's does), and what a dead
//! Planner leaves behind is a pane that is not on stage, not a hole in the roster.

use std::path::PathBuf;

use crate::input::{KeyCode, KeyEvent, Modifiers};
use crate::model::{BrigadeRole, PLANNER_TOKEN};
use crate::plan::{PlanCloseCause, PlanCloseReport, PlanResult};

use super::stack_order_tests::{Arrival, Member, Scene};
use super::*;

/// A Planner member row with no session id yet, as `open_planner` leaves it.
const PLANNER_NEW: Member = ("planner", BrigadeRole::Planner, None);
/// The same Planner once discovery has named its session (`g1`, the one transcript
/// the fixture has beyond the Workers').
const PLANNER: Member = ("planner", BrigadeRole::Planner, Some("g1"));

fn awaiting(scene: &mut Scene, brigade_id: BrigadeId) -> Vec<Cmd> {
    let roster = scene
        .roster(brigade_id)
        .into_iter()
        .map(|(token, ..)| token)
        .collect();
    scene.feed(Event::PlannerObserved {
        observation: PlannerObservation::AwaitingSpawn(PlannerSpawnCandidate {
            brigade_id,
            cwd: PathBuf::from("/work/alpha"),
            roster,
        }),
    })
}

/// The Director's brigade is staged, the Planner's row exists without a session,
/// and the tick has launched it: its pane is on stage under the placeholder key.
fn launched() -> Scene {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.join(1, PLANNER_NEW);
    let cmds = awaiting(&mut scene, 1);
    scene.settle(cmds, Arrival::InOrder);
    scene
}

/// [`launched`], and discovery has named the Planner's session.
fn identified() -> Scene {
    let mut scene = launched();
    scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, PLANNER_TOKEN),
        session_id: "g1".to_string(),
        member: Some((1, PLANNER_TOKEN.to_string())),
    });
    scene
        .joined
        .get_mut(&1)
        .unwrap()
        .retain(|m| m.0 != "planner");
    scene.join(1, PLANNER);
    scene
}

fn opens(cmds: &[Cmd]) -> Vec<&Cmd> {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
        .collect()
}

fn dismissals(cmds: &[Cmd]) -> usize {
    cmds.iter()
        .filter(|cmd| {
            matches!(
                cmd,
                Cmd::Store(
                    StoreIntent::DismissExitedWorker { .. } | StoreIntent::DismissWorker { .. }
                )
            )
        })
        .count()
}

fn look(row: GoinkyoRow, hosted: bool, guard: Option<SessionKey>) -> PlannerLook {
    PlannerLook {
        brigade_id: 1,
        roster_read: true,
        row,
        director_hosted: hosted,
        guard: GoinkyoGuardFacts {
            guard,
            pane: false,
            opening: false,
        },
        cwd: PathBuf::from("/work/alpha"),
        roster: vec!["director".to_string(), "planner".to_string()],
    }
}

// --- the decision ---------------------------------------------------------------

#[test]
fn the_tick_launches_a_planner_only_for_a_hosted_director_and_only_without_a_session() {
    let candidate = PlannerObservation::AwaitingSpawn(PlannerSpawnCandidate {
        brigade_id: 1,
        cwd: PathBuf::from("/work/alpha"),
        roster: vec!["director".to_string(), "planner".to_string()],
    });
    assert_eq!(
        decide_planner(&look(GoinkyoRow::NoSession, true, None)),
        Some(candidate)
    );
    // A brigade nobody opened is not started behind the operator's back.
    assert_eq!(
        decide_planner(&look(GoinkyoRow::NoSession, false, None)),
        None
    );
    // A row with a session is the staging's to bring back, not the tick's.
    assert_eq!(
        decide_planner(&look(GoinkyoRow::Session("p1".into()), true, None)),
        None
    );
}

#[test]
fn a_vanished_planner_row_is_reported_when_the_director_is_hosted_or_a_pane_is_held() {
    let gone = Some(PlannerObservation::NoPlanner { brigade_id: 1 });
    assert_eq!(decide_planner(&look(GoinkyoRow::Absent, true, None)), gone);
    assert_eq!(
        decide_planner(&look(
            GoinkyoRow::Absent,
            false,
            Some(SessionKey::new_worker(1, PLANNER_TOKEN))
        )),
        gone
    );
    assert_eq!(decide_planner(&look(GoinkyoRow::Absent, false, None)), None);
}

#[test]
fn a_failed_roster_read_decides_nothing_for_the_planner() {
    let mut unread = look(GoinkyoRow::Absent, true, None);
    unread.roster_read = false;
    assert_eq!(decide_planner(&unread), None);
}

// --- launch ---------------------------------------------------------------------

#[test]
fn a_planner_is_created_with_its_own_settings_and_a_first_prompt_and_enters_at_the_bottom() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.join(1, PLANNER_NEW);
    let focused_before = match &scene.state.stage {
        Stage::Brigade { focused, .. } => *focused,
        _ => panic!("a brigade is staged"),
    };
    let focus_before = scene.state.focus;

    let cmds = awaiting(&mut scene, 1);
    let opened = opens(&cmds);
    assert_eq!(opened.len(), 1, "{cmds:?}");
    let Cmd::OpenEmbedded {
        key,
        target,
        brigade,
        model,
        effort,
        permission_mode,
        disallowed_tools,
        autocompact,
        first_prompt,
    } = opened[0]
    else {
        unreachable!()
    };
    assert_eq!(key, &SessionKey::new_worker(1, PLANNER_TOKEN));
    assert!(target.id.is_empty() && target.agent == AgentKind::ClaudeCode);
    assert_eq!(
        brigade,
        &Some((1, PLANNER_TOKEN.to_string(), BrigadeRole::Planner))
    );
    assert_eq!(model.as_deref(), Some("opus"));
    assert_eq!(effort.as_deref(), Some("xhigh"));
    assert_eq!(permission_mode.as_deref(), Some("auto"));
    // A Planner is neither compacted early nor stripped of tools by this flag: its
    // tools are its own list (`planner_tools`), named by the shell.
    assert_eq!((disallowed_tools, autocompact), (&None, &None));
    assert_eq!(first_prompt.as_deref(), Some(PLANNER_FIRST_PROMPT));

    scene.settle(cmds, Arrival::InOrder);
    assert_eq!(
        scene.order(),
        ["director", "worker-1", "worker-2", "planner"],
        "{:?}",
        scene.trace
    );
    // It does not take the keyboard from whatever the operator was doing, and is
    // not soloed: the operator is told it started, and goes to it when they choose.
    assert_eq!(scene.state.focus, focus_before);
    match &scene.state.stage {
        Stage::Brigade { focused, .. } => assert_eq!(*focused, focused_before),
        _ => panic!("the brigade is still staged"),
    }
}

#[test]
fn the_launch_options_of_a_planner_are_the_operators_and_an_empty_one_passes_nothing() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.join(1, PLANNER_NEW);
    scene.brigade.claude.planner_model = Some("haiku".to_string());
    scene.brigade.claude.planner_effort = Some(String::new());
    scene.brigade.claude.planner_permission_mode = Some("plan".to_string());
    let cmds = awaiting(&mut scene, 1);
    let Some(Cmd::OpenEmbedded {
        model,
        effort,
        permission_mode,
        ..
    }) = opens(&cmds).first().copied()
    else {
        panic!("{cmds:?}")
    };
    assert_eq!(model.as_deref(), Some("haiku"));
    assert_eq!(effort, &None);
    assert_eq!(permission_mode.as_deref(), Some("plan"));
}

#[test]
fn a_brigade_being_closed_does_not_start_its_planner() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.join(1, PLANNER_NEW);
    scene.state.closing.insert(1, scene.now);
    assert!(opens(&awaiting(&mut scene, 1)).is_empty());
}

// --- an exit nobody asked for ---------------------------------------------------

#[test]
fn a_planner_that_died_before_it_was_named_is_not_created_again_by_the_tick() {
    let mut scene = launched();
    let cmds = scene.exit("planner");
    assert_eq!(dismissals(&cmds), 0, "its row stays: {cmds:?}");
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);

    // The tick still finds a row with no session, and this is the 1 Hz loop the
    // guard exists to stop: nothing is opened however many times it looks.
    for _ in 0..3 {
        assert!(opens(&awaiting(&mut scene, 1)).is_empty());
    }
    assert!(scene.state.planner_pane_for(1).is_some());
}

#[test]
fn staging_the_brigade_again_is_the_way_back_for_a_planner_that_never_started() {
    let mut scene = launched();
    scene.exit("planner");
    assert!(opens(&awaiting(&mut scene, 1)).is_empty());

    scene.stage(1, "dir", Arrival::InOrder);
    assert!(
        scene.state.planner_pane_for(1).is_none(),
        "the guard is lifted"
    );
    let cmds = awaiting(&mut scene, 1);
    let Some(Cmd::OpenEmbedded { first_prompt, .. }) = opens(&cmds).first().copied() else {
        panic!("a Planner that never talked is created again: {cmds:?}")
    };
    // Created, with the first prompt: nothing was said that a resume would keep.
    assert_eq!(first_prompt.as_deref(), Some(PLANNER_FIRST_PROMPT));
}

#[test]
fn a_named_planner_that_exits_is_restarted_in_place_as_a_planner() {
    let mut scene = identified();
    let cmds = scene.exit("planner");
    let Some(Cmd::OpenEmbedded {
        target,
        brigade,
        model,
        effort,
        permission_mode,
        first_prompt,
        ..
    }) = opens(&cmds).first().copied()
    else {
        panic!("restarted: {cmds:?}")
    };
    assert_eq!(target.id, "g1");
    assert_eq!(
        brigade,
        &Some((1, PLANNER_TOKEN.to_string(), BrigadeRole::Planner))
    );
    // A resume keeps what the session last used, except what is the role's.
    assert_eq!((model, effort, first_prompt), (&None, &None, &None));
    assert_eq!(permission_mode.as_deref(), Some("auto"));
    assert_eq!(
        scene.state.displayed_status(),
        Some("the Planner exited \u{2014} restarting")
    );
    assert_eq!(
        scene.order(),
        ["director", "worker-1", "worker-2", "planner"]
    );
}

#[test]
fn a_planner_that_keeps_dying_loses_its_tile_and_keeps_its_row_and_its_plan() {
    let mut scene = identified();
    scene.exit("planner");
    // Died again at once: a Worker's row would go here. The Planner's row leaves
    // only with its Plan, so only the pane goes.
    let cmds = scene.exit("planner");
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert_eq!(dismissals(&cmds), 0, "{cmds:?}");
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);
    assert!(
        scene
            .state
            .member_order
            .get(&1)
            .is_some_and(|joined| joined.iter().any(|t| t == PLANNER_TOKEN)),
        "still on the roster the core holds"
    );
}

#[test]
fn a_planner_that_could_not_be_restarted_keeps_its_row_too() {
    let mut scene = identified();
    let cmds = scene.feed(Event::PtyExited {
        key: scene.key_of("planner"),
        reason: None,
    });
    let key = match opens(&cmds).first().copied() {
        Some(Cmd::OpenEmbedded { key, .. }) => key.clone(),
        other => panic!("{other:?}"),
    };
    let after = scene.feed(Event::SpawnFailed {
        key,
        error: "no such file".to_string(),
        refusal: SpawnRefusal::PtyOpen,
        os_error: None,
    });
    assert_eq!(dismissals(&after), 0, "{after:?}");
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);
}

// --- the Plan ending ------------------------------------------------------------

#[test]
fn a_planner_row_that_vanished_takes_its_pane_with_it() {
    let mut scene = identified();
    scene
        .joined
        .get_mut(&1)
        .unwrap()
        .retain(|member| member.0 != "planner");
    let cmds = scene.feed(Event::PlannerObserved {
        observation: PlannerObservation::NoPlanner { brigade_id: 1 },
    });
    assert!(
        cmds.iter().any(|cmd| matches!(cmd, Cmd::HangUpPty { .. })),
        "{cmds:?}"
    );
    assert!(cmds.contains(&Cmd::Reload), "{cmds:?}");
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);
    assert!(scene.state.planner_pane_for(1).is_none());

    // Said once: the next tick finds nothing left to release.
    let again = scene.feed(Event::PlannerObserved {
        observation: PlannerObservation::NoPlanner { brigade_id: 1 },
    });
    assert!(again.is_empty(), "{again:?}");
}

fn report(cause: PlanCloseCause, export: Result<&str, &str>) -> PlanCloseReport {
    PlanCloseReport {
        cause,
        number: 2,
        result: PlanResult::Abandoned,
        export: export.map(str::to_string).map_err(str::to_string),
    }
}

#[test]
fn the_operators_close_ends_the_pane_at_once_and_says_where_the_plan_went() {
    let mut scene = identified();
    let cmds = scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Ok(Some(report(
            PlanCloseCause::Operator,
            Ok("/data/plans/1-2.md"),
        ))),
    });
    assert!(
        cmds.iter().any(|cmd| matches!(cmd, Cmd::HangUpPty { .. })),
        "{cmds:?}"
    );
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);
    let status = scene.state.displayed_status().unwrap().to_string();
    assert!(
        status.contains("Plan 2 closed (abandoned)") && status.contains("/data/plans/1-2.md"),
        "{status}"
    );
}

#[test]
fn a_failed_export_is_said_and_never_undoes_the_close() {
    let mut scene = identified();
    scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Ok(Some(report(PlanCloseCause::Operator, Err("disk full")))),
    });
    assert_eq!(scene.order(), ["director", "worker-1", "worker-2"]);
    let status = scene.state.displayed_status().unwrap().to_string();
    assert!(
        status.contains("closed") && status.contains("disk full"),
        "{status}"
    );
}

#[test]
fn a_close_the_store_refused_or_found_nothing_to_close_leaves_the_pane_and_says_so() {
    let mut scene = identified();
    let cmds = scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Err("database is locked".to_string()),
    });
    assert!(cmds.is_empty());
    assert_eq!(
        scene.order(),
        ["director", "worker-1", "worker-2", "planner"]
    );
    assert!(
        scene
            .state
            .displayed_status()
            .unwrap()
            .contains("database is locked")
    );

    let cmds = scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Ok(None),
    });
    assert!(cmds.is_empty());
    assert_eq!(
        scene.state.displayed_status(),
        Some("the brigade has no open Plan to close")
    );
}

#[test]
fn a_disbanded_brigades_plan_says_nothing_unless_its_export_failed() {
    let mut scene = identified();
    let quiet = scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Ok(Some(report(
            PlanCloseCause::Disband,
            Ok("/data/plans/1-2.md"),
        ))),
    });
    // The disband closed the panes already, and had its own line.
    assert!(quiet.is_empty(), "{quiet:?}");
    assert_eq!(
        scene.order(),
        ["director", "worker-1", "worker-2", "planner"]
    );

    scene.state.status = Some("brigade disbanded".to_string());
    scene.feed(Event::PlannerClosed {
        brigade_id: 1,
        result: Ok(Some(report(PlanCloseCause::Disband, Err("disk full")))),
    });
    let status = scene.state.displayed_status().unwrap().to_string();
    assert!(
        status.contains("brigade disbanded") && status.contains("disk full"),
        "{status}"
    );
}

// --- prefix-x on the Planner's tile ---------------------------------------------

fn focus_planner(scene: &mut Scene) {
    let planner = scene.key_of("planner");
    if let Stage::Brigade { panes, focused, .. } = &mut scene.state.stage {
        *focused = panes.iter().position(|key| key == &planner).unwrap();
    }
    scene.state.focus = Focus::Pane;
}

fn press(scene: &mut Scene, code: KeyCode) -> Vec<Cmd> {
    scene.feed(Event::Input(InputEvent::Key(KeyEvent::new(
        code,
        Modifiers::NONE,
    ))))
}

fn prefix_x(scene: &mut Scene) -> Vec<Cmd> {
    scene.state.prefix_armed = Some(scene.now);
    press(scene, KeyCode::Char('x'))
}

#[test]
fn prefix_x_on_the_planner_asks_to_close_its_plan_and_enter_closes_it() {
    let mut scene = identified();
    focus_planner(&mut scene);
    assert!(prefix_x(&mut scene).is_empty());
    assert!(
        matches!(
            scene.app.modal(),
            Some(Modal::ConfirmClosePlanner { brigade_id: 1, .. })
        ),
        "no such modal is open"
    );
    // Nothing is dismissed by asking.
    assert!(scene.state.pending_dismiss.is_none());

    let cmds = press(&mut scene, KeyCode::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Store(StoreIntent::ClosePlanner { brigade_id: 1 })]
    );
    assert!(scene.app.modal().is_none());
    // The pane is still there until the store has answered: a close that failed
    // must not leave a Planner with no tile.
    assert_eq!(
        scene.order(),
        ["director", "worker-1", "worker-2", "planner"]
    );
}

#[test]
fn escape_leaves_the_planner_and_its_plan_alone() {
    let mut scene = identified();
    focus_planner(&mut scene);
    prefix_x(&mut scene);
    let cmds = press(&mut scene, KeyCode::Esc);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(scene.app.modal().is_none());
}

#[test]
fn a_plan_is_not_closed_under_a_brigade_that_is_already_closing() {
    let mut scene = identified();
    focus_planner(&mut scene);
    prefix_x(&mut scene);
    scene.state.closing.insert(1, scene.now);
    let cmds = press(&mut scene, KeyCode::Enter);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(
        scene
            .state
            .displayed_status()
            .unwrap()
            .contains("its Plan was left as it is")
    );
}

#[test]
fn prefix_x_on_a_worker_is_still_the_dismissal() {
    let mut scene = identified();
    let worker = scene.key_of("worker-1");
    if let Stage::Brigade { panes, focused, .. } = &mut scene.state.stage {
        *focused = panes.iter().position(|key| key == &worker).unwrap();
    }
    scene.state.focus = Focus::Pane;
    prefix_x(&mut scene);
    assert!(
        matches!(scene.app.modal(), Some(Modal::ConfirmKill { .. })),
        "no such modal is open"
    );
}
