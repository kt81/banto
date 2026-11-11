//! A member's display name in the emporium: it reaches the core through events
//! only (the roster at staging, each member's observation in a tick), changes
//! what the operator reads — the tile's title — and nothing else. Layout, order,
//! solo and collapse stay keyed by the token and are left alone.

use super::stack_order_tests::{
    Arrival, GOINKYO, Scene, WORKER_1, WORKER_2, WORKER_3, off_stage_and_back,
};
use super::*;
use crate::layout::{ButtonKind, title_columns};

fn title(scene: &Scene, token: &str) -> String {
    scene.state.tile_label(&scene.key_of(token))
}

/// The geometry facts a rename must not touch: every tile's rectangle, the
/// order, the layout preferences, and what is collapsed.
/// Tile rectangles, order, collapsed flags and the layout preferences.
type Arrangement = (Vec<(String, Rect)>, Vec<String>, Vec<bool>, String);

fn arrangement(scene: &Scene) -> Arrangement {
    let area = Rect::new(0, 0, 120, 40);
    let tiles = scene
        .state
        .geometry(area)
        .tiles
        .iter()
        .map(|(key, rect)| (key.as_str().to_string(), *rect))
        .collect();
    (
        tiles,
        scene.order(),
        scene.state.stack_collapsed(),
        format!("{:?}", scene.state.brigade_layouts),
    )
}

fn staged() -> Scene {
    let mut scene = Scene::two_brigades(true);
    scene.join(1, GOINKYO);
    scene.stage(1, "dir", Arrival::InOrder);
    scene
}

#[test]
fn an_unnamed_member_keeps_its_plain_token_title() {
    let scene = staged();
    assert_eq!(title(&scene, "worker-1"), "worker-1");
    assert_eq!(title(&scene, "worker-2"), "worker-2");
    assert_eq!(title(&scene, "director"), "director");
    assert_eq!(title(&scene, "goinkyo"), "goinkyo");
}

#[test]
fn a_name_arriving_on_a_tick_changes_the_title_and_nothing_else() {
    let mut scene = staged();
    // The operator's own arrangement, so there is something a rename could
    // disturb: a moved tile, a collapsed one, a solo, a stored height.
    scene.move_tile("worker-2", 1);
    scene.press_solo("goinkyo");
    scene.tick();
    let before = arrangement(&scene);
    let saved_before = scene.saved.clone();

    scene.set_name(1, "worker-2", Some("reviewer"));
    let cmds = scene.tick_with_names();

    assert_eq!(title(&scene, "worker-2"), "reviewer (worker-2)");
    assert_eq!(title(&scene, "worker-1"), "worker-1");
    assert_eq!(
        arrangement(&scene),
        before,
        "no tile moved, resized or collapsed"
    );
    assert!(
        !cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::ResizePty { .. } | Cmd::Store(StoreIntent::SaveBrigadeLayout { .. })
        )),
        "a rename resizes no pane and writes no layout: {cmds:?}"
    );
    assert_eq!(scene.saved, saved_before, "the saved layout is untouched");
}

#[test]
fn a_changed_and_a_cleared_name_follow_the_store_within_a_tick() {
    let mut scene = staged();
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();
    assert_eq!(title(&scene, "worker-1"), "impl (worker-1)");

    scene.set_name(1, "worker-1", Some("docs"));
    scene.tick_with_names();
    assert_eq!(title(&scene, "worker-1"), "docs (worker-1)");

    scene.set_name(1, "worker-1", None);
    scene.tick_with_names();
    assert_eq!(title(&scene, "worker-1"), "worker-1");
}

#[test]
fn a_staging_shows_the_names_the_roster_carries_and_replaces_older_ones() {
    let mut scene = Scene::two_brigades(true);
    scene.set_name(1, "worker-1", Some("impl"));
    scene.stage_one();
    assert_eq!(title(&scene, "worker-1"), "impl (worker-1)");

    // Named differently while the brigade was off the stage: the next staging
    // is the truth, including a name taken back.
    scene.stage_two();
    scene.set_name(1, "worker-1", None);
    scene.set_name(1, "worker-2", Some("docs"));
    scene.stage_one();
    assert_eq!(title(&scene, "worker-1"), "worker-1");
    assert_eq!(title(&scene, "worker-2"), "docs (worker-2)");
}

#[test]
fn the_names_never_reach_the_layout_the_order_or_the_store_writes() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    let order = scene.order();
    scene.set_name(1, "worker-1", Some("impl"));
    scene.set_name(1, "worker-2", Some("docs"));
    let cmds = scene.tick_with_names();
    assert_eq!(scene.order(), order);
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::Store(_))),
        "{cmds:?}"
    );
    assert!(
        scene
            .state
            .brigade_layouts
            .get(&1)
            .is_none_or(|layout| { *layout == BrigadeLayout::default() })
    );
    // Moving a named tile still ranks by token.
    scene.move_tile("worker-2", 1);
    assert_eq!(
        scene.state.brigade_layouts[&1].stack_order,
        ["worker-2", "worker-1"]
    );
}

#[test]
fn a_name_is_forgotten_with_its_member_and_with_its_brigade() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.set_name(1, "worker-2", Some("docs"));
    scene.tick_with_names();
    assert_eq!(scene.state.member_display_name(1, "worker-2"), Some("docs"));

    // Dismissed: gone from the core at once, not at the next tick.
    scene.dismiss(1, "worker-2");
    assert_eq!(scene.state.member_display_name(1, "worker-2"), None);
    // A member under the same token — arranged by hand: banto's own add-worker
    // would number worker-3, and the Goinkyo's token is the one that is really
    // reused — starts unnamed.
    scene.add_worker(1, WORKER_2);
    scene.tick_with_names();
    assert_eq!(title(&scene, "worker-2"), "worker-2");

    // Disbanded: every name of the brigade goes.
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();
    assert_eq!(scene.state.member_display_name(1, "worker-1"), Some("impl"));
    scene.feed(Event::Disbanded {
        brigade_id: 1,
        result: Ok((HashSet::new(), HashSet::new())),
    });
    assert_eq!(scene.state.member_display_name(1, "worker-1"), None);
}

#[test]
fn a_name_that_fails_the_rule_is_not_shown_whatever_the_stream_says() {
    // A recorded stream, or a store the rule never saw.
    let mut scene = staged();
    for bad in [
        "  padded ",
        "esc\u{1b}[31m",
        "quote\"d",
        "worker-3",
        &"x".repeat(25),
    ] {
        scene.set_name(1, "worker-1", Some(bad));
        scene.tick_with_names();
        assert_eq!(title(&scene, "worker-1"), "worker-1", "{bad:?}");
    }
}

#[test]
fn a_stream_recorded_before_names_existed_still_parses() {
    let tick = r#"{"Tick":{"relay":[{"brigade_id":1,"token":"worker-1","key":"w1",
        "has_unseen":false,"is_idle_this_tick":null}]}}"#;
    let Event::Tick { relay, .. } = serde_json::from_str::<Event>(tick).unwrap() else {
        panic!("a tick");
    };
    assert_eq!(relay.len(), 1);
    // A stream recorded while the name rode on the observation still parses (the
    // field is ignored), and a brigade fact recorded before names existed has none.
    let with_old_field = r#"{"Tick":{"relay":[{"brigade_id":1,"token":"worker-1","key":"w1",
        "has_unseen":false,"is_idle_this_tick":null,"display_name":"impl"}]}}"#;
    assert!(serde_json::from_str::<Event>(with_old_field).is_ok());
    let facts: BrigadeNameFacts =
        serde_json::from_str(r#"{"brigade_id":1,"name":"cell","director_session":null}"#).unwrap();
    assert!(facts.member_names.is_empty());

    let resolved =
        r#"{"MembershipResolved":{"session_id":"dir","membership":null,"members":null}}"#;
    let Event::MembershipResolved { names, .. } = serde_json::from_str::<Event>(resolved).unwrap()
    else {
        panic!("a resolution");
    };
    assert!(names.is_empty());

    // And named brigade facts round-trip.
    let named = BrigadeNameFacts {
        brigade_id: 1,
        name: "cell".to_string(),
        director_session: None,
        member_names: vec![("worker-1".to_string(), "impl".to_string())],
    };
    let json = serde_json::to_string(&named).unwrap();
    assert_eq!(
        serde_json::from_str::<BrigadeNameFacts>(&json).unwrap(),
        named
    );
}

/// The title row of the stack tile `token` at `area`: its tile, the cells its
/// label occupies, and whether it has buttons.
fn title_row(scene: &Scene, token: &str, area: Rect) -> (Rect, Option<(u16, u16)>, bool) {
    let key = scene.key_of(token);
    let geometry = scene.state.geometry(area);
    let tile = geometry
        .tiles
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, rect)| *rect)
        .unwrap();
    let label_cells: Vec<u16> = (tile.x..tile.x + tile.width)
        .filter(|column| geometry.label_at(*column, tile.y) == Some(&key))
        .collect();
    let span = label_cells
        .first()
        .zip(label_cells.last())
        .map(|(a, b)| (*a, *b + 1));
    let buttons = geometry.buttons().iter().any(|b| b.key == key);
    (tile, span, buttons)
}

#[test]
fn a_long_name_keeps_the_label_cells_clear_of_handles_and_costs_the_buttons_their_room_by_the_usual_rule()
 {
    let mut scene = staged();
    let area = Rect::new(0, 0, 120, 40);
    let (tile, span, buttons) = title_row(&scene, "worker-1", area);
    assert!(buttons, "an unnamed token leaves the buttons their room");
    let plain_width = span.map(|(a, b)| b - a).unwrap();

    // The longest name there is: 24 columns, then ` · worker-1`.
    scene.set_name(1, "worker-1", Some(&"n".repeat(24)));
    scene.tick_with_names();
    let label = title(&scene, "worker-1");
    assert_eq!(label, format!("{} (worker-1)", "n".repeat(24)));
    let (tile_after, span_after, buttons_after) = title_row(&scene, "worker-1", area);
    assert_eq!(tile_after, tile, "the tile itself is where it was");
    let (start, end) = span_after.expect("the label is a handle for moving the tile");
    // The label is exactly the drawn text, clipped to the tile's inner width.
    assert_eq!((start, end), title_columns(tile, &label));
    assert!(end - start > plain_width);
    assert!(end < tile.x + tile.width, "clear of the corner");
    // 37 columns of label plus a gap plus two buttons do not fit in this
    // tile's inner width: the buttons give way, as they always did.
    let inner = tile.width - 2;
    assert!(inner < (end - start) + 1 + 2 * 3);
    assert!(!buttons_after);
    // No resize handle sits on any cell of the label (the move handle wins there).
    for column in start..end {
        assert!(
            scene
                .state
                .geometry(area)
                .handle_at(column, tile.y)
                .is_none(),
            "column {column}"
        );
    }
    // A short name leaves the buttons their room, and they stay clear of the label.
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();
    let (_, short_span, short_buttons) = title_row(&scene, "worker-1", area);
    assert!(short_buttons);
    let geometry = scene.state.geometry(area);
    let key = scene.key_of("worker-1");
    let first_button = geometry
        .buttons()
        .iter()
        .filter(|b| b.key == key)
        .map(|b| b.rect.x)
        .min()
        .unwrap();
    assert!(short_span.unwrap().1 < first_button);
    assert!(
        geometry
            .buttons()
            .iter()
            .any(|b| b.kind == ButtonKind::Solo)
    );
}

#[test]
fn a_narrow_tile_degrades_as_it_did_with_a_long_title() {
    let mut scene = staged();
    scene.set_name(1, "worker-1", Some("a rather long name"));
    scene.tick_with_names();
    for width in [60u16, 50, 44] {
        scene.feed(Event::Resized { width, height: 40 });
        let area = Rect::new(0, 0, width, 40);
        let (tile, span, buttons) = title_row(&scene, "worker-1", area);
        // Whatever is left of the label stays inside the tile, between its corners.
        if let Some((start, end)) = span {
            assert!(start > tile.x && end < tile.x + tile.width, "width {width}");
            assert_eq!(
                (start, end),
                title_columns(tile, &scene.state.tile_label(&scene.key_of("worker-1")))
            );
        }
        if buttons {
            // A button never overlaps the label cells.
            let geometry = scene.state.geometry(area);
            let key = scene.key_of("worker-1");
            let first = geometry
                .buttons()
                .iter()
                .filter(|b| b.key == key)
                .map(|b| b.rect.x)
                .min();
            assert!(span.is_none_or(|(_, end)| first.is_some_and(|first| end < first)));
        }
    }
}

#[test]
fn a_moved_named_tile_follows_the_pointer_with_its_full_label() {
    // The label drawn at the pointer during a move is the tile's own label.
    let mut scene = staged();
    scene.set_name(1, "worker-2", Some("docs"));
    scene.tick_with_names();
    let key = scene.key_of("worker-2");
    scene.state.move_drag = Some(MoveDrag {
        key: key.clone(),
        row: 10,
        column: 60,
        dragged: true,
    });
    let overlay = scene.state.drag_overlay(Rect::new(0, 0, 120, 40));
    let ghost = overlay.and_then(|o| o.ghost).map(|ghost| ghost.text);
    assert_eq!(ghost.as_deref(), Some(" docs (worker-2) "));
}

#[test]
fn naming_changes_nothing_about_the_joins_or_the_stack_across_a_restage() {
    let mut scene = staged();
    scene.add_worker(1, WORKER_3);
    scene.set_name(1, "worker-3", Some("docs"));
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();
    let order = scene.order();
    off_stage_and_back(&mut scene);
    assert_eq!(scene.order(), order);
    scene.restart();
    scene.stage_one();
    assert_eq!(scene.order(), order);
    assert_eq!(title(&scene, "worker-3"), "docs (worker-3)");
    let _ = (WORKER_1, WORKER_2);
}

fn status(scene: &Scene) -> String {
    scene
        .state
        .displayed_status()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn the_exit_the_added_and_the_directory_statuses_use_the_name_when_there_is_one() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();

    // A pane that exited and is being restarted.
    scene.exit("worker-1");
    assert_eq!(status(&scene), "worker \"impl\" exited \u{2014} restarting");
    scene.exit("worker-2");
    assert_eq!(status(&scene), "worker worker-2 exited \u{2014} restarting");

    // A Codex Worker whose briefing was not confirmed, and a Claude Worker
    // whose directory is not trusted.
    let key = scene.key_of("worker-1");
    scene.feed(Event::CodexWorkerDiscoveryTimedOut {
        key,
        token: "worker-1".to_string(),
    });
    assert_eq!(status(&scene), "\"impl\": Codex briefing wasn't confirmed");
    scene.feed(Event::ClaudeWorkerDirectoryUntrusted {
        token: "worker-1".to_string(),
    });
    assert!(status(&scene).starts_with("\"impl\": Claude hasn't been told to trust"));
    scene.feed(Event::ClaudeWorkerDirectoryUntrusted {
        token: "worker-2".to_string(),
    });
    assert!(status(&scene).starts_with("worker-2: Claude hasn't been told to trust"));

    // A Worker just added has no name yet: its token.
    scene.add_worker(1, WORKER_3);
    assert_eq!(status(&scene), "worker-3 added");
}

fn shown(scene: &Scene, token: &str) -> Option<String> {
    scene
        .state
        .member_display_name(1, token)
        .map(str::to_string)
}

/// A tick whose brigade facts state `named` — the brigade's FULL list of named
/// members, in that order (an empty list says nobody is named).
fn tick_stating(scene: &mut Scene, named: &[(&str, &str)]) {
    let Stage::Brigade { id, .. } = &scene.state.stage else {
        panic!("a brigade is staged");
    };
    let brigade_id = *id;
    scene.feed(Event::Tick {
        relay: Vec::new(),
        brigades: vec![BrigadeNameFacts {
            brigade_id,
            name: "cell".to_string(),
            director_session: Some("dir".to_string()),
            member_names: named
                .iter()
                .map(|(token, name)| (token.to_string(), name.to_string()))
                .collect(),
        }],
    });
}

#[test]
fn a_name_two_members_state_on_a_tick_is_shown_for_the_one_that_joined_first() {
    let mut scene = staged();
    // The list is in the opposite order to the join order: the winner is the
    // first to have joined, not the first mentioned.
    tick_stating(&mut scene, &[("worker-2", "Impl"), ("worker-1", "impl")]);
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("impl"));
    assert_eq!(shown(&scene, "worker-2"), None);
    assert_eq!(title(&scene, "worker-2"), "worker-2");

    // Stable across ticks.
    tick_stating(&mut scene, &[("worker-1", "impl"), ("worker-2", "Impl")]);
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("impl"));
    assert_eq!(shown(&scene, "worker-2"), None);

    // The holder gives it up (it is no longer in the list): the other's
    // statement now shows.
    tick_stating(&mut scene, &[("worker-2", "Impl")]);
    assert_eq!(shown(&scene, "worker-1"), None);
    assert_eq!(shown(&scene, "worker-2").as_deref(), Some("Impl"));
}

#[test]
fn a_name_equal_to_another_members_token_is_shown_for_no_one() {
    let mut scene = staged();
    // A member whose token is not worker-shaped, as a hand-made roster can have.
    scene
        .state
        .member_order
        .get_mut(&1)
        .unwrap()
        .push("scout".to_string());
    tick_stating(&mut scene, &[("worker-1", "Scout")]);
    assert_eq!(shown(&scene, "worker-1"), None);
    // A token-shaped name is refused per name already.
    tick_stating(&mut scene, &[("worker-1", "worker-2")]);
    assert_eq!(shown(&scene, "worker-1"), None);
    tick_stating(&mut scene, &[("worker-1", "scouting")]);
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("scouting"));
}

#[test]
fn a_roster_resolution_carrying_duplicate_or_token_equal_names_shows_each_name_once() {
    let mut scene = Scene::two_brigades(true);
    scene
        .names
        .insert((1, "worker-1".to_string()), "Impl".to_string());
    scene
        .names
        .insert((1, "worker-2".to_string()), "impl".to_string());
    scene
        .names
        .insert((1, "director".to_string()), "worker-1".to_string());
    scene.stage_one();
    // worker-1 joined before worker-2.
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("Impl"));
    assert_eq!(shown(&scene, "worker-2"), None);
    // A name that is a token (the Director states "worker-1") shows for no one.
    assert_eq!(shown(&scene, "director"), None);
    assert_eq!(title(&scene, "worker-1"), "Impl (worker-1)");
    assert_eq!(title(&scene, "worker-2"), "worker-2");
}

#[test]
fn dismissing_the_member_that_holds_a_duplicated_name_frees_it_for_the_other() {
    let mut scene = Scene::two_brigades(true);
    scene
        .names
        .insert((1, "worker-1".to_string()), "impl".to_string());
    scene
        .names
        .insert((1, "worker-2".to_string()), "IMPL".to_string());
    scene.stage_one();
    assert_eq!(shown(&scene, "worker-2"), None);
    scene.dismiss(1, "worker-1");
    // Stated by worker-2 all along (the roster at staging); with its rival
    // gone it shows at once, without waiting for a tick.
    assert_eq!(shown(&scene, "worker-2").as_deref(), Some("IMPL"));
}

#[test]
fn a_name_with_parentheses_is_allowed_and_the_last_group_of_the_title_is_always_the_token() {
    let mut scene = staged();
    scene.set_name(1, "worker-2", Some("impl (UI)"));
    scene.tick_with_names();
    assert_eq!(title(&scene, "worker-2"), "impl (UI) (worker-2)");
    assert_eq!(
        crate::member_name::validate("impl (UI)", "worker-2", &[("worker-2", None)]),
        Ok(Some("impl (UI)".to_string()))
    );
    assert_eq!(title(&scene, "worker-1"), "worker-1");
}

#[test]
fn a_status_quotes_a_given_name_and_leaves_a_token_bare_whatever_the_status() {
    let mut scene = Scene::two_brigades(true);
    scene.stage_one();
    // A name stated for a member about to be added (arranged by hand: banto's own
    // add-worker numbers a new Worker, so it is born unnamed).
    scene.state.state_member_name(1, "worker-3", Some("docs"));
    scene.state.resolve_member_names(1);
    scene.add_worker(1, WORKER_3);
    assert_eq!(status(&scene), "\"docs\" added");
    // Unnamed: the bare token.
    scene.add_worker(1, ("worker-4", BrigadeRole::Worker, Some("w4")));
    assert_eq!(status(&scene), "worker-4 added");
}

#[test]
fn a_brigade_listed_with_no_names_clears_and_one_not_listed_keeps_what_it_had() {
    let mut scene = staged();
    scene.set_name(1, "worker-1", Some("impl"));
    scene.tick_with_names();
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("impl"));

    // A tick that lists no brigade (a failed gather) leaves the names alone.
    scene.feed(Event::Tick {
        relay: Vec::new(),
        brigades: Vec::new(),
    });
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("impl"));
    // Another brigade's facts do not touch this one's.
    scene.feed(Event::Tick {
        relay: Vec::new(),
        brigades: vec![BrigadeNameFacts {
            brigade_id: 2,
            name: "other".to_string(),
            director_session: None,
            member_names: Vec::new(),
        }],
    });
    assert_eq!(shown(&scene, "worker-1").as_deref(), Some("impl"));
    // An empty list for this brigade says nobody is named.
    tick_stating(&mut scene, &[]);
    assert_eq!(shown(&scene, "worker-1"), None);
}

#[test]
fn a_worker_with_no_observation_is_named_all_the_same() {
    // The tick carries no observation for it (no session id yet); the brigade's
    // facts name it all the same, and the tile that is on screen shows the name.
    let mut scene = staged();
    tick_stating(&mut scene, &[("worker-2", "impl")]);
    assert_eq!(title(&scene, "worker-2"), "impl (worker-2)");
}
