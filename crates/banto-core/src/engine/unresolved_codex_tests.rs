//! What a Codex pane that is not yet identified (`unresolved_codex`) does to the
//! operator. Three things:
//!
//! 1. A new Codex session opened solo and never identified, then another session
//!    shown in its place: nothing the operator does shows it again or ends it,
//!    except being turned away because of it (a Codex resume, staging a brigade, `x`
//!    on a row with no pane), which brings it forward and says how to go on.
//! 2. A Codex Worker that exits on its own while some other pane is unidentified:
//!    its reopen is held until the wait ends, and what becomes of its tile, its
//!    screen and its place in the roster meanwhile. Told by a line and a mark, not
//!    answered as a refusal is (nothing is brought forward unasked); and a
//!    relaunch that cannot open at all.
//! 3. What a refusal does: which pane it names, what it says, the mark it leaves.
//!
//! Every input here goes through `update`, as the shell feeds it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::config::BrigadeConfig;
use crate::input::{
    InputEvent, KeyCode, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crate::model::{Activity, AgentKind, PtyExitReason, SessionRow};
use crate::screen::Screen;

use super::*;

fn row_of(id: &str, agent: AgentKind) -> SessionRow {
    SessionRow {
        id: id.to_string(),
        agent,
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

fn brigade() -> BrigadeConfig {
    BrigadeConfig::default()
}

fn key_event(code: KeyCode, modifiers: Modifiers) -> Event {
    Event::Input(InputEvent::Key(KeyEvent::new(code, modifiers)))
}

fn feed(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    update(state, app, &brigade(), event, at)
}

/// [`feed`], and then what the shell does with the one question the core asks of
/// it before it shows a row: `ResolveMembership` is answered "no brigade", which
/// is what any session that is no member gets. Without the answer the stage would
/// never change, whatever the operator pressed.
fn input(state: &mut EmporiumState, app: &mut App, event: Event, at: Instant) -> Vec<Cmd> {
    let mut cmds = feed(state, app, event, at);
    let asked: Vec<String> = cmds
        .iter()
        .filter_map(|cmd| match cmd {
            Cmd::Store(StoreIntent::ResolveMembership { session_id }) => Some(session_id.clone()),
            _ => None,
        })
        .collect();
    for session_id in asked {
        cmds.extend(feed(
            state,
            app,
            Event::MembershipResolved {
                session_id,
                membership: None,
                members: None,
                layout: None,
                names: Vec::new(),
            },
            at,
        ));
    }
    cmds
}

fn select(app: &mut App, id: &str) {
    app.select_first();
    for _ in 0..8 {
        if app.selected_row().is_some_and(|row| row.id == id) {
            return;
        }
        app.select_next();
    }
    panic!("no row {id} to select");
}

fn solo_is(state: &EmporiumState, wanted: &SessionKey) -> bool {
    matches!(&state.stage, Stage::Solo(shown) if shown == wanted)
}

fn opens(cmds: &[Cmd]) -> Vec<&Cmd> {
    cmds.iter()
        .filter(|cmd| matches!(cmd, Cmd::OpenEmbedded { .. }))
        .collect()
}

// --- question 1 ----------------------------------------------------------------

/// What the operator then opened in place of the unidentified pane.
#[derive(Debug, Clone, Copy)]
enum Shown {
    /// A Claude session that already has a pane.
    ClaudeSolo,
    /// A Codex session that already has a pane.
    PanedCodex,
    /// A Claude-only brigade.
    ClaudeBrigade,
}

/// The operator opened a new Codex session solo (`n`), which was never identified,
/// and then showed `shown` in its place. Returns the state, the app and the
/// unidentified pane's key.
///
/// The list has the Claude sessions `c1` and `dir`, and the Codex session `cx`;
/// the unidentified pane has no row, since a row is made from a session id.
fn unidentified_pane_behind(shown: Shown) -> (EmporiumState, App, SessionKey) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let mut app = App::new(vec![
        row_of("c1", AgentKind::ClaudeCode),
        row_of("cx", AgentKind::Codex),
        row_of("dir", AgentKind::ClaudeCode),
    ]);
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::Resized {
            width: 120,
            height: 40,
        },
        at,
    );
    // The `n` flow, as `update_new_session_cwd_checked` leaves it: a minted key
    // held in `unresolved_codex`, opened as a solo until the spawn is reported.
    let pending = state.mint_plain_key(Path::new("/work/new"));
    state.unresolved_codex.insert(pending.clone());
    state
        .pending_opens
        .insert(pending.clone(), PendingOpen::Solo);
    feed(
        &mut state,
        &mut app,
        Event::Spawned {
            key: pending.clone(),
            agent: Some(AgentKind::Codex),
        },
        at,
    );
    state.screens.insert(pending.clone(), Screen::new(6, 20));
    assert!(solo_is(&state, &pending), "{:?}", state.stage);

    // Whatever replaces it on the stage already has its panes: opening a row that
    // has one shows it and spawns nothing.
    let enter_on = |state: &mut EmporiumState, app: &mut App, id: &str| {
        state.screens.insert(key(id), Screen::new(6, 20));
        select(app, id);
        state.focus = Focus::Sidebar;
        let cmds = input(state, app, key_event(KeyCode::Enter, Modifiers::NONE), at);
        assert!(opens(&cmds).is_empty(), "{cmds:?}");
        assert!(solo_is(state, &key(id)), "{:?}", state.stage);
    };
    match shown {
        Shown::ClaudeSolo => enter_on(&mut state, &mut app, "c1"),
        Shown::PanedCodex => enter_on(&mut state, &mut app, "cx"),
        Shown::ClaudeBrigade => {
            let (director, worker) = (key("dir"), key("w1"));
            for (pane, token) in [(&director, DIRECTOR_TOKEN), (&worker, "worker-1")] {
                state.screens.insert(pane.clone(), Screen::new(6, 20));
                state.member_tokens.insert(pane.clone(), token.to_string());
                state.member_brigades.insert(pane.clone(), 1);
            }
            state.stage = Stage::Brigade {
                id: 1,
                director: Some(director.clone()),
                panes: vec![director, worker],
                focused: 0,
            };
        }
    }
    assert!(state.screens.contains_key(&pending));
    assert!(state.unresolved_codex.contains(&pending));
    (state, app, pending)
}

/// What on the stage or in the commands touches `pending` after one input.
fn touches(state: &EmporiumState, cmds: &[Cmd], pending: &SessionKey) -> Option<String> {
    let staged = match &state.stage {
        Stage::Solo(key) => key == pending,
        Stage::Brigade { panes, .. } => panes.contains(pending),
        Stage::Empty => false,
    };
    if staged {
        return Some(format!("staged: {:?}", state.stage));
    }
    // A resize is the layout telling every hosted pane its size, on or off the
    // stage; it neither shows nor ends the pane.
    cmds.iter()
        .filter(|cmd| !matches!(cmd, Cmd::ResizePty { .. }))
        .map(|cmd| format!("{cmd:?}"))
        .find(|text| text.contains(pending.as_str()))
}

/// What the status line says when a refusal names the pane, up to the part that
/// differs by cause.
const WAIT_SAID: &str = "identified yet";

fn said(state: &EmporiumState) -> String {
    state.displayed_status().unwrap_or_default().to_string()
}

/// What an input did to `pending` that only a refusal may do. An input that was
/// refused says so, and then the one thing it may have done is bring the pane to
/// the stage with the keys, sending nothing for it; any other input must not touch it.
fn reaches(state: &EmporiumState, cmds: &[Cmd], pending: &SessionKey) -> Option<String> {
    if !said(state).contains(WAIT_SAID) {
        return touches(state, cmds, pending);
    }
    let sent = cmds
        .iter()
        .filter(|cmd| !matches!(cmd, Cmd::ResizePty { .. }))
        .map(|cmd| format!("{cmd:?}"))
        .find(|text| text.contains(pending.as_str()));
    if sent.is_some() {
        return sent;
    }
    (!solo_is(state, pending) || state.focus != Focus::Pane)
        .then(|| format!("refused, but the pane is not shown: {:?}", state.stage))
}

/// Every key an operator can press (printable ASCII, the named keys, F1-F12),
/// with and without Ctrl/Alt/Shift, bare and after the prefix, with the focus on
/// the list and on a pane and the list's selection on each of the two rows that
/// have a pane, which in two of the three scenes is a Codex row with none; and the
/// mouse over the whole window. Each input starts from a fresh scene, so that what
/// one changed (a modal, a stage) cannot hide the next. The unidentified pane is
/// brought to the stage only by an input that was refused for it (the Codex row's
/// Enter and `x` are), and nothing is ever sent for it.
#[test]
fn nothing_but_a_refusal_shows_or_ends_an_unidentified_pane_that_is_off_the_stage() {
    let prefix = key_event(KeyCode::Char('b'), Modifiers::CONTROL);
    let mut codes: Vec<KeyCode> = (b' '..=b'~').map(|b| KeyCode::Char(b as char)).collect();
    codes.extend([
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Insert,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
    ]);
    codes.extend((1..=12).map(KeyCode::F));
    let modifiers = [
        Modifiers::NONE,
        Modifiers::CONTROL,
        Modifiers::ALT,
        Modifiers::SHIFT,
    ];

    let mut found = Vec::new();
    let mut inputs = 0usize;
    let mut refusals = 0usize;
    for shown in [Shown::ClaudeSolo, Shown::PanedCodex, Shown::ClaudeBrigade] {
        for focus in [Focus::Sidebar, Focus::Pane] {
            for selected in ["c1", "cx"] {
                for after_prefix in [false, true] {
                    for &code in &codes {
                        for &mods in &modifiers {
                            // Shift on a printable character is the character.
                            if mods == Modifiers::SHIFT && matches!(code, KeyCode::Char(_)) {
                                continue;
                            }
                            let (mut s, mut a, p) = unidentified_pane_behind(shown);
                            s.focus = focus;
                            select(&mut a, selected);
                            let at = now();
                            let mut cmds = Vec::new();
                            if after_prefix {
                                cmds.extend(input(&mut s, &mut a, prefix.clone(), at));
                            }
                            cmds.extend(input(&mut s, &mut a, key_event(code, mods), at));
                            inputs += 1;
                            refusals += said(&s).contains(WAIT_SAID) as usize;
                            if let Some(what) = reaches(&s, &cmds, &p) {
                                found.push(format!(
                                    "{shown:?} {focus:?} sel={selected} prefix={after_prefix} \
                                     {code:?} {mods:?}: {what}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::Moved,
        ] {
            for column in (0..120).step_by(5) {
                for row in (0..40).step_by(2) {
                    let (mut s, mut a, p) = unidentified_pane_behind(shown);
                    let event = Event::Input(InputEvent::Mouse(MouseEvent { kind, column, row }));
                    let cmds = input(&mut s, &mut a, event, now());
                    inputs += 1;
                    refusals += said(&s).contains(WAIT_SAID) as usize;
                    if let Some(what) = reaches(&s, &cmds, &p) {
                        found.push(format!("{shown:?} {kind:?} at {column},{row}: {what}"));
                    }
                }
            }
        }
    }
    // The sweep did run: a check over zero inputs would pass too. So did the
    // refusals it allows for, or the exception above would be doing nothing.
    assert!(inputs > 5_000, "{inputs}");
    assert!(refusals > 0, "no input was refused");
    assert!(
        found.is_empty(),
        "{} of {inputs} inputs:\n{}",
        found.len(),
        found.join("\n")
    );
}

/// The sweep's detector is not blind: a pane that is on the stage, or one a
/// command is sent for, is what it reports.
#[test]
fn the_sweeps_detector_sees_a_pane_that_is_staged_or_sent_a_command() {
    let (mut state, _app, pending) = unidentified_pane_behind(Shown::ClaudeSolo);
    assert!(touches(&state, &[], &pending).is_none());
    assert!(
        touches(
            &state,
            &[Cmd::HangUpPty {
                key: pending.clone()
            }],
            &pending
        )
        .is_some()
    );
    state.stage = Stage::Solo(pending.clone());
    assert!(touches(&state, &[], &pending).is_some());
}

/// The one place the operator still meets it: the quit question counts every
/// hosted pane whether or not it is on the stage, and names this one "new
/// session". Two Esc presses quit banto, which ends it with everything else;
/// that is not closing it, and the stage is never asked.
#[test]
fn the_quit_question_counts_the_unidentified_pane_but_only_quitting_ends_it() {
    let (mut state, mut app, pending) = unidentified_pane_behind(Shown::PanedCodex);
    let at = now();
    // Showing a pane gives it the keys, and an Esc there goes to the process: the
    // question is the sidebar's.
    state.focus = Focus::Sidebar;
    // Nothing has read as idle for any pane yet (no sample came in), so the
    // pane is counted as not idle.
    feed(
        &mut state,
        &mut app,
        key_event(KeyCode::Esc, Modifiers::NONE),
        at,
    );
    let asked = state.displayed_status().unwrap_or_default().to_string();
    assert!(!app.should_quit(), "{asked}");
    assert!(asked.contains("new session"), "{asked}");
    assert!(state.screens.contains_key(&pending));
    // The second Esc is the quit, and it names no pane to end: the shell ends
    // them with the process.
    let cmds = feed(
        &mut state,
        &mut app,
        key_event(KeyCode::Esc, Modifiers::NONE),
        at,
    );
    assert!(app.should_quit());
    assert!(touches(&state, &cmds, &pending).is_none(), "{cmds:?}");
}

/// Nothing the operator does restages it, but identification does: the shell's
/// discovery result gives it its session's key, the wait ends, and its row now
/// opens the pane that is already there, with no spawn.
#[test]
fn identification_is_what_brings_the_pane_back_and_its_row_then_shows_it() {
    let (mut state, mut app, pending) = unidentified_pane_behind(Shown::ClaudeSolo);
    let at = now();
    app.replace_rows(vec![
        row_of("c1", AgentKind::ClaudeCode),
        row_of("found", AgentKind::Codex),
    ]);
    let cmds = feed(
        &mut state,
        &mut app,
        Event::DiscoveryResult {
            key: pending.clone(),
            session_id: "found".to_string(),
            member: None,
        },
        at,
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::RekeyPty { from, to } if from == &pending && *to == key("found"))),
        "{cmds:?}"
    );
    assert!(state.unresolved_codex.is_empty());
    assert!(state.screens.contains_key(&key("found")));
    assert!(
        solo_is(&state, &key("c1")),
        "identifying it does not move the stage: {:?}",
        state.stage
    );

    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at,
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &key("found")), "{:?}", state.stage);
}

// --- question 2 ----------------------------------------------------------------

/// A staged brigade 1 whose Worker `w1` is a Codex session, and, off the stage, a
/// solo Codex pane opened from `n` that is still unidentified. Returns the state,
/// the app and the unidentified pane's key.
fn codex_worker_beside_an_unidentified_pane() -> (EmporiumState, App, SessionKey) {
    let mut state = EmporiumState::new(PrefixKey::default());
    let (director, worker) = (key("dir"), key("w1"));
    for (pane, token) in [(&director, DIRECTOR_TOKEN), (&worker, "worker-1")] {
        state.screens.insert(pane.clone(), Screen::new(6, 20));
        state.member_tokens.insert(pane.clone(), token.to_string());
        state.member_brigades.insert(pane.clone(), 1);
    }
    state.stage = Stage::Brigade {
        id: 1,
        director: Some(director.clone()),
        panes: vec![director, worker],
        focused: 0,
    };
    let app = App::new(vec![
        row_of("dir", AgentKind::ClaudeCode),
        row_of("w1", AgentKind::Codex),
    ]);
    let pending = state.mint_plain_key(Path::new("/work/new"));
    state.screens.insert(pending.clone(), Screen::new(6, 20));
    state.unresolved_codex.insert(pending.clone());
    (state, app, pending)
}

fn tick(state: &mut EmporiumState, app: &mut App, at: Instant) -> Vec<Cmd> {
    feed(
        state,
        app,
        Event::Tick {
            relay: Vec::new(),
            brigades: Vec::new(),
        },
        at,
    )
}

/// The Worker's exit is the process ending on its own, so `relaunch_member` would
/// resume it; a Codex resume waits while another Codex pane is unidentified, so the
/// reopen is held. What the operator is left with: the tile still staged and
/// empty, the member still in the roster, the stage and focus where they were, a
/// line that names the Worker, and the mark on the pane it waits for.
#[test]
fn a_codex_worker_that_exits_while_a_pane_is_unidentified_is_held() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let worker = key("w1");
    let at = now();
    let focus_before = state.focus;

    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );

    // Nothing is spawned, and nothing was tried: the cooldown has not started.
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(!state.last_relaunch.contains_key(&worker));
    // The tile keeps its place in the brigade and the member its identity ...
    assert!(
        matches!(&state.stage, Stage::Brigade { panes, .. } if panes.contains(&worker)),
        "{:?}",
        state.stage
    );
    assert_eq!(
        state.member_tokens.get(&worker).map(String::as_str),
        Some("worker-1")
    );
    assert_eq!(state.member_brigades.get(&worker), Some(&1));
    // ... but nothing hosts it, and no store write removes it.
    assert!(!state.screens.contains_key(&worker));
    assert!(!state.pending_opens.contains_key(&worker));
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::DismissExitedWorker { .. }))),
        "{cmds:?}"
    );
    // The wait is untouched.
    assert!(state.unresolved_codex.contains(&pending));
    // Told as an automatic event is: the Worker, the way on, the cause's tile marked,
    // and nothing moved (the brigade keeps the stage and the focus).
    let said = state.displayed_status().expect("a status line");
    assert!(said.contains("Restart of worker-1 is held"), "{said}");
    assert!(said.contains("Ctrl+B x"), "{said}");
    assert!(matches!(&state.stage, Stage::Brigade { focused: 0, .. }));
    assert_eq!(state.focus, focus_before);
    assert!(state.refused_for.contains(&pending));
    assert!(state.tile_label(&pending).ends_with(" \u{b7} unidentified"));
}

/// Once the wait clears (here: the unidentified pane exits), the held Worker is
/// reopened in that same update, once, with the cooldown starting then. Nothing
/// reopens it a second time: not a tick, not a resize, not a key press.
#[test]
fn a_held_worker_is_reopened_once_the_wait_clears() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    assert!(!state.last_relaunch.contains_key(&worker));

    let cleared = at + Duration::from_secs(40);
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: pending.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        cleared,
    );
    assert!(state.unresolved_codex.is_empty());
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");
    assert!(matches!(
        opens(&cmds)[0],
        Cmd::OpenEmbedded { key, target, brigade: Some((1, token, BrigadeRole::Worker)), .. }
            if *key == worker && target.agent == AgentKind::Codex && target.id == "w1"
                && token == "worker-1"
    ));
    assert_eq!(state.last_relaunch.get(&worker), Some(&cleared));
    assert!(state.pending_opens.contains_key(&worker));

    let mut every = Vec::new();
    for second in 41..=240u64 {
        let later = at + Duration::from_secs(second);
        every.extend(tick(&mut state, &mut app, later));
        if second % 50 == 0 {
            every.extend(feed(
                &mut state,
                &mut app,
                Event::Resized {
                    width: 100 + second as u16 % 7,
                    height: 40,
                },
                later,
            ));
            every.extend(feed(
                &mut state,
                &mut app,
                key_event(KeyCode::Char('x'), Modifiers::ALT),
                later,
            ));
        }
    }
    assert!(opens(&every).is_empty(), "{:?}", opens(&every));
}

/// The wait is over when no Codex pane is unidentified, not when one is: with a
/// second one still waiting, the first one's exit releases nothing.
#[test]
fn a_held_worker_keeps_waiting_while_another_pane_is_unidentified() {
    let (mut state, mut app, first) = codex_worker_beside_an_unidentified_pane();
    let second = state.mint_plain_key(Path::new("/work/other"));
    state.screens.insert(second.clone(), Screen::new(6, 20));
    state.unresolved_codex.insert(second.clone());
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: first,
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(1),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: second,
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(2),
    );
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");
}

/// What can happen during a long wait, each judged again just before the reopen:
/// a hold whose member was removed or replaced, whose brigade is closing or being
/// disbanded, or whose key already has a process, is dropped and nothing else
/// happens. (None of these leaves anything to retry.) Which brigade is on the
/// stage is not one of them: a Worker of a brigade that is not shown is reopened
/// when nothing waits, so it is when something did.
#[test]
fn a_held_reopen_is_dropped_when_it_no_longer_applies() {
    type Spoil = fn(&mut EmporiumState, &mut App);
    let spoils: [(&str, Spoil); 5] = [
        ("member forgotten", |state, _| {
            state.member_tokens.remove(&key("w1"));
        }),
        ("member renumbered", |state, _| {
            state
                .member_tokens
                .insert(key("w1"), "worker-9".to_string());
        }),
        ("brigade disbanding", |state, _| {
            state.disbanding.insert(1);
        }),
        ("brigade closing", |state, _| {
            state.closing.insert(1, now());
        }),
        ("a process already there", |state, _| {
            state.screens.insert(key("w1"), Screen::new(6, 20));
        }),
    ];
    for (what, spoil) in spoils {
        let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
        let at = now();
        feed(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: key("w1"),
                reason: Some(PtyExitReason::Code(1)),
            },
            at,
        );
        spoil(&mut state, &mut app);
        let cmds = feed(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: pending,
                reason: Some(PtyExitReason::Code(0)),
            },
            at + Duration::from_secs(1),
        );
        assert!(
            opens(&cmds).is_empty(),
            "{what}: a held reopen was made: {cmds:?}"
        );
        assert!(!state.pending_opens.contains_key(&key("w1")), "{what}");
        assert!(state.held_relaunches.is_empty(), "{what}: the hold stays");
    }
}

/// A row that has left the list is not resumable either.
#[test]
fn a_held_reopen_is_dropped_when_its_row_is_gone() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("w1"),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    app.replace_rows(vec![row_of("dir", AgentKind::ClaudeCode)]);
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: pending,
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(1),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(state.held_relaunches.is_empty());
}

/// Enter on the empty tile is the operator asking: it is answered as a refusal is,
/// bringing the pane that waits onto the stage. Keys, Enter included, are not
/// forwarded to a tile with no process behind it.
#[test]
fn enter_on_a_held_tile_brings_the_cause_forward_and_nothing_is_written() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    // The operator moves to the held tile.
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    state.focus = Focus::Pane;

    let typed = feed(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('a'), Modifiers::NONE),
        at + Duration::from_secs(1),
    );
    assert!(
        !typed.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
        "{typed:?}"
    );

    let cmds = feed(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at + Duration::from_secs(2),
    );
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
        "{cmds:?}"
    );
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    let said = state.displayed_status().expect("a status line");
    assert!(said.contains("Restart of worker-1 is held"), "{said}");
}

/// A relaunch the shell turns away for an unidentified pane that the core did not
/// know of goes back to being held, and is not released in the same update (that
/// would be refused again, and again).
#[test]
fn a_relaunch_the_shell_refuses_for_an_unidentified_pane_goes_back_to_being_held() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let at = now();
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");

    let cmds = feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        at + Duration::from_secs(1),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(state.is_held(&worker));
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::DismissExitedWorker { .. }))),
        "{cmds:?}"
    );
}

/// A relaunch that fails to open for a reason of its own (the process could not be
/// started) is a Worker that exited and cannot be reopened: it leaves the brigade
/// as one that exited twice does, and the line says why.
#[test]
fn a_relaunch_that_cannot_open_removes_the_worker_like_one_that_gave_up() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );

    let cmds = feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: "cannot open a pty".to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: None,
        },
        at + Duration::from_secs(1),
    );
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::Store(StoreIntent::DismissExitedWorker { brigade_id: 1, token, repeated: true })
                if token == "worker-1"
        )),
        "{cmds:?}"
    );
    assert!(
        matches!(&state.stage, Stage::Brigade { panes, .. } if !panes.contains(&worker)),
        "{:?}",
        state.stage
    );
    assert!(!state.screens.contains_key(&worker));
    let said = state.displayed_status().expect("a status line");
    assert!(said.contains("could not restart"), "{said}");
    assert!(said.contains("cannot open a pty"), "{said}");

    // The store's answer says it once the member is gone.
    feed(
        &mut state,
        &mut app,
        Event::ExitedWorkerDismissed {
            brigade_id: 1,
            token: "worker-1".to_string(),
            repeated: true,
            result: Ok((true, HashSet::new(), HashSet::new())),
        },
        at + Duration::from_secs(2),
    );
    let said = state.displayed_status().expect("a status line");
    assert!(said.contains("could not be restarted"), "{said}");
    assert!(said.contains("cannot open a pty"), "{said}");
    assert!(!state.member_tokens.contains_key(&worker));
    assert!(state.relaunch_failures.is_empty());
}

/// An open that was not a relaunch keeps what it had: a status line and nothing
/// else. Only the Worker's own reopen after its exit counts.
#[test]
fn a_failed_open_that_is_not_a_relaunch_does_not_remove_anyone() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let cmds = feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: "cannot open a pty".to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: None,
        },
        now(),
    );
    assert!(cmds.is_empty() || opens(&cmds).is_empty(), "{cmds:?}");
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::DismissExitedWorker { .. })))
    );
    assert!(state.member_tokens.contains_key(&worker));
    assert_eq!(
        state.displayed_status(),
        Some("failed to open: cannot open a pty")
    );
}

/// The same exit with nothing unidentified is reopened at once. This is the
/// control for the tests above: the wait, not the setup, is what holds it.
#[test]
fn the_same_exit_with_nothing_unidentified_is_reopened() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        now(),
    );
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");
    assert!(matches!(
        opens(&cmds)[0],
        Cmd::OpenEmbedded { key, target, .. } if *key == worker && target.agent == AgentKind::Codex
    ));
}

// --- a row for the session exists, the pane is not identified --------------------

/// The scene of the operator's observation: the new Codex pane has said its first
/// words, so Codex wrote the session and the list has a row `found` for it, but
/// banto has not identified the pane (its screen is still under the minted key and
/// the key is still in `unresolved_codex`). The stage then moved to `c1`.
fn row_listed_pane_unidentified() -> (EmporiumState, App, SessionKey) {
    let (state, mut app, pending) = unidentified_pane_behind(Shown::ClaudeSolo);
    app.replace_rows(vec![
        row_of("c1", AgentKind::ClaudeCode),
        row_of("found", AgentKind::Codex),
    ]);
    (state, app, pending)
}

/// The sentence a lone unidentified pane gets, with the default prefix.
const LONE_PANE_SAID: &str = "New Codex pane not identified yet (banto can't tell which session it is). Close it with Ctrl+B x, then Enter on the row again; or send its first message.";

/// Enter on that row is taken for a resume of a session with no pane (its pane is
/// under another key) and is refused, like any Codex resume, while the pane waits
/// to be identified. Nothing is spawned. The refusal brings the pane forward: the
/// stage is the pane and the keys are its, the tile says it is unidentified, and the
/// status says how to go on. Before the refusal the tile carries no mark.
#[test]
fn enter_on_the_row_of_an_unidentified_pane_is_refused_and_brings_the_pane_forward() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    assert!(!state.tile_label(&pending).contains("unidentified"));
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        now(),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
    assert_eq!(state.tile_label(&pending), "session \u{b7} unidentified");
    assert!(state.screens.contains_key(&pending));
    assert!(state.unresolved_codex.contains(&pending));
}

/// `x` on that row used to say "has no open pane", which is false: the pane is
/// open, banto just cannot tie it to the row. It is now the same refusal as Enter's,
/// the same words and the same stage, and opens no dialog of its own; the dialog is
/// the pane's own `Prefix x`, which the words point to.
#[test]
fn x_on_the_row_of_an_unidentified_pane_is_the_same_refusal() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('x'), Modifiers::NONE),
        now(),
    );
    assert!(
        cmds.iter()
            .all(|cmd| matches!(cmd, Cmd::Store(_) | Cmd::ResizePty { .. })),
        "{cmds:?}"
    );
    assert!(app.modal().is_none());
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.tile_label(&pending), "session \u{b7} unidentified");
}

/// `x` on a Codex row with no pane and nothing unidentified is still "no open
/// pane": the refusal is for the case where that sentence is false.
#[test]
fn x_on_a_codex_row_with_no_pane_and_nothing_unidentified_says_so() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    state.unresolved_codex.remove(&pending);
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('x'), Modifiers::NONE),
        now(),
    );
    assert_eq!(state.displayed_status(), Some("found has no open pane"));
    assert!(!solo_is(&state, &pending));
}

/// The third entrance: staging a brigade that has a Codex member with no pane, the
/// Director's row answered as a Director's. Same words, same stage.
#[test]
fn staging_a_brigade_with_a_paneless_codex_member_is_the_same_refusal() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    app.replace_rows(vec![
        row_of("c1", AgentKind::ClaudeCode),
        row_of("dir", AgentKind::ClaudeCode),
        row_of("wx", AgentKind::Codex),
    ]);
    select(&mut app, "dir");
    state.focus = Focus::Sidebar;
    state.pending_membership = Some((PendingMembership::Activate, "dir".to_string()));
    let at = now();
    let cmds = feed(
        &mut state,
        &mut app,
        Event::MembershipResolved {
            session_id: "dir".to_string(),
            membership: Some((7, DIRECTOR_TOKEN.to_string(), BrigadeRole::Director)),
            members: Some(vec![
                (
                    DIRECTOR_TOKEN.to_string(),
                    BrigadeRole::Director,
                    Some("dir".to_string()),
                ),
                (
                    "worker-1".to_string(),
                    BrigadeRole::Worker,
                    Some("wx".to_string()),
                ),
            ]),
            layout: None,
            names: Vec::new(),
        },
        at,
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
}

/// The shell's guard refuses the same resume with the same error, and the core
/// answers that event as it answers its own filter: so the two cannot say
/// different things.
#[test]
fn the_shells_refusal_is_answered_like_the_cores() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: key("found"),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        now(),
    );
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.tile_label(&pending), "session \u{b7} unidentified");
}

/// Any other failure to open is not a refusal for an unidentified pane, and does
/// not move the stage.
#[test]
fn another_spawn_failure_does_not_bring_the_pane_forward() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: key("found"),
            error: "no such file".to_string(),
            refusal: SpawnRefusal::PtyOpen,
            os_error: None,
        },
        now(),
    );
    assert!(!solo_is(&state, &pending), "{:?}", state.stage);
    assert!(state.refused_for.is_empty());
}

/// The sentence writes the prefix the way the hint line does, so a configured one
/// shows. It never says the pane may be "another session": it may be the one asked
/// for.
#[test]
fn the_sentence_uses_the_configured_prefix_and_never_says_another_session() {
    let (mut state, mut app, _pending) = row_listed_pane_unidentified();
    state.prefix = PrefixKey::try_parse("C-a").unwrap();
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        now(),
    );
    let said = said(&state);
    assert!(said.contains(&format!("Close it with {} x,", state.prefix_label())));
    assert!(state.prefix_label() != "Ctrl+B", "{said}");
    assert!(!said.to_lowercase().contains("another session"), "{said}");
    assert!(said.chars().count() <= 160, "{said}");
}

/// With two panes waiting, the one already on the stage is the one named, so the
/// operator is not moved off what they are looking at; with neither on the stage,
/// the one that has waited longest. Closing that one names the next.
#[test]
fn with_several_unidentified_panes_the_staged_one_then_the_oldest_is_named() {
    let (mut state, mut app, first) = row_listed_pane_unidentified();
    let second = state.mint_plain_key(Path::new("/work/other"));
    state.screens.insert(second.clone(), Screen::new(6, 20));
    state.unresolved_codex.insert(second.clone());
    select(&mut app, "found");
    let ask = |state: &mut EmporiumState, app: &mut App| {
        state.focus = Focus::Sidebar;
        input(
            state,
            app,
            key_event(KeyCode::Enter, Modifiers::NONE),
            now(),
        );
    };

    // Neither staged: the older.
    ask(&mut state, &mut app);
    assert!(solo_is(&state, &first), "{:?}", state.stage);

    // The younger on the stage: it stays.
    state.stage = Stage::Solo(second.clone());
    ask(&mut state, &mut app);
    assert!(solo_is(&state, &second), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert!(state.refused_for.contains(&second));

    // The older closes, the younger is the one left to name.
    state.stage = Stage::Solo(key("c1"));
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: first.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        now(),
    );
    assert!(!state.refused_for.contains(&first));
    ask(&mut state, &mut app);
    assert!(solo_is(&state, &second), "{:?}", state.stage);
}

/// The mark is kept until the pane is identified or gone, and is gone with either.
#[test]
fn the_mark_goes_when_the_pane_is_identified_or_exits() {
    let at = now();
    let marked = || {
        let (mut state, mut app, pending) = row_listed_pane_unidentified();
        select(&mut app, "found");
        state.focus = Focus::Sidebar;
        input(
            &mut state,
            &mut app,
            key_event(KeyCode::Enter, Modifiers::NONE),
            at,
        );
        assert!(state.refused_for.contains(&pending));
        (state, app, pending)
    };

    let (mut state, mut app, pending) = marked();
    // Time passing keeps it.
    tick(&mut state, &mut app, at + Duration::from_secs(60));
    assert_eq!(state.tile_label(&pending), "session \u{b7} unidentified");
    feed(
        &mut state,
        &mut app,
        Event::DiscoveryResult {
            key: pending.clone(),
            session_id: "found".to_string(),
            member: None,
        },
        at,
    );
    assert!(state.refused_for.is_empty());
    assert!(!state.tile_label(&key("found")).contains("unidentified"));

    let (mut state, mut app, pending) = marked();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: pending.clone(),
            reason: Some(PtyExitReason::Code(0)),
        },
        at,
    );
    assert!(state.refused_for.is_empty());
    assert!(!state.tile_label(&pending).contains("unidentified"));
}

/// A refusal with nothing to show (the unidentified pane has no screen yet) names
/// nothing and moves nothing, but still says it is waiting and marks nothing.
#[test]
fn a_refusal_with_no_pane_to_show_says_so_and_moves_nothing() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    state.screens.remove(&pending);
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        now(),
    );
    assert!(solo_is(&state, &key("c1")), "{:?}", state.stage);
    assert!(state.refused_for.is_empty());
    assert!(
        said(&state).contains("is still being identified"),
        "{}",
        said(&state)
    );
}

/// When the pane that is waiting is a Codex Worker, the sentence names it and its
/// brigade and says to wait first; only after that does it give the way out for a
/// Worker that never identifies (`Prefix x` on its tile, which removes it from the
/// brigade). The brigade stays staged, with the Worker's tile focused.
#[test]
fn a_waiting_worker_is_named_with_its_brigade_and_told_to_wait_before_the_close() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let worker = key("w1");
    state.unresolved_codex.remove(&pending);
    state.unresolved_codex.insert(worker.clone());
    app.replace_rows(vec![
        row_of("dir", AgentKind::ClaudeCode),
        row_of("w1", AgentKind::Codex),
        row_of("other", AgentKind::Codex),
    ]);
    select(&mut app, "other");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        now(),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert_eq!(
        state.displayed_status(),
        Some(
            "worker-1 of brigade \"dir\" isn't identified yet \u{2014} try again in a moment. If it stays that way, remove it with Ctrl+B x on its tile."
        )
    );
    assert!(
        matches!(&state.stage, Stage::Brigade { panes, focused, .. } if panes.get(*focused) == Some(&worker)),
        "{:?}",
        state.stage
    );
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.tile_label(&worker), "worker-1 \u{b7} unidentified");
}

/// The same two keys once the pane is identified: Enter shows the pane that is
/// there, `x` asks to end it, and confirming hangs that pane up. This is the
/// ordinary case the operator may have seen.
#[test]
fn once_identified_the_row_shows_the_pane_and_x_ends_it() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::DiscoveryResult {
            key: pending.clone(),
            session_id: "found".to_string(),
            member: None,
        },
        at,
    );
    select(&mut app, "found");
    state.focus = Focus::Sidebar;

    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('x'), Modifiers::NONE),
        at,
    );
    assert!(matches!(app.modal(), Some(Modal::ConfirmKill { .. })));
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at,
    );
    assert!(
        cmds.contains(&Cmd::HangUpPty { key: key("found") }),
        "{cmds:?}"
    );

    let (mut state, mut app, _) = row_listed_pane_unidentified();
    feed(
        &mut state,
        &mut app,
        Event::DiscoveryResult {
            key: pending,
            session_id: "found".to_string(),
            member: None,
        },
        at,
    );
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at,
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &key("found")), "{:?}", state.stage);
}

/// While the unidentified pane is on the stage, Prefix+`x` inside it works: the
/// dialog names the minted key and confirming hangs that pane up. This is the
/// only way to close it, and it needs the pane to be shown.
#[test]
fn prefix_x_inside_an_unidentified_pane_that_is_on_the_stage_closes_it() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    // Back on the stage: only what the stage held when it was opened.
    state.stage = Stage::Solo(pending.clone());
    state.focus = Focus::Pane;
    let at = now();
    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('b'), Modifiers::CONTROL),
        at,
    );
    input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('x'), Modifiers::NONE),
        at,
    );
    assert!(matches!(app.modal(), Some(Modal::ConfirmKill { .. })));
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at,
    );
    assert!(
        cmds.contains(&Cmd::HangUpPty {
            key: pending.clone()
        }),
        "{cmds:?}"
    );
}

// --- forming a brigade while a pane is unidentified ------------------------------

/// `B` on a Codex row while a pane is unidentified is the operator asking, so it
/// is answered like any other refusal: the pane comes forward, is marked, and the
/// line says how to go on. It used to say only "waiting to identify a new Codex
/// pane before forming a brigade", naming nothing.
#[test]
fn forming_a_brigade_on_a_codex_row_is_answered_like_any_refusal() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    select(&mut app, "found");
    state.focus = Focus::Sidebar;
    let cmds = input(
        &mut state,
        &mut app,
        key_event(KeyCode::Char('B'), Modifiers::SHIFT),
        now(),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.focus, Focus::Pane);
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
    assert_eq!(state.tile_label(&pending), "session \u{b7} unidentified");
}

/// The same refusal where the formation is saved first and staged after: the
/// brigade is saved and stays so, nothing is staged, and the operator is told
/// what the pane is waiting for, the same way, after being told it was saved
/// (or they form it again).
#[test]
fn a_saved_brigade_is_not_staged_and_the_refusal_is_the_same() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    let cmds = update_brigade_formed(
        &mut state,
        &mut app,
        FormedBrigade {
            director_row_id: "found".to_string(),
            name: "dir".to_string(),
            cwd: PathBuf::from("/work/alpha"),
            worker_agent: AgentKind::ClaudeCode,
            worker_model: String::new(),
            worker_effort: String::new(),
        },
        Ok((1, vec!["worker-1".to_string()])),
        now(),
    );
    settle_identification_refusal(&mut state, &app, now());
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(matches!(state.stage, Stage::Solo(_)), "{:?}", state.stage);
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(
        state.displayed_status().map(str::to_string),
        Some(format!("Brigade saved \u{2014} {LONE_PANE_SAID}"))
    );
}

/// The formation that waits for the Codex trust check is refused the same way
/// when the check comes back with a pane still unidentified.
#[test]
fn a_formation_waiting_on_the_trust_check_is_refused_the_same_way() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    state.pending_codex_trust_formation = Some(PendingCodexTrustFormation {
        director_row_id: "found".to_string(),
        director_agent: AgentKind::Codex,
        name: "dir".to_string(),
        cwd: PathBuf::from("/work/alpha"),
        worker_count: 1,
        worker_agent: AgentKind::Codex,
        worker_model: String::new(),
        worker_effort: String::new(),
    });
    let cmds = update_codex_trust_checked(&mut state, &mut app, true, true, now());
    settle_identification_refusal(&mut state, &app, now());
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
}

// --- review fixes -----------------------------------------------------------------

/// The operator looked at something else while the Worker's reopen was held: the
/// stage is another brigade, a lone session, or nothing. The wait ending reopens
/// the Worker all the same, as it does for a Worker of a brigade that is not shown
/// when nothing waits.
#[test]
fn a_held_worker_is_reopened_whatever_the_stage_shows_by_then() {
    for shown in ["nothing", "a lone session"] {
        let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
        let at = now();
        feed(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: key("w1"),
                reason: Some(PtyExitReason::Code(1)),
            },
            at,
        );
        state.stage = match shown {
            "nothing" => Stage::Empty,
            _ => Stage::Solo(key("dir")),
        };
        let cmds = feed(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: pending,
                reason: Some(PtyExitReason::Code(0)),
            },
            at + Duration::from_secs(1),
        );
        assert_eq!(opens(&cmds).len(), 1, "stage {shown}: {cmds:?}");
    }
}

/// A relaunch the shell turned away for a pane the core does not know of is held
/// again, and is made again only after something that can change what the shell
/// knows of: a pane ending, an identification. Not on a tick (that would be
/// refused again, every tick), and once only after that event.
#[test]
fn a_relaunch_the_shell_refused_waits_for_an_event_that_changes_its_trackers() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        at + Duration::from_secs(1),
    );
    let mut every = Vec::new();
    for second in 2..=12u64 {
        every.extend(tick(&mut state, &mut app, at + Duration::from_secs(second)));
    }
    every.extend(feed(
        &mut state,
        &mut app,
        Event::Resized {
            width: 101,
            height: 40,
        },
        at + Duration::from_secs(13),
    ));
    assert!(opens(&every).is_empty(), "{:?}", opens(&every));
    assert!(state.is_held(&worker));

    // A pane ending is such an event: the held reopen is made, once.
    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("elsewhere"),
            reason: None,
        },
        at + Duration::from_secs(14),
    );
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");
    let after = tick(&mut state, &mut app, at + Duration::from_secs(15));
    assert!(opens(&after).is_empty(), "{after:?}");
}

/// The shell turning away an open the operator asked for (staging a brigade with a
/// Codex Worker) is answered like any refusal, though the key is a member's: only
/// the Worker's own automatic reopen is told without moving anything.
#[test]
fn the_shell_refusing_a_members_open_the_operator_asked_for_is_answered_as_a_refusal() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.screens.remove(&key("w1"));
    let cmds = feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: key("w1"),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        now(),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(state.refused_for.contains(&pending));
    assert!(solo_is(&state, &pending), "{:?}", state.stage);
    assert_eq!(state.displayed_status(), Some(LONE_PANE_SAID));
}

/// Only a process that could not be started removes a Worker from its brigade.
/// The others (the banto executable was replaced, the MCP configuration could not
/// be written, the session runs in another pane, a wait that is not a failure)
/// leave the frame and the roster as they were, with the words, as an open that
/// was never a relaunch does.
#[test]
fn a_relaunch_that_fails_for_another_reason_leaves_the_worker_where_it_was() {
    for refusal in [
        SpawnRefusal::ExeUnavailable,
        SpawnRefusal::ChannelWiring,
        SpawnRefusal::AlreadyRunning,
        SpawnRefusal::Unspecified,
    ] {
        let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
        state.unresolved_codex.remove(&pending);
        let worker = key("w1");
        let at = now();
        feed(
            &mut state,
            &mut app,
            Event::PtyExited {
                key: worker.clone(),
                reason: Some(PtyExitReason::Code(1)),
            },
            at,
        );
        let cmds = feed(
            &mut state,
            &mut app,
            Event::SpawnFailed {
                key: worker.clone(),
                error: "no".to_string(),
                refusal,
                os_error: None,
            },
            at + Duration::from_secs(1),
        );
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Store(StoreIntent::DismissExitedWorker { .. }))),
            "{refusal:?}: {cmds:?}"
        );
        assert!(
            matches!(&state.stage, Stage::Brigade { panes, .. } if panes.contains(&worker)),
            "{refusal:?}: {:?}",
            state.stage
        );
        assert!(state.member_tokens.contains_key(&worker), "{refusal:?}");
        assert_eq!(
            state.displayed_status(),
            Some("failed to open: no"),
            "{refusal:?}"
        );
    }
}

/// What turns a formation away because the Director's previous process has not
/// exited is that, not an unidentified pane.
#[test]
fn a_formation_waiting_for_an_exit_says_so() {
    let (mut state, mut app, pending) = row_listed_pane_unidentified();
    state.unresolved_codex.remove(&pending);
    state.lost_output_waiting_for_exit.insert(key("found"));
    state.pending_codex_trust_formation = Some(PendingCodexTrustFormation {
        director_row_id: "found".to_string(),
        director_agent: AgentKind::Codex,
        name: "dir".to_string(),
        cwd: PathBuf::from("/work/alpha"),
        worker_count: 1,
        worker_agent: AgentKind::Codex,
        worker_model: String::new(),
        worker_effort: String::new(),
    });
    update_codex_trust_checked(&mut state, &mut app, true, true, now());
    assert_eq!(
        state.displayed_status(),
        Some("waiting for the previous process to exit before forming a brigade")
    );
}

/// A brigade the operator closes while a Worker's reopen is held has nothing to
/// reopen it into: when the wait ends, no process starts for it. Closing a brigade
/// leaves its members on the roster, so nothing else would tell the release.
#[test]
fn a_hold_of_a_brigade_that_was_closed_is_not_released_when_the_wait_ends() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("w1"),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    assert!(state.is_held(&key("w1")));

    app.open_confirm_close_brigade_modal(1, "dir".to_string());
    confirm_close_brigade_modal(&mut state, &mut app, at + Duration::from_secs(1));
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: key("dir"),
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(2),
    );
    assert!(state.closing.is_empty(), "the close has finished");

    let cmds = feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: pending,
            reason: Some(PtyExitReason::Code(0)),
        },
        at + Duration::from_secs(3),
    );
    assert!(opens(&cmds).is_empty(), "{cmds:?}");
    assert!(!state.is_held(&key("w1")));
}

/// A hold the shell refused waits for an event the core may never see (the shell
/// dropped a tracker the core had already forgotten). Enter on the tile is the
/// operator's way out: with nothing unidentified to show, it tries the reopen again,
/// and the shell turning it away again only puts it back.
#[test]
fn enter_on_a_shell_refused_hold_with_nothing_unidentified_tries_the_reopen_again() {
    let (mut state, mut app, pending) = codex_worker_beside_an_unidentified_pane();
    state.unresolved_codex.remove(&pending);
    let worker = key("w1");
    let at = now();
    feed(
        &mut state,
        &mut app,
        Event::PtyExited {
            key: worker.clone(),
            reason: Some(PtyExitReason::Code(1)),
        },
        at,
    );
    feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        at + Duration::from_secs(1),
    );
    assert!(state.is_held(&worker));
    if let Stage::Brigade { focused, .. } = &mut state.stage {
        *focused = 1;
    }
    state.focus = Focus::Pane;

    let cmds = feed(
        &mut state,
        &mut app,
        key_event(KeyCode::Enter, Modifiers::NONE),
        at + Duration::from_secs(2),
    );
    assert_eq!(opens(&cmds).len(), 1, "{cmds:?}");
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::WritePty { .. })),
        "{cmds:?}"
    );

    // Refused again: held again, quietly, until the operator asks once more.
    feed(
        &mut state,
        &mut app,
        Event::SpawnFailed {
            key: worker.clone(),
            error: CODEX_IDENTIFYING_ERROR.to_string(),
            refusal: SpawnRefusal::CodexIdentifying,
            os_error: None,
        },
        at + Duration::from_secs(3),
    );
    let after = tick(&mut state, &mut app, at + Duration::from_secs(4));
    assert!(opens(&after).is_empty(), "{after:?}");
    assert!(state.is_held(&worker));
}
