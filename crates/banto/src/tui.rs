//! ratatui render loop: terminal setup/teardown, event handling and drawing.
//!
//! This is a thin shell over [`crate::app::App`]; all list logic lives there.
//! The terminal is restored both on normal exit and on panic (via a panic
//! hook), and mouse capture is enabled for wheel/click support. All code here
//! is cross-platform — crossterm handles the Windows specifics.

use std::io::{self, Stdout};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use banto_core::model::{Activity, AgeBucket};
use banto_core::status::AgeThresholds;

use crate::app::{App, ClickOutcome};
use crate::session::{self, SessionRow};

/// The concrete terminal type used throughout this module.
type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Load sessions under `claude_home` and run the interactive TUI.
pub fn run(claude_home: &Path, thresholds: &AgeThresholds) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    let mut app = App::new(rows);

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app);
    // Always restore the terminal, even if the loop errored.
    let restored = restore_terminal();
    result.and(restored)
}

/// Enter raw mode + the alternate screen with mouse capture, installing a
/// panic hook that restores the terminal first.
fn setup_terminal() -> Result<Tui> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

/// Leave the alternate screen, disable mouse capture and raw mode.
fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

/// Install a panic hook that best-effort restores the terminal before the
/// default hook prints the panic message, so a panic never leaves the user in
/// raw mode on the alternate screen.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original(info);
    }));
}

/// Split an area into (search box, list, status bar).
fn layout_areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// Draw, wait for one event, dispatch it, repeat until quit.
fn event_loop(terminal: &mut Tui, app: &mut App) -> Result<()> {
    loop {
        // Compute the layout up front so the viewport height and mouse
        // hit-testing agree with what we are about to render.
        let size = terminal.size()?;
        let [_, list_area, _] = layout_areas(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(list_area.height as usize);

        terminal.draw(|frame| render(frame, app))?;

        match event::read()? {
            Event::Key(key) => {
                // On Windows crossterm also reports key releases; ignore them.
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                handle_key(app, key.code, key.modifiers);
            }
            Event::Mouse(mouse) => handle_mouse(app, mouse, list_area),
            _ => {}
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}

/// Translate a key press into an [`App`] action.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        // Ctrl+C always quits; other Ctrl combos are ignored for now.
        KeyCode::Char('c') if ctrl => app.request_quit(),
        _ if ctrl => {}
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        KeyCode::Enter => app.activate_selected(),
        KeyCode::Backspace => app.backspace(),
        // Esc clears a non-empty query, otherwise quits.
        KeyCode::Esc => {
            if app.query().is_empty() {
                app.request_quit();
            } else {
                app.clear_query();
            }
        }
        // 'q' quits only when not typing a query; otherwise it is input.
        KeyCode::Char('q') if app.query().is_empty() => app.request_quit(),
        KeyCode::Char(c) => app.push_char(c),
        _ => {}
    }
}

/// Translate a mouse event into an [`App`] action.
fn handle_mouse(app: &mut App, mouse: event::MouseEvent, list_area: Rect) {
    match mouse.kind {
        MouseEventKind::ScrollDown => app.scroll(1),
        MouseEventKind::ScrollUp => app.scroll(-1),
        MouseEventKind::Down(MouseButton::Left) => {
            let position = Position::new(mouse.column, mouse.row);
            if list_area.contains(position) {
                let viewport_row = (mouse.row - list_area.y) as usize;
                if app.click(viewport_row, Instant::now()) == Some(ClickOutcome::Activated) {
                    app.activate_selected();
                }
            }
        }
        _ => {}
    }
}

/// Render the whole UI for one frame.
fn render(frame: &mut Frame, app: &App) {
    let [search_area, list_area, status_area] = layout_areas(frame.area());
    render_search(frame, app, search_area);
    render_list(frame, app, list_area);
    render_status(frame, app, status_area);
}

/// Render the top search box and place the text cursor after the query.
fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Search ");
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(app.query()).block(block), area);

    if inner.width > 0 {
        let query_cols = app.query().chars().count() as u16;
        let cursor_x = (inner.x + query_cols).min(inner.x + inner.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, inner.y));
    }
}

/// Render the session list (or a placeholder when nothing matches).
fn render_list(frame: &mut Frame, app: &App, area: Rect) {
    if app.filtered_len() == 0 {
        let message = if app.total_len() == 0 {
            "No sessions found."
        } else {
            "No matching sessions."
        };
        let placeholder = Paragraph::new(message).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(placeholder, area);
        return;
    }

    let items: Vec<ListItem> = app.visible().into_iter().map(list_item).collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default();
    state.select(app.selected_in_viewport());
    frame.render_stateful_widget(list, area, &mut state);
}

/// Build one list row: colored activity dot, title (or id), dimmed cwd.
fn list_item(row: &SessionRow) -> ListItem<'static> {
    let dot = Span::styled(
        "\u{25cf} ",
        Style::default().fg(activity_color(row.activity)),
    );
    let title = Span::raw(row.display_title().to_string());
    let cwd = row.cwd_display();
    let line = if cwd.is_empty() {
        Line::from(vec![dot, title])
    } else {
        Line::from(vec![
            dot,
            title,
            Span::raw("  "),
            Span::styled(cwd, Style::default().fg(Color::DarkGray)),
        ])
    };
    ListItem::new(line)
}

/// Render the bottom status bar: key hints (or a transient message) on the
/// left, match count right-aligned. Rendered as two separate widgets (rather
/// than one line) so the count stays visible even when the hints are too long
/// for a narrow terminal and get truncated.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    const HINTS: &str =
        "\u{2191}\u{2193} move  PgUp/PgDn page  Enter open  type to search  Esc clear/quit  q quit";
    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    let counts_width = counts.chars().count() as u16;

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);

    let (left, color) = match app.status() {
        Some(message) => (message.to_string(), Color::Yellow),
        None => (HINTS.to_string(), Color::Gray),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(left, Style::default().fg(color))),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            counts,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        right_area,
    );
}

/// Map an [`Activity`] to its list-dot color.
fn activity_color(activity: Activity) -> Color {
    match activity {
        Activity::Busy => Color::Green,
        Activity::Alive => Color::Cyan,
        Activity::Idle(AgeBucket::Today) => Color::Yellow,
        Activity::Idle(AgeBucket::ThisWeek) => Color::Gray,
        Activity::Idle(AgeBucket::Older) => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use banto_core::model::{Activity, AgeBucket};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::path::PathBuf;

    fn row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            id: id.into(),
            title: Some(title.into()),
            cwd: Some(PathBuf::from(cwd)),
            activity,
        }
    }

    /// Flatten a rendered buffer into text for content assertions.
    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }

    fn draw(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 15)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn renders_titles_cwd_and_counts() {
        let rows = vec![
            row("id-alpha", "Alpha task", "/work/alpha", Activity::Busy),
            row(
                "id-beta",
                "Beta task",
                "/work/beta",
                Activity::Idle(AgeBucket::Today),
            ),
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(11);

        let text = draw(&app);
        assert!(text.contains("Search"), "search box missing:\n{text}");
        assert!(text.contains("Alpha task"), "titles missing:\n{text}");
        assert!(text.contains("Beta task"));
        assert!(text.contains("/work/alpha"), "cwd missing:\n{text}");
        assert!(text.contains("2/2"), "count missing:\n{text}");
    }

    #[test]
    fn filtering_narrows_the_rendered_rows() {
        let rows = vec![
            row("id-alpha", "Alpha task", "/work/alpha", Activity::Alive),
            row("id-beta", "Beta task", "/work/beta", Activity::Alive),
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(11);
        for c in "beta".chars() {
            app.push_char(c);
        }

        let text = draw(&app);
        assert!(text.contains("Beta task"), "kept row missing:\n{text}");
        assert!(
            !text.contains("Alpha task"),
            "filtered row present:\n{text}"
        );
        assert!(text.contains("1/2"), "count missing:\n{text}");
    }

    #[test]
    fn empty_list_shows_placeholder() {
        let app = App::new(Vec::new());
        let text = draw(&app);
        assert!(text.contains("No sessions found."), "placeholder:\n{text}");
        assert!(text.contains("0/0"));
    }
}
