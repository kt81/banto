//! The "emporium" (大店 / `--emporium` / `--oodana`) mode: banto as a
//! persistent left sidebar (the session list) plus a right pane hosting the
//! selected session embedded. Sessions stay alive across switches (keep-alive);
//! Slice 2 (brigades — multiple visible panes) builds on that.
//!
//! A separate top-level mode chosen at launch. The classic list TUI
//! (`crate::tui`) owns the shared pieces this reuses — `App` (list state), the
//! `view` renderers, the store-load helpers, and `render_modal`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Paragraph};

use banto_core::config::{BrigadeConfig, RelayMode};
use banto_core::model::SessionId;
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::status::{AgeThresholds, ProcessProbe, SysinfoProbe, read_live_sessions};
use banto_core::store::{BrigadeId, BrigadeMember, BrigadeRole, MemberToken, Store};

use crate::app::{App, ClickOutcome, GroupJoinTarget, Modal, Mode};
use crate::opener::{self, SessionToOpen};
use crate::session::{self, SessionRow};
use crate::tui::LiveWatch;
use crate::view;

use super::pty::PortablePtyHost;
use super::render::screen_to_text;
use super::session::EmbeddedSession;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Fixed width of the left sidebar (the session list), in columns.
const SIDEBAR_WIDTH: u16 = 36;

/// Details panel height: one border row plus its content rows.
const SUMMARY_HEIGHT: u16 = 5;

/// Below this left-column height the details panel is dropped so the list keeps
/// the room.
const MIN_HEIGHT_FOR_SUMMARY: u16 = 12;

/// Which side currently receives keyboard input.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sidebar,
    Pane,
}

/// The emporium's own mutable UI state, kept apart from `App` (which holds the
/// shared list state): the kept-alive session panes, what the pane region
/// currently shows (the [`Stage`]), the focus, and a transient status line.
struct Emporium {
    /// Kept-alive embedded sessions, keyed by session id, or a synthetic key
    /// for freshly-launched ones with no id yet: `new::<cwd>` for a plain new
    /// session, `new-worker::<brigade>::<token>` for an auto-spawned brigade
    /// Worker (globally unique per member, so several Workers launched into
    /// the same cwd at once never collide the way a shared `new::<cwd>` key
    /// would — see [`discover_new_ids`]).
    ///
    /// Append-only for the lifetime of a run: sessions are never removed
    /// (disbanding a brigade only drops it from the [`Stage`], the session
    /// stays alive here). So every index held by a `Stage` stays valid.
    sessions: Vec<(String, EmbeddedSession)>,
    /// What the pane region currently shows.
    stage: Stage,
    focus: Focus,
    status: Option<String>,
    /// Freshly-launched sessions awaiting id discovery (see [`discover_new_ids`]).
    pending_new: Vec<PendingNew>,
    /// Relay engine bookkeeping per brigade member token (see [`relay_tick`]).
    relay_states: HashMap<MemberToken, RelayState>,
    /// When a key/mouse event was last forwarded to the focused pane's
    /// child — the relay engine's "not being typed into" guard (see
    /// [`should_nudge`]).
    last_forwarded_input: Option<Instant>,
}

/// What the right-hand pane region is showing: nothing, a single session, or a
/// brigade tiled across several panes.
enum Stage {
    /// Nothing staged (the "select a session" placeholder).
    Empty,
    /// A single session filling the pane.
    Solo(usize),
    /// A brigade: its members tiled with the Director first, one tile focused.
    Brigade {
        /// The persisted brigade this stage reflects.
        id: BrigadeId,
        /// Indices into [`Emporium::sessions`], Director first.
        panes: Vec<usize>,
        /// Which of `panes` currently receives input (an index into `panes`).
        focused: usize,
    },
}

impl Stage {
    /// The session index that currently receives pane input, if any.
    fn focused_index(&self) -> Option<usize> {
        match self {
            Stage::Empty => None,
            Stage::Solo(i) => Some(*i),
            Stage::Brigade { panes, focused, .. } => panes.get(*focused).copied(),
        }
    }

    /// Whether anything is staged (i.e. the pane region shows a session).
    fn is_active(&self) -> bool {
        !matches!(self, Stage::Empty)
    }
}

/// The outer (bordered) tile rects for the currently-staged sessions, each
/// paired with its index into [`Emporium::sessions`]. A solo session fills the
/// whole pane; a brigade puts the Director on the left and stacks the Workers
/// down the right (a "master + stack" layout).
fn stage_tiles(pane_area: Rect, stage: &Stage) -> Vec<(usize, Rect)> {
    match stage {
        Stage::Empty => Vec::new(),
        Stage::Solo(i) => vec![(*i, pane_area)],
        Stage::Brigade { panes, .. } => match panes.split_first() {
            None => Vec::new(),
            Some((&director, [])) => vec![(director, pane_area)],
            Some((&director, workers)) => {
                let [master, stack] =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .areas(pane_area);
                let rows = Layout::vertical(vec![
                    Constraint::Ratio(1, workers.len() as u32);
                    workers.len()
                ])
                .split(stack);
                let mut tiles = vec![(director, master)];
                for (worker, row) in workers.iter().zip(rows.iter()) {
                    tiles.push((*worker, *row));
                }
                tiles
            }
        },
    }
}

/// A new session whose real id Claude hasn't assigned yet: its synthetic
/// collection key, launch cwd, and launch time. When it's a brigade Worker
/// banto auto-spawned, `member` carries the `(brigade, token)` to persist its
/// id under once discovered (see [`discover_new_ids`]).
struct PendingNew {
    key: String,
    cwd: PathBuf,
    since: SystemTime,
    member: Option<(BrigadeId, MemberToken)>,
}

/// Which confirm branch an open modal takes — resolved before mutating `App`
/// so its `modal()` borrow doesn't overlap the mutation.
enum ModalKind {
    Archive,
    Group,
    New,
    Disband,
}

/// Run the emporium mode until the user quits (`q`/Esc from the sidebar).
/// `brigade` is `[brigade]` from config.toml: how many fresh Workers `B`
/// auto-spawns when forming a new brigade, the `--model` an auto-spawned
/// Worker launches with, and whether the relay engine (see [`relay_tick`])
/// is enabled.
pub fn run(
    claude_home: &Path,
    thresholds: &AgeThresholds,
    store: &RefCell<Store>,
    brigade: &BrigadeConfig,
) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    // Same store-backed state the classic list builds, so grouping / pins /
    // archived-hiding / brigade hiding show identically in the sidebar.
    let (rows, pinned, groups, session_groups, hidden, directors) = {
        let store = store.borrow();
        let rows = crate::tui::exclude_archived(rows, &store);
        let pinned = crate::tui::load_pinned(&store);
        let groups = crate::tui::load_groups(&store);
        let session_groups = crate::tui::load_session_groups(&store, &groups);
        let hidden = crate::tui::load_hidden_worker_ids(&store);
        let directors = crate::tui::load_directors(&store);
        (rows, pinned, groups, session_groups, hidden, directors)
    };
    let mut app = App::new(rows)
        .with_pinned(pinned)
        .with_groups(groups, session_groups)
        .with_hidden_worker_ids(hidden)
        .with_directors(directors);

    let mut terminal = setup_terminal()?;
    let result = event_loop(
        &mut terminal,
        &mut app,
        claude_home,
        thresholds,
        store,
        brigade,
    );
    let restored = restore_terminal();
    result.and(restored)
}

fn event_loop(
    terminal: &mut Tui,
    app: &mut App,
    claude_home: &Path,
    thresholds: &AgeThresholds,
    store: &RefCell<Store>,
    brigade: &BrigadeConfig,
) -> Result<()> {
    let mut ui = Emporium {
        sessions: Vec::new(),
        stage: Stage::Empty,
        focus: Focus::Sidebar,
        status: None,
        pending_new: Vec::new(),
        relay_states: HashMap::new(),
        last_forwarded_input: None,
    };
    let mut watch = LiveWatch::new(claude_home);
    let provider = ClaudeCodeProvider::new(claude_home.to_path_buf());
    let mut last_relay_tick: Option<Instant> = None;

    loop {
        let size = terminal.size()?;
        let areas = layout(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(areas.sidebar.height.saturating_sub(2) as usize);

        // Pump every session so hidden ones keep advancing; resize each staged
        // one to its own tile (solo = the whole pane; brigade = its tile).
        for (_, session) in ui.sessions.iter_mut() {
            session.pump();
        }
        for (idx, rect) in stage_tiles(areas.pane, &ui.stage) {
            let content = pane_content(rect);
            ui.sessions[idx].1.resize(content.height, content.width);
        }

        terminal.draw(|frame| draw(frame, app, &ui, areas))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if !handle_key(&mut ui, app, store, claude_home, thresholds, brigade, key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => {
                    handle_mouse(&mut ui, app, store, mouse, areas, claude_home, brigade)
                }
                _ => {}
            }
        }

        // Live updates: reload the list once the watched dirs settle.
        if watch.poll_ready(SystemTime::now()) {
            reload(app, claude_home, thresholds, store);
        }
        // Discover the ids Claude assigns to freshly-launched sessions, so they
        // can be re-selected from the sidebar without a second resume (and, for
        // a brigade Worker, so its membership row gets its real id).
        if !ui.pending_new.is_empty() {
            discover_new_ids(&mut ui, app, store, &provider);
        }
        // Relay engine tick, throttled to ~1/s (see `relay_tick`).
        let now = Instant::now();
        if last_relay_tick.is_none_or(|tick| now.duration_since(tick) >= RELAY_TICK_INTERVAL) {
            last_relay_tick = Some(now);
            relay_tick(&mut ui, store, claude_home, brigade.relay);
        }
    }
    Ok(())
}

/// Dispatch one key press. Returns `false` when the user asked to quit.
fn handle_key(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    thresholds: &AgeThresholds,
    brigade: &BrigadeConfig,
    key: KeyEvent,
) -> bool {
    let code = key.code;

    // A modal takes over all keys.
    if app.modal().is_some() {
        modal_key(ui, app, store, claude_home, thresholds, code);
        return true;
    }
    // Search mode: characters type into the query.
    if app.mode() == Mode::Search {
        search_key(app, code);
        return true;
    }
    // F2 toggles focus and is never forwarded to the child.
    if code == KeyCode::F(2) {
        ui.focus = match ui.focus {
            Focus::Sidebar if ui.stage.is_active() => Focus::Pane,
            _ => Focus::Sidebar,
        };
        return true;
    }
    // F3 cycles the focused pane within a staged brigade (never forwarded).
    if code == KeyCode::F(3) {
        if let Stage::Brigade { panes, focused, .. } = &mut ui.stage
            && !panes.is_empty()
        {
            *focused = (*focused + 1) % panes.len();
        }
        return true;
    }

    match ui.focus {
        Focus::Pane => {
            if let Some(i) = ui.stage.focused_index() {
                ui.sessions[i].1.send_key(&key);
                ui.last_forwarded_input = Some(Instant::now());
            }
        }
        Focus::Sidebar => {
            ui.status = None;
            match code {
                KeyCode::Char('q') | KeyCode::Esc => return false,
                KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                KeyCode::PageUp => app.page_up(),
                KeyCode::PageDown => app.page_down(),
                KeyCode::Home => app.select_first(),
                KeyCode::End => app.select_last(),
                KeyCode::Enter => open_or_switch(ui, app, store, claude_home, brigade),
                KeyCode::Char('B') => handle_brigade_key(ui, app, store, claude_home, brigade),
                KeyCode::Char('b') => add_worker(ui, app, store, brigade),
                KeyCode::Tab => {
                    app.toggle_grouped_view();
                }
                KeyCode::Char('/') => app.enter_search(),
                KeyCode::Char('a') => {
                    app.toggle_agent_filter();
                }
                KeyCode::Char('p') => toggle_pin(app, store),
                KeyCode::Char('d') => app.open_confirm_archive_modal(),
                KeyCode::Char('g') => app.open_group_join_modal(),
                KeyCode::Char('n') => app.open_new_session_modal(),
                _ => {}
            }
        }
    }
    true
}

/// Handle a mouse event: scroll / click the sidebar, or focus + forward to the
/// pane. A no-op while a modal is open (matching the classic mouse guard).
fn handle_mouse(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    mouse: MouseEvent,
    areas: Areas,
    claude_home: &Path,
    brigade: &BrigadeConfig,
) {
    if app.modal().is_some() {
        return;
    }
    let pos = Position::new(mouse.column, mouse.row);

    // Over the pane: focus it (and, within a brigade, the clicked tile) on a
    // left click, then forward the mouse to that tile's child once focused.
    if areas.pane.contains(pos) {
        let tiles = stage_tiles(areas.pane, &ui.stage);
        let hit = tiles.iter().find(|(_, rect)| rect.contains(pos)).copied();
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            ui.focus = Focus::Pane;
            if let Stage::Brigade { panes, focused, .. } = &mut ui.stage
                && let Some((idx, _)) = hit
                && let Some(p) = panes.iter().position(|&i| i == idx)
            {
                *focused = p;
            }
        }
        if ui.focus == Focus::Pane
            && let Some((idx, rect)) = hit
            && let Some(bytes) = mouse_to_sgr(&mouse, pane_content(rect))
        {
            ui.sessions[idx].1.send_bytes(&bytes);
            ui.last_forwarded_input = Some(Instant::now());
        }
        return;
    }

    // Over the sidebar: scroll, or click to select / open.
    let sb = areas.sidebar;
    let sidebar_inner = Rect {
        x: sb.x + 1,
        y: sb.y + 1,
        width: sb.width.saturating_sub(2),
        height: sb.height.saturating_sub(2),
    };
    match mouse.kind {
        MouseEventKind::ScrollUp if sb.contains(pos) => app.scroll(-1),
        MouseEventKind::ScrollDown if sb.contains(pos) => app.scroll(1),
        MouseEventKind::Down(MouseButton::Left) if sidebar_inner.contains(pos) => {
            ui.focus = Focus::Sidebar;
            let viewport_row = (pos.y - sidebar_inner.y) as usize;
            if app.click(viewport_row, Instant::now()) == Some(ClickOutcome::Activated) {
                open_or_switch(ui, app, store, claude_home, brigade);
            }
        }
        _ => {}
    }
}

/// Encode a mouse event as an SGR mouse report for a child whose grid starts at
/// `content` (screen coords mapped into the grid, 1-based).
fn mouse_to_sgr(mouse: &MouseEvent, content: Rect) -> Option<Vec<u8>> {
    if mouse.column < content.x || mouse.row < content.y {
        return None;
    }
    let cx = mouse.column - content.x;
    let cy = mouse.row - content.y;
    if cx >= content.width || cy >= content.height {
        return None;
    }
    let (cb, release) = match mouse.kind {
        MouseEventKind::Down(b) => (mouse_btn(b), false),
        MouseEventKind::Up(b) => (mouse_btn(b), true),
        MouseEventKind::Drag(b) => (mouse_btn(b) + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    let final_char = if release { 'm' } else { 'M' };
    Some(format!("\x1b[<{};{};{}{}", cb, cx + 1, cy + 1, final_char).into_bytes())
}

fn mouse_btn(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Enter / double-click on the sidebar: if the selected session is a brigade
/// Director, stage that whole cell; otherwise switch to the session if it's
/// already open (keeping every session alive), else open it solo in a new
/// kept-alive pane. Switching to an already-open session never re-resumes it —
/// that would fork its history (a double resume) even though banto itself is
/// what holds it. Workers never reach this: they're hidden from the list (see
/// `App::hidden`).
fn open_or_switch(
    ui: &mut Emporium,
    app: &App,
    store: &RefCell<Store>,
    claude_home: &Path,
    brigade: &BrigadeConfig,
) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let id = row.id.clone();

    // A brigade Director? Open the whole cell instead of the lone session.
    let membership = store
        .borrow()
        .brigade_of_claude_session(&SessionId(id.clone()))
        .ok()
        .flatten();
    if let Some((brigade_id, _, BrigadeRole::Director)) = membership {
        stage_brigade(ui, app, store, claude_home, brigade_id, brigade);
        return;
    }

    if let Some(i) = ensure_session_open(ui, row, claude_home, None) {
        ui.stage = Stage::Solo(i);
        ui.focus = Focus::Pane;
    }
}

/// Ensure `row`'s session is open as an embedded pane, returning its index in
/// [`Emporium::sessions`]. Reuses the pane if already open; otherwise opens it
/// (enforcing no-double-resume). When `brigade` is set, a freshly-launched
/// session is wired to its MCP channel under that `(brigade, token, role)`.
/// Returns `None` when it can't be opened (already running elsewhere, or an
/// error — a status is set in both cases).
fn ensure_session_open(
    ui: &mut Emporium,
    row: &SessionRow,
    claude_home: &Path,
    brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
) -> Option<usize> {
    let id = row.id.clone();
    if let Some(i) = ui.sessions.iter().position(|(sid, _)| *sid == id) {
        // Already kept alive: reused as-is. If it's joining a brigade now, its
        // MCP channel can't be wired without relaunching — and banto won't
        // relaunch a live session (that would fork its history).
        if brigade.is_some() {
            ui.status = Some(format!(
                "{} was already open; its brigade channel activates when reopened",
                row.display_title()
            ));
        }
        return Some(i);
    }
    let target = SessionToOpen {
        id: id.clone(),
        title: row.display_title().to_string(),
        cwd: row
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default(),
    };
    match open_embedded(&target, claude_home, brigade, &mut ui.status) {
        Ok(Some(embedded)) => {
            ui.sessions.push((id, embedded));
            Some(ui.sessions.len() - 1)
        }
        Ok(None) => None,
        Err(err) => {
            ui.status = Some(format!("failed to open: {err}"));
            None
        }
    }
}

/// `B`: on a session not yet in a brigade, form one (appoint it Director,
/// auto-spawn `brigade.worker_count()` fresh Workers). On a session that IS a
/// brigade's Director, open the disband confirmation instead. A Worker never
/// reaches here (hidden from the list), but a defensive no-op status covers
/// it in case that ever changes.
fn handle_brigade_key(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    brigade: &BrigadeConfig,
) {
    let Some(row) = app.selected_row().cloned() else {
        return;
    };
    let membership = store
        .borrow()
        .brigade_of_claude_session(&SessionId(row.id.clone()))
        .ok()
        .flatten();
    match membership {
        Some((brigade_id, _, BrigadeRole::Director)) => {
            app.open_confirm_disband_modal(brigade_id, row.display_title().to_string());
        }
        Some((_, _, BrigadeRole::Worker)) => {
            ui.status = Some("workers can't be promoted to Director directly".to_string());
        }
        None => form_brigade(ui, app, store, claude_home, brigade, &row),
    }
}

/// Form a new brigade with `row` as Director, then auto-spawn
/// `brigade.worker_count()` fresh Workers (plain `claude` processes,
/// launched with `--model brigade.worker_model` when non-empty) in its cwd,
/// each wired to the brigade's MCP channel under its own `worker-N` token.
/// The brigade is persisted immediately (schema v7), so it can be reopened
/// later from the sidebar (Enter on the Director).
fn form_brigade(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    brigade: &BrigadeConfig,
    row: &SessionRow,
) {
    let name = row.display_title().to_string();
    let cwd = row
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    // Create + persist the brigade first, so the Director launches already
    // wired to its MCP channel (its identity is passed into the launch below).
    let brigade_id = {
        let mut store = store.borrow_mut();
        match store.create_brigade(&name).and_then(|bid| {
            store.add_brigade_member(
                bid,
                "director",
                BrigadeRole::Director,
                Some(&SessionId(row.id.clone())),
            )?;
            Ok(bid)
        }) {
            Ok(bid) => bid,
            Err(err) => {
                drop(store);
                ui.status = Some(format!("failed to form brigade: {err}"));
                return;
            }
        }
    };

    let Some(director_idx) = ensure_session_open(
        ui,
        row,
        claude_home,
        Some((brigade_id, "director".to_string(), BrigadeRole::Director)),
    ) else {
        // The brigade is persisted but its Director couldn't open (status set).
        return;
    };

    let worker_count = brigade.worker_count();
    let mut panes = vec![director_idx];
    for n in 1..=worker_count {
        let token = format!("worker-{n}");
        if let Err(err) =
            store
                .borrow_mut()
                .add_brigade_member(brigade_id, &token, BrigadeRole::Worker, None)
        {
            ui.status = Some(format!("failed to add {token}: {err}"));
            continue;
        }
        if let Some(idx) = spawn_worker(ui, &cwd, brigade_id, &token, &brigade.worker_model) {
            panes.push(idx);
        }
    }

    ui.stage = Stage::Brigade {
        id: brigade_id,
        panes,
        focused: 0,
    };
    ui.focus = Focus::Pane;
    refresh_brigade_caches(app, store);
    ui.status = Some(format!(
        "brigade formed — director: {name}, {worker_count} worker(s) spawned"
    ));
}

/// `b`: spawn one more fresh Worker into the staged brigade, under the next
/// `worker-N` token. Requires a brigade to be staged.
fn add_worker(ui: &mut Emporium, app: &App, store: &RefCell<Store>, brigade: &BrigadeConfig) {
    let brigade_id = match &ui.stage {
        Stage::Brigade { id, .. } => *id,
        _ => {
            ui.status = Some("no brigade staged — press B to start one".to_string());
            return;
        }
    };
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(err) => {
            ui.status = Some(format!("failed to load brigade: {err}"));
            return;
        }
    };
    let next_n = members
        .iter()
        .filter(|m| m.role == BrigadeRole::Worker)
        .count()
        + 1;
    let token = format!("worker-{next_n}");
    let cwd = director_cwd(app, &members)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    if let Err(err) =
        store
            .borrow_mut()
            .add_brigade_member(brigade_id, &token, BrigadeRole::Worker, None)
    {
        ui.status = Some(format!("failed to add {token}: {err}"));
        return;
    }
    if let Some(idx) = spawn_worker(ui, &cwd, brigade_id, &token, &brigade.worker_model) {
        if let Stage::Brigade { panes, .. } = &mut ui.stage {
            panes.push(idx);
        }
        ui.status = Some(format!("{token} added"));
    }
}

/// The Director's cwd, if it can be resolved from the loaded session list —
/// used as the launch cwd for a newly- or re-spawned Worker.
fn director_cwd(app: &App, members: &[BrigadeMember]) -> Option<PathBuf> {
    members
        .iter()
        .find(|m| m.role == BrigadeRole::Director)
        .and_then(|m| m.claude_session_id.as_ref())
        .and_then(|sid| app.row_for_id(&sid.0))
        .and_then(|row| row.cwd.clone())
}

/// Stage brigade `brigade_id`: ensure each member is open (embedded) and show
/// them tiled with the Director focused. A member whose Claude session can't
/// be resolved (a Worker still awaiting id discovery, or one whose session
/// file is gone from disk) is re-spawned fresh under its same token, since a
/// Worker is disposable — only the Director not resolving counts as missing.
fn stage_brigade(
    ui: &mut Emporium,
    app: &App,
    store: &RefCell<Store>,
    claude_home: &Path,
    brigade_id: BrigadeId,
    brigade: &BrigadeConfig,
) {
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(err) => {
            ui.status = Some(format!("failed to load brigade: {err}"));
            return;
        }
    };
    let cwd = director_cwd(app, &members)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    let mut panes = Vec::new();
    let mut missing = 0;
    for member in &members {
        let resolved_row = member
            .claude_session_id
            .as_ref()
            .and_then(|sid| app.row_for_id(&sid.0));
        match resolved_row {
            Some(row) => {
                if let Some(idx) = ensure_session_open(
                    ui,
                    row,
                    claude_home,
                    Some((brigade_id, member.token.clone(), member.role)),
                ) && !panes.contains(&idx)
                {
                    panes.push(idx);
                }
            }
            None if member.role == BrigadeRole::Worker => {
                if let Some(idx) =
                    spawn_worker(ui, &cwd, brigade_id, &member.token, &brigade.worker_model)
                {
                    panes.push(idx);
                }
            }
            None => missing += 1,
        }
    }
    if panes.is_empty() {
        ui.status = Some("no brigade members could be opened".to_string());
        return;
    }
    ui.stage = Stage::Brigade {
        id: brigade_id,
        panes,
        focused: 0,
    };
    ui.focus = Focus::Pane;
    if missing > 0 {
        ui.status = Some(format!("brigade staged ({missing} member(s) not found)"));
    }
}

/// Spawn a fresh, plain `claude` process in `cwd` as a Worker under `token`,
/// wired to the brigade's MCP channel, and register it for id discovery
/// ([`PendingNew`]) so its Claude-assigned session id gets recorded via
/// `Store::set_member_claude_session` once known (see [`discover_new_ids`]).
/// `worker_model` is `[brigade].worker_model`: appended as `--model
/// <worker_model>` when non-empty; an empty string is the escape hatch to
/// inherit the operator's default model (no flag passed at all). Returns its
/// index in [`Emporium::sessions`], or `None` on failure (a status is set).
fn spawn_worker(
    ui: &mut Emporium,
    cwd: &Path,
    brigade_id: BrigadeId,
    token: &str,
    worker_model: &str,
) -> Option<usize> {
    let mut argv = opener::inplace_argv(None);
    if !worker_model.is_empty() {
        argv.push("--model".to_string());
        argv.push(worker_model.to_string());
    }
    match write_mcp_config(brigade_id, token, BrigadeRole::Worker, None) {
        Ok(path) => {
            argv.push("--mcp-config".to_string());
            argv.push(path.to_string_lossy().into_owned());
        }
        Err(err) => {
            ui.status = Some(format!("brigade channel unavailable for {token}: {err}"));
        }
    }
    let since = SystemTime::now();
    match EmbeddedSession::open(&PortablePtyHost, &argv, Some(cwd), 24, 80) {
        Ok(embedded) => {
            let key = format!("new-worker::{brigade_id}::{token}");
            ui.sessions.push((key.clone(), embedded));
            ui.pending_new.push(PendingNew {
                key,
                cwd: cwd.to_path_buf(),
                since,
                member: Some((brigade_id, token.to_string())),
            });
            Some(ui.sessions.len() - 1)
        }
        Err(err) => {
            ui.status = Some(format!("failed to spawn {token}: {err}"));
            None
        }
    }
}

/// Refresh `App`'s hidden-worker/director id caches from the store — called
/// after any brigade mutation (formation, disband) so the list reflects it
/// immediately rather than waiting for the next filesystem-triggered reload.
fn refresh_brigade_caches(app: &mut App, store: &RefCell<Store>) {
    let store = store.borrow();
    app.set_hidden_worker_ids(crate::tui::load_hidden_worker_ids(&store));
    app.set_directors(crate::tui::load_directors(&store));
}

/// Spawn `session` in a new embedded pane, enforcing the no-double-resume guard
/// (reusing the classic in-place decision). Returns `None` (and sets a status)
/// when it's already running elsewhere. When `brigade` is set, the launch is
/// wired to banto's own MCP server so the session can message its peer.
fn open_embedded(
    session: &SessionToOpen,
    claude_home: &Path,
    brigade: Option<(BrigadeId, MemberToken, BrigadeRole)>,
    status: &mut Option<String>,
) -> Result<Option<EmbeddedSession>> {
    let live = read_live_sessions(&claude_home.join("sessions"));
    let Some(launch) = opener::decide_inplace_resume(session, &SysinfoProbe, &live) else {
        *status = Some("already running elsewhere".to_string());
        return Ok(None);
    };
    let mut argv = launch.argv;
    // A brigade member connects to banto's own MCP server (`banto _mcp`) so it
    // can message its peer. banto owns the launch argv, so the config file it
    // points at lives under banto's data dir — never under ~/.claude.
    if let Some((brigade_id, token, role)) = brigade {
        match write_mcp_config(brigade_id, &token, role, Some(&session.id)) {
            Ok(path) => {
                argv.push("--mcp-config".to_string());
                argv.push(path.to_string_lossy().into_owned());
            }
            Err(err) => *status = Some(format!("brigade channel unavailable: {err}")),
        }
    }
    // Size is corrected on the next loop tick from the real pane geometry.
    let embedded = EmbeddedSession::open(&PortablePtyHost, &argv, Some(&launch.cwd), 24, 80)?;
    Ok(Some(embedded))
}

/// Write a per-member `--mcp-config` file wiring the embedded claude to banto's
/// own MCP server (`banto _mcp`) with this member's brigade identity, and
/// return its path. Named by `(brigade_id, token)` rather than the Claude
/// session id, since that's the only identity known upfront for a
/// freshly-spawned Worker (`claude_session_id` is `None` until Claude assigns
/// one). Lives under banto's own data dir, never under `~/.claude`.
fn write_mcp_config(
    brigade_id: BrigadeId,
    token: &str,
    role: BrigadeRole,
    claude_session_id: Option<&str>,
) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let role_token = match role {
        BrigadeRole::Director => "director",
        BrigadeRole::Worker => "worker",
    };
    let mut args = vec![
        "_mcp".to_string(),
        "--brigade".to_string(),
        brigade_id.to_string(),
        "--member".to_string(),
        token.to_string(),
        "--role".to_string(),
        role_token.to_string(),
    ];
    if let Some(session_id) = claude_session_id {
        args.push("--session".to_string());
        args.push(session_id.to_string());
    }
    let config = serde_json::json!({
        "mcpServers": {
            "banto": {
                "command": exe.to_string_lossy(),
                "args": args,
            }
        }
    });
    let dir = dirs::data_local_dir()
        .map(|base| base.join("banto").join("mcp"))
        .ok_or_else(|| anyhow::anyhow!("could not determine banto's data directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{brigade_id}-{}.json", sanitize_filename(token)));
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}

/// Keep a token safe as a filename component (tokens are `director`/`worker-N`,
/// but be defensive).
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Toggle the selected session's pin and persist it (mirrors the classic
/// `toggle_pin`). No status bar in this layout, so the re-sort is the feedback.
fn toggle_pin(app: &mut App, store: &RefCell<Store>) {
    let Some((id, now_pinned)) = app.toggle_pin() else {
        return;
    };
    let store = store.borrow();
    let _ = if now_pinned {
        store.pin(&SessionId(id))
    } else {
        store.unpin(&SessionId(id))
    };
}

/// Reload the session list from disk (after an archive, so it disappears
/// immediately) — the emporium counterpart of the classic `reload`. Also
/// refreshes the hidden-worker/director id sets.
fn reload(app: &mut App, claude_home: &Path, thresholds: &AgeThresholds, store: &RefCell<Store>) {
    if let Ok(rows) = session::load_rows(claude_home, thresholds) {
        let store = store.borrow();
        let rows = crate::tui::exclude_archived(rows, &store);
        app.replace_rows(rows);
        app.set_hidden_worker_ids(crate::tui::load_hidden_worker_ids(&store));
        app.set_directors(crate::tui::load_directors(&store));
    }
}

/// Search-mode keys: type into / edit the query (mirrors classic
/// `handle_search_key`).
fn search_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward(),
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home => app.move_cursor_home(),
        KeyCode::End => app.move_cursor_end(),
        KeyCode::Enter => app.confirm_search(),
        KeyCode::Esc => app.exit_search(),
        KeyCode::Char(c) => app.push_char(c),
        _ => {}
    }
}

/// Modal keys: edit the modal's input / candidate selection, confirm, or
/// cancel (mirrors classic `handle_modal_key`; confirm is emporium-specific
/// since a new session opens embedded here, not in-place/split).
fn modal_key(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    thresholds: &AgeThresholds,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => app.close_modal(),
        KeyCode::Up => app.modal_select_prev(),
        KeyCode::Down => app.modal_select_next(),
        KeyCode::Left => app.modal_cursor_left(),
        KeyCode::Right => app.modal_cursor_right(),
        KeyCode::Home => app.modal_cursor_home(),
        KeyCode::End => app.modal_cursor_end(),
        KeyCode::Tab => app.modal_complete_candidate(),
        KeyCode::Backspace => app.modal_backspace(),
        KeyCode::Delete => app.modal_delete_forward(),
        KeyCode::Enter => confirm_modal(ui, app, store, claude_home, thresholds),
        KeyCode::Char(c) => app.modal_push_char(c),
        _ => {}
    }
}

fn confirm_modal(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    thresholds: &AgeThresholds,
) {
    // Resolve the kind first so `app.modal()`'s borrow doesn't overlap the
    // mutations each branch performs.
    let kind = match app.modal() {
        Some(Modal::ConfirmArchive { .. }) => Some(ModalKind::Archive),
        Some(Modal::GroupJoin(_)) => Some(ModalKind::Group),
        Some(Modal::NewSession(_)) => Some(ModalKind::New),
        Some(Modal::ConfirmDisband { .. }) => Some(ModalKind::Disband),
        None => None,
    };
    match kind {
        Some(ModalKind::Archive) => confirm_archive(ui, app, store, claude_home, thresholds),
        Some(ModalKind::Group) => confirm_group_join(ui, app, store),
        Some(ModalKind::New) => confirm_new_embedded(ui, app),
        Some(ModalKind::Disband) => confirm_disband(ui, app, store),
        None => {}
    }
}

/// Confirm the archive dialog: soft-hide via the store, then reload so it
/// leaves the list immediately.
fn confirm_archive(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    claude_home: &Path,
    thresholds: &AgeThresholds,
) {
    let Some(Modal::ConfirmArchive { session_id, title }) = app.modal() else {
        return;
    };
    let session_id = session_id.clone();
    let title = title.clone();
    let result = store.borrow().archive_session(&SessionId(session_id));
    ui.status = Some(match result {
        Ok(()) => format!("archived {title}"),
        Err(err) => format!("failed to archive {title}: {err}"),
    });
    app.close_modal();
    reload(app, claude_home, thresholds, store);
}

/// Confirm the group-join dialog: join the highlighted group or create+join a
/// new one (mirrors classic `confirm_group_join_modal`).
fn confirm_group_join(ui: &mut Emporium, app: &mut App, store: &RefCell<Store>) {
    let Some(Modal::GroupJoin(state)) = app.modal() else {
        return;
    };
    let session_id = state.session_id().to_string();
    let Some(target) = app.modal_group_join_target() else {
        return;
    };

    let mut store = store.borrow_mut();
    let (group_id, group_name, result) = match target {
        GroupJoinTarget::Existing(group_id, name) => {
            let result = store.set_session_group(&SessionId(session_id.clone()), group_id);
            (group_id, name, result)
        }
        GroupJoinTarget::New(name) => match store.create_group(&name) {
            Ok(group_id) => {
                let result = store.set_session_group(&SessionId(session_id.clone()), group_id);
                (group_id, name, result)
            }
            Err(err) => {
                drop(store);
                ui.status = Some(format!("failed to create group \"{name}\": {err}"));
                app.close_modal();
                return;
            }
        },
    };
    drop(store);

    ui.status = Some(match &result {
        Ok(()) => format!("joined group \"{group_name}\""),
        Err(err) => format!("failed to join group \"{group_name}\": {err}"),
    });
    if result.is_ok() {
        app.set_session_group_cache(&session_id, group_id, group_name);
    }
    app.close_modal();
}

/// Confirm the new-session dialog: launch a fresh `claude` in the chosen cwd as
/// an embedded pane (emporium's answer to the classic in-place/split new).
fn confirm_new_embedded(ui: &mut Emporium, app: &mut App) {
    let Some(Modal::NewSession(_)) = app.modal() else {
        return;
    };
    let Some(cwd) = app.modal_new_session_target() else {
        return;
    };
    if !cwd.is_dir() {
        app.modal_set_error(format!("{} is not a directory", cwd.display()));
        return;
    }
    let argv = opener::inplace_argv(None);
    let since = SystemTime::now();
    match EmbeddedSession::open(&PortablePtyHost, &argv, Some(&cwd), 24, 80) {
        Ok(embedded) => {
            // No session id yet (Claude assigns it). Key it with a synthetic
            // `new::<cwd>` (never collides with a real UUID id) and record it
            // for id discovery, so it can later be re-selected from the sidebar
            // without a second resume.
            let key = format!("new::{}", cwd.display());
            ui.sessions.push((key.clone(), embedded));
            ui.stage = Stage::Solo(ui.sessions.len() - 1);
            ui.focus = Focus::Pane;
            ui.pending_new.push(PendingNew {
                key,
                cwd,
                since,
                member: None,
            });
        }
        Err(err) => ui.status = Some(format!("failed to start a new session: {err}")),
    }
    app.close_modal();
}

/// Confirm the disband dialog: purge the brigade (schema v7: membership,
/// messages, cursors), refresh the hidden-worker/director caches so its
/// Workers reappear in the list immediately, and fall back the stage to the
/// Director alone. Its Workers' `claude` processes keep running — they simply
/// reappear as ordinary live sessions (the emporium's append-only
/// keep-alive invariant never kills a session).
fn confirm_disband(ui: &mut Emporium, app: &mut App, store: &RefCell<Store>) {
    let Some(Modal::ConfirmDisband { brigade_id, .. }) = app.modal() else {
        return;
    };
    let brigade_id = *brigade_id;
    let director_idx = match &ui.stage {
        Stage::Brigade { id, panes, .. } if *id == brigade_id => panes.first().copied(),
        _ => None,
    };
    if let Err(err) = store.borrow_mut().delete_brigade(brigade_id) {
        ui.status = Some(format!("failed to disband: {err}"));
        app.close_modal();
        return;
    }
    refresh_brigade_caches(app, store);
    ui.stage = match director_idx {
        Some(idx) => Stage::Solo(idx),
        None => Stage::Empty,
    };
    ui.status = Some("brigade disbanded".to_string());
    app.close_modal();
}

/// Poll for the ids Claude assigns to freshly-launched sessions and re-key their
/// collection entries from their synthetic key to the real id, so they behave
/// like any other open session (re-selectable, no second resume). A batch of
/// brigade Workers auto-spawned into the same cwd at once is disambiguated by
/// fetching every matching candidate (`find_new_sessions`, not the single-best
/// `find_new_session`) and greedily assigning each to a still-pending entry,
/// skipping ids already claimed by another open session — otherwise every
/// pending entry sharing that cwd would independently resolve to the same
/// "newest" file. A Worker's discovered id is also persisted via
/// `Store::set_member_claude_session`, and the hidden-worker cache is
/// refreshed right away so it disappears from the list on this same tick
/// rather than waiting for the next filesystem-triggered reload.
fn discover_new_ids(
    ui: &mut Emporium,
    app: &mut App,
    store: &RefCell<Store>,
    provider: &ClaudeCodeProvider,
) {
    // (pending's synthetic key, its discovered id, its brigade member if any)
    type Resolved = (String, String, Option<(BrigadeId, MemberToken)>);

    let claimed: HashSet<String> = ui.sessions.iter().map(|(k, _)| k.clone()).collect();
    let mut used_this_pass: HashSet<String> = HashSet::new();
    let mut resolved: Vec<Resolved> = Vec::new();

    for pending in &ui.pending_new {
        let id = provider
            .find_new_sessions(&pending.cwd, pending.since)
            .into_iter()
            .map(|id| id.0)
            .find(|id| !claimed.contains(id) && !used_this_pass.contains(id));
        if let Some(id) = id {
            used_this_pass.insert(id.clone());
            resolved.push((pending.key.clone(), id, pending.member.clone()));
        }
    }
    if resolved.is_empty() {
        return;
    }
    let mut any_member = false;
    for (key, id, member) in &resolved {
        if let Some(entry) = ui.sessions.iter_mut().find(|(k, _)| k == key) {
            entry.0 = id.clone();
        }
        if let Some((brigade_id, token)) = member {
            any_member = true;
            let _ = store.borrow_mut().set_member_claude_session(
                *brigade_id,
                token,
                &SessionId(id.clone()),
            );
        }
    }
    ui.pending_new
        .retain(|pending| !resolved.iter().any(|(key, _, _)| key == &pending.key));
    if any_member {
        refresh_brigade_caches(app, store);
    }
}

// --- Relay engine ----------------------------------------------------------
//
// Today a queued brigade message sits until a human prompts the recipient to
// call `check_messages`. The relay engine closes that gap: once banto sees a
// staged member has unseen messages and is idle at its prompt, it types a
// short fixed line into that member's stdin to start a turn, and the member
// pulls the real message itself via the tool. The nudge is a control-plane
// push only — the message body never goes through stdin (attribution of who
// actually said what, the exactly-once queue semantics, and the fragility of
// injecting arbitrary text into a live TUI's input all forbid it).

/// How often [`relay_tick`] re-evaluates the staged brigade's members.
const RELAY_TICK_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive relay ticks a member must be observed idle before it's
/// eligible for a nudge — a single idle observation could be a live-session
/// file caught mid-update.
const RELAY_IDLE_STREAK_REQUIRED: u32 = 2;
/// How long a focused pane's own recently-forwarded input suppresses a nudge
/// to it, so the relay never interrupts the user's own typing.
const RELAY_INPUT_QUIET_PERIOD: Duration = Duration::from_secs(3);
/// Minimum gap between nudges to the same member. The first nudge in a batch
/// (no prior nudge recorded) is exempt from this wait.
const RELAY_NUDGE_COOLDOWN: Duration = Duration::from_secs(60);
/// Give up nudging a member after this many attempts on one batch of unseen
/// messages. It becomes eligible again once the batch drains (a human or the
/// member itself calls `check_messages`) and a new message arrives.
const RELAY_MAX_ATTEMPTS: u32 = 3;
/// The fixed, ASCII-only line typed into a nudged member's stdin, followed by
/// a lone `\r` to submit it (see [`relay_tick`]).
const RELAY_NUDGE_LINE: &str =
    "[banto relay] Your brigade peer sent you a message. Call the check_messages tool now.";

/// A member's nudge backoff: when it was last nudged and how many attempts
/// have been made since its unseen-message batch was last seen drained.
#[derive(Debug, Default, Clone, Copy)]
struct NudgeState {
    last_nudge: Option<Instant>,
    attempts: u32,
}

/// Per-member relay bookkeeping, keyed by member token in
/// [`Emporium::relay_states`].
#[derive(Debug, Default, Clone, Copy)]
struct RelayState {
    /// Consecutive relay ticks this member has been observed idle.
    idle_streak: u32,
    nudge: NudgeState,
}

/// The relay engine's go/no-go decision for nudging one member on this tick.
/// Pure and side-effect free: `idle_streak` and `state` are this member's
/// already-updated bookkeeping (see [`tick_relay_decision`], which owns
/// advancing them from the tick's raw observations).
fn should_nudge(
    now: Instant,
    idle_streak: u32,
    is_focused: bool,
    last_forwarded_input: Option<Instant>,
    has_unseen: bool,
    state: &NudgeState,
) -> bool {
    if !has_unseen || idle_streak < RELAY_IDLE_STREAK_REQUIRED {
        return false;
    }
    if is_focused
        && let Some(last_input) = last_forwarded_input
        && now.saturating_duration_since(last_input) < RELAY_INPUT_QUIET_PERIOD
    {
        return false;
    }
    if state.attempts >= RELAY_MAX_ATTEMPTS {
        return false;
    }
    if let Some(last_nudge) = state.last_nudge
        && now.saturating_duration_since(last_nudge) < RELAY_NUDGE_COOLDOWN
    {
        return false;
    }
    true
}

/// Advance `token`'s entry in `states` for one relay tick and decide whether
/// to nudge it now (see [`should_nudge`]). `is_idle_this_tick` is `None` when
/// the member's live-session entry is missing or unmatched ("unknown" —
/// never counted toward the idle streak, per the relay spec). `has_unseen`
/// going `false` drops the entry entirely, so once the member (or a human)
/// drains its queue, the next message it gets starts an entirely fresh batch
/// — a fresh idle streak and no cooldown/attempt count carried over.
fn tick_relay_decision(
    states: &mut HashMap<MemberToken, RelayState>,
    token: &MemberToken,
    now: Instant,
    is_idle_this_tick: Option<bool>,
    is_focused: bool,
    last_forwarded_input: Option<Instant>,
    has_unseen: bool,
) -> bool {
    if !has_unseen {
        states.remove(token);
        return false;
    }
    let state = states.entry(token.clone()).or_default();
    state.idle_streak = if is_idle_this_tick == Some(true) {
        state.idle_streak + 1
    } else {
        0
    };
    let nudge = should_nudge(
        now,
        state.idle_streak,
        is_focused,
        last_forwarded_input,
        has_unseen,
        &state.nudge,
    );
    if nudge {
        state.nudge.last_nudge = Some(now);
        state.nudge.attempts += 1;
    }
    nudge
}

/// Relay engine tick, called from the main loop at [`RELAY_TICK_INTERVAL`]: a
/// no-op unless a brigade is staged and `relay == RelayMode::Auto`. For each
/// of that brigade's members with a known Claude session id and an open pane
/// among the staged ones, checks whether it has unseen messages addressed to
/// its role (`Store::has_unseen_brigade_messages` — read-only, never
/// advances the recipient's cursor) and whether its live-session entry shows
/// an alive, non-busy pid, then feeds both into [`tick_relay_decision`]. When
/// that says to nudge, types [`RELAY_NUDGE_LINE`] into the member's stdin via
/// the pane's existing `send_bytes`, followed by a lone `\r` to submit it,
/// and sets a status message. The stdin write and the live-session read
/// themselves are not unit-tested (this file's existing testing boundary for
/// child-process I/O); the decision logic above is.
fn relay_tick(ui: &mut Emporium, store: &RefCell<Store>, claude_home: &Path, relay: RelayMode) {
    if relay != RelayMode::Auto {
        return;
    }
    let (brigade_id, panes, focused_pane) = match &ui.stage {
        Stage::Brigade { id, panes, focused } => (*id, panes.clone(), panes.get(*focused).copied()),
        _ => return,
    };
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(_) => return,
    };
    let live = read_live_sessions(&claude_home.join("sessions"));
    let now = Instant::now();

    for member in &members {
        let Some(claude_session_id) = member.claude_session_id.as_ref() else {
            continue;
        };
        let Some(idx) = ui
            .sessions
            .iter()
            .position(|(key, _)| key == &claude_session_id.0)
        else {
            continue;
        };
        if !panes.contains(&idx) {
            continue;
        }
        let has_unseen = store
            .borrow()
            .has_unseen_brigade_messages(brigade_id, &member.token, member.role)
            .unwrap_or(false);
        let is_idle_this_tick = live
            .iter()
            .find(|entry| entry.session_id.as_deref() == Some(claude_session_id.0.as_str()))
            .map(|entry| {
                SysinfoProbe.is_alive(entry.pid) && entry.status.as_deref() != Some("busy")
            });
        let is_focused = ui.focus == Focus::Pane && focused_pane == Some(idx);

        let nudge = tick_relay_decision(
            &mut ui.relay_states,
            &member.token,
            now,
            is_idle_this_tick,
            is_focused,
            ui.last_forwarded_input,
            has_unseen,
        );
        if nudge {
            ui.sessions[idx].1.send_bytes(RELAY_NUDGE_LINE.as_bytes());
            ui.sessions[idx].1.send_bytes(b"\r");
            ui.status = Some(format!("relay: nudged {}", member.token));
        }
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App, ui: &Emporium, areas: Areas) {
    let full_area = frame.area();
    let focus = ui.focus;

    // Sidebar: bordered block with the query in its title while searching.
    let sidebar_title = if app.mode() == Mode::Search {
        format!("/ {}", app.query())
    } else {
        "banto".to_string()
    };
    let sidebar_block = Block::bordered()
        .title(sidebar_title)
        .border_style(border_style(focus == Focus::Sidebar));
    let sidebar_inner = sidebar_block.inner(areas.sidebar);
    frame.render_widget(sidebar_block, areas.sidebar);
    view::render_list(frame, app, sidebar_inner);

    // Details panel below the list (shared with the classic summary).
    view::render_summary(frame, app, areas.summary);

    // Right region: the staged session(s), tiled — or a placeholder when
    // nothing is staged.
    let tiles = stage_tiles(areas.pane, &ui.stage);
    if tiles.is_empty() {
        let block = Block::bordered()
            .title("session")
            .border_style(border_style(false));
        let content = block.inner(areas.pane);
        frame.render_widget(block, areas.pane);
        frame.render_widget(
            Paragraph::new(
                "Select a session and press Enter.\n\
                 F2 toggles focus · B starts a brigade · q quits.",
            ),
            content,
        );
    } else {
        let focused_index = ui.stage.focused_index();
        for (idx, rect) in &tiles {
            let session = &ui.sessions[*idx].1;
            let focused_tile = focus == Focus::Pane && focused_index == Some(*idx);
            let block = Block::bordered()
                .title(tile_title(&ui.stage, *idx))
                .border_style(border_style(focused_tile));
            let content = block.inner(*rect);
            frame.render_widget(block, *rect);
            frame.render_widget(Paragraph::new(screen_to_text(session.screen())), content);
            if focused_tile && !session.screen().hide_cursor() {
                let (cursor_row, cursor_col) = session.screen().cursor_position();
                let (x, y) = (content.x + cursor_col, content.y + cursor_row);
                if x < content.x + content.width && y < content.y + content.height {
                    frame.set_cursor_position(Position::new(x, y));
                }
            }
        }
    }

    render_status_bar(frame, app, ui.status.as_deref(), areas.status);

    // A modal overlays everything, reusing the classic modal rendering.
    if let Some(modal) = app.modal() {
        crate::tui::render_modal(frame, modal, full_area);
    }
}

/// The title shown on a staged tile: its role within a brigade ("director" /
/// "worker N"), or just "session" for a solo pane.
fn tile_title(stage: &Stage, session_index: usize) -> String {
    match stage {
        Stage::Brigade { panes, .. } => match panes.iter().position(|&i| i == session_index) {
            Some(0) => "director".to_string(),
            Some(n) => format!("worker {n}"),
            _ => "session".to_string(),
        },
        _ => "session".to_string(),
    }
}

/// Bottom status bar: emporium key hints (or a transient status) on the left,
/// the match count on the right — the emporium counterpart of the classic
/// `render_status` (its own hints, and its own status line).
fn render_status_bar(frame: &mut ratatui::Frame, app: &App, status: Option<&str>, area: Rect) {
    const NORMAL_HINTS: &str = "j/k move · Enter open · F2 focus · B brigade/disband · b +worker · \
                                F3 pane · / search · n new · d archive · g group · Tab view · \
                                p pin · a agents · q quit";
    const SEARCH_HINTS: &str = "type to search · Enter confirm · Esc cancel";

    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    let counts_width = counts.chars().count() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);

    let (text, color) = match status {
        Some(message) => (message.to_string(), Color::Yellow),
        None => {
            let hints = if app.mode() == Mode::Search {
                SEARCH_HINTS
            } else {
                NORMAL_HINTS
            };
            (hints.to_string(), Color::Gray)
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(color))),
        left,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            counts,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        right,
    );
}

fn border_style(focused: bool) -> Style {
    Style::default().fg(if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    })
}

/// The regions of the emporium layout.
#[derive(Clone, Copy)]
struct Areas {
    /// Bordered sidebar block holding the session list.
    sidebar: Rect,
    /// Details / summary panel below the list (0-height in a short terminal).
    summary: Rect,
    /// Right pane hosting the session.
    pane: Rect,
    /// Bottom status bar (one row).
    status: Rect,
}

/// Compute the layout: a bottom status bar, and above it a left column
/// (sidebar list + details panel) beside the session pane.
fn layout(area: Rect) -> Areas {
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let [left, pane] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]).areas(body);
    let summary_h = if left.height < MIN_HEIGHT_FOR_SUMMARY {
        0
    } else {
        SUMMARY_HEIGHT
    };
    let [sidebar, summary] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(summary_h)]).areas(left);
    Areas {
        sidebar,
        summary,
        pane,
        status,
    }
}

/// The inner content rect of the right pane (inside its border).
fn pane_content(pane_area: Rect) -> Rect {
    Rect {
        x: pane_area.x + 1,
        y: pane_area.y + 1,
        width: pane_area.width.saturating_sub(2).max(1),
        height: pane_area.height.saturating_sub(2).max(1),
    }
}

fn setup_terminal() -> Result<Tui> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            original(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use ratatui::layout::Rect;

    use super::{
        MIN_HEIGHT_FOR_SUMMARY, NudgeState, RELAY_IDLE_STREAK_REQUIRED, RELAY_INPUT_QUIET_PERIOD,
        RELAY_MAX_ATTEMPTS, RELAY_NUDGE_COOLDOWN, RelayState, SIDEBAR_WIDTH, SUMMARY_HEIGHT, Stage,
        layout, pane_content, should_nudge, stage_tiles, tick_relay_decision,
    };

    #[test]
    fn layout_reserves_sidebar_status_bar_and_details_panel() {
        let areas = layout(Rect::new(0, 0, 120, 40));
        // A one-row status bar at the bottom; the rest is the body.
        assert_eq!(areas.status.height, 1);
        assert_eq!(areas.status.y, 39);
        // Left column of SIDEBAR_WIDTH; the pane takes the rest.
        assert_eq!(areas.sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(areas.pane.x, SIDEBAR_WIDTH);
        assert_eq!(areas.pane.width, 120 - SIDEBAR_WIDTH);
        assert_eq!(areas.pane.height, 39);
        // Tall enough for the details panel, so it takes SUMMARY_HEIGHT.
        assert_eq!(areas.summary.height, SUMMARY_HEIGHT);
    }

    #[test]
    fn layout_drops_the_details_panel_when_short() {
        // Body height (total - status row) ends up below MIN_HEIGHT_FOR_SUMMARY.
        let areas = layout(Rect::new(0, 0, 120, MIN_HEIGHT_FOR_SUMMARY));
        assert_eq!(areas.summary.height, 0);
    }

    #[test]
    fn pane_content_shrinks_by_the_border() {
        let content = pane_content(Rect::new(36, 0, 84, 40));
        assert_eq!(content.x, 37);
        assert_eq!(content.y, 1);
        assert_eq!(content.width, 82);
        assert_eq!(content.height, 38);
    }

    #[test]
    fn solo_stage_fills_the_whole_pane() {
        let area = Rect::new(36, 0, 84, 39);
        assert_eq!(stage_tiles(area, &Stage::Solo(2)), vec![(2, area)]);
    }

    #[test]
    fn empty_stage_has_no_tiles() {
        let area = Rect::new(36, 0, 84, 39);
        assert!(stage_tiles(area, &Stage::Empty).is_empty());
    }

    #[test]
    fn brigade_with_one_member_fills_the_pane() {
        let area = Rect::new(36, 0, 84, 39);
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![5],
            focused: 0,
        };
        assert_eq!(stage_tiles(area, &stage), vec![(5, area)]);
    }

    #[test]
    fn brigade_tiles_director_left_and_stacks_workers_right() {
        let area = Rect::new(36, 0, 84, 40);
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![5, 6, 7],
            focused: 0,
        };
        let tiles = stage_tiles(area, &stage);
        assert_eq!(tiles.len(), 3);

        // Director takes the left half; its session index leads.
        let (director_idx, director_rect) = tiles[0];
        assert_eq!(director_idx, 5);
        assert_eq!(director_rect.x, 36);
        assert_eq!(director_rect.width, 42);
        assert_eq!(director_rect.height, 40);

        // Workers share the right half, stacked top-to-bottom in order.
        let (w0_idx, w0) = tiles[1];
        let (w1_idx, w1) = tiles[2];
        assert_eq!((w0_idx, w1_idx), (6, 7));
        assert_eq!(w0.x, 78);
        assert_eq!(w1.x, 78);
        assert_eq!(w0.width, 42);
        assert!(w1.y > w0.y, "workers stack downward");
        assert_eq!(w0.height + w1.height, 40, "workers fill the right column");
    }

    #[test]
    fn focused_index_tracks_the_focused_pane() {
        assert_eq!(Stage::Empty.focused_index(), None);
        assert_eq!(Stage::Solo(3).focused_index(), Some(3));
        let stage = Stage::Brigade {
            id: 1,
            panes: vec![5, 6, 7],
            focused: 2,
        };
        assert_eq!(stage.focused_index(), Some(7));
    }

    // --- Relay engine: should_nudge / tick_relay_decision -------------------

    #[test]
    fn should_nudge_happy_path() {
        let now = Instant::now();
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_blocks_without_unseen_messages() {
        let now = Instant::now();
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            false,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_busy_blocks() {
        // A member observed busy never accumulates an idle streak — modeled
        // here as idle_streak staying at 0.
        let now = Instant::now();
        assert!(!should_nudge(
            now,
            0,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_single_tick_idle_blocks_debounce() {
        let now = Instant::now();
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED - 1,
            false,
            None,
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_focused_with_recent_input_blocks() {
        let now = Instant::now();
        let last_input = now - Duration::from_millis(500);
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            true,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_focused_without_recent_input_is_allowed() {
        let now = Instant::now();
        let last_input = now - RELAY_INPUT_QUIET_PERIOD - Duration::from_secs(1);
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            true,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_unfocused_ignores_recent_input() {
        let now = Instant::now();
        let last_input = now - Duration::from_millis(10);
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            Some(last_input),
            true,
            &NudgeState::default(),
        ));
    }

    #[test]
    fn should_nudge_attempt_cap_blocks() {
        let now = Instant::now();
        let state = NudgeState {
            last_nudge: Some(now - RELAY_NUDGE_COOLDOWN - Duration::from_secs(1)),
            attempts: RELAY_MAX_ATTEMPTS,
        };
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_cooldown_blocks_a_too_soon_second_attempt() {
        let now = Instant::now();
        let state = NudgeState {
            last_nudge: Some(now - Duration::from_secs(10)),
            attempts: 1,
        };
        assert!(!should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_cooldown_elapsed_allows_another_attempt() {
        let now = Instant::now();
        let state = NudgeState {
            last_nudge: Some(now - RELAY_NUDGE_COOLDOWN - Duration::from_secs(1)),
            attempts: 1,
        };
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &state,
        ));
    }

    #[test]
    fn should_nudge_first_nudge_is_exempt_from_the_cooldown_wait() {
        // No prior nudge recorded: the cooldown check never blocks it.
        let now = Instant::now();
        assert!(should_nudge(
            now,
            RELAY_IDLE_STREAK_REQUIRED,
            false,
            None,
            true,
            &NudgeState {
                last_nudge: None,
                attempts: 0,
            },
        ));
    }

    #[test]
    fn tick_relay_decision_requires_two_consecutive_idle_ticks() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = Instant::now();

        // First idle observation: streak is only 1, not nudged yet.
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        // Second consecutive idle observation: streak reaches 2, nudged.
        assert!(tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_busy_tick_resets_the_idle_streak() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = Instant::now();

        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        // Observed busy: streak resets, so the next idle tick starts over.
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(false),
            false,
            None,
            true,
        ));
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    #[test]
    fn tick_relay_decision_unknown_live_entry_never_counts_as_idle() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = Instant::now();

        for _ in 0..5 {
            assert!(!tick_relay_decision(
                &mut states,
                &token,
                now,
                None, // no matching live entry: "unknown"
                false,
                None,
                true,
            ));
        }
    }

    #[test]
    fn tick_relay_decision_resets_on_drain_so_the_next_batch_starts_fresh() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let now = Instant::now();

        // Build up to a nudge.
        tick_relay_decision(&mut states, &token, now, Some(true), false, None, true);
        assert!(tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert_eq!(states.get(&token).unwrap().nudge.attempts, 1);

        // The member drains its queue: has_unseen goes false.
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            false,
        ));
        assert!(!states.contains_key(&token));

        // A fresh message arrives: the streak and attempts start over, so a
        // single idle tick does not immediately nudge again.
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
        assert_eq!(states.get(&token).unwrap().idle_streak, 1);
        assert_eq!(states.get(&token).unwrap().nudge.attempts, 0);
    }

    #[test]
    fn tick_relay_decision_stops_after_the_attempt_cap_even_past_cooldown() {
        let mut states = HashMap::new();
        let token = "worker-1".to_string();
        let mut now = Instant::now();

        // Two idle ticks to arm the streak, then repeatedly advance past the
        // cooldown and re-observe idle to rack up nudges.
        tick_relay_decision(&mut states, &token, now, Some(true), false, None, true);
        for _ in 0..RELAY_MAX_ATTEMPTS {
            now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
            assert!(tick_relay_decision(
                &mut states,
                &token,
                now,
                Some(true),
                false,
                None,
                true,
            ));
        }
        assert_eq!(
            states.get(&token).unwrap().nudge.attempts,
            RELAY_MAX_ATTEMPTS
        );

        // One more, well past cooldown: the attempt cap still blocks it.
        now += RELAY_NUDGE_COOLDOWN + Duration::from_secs(1);
        assert!(!tick_relay_decision(
            &mut states,
            &token,
            now,
            Some(true),
            false,
            None,
            true,
        ));
    }

    // A regression guard for `RelayState`'s field shape used above.
    #[test]
    fn relay_state_defaults_to_a_zero_streak_and_fresh_backoff() {
        let state = RelayState::default();
        assert_eq!(state.idle_streak, 0);
        assert_eq!(state.nudge.attempts, 0);
        assert!(state.nudge.last_nudge.is_none());
    }
}
