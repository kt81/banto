//! The "emporium" (大店 / `--emporium` / `--oodana`) mode: banto as a
//! persistent left sidebar (the session list) plus a right pane hosting the
//! selected session embedded. Slice 1b — a single right pane; brigades
//! (multiple panes) come in Slice 2.
//!
//! This is a separate top-level mode chosen at launch. The classic list TUI
//! (`crate::tui`) is left untouched; only `main` decides which mode to run.

use std::io::{self, Stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
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

use banto_core::status::{AgeThresholds, SysinfoProbe, read_live_sessions};

use crate::app::App;
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

/// Run the emporium mode until the user quits (`q`/Esc from the sidebar).
pub fn run(claude_home: &Path, thresholds: &AgeThresholds) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    let mut app = App::new(rows);

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, claude_home);
    let restored = restore_terminal();
    result.and(restored)
}

fn event_loop(terminal: &mut Tui, app: &mut App, claude_home: &Path) -> Result<()> {
    let mut focus = Focus::Sidebar;
    let mut pane: Option<EmbeddedSession> = None;
    let mut status: Option<String> = None;

    loop {
        let size = terminal.size()?;
        let (sidebar_area, pane_area) = split(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(sidebar_area.height.saturating_sub(2) as usize);

        // Keep the embedded child current and sized to its pane.
        if let Some(session) = pane.as_mut() {
            session.pump();
            let content = pane_content(pane_area);
            session.resize(content.height, content.width);
        }

        terminal.draw(|frame| {
            draw(
                frame,
                app,
                pane.as_ref(),
                focus,
                status.as_deref(),
                sidebar_area,
                pane_area,
            )
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            // F2 always toggles focus and is never forwarded to the child.
            if key.code == KeyCode::F(2) {
                focus = match focus {
                    Focus::Sidebar if pane.is_some() => Focus::Pane,
                    _ => Focus::Sidebar,
                };
                continue;
            }
            match focus {
                Focus::Sidebar => {
                    status = None;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                        KeyCode::PageUp => app.page_up(),
                        KeyCode::PageDown => app.page_down(),
                        KeyCode::Home => app.select_first(),
                        KeyCode::End => app.select_last(),
                        KeyCode::Enter => match open_selected(app, claude_home, &mut status) {
                            Ok(Some(session)) => {
                                pane = Some(session);
                                focus = Focus::Pane;
                            }
                            Ok(None) => {}
                            Err(err) => status = Some(format!("failed to open: {err}")),
                        },
                        _ => {}
                    }
                }
                Focus::Pane => {
                    if let Some(session) = pane.as_mut() {
                        session.send_key(&key);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Open the selected session in an embedded pane, enforcing the no-double-resume
/// guard (reusing the same decision the classic in-place path uses). Returns the
/// opened session, or `None` when nothing is selected or the session is already
/// running elsewhere (in which case a status message is set).
fn open_selected(
    app: &App,
    claude_home: &Path,
    status: &mut Option<String>,
) -> Result<Option<EmbeddedSession>> {
    let Some(row) = app.selected_row() else {
        return Ok(None);
    };
    let session = SessionToOpen {
        id: row.id.clone(),
        title: row.display_title().to_string(),
        cwd: row
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default(),
    };
    let live = read_live_sessions(&claude_home.join("sessions"));
    let Some(launch) = opener::decide_inplace_resume(&session, &SysinfoProbe, &live) else {
        *status = Some("already running elsewhere".to_string());
        return Ok(None);
    };
    // Size is corrected on the next loop tick from the real pane geometry.
    let embedded =
        EmbeddedSession::open(&PortablePtyHost, &launch.argv, Some(&launch.cwd), 24, 80)?;
    Ok(Some(embedded))
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
    let sidebar_block = Block::bordered()
        .title("banto")
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
