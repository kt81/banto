//! A pane's screen and its PTY are never given a size the terminal model cannot
//! handle, however small the tile the layout computes for it.

use super::*;
use crate::app::App;
use crate::config::BrigadeConfig;
use crate::screen::Screen;

#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

fn send(state: &mut EmporiumState, app: &mut App, event: Event) -> Vec<Cmd> {
    update(state, app, &BrigadeConfig::default(), event, now())
}

fn key(id: &str) -> SessionKey {
    SessionKey::from_id(id)
}

/// A hosted session staged on its own, with the terminal of `width` × `height`.
fn solo_in_a_terminal(width: u16, height: u16) -> (EmporiumState, App, SessionKey) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let pane = key("solo");
    state.screens.insert(pane.clone(), Screen::new(24, 80));
    state.stage = Stage::Solo(pane.clone());
    send(&mut state, &mut app, Event::Resized { width, height });
    (state, app, pane)
}

/// Output that wraps at any width this test makes (a line of 240 columns), and
/// a newline: what any child writes first.
fn wrapping_text() -> Vec<u8> {
    let mut text = "a line of text ".repeat(16).into_bytes();
    text.extend_from_slice(b"\r\nand another\r\n");
    text
}

#[test]
fn a_terminal_four_rows_tall_does_not_crash_a_pane_that_prints_a_wrapping_line() {
    let (mut state, mut app, pane) = solo_in_a_terminal(120, 4);
    send(
        &mut state,
        &mut app,
        Event::PtyOutput {
            key: pane,
            chunk: wrapping_text(),
        },
    );
}

// --- the size every pane is given --------------------------------------------------------------

use crate::screen::{MIN_COLS, MIN_ROWS, SPAWN_COLS, SPAWN_ROWS};

/// The PTY resizes in `cmds` are exactly the sizes the screens took, and no
/// screen is below the floor: the screen and its child are always one size.
fn assert_sizes_agree(state: &EmporiumState, cmds: &[Cmd], what: &str) {
    for cmd in cmds {
        if let Cmd::ResizePty { key, rows, cols } = cmd {
            assert_eq!(
                state.screens[key].size(),
                (*rows, *cols),
                "{what}: the PTY of {} is told what its screen took",
                key.as_str()
            );
        }
    }
    for (key, screen) in &state.screens {
        let (rows, cols) = screen.size();
        assert!(
            rows >= MIN_ROWS && cols >= MIN_COLS,
            "{what}: {} is {rows}x{cols}",
            key.as_str()
        );
    }
}

/// Everything a child writes that has made a terminal model panic: text that
/// wraps, double-width glyphs, scrolling, a region of one row.
fn rough_output() -> Vec<u8> {
    let mut out = wrapping_text();
    out.extend_from_slice("\u{3042}\u{3044}\u{1F600}e\u{301}".repeat(12).as_bytes());
    out.extend_from_slice(b"\x1b[3S\x1b[3T\x1b[2L\x1b[2M\x1b[1;1rabc\r\n\r\n\x1bM\x1b[r");
    out.extend_from_slice(b"\x1b[999;999H");
    out.extend_from_slice("\u{3042}".as_bytes());
    out.extend_from_slice(b"\x1b[?1049hxyz\x1b[?1049l");
    out
}

fn feed(state: &mut EmporiumState, app: &mut App, pane: &SessionKey) {
    send(
        state,
        app,
        Event::PtyOutput {
            key: pane.clone(),
            chunk: rough_output(),
        },
    );
}

#[test]
fn a_terminal_four_rows_tall_gives_the_pane_the_floor_and_tells_the_pty_the_same() {
    let (mut state, mut app, pane) = solo_in_a_terminal(120, 80);
    let cmds = send(
        &mut state,
        &mut app,
        Event::Resized {
            width: 120,
            height: 4,
        },
    );
    // The tile has one content row; the screen and the child have two.
    assert_eq!(state.screens[&pane].size(), (MIN_ROWS, 82));
    assert!(
        cmds.contains(&Cmd::ResizePty {
            key: pane.clone(),
            rows: MIN_ROWS,
            cols: 82
        }),
        "{cmds:?}"
    );
    assert_sizes_agree(&state, &cmds, "120x4");
    feed(&mut state, &mut app, &pane);
}

#[test]
fn every_terminal_too_small_for_a_solo_pane_floors_both_axes() {
    for (width, height) in [
        (0, 0),
        (1, 1),
        (3, 3),
        (120, 1),
        (120, 3),
        (120, 4),
        (30, 40),
        (35, 40),
        (10, 4),
    ] {
        let (mut state, mut app, pane) = solo_in_a_terminal(80, 40);
        let cmds = send(&mut state, &mut app, Event::Resized { width, height });
        assert_sizes_agree(&state, &cmds, &format!("{width}x{height}"));
        feed(&mut state, &mut app, &pane);
        // Growing it back is followed too, and the child is told.
        let cmds = send(
            &mut state,
            &mut app,
            Event::Resized {
                width: 120,
                height: 40,
            },
        );
        assert_sizes_agree(&state, &cmds, "back to 120x40");
        assert_eq!(state.screens[&pane].size(), (37, 82));
    }
}

/// A staged brigade of a Director and `workers` Workers, every pane hosted.
fn brigade_in_a_terminal(
    workers: usize,
    width: u16,
    height: u16,
) -> (EmporiumState, App, Vec<SessionKey>) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let mut panes = vec![key("dir")];
    state
        .member_tokens
        .insert(key("dir"), DIRECTOR_TOKEN.to_string());
    for n in 1..=workers {
        let pane = key(&format!("w{n}"));
        state
            .member_tokens
            .insert(pane.clone(), format!("worker-{n}"));
        panes.push(pane);
    }
    for pane in &panes {
        state.member_brigades.insert(pane.clone(), 1);
        state.screens.insert(pane.clone(), Screen::new(24, 80));
    }
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(key("dir")),
        panes: panes.clone(),
        focused: 0,
    };
    send(&mut state, &mut app, Event::Resized { width, height });
    (state, app, panes)
}

#[test]
fn a_stack_with_more_workers_than_rows_floors_every_tile() {
    // Six Workers on a 24-row terminal is where the stack stops fitting its
    // minimums and splits what rows it has: a tile with one content row.
    let (mut state, mut app, panes) = brigade_in_a_terminal(6, 120, 24);
    let geometry = state.geometry(Rect::new(0, 0, 120, 24));
    let one_row_tiles = geometry
        .tiles
        .iter()
        .filter(|(_, rect)| pane_content(*rect).height == 1)
        .count();
    assert!(one_row_tiles > 0, "the layout gives a one-row tile here");
    assert_sizes_agree(&state, &[], "6 workers on 120x24");
    for pane in &panes {
        feed(&mut state, &mut app, pane);
    }
    // And more of them, narrower, shorter.
    for (workers, width, height) in [(10, 120, 40), (8, 80, 24), (6, 60, 10), (3, 36, 5)] {
        let (mut state, mut app, panes) = brigade_in_a_terminal(workers, width, height);
        assert_sizes_agree(
            &state,
            &[],
            &format!("{workers} workers on {width}x{height}"),
        );
        for pane in &panes {
            feed(&mut state, &mut app, pane);
        }
    }
}

#[test]
fn a_collapsed_tile_keeps_its_size_and_is_floored_when_it_opens_again() {
    let (mut state, mut app, panes) = brigade_in_a_terminal(2, 120, 40);
    let worker = panes[1].clone();
    let before = state.screens[&worker].size();
    toggle_minimize(&mut state, &worker);
    // Collapsed, and the terminal shrinks to almost nothing: the strip is not
    // resized (its child keeps the screen it has), and what it has is above the
    // floor.
    let cmds = send(
        &mut state,
        &mut app,
        Event::Resized {
            width: 40,
            height: 6,
        },
    );
    assert_eq!(state.screens[&worker].size(), before);
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::ResizePty { key, .. } if *key == worker))
    );
    assert_sizes_agree(&state, &cmds, "collapsed in 40x6");
    // Opened again in the tiny terminal: sized by the same pass, floored.
    toggle_minimize(&mut state, &worker);
    let cmds = send(
        &mut state,
        &mut app,
        Event::Resized {
            width: 40,
            height: 6,
        },
    );
    assert_sizes_agree(&state, &cmds, "opened in 40x6");
    feed(&mut state, &mut app, &worker);
}

#[test]
fn an_off_stage_pane_keeps_the_size_it_has_whatever_the_terminal_does() {
    let (mut state, mut app, _panes) = brigade_in_a_terminal(1, 120, 40);
    let off_stage = key("kept-alive");
    state
        .screens
        .insert(off_stage.clone(), Screen::new(SPAWN_ROWS, SPAWN_COLS));
    let cmds = send(
        &mut state,
        &mut app,
        Event::Resized {
            width: 20,
            height: 3,
        },
    );
    assert_eq!(state.screens[&off_stage].size(), (SPAWN_ROWS, SPAWN_COLS));
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::ResizePty { key, .. } if *key == off_stage))
    );
    assert_sizes_agree(&state, &cmds, "off stage in 20x3");
    feed(&mut state, &mut app, &off_stage);
}

#[test]
fn soloing_a_tile_in_a_small_terminal_floors_it_too() {
    let (mut state, mut app, panes) = brigade_in_a_terminal(3, 120, 40);
    for solo in &panes {
        toggle_solo(&mut state, solo);
        for (width, height) in [(120, 4), (30, 40), (4, 4), (120, 40)] {
            let cmds = send(&mut state, &mut app, Event::Resized { width, height });
            assert_sizes_agree(
                &state,
                &cmds,
                &format!("{} soloed in {width}x{height}", solo.as_str()),
            );
            feed(&mut state, &mut app, solo);
        }
        toggle_solo(&mut state, solo);
    }
}

#[test]
fn a_freshly_spawned_pane_starts_above_the_floor_with_the_size_its_pty_is_opened_at() {
    const {
        assert!(SPAWN_ROWS >= MIN_ROWS && SPAWN_COLS >= MIN_COLS);
    }
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(Vec::new());
    let pane = key("fresh");
    send(&mut state, &mut app, Event::Spawned { key: pane.clone() });
    assert_eq!(state.screens[&pane].size(), (SPAWN_ROWS, SPAWN_COLS));
}
