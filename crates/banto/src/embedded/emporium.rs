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
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Paragraph};

use banto_core::model::SessionId;
use banto_core::status::{AgeThresholds, SysinfoProbe, read_live_sessions};
use banto_core::store::Store;

use crate::app::{App, GroupJoinTarget, Modal, Mode};
use crate::opener::{self, SessionToOpen};
use crate::session;
use crate::view;

use super::pty::PortablePtyHost;
use super::render::screen_to_text;
use super::session::EmbeddedSession;

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Fixed width of the left sidebar (the session list), in columns.
const SIDEBAR_WIDTH: u16 = 36;

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
    };

    loop {
        let size = terminal.size()?;
        let (sidebar_area, pane_area) = split(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(sidebar_area.height.saturating_sub(2) as usize);

        // Pump every session so hidden ones keep advancing; resize only the
        // visible one to its pane.
        for (_, session) in ui.sessions.iter_mut() {
            session.pump();
        }
        if let Some(i) = ui.current {
            let content = pane_content(pane_area);
            ui.sessions[i].1.resize(content.height, content.width);
        }

        let pane = ui.current.map(|i| &ui.sessions[i].1);
        terminal.draw(|frame| {
            draw(
                frame,
                app,
                pane,
                ui.focus,
                ui.status.as_deref(),
                sidebar_area,
                pane_area,
            )
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && !handle_key(&mut ui, app, store, claude_home, thresholds, key)
        {
            break;
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
    match EmbeddedSession::open(&PortablePtyHost, &argv, Some(&cwd), 24, 80) {
        Ok(embedded) => {
            // No session id yet (Claude assigns it); a `new::` key never
            // collides with a real UUID row id, so it's never re-selected from
            // the sidebar (which would risk a second resume).
            ui.sessions
                .push((format!("new::{}", cwd.display()), embedded));
            ui.current = Some(ui.sessions.len() - 1);
            ui.focus = Focus::Pane;
        }
        Err(err) => ui.status = Some(format!("failed to start a new session: {err}")),
    }
    app.close_modal();
}

fn draw(
    frame: &mut ratatui::Frame,
    app: &App,
    pane: Option<&EmbeddedSession>,
    focus: Focus,
    status: Option<&str>,
    sidebar_area: Rect,
    pane_area: Rect,
) {
    let full_area = frame.area();

    // In search mode the query lives in the sidebar title (no dedicated search
    // box in this layout).
    let sidebar_title = if app.mode() == Mode::Search {
        format!("/ {}", app.query())
    } else {
        "banto".to_string()
    };
    let sidebar_block = Block::bordered()
        .title(sidebar_title)
        .border_style(border_style(focus == Focus::Sidebar));
    let sidebar_inner = sidebar_block.inner(sidebar_area);
    frame.render_widget(sidebar_block, sidebar_area);
    render_sidebar(frame, app, sidebar_inner, status);

    let pane_focused = focus == Focus::Pane;
    let pane_block = Block::bordered()
        .title("session")
        .border_style(border_style(pane_focused));
    let content = pane_block.inner(pane_area);
    frame.render_widget(pane_block, pane_area);

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

    // A modal overlays everything, reusing the classic modal rendering.
    if let Some(modal) = app.modal() {
        crate::tui::render_modal(frame, modal, full_area);
    }
}

fn render_sidebar(frame: &mut ratatui::Frame, app: &App, area: Rect, status: Option<&str>) {
    // Reuse the classic list rendering so both modes look identical.
    view::render_list(frame, app, area);
    // A transient status (e.g. "already running elsewhere") overlays the last
    // sidebar row; it's rare and short-lived, so it needn't reserve a row.
    if let Some(status) = status
        && area.height > 0
    {
        let status_area = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Span::styled(status, Style::default().fg(Color::Yellow))),
            status_area,
        );
    }
}

fn border_style(focused: bool) -> Style {
    Style::default().fg(if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    })
}

/// Split the whole area into (sidebar, pane).
fn split(area: Rect) -> (Rect, Rect) {
    let [sidebar, pane] =
        Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]).areas(area);
    (sidebar, pane)
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

    use super::{SIDEBAR_WIDTH, pane_content, split};

    #[test]
    fn split_reserves_the_sidebar_and_gives_the_rest_to_the_pane() {
        let (sidebar, pane) = split(Rect::new(0, 0, 120, 40));
        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(pane.x, SIDEBAR_WIDTH);
        assert_eq!(pane.width, 120 - SIDEBAR_WIDTH);
        assert_eq!(pane.height, 40);
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
