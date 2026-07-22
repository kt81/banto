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
use banto_core::store::Store;

use crate::app::{App, ClickOutcome, GroupJoinTarget, Modal, Mode};
use crate::opener::{self, SessionToOpen};
use crate::session;
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
/// shared list state): the kept-alive session panes, which one is shown, the
/// focus, and a transient status line.
struct Emporium {
    /// Kept-alive embedded sessions, keyed by session id (or a `new::<cwd>`
    /// synthetic key for freshly-launched ones that have no id yet).
    sessions: Vec<(String, EmbeddedSession)>,
    /// Index into `sessions` of the one shown in the pane.
    current: Option<usize>,
    focus: Focus,
    status: Option<String>,
    /// Freshly-launched sessions awaiting id discovery (see [`discover_new_ids`]).
    pending_new: Vec<PendingNew>,
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
        current: None,
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

        // Pump every session so hidden ones keep advancing; resize only the
        // visible one to its pane.
        for (_, session) in ui.sessions.iter_mut() {
            session.pump();
        }
        if let Some(i) = ui.current {
            let content = pane_content(areas.pane);
            ui.sessions[i].1.resize(content.height, content.width);
        }

        let pane = ui.current.map(|i| &ui.sessions[i].1);
        terminal.draw(|frame| draw(frame, app, pane, ui.focus, ui.status.as_deref(), areas))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if !handle_key(&mut ui, app, store, claude_home, thresholds, key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => handle_mouse(&mut ui, app, mouse, areas, claude_home),
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
            Focus::Sidebar if ui.current.is_some() => Focus::Pane,
            _ => Focus::Sidebar,
        };
        return true;
    }

    match ui.focus {
        Focus::Pane => {
            if let Some(i) = ui.current {
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
                KeyCode::Enter => open_or_switch(ui, app, claude_home),
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
    mouse: MouseEvent,
    areas: Areas,
    claude_home: &Path,
) {
    if app.modal().is_some() {
        return;
    }
    let pos = Position::new(mouse.column, mouse.row);

    // Over the pane: focus it on a left click, and forward the mouse to the
    // child once focused.
    if areas.pane.contains(pos) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            ui.focus = Focus::Pane;
        }
        if ui.focus == Focus::Pane
            && let Some(i) = ui.current
            && let Some(bytes) = mouse_to_sgr(&mouse, pane_content(areas.pane))
        {
            ui.sessions[i].1.send_bytes(&bytes);
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
                open_or_switch(ui, app, claude_home);
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

/// Enter on the sidebar: switch to the selected session if it's already open
/// (keeping every session alive), else open it in a new kept-alive pane.
/// Switching to an already-open session never re-resumes it — that would fork
/// its history (a double resume) even though banto itself is what holds it.
fn open_or_switch(ui: &mut Emporium, app: &App, claude_home: &Path) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let id = row.id.clone();
    if let Some(i) = ui.sessions.iter().position(|(sid, _)| *sid == id) {
        ui.current = Some(i);
        ui.focus = Focus::Pane;
        return;
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
    match open_embedded(&target, claude_home, &mut ui.status) {
        Ok(Some(embedded)) => {
            ui.sessions.push((id, embedded));
            ui.current = Some(ui.sessions.len() - 1);
            ui.focus = Focus::Pane;
        }
        Ok(None) => {}
        Err(err) => ui.status = Some(format!("failed to open: {err}")),
    }
}

/// Spawn `session` in a new embedded pane, enforcing the no-double-resume guard
/// (reusing the classic in-place decision). Returns `None` (and sets a status)
/// when it's already running elsewhere.
fn open_embedded(
    session: &SessionToOpen,
    claude_home: &Path,
    status: &mut Option<String>,
) -> Result<Option<EmbeddedSession>> {
    let live = read_live_sessions(&claude_home.join("sessions"));
    let Some(launch) = opener::decide_inplace_resume(session, &SysinfoProbe, &live) else {
        *status = Some("already running elsewhere".to_string());
        return Ok(None);
    };
    // Size is corrected on the next loop tick from the real pane geometry.
    let embedded =
        EmbeddedSession::open(&PortablePtyHost, &launch.argv, Some(&launch.cwd), 24, 80)?;
    Ok(Some(embedded))
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
            ui.current = Some(ui.sessions.len() - 1);
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

fn draw(
    frame: &mut ratatui::Frame,
    app: &App,
    pane: Option<&EmbeddedSession>,
    focus: Focus,
    status: Option<&str>,
    areas: Areas,
) {
    let full_area = frame.area();

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

    // Right pane hosting the session.
    let pane_focused = focus == Focus::Pane;
    let pane_block = Block::bordered()
        .title("session")
        .border_style(border_style(pane_focused));
    let content = pane_block.inner(areas.pane);
    frame.render_widget(pane_block, areas.pane);
    match pane {
        Some(session) => {
            frame.render_widget(Paragraph::new(screen_to_text(session.screen())), content);
            if pane_focused && !session.screen().hide_cursor() {
                let (cursor_row, cursor_col) = session.screen().cursor_position();
                let (x, y) = (content.x + cursor_col, content.y + cursor_row);
                if x < content.x + content.width && y < content.y + content.height {
                    frame.set_cursor_position(Position::new(x, y));
                }
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new("Select a session and press Enter.\nF2 toggles focus · q quits."),
                content,
            );
        }
    }

    render_status_bar(frame, app, status, areas.status);

    // A modal overlays everything, reusing the classic modal rendering.
    if let Some(modal) = app.modal() {
        crate::tui::render_modal(frame, modal, full_area);
    }
}

/// Bottom status bar: emporium key hints (or a transient status) on the left,
/// the match count on the right — the emporium counterpart of the classic
/// `render_status` (its own hints, and its own status line).
fn render_status_bar(frame: &mut ratatui::Frame, app: &App, status: Option<&str>, area: Rect) {
    const NORMAL_HINTS: &str = "j/k move · Enter open · F2 focus · / search · n new · \
                                d archive · g group · Tab view · p pin · a agents · q quit";
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

    use super::{MIN_HEIGHT_FOR_SUMMARY, SIDEBAR_WIDTH, SUMMARY_HEIGHT, layout, pane_content};

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
}
