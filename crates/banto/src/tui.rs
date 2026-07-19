//! ratatui render loop: terminal setup/teardown, event handling and drawing.
//!
//! This is a thin shell over [`crate::app::App`]; all list logic lives there.
//! The terminal is restored both on normal exit and on panic (via a panic
//! hook), and mouse capture is enabled for wheel/click support. All code here
//! is cross-platform — crossterm handles the Windows specifics.

use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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

use banto_core::config::OpenerMode;
use banto_core::model::{Activity, AgeBucket, SessionId};
use banto_core::opener::SystemCommandRunner;
use banto_core::status::{AgeThresholds, SysinfoProbe};
use banto_core::store::Store;
use banto_core::watch::{ChangeSource, Debouncer, NotifyChangeSource};

use crate::app::{App, ClickOutcome, Mode, VisibleRow};
use crate::opener::{self, OpenOutcome, SessionToOpen};
use crate::session;
use crate::sgr::{self, SgrParse};

/// The concrete terminal type used throughout this module.
type Tui = Terminal<CrosstermBackend<Stdout>>;

/// How long each event-loop tick waits for input before checking for
/// filesystem changes; keeps input latency imperceptible while still polling
/// for live updates roughly this often.
const TICK_INTERVAL: Duration = Duration::from_millis(150);

/// How long `projects/`/`sessions/` must stay quiet before a burst of
/// filesystem changes triggers a reload (see `banto_core::watch::Debouncer`).
const DEBOUNCE_QUIET: Duration = Duration::from_millis(250);

/// How long to wait for the next byte of a possible escape sequence after a
/// lone `Esc`, before concluding it really is just Esc. A real leaked SGR
/// mouse sequence arrives as one burst, taking far less than this; a human
/// pressing Esc and then another key takes far longer — so this never adds
/// perceptible latency to an ordinary Esc press.
const ESCAPE_BURST_TIMEOUT: Duration = Duration::from_millis(25);

/// Everything the render loop needs beyond [`App`] itself: dependencies for
/// opening/focusing sessions and reloading rows from disk.
struct Context<'a> {
    claude_home: &'a Path,
    thresholds: &'a AgeThresholds,
    store: &'a Store,
    opener_mode: OpenerMode,
}

/// Watches `claude_home` for changes and debounces them into a "reload now"
/// signal. Construction failures (e.g. an exotic filesystem `notify` can't
/// watch) degrade to "no live updates" rather than blocking the TUI from
/// starting at all.
struct LiveWatch {
    source: Option<NotifyChangeSource>,
    debouncer: Debouncer,
}

impl LiveWatch {
    fn new(claude_home: &Path) -> Self {
        Self {
            source: NotifyChangeSource::new(claude_home).ok(),
            debouncer: Debouncer::new(DEBOUNCE_QUIET),
        }
    }

    /// Drain any pending filesystem changes and report whether their quiet
    /// period has elapsed as of `now`, i.e. whether a reload is due.
    fn poll_ready(&mut self, now: SystemTime) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        for change in source.drain() {
            self.debouncer.record(change.root, change.at);
        }
        !self.debouncer.poll(now).is_empty()
    }
}

/// Load sessions under `claude_home` and run the interactive TUI.
pub fn run(
    claude_home: &Path,
    thresholds: &AgeThresholds,
    opener_mode: OpenerMode,
    store: &Store,
) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    let pinned = load_pinned(store);
    let mut app = App::new(rows).with_pinned(pinned);
    let ctx = Context {
        claude_home,
        thresholds,
        store,
        opener_mode,
    };

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, &ctx);
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

/// Load the currently pinned session ids from the store. Tolerant: a read
/// failure just means no sessions start out pinned, rather than blocking the
/// TUI from starting.
fn load_pinned(store: &Store) -> HashSet<String> {
    store
        .pinned_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
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

/// Draw, wait up to [`TICK_INTERVAL`] for one event, dispatch it, check for
/// filesystem changes, repeat until quit. Polling (rather than blocking on
/// `event::read()`) is what lets live updates land without waiting for the
/// next keypress.
fn event_loop(terminal: &mut Tui, app: &mut App, ctx: &Context) -> Result<()> {
    let mut watch = LiveWatch::new(ctx.claude_home);

    loop {
        // Compute the layout up front so the viewport height and mouse
        // hit-testing agree with what we are about to render.
        let size = terminal.size()?;
        let [_, list_area, _] = layout_areas(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(list_area.height as usize);

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(TICK_INTERVAL)? {
            match event::read()? {
                Event::Key(key) => {
                    // On Windows crossterm also reports key releases; ignore them.
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // Esc needs special handling: it may be a lone Escape
                    // press, or the start of a leaked SGR mouse sequence.
                    if key.code == KeyCode::Esc {
                        resolve_escape(app, ctx, list_area)?;
                    } else {
                        handle_key(app, key.code, key.modifiers, ctx);
                    }
                }
                Event::Mouse(mouse) => handle_mouse(app, mouse, list_area, ctx),
                _ => {}
            }
        }

        if watch.poll_ready(SystemTime::now()) {
            reload(app, ctx);
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}

/// Translate a key press into an [`App`] action. Navigation, paging and
/// Enter behave the same in both modes; everything else is mode-specific
/// (see [`handle_normal_key`] / [`handle_search_key`]) because letter keys
/// mean different things: commands in Normal mode, query text in Search mode.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, ctx: &Context) {
    if mods.contains(KeyModifiers::CONTROL) {
        // Ctrl+C always quits; other Ctrl combos are ignored for now.
        if code == KeyCode::Char('c') {
            app.request_quit();
        }
        return;
    }
    match code {
        KeyCode::Enter => activate(app, ctx),
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        _ => match app.mode() {
            Mode::Normal => handle_normal_key(app, code, ctx),
            Mode::Search => handle_search_key(app, code),
        },
    }
}

/// Normal-mode keys: letters are commands, not query input.
fn handle_normal_key(app: &mut App, code: KeyCode, ctx: &Context) {
    match code {
        KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('k') => app.select_prev(),
        KeyCode::Char('/') => app.enter_search(),
        KeyCode::Char('p') => toggle_pin(app, ctx),
        KeyCode::Char('a') => toggle_agent_filter(app),
        KeyCode::Char('q') | KeyCode::Esc => app.request_quit(),
        _ => {}
    }
}

/// Search-mode keys: characters type into the query; Esc cancels the search
/// (clearing the query) back to Normal mode.
fn handle_search_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Backspace => app.backspace(),
        KeyCode::Esc => app.exit_search(),
        KeyCode::Char(c) => app.push_char(c),
        _ => {}
    }
}

/// Resolve a `KeyCode::Esc` key event: either a genuine lone Escape press,
/// or the start of an SGR mouse escape sequence (`ESC [ < Cb ; Cx ; Cy
/// (M|m)`) that leaked through as individual character keys instead of a
/// proper `Event::Mouse` — observed under some nested terminal/multiplexer
/// setups. Buffers characters and re-checks [`sgr::parse_prefix`] after
/// each one:
/// - a complete match is swallowed entirely (never reaches [`App`]);
/// - a definite mismatch replays the buffered characters as ordinary key
///   presses (the leading Esc dispatched normally, the rest as if typed);
/// - if nothing follows within [`ESCAPE_BURST_TIMEOUT`], it's a lone Esc.
fn resolve_escape(app: &mut App, ctx: &Context, list_area: Rect) -> Result<()> {
    let mut pending = vec!['\u{1b}'];
    loop {
        match sgr::parse_prefix(&pending) {
            SgrParse::Complete(_) => return Ok(()), // a real mouse click/scroll; swallow it
            SgrParse::NotSgr => {
                replay(app, ctx, &pending);
                return Ok(());
            }
            SgrParse::Incomplete => {
                if !event::poll(ESCAPE_BURST_TIMEOUT)? {
                    replay(app, ctx, &pending);
                    return Ok(());
                }
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                        KeyCode::Char(c) => pending.push(c),
                        KeyCode::Esc => pending.push('\u{1b}'),
                        other => {
                            replay(app, ctx, &pending);
                            handle_key(app, other, key.modifiers, ctx);
                            return Ok(());
                        }
                    },
                    Event::Mouse(mouse) => {
                        replay(app, ctx, &pending);
                        handle_mouse(app, mouse, list_area, ctx);
                        return Ok(());
                    }
                    _ => {} // resize/focus/paste mid-burst: keep waiting
                }
            }
        }
    }
}

/// Replay buffered characters as if they had arrived as ordinary key
/// presses. The first is always the `Esc` that started buffering.
fn replay(app: &mut App, ctx: &Context, buffered: &[char]) {
    let mut chars = buffered.iter().copied();
    if chars.next().is_some() {
        handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
    }
    for c in chars {
        handle_key(app, KeyCode::Char(c), KeyModifiers::NONE, ctx);
    }
}

/// Translate a mouse event into an [`App`] action.
fn handle_mouse(app: &mut App, mouse: event::MouseEvent, list_area: Rect, ctx: &Context) {
    match mouse.kind {
        MouseEventKind::ScrollDown => app.scroll(1),
        MouseEventKind::ScrollUp => app.scroll(-1),
        MouseEventKind::Down(MouseButton::Left) => {
            let position = Position::new(mouse.column, mouse.row);
            if list_area.contains(position) {
                let viewport_row = (mouse.row - list_area.y) as usize;
                if app.click(viewport_row, Instant::now()) == Some(ClickOutcome::Activated) {
                    activate(app, ctx);
                }
            }
        }
        _ => {}
    }
}

/// Open or focus the selected session (Enter / double-click), posting the
/// outcome as a status-bar message, then reload rows once since the open/
/// focus attempt just changed the pane map. All I/O lives in this thin
/// shell, not in [`App`], which stays a pure, testable state struct — see
/// `crate::opener::open_session` for the actual decision logic (unit tested
/// there with mocked process/store dependencies).
fn activate(app: &mut App, ctx: &Context) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let id = row.id.clone();
    let session = SessionToOpen {
        id: id.clone(),
        title: row.display_title().to_string(),
        cwd: row
            .cwd
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from(".")),
    };

    let backend = opener::resolve_backend(ctx.opener_mode, |key| std::env::var(key).ok());
    let outcome = opener::open_session(
        ctx.store,
        &SysinfoProbe,
        backend,
        &session,
        SystemCommandRunner,
    );

    let message = match outcome {
        Ok(OpenOutcome::Focused) => format!("focused existing pane (session {id})"),
        Ok(OpenOutcome::Opened) => format!("opened session {id} in a new pane/tab"),
        Ok(OpenOutcome::AlreadyOpenCannotFocus) => {
            format!("session {id} is already open (this backend can't auto-focus it)")
        }
        Ok(OpenOutcome::NoBackendDetected) => {
            "no terminal backend detected (run inside psmux/Windows Terminal, \
             or set `opener` in config.toml)"
                .to_string()
        }
        Err(err) => format!("failed to open session {id}: {err}"),
    };
    app.set_status(message);
    reload(app, ctx);
}

/// Toggle the pinned state of the selected session and persist it to the
/// store, which stays the durable source of truth — [`App`] only caches pin
/// state for sorting/display (see [`App::toggle_pin`]).
fn toggle_pin(app: &mut App, ctx: &Context) {
    let Some((id, now_pinned)) = app.toggle_pin() else {
        return;
    };
    let session_id = SessionId(id.clone());
    let result = if now_pinned {
        ctx.store.pin(&session_id)
    } else {
        ctx.store.unpin(&session_id)
    };
    let message = match result {
        Ok(()) if now_pinned => format!("pinned session {id}"),
        Ok(()) => format!("unpinned session {id}"),
        Err(err) => format!("failed to update pin for session {id}: {err}"),
    };
    app.set_status(message);
}

/// Toggle whether agent-run sessions are shown, posting the new state as a
/// status message.
fn toggle_agent_filter(app: &mut App) {
    let showing = app.toggle_agent_filter();
    app.set_status(if showing {
        "showing agent sessions".to_string()
    } else {
        "hiding agent sessions".to_string()
    });
}

/// Re-read sessions from disk and re-classify their activity, preserving
/// selection (by session id), query and scroll clamping — see
/// [`App::replace_rows`]. A read failure is tolerated: the previous rows are
/// kept rather than the TUI erroring out over a transient filesystem hiccup.
fn reload(app: &mut App, ctx: &Context) {
    if let Ok(rows) = session::load_rows(ctx.claude_home, ctx.thresholds) {
        app.replace_rows(rows);
    }
}

/// Render the whole UI for one frame.
fn render(frame: &mut Frame, app: &App) {
    let [search_area, list_area, status_area] = layout_areas(frame.area());
    render_search(frame, app, search_area);
    render_list(frame, app, list_area);
    render_status(frame, app, status_area);
}

/// Render the top search box; only shows a text cursor while actually in
/// [`Mode::Search`] (Normal mode isn't editing the query, so a blinking
/// caret there would be misleading), with a highlighted border as the
/// active-mode indicator.
fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let active = app.mode() == Mode::Search;
    let border_color = if active { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(" Search ");
    let inner = block.inner(area);
    frame.render_widget(Paragraph::new(app.query()).block(block), area);

    if active && inner.width > 0 {
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

/// Build one list row: colored activity dot, pin marker (if pinned), title
/// (or id), dimmed cwd.
fn list_item(visible: VisibleRow<'_>) -> ListItem<'static> {
    let dot = Span::styled(
        "\u{25cf} ",
        Style::default().fg(activity_color(visible.row.activity)),
    );
    let pin = if visible.pinned {
        // Plain ASCII, not a star symbol/emoji: those can render double-width
        // in some terminals and would break column alignment.
        Span::styled(
            "* ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let title = Span::raw(visible.row.display_title().to_string());
    let cwd = visible.row.cwd_display();
    let line = if cwd.is_empty() {
        Line::from(vec![dot, pin, title])
    } else {
        Line::from(vec![
            dot,
            pin,
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
    const NORMAL_HINTS: &str = "j/k\u{2191}\u{2193} move  PgUp/PgDn page  Enter open  / search  \
                                p pin  a agents  q/Esc quit";
    const SEARCH_HINTS: &str =
        "type to search  \u{2191}\u{2193} move  Enter open  Esc cancel search";

    let counts = format!("[{}/{}]", app.filtered_len(), app.total_len());
    let counts_width = counts.chars().count() as u16;

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);

    let (left, color) = match app.status() {
        Some(message) => (message.to_string(), Color::Yellow),
        None => {
            let hints = match app.mode() {
                Mode::Normal => NORMAL_HINTS,
                Mode::Search => SEARCH_HINTS,
            };
            let mut hints = hints.to_string();
            if !app.show_agents() {
                hints.push_str("  (agents hidden)");
            }
            (hints, Color::Gray)
        }
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
    use crate::session::SessionRow;
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
            is_agent: false,
        }
    }

    fn agent_row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            is_agent: true,
            ..row(id, title, cwd, activity)
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
        draw_with_width(app, 60)
    }

    /// Like [`draw`], but with a wider terminal — needed for assertions on
    /// the full hint text, which is long enough to be truncated (by design;
    /// see `render_status`) at the narrow 60-column width the other tests
    /// use to check the match count stays visible.
    fn draw_with_width(app: &App, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 15)).unwrap();
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

    #[test]
    fn pinned_rows_are_marked_and_sorted_first() {
        let rows = vec![
            row("id-alpha", "Alpha task", "/work/alpha", Activity::Alive),
            row("id-beta", "Beta task", "/work/beta", Activity::Alive),
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(11);
        app = app.with_pinned(["id-beta".to_string()].into_iter().collect());

        let text = draw(&app);
        // Pinned "Beta task" sorts first and carries the pin marker.
        let beta_line = text
            .lines()
            .find(|line| line.contains("Beta task"))
            .unwrap();
        assert!(beta_line.contains('*'), "missing pin marker:\n{text}");
        let alpha_line = text
            .lines()
            .find(|line| line.contains("Alpha task"))
            .unwrap();
        assert!(!alpha_line.contains('*'), "unexpected marker:\n{text}");
        assert!(text.contains("p pin"), "hint missing:\n{text}");
    }

    #[test]
    fn agent_sessions_are_hidden_until_toggled() {
        let rows = vec![
            row("h1", "Human task", "/work/human", Activity::Alive),
            agent_row("a1", "Agent task", "/work/agent", Activity::Alive),
        ];
        let mut app = App::new(rows);
        app.set_viewport_height(11);

        let text = draw(&app);
        assert!(text.contains("Human task"), "human row missing:\n{text}");
        assert!(
            !text.contains("Agent task"),
            "agent row shown by default:\n{text}"
        );
        // The hint text (including the hidden-indicator suffix) is longer
        // than the narrow 60-col terminal `draw` uses, so check it wider.
        let wide_text = draw_with_width(&app, 110);
        assert!(
            wide_text.contains("agents hidden"),
            "missing hidden indicator:\n{wide_text}"
        );

        app.toggle_agent_filter();
        let text = draw(&app);
        assert!(
            text.contains("Agent task"),
            "agent row not shown after toggle:\n{text}"
        );
        let wide_text = draw_with_width(&app, 110);
        assert!(!wide_text.contains("agents hidden"));
    }

    #[test]
    fn search_mode_hint_differs_from_normal_mode_hint() {
        let rows = vec![row("h1", "Human task", "/work/human", Activity::Alive)];
        let mut app = App::new(rows);
        app.set_viewport_height(11);

        let normal_text = draw_with_width(&app, 110);
        assert!(normal_text.contains("/ search"), "{normal_text}");
        assert!(!normal_text.contains("cancel search"), "{normal_text}");

        app.enter_search();
        let search_text = draw_with_width(&app, 110);
        assert!(search_text.contains("cancel search"), "{search_text}");
    }
}
