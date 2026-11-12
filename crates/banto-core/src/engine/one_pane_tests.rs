//! A member has at most one live pane, and staging a brigade never opens a pane
//! for a member that already has one, whatever key that pane is under: a
//! member's identity is `(brigade, token)`, not its key.
//!
//! The defect these pin: a Worker that has a recorded session id but never
//! produced a transcript has no session row, so staging looked for it only under
//! its placeholder key — where it no longer lives once discovery re-keyed it —
//! and started a second process, leaving the first running off-stage.

use super::stack_order_tests::{Arrival, DIRECTOR, Member, Scene, WORKER_2, off_stage_and_back};
use super::*;
use crate::model::BrigadeRole;

/// A Worker the store knows by token only: no session id yet.
const UNSPOKEN: Member = ("worker-1", BrigadeRole::Worker, None);
/// The session id discovery finds for it — which has no transcript, so no row.
pub(super) const DISCOVERED: &str = "nr1";

/// The live panes core holds for `(brigade, token)`: a screen under a key whose
/// ownership maps name that member.
pub(super) fn live_panes(scene: &Scene, brigade_id: BrigadeId, token: &str) -> Vec<String> {
    let mut keys: Vec<String> = scene
        .state
        .screens
        .keys()
        .filter(|key| {
            scene.state.member_brigades.get(*key) == Some(&brigade_id)
                && scene.state.member_tokens.get(*key).map(String::as_str) == Some(token)
        })
        .map(|key| key.as_str().to_string())
        .collect();
    keys.sort();
    keys
}

pub(super) fn opens(cmds: &[Cmd]) -> Vec<&SessionKey> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            Cmd::OpenEmbedded { key, .. } => Some(key),
            _ => None,
        })
        .collect()
}

/// Brigade 1 with the Director and one Worker that has not spoken, staged,
/// discovered (a session id recorded, no transcript) — and the store updated the
/// way `SetMemberSession` would.
pub(super) fn discovered_unspoken() -> Scene {
    let mut scene = Scene::new(false);
    scene.join(1, DIRECTOR);
    scene.join(1, UNSPOKEN);
    scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
    scene.stage(1, "dir", Arrival::InOrder);
    assert_eq!(live_panes(&scene, 1, "worker-1").len(), 1);
    let cmds = scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, "worker-1"),
        session_id: DISCOVERED.to_string(),
        member: Some((1, "worker-1".to_string())),
    });
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::SetMemberSession { session_id, .. }) if session_id == DISCOVERED
        )),
        "discovery records the session id: {cmds:?}"
    );
    // The store's row now holds it; the session list still has no row for it.
    for member in scene.joined.get_mut(&1).unwrap() {
        if member.0 == "worker-1" {
            member.2 = Some(DISCOVERED);
        }
    }
    assert!(scene.app.row_for_id(DISCOVERED).is_none());
    scene
}

#[test]
fn reproduction_a_worker_with_a_recorded_session_but_no_transcript_is_started_again_each_staging() {
    let mut scene = discovered_unspoken();
    let first = live_panes(&scene, 1, "worker-1");
    assert_eq!(first, [DISCOVERED], "the pane now lives under its real id");

    // Away and back, as often as the operator likes.
    let mut extra_opens = 0;
    for _ in 0..3 {
        scene.stage_two();
        let cmds = scene.stage(1, "dir", Arrival::InOrder);
        extra_opens += opens(&cmds)
            .iter()
            .filter(|key| key.as_str() != "dir")
            .count();
    }
    let panes = live_panes(&scene, 1, "worker-1");
    println!("opens after the first staging: {extra_opens}; live panes now: {panes:?}");
    assert_eq!(
        extra_opens, 0,
        "no second process for a member that has one"
    );
    assert_eq!(panes.len(), 1, "one pane for one member: {panes:?}");
}

#[test]
fn a_worker_with_a_session_row_is_not_started_again() {
    // The same sequence for a Worker that has spoken: already fine.
    let mut scene = Scene::new(false);
    scene.join(1, DIRECTOR);
    scene.join(1, ("worker-2", BrigadeRole::Worker, Some("w2")));
    scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
    scene.stage(1, "dir", Arrival::InOrder);
    for _ in 0..3 {
        scene.stage_two();
        let cmds = scene.stage(1, "dir", Arrival::InOrder);
        assert!(opens(&cmds).is_empty(), "{cmds:?}");
    }
    assert_eq!(live_panes(&scene, 1, "worker-2"), ["w2"]);
    let _ = (WORKER_2, off_stage_and_back);
}
