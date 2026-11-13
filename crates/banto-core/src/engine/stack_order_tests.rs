//! The order of the tiles in a brigade's stack, driven through real `Event`s the
//! way the shell feeds them: staging by `MembershipResolved`, arrivals by
//! `Spawned`, the Goinkyo's spawn and discovery, a Worker added or dismissed, a
//! pane exiting, a solo by its button, and the stage moving to another brigade
//! and back.
//!
//! The invariant under test is the operator's: the order of the tiles he sees
//! in a brigade's stack does not change unless he moves a tile — not by the
//! brigade leaving the stage and coming back, not by a restart, not by a
//! member's pane exiting and being relaunched. A member that joins later goes
//! to the bottom.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{MouseButton, MouseEvent, MouseEventKind};
use crate::layout::ButtonKind;
use crate::model::{
    Activity, AgentKind, BrigadeId, BrigadeRole, MemberToken, SessionRow, StoredLayout,
};

use super::*;

pub(super) fn row(id: &str) -> SessionRow {
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

/// A roster entry as the shell hands it over: `(token, role, session id)`.
pub(super) type Member = (&'static str, BrigadeRole, Option<&'static str>);

pub(super) const DIRECTOR: Member = ("director", BrigadeRole::Director, Some("dir"));
pub(super) const WORKER_1: Member = ("worker-1", BrigadeRole::Worker, Some("w1"));
pub(super) const WORKER_2: Member = ("worker-2", BrigadeRole::Worker, Some("w2"));
pub(super) const WORKER_3: Member = ("worker-3", BrigadeRole::Worker, Some("w3"));
pub(super) const WORKER_4: Member = ("worker-4", BrigadeRole::Worker, Some("w4"));
pub(super) const GOINKYO: Member = ("goinkyo", BrigadeRole::Goinkyo, Some("g1"));

/// Whether a freshly opened pane announces itself in roster order or the other
/// way round: which of several concurrent spawns lands first is not something
/// the core controls.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Arrival {
    InOrder,
    Reversed,
}

/// The emporium plus what the shell and the store would hold around it: the
/// last layout snapshot saved for each brigade (what the next staging reads
/// back) and each brigade's members in the order they joined.
pub(super) struct Scene {
    pub(super) state: EmporiumState,
    pub(super) app: App,
    pub(super) brigade: BrigadeConfig,
    pub(super) now: Instant,
    pub(super) saved: HashMap<BrigadeId, StoredLayout>,
    pub(super) joined: HashMap<BrigadeId, Vec<Member>>,
    /// Every step's name and the order it left, for the report.
    pub(super) trace: Vec<(String, Vec<String>)>,
    /// The display names the store holds, per `(brigade, token)`.
    pub(super) names: HashMap<(BrigadeId, String), String>,
}

impl Scene {
    /// Rows `dir`, `w1`..`w4`, `dir2` and `x1` exist; the Goinkyo's `g1` only
    /// when `goinkyo_transcript` (Claude writes none until its first turn).
    pub(super) fn new(goinkyo_transcript: bool) -> Self {
        let mut ids = vec!["dir", "w1", "w2", "w3", "w4", "dir2", "x1"];
        if goinkyo_transcript {
            ids.push("g1");
        }
        let mut scene = Scene {
            state: EmporiumState::new(PrefixKey::default()),
            app: App::new(ids.into_iter().map(row).collect()),
            brigade: BrigadeConfig::default(),
            #[allow(clippy::disallowed_methods)]
            now: Instant::now(),
            saved: HashMap::new(),
            joined: HashMap::new(),
            trace: Vec::new(),
            names: HashMap::new(),
        };
        scene.resize();
        scene
    }

    /// Brigade 1 with a Director and two Workers, brigade 2 with a Director and
    /// one Worker.
    pub(super) fn two_brigades(goinkyo_transcript: bool) -> Self {
        let mut scene = Scene::new(goinkyo_transcript);
        for member in [DIRECTOR, WORKER_1, WORKER_2] {
            scene.join(1, member);
        }
        scene.join(2, ("director", BrigadeRole::Director, Some("dir2")));
        scene.join(2, ("worker-1", BrigadeRole::Worker, Some("x1")));
        scene
    }

    pub(super) fn resize(&mut self) {
        self.feed(Event::Resized {
            width: 120,
            height: 40,
        });
    }

    pub(super) fn feed(&mut self, event: Event) -> Vec<Cmd> {
        let cmds = update(
            &mut self.state,
            &mut self.app,
            &self.brigade,
            event,
            self.now,
        );
        for cmd in &cmds {
            if let Cmd::Store(StoreIntent::SaveBrigadeLayout { brigade_id, layout }) = cmd {
                self.saved.insert(*brigade_id, layout.clone());
            }
        }
        cmds
    }

    /// Answer every `OpenEmbedded` in `cmds` with its `Spawned`.
    pub(super) fn settle(&mut self, cmds: Vec<Cmd>, arrival: Arrival) {
        let mut keys: Vec<SessionKey> = cmds
            .into_iter()
            .filter_map(|cmd| match cmd {
                Cmd::OpenEmbedded { key, .. } => Some(key),
                _ => None,
            })
            .collect();
        if arrival == Arrival::Reversed {
            keys.reverse();
        }
        for key in keys {
            let more = self.feed(Event::Spawned { key });
            self.settle(more, arrival);
        }
    }

    /// The named members of `brigade_id` as the store hands them over.
    pub(super) fn names_of(&self, brigade_id: BrigadeId) -> Vec<(MemberToken, String)> {
        let mut names: Vec<(MemberToken, String)> = self
            .names
            .iter()
            .filter(|((id, _), _)| *id == brigade_id)
            .map(|((_, token), name)| (token.clone(), name.clone()))
            .collect();
        names.sort();
        names
    }

    /// The Director names (or, with `None`, un-names) a member: a store write,
    /// which the core learns of at the next tick or staging.
    pub(super) fn set_name(&mut self, brigade_id: BrigadeId, token: &str, name: Option<&str>) {
        let key = (brigade_id, token.to_string());
        match name {
            Some(name) => self.names.insert(key, name.to_string()),
            None => self.names.remove(&key),
        };
    }

    /// A tick whose observations cover every staged pane of the staged brigade
    /// and whose brigade facts carry the names the store has for its members —
    /// what the shell's gather sends each second.
    pub(super) fn tick_with_names(&mut self) -> Vec<Cmd> {
        let Stage::Brigade { id, panes, .. } = &self.state.stage else {
            return Vec::new();
        };
        let brigade_id = *id;
        let relay: Vec<RelayObservation> = panes
            .iter()
            .filter_map(|key| {
                let token = self.state.member_tokens.get(key)?.clone();
                Some(RelayObservation {
                    brigade_id,
                    token,
                    key: key.clone(),
                    has_unseen: false,
                    is_idle_this_tick: None,
                    agent: None,
                    glance: None,
                })
            })
            .collect();
        let facts = BrigadeNameFacts {
            brigade_id,
            name: "cell".to_string(),
            director_session: Some("dir".to_string()),
            member_names: self.names_of(brigade_id),
        };
        self.feed(Event::Tick {
            relay,
            brigades: vec![facts],
        })
    }

    /// A member joined the brigade in the store, after the ones already there.
    pub(super) fn join(&mut self, brigade_id: BrigadeId, member: Member) {
        self.joined.entry(brigade_id).or_default().push(member);
    }

    /// The roster as the shell reads it for a staging: the Director first, then
    /// the members in the order they joined
    /// (`Store::brigade_members_in_join_order`). Before that query the roster
    /// was ordered by token, which put `goinkyo` ahead of `worker-1` — the
    /// defect the operator saw.
    pub(super) fn roster(
        &self,
        brigade_id: BrigadeId,
    ) -> Vec<(MemberToken, BrigadeRole, Option<String>)> {
        let mut members = self.joined.get(&brigade_id).cloned().unwrap_or_default();
        members.sort_by_key(|(_, role, _)| *role != BrigadeRole::Director);
        members
            .into_iter()
            .map(|(token, role, session)| (token.to_string(), role, session.map(str::to_string)))
            .collect()
    }

    /// Activate the Director of `brigade_id` (session `dir_session`) with the
    /// roster the store has for it now, and let every open that asks for land.
    /// Returns what the update asked of the shell.
    pub(super) fn stage(
        &mut self,
        brigade_id: BrigadeId,
        dir_session: &str,
        arrival: Arrival,
    ) -> Vec<Cmd> {
        self.state.pending_membership = Some(PendingMembership::Activate);
        let cmds = self.feed(Event::MembershipResolved {
            session_id: dir_session.to_string(),
            membership: Some((
                brigade_id,
                DIRECTOR_TOKEN.to_string(),
                BrigadeRole::Director,
            )),
            members: Some(self.roster(brigade_id)),
            layout: self.saved.get(&brigade_id).cloned(),
            names: self.names_of(brigade_id),
        });
        self.settle(cmds.clone(), arrival);
        cmds
    }

    pub(super) fn stage_one(&mut self) {
        self.stage(1, "dir", Arrival::InOrder);
    }

    pub(super) fn stage_two(&mut self) {
        self.stage(2, "dir2", Arrival::InOrder);
    }

    /// The Director asks for a consultation: the shell sees a Goinkyo row with
    /// no session id, the core spawns it, and discovery then names it `g1`.
    pub(super) fn consult_goinkyo(&mut self, brigade_id: BrigadeId) {
        self.join(brigade_id, GOINKYO);
        self.spawn_goinkyo(brigade_id);
    }

    /// The tick finds the Goinkyo row without a session id and reports it,
    /// with the brigade's roster as the store has it now.
    pub(super) fn awaiting_spawn(&mut self, brigade_id: BrigadeId) -> Vec<Cmd> {
        let roster = self
            .roster(brigade_id)
            .into_iter()
            .map(|(token, ..)| token)
            .collect();
        self.feed(Event::GoinkyoAwaitingSpawn {
            observation: GoinkyoObservation::AwaitingSpawn(GoinkyoSpawnCandidate {
                brigade_id,
                cwd: PathBuf::from("/work/alpha"),
                roster,
            }),
        })
    }

    /// The core spawns the Goinkyo whose row has no session id, and discovery
    /// then names it `g1`.
    pub(super) fn spawn_goinkyo(&mut self, brigade_id: BrigadeId) {
        let cmds = self.awaiting_spawn(brigade_id);
        self.settle(cmds, Arrival::InOrder);
        self.feed(Event::DiscoveryResult {
            key: SessionKey::new_worker(brigade_id, GOINKYO_TOKEN),
            session_id: "g1".to_string(),
            member: Some((brigade_id, GOINKYO_TOKEN.to_string())),
        });
    }

    /// The Director dismissed the consultation: the row is gone, the next tick
    /// sees it.
    pub(super) fn end_consultation(&mut self, brigade_id: BrigadeId) {
        self.joined
            .entry(brigade_id)
            .or_default()
            .retain(|(token, ..)| *token != "goinkyo");
        self.feed(Event::GoinkyoAwaitingSpawn {
            observation: GoinkyoObservation::NoGoinkyo { brigade_id },
        });
    }

    /// The operator adds a Worker (`b`): the store numbers it and the core
    /// opens it under a placeholder key. Returns the open, not yet landed.
    pub(super) fn worker_added(&mut self, brigade_id: BrigadeId, member: Member) -> Vec<Cmd> {
        self.join(brigade_id, member);
        self.feed(Event::WorkerAdded {
            brigade_id,
            cwd: PathBuf::from("/work/alpha"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: String::new(),
            result: Ok(member.0.to_string()),
        })
    }

    /// Discovery gives a placeholder-keyed Worker its session.
    pub(super) fn discover(&mut self, brigade_id: BrigadeId, member: Member) {
        self.feed(Event::DiscoveryResult {
            key: SessionKey::new_worker(brigade_id, member.0),
            session_id: member.2.unwrap().to_string(),
            member: Some((brigade_id, member.0.to_string())),
        });
    }

    /// A Worker added and everything about it landed.
    pub(super) fn add_worker(&mut self, brigade_id: BrigadeId, member: Member) {
        let cmds = self.worker_added(brigade_id, member);
        self.settle(cmds, Arrival::InOrder);
        self.discover(brigade_id, member);
    }

    /// The operator dismisses a staged Worker.
    pub(super) fn dismiss(&mut self, brigade_id: BrigadeId, token: &str) {
        let key = self.key_of(token);
        self.state.pending_dismiss = Some(key);
        self.state.pending_dismiss_member = Some((brigade_id, token.to_string()));
        self.names.remove(&(brigade_id, token.to_string()));
        self.joined
            .entry(brigade_id)
            .or_default()
            .retain(|(member, ..)| *member != token);
        self.feed(Event::WorkerDismissed {
            brigade_id,
            result: Ok((HashSet::new(), HashSet::new())),
        });
    }

    /// A member's process ends without banto asking, and whatever the core
    /// answers by opening is let land.
    pub(super) fn exit(&mut self, token: &str) -> Vec<Cmd> {
        let key = self.key_of(token);
        let cmds = self.feed(Event::PtyExited { key, reason: None });
        self.settle(cmds.clone(), Arrival::InOrder);
        cmds
    }

    /// The operator drags the member's tile to slot `to` of the stack.
    pub(super) fn move_tile(&mut self, token: &str, to: usize) {
        let key = self.key_of(token);
        move_tile(&mut self.state, &key, to);
        // The end of the next `update` applies and saves it.
        self.tick();
    }

    pub(super) fn tick(&mut self) -> Vec<Cmd> {
        self.feed(Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        })
    }

    /// Press the member's solo button.
    pub(super) fn press_solo(&mut self, token: &str) {
        let key = self.key_of(token);
        let button = self
            .state
            .geometry(Rect::new(0, 0, 120, 40))
            .buttons()
            .iter()
            .find(|b| b.key == key && b.kind == ButtonKind::Solo)
            .cloned()
            .unwrap_or_else(|| panic!("{token} has a solo button"));
        self.feed(Event::Input(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: button.rect.x + 1,
            row: button.rect.y,
        })));
    }

    pub(super) fn key_of(&self, token: &str) -> SessionKey {
        let Stage::Brigade { panes, .. } = &self.state.stage else {
            panic!("a brigade is staged");
        };
        panes
            .iter()
            .find(|key| self.state.member_tokens.get(key).map(String::as_str) == Some(token))
            .cloned()
            .unwrap_or_else(|| panic!("{token} is staged"))
    }

    /// The staged brigade's tiles, top to bottom (the master first), by member
    /// token — what the operator reads off the screen.
    pub(super) fn order(&self) -> Vec<String> {
        match &self.state.stage {
            Stage::Brigade { panes, .. } => panes
                .iter()
                .map(|key| {
                    self.state
                        .member_tokens
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| format!("?{}", key.as_str()))
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Note the order now, under `step`.
    pub(super) fn note(&mut self, step: &str) -> Vec<String> {
        let order = self.order();
        self.trace.push((step.to_string(), order.clone()));
        order
    }

    /// The store and the shell both stay; the banto process starts over: no
    /// panes, no state, the same rows and the same saved layouts.
    pub(super) fn restart(&mut self) {
        self.state = EmporiumState::new(PrefixKey::default());
        self.resize();
    }
}

/// Leave brigade 1, show brigade 2, and come back to brigade 1.
pub(super) fn off_stage_and_back(scene: &mut Scene) {
    scene.stage_two();
    scene.stage_one();
}

#[track_caller]
fn assert_order(scene: &Scene, expected: &[&str]) {
    assert_eq!(scene.order(), expected, "{:?}", scene.trace);
}

/// What the operator did, as he reported it: stage the brigade, start the
/// Goinkyo, solo it, go to another brigade, come back. The reproduction: the
/// Goinkyo, started last and shown below the Workers, came back at the top.
#[test]
fn the_operators_sequence_keeps_the_goinkyo_below_the_workers() {
    for solo in [true, false] {
        let mut scene = Scene::two_brigades(false);
        scene.stage_one();
        scene.note("1. brigade 1 staged (director, worker-1, worker-2)");
        scene.consult_goinkyo(1);
        scene.note("2. goinkyo started and discovered");
        if solo {
            scene.press_solo("goinkyo");
            scene.note("3. goinkyo soloed");
        }
        scene.stage_two();
        scene.note("4. brigade 2 staged");
        scene.stage_one();
        let last = scene.note("5. brigade 1 staged again");

        for (step, order) in &scene.trace {
            println!("solo={solo} {step}: {order:?}");
        }
        assert_eq!(
            last,
            ["director", "worker-1", "worker-2", "goinkyo"],
            "solo={solo}: the order the operator left is the order he finds"
        );
    }
}

#[test]
fn staging_the_same_brigade_again_while_it_is_staged_changes_nothing() {
    let mut scene = Scene::two_brigades(false);
    scene.stage_one();
    scene.consult_goinkyo(1);
    scene.stage_one();
    assert_order(&scene, &["director", "worker-1", "worker-2", "goinkyo"]);
}

#[test]
fn the_order_never_depends_on_which_pane_arrives_first() {
    for arrival in [Arrival::InOrder, Arrival::Reversed] {
        let mut scene = Scene::two_brigades(true);
        scene.join(1, GOINKYO);
        // Nothing is running: every member opens afresh, concurrently, and the
        // Director may be the last to announce itself.
        scene.stage(1, "dir", arrival);
        assert_order(&scene, &["director", "worker-1", "worker-2", "goinkyo"]);
    }
}

#[test]
fn a_restart_keeps_the_order_whatever_arrives_first() {
    for explicit_move in [false, true] {
        for arrival in [Arrival::InOrder, Arrival::Reversed] {
            let mut scene = Scene::two_brigades(true);
            scene.stage_one();
            scene.consult_goinkyo(1);
            let expected: &[&str] = if explicit_move {
                scene.move_tile("goinkyo", 1);
                &["director", "goinkyo", "worker-1", "worker-2"]
            } else {
                &["director", "worker-1", "worker-2", "goinkyo"]
            };
            assert_order(&scene, expected);

            // The process starts over; the store (roster and saved layout) stays.
            scene.restart();
            scene.stage(1, "dir", arrival);
            assert_order(&scene, expected);
        }
    }
}

#[test]
fn leaving_the_stage_and_coming_back_keeps_a_move_the_operator_made() {
    for solo in [false, true] {
        let mut scene = Scene::two_brigades(true);
        scene.stage_one();
        scene.consult_goinkyo(1);
        scene.move_tile("worker-2", 1);
        if solo {
            scene.press_solo("worker-1");
        }
        assert_order(&scene, &["director", "worker-2", "worker-1", "goinkyo"]);
        off_stage_and_back(&mut scene);
        assert_order(&scene, &["director", "worker-2", "worker-1", "goinkyo"]);
        scene.restart();
        scene.stage_one();
        assert_order(&scene, &["director", "worker-2", "worker-1", "goinkyo"]);
    }
}

#[test]
fn a_pane_exiting_and_being_relaunched_keeps_its_place() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.consult_goinkyo(1);
    let before = scene.order();

    // An agent updated itself: the Worker's process ended, banto restarts it.
    let cmds = scene.exit("worker-1");
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. })),
        "the exit was answered by a relaunch: {cmds:?}"
    );
    assert_eq!(scene.order(), before);

    // Died again at once: no relaunch this time, the tile goes. The brigade
    // comes back with it where it was.
    scene.exit("worker-1");
    assert_order(&scene, &["director", "worker-2", "goinkyo"]);
    scene.stage_one();
    assert_eq!(scene.order(), before);
}

#[test]
fn a_goinkyos_pane_exiting_and_the_brigade_being_staged_again_keeps_its_place() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.consult_goinkyo(1);
    scene.exit("goinkyo");
    assert_order(&scene, &["director", "worker-1", "worker-2"]);
    // Resumed from its own transcript.
    scene.stage_one();
    assert_order(&scene, &["director", "worker-1", "worker-2", "goinkyo"]);
}

#[test]
fn a_member_added_later_goes_to_the_bottom_and_stays_there() {
    for solo in [false, true] {
        let mut scene = Scene::two_brigades(true);
        scene.stage_one();
        scene.consult_goinkyo(1);
        scene.add_worker(1, WORKER_3);
        if solo {
            scene.press_solo("worker-3");
        }
        // It joined after the Goinkyo, so it is below the Goinkyo too.
        let expected = ["director", "worker-1", "worker-2", "goinkyo", "worker-3"];
        assert_order(&scene, &expected);
        off_stage_and_back(&mut scene);
        assert_order(&scene, &expected);
        scene.restart();
        scene.stage_one();
        assert_order(&scene, &expected);
    }
}

#[test]
fn a_goinkyo_consulted_later_goes_below_what_joined_before_it() {
    let mut scene = Scene::two_brigades(false);
    scene.stage_one();
    scene.add_worker(1, WORKER_3);
    assert_order(&scene, &["director", "worker-1", "worker-2", "worker-3"]);
    scene.consult_goinkyo(1);
    let expected = ["director", "worker-1", "worker-2", "worker-3", "goinkyo"];
    assert_order(&scene, &expected);
    off_stage_and_back(&mut scene);
    assert_order(&scene, &expected);
}

#[test]
fn a_member_that_joins_after_a_move_goes_below_the_moved_tiles() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.move_tile("worker-2", 1);
    assert_order(&scene, &["director", "worker-2", "worker-1"]);
    scene.consult_goinkyo(1);
    scene.add_worker(1, WORKER_3);
    let expected = ["director", "worker-2", "worker-1", "goinkyo", "worker-3"];
    assert_order(&scene, &expected);
    off_stage_and_back(&mut scene);
    assert_order(&scene, &expected);
    scene.restart();
    scene.stage_one();
    assert_order(&scene, &expected);
}

#[test]
fn a_token_reused_by_hand_joins_last() {
    // banto's own add-worker never reuses a Worker's token; the Goinkyo's is
    // reused, and a hand-edited store could reuse any. Whoever holds it joins
    // last.
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.consult_goinkyo(1);
    scene.dismiss(1, "worker-2");
    assert_order(&scene, &["director", "worker-1", "goinkyo"]);
    // A member under worker-2 again (by hand), after the Goinkyo.
    scene.add_worker(1, WORKER_2);
    let expected = ["director", "worker-1", "goinkyo", "worker-2"];
    assert_order(&scene, &expected);
    off_stage_and_back(&mut scene);
    assert_order(&scene, &expected);
    scene.restart();
    scene.stage_one();
    assert_order(&scene, &expected);
}

#[test]
fn a_second_consultation_joins_after_whoever_joined_in_between() {
    let mut scene = Scene::two_brigades(false);
    scene.stage_one();
    scene.consult_goinkyo(1);
    scene.end_consultation(1);
    assert_order(&scene, &["director", "worker-1", "worker-2"]);
    scene.add_worker(1, WORKER_3);
    scene.consult_goinkyo(1);
    let expected = ["director", "worker-1", "worker-2", "worker-3", "goinkyo"];
    assert_order(&scene, &expected);
    off_stage_and_back(&mut scene);
    assert_order(&scene, &expected);
}

#[test]
fn a_brigade_nobody_has_arranged_is_shown_in_join_order_and_nothing_is_saved_for_it() {
    let mut scene = Scene::two_brigades(true);
    // What an old brigade looks like: a Goinkyo and a late Worker in the
    // store, no layout ever saved.
    scene.join(1, GOINKYO);
    scene.join(1, WORKER_3);
    let mut cmds = scene.stage(1, "dir", Arrival::Reversed);
    cmds.extend(scene.tick());
    assert_order(
        &scene,
        &["director", "worker-1", "worker-2", "goinkyo", "worker-3"],
    );
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::SaveBrigadeLayout { .. }))),
        "showing a brigade in join order is not an arrangement to remember: {cmds:?}"
    );
}

#[test]
fn a_late_director_does_not_disturb_the_stack() {
    // The Workers announce themselves first and a Worker briefly holds
    // `panes[0]`; the Director then takes the front.
    let mut scene = Scene::two_brigades(true);
    scene.join(1, GOINKYO);
    scene.stage(1, "dir", Arrival::Reversed);
    assert_order(&scene, &["director", "worker-1", "worker-2", "goinkyo"]);
    scene.move_tile("goinkyo", 1);
    scene.restart();
    scene.stage(1, "dir", Arrival::Reversed);
    assert_order(&scene, &["director", "goinkyo", "worker-1", "worker-2"]);
}

#[test]
fn a_goinkyo_respawned_after_a_restart_returns_to_its_place_in_the_join_order() {
    // No transcript: the restarted process cannot resume the Goinkyo, so the
    // store forgets its session and the next tick spawns it afresh, after the
    // Workers have long been back.
    let mut scene = Scene::two_brigades(false);
    scene.stage_one();
    scene.consult_goinkyo(1);
    scene.add_worker(1, WORKER_3);
    scene.restart();
    let cmds = scene.stage(1, "dir", Arrival::InOrder);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::ClearMemberSession { .. }))),
        "{cmds:?}"
    );
    assert_order(&scene, &["director", "worker-1", "worker-2", "worker-3"]);

    scene.spawn_goinkyo(1);
    // It joined before worker-3, and is shown there, not at the bottom.
    assert_order(
        &scene,
        &["director", "worker-1", "worker-2", "goinkyo", "worker-3"],
    );
}

#[test]
fn a_freshly_formed_brigade_shows_its_workers_in_the_order_they_were_numbered() {
    // Both Workers spawn at once and the second may announce itself first;
    // the order must still be the one the store will hand back later.
    for arrival in [Arrival::InOrder, Arrival::Reversed] {
        let mut scene = Scene::new(true);
        let cmds = scene.feed(Event::BrigadeFormed {
            director_row_id: "dir".to_string(),
            name: "cell".to_string(),
            cwd: PathBuf::from("/work/alpha"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: String::new(),
            result: Ok((1, vec!["worker-1".to_string(), "worker-2".to_string()])),
        });
        scene.settle(cmds, arrival);
        // The Director's spawn asks for the Workers; they land the other way
        // round when `Reversed`.
        assert_order(&scene, &["director", "worker-1", "worker-2"]);
        // The store now has them in that order, and a restart shows the same.
        for member in [DIRECTOR, WORKER_1, WORKER_2] {
            scene.join(1, member);
        }
        scene.restart();
        scene.stage(1, "dir", arrival);
        assert_order(&scene, &["director", "worker-1", "worker-2"]);
    }
}

/// Both of these orders must come back the same after leaving the stage and
/// after a restart.
fn assert_stable_across_restage_and_restart(scene: &mut Scene, expected: &[&str]) {
    assert_order(scene, expected);
    off_stage_and_back(scene);
    assert_order(scene, expected);
    scene.restart();
    scene.stage_one();
    assert_order(scene, expected);
}

#[test]
fn two_workers_added_in_a_row_keep_the_order_they_were_numbered_whichever_pane_lands_first() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    // `b` twice: the store creates worker-3 then worker-4 ...
    let first = scene.worker_added(1, WORKER_3);
    let second = scene.worker_added(1, WORKER_4);
    // ... and worker-4's pane is the first to appear.
    scene.settle(second, Arrival::InOrder);
    scene.settle(first, Arrival::InOrder);
    scene.discover(1, WORKER_4);
    scene.discover(1, WORKER_3);
    assert_stable_across_restage_and_restart(
        &mut scene,
        &["director", "worker-1", "worker-2", "worker-3", "worker-4"],
    );
}

#[test]
fn a_consultation_and_a_worker_added_close_together_keep_the_store_order() {
    // The consultation's row is written (by the Director's MCP process, which
    // no event reports) before the Worker is added, but the core hears of the
    // Worker first: the Goinkyo's row is only noticed by the next tick. The
    // roster that tick carries puts the Goinkyo where it joined, and the panes
    // arrive in the other order.
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.join(1, GOINKYO);
    let worker = scene.worker_added(1, WORKER_3);
    let goinkyo = scene.awaiting_spawn(1);
    scene.settle(worker, Arrival::InOrder);
    scene.settle(goinkyo, Arrival::InOrder);
    scene.discover(1, WORKER_3);
    scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, GOINKYO_TOKEN),
        session_id: "g1".to_string(),
        member: Some((1, GOINKYO_TOKEN.to_string())),
    });
    assert_stable_across_restage_and_restart(
        &mut scene,
        &["director", "worker-1", "worker-2", "goinkyo", "worker-3"],
    );
}

#[test]
fn the_other_way_round_a_worker_then_a_consultation_with_panes_reversed() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    let worker = scene.worker_added(1, WORKER_3);
    scene.join(1, GOINKYO);
    let goinkyo = scene.awaiting_spawn(1);
    scene.settle(goinkyo, Arrival::InOrder);
    scene.settle(worker, Arrival::InOrder);
    scene.discover(1, WORKER_3);
    scene.feed(Event::DiscoveryResult {
        key: SessionKey::new_worker(1, GOINKYO_TOKEN),
        session_id: "g1".to_string(),
        member: Some((1, GOINKYO_TOKEN.to_string())),
    });
    assert_stable_across_restage_and_restart(
        &mut scene,
        &["director", "worker-1", "worker-2", "worker-3", "goinkyo"],
    );
}

#[test]
fn a_spawn_candidate_recorded_without_a_roster_still_parses_and_joins_last() {
    let old: GoinkyoSpawnCandidate =
        serde_json::from_str(r#"{"brigade_id":1,"cwd":"/work/alpha"}"#).unwrap();
    assert!(old.roster.is_empty());

    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    let cmds = scene.feed(Event::GoinkyoAwaitingSpawn {
        observation: GoinkyoObservation::AwaitingSpawn(old),
    });
    scene.settle(cmds, Arrival::InOrder);
    assert_order(&scene, &["director", "worker-1", "worker-2", "goinkyo"]);
}
