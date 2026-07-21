//! ratatui render loop: terminal setup/teardown, event handling and drawing.
//!
//! This is a thin shell over [`crate::app::App`]; all list logic lives there.
//! The terminal is restored both on normal exit and on panic (via a panic
//! hook), and mouse capture is enabled for wheel/click support. All code here
//! is cross-platform — crossterm handles the Windows specifics.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use banto_core::config::OpenerMode;
use banto_core::model::{Activity, AgeBucket, SessionId};
use banto_core::opener::SystemCommandRunner;
use banto_core::status::{AgeThresholds, SysinfoProbe, read_live_sessions};
use banto_core::store::Store;
use banto_core::watch::{ChangeSource, Debouncer, NotifyChangeSource};

use crate::app::{
    App, ClickOutcome, GroupJoinState, GroupJoinTarget, ListLine, Modal, Mode, NewSessionPlacement,
    NewSessionState,
};
use crate::opener::{self, OpenOutcome, SessionToOpen};
use crate::process::{ProcessRunner, SystemProcessRunner};
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

/// Grace period for deciding whether a lone `Esc` is genuine or the start of
/// a leaked SGR sequence (see [`resolve_escape`]). A zero-wait poll was
/// tried first but proved wrong in the field: under split-pacing delivery
/// (observed with psmux/ConPTY) the sequence's next byte isn't always
/// already queued the instant `Esc` arrives, so a zero-wait check
/// misclassified a leaked sequence as a standalone Esc — cancelling the
/// search or quitting the app mid-mouse-motion. ~30ms is still far below
/// human reaction time between two real keypresses, so it doesn't make an
/// ordinary Esc press feel laggy.
const ESCAPE_GRACE: Duration = Duration::from_millis(30);

/// Grace period between bytes of a leaked SGR sequence that arrives with its
/// leading `ESC` already missing (see [`resolve_headless_bracket`]) —
/// confirmed via `BANTO_INPUT_LOG`: real leaked sequences are a stream of
/// plain `Char` presses, ~1-2ms apart, but one recorded burst had a single
/// 96ms gap between its last digit and the `M` terminator (the byte-delivery
/// pacing is not perfectly uniform), so this needs a real safety margin
/// above that rather than the sub-20ms a purely "1-2ms apart" reading would
/// suggest. Falsely swallowing genuine typed text would require a human to
/// type the exact grammar (`[<digits;digits;digitsM`) with every one of the
/// ~9 gaps in that run under this threshold — a coincidence rare enough that
/// even a generous margin here carries negligible risk to real typing, while
/// still resolving in far less time than a human notices as lag.
const HEADLESS_GRACE: Duration = Duration::from_millis(120);

/// How long after dispatching a genuine Esc (see [`dispatch_genuine_esc`])
/// its trailing Release event is still recognized as "already handled" and
/// consumed rather than misread as a second, independent Esc — see
/// [`consume_recent_genuine_esc`]. Regression: whenever a physical Esc press
/// is held longer than [`ESCAPE_GRACE`] (which, since ordinary human key
/// taps routinely run well past 30ms, is the common case, not an edge
/// case), `resolve_escape` times out and dispatches the press *before* the
/// key's own Release has arrived — so that Release later reaches the
/// top-level loop on its own, with nothing left to say it was already
/// accounted for, and the "press must have been lost, so treat this bare
/// Release as the real Esc" fallback (added for a *different*, genuine
/// dropped-press case during mouse motion) fires a second, spurious Esc —
/// e.g. closing a modal and then immediately quitting the app. Long enough
/// to comfortably cover any realistic key hold (well beyond typical human
/// tap/hold durations), but still bounded so a Release that, for whatever
/// reason, never arrives doesn't leave the flag stuck and wrongly suppress
/// a later, actually-independent dropped-press Release.
const ESC_RELEASE_SUPPRESS_WINDOW: Duration = Duration::from_millis(500);

/// Everything the render loop needs beyond [`App`] itself: dependencies for
/// opening/focusing sessions and reloading rows from disk.
struct Context<'a> {
    claude_home: &'a Path,
    thresholds: &'a AgeThresholds,
    /// `RefCell`-wrapped so `Store::set_session_group` (which takes `&mut
    /// self`, since it wraps a transaction) can be called from the many key
    /// handlers that only hold a plain `&Context` — see [`run`].
    store: &'a RefCell<Store>,
    opener_mode: OpenerMode,
    /// Diagnostic input-event log, enabled via the `BANTO_INPUT_LOG` env var
    /// (its value is the file path). Records every raw crossterm event and
    /// every escape-resolution decision with a millisecond timestamp, for
    /// debugging input pipelines we cannot reproduce synthetically.
    input_log: std::cell::RefCell<Option<std::fs::File>>,
    /// When a genuine Esc was last dispatched (see [`dispatch_genuine_esc`]),
    /// so its trailing Release can be recognized and silently consumed
    /// instead of misfiring a second Esc (see
    /// [`consume_recent_genuine_esc`]/[`ESC_RELEASE_SUPPRESS_WINDOW`]).
    last_genuine_esc: RefCell<Option<Instant>>,
    /// An in-place launch decided by `activate`/`confirm_new_session_modal`
    /// but not yet run: only `event_loop` owns `&mut Tui`, so a key/mouse
    /// handler (which only ever sees `&Context`) stashes it here instead of
    /// running it directly — drained once per `event_loop` iteration by
    /// [`run_pending_inplace`].
    pending_inplace: RefCell<Option<opener::InPlaceLaunch>>,
}

impl Context<'_> {
    /// Append one line to the diagnostic input log (no-op when disabled).
    /// Every line is prefixed `tui:` — `BANTO_INPUT_LOG` and `_wrap`'s
    /// `BANTO_WRAP_LOG` may point at the same file, so this makes which
    /// process wrote a given line unambiguous at a glance (see
    /// `crate::wrap::WrapLog::log`'s matching `wrap:` prefix).
    fn log(&self, message: &str) {
        use std::io::Write as _;
        if let Some(file) = self.input_log.borrow_mut().as_mut() {
            let ms = std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(file, "{ms} tui: {message}");
        }
    }
}

/// Open the diagnostic log file when `BANTO_INPUT_LOG` is set.
fn open_input_log() -> Option<std::fs::File> {
    let path = std::env::var_os("BANTO_INPUT_LOG")?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
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
    store: &RefCell<Store>,
) -> Result<()> {
    let rows = session::load_rows(claude_home, thresholds)?;
    let (rows, pinned, groups, session_groups) = {
        let store = store.borrow();
        let rows = exclude_archived(rows, &store);
        let pinned = load_pinned(&store);
        let groups = load_groups(&store);
        let session_groups = load_session_groups(&store, &groups);
        (rows, pinned, groups, session_groups)
    };
    let mut app = App::new(rows)
        .with_pinned(pinned)
        .with_groups(groups, session_groups);
    let ctx = Context {
        claude_home,
        thresholds,
        store,
        opener_mode,
        input_log: std::cell::RefCell::new(open_input_log()),
        last_genuine_esc: RefCell::new(None),
        pending_inplace: RefCell::new(None),
    };
    ctx.log(&format!(
        "=== banto TUI started === own TMUX={:?} TMUX_PANE={:?}",
        std::env::var("TMUX").ok(),
        std::env::var("TMUX_PANE").ok()
    ));

    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, &mut app, &ctx);
    // Always restore the terminal, even if the loop errored.
    let restored = restore_terminal();
    result.and(restored)
}

/// Drop archived sessions from `rows` (soft-hide via `d` — see
/// `App::open_confirm_archive_modal`/`confirm_modal`). A read failure is
/// tolerated: nothing gets excluded rather than blocking the TUI.
fn exclude_archived(rows: Vec<session::SessionRow>, store: &Store) -> Vec<session::SessionRow> {
    let archived: HashSet<String> = store
        .archived_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect();
    rows.into_iter()
        .filter(|row| !archived.contains(&row.id))
        .collect()
}

/// Load every known group, alphabetical by name. Tolerant: a read failure
/// just means no groups are known yet, rather than blocking the TUI.
fn load_groups(store: &Store) -> Vec<(i64, String)> {
    let mut groups: Vec<(i64, String)> = store
        .list_groups()
        .unwrap_or_default()
        .into_iter()
        .map(|g| (g.id, g.name))
        .collect();
    groups.sort_by(|a, b| a.1.cmp(&b.1));
    groups
}

/// Load the session -> group id map by walking each group's members (fewer
/// queries than asking per-session). Tolerant: a read failure for one group
/// just means its members show as ungrouped, rather than blocking the TUI.
fn load_session_groups(store: &Store, groups: &[(i64, String)]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for &(group_id, _) in groups {
        for session_id in store.group_members(group_id).unwrap_or_default() {
            map.insert(session_id.0, group_id);
        }
    }
    map
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

/// Run an in-place launch to completion, handing banto's own pane to it:
/// leave the TUI's alternate screen/raw mode/mouse capture (so the child
/// gets a normal terminal), block on it with inherited stdio, then
/// re-initialize the TUI and reload rows (the just-used session's
/// mtime/activity changed). This is the thin, untested-by-design shell
/// around [`opener::decide_inplace_resume`]'s pure decision (see
/// [`activate`]) — the standard "shell out to a full-screen program and
/// come back" pattern; crossterm handles the re-init.
///
/// `*terminal` is replaced with a freshly re-initialized one rather than
/// reused, mirroring [`run`]'s own one-time `setup_terminal` — there is no
/// cheaper way to resume drawing after ceding the alternate screen/raw mode
/// to the child. If re-initializing fails, that error propagates (matching
/// [`run`]'s "always restore, but a failure is still an error" discipline)
/// rather than leaving the app silently stuck outside the alternate screen.
fn run_pending_inplace(
    terminal: &mut Tui,
    app: &mut App,
    ctx: &Context,
    pending: opener::InPlaceLaunch,
) -> Result<()> {
    ctx.log(&format!(
        "run_pending_inplace argv={:?} cwd={}",
        pending.argv,
        pending.cwd.display()
    ));
    restore_terminal()?;
    // Resuming can take a few seconds before `claude` paints anything of its
    // own; without this, that gap is a bare, silent shell. Drawn after
    // `restore_terminal` (so it lands in the real terminal, not the
    // about-to-be-torn-down alternate screen) — nothing runs between this
    // and the child taking over stdout to clear or overwrite it, so it
    // stays visible for the whole gap.
    draw_loading_screen(&pending.loading_lines);
    let result = SystemProcessRunner.run_in(&pending.argv, &pending.cwd);
    *terminal = setup_terminal()?;
    ctx.log(&format!("run_pending_inplace child result={result:?}"));

    let message = match result {
        Ok(_) => "returned from session".to_string(),
        Err(err) => format!("failed to run {:?}: {err}", pending.argv),
    };
    app.set_status(message);
    reload(app, ctx);
    Ok(())
}

/// Clear the screen and draw `lines` centered — the "loading screen" shown
/// while `claude` is starting (see [`run_pending_inplace`]). A true
/// persistent indicator isn't possible in-place (the pane is handed
/// entirely to the child, free to paint over or scroll past it at any
/// point), so a one-shot centered message drawn just before the hand-off is
/// the best available; it stays until `claude` overwrites it.
///
/// Best-effort throughout (`crossterm::terminal::size()` failing — e.g.
/// stdout isn't actually a terminal — falls back to a conservative 80x24;
/// individual draw calls' errors are swallowed): this is a cosmetic step
/// between tearing down the TUI and spawning the child, and must never be
/// what blocks an otherwise-working resume.
fn draw_loading_screen(lines: &[String]) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );
    for (col, row, line) in centered_lines(lines, cols, rows) {
        let _ = execute!(stdout, crossterm::cursor::MoveTo(col, row));
        print!("{line}");
    }
    use std::io::Write as _;
    let _ = stdout.flush();
}

/// Truncate each of `lines` to fit within `cols` (see [`truncate_to_width`])
/// and compute where to draw it so the whole block is centered in a
/// `cols`x`rows` terminal: each line horizontally centered on its own
/// (possibly-truncated) width, the block as a whole vertically centered.
/// Pure and terminal-free (`cols`/`rows` are supplied by the caller, real
/// terminal size in production) so it's unit-testable without one — see
/// [`draw_loading_screen`], its only caller. Degrades gracefully rather
/// than panicking/underflowing when the terminal is smaller than the
/// block: `saturating_sub` clamps both axes to 0 (top-left) instead of
/// going negative.
fn centered_lines(lines: &[String], cols: u16, rows: u16) -> Vec<(u16, u16, String)> {
    let start_row = rows.saturating_sub(lines.len() as u16) / 2;
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let truncated = truncate_to_width(line, cols);
            let width = truncated.width() as u16;
            let col = cols.saturating_sub(width) / 2;
            (col, start_row + i as u16, truncated)
        })
        .collect()
}

/// Install a panic hook that best-effort restores the terminal before the
/// default hook prints the panic message, so a panic never leaves the user in
/// raw mode on the alternate screen. Idempotent (`Once`-guarded): in-place
/// mode calls [`setup_terminal`] again on every round trip back from a
/// session, and each call installing another wrapping layer would grow the
/// hook chain unboundedly over a long-running banto session.
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

/// Height of the always-visible summary panel below the list: one row for
/// its top border/title plus [`SUMMARY_CONTENT_LINES`] content rows.
const SUMMARY_HEIGHT: u16 = 1 + SUMMARY_CONTENT_LINES;
/// Content rows inside the summary panel: title, preview, cwd, meta line.
const SUMMARY_CONTENT_LINES: u16 = 4;
/// Below this total terminal height, the summary panel is dropped entirely
/// so the list keeps whatever room it needs — the panel is a nice-to-have,
/// the list is the whole point of the app.
const MIN_HEIGHT_FOR_SUMMARY: u16 = 12;

/// Split an area into (search box, list, summary panel, status bar). The
/// summary panel collapses to zero height in a too-short terminal (see
/// [`MIN_HEIGHT_FOR_SUMMARY`]) rather than squeezing the list down to make
/// room for it.
fn layout_areas(area: Rect) -> [Rect; 4] {
    let summary_height = if area.height < MIN_HEIGHT_FOR_SUMMARY {
        0
    } else {
        SUMMARY_HEIGHT
    };
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(summary_height),
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
        let [_, list_area, _, _] = layout_areas(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(list_area.height as usize);

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(TICK_INTERVAL)? {
            match event::read()? {
                Event::Key(key) => {
                    ctx.log(&format!(
                        "loop key code={:?} kind={:?} mods={:?}",
                        key.code, key.kind, key.modifiers
                    ));
                    let code = normalize_key_code(key.code);
                    // On Windows crossterm also reports key releases; ignore
                    // them — except a bare Esc Release, which needs special
                    // handling (see the comment on that branch below).
                    if key.kind == KeyEventKind::Release {
                        if code == KeyCode::Esc {
                            // Confirmed via BANTO_INPUT_LOG: during active
                            // mouse motion, Esc's *press* can be dropped
                            // upstream entirely (same family as the
                            // dropped-ESC-byte finding for leaked mouse
                            // reports) — no Esc press of any shape reaches
                            // us, only its Release. That's a genuine
                            // dropped-press case and this bare Release is
                            // the only real signal of it — BUT a bare
                            // Release also reaches here whenever a normal,
                            // successfully-dispatched Esc's physical hold
                            // outlasts `ESCAPE_GRACE` (routine for an
                            // ordinary human tap, not just a held key):
                            // `resolve_escape` times out and dispatches
                            // before that Esc's own Release has arrived,
                            // so the Release shows up here on its own with
                            // nothing else to say it was already handled.
                            // `consume_recent_genuine_esc` tells the two
                            // cases apart via the timestamp
                            // `dispatch_genuine_esc` leaves behind.
                            if consume_recent_genuine_esc(ctx, Instant::now()) {
                                ctx.log(
                                    "loop: bare Esc Release matches a just-dispatched genuine Esc -> consuming, not re-dispatching",
                                );
                            } else {
                                ctx.log(
                                    "loop: bare Esc Release with no matching Press -> dispatching as Esc",
                                );
                                handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
                            }
                        }
                        continue;
                    }
                    // Esc needs special handling: it may be a lone Escape
                    // press, or the start of a leaked SGR mouse sequence.
                    if code == KeyCode::Esc {
                        resolve_escape(app, ctx, list_area)?;
                    } else if is_headless_bracket(code, key.modifiers) {
                        // Confirmed from BANTO_INPUT_LOG evidence: under
                        // psmux/ConPTY, leaked SGR sequences can arrive with
                        // their leading `ESC` dropped entirely, as a plain
                        // `Char('[')` press with no modifiers — see
                        // `resolve_headless_bracket`.
                        resolve_headless_bracket(app, ctx, list_area)?;
                    } else {
                        handle_key(app, code, key.modifiers, ctx);
                    }
                }
                Event::Mouse(mouse) => {
                    ctx.log(&format!(
                        "loop mouse kind={:?} col={} row={}",
                        mouse.kind, mouse.column, mouse.row
                    ));
                    handle_mouse(app, mouse, list_area, ctx)
                }
                other => ctx.log(&format!("loop other {other:?}")),
            }
        }

        // Only `event_loop` owns `&mut Tui`, so an in-place activation
        // decided above (`activate`/`confirm_new_session_modal`, however
        // deeply nested — direct Enter, a double-click, a leaked-SGR
        // double-click) is run here instead of at its own call site — see
        // [`Context::pending_inplace`].
        if let Some(pending) = ctx.pending_inplace.borrow_mut().take() {
            run_pending_inplace(terminal, app, ctx, pending)?;
        }

        if watch.poll_ready(SystemTime::now()) {
            reload(app, ctx);
        }

        // Runs every tick regardless of whether an event arrived, so a
        // status message auto-clears roughly `STATUS_TIMEOUT` after it was
        // posted even if the user never presses another key.
        app.expire_status(Instant::now());

        if app.should_quit() {
            return Ok(());
        }
    }
}

/// Translate a key press into an [`App`] action. Up/Down/PageUp/PageDown
/// behave the same in both modes (always list navigation); everything else
/// is mode-specific (see [`handle_normal_key`] / [`handle_search_key`]) —
/// not just letters (commands in Normal mode, query text in Search mode),
/// but also Left/Right/Home/End: list-jump/no-op in Normal mode, versus
/// moving the search-box text cursor in Search mode, since there's no text
/// input to edit outside of it.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, ctx: &Context) {
    // A transient notification (e.g. "pinned session X") is only relevant
    // until the user does something else — cleared here, before dispatch,
    // so it doesn't linger over the hints once its moment has passed. If
    // this same key press posts its own message (see e.g. `toggle_pin`),
    // that happens further down this same call and overwrites the clear.
    app.clear_status();
    if mods.contains(KeyModifiers::CONTROL) {
        // Ctrl+C always quits; other Ctrl combos are ignored for now.
        if code == KeyCode::Char('c') {
            app.request_quit();
        }
        return;
    }
    // A modal takes over all key handling while it's open — including
    // Up/Down, which mean "move the candidate selection" there rather than
    // "move the list selection", and Left/Right/Home/End, which move its
    // text-input cursor rather than the list.
    if app.modal().is_some() {
        handle_modal_key(app, code, ctx);
        return;
    }
    match code {
        KeyCode::Up => app.select_prev(),
        KeyCode::Down => app.select_next(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => match app.mode() {
            Mode::Normal => handle_normal_key(app, code, ctx),
            Mode::Search => handle_search_key(app, code),
        },
    }
}

/// Normal-mode keys: letters are commands, not query input. Home/End jump
/// the list selection here (there's no text input to move a cursor within);
/// Left/Right have nothing to do and are no-ops.
fn handle_normal_key(app: &mut App, code: KeyCode, ctx: &Context) {
    match code {
        KeyCode::Char('j') => app.select_next(),
        KeyCode::Char('k') => app.select_prev(),
        KeyCode::Home => app.select_first(),
        KeyCode::End => app.select_last(),
        KeyCode::Enter => activate(app, ctx),
        KeyCode::Char('s') => activate_split(app, ctx),
        KeyCode::Char('/') => app.enter_search(),
        KeyCode::Char('n') => app.open_new_session_modal(),
        KeyCode::Char('N') => app.open_new_session_modal_split(),
        KeyCode::Char('d') => app.open_confirm_archive_modal(),
        KeyCode::Char('g') => app.open_group_join_modal(),
        KeyCode::Tab => toggle_grouped_view(app),
        KeyCode::Char('p') => toggle_pin(app, ctx),
        KeyCode::Char('a') => toggle_agent_filter(app),
        KeyCode::Char('q') | KeyCode::Esc => app.request_quit(),
        _ => {}
    }
}

/// Toggle grouped view, posting the new state as a status message (bound to
/// Tab in [`Mode::Normal`]; see [`App::toggle_grouped_view`]).
fn toggle_grouped_view(app: &mut App) {
    let grouped = app.toggle_grouped_view();
    app.set_status(
        if grouped {
            "grouped view (Pinned / groups / Ungrouped)"
        } else {
            "flat view"
        }
        .to_string(),
    );
}

/// Keys while a modal is open: typed characters insert at its text-input
/// cursor, Left/Right/Home/End move that cursor (never the candidate
/// selection), Up/Down move the candidate selection instead, Tab completes
/// the input to the highlighted candidate (new-session modal only),
/// Backspace/Delete edit around the cursor, Enter confirms (see
/// [`confirm_modal`]), Esc cancels without acting. Shared across every modal
/// kind — each of `App`'s `modal_*` methods is a no-op for a modal that
/// doesn't have the relevant piece (e.g. the archive confirm dialog has no
/// text input or candidate list).
fn handle_modal_key(app: &mut App, code: KeyCode, ctx: &Context) {
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
        KeyCode::Enter => confirm_modal(app, ctx),
        KeyCode::Char(c) => app.modal_push_char(c),
        _ => {}
    }
}

/// Confirm whichever modal is open, dispatching to its kind-specific logic.
fn confirm_modal(app: &mut App, ctx: &Context) {
    match app.modal() {
        Some(Modal::NewSession(_)) => confirm_new_session_modal(app, ctx),
        Some(Modal::ConfirmArchive { .. }) => confirm_archive_modal(app, ctx),
        Some(Modal::GroupJoin(_)) => confirm_group_join_modal(app, ctx),
        None => {}
    }
}

/// Confirm the new-session modal: resolve the target cwd (the highlighted
/// candidate, or the raw typed path — see [`App::modal_new_session_target`]),
/// validate it's an existing directory (an invalid path becomes an inline
/// modal error instead of a failed launch — see [`App::modal_set_error`] —
/// so the user can correct it without losing what they typed), then launch
/// it per the modal's own [`NewSessionPlacement`] (fixed when it was opened
/// — `n` vs `N`, see [`App::open_new_session_modal`]/
/// [`App::open_new_session_modal_split`]): in-place stashes a launch for
/// `event_loop` to run, same model as [`activate`]'s resume path (see
/// [`Context::pending_inplace`]); split calls `opener::open_new_session`
/// directly and posts its outcome as a status message, same model as
/// [`activate_split`]. Neither needs a double-resume guard: a brand-new
/// `claude` launch never forks an existing session's history. A modal with
/// nothing to confirm yet (empty input, no candidates) is left open — Enter
/// does nothing, matching how Enter on an empty list does nothing in
/// [`activate`].
fn confirm_new_session_modal(app: &mut App, ctx: &Context) {
    let Some(Modal::NewSession(state)) = app.modal() else {
        return;
    };
    let placement = state.placement();
    let Some(cwd) = app.modal_new_session_target() else {
        return;
    };
    if !cwd.is_dir() {
        app.modal_set_error(format!("{} is not a directory", cwd.display()));
        return;
    }

    match placement {
        NewSessionPlacement::InPlace => {
            ctx.log(&format!(
                "confirm_new_session_modal (in-place) cwd={}",
                cwd.display()
            ));
            *ctx.pending_inplace.borrow_mut() = Some(opener::InPlaceLaunch {
                argv: opener::inplace_argv(None),
                loading_lines: opener::new_session_loading_lines(&cwd),
                cwd,
            });
        }
        NewSessionPlacement::Split => {
            let backend = opener::resolve_backend(ctx.opener_mode, |key| std::env::var(key).ok());
            let tmux_pane = std::env::var("TMUX_PANE").ok();
            let anchor =
                opener::resolve_own_anchor(backend, &SystemCommandRunner, tmux_pane.as_deref());
            // Passed through explicitly rather than left for `_wrap` to read
            // its own environment: a psmux-spawned process doesn't reliably
            // inherit banto's (docs/notes/psmux-spike.md) — see
            // `crate::wrap::WrapLog::new`.
            let wrap_log = std::env::var("BANTO_WRAP_LOG").ok();
            let outcome = opener::open_new_session(
                backend,
                &cwd,
                SystemCommandRunner,
                anchor.as_deref(),
                wrap_log.as_deref(),
            );
            ctx.log(&format!(
                "confirm_new_session_modal (split) cwd={} outcome={outcome:?}",
                cwd.display()
            ));
            let message = match outcome {
                Ok(OpenOutcome::Opened) => format!("launched a new session in {}", cwd.display()),
                Ok(OpenOutcome::NoBackendDetected) => {
                    "no terminal backend detected (run inside psmux/Windows Terminal, \
                     or set `opener` in config.toml)"
                        .to_string()
                }
                // `open_new_session` never focuses or refuses an existing
                // pane — there's no pre-existing session for a fresh launch
                // to key off of.
                Ok(
                    OpenOutcome::Focused
                    | OpenOutcome::AlreadyOpenCannotFocus
                    | OpenOutcome::AlreadyRunningUntracked,
                ) => unreachable!(),
                Err(err) => format!("failed to launch a new session in {}: {err}", cwd.display()),
            };
            app.set_status(message);
        }
    }
    app.close_modal();
}

/// Confirm the archive dialog: soft-hides the session via
/// `Store::archive_session` (never touches the real jsonl file — see
/// `App::open_confirm_archive_modal`), then reloads so it disappears from
/// the list immediately instead of waiting for the next filesystem event.
fn confirm_archive_modal(app: &mut App, ctx: &Context) {
    let Some(Modal::ConfirmArchive { session_id, title }) = app.modal() else {
        return;
    };
    let session_id = session_id.clone();
    let title = title.clone();

    let result = ctx
        .store
        .borrow()
        .archive_session(&SessionId(session_id.clone()));
    let message = match result {
        Ok(()) => format!("archived session {title}"),
        Err(err) => format!("failed to archive session {title}: {err}"),
    };
    app.set_status(message);
    app.close_modal();
    reload(app, ctx);
}

/// Confirm the group-join dialog: join the highlighted existing group, or
/// create a new one named after the typed text and join that (see
/// [`App::modal_group_join_target`]). `Store::set_session_group` moves the
/// session (clearing any prior group membership first), matching the
/// single-group-per-session UX. Updates `App`'s group cache on success so
/// grouped view reflects the change immediately.
fn confirm_group_join_modal(app: &mut App, ctx: &Context) {
    let Some(Modal::GroupJoin(state)) = app.modal() else {
        return;
    };
    let session_id = state.session_id().to_string();
    let Some(target) = app.modal_group_join_target() else {
        return;
    };

    let mut store = ctx.store.borrow_mut();
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
                app.set_status(format!("failed to create group \"{name}\": {err}"));
                app.close_modal();
                return;
            }
        },
    };
    drop(store);

    let message = match &result {
        Ok(()) => format!("joined group \"{group_name}\""),
        Err(err) => format!("failed to join group \"{group_name}\": {err}"),
    };
    app.set_status(message);
    if result.is_ok() {
        app.set_session_group_cache(&session_id, group_id, group_name);
    }
    app.close_modal();
}

/// Search-mode keys: characters type into the query (`j`/`k` included —
/// they're ordinary query text here, not movement) at the query cursor;
/// Left/Right/Home/End move that cursor, Backspace/Delete edit around it.
/// Enter confirms the search (back to Normal, keeping the query/filter, so
/// the just-filtered list can be navigated); Esc cancels it (clears the
/// query, back to Normal).
fn handle_search_key(app: &mut App, code: KeyCode) {
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

/// Dispatch a confirmed, genuine Esc — every code path that has decided a
/// buffered/pending Esc really is one (as opposed to the start of a leaked
/// sequence) must go through this, not `handle_key` directly, so its
/// timestamp is on record for [`consume_recent_genuine_esc`] to find. See
/// [`ESC_RELEASE_SUPPRESS_WINDOW`] for why this matters.
fn dispatch_genuine_esc(app: &mut App, ctx: &Context) {
    *ctx.last_genuine_esc.borrow_mut() = Some(Instant::now());
    handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
}

/// Whether a bare Esc Release we're about to treat as "the press must have
/// been lost" (see the identical branches in [`event_loop`]/[`drain_more`])
/// is actually just the trailing Release of an Esc [`dispatch_genuine_esc`]
/// already fired — in which case it must be silently swallowed, not
/// dispatched a second time. Consumes the stamp unconditionally (clearing
/// it either way) so a single stamp only ever gets checked against one
/// Release, rather than lingering to (mis)judge some later, unrelated one.
fn consume_recent_genuine_esc(ctx: &Context, now: Instant) -> bool {
    let stamp = ctx.last_genuine_esc.borrow_mut().take();
    stamp.is_some_and(|t| now.saturating_duration_since(t) <= ESC_RELEASE_SUPPRESS_WINDOW)
}

/// Resolve a `KeyCode::Esc` key event by waiting up to [`ESCAPE_GRACE`] to
/// see if anything follows: if nothing shows up, this is a genuine
/// standalone Esc. If something DOES follow, it may be the start of an SGR
/// mouse sequence (`[ < Cb ; Cx ; Cy (M|m)`, the leading `ESC` already
/// consumed) that leaked through as character keys instead of a proper
/// `Event::Mouse` — observed under some nested terminal/multiplexer setups
/// (psmux, ConPTY).
///
/// A successfully parsed sequence is translated into the corresponding
/// scroll/click action (see [`apply_sgr_action`]) rather than just dropped.
/// Mouse motion fires many of these back to back, so after resolving one
/// this keeps draining the queue (see [`drain_more`]) for another leaked
/// sequence before returning to the render loop.
fn resolve_escape(app: &mut App, ctx: &Context, list_area: Rect) -> Result<()> {
    if !event::poll(ESCAPE_GRACE)? {
        ctx.log("esc: entry grace expired with empty queue -> lone Esc");
        dispatch_genuine_esc(app, ctx);
        return Ok(());
    }
    match swallow_one_sequence(
        app,
        ctx,
        list_area,
        vec!['\u{1b}'],
        sgr::parse_prefix,
        ESCAPE_GRACE,
    )? {
        EscapeOutcome::Done => Ok(()),
        EscapeOutcome::Swallowed => drain_more(app, ctx, list_area),
    }
}

/// Resolve a `Char('[')` key event with no modifiers by buffering it as a
/// possible SGR mouse sequence with its leading `ESC` already missing.
/// Confirmed via `BANTO_INPUT_LOG`: under psmux/ConPTY, leaked SGR mouse
/// reports can arrive as a headless stream of plain `Char` press events
/// (`[`, `<`, digits, `;`, ..., `M`), with no `Esc` event, no modifier, and
/// no `Event::Mouse` ever involved — the leading `ESC` byte is dropped
/// somewhere upstream (ConPTY or crossterm's Windows input path) before it
/// ever reaches us. Unlike [`resolve_escape`] there is no ambiguity to wait
/// out at entry: an ordinary typed `[` looks identical to the start of a
/// leaked sequence at this first byte either way, so buffering always
/// begins — [`HEADLESS_GRACE`] is what keeps genuine typing from being
/// mistaken for one (see [`swallow_one_sequence`]'s `NotSgr`/timeout path).
fn resolve_headless_bracket(app: &mut App, ctx: &Context, list_area: Rect) -> Result<()> {
    match swallow_one_sequence(
        app,
        ctx,
        list_area,
        vec!['['],
        sgr::parse_headless_prefix,
        HEADLESS_GRACE,
    )? {
        EscapeOutcome::Done => Ok(()),
        EscapeOutcome::Swallowed => drain_more(app, ctx, list_area),
    }
}

/// After swallowing one complete leaked sequence, keep handling
/// immediately-queued follow-up sequences (Esc-headed or headless-bracket
/// shaped — mouse motion fires many of these back to back) before returning
/// to the render loop. Zero-wait is fine here (unlike the checks in
/// [`resolve_escape`]/[`resolve_headless_bracket`]): this only decides
/// whether to keep going, never whether to dispatch a genuine key, so
/// there's no misclassification risk to guard against with a grace period.
fn drain_more(app: &mut App, ctx: &Context, list_area: Rect) -> Result<()> {
    loop {
        if !event::poll(Duration::ZERO)? {
            return Ok(());
        }
        let read = event::read()?;
        ctx.log(&format!("esc: drain read {read:?}"));
        match read {
            Event::Key(key) if key.kind == KeyEventKind::Release => {
                if normalize_key_code(key.code) == KeyCode::Esc {
                    // See the identical branch in `event_loop`: this bare
                    // Release is either a genuinely dropped press (dispatch
                    // it) or the trailing Release of an Esc already handled
                    // via `dispatch_genuine_esc` (consume it silently).
                    if consume_recent_genuine_esc(ctx, Instant::now()) {
                        ctx.log(
                            "esc: drain saw a bare Esc Release matching a just-dispatched genuine Esc -> consuming, not re-dispatching",
                        );
                    } else {
                        ctx.log(
                            "esc: drain saw bare Esc Release with no matching Press -> dispatching as Esc",
                        );
                        handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
                    }
                    return Ok(());
                }
            }
            Event::Key(key) if normalize_key_code(key.code) == KeyCode::Esc => {
                match swallow_one_sequence(
                    app,
                    ctx,
                    list_area,
                    vec!['\u{1b}'],
                    sgr::parse_prefix,
                    ESCAPE_GRACE,
                )? {
                    EscapeOutcome::Done => return Ok(()),
                    EscapeOutcome::Swallowed => {}
                }
            }
            Event::Key(key) if is_headless_bracket(normalize_key_code(key.code), key.modifiers) => {
                match swallow_one_sequence(
                    app,
                    ctx,
                    list_area,
                    vec!['['],
                    sgr::parse_headless_prefix,
                    HEADLESS_GRACE,
                )? {
                    EscapeOutcome::Done => return Ok(()),
                    EscapeOutcome::Swallowed => {}
                }
            }
            Event::Key(key) => {
                handle_key(app, normalize_key_code(key.code), key.modifiers, ctx);
                return Ok(());
            }
            Event::Mouse(mouse) => {
                handle_mouse(app, mouse, list_area, ctx);
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

/// Whether a key event is the headless-leak shape `resolve_headless_bracket`
/// handles: `'['` is the only character that can start the SGR grammar (with
/// or without its leading `ESC`), so it's the only plain char worth
/// buffering as a possible sequence start.
fn is_headless_bracket(code: KeyCode, mods: KeyModifiers) -> bool {
    code == KeyCode::Char('[') && mods.is_empty()
}

/// Recover a key code this pipeline is known to sometimes deliver as a raw
/// control character instead of its proper `KeyCode` variant. Confirmed via
/// `BANTO_INPUT_LOG`: during active mouse motion, Backspace's press
/// consistently (22/22 occurrences in one capture, vs. 2/2 correct
/// `KeyCode::Backspace` presses while the mouse was stationary) arrives as
/// `Char('\u{7f}')` (DEL) rather than `KeyCode::Backspace`. `Char('\u{8}')`
/// (BS) and a literal `Char('\u{1b}')` (ESC) are handled the same way as a
/// defensive extension of the identical mechanism, though only DEL was
/// actually observed. `Char('\r')`/`Char('\n')` -> `KeyCode::Enter` is the
/// same kind of hedge: a literal CR/LF is never legitimate text in any of
/// banto's single-line inputs, so recovering it as Enter is safe even though
/// (unlike the DEL finding) no capture has confirmed Enter actually arrives
/// this way. All these codepoints are already inert as query text —
/// `App::push_char` drops control characters outright — so recovering the
/// intended key here costs nothing and fixes real, silent breakage.
fn normalize_key_code(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => KeyCode::Backspace,
        KeyCode::Char('\u{1b}') => KeyCode::Esc,
        KeyCode::Char('\r') | KeyCode::Char('\n') => KeyCode::Enter,
        other => other,
    }
}

/// Recognize a leaked cursor-movement sequence (plain `CSI A/B/C/D` — Up/
/// Down/Right/Left, no parameters), with or without its leading `ESC` byte —
/// the same leak pathway `sgr::parse_prefix`/`parse_headless_prefix` handle
/// for mouse reports (see the module's doc comments). `[A`/`[B`/`[C`/`[D`
/// never satisfy the SGR grammar (no `<`), so `swallow_one_sequence` would
/// otherwise reach `SgrParse::NotSgr` and replay the raw bytes as ordinary
/// key presses — dogfooding confirmed this as visible mojibake (`[` and a
/// letter landing in the search box or a modal's text input) and, worse,
/// when the buffer was Esc-headed, the replayed `Esc` silently closing
/// whatever modal was open (see the `g`-then-arrow-key regression test).
fn arrow_key_for(pending: &[char]) -> Option<KeyCode> {
    let rest: &[char] = match pending.first() {
        Some('\u{1b}') => &pending[1..],
        _ => pending,
    };
    match rest {
        ['[', 'A'] => Some(KeyCode::Up),
        ['[', 'B'] => Some(KeyCode::Down),
        ['[', 'C'] => Some(KeyCode::Right),
        ['[', 'D'] => Some(KeyCode::Left),
        _ => None,
    }
}

/// Outcome of buffering and resolving one leaked-sequence candidate.
enum EscapeOutcome {
    /// A complete SGR sequence was recognized and applied/swallowed; the
    /// caller may check for another one immediately following.
    Swallowed,
    /// The buffer didn't match (or nothing followed in time) and has been
    /// replayed as ordinary key presses; nothing more to do.
    Done,
}

/// Buffer characters starting from `pending` (already seeded with the bytes
/// [`resolve_escape`]/[`resolve_headless_bracket`] have consumed so far) and
/// re-check `parse` after each additional byte, waiting up to `grace` each
/// time for the next one (see [`resolve_escape`]/[`resolve_headless_bracket`]
/// — the same split-pacing risk applies at every byte boundary within the
/// sequence, not just its start):
/// - a complete match is applied via [`apply_sgr_action`] and swallowed;
/// - a definite mismatch replays the buffered characters as ordinary key
///   presses (see [`replay`]);
/// - if nothing more arrives in time, whatever was buffered is replayed the same way.
fn swallow_one_sequence(
    app: &mut App,
    ctx: &Context,
    list_area: Rect,
    mut pending: Vec<char>,
    parse: fn(&[char]) -> SgrParse,
    grace: Duration,
) -> Result<EscapeOutcome> {
    loop {
        match parse(&pending) {
            SgrParse::Complete(event) => {
                ctx.log(&format!("esc: swallowed complete sequence {event:?}"));
                apply_sgr_action(app, ctx, list_area, event);
                return Ok(EscapeOutcome::Swallowed);
            }
            SgrParse::NotSgr => {
                if let Some(code) = arrow_key_for(&pending) {
                    ctx.log(&format!(
                        "esc: recognized leaked arrow key {code:?} from buffer {pending:?}"
                    ));
                    handle_key(app, code, KeyModifiers::NONE, ctx);
                    return Ok(EscapeOutcome::Swallowed);
                }
                ctx.log(&format!("esc: NotSgr, replaying buffer {pending:?}"));
                replay(app, ctx, &pending);
                return Ok(EscapeOutcome::Done);
            }
            SgrParse::Incomplete => {
                if !event::poll(grace)? {
                    ctx.log(&format!(
                        "esc: per-byte grace expired, replaying buffer {pending:?}"
                    ));
                    replay(app, ctx, &pending);
                    return Ok(EscapeOutcome::Done);
                }
                let read = event::read()?;
                ctx.log(&format!("esc: buffered read {read:?}"));
                match read {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        match normalize_key_code(key.code) {
                            // CONTROL is the one modifier that changes what a
                            // `Char` key *means* (Ctrl+C is a quit signal,
                            // not the letter "c"); SHIFT does not — it's
                            // already baked into which character this is
                            // (e.g. a real, shifted `<`, which the injection
                            // harness confirmed genuinely arrives with SHIFT
                            // set even though every leaked-byte capture in
                            // BANTO_INPUT_LOG shows no modifiers at all) —
                            // so only CONTROL routes a `Char` to the
                            // interrupting-event arm below instead of being
                            // buffered as a candidate sequence byte.
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                pending.push(c)
                            }
                            KeyCode::Esc => pending.push('\u{1b}'),
                            other => {
                                end_interrupted_buffer(app, ctx, &pending);
                                handle_key(app, other, key.modifiers, ctx);
                                return Ok(EscapeOutcome::Done);
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        end_interrupted_buffer(app, ctx, &pending);
                        handle_mouse(app, mouse, list_area, ctx);
                        return Ok(EscapeOutcome::Done);
                    }
                    _ => {} // resize/focus/paste mid-burst: keep waiting
                }
            }
        }
    }
}

/// Resolve a buffer that's being interrupted by an event `swallow_one_sequence`
/// must dispatch immediately (a modified `Char` — e.g. Ctrl+C — any other
/// non-`Char`/`Esc` key, or a real `Event::Mouse`). Confirmed by inspection:
/// the previous behavior unconditionally replayed the whole buffer as typed
/// characters first, which (a) could dump a partial SGR fragment like
/// `[<35;2` into the query as garbage when the buffer really was a truncated
/// leaked sequence, and (b) for a bare-Esc-seeded buffer, buried the user's
/// real Esc action behind that same garbage instead of dispatching it as the
/// meaningful action it is.
///
/// - If `pending` starts with the literal escape character, that first
///   character is a real user action, not candidate text — it's always
///   dispatched as `Esc`, discarding only whatever came after it (which, if
///   the buffer grew this far, is far more likely a truncated leaked
///   sequence than something a human typed).
/// - Otherwise (a headless bracket-headed buffer, whose leading `[` is
///   ordinary text): a buffer that has grown past its bare `[` seed is
///   discarded for the same reason (nobody types `[<35` then immediately
///   presses another key); a buffer that's still just the bare `[` is a
///   single real keystroke and is replayed so it isn't lost.
fn end_interrupted_buffer(app: &mut App, ctx: &Context, pending: &[char]) {
    if pending.first() == Some(&'\u{1b}') {
        ctx.log(&format!(
            "esc: interrupted, dispatching leading Esc and discarding tail {:?}",
            &pending[1..]
        ));
        dispatch_genuine_esc(app, ctx);
        return;
    }
    if pending.len() > 1 {
        ctx.log(&format!("esc: interrupted, discarding buffer {pending:?}"));
    } else {
        ctx.log(&format!("esc: interrupted, replaying buffer {pending:?}"));
        replay(app, ctx, pending);
    }
}

/// Translate a successfully parsed SGR mouse sequence into the
/// corresponding [`App`] action, reusing the same scroll/click/double-click
/// path real `Event::Mouse` events go through. `Cb` (button) encodes: 64/65
/// = wheel up/down; 0 with a press terminator (`M`) = left click. Anything
/// else (drag/motion — e.g. `Cb` 35 — other buttons, releases) is
/// intentionally discarded: there's nothing sensible to do with it here.
///
/// A no-op while a modal is open — same guard as [`handle_mouse`], and for
/// the same reason: without it, a leaked SGR click "through" the overlay
/// could select/activate a background row the user can't currently see is
/// being affected (confirmed in dogfooding: `handle_mouse` already had this
/// guard, but this parallel delivery path for the same click did not).
fn apply_sgr_action(app: &mut App, ctx: &Context, list_area: Rect, event: sgr::SgrMouseEvent) {
    if app.modal().is_some() {
        return;
    }
    const WHEEL_UP: u32 = 64;
    const WHEEL_DOWN: u32 = 65;
    const LEFT_BUTTON: u32 = 0;

    match event.button {
        WHEEL_UP => app.scroll(-1),
        WHEEL_DOWN => app.scroll(1),
        LEFT_BUTTON if event.pressed => {
            // SGR coordinates are 1-based; ours are 0-based screen cells.
            let position = Position::new(event.x.saturating_sub(1), event.y.saturating_sub(1));
            if list_area.contains(position) {
                let viewport_row = (position.y - list_area.y) as usize;
                if app.click(viewport_row, Instant::now()) == Some(ClickOutcome::Activated) {
                    activate(app, ctx);
                }
            }
        }
        _ => {}
    }
}

/// Replay a buffered sequence as if its characters had arrived as ordinary
/// key presses — used both when an Esc-headed buffer (`pending[0] ==
/// '\u{1b}'`, from [`resolve_escape`]) turns out not to be SGR, and when a
/// headless bracket-headed buffer (`pending[0] == '['`, from
/// [`resolve_headless_bracket`]) does. The literal escape character is the
/// only buffered value that isn't dispatched as itself — everywhere else,
/// including a bracket-headed `pending[0]`, the character is dispatched as
/// the plain `Char` it actually was.
fn replay(app: &mut App, ctx: &Context, buffered: &[char]) {
    for &c in buffered {
        if c == '\u{1b}' {
            dispatch_genuine_esc(app, ctx);
        } else {
            handle_key(app, KeyCode::Char(c), KeyModifiers::NONE, ctx);
        }
    }
}

/// Translate a mouse event into an [`App`] action. A no-op while a modal is
/// open, so clicking "through" the overlay can't select/activate a
/// background row the user can't currently see is being affected.
fn handle_mouse(app: &mut App, mouse: event::MouseEvent, list_area: Rect, ctx: &Context) {
    if app.modal().is_some() {
        return;
    }
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

/// Resolve the selected row into a [`SessionToOpen`], defaulting `cwd` to
/// the home directory (then `.`) when the session recorded none — shared by
/// [`activate`] (in-place) and [`activate_split`].
fn selected_session(app: &App) -> Option<SessionToOpen> {
    let row = app.selected_row()?;
    Some(SessionToOpen {
        id: row.id.clone(),
        title: row.display_title().to_string(),
        cwd: row
            .cwd
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from(".")),
    })
}

/// Resume the selected session in place (Enter / double-click, the default
/// action — docs/REQUIREMENTS.md, config default `OpenerMode::InPlace`):
/// hand banto's own pane to the session instead of spawning a psmux/WT
/// pane/tab, sidestepping session-qualification, `_wrap`, and the pane map
/// entirely (`crate::opener::decide_inplace_resume`'s doc comment). Refuses
/// (posting a status message, staying in the list) when the session is
/// already running elsewhere — the only guard available here, since
/// in-place mode has no pane map of its own to consult first. The decision
/// is a pure function (`opener::decide_inplace_resume`, unit tested there
/// with a mocked probe/live list); this only does the I/O — reading live
/// state and, on success, stashing the launch for `event_loop` to actually
/// run (see [`Context::pending_inplace`]), since only it owns `&mut Tui`.
fn activate(app: &mut App, ctx: &Context) {
    let Some(session) = selected_session(app) else {
        return;
    };
    let id = session.id.clone();

    // Only consulted here — in-place mode has no pane map, so this is the
    // *only* double-resume guard, not a fallback for an untracked case.
    let live = read_live_sessions(&ctx.claude_home.join("sessions"));
    match opener::decide_inplace_resume(&session, &SysinfoProbe, &live) {
        Some(launch) => {
            ctx.log(&format!(
                "activate (in-place) session={id} argv={:?} cwd={}",
                launch.argv,
                launch.cwd.display()
            ));
            *ctx.pending_inplace.borrow_mut() = Some(launch);
        }
        None => {
            ctx.log(&format!(
                "activate (in-place) session={id} refused: already live"
            ));
            app.set_status(format!("session {id} is already running elsewhere"));
        }
    }
}

/// Open or focus the selected session in a split pane/tab (`s`), posting
/// the outcome as a status-bar message, then reload rows once since the
/// open/focus attempt just changed the pane map. This is the pre-in-place
/// behavior, kept as an explicit alternative to [`activate`]'s default
/// in-place action for whoever wants a separate pane instead (or is on a
/// non-`InPlace` `opener_mode` — see [`opener::resolve_backend`]'s doc
/// comment for how `s` resolves a backend regardless of that setting). All
/// I/O lives in this thin shell, not in [`App`], which stays a pure,
/// testable state struct — see `crate::opener::open_session` for the actual
/// decision logic (unit tested there with mocked process/store
/// dependencies).
fn activate_split(app: &mut App, ctx: &Context) {
    let Some(session) = selected_session(app) else {
        return;
    };
    let id = session.id.clone();

    let backend = opener::resolve_backend(ctx.opener_mode, |key| std::env::var(key).ok());
    // Anchor psmux splits on banto's own session-qualified pane (psmux
    // reuses window/pane ids across sessions — docs/notes/psmux-spike.md,
    // 2026-07-20) so the resume pane lands next to banto, not in whatever
    // window the client has focused, and never targets the wrong session's
    // pane by a reused bare id.
    let tmux_pane = std::env::var("TMUX_PANE").ok();
    let anchor = opener::resolve_own_anchor(backend, &SystemCommandRunner, tmux_pane.as_deref());
    // Diagnostic only (BANTO_INPUT_LOG): banto's own server/pane identity,
    // so a captured log can be compared against `_wrap`'s own $TMUX (see
    // `crate::wrap`'s BANTO_WRAP_LOG instrumentation) to confirm or rule
    // out a resumed/opened pane landing on a *different* psmux server.
    ctx.log(&format!(
        "activate_split session={id} banto TMUX={:?} TMUX_PANE={:?} anchor={anchor:?}",
        std::env::var("TMUX").ok(),
        tmux_pane
    ));
    // Only consulted when there's no pane record for this session (see
    // `opener::open_session`), so a fresh read here (rather than caching
    // across activations) keeps it current without needing to invalidate.
    let live = read_live_sessions(&ctx.claude_home.join("sessions"));
    let outcome = opener::open_session(
        &ctx.store.borrow(),
        &SysinfoProbe,
        backend,
        &session,
        SystemCommandRunner,
        anchor.as_deref(),
        &live,
    );
    ctx.log(&format!("activate_split open_session outcome={outcome:?}"));
    if let Ok(record) = ctx.store.borrow().get_pane(&SessionId(id.clone())) {
        ctx.log(&format!(
            "activate_split pane record after open = {:?}",
            record.map(|r| (r.backend, r.target))
        ));
    }

    let message = match outcome {
        Ok(OpenOutcome::Focused) => format!("focused existing pane (session {id})"),
        Ok(OpenOutcome::Opened) => format!("opened session {id} in a new pane/tab"),
        Ok(OpenOutcome::AlreadyOpenCannotFocus) => {
            format!("session {id} is already open (this backend can't auto-focus it)")
        }
        Ok(OpenOutcome::AlreadyRunningUntracked) => {
            format!(
                "session {id} is already running (banto can't focus an \
                 externally/n-launched pane yet)"
            )
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
    let store = ctx.store.borrow();
    let result = if now_pinned {
        store.pin(&session_id)
    } else {
        store.unpin(&session_id)
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
/// [`App::replace_rows`]. Archived sessions are excluded, same as the
/// initial load in [`run`]. A read failure is tolerated: the previous rows
/// are kept rather than the TUI erroring out over a transient filesystem
/// hiccup.
fn reload(app: &mut App, ctx: &Context) {
    if let Ok(rows) = session::load_rows(ctx.claude_home, ctx.thresholds) {
        let rows = exclude_archived(rows, &ctx.store.borrow());
        app.replace_rows(rows);
    }
}

/// Render the whole UI for one frame: search box, list, the always-visible
/// summary panel, status bar, and finally a modal overlay on top of
/// everything else, if one is open.
fn render(frame: &mut Frame, app: &App) {
    let [search_area, list_area, summary_area, status_area] = layout_areas(frame.area());
    render_search(frame, app, search_area);
    render_list(frame, app, list_area);
    render_summary(frame, app, summary_area);
    render_status(frame, app, status_area);
    if let Some(modal) = app.modal() {
        render_modal(frame, modal, frame.area());
    }
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
    let (visible, cursor_col) = windowed_view(app.query(), app.query_cursor(), inner.width);
    frame.render_widget(Paragraph::new(visible.as_str()).block(block), area);

    if active && inner.width > 0 {
        let cursor_x = (inner.x + cursor_col).min(inner.x + inner.width - 1);
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

/// Build one list line: a bold section-header line (grouped view only), or
/// a row — colored activity dot, pin marker (if pinned), title (or id),
/// dimmed cwd. Each is its own `ListItem`/physical line rather than a
/// header bundled into its row, matching the index space
/// `App::click`/`App::scroll`/`App::ensure_visible` all use — see
/// [`crate::app::ListLine`] for why that matters for mouse clicks.
fn list_item(line: ListLine<'_>) -> ListItem<'static> {
    match line {
        ListLine::Header(name) => ListItem::new(Line::from(Span::styled(
            name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))),
        ListLine::Row(visible) => {
            let dot = Span::styled(
                "\u{25cf} ",
                Style::default().fg(activity_color(visible.row.activity)),
            );
            let pin = if visible.pinned {
                // Plain ASCII, not a star symbol/emoji: those can render
                // double-width in some terminals and would break column
                // alignment.
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
            let row_line = if cwd.is_empty() {
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
            ListItem::new(row_line)
        }
    }
}

/// Render the always-visible summary panel below the list: the selected
/// session's activity dot + title, preview excerpt, cwd, and a meta line
/// (relative age, size, short id, pinned/agent markers). A top border is the
/// only visual separation from the list, to keep this compact. Dropped
/// entirely in a too-short terminal — see [`MIN_HEIGHT_FOR_SUMMARY`] — in
/// which case `area` is zero-height and this is a no-op.
fn render_summary(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Details ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(row) = app.selected_row() else {
        frame.render_widget(
            Paragraph::new("No session selected.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let title_line = Line::from(vec![
        Span::styled(
            "\u{25cf} ",
            Style::default().fg(activity_color(row.activity)),
        ),
        Span::styled(
            row.display_title().to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    let preview_line = Line::from(Span::styled(
        row.preview.as_deref().unwrap_or_default(),
        Style::default().fg(Color::DarkGray),
    ));
    let cwd_line = Line::from(Span::styled(
        row.cwd_display(),
        Style::default().fg(Color::DarkGray),
    ));
    let meta_line = Line::from(Span::styled(
        summary_meta(row, app.is_selected_pinned(), SystemTime::now()),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(vec![title_line, preview_line, cwd_line, meta_line]),
        inner,
    );
}

/// Build the summary panel's meta line: relative age, size, short id, and
/// any markers (pinned/agent) that apply.
fn summary_meta(row: &session::SessionRow, pinned: bool, now: SystemTime) -> String {
    let mut parts = vec![
        session::humanize_age(row.mtime, now),
        session::humanize_size(row.size),
        session::short_id(&row.id),
    ];
    if pinned {
        parts.push("pinned".to_string());
    }
    if row.is_agent {
        parts.push("agent".to_string());
    }
    parts.join("  \u{b7}  ")
}

/// Render the bottom status bar: key hints (or a transient message) on the
/// left, match count right-aligned. Rendered as two separate widgets (rather
/// than one line) so the count stays visible even when the hints are too long
/// for a narrow terminal and get truncated.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    const NORMAL_HINTS: &str = "j/k\u{2191}\u{2193} move  PgUp/PgDn page  Enter open  s split  \
                                / search  n new  N new-split  d archive  g group  Tab view  \
                                p pin  a agents  q/Esc quit";
    const SEARCH_HINTS: &str =
        "type to search  \u{2191}\u{2193} move  Enter confirm  Esc cancel search";

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
            if app.mode() == Mode::Normal && !app.grouped_view() {
                hints.push_str("  (flat)");
            }
            let hidden = app.hidden_agent_count();
            if hidden > 0 {
                let plural = if hidden == 1 { "" } else { "s" };
                hints.push_str(&format!("  ({hidden} agent session{plural} hidden)"));
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

/// Minimum margin (columns/rows) kept around a modal, even in a narrow pane.
const MODAL_MIN_MARGIN: u16 = 2;
/// Below this width, the pane counts as "narrow" (the user mostly runs
/// banto in a tall, narrow pane): the modal fills nearly the whole width
/// instead of leaving a percentage-based margin.
const MODAL_NARROW_THRESHOLD: u16 = 90;
/// In a pane at or above [`MODAL_NARROW_THRESHOLD`], the modal uses this
/// percentage of the width, before the [`MODAL_MAX_WIDTH`] cap.
const MODAL_WIDE_WIDTH_PERCENT: u32 = 60;
/// A modal's width never exceeds this even in a very wide pane, so it still
/// gets a comfortable margin instead of stretching edge to edge.
const MODAL_MAX_WIDTH: u16 = 80;
const MODAL_MAX_HEIGHT: u16 = 20;

/// Center a modal box within `area`. Width: below [`MODAL_NARROW_THRESHOLD`]
/// the modal fills nearly the whole pane (just [`MODAL_MIN_MARGIN`] on each
/// side); at or above it, the modal uses [`MODAL_WIDE_WIDTH_PERCENT`] of the
/// width, capped at [`MODAL_MAX_WIDTH`] so a very wide pane still gets a
/// generous margin instead of an edge-to-edge dialog. Height uses the same
/// minimal-margin/capped-max shape as the narrow-width case, since the
/// user's typical pane is already tall.
fn modal_area(area: Rect) -> Rect {
    let width = if area.width < MODAL_NARROW_THRESHOLD {
        area.width.saturating_sub(MODAL_MIN_MARGIN * 2)
    } else {
        let wide_width = area.width as u32 * MODAL_WIDE_WIDTH_PERCENT / 100;
        wide_width.min(MODAL_MAX_WIDTH as u32) as u16
    }
    .max(1);
    let height = area
        .height
        .saturating_sub(MODAL_MIN_MARGIN * 2)
        .clamp(1, MODAL_MAX_HEIGHT);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// The area [`render_modal`] should pass to [`Clear`] for a modal box that
/// itself occupies [`modal_area`]`(full_area)`: [`modal_area`]'s box widened
/// by one column on each side, clamped to `full_area`'s own bounds.
///
/// This is one column wider than the box on purpose. A background row's
/// full-width character (e.g. Japanese) can, by coincidence, have its glyph
/// cell sit one column to the *left* of the box while its blank
/// "continuation" cell (see `ratatui_core::buffer::Buffer::set_string`)
/// lands exactly on the box's own left border column. If that glyph cell is
/// left untouched by `Clear` (i.e. only [`modal_area`] is cleared) and
/// therefore unchanged from the previous frame, `ratatui`'s buffer-diffing
/// (`ratatui_core::buffer::diff`) treats it as an unmodified double-width
/// cell and — assuming the terminal's own rendering of it already covers
/// the next column — skips diffing that next column at all. Any different
/// content the border widget then writes into that "continuation" cell
/// (e.g. the box's own left border) is silently dropped rather than sent to
/// the backend, so the border never actually gets (re)drawn there
/// (dogfooding report; confirmed by inspecting the drawn `TestBackend`
/// buffer directly). Widening the cleared area by one column blanks that
/// glyph cell too, which makes it match the previous frame's already-blank
/// state and stops the diff from skipping the border's own cell.
fn modal_clear_area(full_area: Rect) -> Rect {
    let modal = modal_area(full_area);
    let x = modal.x.saturating_sub(1).max(full_area.x);
    let right = (modal.x + modal.width + 1).min(full_area.x + full_area.width);
    Rect::new(x, modal.y, right.saturating_sub(x), modal.height)
}

/// Inset `area` by one column on the left and right, leaving its vertical
/// extent untouched — modal content (input text, candidate lists, the
/// archive-confirm prompt) was rendered flush against the box's left/right
/// border with no breathing room (dogfooding report).
fn pad_horizontal(area: Rect) -> Rect {
    area.inner(Margin::new(1, 0))
}

/// Truncate `s` to fit within `max_width` display columns (a full-width
/// character — e.g. Japanese — counts as 2, matching how a terminal actually
/// advances the cursor for it), appending an ellipsis when anything was cut.
/// `ratatui` already clips a `Paragraph`/`ListItem` cleanly at its own area
/// boundary, so this isn't papering over a rendering bug — it's so long
/// content (a session title, a cwd, a group name) that gets cut ends in a
/// visible `…` instead of silently vanishing past the edge with no
/// indication anything was hidden.
fn truncate_to_width(s: &str, max_width: u16) -> String {
    let max_width = max_width as usize;
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // reserve 1 column for the ellipsis
    let mut out = String::new();
    let mut width = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > budget {
            break;
        }
        out.push(c);
        width += w;
    }
    out.push('\u{2026}');
    out
}

/// Compute the slice of `s` to display in a `max_width`-column single-line
/// input box so the cursor (`cursor`, a char index into `s`) stays visible,
/// plus the cursor's on-screen column relative to that slice (which the
/// caller still needs to clamp to `max_width - 1`, same as any other
/// in-box column, since a cursor at the very end of a maxed-out field sits
/// one column past the last visible character). Used by every editable
/// text input (the search box, the cwd/group-name modal inputs), now that
/// the cursor can sit anywhere in the string rather than always at the end.
///
/// Never splits a full-width character (see [`truncate_to_width`]): a
/// character whose column span crosses the window's edge is left out
/// entirely rather than half-drawn. Reduces to showing the tail of the
/// string when the cursor is at or near the end (matching a normal
/// terminal input box scrolling as you type), and the head when the cursor
/// is at or near the start.
fn windowed_view(s: &str, cursor: usize, max_width: u16) -> (String, u16) {
    let max_width = max_width as usize;
    if max_width == 0 {
        return (String::new(), 0);
    }
    let cursor_byte = s
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let prefix_width = s[..cursor_byte].width();
    let total_width = s.width();
    if total_width <= max_width {
        return (s.to_string(), prefix_width as u16);
    }

    // Choose the window's start column so the cursor stays inside it,
    // clamped so the window never scrolls further right than the string's
    // own end (i.e. it shows the tail exactly, rather than trailing blank
    // space, once the cursor is near the end).
    let max_start = total_width - max_width;
    let start_col = prefix_width
        .saturating_sub(max_width.saturating_sub(1))
        .min(max_start);

    let mut visible = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if col >= start_col {
            if col + w > start_col + max_width {
                break;
            }
            visible.push(c);
        }
        col += w;
    }
    let cursor_col = prefix_width.saturating_sub(start_col) as u16;
    (visible, cursor_col)
}

/// Render whichever modal is open as a centered overlay on top of the rest
/// of the UI: [`Clear`] blanks only the modal's own box (widened by one
/// column each side — see [`modal_clear_area`]), leaving the background list
/// visible in the margin around it — that's a modal's virtue, not a bug, and
/// worth keeping. A background row with a long full-width title (the common
/// case for this modal, since the archive/group-join prompts echo that very
/// session's own title) never actually overflows *into* the box: the box's
/// own content is always truncated to fit (see [`truncate_to_width`]), so
/// there is nothing to blank behind it — the one-column widen is solely to
/// neutralize a background full-width character straddling the border
/// itself (see [`modal_clear_area`]).
fn render_modal(frame: &mut Frame, modal: &Modal, full_area: Rect) {
    let area = modal_area(full_area);
    frame.render_widget(Clear, modal_clear_area(full_area));
    match modal {
        Modal::NewSession(state) => render_new_session_modal(frame, state, area),
        Modal::ConfirmArchive { title, .. } => render_confirm_archive_modal(frame, title, area),
        Modal::GroupJoin(state) => render_group_join_modal(frame, state, area),
    }
}

/// Render the `n` new-session dialog: a one-line cwd input (with a blinking
/// cursor, same convention as the search box), an inline validation error
/// when the last confirm attempt failed (see [`App::modal_set_error`]), and
/// a substring-filtered list of previously seen cwds to pick from instead of
/// typing a full path (Tab completes the highlighted one into the input).
fn render_new_session_modal(frame: &mut Frame, state: &NewSessionState, area: Rect) {
    let placement_label = match state.placement() {
        NewSessionPlacement::InPlace => "in-place",
        NewSessionPlacement::Split => "split",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(" New Session ({placement_label}) \u{2014} cwd "))
        .title_bottom(" Enter launch  Tab complete  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let [input_area, error_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    let (visible_input, cursor_col) =
        windowed_view(state.input(), state.cursor(), input_area.width);
    frame.render_widget(Paragraph::new(visible_input.as_str()), input_area);
    if input_area.width > 0 {
        let cursor_x = (input_area.x + cursor_col).min(input_area.x + input_area.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
    }

    if let Some(error) = state.error() {
        frame.render_widget(
            Paragraph::new(truncate_to_width(error, error_area.width))
                .style(Style::default().fg(Color::Red)),
            error_area,
        );
    }

    let candidates = state.candidates();
    if candidates.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching directories \u{2014} Enter uses the typed path.")
                .style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = candidates
        .iter()
        .map(|candidate| ListItem::new(truncate_to_width(candidate, list_area.width)))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default();
    list_state.select(state.selected());
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

/// Render the `d` archive confirm dialog: a one-line yes/no prompt naming
/// the session. Archiving only soft-hides it (`Store::archive_session`) —
/// the real jsonl file is never touched, and unarchiving isn't exposed in
/// the UI yet, so the prompt says as much to set expectations.
fn render_confirm_archive_modal(frame: &mut Frame, title: &str, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Archive Session \u{2014} confirm ")
        .title_bottom(" Enter archive  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let prompt = truncate_to_width(&format!("Archive \"{title}\"?"), inner.width);
    let lines = vec![
        Line::from(prompt),
        Line::from(Span::styled(
            "Hides it from the list; the session file itself is untouched.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render the `g` group-join dialog: a one-line new-group-name input (same
/// input/cursor convention as the search box and the new-session modal)
/// above a substring-filtered list of existing groups to pick from instead.
fn render_group_join_modal(frame: &mut Frame, state: &GroupJoinState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta))
        .title(" Join Group ")
        .title_bottom(" Enter join/create  Esc cancel ");
    let inner = pad_horizontal(block.inner(area));
    frame.render_widget(block, area);

    let [hint_area, input_area, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(truncate_to_width(
            "Type a new group name, or pick an existing one below:",
            hint_area.width,
        ))
        .style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );

    let (visible_input, cursor_col) =
        windowed_view(state.input(), state.cursor(), input_area.width);
    frame.render_widget(Paragraph::new(visible_input.as_str()), input_area);
    if input_area.width > 0 {
        let cursor_x = (input_area.x + cursor_col).min(input_area.x + input_area.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
    }

    let candidates = state.candidates();
    if candidates.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching groups \u{2014} Enter creates a new one.")
                .style(Style::default().fg(Color::DarkGray)),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = candidates
        .iter()
        .map(|candidate| ListItem::new(truncate_to_width(candidate, list_area.width)))
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default();
    list_state.select(state.selected());
    frame.render_stateful_widget(list, list_area, &mut list_state);
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
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
        }
    }

    fn agent_row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            is_agent: true,
            ..row(id, title, cwd, activity)
        }
    }

    /// A `Context` for tests exercising `handle_key`/`handle_normal_key`/
    /// `handle_search_key` directly — these are ordinary functions with no
    /// terminal dependency (only `resolve_escape` touches `event::poll`/
    /// `read`), so they're testable without a real terminal, just an
    /// in-memory store (and caller-owned `thresholds`, so the returned
    /// `Context`'s lifetime doesn't outlive a temporary) to satisfy
    /// `Context`'s shape.
    fn test_context<'a>(store: &'a RefCell<Store>, thresholds: &'a AgeThresholds) -> Context<'a> {
        Context {
            claude_home: Path::new("."),
            thresholds,
            store,
            opener_mode: OpenerMode::Auto,
            input_log: std::cell::RefCell::new(None),
            last_genuine_esc: RefCell::new(None),
            pending_inplace: RefCell::new(None),
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
        // The hint text is longer than the narrow 60-col terminal `draw`
        // uses, so check it wider (see `search_mode_hint_differs_...`) — and
        // wide enough to comfortably outlive ordinary hint-text growth (new
        // keybinding hints added over time), not just today's exact length.
        let wide_text = draw_with_width(&app, 160);
        assert!(wide_text.contains("p pin"), "hint missing:\n{wide_text}");
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
        // The hint text (including the hidden-count suffix) is longer than
        // the narrow 60-col terminal `draw` uses, so check it wider. 200 is
        // deliberately generous — this exact assertion has already broken
        // twice from ordinary hint-text growth (new keybinding hints added
        // over time), so leave real headroom rather than a tight fit.
        let wide_text = draw_with_width(&app, 200);
        assert!(
            wide_text.contains("1 agent session hidden"),
            "missing hidden indicator:\n{wide_text}"
        );

        app.toggle_agent_filter();
        let text = draw(&app);
        assert!(
            text.contains("Agent task"),
            "agent row not shown after toggle:\n{text}"
        );
        let wide_text = draw_with_width(&app, 110);
        assert!(
            !wide_text.contains("hidden)"),
            "hidden indicator still shown after toggle:\n{wide_text}"
        );
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

    #[test]
    fn slash_enters_search_mode_and_typed_characters_filter() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "Alpha", "", Activity::Alive),
            row("b", "Beta", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.mode(), Mode::Search);

        handle_key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "b");
        assert_eq!(app.filtered_len(), 1);
    }

    #[test]
    fn j_and_k_move_selection_in_normal_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "A", "", Activity::Alive),
            row("b", "B", "", Activity::Alive),
            row("c", "C", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.selected_row().unwrap().id, "b");
        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.selected_row().unwrap().id, "c");
        handle_key(&mut app, KeyCode::Char('k'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.selected_row().unwrap().id, "b");
    }

    #[test]
    fn home_and_end_jump_the_list_selection_in_normal_mode() {
        // Home/End in a modal or Search mode move a text cursor instead (see
        // `left_right_and_home_end_move_the_search_query_cursor_in_search_mode`
        // / `left_right_in_a_modal_move_the_text_cursor_not_the_candidate_selection`);
        // this guards against that routing regressing plain list navigation.
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "A", "", Activity::Alive),
            row("b", "B", "", Activity::Alive),
            row("c", "C", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::End, KeyModifiers::NONE, &ctx);
        assert_eq!(app.selected_row().unwrap().id, "c");

        handle_key(&mut app, KeyCode::Home, KeyModifiers::NONE, &ctx);
        assert_eq!(app.selected_row().unwrap().id, "a");
    }

    #[test]
    fn left_right_and_home_end_move_the_search_query_cursor_in_search_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        for c in "ac".chars() {
            handle_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE, &ctx);
        }
        assert_eq!(app.query(), "ac");
        assert_eq!(app.query_cursor(), 2);

        // Left moves the cursor, so the next typed char inserts mid-string.
        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "abc");

        handle_key(&mut app, KeyCode::Home, KeyModifiers::NONE, &ctx);
        assert_eq!(app.query_cursor(), 0);
        handle_key(&mut app, KeyCode::End, KeyModifiers::NONE, &ctx);
        assert_eq!(app.query_cursor(), 3);

        // Right at the end clamps rather than overflowing.
        handle_key(&mut app, KeyCode::Right, KeyModifiers::NONE, &ctx);
        assert_eq!(app.query_cursor(), 3);
    }

    #[test]
    fn q_quits_in_normal_mode_but_is_query_input_in_search_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "q");
        assert!(!app.should_quit());

        app.exit_search();
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE, &ctx);
        assert!(app.should_quit());
    }

    #[test]
    fn esc_quits_in_normal_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &ctx);
        assert!(app.should_quit());
    }

    #[test]
    fn esc_in_search_mode_clears_the_query_and_returns_to_normal_without_quitting() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "a");

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &ctx);

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.query(), "");
        assert!(!app.should_quit());
    }

    #[test]
    fn enter_in_search_mode_confirms_without_opening_and_keeps_the_query() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "Alpha", "", Activity::Alive),
            row("b", "Beta", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.filtered_len(), 1);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);

        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.query(), "b"); // kept, not cleared
        assert_eq!(app.filtered_len(), 1); // filter preserved
        assert!(app.status().is_none()); // nothing was opened
    }

    #[test]
    fn a_toggles_the_agent_filter_only_in_normal_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("h1", "Human", "", Activity::Alive),
            agent_row("a1", "Agent", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        assert_eq!(app.filtered_len(), 1);

        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.filtered_len(), 2);

        handle_key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE, &ctx);
        // In Search mode, 'a' is just a query character, not the toggle.
        assert_eq!(app.query(), "a");
    }

    /// The exact bytes from the bug report: continuous mouse motion (button
    /// code 35) leaking as character keys. Regression test for the crash —
    /// none of this should ever reach the query or a keybinding.
    #[test]
    fn leaked_motion_sequences_never_reach_the_query_or_quit_binding() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        // Simulate what `resolve_escape`/`swallow_one_sequence` does with a
        // fully-buffered leaked sequence: it must parse as Complete (motion,
        // discarded) and never call `push_char` for any of its characters.
        for seq in ["\u{1b}[<35;18;12M", "\u{1b}[<35;19;12M"] {
            let chars: Vec<char> = seq.chars().collect();
            assert!(matches!(sgr::parse_prefix(&chars), SgrParse::Complete(_)));
        }
        assert_eq!(app.query(), "");

        // 'q' still works as a normal query character (Search mode) and as
        // quit (Normal mode) — the leaked bytes never changed the mode.
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "q");
        assert!(!app.should_quit());
    }

    #[test]
    fn apply_sgr_action_wheel_scrolls_without_moving_selection() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(
            (0..10)
                .map(|i| row(&format!("id{i}"), "t", "", Activity::Alive))
                .collect(),
        );
        app.set_viewport_height(3);
        let list_area = Rect::new(0, 4, 60, 3);

        apply_sgr_action(
            &mut app,
            &ctx,
            list_area,
            sgr::SgrMouseEvent {
                button: 65,
                x: 1,
                y: 1,
                pressed: true,
            },
        );

        // Wheel never moves the selection.
        assert_eq!(app.selected_row().unwrap().id, "id0");
    }

    #[test]
    fn apply_sgr_action_left_click_selects_the_row_under_the_cursor() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "A", "", Activity::Alive),
            row("b", "B", "", Activity::Alive),
            row("c", "C", "", Activity::Alive),
        ]);
        app.set_viewport_height(3);
        let list_area = Rect::new(0, 4, 60, 3);

        // SGR coordinates are 1-based; row 6 (1-based) is list_area row 5
        // (0-based) => viewport row 1 (list_area starts at y=4) => "b".
        apply_sgr_action(
            &mut app,
            &ctx,
            list_area,
            sgr::SgrMouseEvent {
                button: 0,
                x: 1,
                y: 6,
                pressed: true,
            },
        );

        assert_eq!(app.selected_row().unwrap().id, "b");
    }

    #[test]
    fn apply_sgr_action_ignores_motion_and_releases() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(3);
        let list_area = Rect::new(0, 4, 60, 3);

        for event in [
            sgr::SgrMouseEvent {
                button: 35,
                x: 1,
                y: 5,
                pressed: true,
            }, // pure motion
            sgr::SgrMouseEvent {
                button: 0,
                x: 1,
                y: 5,
                pressed: false,
            }, // left-button release
        ] {
            apply_sgr_action(&mut app, &ctx, list_area, event);
        }

        assert_eq!(app.selected_row().unwrap().id, "a");
    }

    #[test]
    fn is_headless_bracket_matches_only_char_bracket_with_no_modifiers() {
        assert!(is_headless_bracket(KeyCode::Char('['), KeyModifiers::NONE));
        assert!(!is_headless_bracket(KeyCode::Char('['), KeyModifiers::ALT));
        assert!(!is_headless_bracket(
            KeyCode::Char('['),
            KeyModifiers::CONTROL
        ));
        assert!(!is_headless_bracket(KeyCode::Char('<'), KeyModifiers::NONE));
        assert!(!is_headless_bracket(KeyCode::Esc, KeyModifiers::NONE));
    }

    /// The exact event stream from `BANTO_INPUT_LOG` (lines 26-36 of a real
    /// repro session): a leaked motion report arriving as plain `Char`
    /// presses with no leading `Esc` and no modifiers at all —
    /// `resolve_escape` never ran once during that entire session (zero
    /// "esc:" log lines anywhere in it). This is the shape
    /// `resolve_headless_bracket` exists to catch.
    #[test]
    fn headless_motion_sequence_from_the_real_log_is_swallowed_as_motion() {
        let chars: Vec<char> = "[<35;41;14M".chars().collect();
        assert_eq!(
            sgr::parse_headless_prefix(&chars),
            SgrParse::Complete(sgr::SgrMouseEvent {
                button: 35,
                x: 41,
                y: 14,
                pressed: true,
            })
        );
    }

    #[test]
    fn headless_wheel_sequence_scrolls_via_apply_sgr_action() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();
        let list_area = Rect::new(0, 4, 60, 3);

        let chars: Vec<char> = "[<65;1;1M".chars().collect();
        match sgr::parse_headless_prefix(&chars) {
            SgrParse::Complete(event) => apply_sgr_action(&mut app, &ctx, list_area, event),
            other => panic!("expected Complete, got {other:?}"),
        }

        assert_eq!(app.query(), "");
        assert_eq!(app.mode(), Mode::Search);
    }

    /// A real human typing "[x" (never continuing into the SGR grammar,
    /// which requires `<` next) must still see both characters land in the
    /// query once the recognizer gives up on it — a fix for the flood must
    /// not eat legitimate keystrokes.
    #[test]
    fn replay_of_a_headless_bracket_mismatch_types_every_buffered_character() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        let buffered: Vec<char> = "[x".chars().collect();
        replay(&mut app, &ctx, &buffered);

        assert_eq!(app.query(), "[x");
        assert_eq!(app.mode(), Mode::Search);
    }

    /// A lone `[` that times out with nothing else queued (the user typed
    /// `[` and paused) must still land in the query, not vanish.
    #[test]
    fn replay_of_a_lone_headless_bracket_types_the_bracket() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        replay(&mut app, &ctx, &['[']);

        assert_eq!(app.query(), "[");
    }

    #[test]
    fn normalize_key_code_recovers_the_confirmed_control_char_corruptions() {
        assert_eq!(
            normalize_key_code(KeyCode::Char('\u{7f}')),
            KeyCode::Backspace
        );
        assert_eq!(
            normalize_key_code(KeyCode::Char('\u{8}')),
            KeyCode::Backspace
        );
        assert_eq!(normalize_key_code(KeyCode::Char('\u{1b}')), KeyCode::Esc);
        assert_eq!(normalize_key_code(KeyCode::Char('\r')), KeyCode::Enter);
        assert_eq!(normalize_key_code(KeyCode::Char('\n')), KeyCode::Enter);

        // Everything else passes through unchanged.
        assert_eq!(normalize_key_code(KeyCode::Char('[')), KeyCode::Char('['));
        assert_eq!(normalize_key_code(KeyCode::Char('a')), KeyCode::Char('a'));
        assert_eq!(normalize_key_code(KeyCode::Backspace), KeyCode::Backspace);
        assert_eq!(normalize_key_code(KeyCode::Esc), KeyCode::Esc);
        assert_eq!(normalize_key_code(KeyCode::Enter), KeyCode::Enter);
    }

    /// Regression test for the exact corruption confirmed in `BANTO_INPUT_LOG`:
    /// during active mouse motion, every one of 22 Backspace presses arrived
    /// as `Char('\u{7f}')` (DEL) instead of `KeyCode::Backspace` — silently
    /// eaten by `App::push_char`'s control-character guard before this fix,
    /// since it went through `handle_search_key`'s `Char(c) => push_char(c)`
    /// arm like any other typed character. The 2 Backspace presses captured
    /// while the mouse was stationary arrived correctly as `KeyCode::Backspace`
    /// and are covered by other tests using that code directly.
    #[test]
    fn backspace_delivered_as_del_during_motion_still_deletes_query_text() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();
        app.push_char('a');
        app.push_char('b');
        assert_eq!(app.query(), "ab");

        let code = normalize_key_code(KeyCode::Char('\u{7f}'));
        handle_key(&mut app, code, KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "a");

        handle_key(&mut app, code, KeyModifiers::NONE, &ctx);
        assert_eq!(app.query(), "");
    }

    /// A bare-Esc-seeded buffer that grew past its seed (e.g. an Esc
    /// followed by a `[` before something else interrupted it) must still
    /// honor the real Esc action — the tail is discarded, not typed.
    #[test]
    fn end_interrupted_buffer_dispatches_leading_esc_and_discards_the_tail() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();
        app.push_char('x');

        end_interrupted_buffer(&mut app, &ctx, &['\u{1b}', '[', '<', '3']);

        // Esc fired (query cleared, back to Normal); the "[ < 3" tail never
        // reached the query.
        assert_eq!(app.mode(), Mode::Normal);
        assert_eq!(app.query(), "");
    }

    /// A bare-Esc-seeded buffer that's still just the seed itself (nothing
    /// followed before the interruption) is a single real keystroke and
    /// must still be dispatched.
    #[test]
    fn end_interrupted_buffer_replays_a_lone_bare_esc_seed() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        end_interrupted_buffer(&mut app, &ctx, &['\u{1b}']);

        assert_eq!(app.mode(), Mode::Normal);
    }

    /// A headless-bracket-seeded buffer that grew past its `[` seed is
    /// discarded silently — no query garbage, no stray action — since it's
    /// far more likely a truncated leaked sequence than genuine typing.
    #[test]
    fn end_interrupted_buffer_discards_a_grown_headless_bracket_buffer() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        end_interrupted_buffer(&mut app, &ctx, &['[', '<', '3', '5']);

        assert_eq!(app.query(), "");
        assert_eq!(app.mode(), Mode::Search);
    }

    /// A headless-bracket buffer that never grew past its bare `[` seed is a
    /// single real keystroke and must still reach the query.
    #[test]
    fn end_interrupted_buffer_replays_a_lone_headless_bracket_seed() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        end_interrupted_buffer(&mut app, &ctx, &['[']);

        assert_eq!(app.query(), "[");
    }

    /// The dispatch target of the modifier-preserving fix: `swallow_one_sequence`
    /// now matches `KeyCode::Char(c) if key.modifiers.is_empty()` before
    /// absorbing a char into the buffer, so a modified key like Ctrl+C falls
    /// through to the interrupting-event arm and is dispatched via
    /// `handle_key(app, other, key.modifiers, ctx)` with its modifier intact.
    /// Before this fix, `pending.push(c)` matched on `key.code` alone and
    /// silently discarded the modifier, downgrading Ctrl+C to a plain `'c'`
    /// (a no-op in Normal mode) and losing the quit.
    #[test]
    fn ctrl_c_still_quits_when_dispatched_with_its_modifier_intact() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, &ctx);

        assert!(app.should_quit());
    }

    #[test]
    fn n_opens_the_new_session_modal_from_normal_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE, &ctx);

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.placement(), NewSessionPlacement::InPlace);
    }

    #[test]
    fn n_is_query_text_in_search_mode_not_the_new_session_shortcut() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        handle_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE, &ctx);

        assert_eq!(app.query(), "n");
        assert!(app.modal().is_none());
    }

    #[test]
    fn shift_n_opens_the_new_session_modal_in_split_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('N'), KeyModifiers::NONE, &ctx);

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.placement(), NewSessionPlacement::Split);
    }

    #[test]
    fn shift_n_is_query_text_in_search_mode_not_the_split_new_session_shortcut() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        handle_key(&mut app, KeyCode::Char('N'), KeyModifiers::NONE, &ctx);

        assert_eq!(app.query(), "N");
        assert!(app.modal().is_none());
    }

    #[test]
    fn new_session_modal_title_names_its_placement() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        app.open_new_session_modal();
        let in_place_text = draw(&app);
        assert!(
            in_place_text.contains("(in-place)"),
            "in-place label missing:\n{in_place_text}"
        );

        app.open_new_session_modal_split();
        let split_text = draw(&app);
        assert!(
            split_text.contains("(split)"),
            "split label missing:\n{split_text}"
        );
    }

    #[test]
    fn esc_closes_the_open_modal_without_quitting_or_reaching_the_recognizer() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &ctx);

        assert!(app.modal().is_none());
        assert!(!app.should_quit());
    }

    #[test]
    fn dispatch_genuine_esc_stamps_the_context_and_dispatches_the_key() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        assert!(ctx.last_genuine_esc.borrow().is_none());

        dispatch_genuine_esc(&mut app, &ctx);

        assert!(app.should_quit()); // Normal mode: Esc quits.
        assert!(ctx.last_genuine_esc.borrow().is_some());
    }

    #[test]
    fn consume_recent_genuine_esc_suppresses_within_the_window_and_clears_the_stamp() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let t0 = Instant::now();
        *ctx.last_genuine_esc.borrow_mut() = Some(t0);

        assert!(consume_recent_genuine_esc(
            &ctx,
            t0 + Duration::from_millis(50)
        ));
        // Consumed: checking again immediately finds nothing left to match.
        assert!(!consume_recent_genuine_esc(
            &ctx,
            t0 + Duration::from_millis(50)
        ));
    }

    #[test]
    fn consume_recent_genuine_esc_does_not_suppress_once_the_window_has_passed() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let t0 = Instant::now();
        *ctx.last_genuine_esc.borrow_mut() = Some(t0);

        assert!(!consume_recent_genuine_esc(
            &ctx,
            t0 + ESC_RELEASE_SUPPRESS_WINDOW + Duration::from_millis(1)
        ));
        // Still cleared even though it didn't match, so a stale stamp never
        // lingers to wrongly suppress a later, unrelated Release.
        assert!(ctx.last_genuine_esc.borrow().is_none());
    }

    #[test]
    fn consume_recent_genuine_esc_is_false_with_no_stamp_at_all() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);

        assert!(!consume_recent_genuine_esc(&ctx, Instant::now()));
    }

    /// Regression: a held Esc press that outlasts `ESCAPE_GRACE` (routine
    /// for an ordinary human tap, not just a deliberately held key) makes
    /// `resolve_escape` dispatch the press before its own trailing Release
    /// has arrived; that Release then reaches the top-level loop's "press
    /// must have been lost" fallback with nothing left to say it was
    /// already handled. Reproduces the sequence directly: a genuine
    /// dispatch (as `resolve_escape`'s entry-grace-timeout branch would
    /// perform) closes the modal, then the same guard the fixed
    /// `event_loop`/`drain_more` branches run before falling back to a
    /// second dispatch — proving that second Esc never reaches
    /// `handle_key`.
    #[test]
    fn a_delayed_esc_release_after_a_genuine_dispatch_does_not_fire_a_second_esc() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        // The genuine press: closes the modal (mirrors `resolve_escape`'s
        // entry-grace-timeout branch).
        dispatch_genuine_esc(&mut app, &ctx);
        assert!(app.modal().is_none());
        assert!(!app.should_quit());

        // The trailing Release, arriving on its own moments later: the
        // fallback's guard must recognize and consume it rather than
        // re-dispatching.
        if !consume_recent_genuine_esc(&ctx, Instant::now()) {
            handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &ctx);
        }

        // Without the fix this second Esc would have quit the app (Normal
        // mode, no modal open any more).
        assert!(!app.should_quit());
    }

    #[test]
    fn typing_and_arrow_keys_drive_the_open_modal_instead_of_the_background_list() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "Alpha", "/work/alpha", Activity::Alive),
            row("b", "Beta", "/work/beta", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        let selected_before = app.selected_row().unwrap().id.clone();
        app.open_new_session_modal();

        for c in "beta".chars() {
            handle_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE, &ctx);
        }
        handle_key(&mut app, KeyCode::Up, KeyModifiers::NONE, &ctx);

        // Up/Down moved the modal's candidate selection, not the background
        // list's selection.
        assert_eq!(app.selected_row().unwrap().id, selected_before);
        assert_eq!(
            app.modal_new_session_target(),
            Some(PathBuf::from("/work/beta"))
        );
    }

    #[test]
    fn left_right_in_a_modal_move_the_text_cursor_not_the_candidate_selection() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "Alpha", "/work/alpha", Activity::Alive),
            row("b", "Beta", "/work/beta", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        for c in "wo".chars() {
            handle_key(&mut app, KeyCode::Char(c), KeyModifiers::NONE, &ctx);
        }
        handle_key(&mut app, KeyCode::Down, KeyModifiers::NONE, &ctx); // selects /work/beta
        let target_before = app.modal_new_session_target();

        handle_key(&mut app, KeyCode::Left, KeyModifiers::NONE, &ctx);

        // Left didn't touch the candidate selection...
        assert_eq!(app.modal_new_session_target(), target_before);

        // ...but it did move the text cursor: a following char inserts
        // between 'w' and 'o', not appended at the end.
        handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE, &ctx);
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "wxo");
    }

    #[test]
    fn modal_area_shrinks_margin_in_a_narrow_pane_scales_by_percentage_in_a_mid_one_and_caps_in_a_wide_one()
     {
        // Narrow: minimal margin, modal fills almost the whole width.
        let narrow = modal_area(Rect::new(0, 0, 30, 20));
        assert_eq!(narrow.width, 26); // 30 - 2*MODAL_MIN_MARGIN
        assert_eq!(narrow.x, 2);

        // Mid: at/above the narrow threshold but under the cap, the modal
        // uses MODAL_WIDE_WIDTH_PERCENT of the width.
        let mid = modal_area(Rect::new(0, 0, 100, 30));
        assert_eq!(mid.width, 60); // 100 * 60 / 100
        assert_eq!(mid.x, 20); // centered: (100 - 60) / 2

        // Wide: capped at MODAL_MAX_WIDTH, leaving a large margin.
        let wide = modal_area(Rect::new(0, 0, 200, 50));
        assert_eq!(wide.width, 80); // MODAL_MAX_WIDTH
        assert_eq!(wide.x, 60); // centered: (200 - 80) / 2
    }

    #[test]
    fn modal_clear_area_widens_the_modal_box_by_one_column_on_each_side() {
        let full = Rect::new(0, 0, 40, 15);
        let modal = modal_area(full);

        let clear = modal_clear_area(full);

        assert_eq!(clear.x, modal.x - 1);
        assert_eq!(clear.width, modal.width + 2);
        assert_eq!(clear.y, modal.y);
        assert_eq!(clear.height, modal.height);
    }

    #[test]
    fn modal_clear_area_clamps_at_the_frame_edges_in_a_maximally_narrow_pane() {
        // A pane so narrow that modal_area is flush against x=0 (its own
        // saturating-sub clamp already kicks in); the widened clear area
        // must not underflow past the frame's own left edge, nor extend
        // past its right edge.
        let full = Rect::new(0, 0, 1, 10);
        assert_eq!(modal_area(full).x, 0);

        let clear = modal_clear_area(full);

        assert!(clear.x >= full.x);
        assert!(clear.x + clear.width <= full.x + full.width);
    }

    #[test]
    fn render_modal_neutralizes_a_background_full_width_char_straddling_the_left_border() {
        let modal = Modal::ConfirmArchive {
            session_id: "a".to_string(),
            title: "Alpha".to_string(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                // Simulate a background full-width character whose glyph
                // cell sits exactly 1 column left of the modal's own left
                // border (x=2, see
                // `modal_area_shrinks_margin_in_a_narrow_pane_...`), with its
                // blank "continuation" cell (see
                // `ratatui_core::buffer::Buffer::set_string`) landing
                // exactly on the border column — the scenario that used to
                // leave a dangling half-glyph once the border overwrote
                // only the continuation cell.
                frame.render_widget(Span::raw("あ"), Rect::new(1, 5, 2, 1));
                render_modal(frame, &modal, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The border survived intact...
        assert_eq!(buf.cell((2, 5)).unwrap().symbol(), "\u{2502}");
        // ...and the dangling glyph was neutralized (blanked) instead of
        // being left half-erased.
        assert_eq!(buf.cell((1, 5)).unwrap().symbol(), " ");
    }

    #[test]
    fn render_modal_neutralizes_a_background_full_width_char_straddling_the_right_border() {
        let modal = Modal::ConfirmArchive {
            session_id: "a".to_string(),
            title: "Alpha".to_string(),
        };
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                // Mirror of the left-border case: glyph at column 38, just
                // past the box's own right border at column 37 (see
                // `modal_area_shrinks_margin_in_a_narrow_pane_...`:
                // width 36 starting at x=2 ends at column 37).
                frame.render_widget(Span::raw("あ"), Rect::new(38, 5, 2, 1));
                render_modal(frame, &modal, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The right border survived intact...
        assert_eq!(buf.cell((37, 5)).unwrap().symbol(), "\u{2502}");
        // ...and the adjacent glyph was neutralized rather than left
        // dangling at the very edge of the frame.
        assert_eq!(buf.cell((38, 5)).unwrap().symbol(), " ");
    }

    #[test]
    fn render_summary_shows_the_selected_session_and_a_placeholder_when_empty() {
        let mut app = App::new(vec![row(
            "9f8e7d6c-uuid-rest",
            "Fix login",
            "/work/alpha",
            Activity::Alive,
        )]);
        app.set_viewport_height(10);

        let text = draw(&app);
        assert!(text.contains("Details"), "panel border missing:\n{text}");
        assert!(text.contains("Fix login"), "title missing:\n{text}");
        assert!(text.contains("/work/alpha"), "cwd missing:\n{text}");
        // Meta line: size and short id are deterministic even though the
        // relative-age part depends on `SystemTime::now()`.
        assert!(text.contains("0 B"), "size missing:\n{text}");
        assert!(text.contains("9f8e7d6c"), "short id missing:\n{text}");

        let empty_app = App::new(Vec::new());
        let empty_text = draw(&empty_app);
        assert!(
            empty_text.contains("No session selected."),
            "placeholder missing:\n{empty_text}"
        );
    }

    #[test]
    fn render_summary_marks_a_pinned_selection() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);
        app = app.with_pinned(["a".to_string()].into_iter().collect());

        let text = draw(&app);
        assert!(text.contains("pinned"), "pinned marker missing:\n{text}");
    }

    #[test]
    fn summary_panel_is_dropped_in_a_too_short_terminal() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(3);

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(
            !text.contains("Details"),
            "summary panel shown in a too-short terminal:\n{text}"
        );
    }

    #[test]
    fn render_new_session_modal_shows_input_and_matching_candidates() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "alpha".chars() {
            app.modal_push_char(c);
        }

        let text = draw_with_width(&app, 110);
        assert!(text.contains("New Session"), "modal title missing:\n{text}");
        assert!(text.contains("alpha"), "typed input missing:\n{text}");
        assert!(
            text.contains("/work/alpha"),
            "matching candidate missing:\n{text}"
        );
        assert!(text.contains("Tab complete"), "tab hint missing:\n{text}");
    }

    #[test]
    fn new_session_modal_filtering_is_a_literal_substring_match_not_fuzzy() {
        let mut app = App::new(vec![
            row("a", "one", "/work/alpha", Activity::Alive),
            row("b", "two", "/other/beta", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        // "obeta" is a valid fuzzy subsequence of "/other/beta" (o-b-e-t-a
        // appear in that order, just not contiguously) but never occurs as
        // a literal substring of it — proves this filters by substring, not
        // the same fuzzy ranker the main search box uses.
        for c in "obeta".chars() {
            app.modal_push_char(c);
        }

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        // "obeta" is not a literal substring of "/other/beta" (there's an
        // "her/b" in between), so a substring matcher finds nothing, even
        // though a fuzzy matcher would.
        assert!(state.candidates().is_empty());
    }

    #[test]
    fn tab_completes_the_highlighted_candidate_into_the_input() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "alp".chars() {
            app.modal_push_char(c);
        }

        app.modal_complete_candidate();

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected an open new-session modal");
        };
        assert_eq!(state.input(), "/work/alpha");
    }

    #[test]
    fn confirm_modal_sets_an_inline_error_and_stays_open_for_a_nonexistent_directory() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(Vec::new());
        app.set_viewport_height(10);
        app.open_new_session_modal();
        for c in "/definitely/not/a/real/path".chars() {
            app.modal_push_char(c);
        }

        confirm_modal(&mut app, &ctx);

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected the modal to still be open");
        };
        assert!(state.error().is_some(), "expected an inline error");
        // Nothing was typed away: editing further clears the error again.
        app.modal_push_char('x');
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected the modal to still be open");
        };
        assert!(state.error().is_none());
    }

    #[test]
    fn confirm_new_session_modal_stages_an_in_place_launch_and_closes() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(Vec::new());
        app.set_viewport_height(10);
        app.open_new_session_modal();
        // "." always exists as a directory, whatever the test's cwd is.
        app.modal_push_char('.');

        confirm_modal(&mut app, &ctx);

        let launch = ctx
            .pending_inplace
            .borrow_mut()
            .take()
            .expect("expected a pending in-place launch");
        assert_eq!(launch.argv, ["claude"].map(str::to_string));
        assert_eq!(launch.cwd, PathBuf::from("."));
        assert_eq!(
            launch.loading_lines,
            opener::new_session_loading_lines(&PathBuf::from("."))
        );
        assert!(app.modal().is_none(), "modal should have closed");
    }

    /// The SGR recognizer runs at the event-loop level, entirely before
    /// `handle_key`'s modal check — so a leaked mouse sequence must still be
    /// swallowed cleanly (never landing in the modal's input) regardless of
    /// whether a modal happens to be open when it arrives.
    #[test]
    fn leaked_sgr_sequences_are_swallowed_even_while_a_modal_is_open() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();
        let list_area = Rect::new(0, 4, 60, 3);

        let chars: Vec<char> = "[<35;18;12M".chars().collect();
        match sgr::parse_headless_prefix(&chars) {
            SgrParse::Complete(event) => apply_sgr_action(&mut app, &ctx, list_area, event),
            other => panic!("expected Complete, got {other:?}"),
        }

        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected the modal to still be open");
        };
        assert_eq!(state.input(), "");
    }

    #[test]
    fn d_opens_the_archive_confirm_modal_with_the_selected_session() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('d'), KeyModifiers::NONE, &ctx);

        let Some(Modal::ConfirmArchive { session_id, title }) = app.modal() else {
            panic!("expected an open archive-confirm modal");
        };
        assert_eq!(session_id, "a");
        assert_eq!(title, "Alpha");
    }

    #[test]
    fn enter_stages_an_in_place_resume_for_the_selected_session() {
        // `claude_home` is `.` (see `test_context`), under which no
        // `sessions/` live-state directory exists, so `read_live_sessions`
        // tolerantly yields an empty list — the session is never live here,
        // deterministically, without depending on real process state. This
        // never spawns anything (unlike `s`/`activate_split`, which is
        // deliberately left untested here — it can shell out to a real
        // psmux/wt binary depending on env/backend resolution): `activate`
        // only stashes the launch for `event_loop` to run.
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("sess-1", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);

        let launch = ctx
            .pending_inplace
            .borrow_mut()
            .take()
            .expect("expected a pending in-place launch");
        assert_eq!(
            launch.argv,
            ["claude", "--resume", "sess-1"].map(str::to_string)
        );
        assert_eq!(launch.cwd, PathBuf::from("/work/alpha"));
        assert_eq!(launch.loading_lines, opener::resume_loading_lines("Alpha"));
        // No refusal status posted for the ordinary "proceed" case.
        assert!(app.status().is_none());
    }

    #[test]
    fn enter_does_nothing_when_the_list_is_empty() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(Vec::new());
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);

        assert!(ctx.pending_inplace.borrow().is_none());
    }

    #[test]
    fn g_opens_the_group_join_modal_for_the_selected_session() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('g'), KeyModifiers::NONE, &ctx);

        assert!(matches!(app.modal(), Some(Modal::GroupJoin(_))));
    }

    #[test]
    fn tab_toggles_grouped_view_in_normal_mode() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        assert!(app.grouped_view());

        handle_key(&mut app, KeyCode::Tab, KeyModifiers::NONE, &ctx);

        assert!(!app.grouped_view());
        assert_eq!(app.status(), Some("flat view"));
    }

    #[test]
    fn tab_completes_a_candidate_in_the_new_session_modal_not_grouped_view_toggle() {
        // Tab means different things depending on context: with a modal
        // open it completes the candidate (see the new-session modal
        // tests); grouped-view toggling only applies in Normal mode with no
        // modal open, per `handle_key`'s modal-first routing.
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_new_session_modal();

        handle_key(&mut app, KeyCode::Tab, KeyModifiers::NONE, &ctx);

        // Grouped view is untouched; the modal's input got completed instead.
        assert!(app.grouped_view());
        let Some(Modal::NewSession(state)) = app.modal() else {
            panic!("expected the new-session modal to still be open");
        };
        assert_eq!(state.input(), "/work/alpha");
    }

    #[test]
    fn render_confirm_archive_modal_shows_the_session_title() {
        let mut app = App::new(vec![row("a", "Fix login", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_confirm_archive_modal();

        let text = draw_with_width(&app, 110);
        assert!(text.contains("Archive Session"), "title missing:\n{text}");
        assert!(text.contains("Fix login"), "session name missing:\n{text}");
        assert!(text.contains("Enter archive"), "hint missing:\n{text}");
    }

    #[test]
    fn render_group_join_modal_shows_input_and_matching_groups() {
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)])
            .with_groups(vec![(1, "work".to_string())], HashMap::new());
        app.set_viewport_height(10);
        app.open_group_join_modal();
        app.modal_push_char('w');

        let text = draw_with_width(&app, 110);
        assert!(text.contains("Join Group"), "title missing:\n{text}");
        assert!(text.contains('w'), "typed input missing:\n{text}");
        assert!(text.contains("work"), "matching group missing:\n{text}");
    }

    #[test]
    fn render_list_shows_section_headers_in_grouped_view() {
        let mut app = App::new(vec![
            row("a", "Alpha", "", Activity::Alive),
            row("b", "Beta", "", Activity::Alive),
        ])
        .with_pinned(["a".to_string()].into_iter().collect());
        app.set_viewport_height(10);

        let text = draw(&app);
        assert!(text.contains("Pinned"), "pinned header missing:\n{text}");
        assert!(
            text.contains("Ungrouped"),
            "ungrouped header missing:\n{text}"
        );
    }

    // --- dogfooding fixes: full-width truncation / modal padding ---------

    #[test]
    fn truncate_to_width_leaves_short_text_untouched() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_cuts_ascii_and_appends_an_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 6), "hello\u{2026}");
    }

    #[test]
    fn truncate_to_width_never_splits_a_full_width_character() {
        // Each "あ" is 2 display columns; the budget for content is
        // max_width - 1 (reserved for the ellipsis) = 4, which fits exactly
        // 2 of them (4 columns) with none left over for a 3rd.
        assert_eq!(truncate_to_width(&"あ".repeat(5), 5), "ああ\u{2026}");
    }

    #[test]
    fn centered_lines_centers_a_single_short_line() {
        let lines = ["hi".to_string()];

        let placed = centered_lines(&lines, 80, 24);

        // (80 - 2) / 2 = 39; a single line is vertically centered the same
        // way: (24 - 1) / 2 = 11.
        assert_eq!(placed, vec![(39, 11, "hi".to_string())]);
    }

    #[test]
    fn centered_lines_stacks_multiple_lines_as_a_vertically_centered_block() {
        let lines = ["a".to_string(), "bb".to_string(), "ccc".to_string()];

        let placed = centered_lines(&lines, 10, 5);

        // Block height 3 in 5 rows: start row (5 - 3) / 2 = 1, then stacked.
        // Each line is independently horizontally centered on its own width.
        assert_eq!(
            placed,
            vec![
                (4, 1, "a".to_string()),
                (4, 2, "bb".to_string()),
                (3, 3, "ccc".to_string()),
            ]
        );
    }

    #[test]
    fn centered_lines_accounts_for_full_width_characters_when_centering() {
        // "ああ" is 4 display columns (2 per character), not 2.
        let lines = ["ああ".to_string()];

        let placed = centered_lines(&lines, 20, 1);

        assert_eq!(placed, vec![(8, 0, "ああ".to_string())]);
    }

    #[test]
    fn centered_lines_truncates_a_line_wider_than_the_terminal() {
        let lines = ["a very long line that will not fit".to_string()];

        let placed = centered_lines(&lines, 10, 1);

        let (col, row, text) = &placed[0];
        assert_eq!(*col, 0);
        assert_eq!(*row, 0);
        assert_eq!(text.width(), 10);
        assert!(text.ends_with('\u{2026}'));
    }

    #[test]
    fn centered_lines_degrades_gracefully_when_the_terminal_is_smaller_than_the_block() {
        let lines = ["one".to_string(), "two".to_string(), "three".to_string()];

        // Only 1 row for a 3-line block: must not underflow/panic.
        let placed = centered_lines(&lines, 20, 1);

        assert_eq!(placed[0].1, 0);
        assert_eq!(placed[1].1, 1);
        assert_eq!(placed[2].1, 2);
    }

    #[test]
    fn windowed_view_shows_the_tail_when_the_cursor_is_at_the_end() {
        let (visible, cursor_col) = windowed_view("hello world", 11, 5);
        assert_eq!(visible, "world");
        // One past the last visible column; the caller clamps this into the
        // box, same as any other cursor position.
        assert_eq!(cursor_col, 5);
    }

    #[test]
    fn windowed_view_shows_the_head_when_the_cursor_is_at_the_start() {
        let (visible, cursor_col) = windowed_view("hello world", 0, 5);
        assert_eq!(visible, "hello");
        assert_eq!(cursor_col, 0);
    }

    #[test]
    fn windowed_view_keeps_the_cursor_visible_when_editing_mid_string() {
        // cursor at char-index 8 ("hello wo|rld"), a 6-column window.
        let (visible, cursor_col) = windowed_view("hello world", 8, 6);
        assert_eq!(visible, "lo wor");
        assert_eq!(cursor_col, 5);
    }

    #[test]
    fn windowed_view_never_splits_a_full_width_character() {
        let (visible, cursor_col) = windowed_view(&"あ".repeat(3), 3, 3);
        assert_eq!(visible, "あ");
        assert_eq!(cursor_col, 3);
    }

    #[test]
    fn windowed_view_cursor_column_accounts_for_a_full_width_character_before_it() {
        // "aあb": cursor after char-index 2 (past 'a' and 'あ'). Width-wise
        // that's column 3 (1 + 2), not char-count 2 — this is what a
        // width-aware cursor column must get right that a naive
        // `chars().count()` one would not.
        let (visible, cursor_col) = windowed_view("aあb", 2, 10);
        assert_eq!(visible, "aあb");
        assert_eq!(cursor_col, 3);
    }

    /// Reproduces the actual dogfooding scenario, not a contrived one: the
    /// archive-confirm modal's prompt echoes the very session it's
    /// archiving, so whenever that session's title is a long, full-width
    /// string, the *background* list row behind the modal shares that exact
    /// same text. `render_modal` briefly `Clear`ed the whole frame to guard
    /// against this, but a visible background around a modal is the point of
    /// an overlay, not a bug — the actual defect was the untruncated title
    /// *inside* the box, which `truncate_to_width` already handles. This
    /// test now asserts the box's own border and truncated content survive,
    /// without asserting anything about the margin (which may legitimately
    /// still show the background row).
    #[test]
    fn a_long_full_width_session_title_stays_truncated_inside_the_modal_box() {
        let long_title = "あ".repeat(60);
        let mut app = App::new(vec![row("a", &long_title, "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_confirm_archive_modal();

        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();

        let area = modal_area(Rect::new(0, 0, 40, 15));
        let title_row = area.y + 1;
        let right_border_x = area.x + area.width - 1;
        assert_eq!(
            buf.cell((right_border_x, title_row)).unwrap().symbol(),
            "\u{2502}",
            "the box's own right border must survive"
        );
        let row_text: String = (area.x..=right_border_x)
            .map(|x| buf.cell((x, title_row)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row_text.contains('\u{2026}'),
            "long content must be truncated with a visible ellipsis inside the box:\n{row_text}"
        );
    }

    #[test]
    fn a_long_title_in_the_archive_modal_is_truncated_with_an_ellipsis() {
        let long_title = "あ".repeat(60);
        let area = Rect::new(2, 2, 20, 6);
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| render_confirm_archive_modal(frame, &long_title, area))
            .unwrap();
        let buf = terminal.backend().buffer();

        let title_row = area.y + 1;
        let right_border_x = area.x + area.width - 1;
        assert_eq!(
            buf.cell((right_border_x, title_row)).unwrap().symbol(),
            "\u{2502}",
            "the block's own right border must survive"
        );
        assert_eq!(
            buf.cell((right_border_x - 1, title_row)).unwrap().symbol(),
            " ",
            "the 1-column right padding must stay blank"
        );
        let row_text: String = (area.x..=right_border_x)
            .map(|x| buf.cell((x, title_row)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row_text.contains('\u{2026}'),
            "long content should end in a visible ellipsis, not a silent cutoff:\n{row_text}"
        );
    }

    #[test]
    fn archive_modal_content_has_one_column_of_padding_inside_the_border() {
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_confirm_archive_modal();

        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buf = terminal.backend().buffer();

        // Left border at x=2 (same math as above); content used to start
        // right at x=3, flush against it. It must now start at x=4, leaving
        // x=3 blank.
        assert_eq!(buf.cell((2, 3)).unwrap().symbol(), "\u{2502}");
        assert_eq!(
            buf.cell((3, 3)).unwrap().symbol(),
            " ",
            "modal content must not be flush against the left border"
        );
        assert_eq!(buf.cell((4, 3)).unwrap().symbol(), "A");
    }

    // --- dogfooding fixes: headless arrow-key CSI leaks -------------------

    #[test]
    fn arrow_key_for_recognizes_all_four_directions_headless_and_esc_headed() {
        assert_eq!(arrow_key_for(&['[', 'A']), Some(KeyCode::Up));
        assert_eq!(arrow_key_for(&['[', 'B']), Some(KeyCode::Down));
        assert_eq!(arrow_key_for(&['[', 'C']), Some(KeyCode::Right));
        assert_eq!(arrow_key_for(&['[', 'D']), Some(KeyCode::Left));
        assert_eq!(arrow_key_for(&['\u{1b}', '[', 'A']), Some(KeyCode::Up));
        assert_eq!(arrow_key_for(&['\u{1b}', '[', 'D']), Some(KeyCode::Left));
    }

    #[test]
    fn arrow_key_for_rejects_shapes_that_are_not_a_bare_arrow_key() {
        assert_eq!(arrow_key_for(&['[', 'x']), None);
        assert_eq!(arrow_key_for(&['[', '<']), None); // SGR mouse lead-in
        assert_eq!(arrow_key_for(&['[']), None);
        assert_eq!(arrow_key_for(&[]), None);
    }

    #[test]
    fn a_headless_leaked_up_arrow_moves_selection_instead_of_replaying_garbage() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "A", "", Activity::Alive),
            row("b", "B", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        app.select_next();
        assert_eq!(app.selected_row().unwrap().id, "b");

        let list_area = Rect::new(0, 4, 60, 3);
        let outcome = swallow_one_sequence(
            &mut app,
            &ctx,
            list_area,
            vec!['[', 'A'],
            sgr::parse_headless_prefix,
            HEADLESS_GRACE,
        )
        .unwrap();

        assert!(matches!(outcome, EscapeOutcome::Swallowed));
        assert_eq!(app.selected_row().unwrap().id, "a");
        assert_eq!(app.query(), "", "must not have been typed as garbage");
    }

    #[test]
    fn an_esc_headed_leaked_down_arrow_does_not_close_an_open_modal() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_group_join_modal();

        let list_area = Rect::new(0, 4, 60, 3);
        let outcome = swallow_one_sequence(
            &mut app,
            &ctx,
            list_area,
            vec!['\u{1b}', '[', 'B'],
            sgr::parse_prefix,
            ESCAPE_GRACE,
        )
        .unwrap();

        assert!(matches!(outcome, EscapeOutcome::Swallowed));
        assert!(
            app.modal().is_some(),
            "the leaked arrow's Esc byte must not have been replayed as a real Esc"
        );
    }

    /// Regression test for the "can't create a group" dogfooding report:
    /// what looked like an independent bug turned out to be entirely a
    /// side effect of the arrow-key leak above — before this fix, a leaked
    /// arrow key firing mid-typing would replay its leading `Esc` byte,
    /// silently closing the group-join modal and discarding the name the
    /// user had just typed.
    #[test]
    fn a_leaked_arrow_key_while_typing_a_new_group_name_does_not_corrupt_it_or_close_the_modal() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_group_join_modal();
        for c in "myteam".chars() {
            app.modal_push_char(c);
        }

        let list_area = Rect::new(0, 4, 60, 3);
        swallow_one_sequence(
            &mut app,
            &ctx,
            list_area,
            vec!['\u{1b}', '[', 'B'],
            sgr::parse_prefix,
            ESCAPE_GRACE,
        )
        .unwrap();

        let Some(Modal::GroupJoin(state)) = app.modal() else {
            panic!("modal must still be open after the leaked arrow key");
        };
        assert_eq!(state.input(), "myteam", "input must not be corrupted");

        match app.modal_group_join_target() {
            Some(GroupJoinTarget::New(name)) => assert_eq!(name, "myteam"),
            other => panic!("expected New(\"myteam\"), got {other:?}"),
        }
    }

    // --- dogfooding fixes: SGR-leaked click hitting the background list --

    #[test]
    fn apply_sgr_action_left_click_is_a_noop_while_a_modal_is_open() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![
            row("a", "Alpha", "", Activity::Alive),
            row("b", "Beta", "", Activity::Alive),
        ]);
        app.set_viewport_height(10);
        let selected_before = app.selected_row().unwrap().id.clone();
        app.open_new_session_modal();

        let list_area = Rect::new(0, 4, 60, 3);
        let event = sgr::SgrMouseEvent {
            button: 0,
            x: 1,
            y: 5,
            pressed: true,
        };
        apply_sgr_action(&mut app, &ctx, list_area, event);

        assert_eq!(
            app.selected_row().unwrap().id,
            selected_before,
            "a leaked SGR click must not select a background row while a modal is open"
        );
        assert!(app.modal().is_some());
    }

    // --- dogfooding fixes: a lingering status notification ---------------

    #[test]
    fn a_transient_status_message_clears_on_the_next_key_press() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::NONE, &ctx);
        assert!(
            app.status().is_some(),
            "pinning should post a status message"
        );

        handle_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE, &ctx);
        assert!(
            app.status().is_none(),
            "the notification must clear once the user does something else"
        );
    }
}
