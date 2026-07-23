//! The "emporium" (大店 / `--emporium` / `--oodana`) mode: banto as a
//! persistent left sidebar (the session list) plus a right pane hosting the
//! selected session embedded. Sessions stay alive across switches (keep-alive);
//! Slice 2 (brigades — multiple visible panes) builds on that.
//!
//! A separate top-level mode chosen at launch. The classic list TUI
//! (`crate::tui`) owns the shared pieces this reuses — `App` (list state), the
//! `view` renderers, the store-load helpers, and `render_modal`.

use std::cell::RefCell;
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

use banto_core::model::SessionId;
use banto_core::provider::claude_code::ClaudeCodeProvider;
use banto_core::status::{AgeThresholds, SysinfoProbe, read_live_sessions};
use banto_core::store::{BrigadeId, BrigadeRole, Store};

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
    /// Kept-alive embedded sessions, keyed by session id (or a `new::<cwd>`
    /// synthetic key for freshly-launched ones that have no id yet).
    ///
    /// Append-only for the lifetime of a run: sessions are never removed
    /// (removing one from a brigade only drops it from the [`Stage`], the
    /// session stays alive here). So every index held by a `Stage` stays valid.
    sessions: Vec<(String, EmbeddedSession)>,
    /// What the pane region currently shows.
    stage: Stage,
    focus: Focus,
    status: Option<String>,
    /// Freshly-launched sessions awaiting id discovery (see [`discover_new_ids`]).
    pending_new: Vec<PendingNew>,
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
/// `new::<cwd>` collection key, launch cwd, and launch time.
struct PendingNew {
    key: String,
    cwd: PathBuf,
    since: SystemTime,
}

/// Which confirm branch an open modal takes — resolved before mutating `App`
/// so its `modal()` borrow doesn't overlap the mutation.
enum ModalKind {
    Archive,
    Group,
    New,
}

/// Run the emporium mode until the user quits (`q`/Esc from the sidebar).
pub fn run(claude_home: &Path, thresholds: &AgeThresholds, store: &RefCell<Store>) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    // Same store-backed state the classic list builds, so grouping / pins /
    // archived-hiding show identically in the sidebar.
    let (rows, pinned, groups, session_groups) = {
        let store = store.borrow();
        let rows = crate::tui::exclude_archived(rows, &store);
        let pinned = crate::tui::load_pinned(&store);
        let groups = crate::tui::load_groups(&store);
        let session_groups = crate::tui::load_session_groups(&store, &groups);
        (rows, pinned, groups, session_groups)
    };
    let mut app = App::new(rows)
        .with_pinned(pinned)
        .with_groups(groups, session_groups);

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, claude_home, thresholds, store);
    let restored = restore_terminal();
    result.and(restored)
}

fn event_loop(
    terminal: &mut Tui,
    app: &mut App,
    claude_home: &Path,
    thresholds: &AgeThresholds,
    store: &RefCell<Store>,
) -> Result<()> {
    let mut ui = Emporium {
        sessions: Vec::new(),
        stage: Stage::Empty,
        focus: Focus::Sidebar,
        status: None,
        pending_new: Vec::new(),
    };
    let mut watch = LiveWatch::new(claude_home);
    let provider = ClaudeCodeProvider::new(claude_home.to_path_buf());

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
                    if !handle_key(&mut ui, app, store, claude_home, thresholds, key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => handle_mouse(&mut ui, app, store, mouse, areas, claude_home),
                _ => {}
            }
        }

        // Live updates: reload the list once the watched dirs settle.
        if watch.poll_ready(SystemTime::now()) {
            reload(app, claude_home, thresholds, store);
        }
        // Discover the ids Claude assigns to freshly-launched sessions, so they
        // can be re-selected from the sidebar without a second resume.
        if !ui.pending_new.is_empty() {
            discover_new_ids(&mut ui, &provider);
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
                KeyCode::Enter => open_or_switch(ui, app, store, claude_home),
                KeyCode::Char('B') => start_brigade(ui, app, store, claude_home),
                KeyCode::Char('b') => toggle_worker(ui, app, store, claude_home),
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
                open_or_switch(ui, app, store, claude_home);
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

/// Enter / double-click on the sidebar: if the selected session belongs to a
/// brigade, stage that whole cell; otherwise switch to the session if it's
/// already open (keeping every session alive), else open it solo in a new
/// kept-alive pane. Switching to an already-open session never re-resumes it —
/// that would fork its history (a double resume) even though banto itself is
/// what holds it.
fn open_or_switch(ui: &mut Emporium, app: &App, store: &RefCell<Store>, claude_home: &Path) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let id = row.id.clone();

    // Belongs to a brigade? Open the whole cell instead of the lone session.
    let membership = store
        .borrow()
        .brigade_of_session(&SessionId(id.clone()))
        .ok()
        .flatten();
    if let Some((brigade_id, _)) = membership {
        stage_brigade(ui, app, store, claude_home, brigade_id);
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
/// session is wired to its MCP channel. Returns `None` when it can't be opened
/// (already running elsewhere, or an error — a status is set in both cases).
fn ensure_session_open(
    ui: &mut Emporium,
    row: &SessionRow,
    claude_home: &Path,
    brigade: Option<(BrigadeId, BrigadeRole)>,
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

/// `B`: form a new brigade with the selected session as its Director and stage
/// it. The brigade is persisted immediately (schema v4), so it can be reopened
/// later from the sidebar (Enter on any member).
fn start_brigade(ui: &mut Emporium, app: &App, store: &RefCell<Store>, claude_home: &Path) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let name = row.display_title().to_string();
    let session_id = row.id.clone();

    // Create + persist the brigade first, so the Director launches already wired
    // to its MCP channel (its identity is passed into the launch below).
    let brigade_id = {
        let mut store = store.borrow_mut();
        match store.create_brigade(&name).and_then(|bid| {
            store.set_brigade_member(bid, &SessionId(session_id), BrigadeRole::Director)?;
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

    let Some(idx) = ensure_session_open(
        ui,
        row,
        claude_home,
        Some((brigade_id, BrigadeRole::Director)),
    ) else {
        // The brigade is persisted but its Director couldn't open (status set).
        return;
    };
    ui.stage = Stage::Brigade {
        id: brigade_id,
        panes: vec![idx],
        focused: 0,
    };
    ui.focus = Focus::Pane;
    ui.status = Some(format!(
        "brigade formed — director: {name}. F2 → pick a session → b to add a worker."
    ));
}

/// `b`: add the selected session to the staged brigade as a Worker, or remove
/// it if it's already a Worker there (a no-op on the Director). Requires a
/// brigade to be staged.
fn toggle_worker(ui: &mut Emporium, app: &App, store: &RefCell<Store>, claude_home: &Path) {
    let brigade_id = match &ui.stage {
        Stage::Brigade { id, .. } => *id,
        _ => {
            ui.status = Some("no brigade staged — press B to start one".to_string());
            return;
        }
    };
    let Some(row) = app.selected_row() else {
        return;
    };
    let session_id = row.id.clone();

    // Already a member of this brigade? Toggle it out (unless it's the
    // Director, which `b` never removes).
    let membership = store
        .borrow()
        .brigade_of_session(&SessionId(session_id.clone()))
        .ok()
        .flatten();
    if let Some((member_brigade, role)) = membership
        && member_brigade == brigade_id
    {
        if role == BrigadeRole::Director {
            ui.status = Some("that session is the Director".to_string());
            return;
        }
        let _ = store
            .borrow()
            .remove_brigade_member(brigade_id, &SessionId(session_id.clone()));
        let open_idx = ui.sessions.iter().position(|(sid, _)| *sid == session_id);
        if let Stage::Brigade { panes, focused, .. } = &mut ui.stage
            && let Some(removed) = open_idx
        {
            panes.retain(|&i| i != removed);
            if *focused >= panes.len() {
                *focused = panes.len().saturating_sub(1);
            }
        }
        ui.status = Some("worker removed".to_string());
        return;
    }

    // Otherwise add it as a Worker: persist first, then launch it wired to the
    // MCP channel, and tile it in.
    if let Err(err) = store.borrow_mut().set_brigade_member(
        brigade_id,
        &SessionId(session_id.clone()),
        BrigadeRole::Worker,
    ) {
        ui.status = Some(format!("failed to add worker: {err}"));
        return;
    }
    let Some(idx) = ensure_session_open(
        ui,
        row,
        claude_home,
        Some((brigade_id, BrigadeRole::Worker)),
    ) else {
        return;
    };
    if let Stage::Brigade { panes, .. } = &mut ui.stage
        && !panes.contains(&idx)
    {
        panes.push(idx);
    }
    ui.status = Some("worker added".to_string());
}

/// Stage brigade `brigade_id`: ensure each member is open (embedded) and show
/// them tiled with the Director focused. Members that aren't in the loaded
/// session list (e.g. gone from disk) are skipped.
fn stage_brigade(
    ui: &mut Emporium,
    app: &App,
    store: &RefCell<Store>,
    claude_home: &Path,
    brigade_id: BrigadeId,
) {
    let members = match store.borrow().brigade_members(brigade_id) {
        Ok(members) => members,
        Err(err) => {
            ui.status = Some(format!("failed to load brigade: {err}"));
            return;
        }
    };
    let mut panes = Vec::new();
    let mut missing = 0;
    for member in &members {
        match app.row_for_id(&member.session_id.0) {
            Some(row) => {
                if let Some(idx) =
                    ensure_session_open(ui, row, claude_home, Some((brigade_id, member.role)))
                    && !panes.contains(&idx)
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

/// Spawn `session` in a new embedded pane, enforcing the no-double-resume guard
/// (reusing the classic in-place decision). Returns `None` (and sets a status)
/// when it's already running elsewhere. When `brigade` is set, the launch is
/// wired to banto's own MCP server so the session can message its peer.
fn open_embedded(
    session: &SessionToOpen,
    claude_home: &Path,
    brigade: Option<(BrigadeId, BrigadeRole)>,
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
    if let Some((brigade_id, role)) = brigade {
        match write_mcp_config(&session.id, brigade_id, role) {
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
/// own MCP server (`banto _mcp`) with this member's brigade identity, and return
/// its path. Lives under banto's own data dir, never under ~/.claude.
fn write_mcp_config(session_id: &str, brigade_id: BrigadeId, role: BrigadeRole) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let role_token = match role {
        BrigadeRole::Director => "director",
        BrigadeRole::Worker => "worker",
    };
    let config = serde_json::json!({
        "mcpServers": {
            "banto": {
                "command": exe.to_string_lossy(),
                "args": [
                    "_mcp",
                    "--session", session_id,
                    "--brigade", brigade_id.to_string(),
                    "--role", role_token,
                ],
            }
        }
    });
    let dir = dirs::data_local_dir()
        .map(|base| base.join("banto").join("mcp"))
        .ok_or_else(|| anyhow::anyhow!("could not determine banto's data directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", sanitize_filename(session_id)));
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    Ok(path)
}

/// Keep a session id safe as a filename stem (ids are UUIDs, but be defensive).
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
/// immediately) — the emporium counterpart of the classic `reload`.
fn reload(app: &mut App, claude_home: &Path, thresholds: &AgeThresholds, store: &RefCell<Store>) {
    if let Ok(rows) = session::load_rows(claude_home, thresholds) {
        let rows = crate::tui::exclude_archived(rows, &store.borrow());
        app.replace_rows(rows);
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
        None => None,
    };
    match kind {
        Some(ModalKind::Archive) => confirm_archive(ui, app, store, claude_home, thresholds),
        Some(ModalKind::Group) => confirm_group_join(ui, app, store),
        Some(ModalKind::New) => confirm_new_embedded(ui, app),
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
            ui.pending_new.push(PendingNew { key, cwd, since });
        }
        Err(err) => ui.status = Some(format!("failed to start a new session: {err}")),
    }
    app.close_modal();
}

/// Poll for the ids Claude assigns to freshly-launched sessions and re-key their
/// collection entries from the synthetic `new::<cwd>` key to the real id, so
/// they behave like any other open session (re-selectable, no second resume).
fn discover_new_ids(ui: &mut Emporium, provider: &ClaudeCodeProvider) {
    let mut discovered: Vec<(String, String)> = Vec::new();
    for pending in &ui.pending_new {
        if let Some(id) = provider.find_new_session(&pending.cwd, pending.since) {
            discovered.push((pending.key.clone(), id.0));
        }
    }
    if discovered.is_empty() {
        return;
    }
    for (key, id) in &discovered {
        if let Some(entry) = ui.sessions.iter_mut().find(|(k, _)| k == key) {
            entry.0 = id.clone();
        }
    }
    ui.pending_new
        .retain(|pending| !discovered.iter().any(|(key, _)| key == &pending.key));
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
    const NORMAL_HINTS: &str = "j/k move · Enter open · F2 focus · B brigade · b +worker · \
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
    use ratatui::layout::Rect;

    use super::{
        MIN_HEIGHT_FOR_SUMMARY, SIDEBAR_WIDTH, SUMMARY_HEIGHT, Stage, layout, pane_content,
        stage_tiles,
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
}
