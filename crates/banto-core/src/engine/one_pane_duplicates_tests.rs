//! The regression in the shapes the operator hit, and the self-healing for
//! duplicates that already exist (see `one_pane_tests`).

use super::one_pane_tests::{DISCOVERED, discovered_unspoken, live_panes, opens};
use super::stack_order_tests::{Arrival, DIRECTOR, Scene, WORKER_2};
use super::*;
use crate::model::BrigadeRole;
use crate::screen::Screen;

/// Every staging after the first opens nothing for a member that has a pane, and
/// the member has exactly one pane throughout.
fn assert_stable_over_stagings(scene: &mut Scene, token: &str, rounds: usize) {
    let before = live_panes(scene, 1, token);
    assert_eq!(before.len(), 1, "{before:?}");
    for round in 0..rounds {
        scene.stage_two();
        let cmds = scene.stage(1, "dir", Arrival::InOrder);
        assert!(
            opens(&cmds).is_empty(),
            "round {round}: staging opened a pane for a member that has one: {cmds:?}"
        );
        assert_eq!(live_panes(scene, 1, token), before, "round {round}");
    }
}

#[test]
fn staging_again_and_again_never_starts_a_second_process_for_a_discovered_unspoken_worker() {
    let mut scene = discovered_unspoken();
    assert_stable_over_stagings(&mut scene, "worker-1", 5);
    assert_eq!(scene.order(), ["director", "worker-1"], "one tile");
}

#[test]
fn a_just_added_worker_that_has_not_spoken_survives_switching_brigades_and_back() {
    let mut scene = Scene::new(false);
    scene.join(1, DIRECTOR);
    scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
    scene.stage(1, "dir", Arrival::InOrder);
    // `b`: the store numbers it, the core opens it under its placeholder key,
    // discovery re-keys it and records the session id; no transcript yet.
    let cmds = scene.worker_added(1, ("worker-1", BrigadeRole::Worker, None));
    scene.settle(cmds, Arrival::InOrder);
    scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, "worker-1"),
        session_id: DISCOVERED.to_string(),
        member: Some((1, "worker-1".to_string())),
    });
    for member in scene.joined.get_mut(&1).unwrap() {
        if member.0 == "worker-1" {
            member.2 = Some(DISCOVERED);
        }
    }
    assert_stable_over_stagings(&mut scene, "worker-1", 4);
}

#[test]
fn after_a_restart_a_worker_with_nothing_to_resume_is_started_once_however_often_it_is_staged() {
    let mut scene = discovered_unspoken();
    scene.restart();
    // No panes exist any more; the roster still records the session id, which has
    // no transcript: the Worker is started fresh, once.
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    let fresh: Vec<_> = opens(&cmds)
        .into_iter()
        .filter(|key| key.as_str() != "dir")
        .collect();
    assert_eq!(fresh.len(), 1, "{cmds:?}");
    assert!(fresh[0].is_synthetic());
    // Discovery finds the new session; the store records it.
    scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, "worker-1"),
        session_id: "nr2".to_string(),
        member: Some((1, "worker-1".to_string())),
    });
    for member in scene.joined.get_mut(&1).unwrap() {
        if member.0 == "worker-1" {
            member.2 = Some("nr2");
        }
    }
    assert_stable_over_stagings(&mut scene, "worker-1", 4);
    assert_eq!(live_panes(&scene, 1, "worker-1"), ["nr2"]);
}

// --- duplicates that already exist --------------------------------------------------

/// Give `(brigade, token)` one more live pane under `id`, spawned now.
fn add_pane(scene: &mut Scene, id: &str, brigade_id: BrigadeId, token: &str) {
    let key = SessionKey::from_id(id);
    scene.state.screens.insert(key.clone(), Screen::new(24, 80));
    scene
        .state
        .member_tokens
        .insert(key.clone(), token.to_string());
    scene.state.member_brigades.insert(key.clone(), brigade_id);
    let seq = scene.state.next_spawn;
    scene.state.spawn_order.insert(key, seq);
    scene.state.next_spawn += 1;
}

fn kills(cmds: &[Cmd]) -> Vec<String> {
    let mut keys: Vec<String> = cmds
        .iter()
        .filter_map(|cmd| match cmd {
            Cmd::KillPty { key } => Some(key.as_str().to_string()),
            _ => None,
        })
        .collect();
    keys.sort();
    keys
}

fn records_a_session(cmds: &[Cmd], token: &str, session: &str) -> bool {
    cmds.iter().any(|cmd| {
        matches!(
            cmd,
            Cmd::Store(StoreIntent::SetMemberSession { token: t, session_id, .. })
                if t == token && session_id == session
        )
    })
}

#[test]
fn duplicates_leave_the_pane_the_roster_records_and_close_the_rest_as_asked_for() {
    let mut scene = discovered_unspoken();
    // Older and newer duplicates, as the operator's machine had.
    add_pane(&mut scene, "older", 1, "worker-1");
    add_pane(&mut scene, "newer", 1, "worker-1");
    scene.stage_two();
    let cmds = scene.stage(1, "dir", Arrival::InOrder);

    // The roster records "nr1": that one stays, in the stage and untouched.
    assert_eq!(kills(&cmds), ["newer", "older"]);
    // Ownership left the duplicates at once (nothing can name them as the member);
    // their screens stay until the exits arrive.
    assert_eq!(live_panes(&scene, 1, "worker-1"), [DISCOVERED]);
    assert!(
        scene
            .state
            .screens
            .contains_key(&SessionKey::from_id("older"))
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert_eq!(scene.order(), ["director", "worker-1"]);
    assert!(
        scene
            .state
            .status
            .as_deref()
            .unwrap()
            .contains("closed 2 duplicate panes of one member")
    );
    // No SetMemberSession: the roster already names the survivor.
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::SetMemberSession { .. })))
    );

    // The killed panes end: asked for, so neither relaunched nor a Worker leaving.
    for id in ["older", "newer"] {
        let after = scene.feed(Event::PtyExited {
            key: SessionKey::from_id(id),
            reason: None,
        });
        assert!(
            !after.iter().any(|cmd| matches!(
                cmd,
                Cmd::OpenEmbedded { .. } | Cmd::Store(StoreIntent::DismissExitedWorker { .. })
            )),
            "{id}: {after:?}"
        );
        // And the shell's reaper condition holds for it: no screen left.
        assert!(!scene.state.screens.contains_key(&SessionKey::from_id(id)));
    }
    assert_eq!(live_panes(&scene, 1, "worker-1"), [DISCOVERED]);
}

#[test]
fn without_a_recorded_match_the_most_recently_spawned_pane_survives_and_is_recorded() {
    let mut scene = Scene::new(false);
    scene.join(1, DIRECTOR);
    scene.join(
        1,
        ("worker-1", BrigadeRole::Worker, Some("recorded-but-gone")),
    );
    scene.stage(1, "dir", Arrival::InOrder);
    // Drop whatever staging opened for the member; set up three live panes by hand.
    for id in live_panes(&scene, 1, "worker-1") {
        let key = SessionKey::from_id(&id);
        scene.state.screens.remove(&key);
        scene.state.member_tokens.remove(&key);
        scene.state.member_brigades.remove(&key);
    }
    add_pane(&mut scene, "a-first", 1, "worker-1");
    add_pane(&mut scene, "z-second", 1, "worker-1");
    add_pane(&mut scene, "m-third", 1, "worker-1");
    scene.stage_two();
    let cmds = scene.stage(1, "dir", Arrival::InOrder);

    assert_eq!(
        kills(&cmds),
        ["a-first", "z-second"],
        "the newest, m-third, stays"
    );
    // The store is told which pane the operator now sees.
    assert!(records_a_session(&cmds, "worker-1", "m-third"), "{cmds:?}");
}

#[test]
fn a_pane_still_under_its_placeholder_key_is_a_duplicate_beside_a_recorded_one() {
    let mut scene = discovered_unspoken();
    // Mid-discovery: a second process under the synthetic key.
    let synthetic = SessionKey::new_worker(1, "worker-1");
    scene
        .state
        .screens
        .insert(synthetic.clone(), Screen::new(24, 80));
    scene
        .state
        .member_tokens
        .insert(synthetic.clone(), "worker-1".to_string());
    scene.state.member_brigades.insert(synthetic.clone(), 1);
    scene.stage_two();
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert_eq!(kills(&cmds), [synthetic.as_str().to_string()]);
}

#[test]
fn duplicates_of_two_members_at_once_are_closed_and_an_off_stage_brigade_is_left_alone() {
    let mut scene = Scene::new(true);
    scene.join(1, DIRECTOR);
    scene.join(1, ("worker-1", BrigadeRole::Worker, Some("w1")));
    scene.join(1, WORKER_2);
    scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
    scene.join(2, ("worker-1", BrigadeRole::Worker, Some("x1")));
    scene.stage(1, "dir", Arrival::InOrder);
    scene.stage_two();
    // Duplicates for both of brigade 1's Workers, and for brigade 2's.
    add_pane(&mut scene, "dup-a", 1, "worker-1");
    add_pane(&mut scene, "dup-b", 1, "worker-2");
    add_pane(&mut scene, "dup-c", 2, "worker-1");
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert_eq!(kills(&cmds).len(), 2, "{cmds:?}");
    assert!(
        scene
            .state
            .status
            .as_deref()
            .unwrap()
            .contains("closed 2 duplicate panes")
    );
    // Brigade 2's, off stage while brigade 1 is staged, are untouched.
    assert!(
        scene
            .state
            .screens
            .contains_key(&SessionKey::from_id("dup-c"))
    );
    assert!(scene.state.screens.contains_key(&SessionKey::from_id("x1")));
}

#[test]
fn members_that_have_a_row_and_one_pane_see_nothing_new() {
    let mut scene = Scene::new(true);
    scene.join(1, DIRECTOR);
    scene.join(1, ("worker-1", BrigadeRole::Worker, Some("w1")));
    scene.stage(1, "dir", Arrival::InOrder);
    scene.stage_two();
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert!(opens(&cmds).is_empty());
    assert!(kills(&cmds).is_empty());
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::SetMemberSession { .. })))
    );
    assert!(
        !scene
            .state
            .status
            .as_deref()
            .unwrap_or("")
            .contains("duplicate")
    );
}

#[test]
fn the_goinkyo_and_the_director_with_no_row_behave_as_they_did() {
    // A Goinkyo with a recorded id and no row: with no live pane its session is
    // cleared so the tick restarts the consultation; a live pane under that id is
    // reused. A Director with no row is never started (counted as not found).
    let mut scene = Scene::new(false);
    scene.join(1, DIRECTOR);
    scene.join(1, ("goinkyo", BrigadeRole::Goinkyo, Some("gx")));
    scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::ClearMemberSession { token, .. }) if token == "goinkyo"
        )),
        "{cmds:?}"
    );

    add_pane(&mut scene, "gx", 1, "goinkyo");
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert!(
        opens(&cmds).is_empty() && kills(&cmds).is_empty(),
        "{cmds:?}"
    );
    assert_eq!(
        scene.state.goinkyo_pane.get(&1),
        Some(&SessionKey::from_id("gx"))
    );

    scene.join(3, ("director", BrigadeRole::Director, Some("ghost")));
    let cmds = scene.stage(3, "ghost", Arrival::InOrder);
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
}
