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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use banto_core::config::OpenerMode;
use banto_core::model::{Activity, AgeBucket, SessionId};
use banto_core::opener::SystemCommandRunner;
use banto_core::status::{AgeThresholds, SysinfoProbe};
use banto_core::store::Store;
use banto_core::watch::{ChangeSource, Debouncer, NotifyChangeSource};

use crate::app::{App, ClickOutcome, Modal, Mode, NewSessionState, VisibleRow};
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

/// Everything the render loop needs beyond [`App`] itself: dependencies for
/// opening/focusing sessions and reloading rows from disk.
struct Context<'a> {
    claude_home: &'a Path,
    thresholds: &'a AgeThresholds,
    store: &'a Store,
    opener_mode: OpenerMode,
    /// Diagnostic input-event log, enabled via the `BANTO_INPUT_LOG` env var
    /// (its value is the file path). Records every raw crossterm event and
    /// every escape-resolution decision with a millisecond timestamp, for
    /// debugging input pipelines we cannot reproduce synthetically.
    input_log: std::cell::RefCell<Option<std::fs::File>>,
}

impl Context<'_> {
    /// Append one line to the diagnostic input log (no-op when disabled).
    fn log(&self, message: &str) {
        use std::io::Write as _;
        if let Some(file) = self.input_log.borrow_mut().as_mut() {
            let ms = std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(file, "{ms} {message}");
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
        input_log: std::cell::RefCell::new(open_input_log()),
    };
    ctx.log("=== banto TUI started ===");

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

/// Height of the always-visible summary panel below the list: one row for
/// its top border/title plus [`SUMMARY_CONTENT_LINES`] content rows.
const SUMMARY_HEIGHT: u16 = 1 + SUMMARY_CONTENT_LINES;
/// Content rows inside the summary panel: activity dot + title, cwd, preview.
const SUMMARY_CONTENT_LINES: u16 = 3;

/// Split an area into (search box, list, summary panel, status bar).
fn layout_areas(area: Rect) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(SUMMARY_HEIGHT),
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
                            // us, only its Release. This signature is safe
                            // to treat as a real Esc: in the working case,
                            // a real Esc press is dispatched through
                            // `resolve_escape`, which consumes the matching
                            // Release internally (see `swallow_one_sequence`)
                            // — so a bare Esc Release reaching here always
                            // means its press never arrived.
                            ctx.log(
                                "loop: bare Esc Release with no matching Press -> dispatching as Esc",
                            );
                            handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
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

        if watch.poll_ready(SystemTime::now()) {
            reload(app, ctx);
        }

        if app.should_quit() {
            return Ok(());
        }
    }
}

/// Translate a key press into an [`App`] action. Navigation and paging
/// behave the same in both modes; everything else — including Enter — is
/// mode-specific (see [`handle_normal_key`] / [`handle_search_key`]) because
/// letter keys mean different things: commands in Normal mode, query text
/// in Search mode.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers, ctx: &Context) {
    if mods.contains(KeyModifiers::CONTROL) {
        // Ctrl+C always quits; other Ctrl combos are ignored for now.
        if code == KeyCode::Char('c') {
            app.request_quit();
        }
        return;
    }
    // A modal takes over all key handling while it's open — including
    // Up/Down, which mean "move the candidate selection" there rather than
    // "move the list selection".
    if app.modal().is_some() {
        handle_modal_key(app, code, ctx);
        return;
    }
    match code {
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
        KeyCode::Enter => activate(app, ctx),
        KeyCode::Char('/') => app.enter_search(),
        KeyCode::Char('n') => app.open_new_session_modal(),
        KeyCode::Char('p') => toggle_pin(app, ctx),
        KeyCode::Char('a') => toggle_agent_filter(app),
        KeyCode::Char('q') | KeyCode::Esc => app.request_quit(),
        _ => {}
    }
}

/// Keys while a modal is open: typed characters build its text input,
/// Up/Down move its candidate selection, Backspace edits the input, Enter
/// confirms (see [`confirm_modal`]), Esc cancels without acting. Only the
/// new-session modal exists so far, but none of this is specific to it — a
/// future modal (e.g. group-join) reuses this same shape.
fn handle_modal_key(app: &mut App, code: KeyCode, ctx: &Context) {
    match code {
        KeyCode::Esc => app.close_modal(),
        KeyCode::Up => app.modal_select_prev(),
        KeyCode::Down => app.modal_select_next(),
        KeyCode::Backspace => app.modal_backspace(),
        KeyCode::Enter => confirm_modal(app, ctx),
        KeyCode::Char(c) => app.modal_push_char(c),
        _ => {}
    }
}

/// Confirm the open modal. For the new-session modal: resolve the target cwd
/// (the highlighted candidate, or the raw typed path — see
/// [`App::modal_new_session_target`]), launch a fresh `claude` there, post
/// the outcome as a status message, and close the modal either way (the
/// message reports success or failure, same as [`activate`]'s resume path).
/// A modal with nothing to confirm yet (empty input, no candidates) is left
/// open — Enter does nothing, matching how Enter on an empty list does
/// nothing in [`activate`].
fn confirm_modal(app: &mut App, ctx: &Context) {
    let Some(cwd) = app.modal_new_session_target() else {
        return;
    };

    let backend = opener::resolve_backend(ctx.opener_mode, |key| std::env::var(key).ok());
    let anchor = std::env::var("TMUX_PANE").ok();
    let outcome = opener::open_new_session(backend, &cwd, SystemCommandRunner, anchor.as_deref());

    let message = match outcome {
        Ok(OpenOutcome::Opened) => format!("launched a new session in {}", cwd.display()),
        Ok(OpenOutcome::NoBackendDetected) => {
            "no terminal backend detected (run inside psmux/Windows Terminal, \
             or set `opener` in config.toml)"
                .to_string()
        }
        // `open_new_session` never focuses or refuses an existing pane —
        // there's no pre-existing session for a fresh launch to key off of.
        Ok(OpenOutcome::Focused | OpenOutcome::AlreadyOpenCannotFocus) => unreachable!(),
        Err(err) => format!("failed to launch a new session in {}: {err}", cwd.display()),
    };
    app.set_status(message);
    app.close_modal();
}

/// Search-mode keys: characters type into the query (`j`/`k` included —
/// they're ordinary query text here, not movement). Enter confirms the
/// search (back to Normal, keeping the query/filter, so the just-filtered
/// list can be navigated); Esc cancels it (clears the query, back to
/// Normal).
fn handle_search_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Backspace => app.backspace(),
        KeyCode::Enter => app.confirm_search(),
        KeyCode::Esc => app.exit_search(),
        KeyCode::Char(c) => app.push_char(c),
        _ => {}
    }
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
        handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
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
                    // See the identical case in `event_loop`: a bare Esc
                    // Release reaching here means its press was dropped
                    // upstream during motion — dispatch it as the real Esc.
                    ctx.log(
                        "esc: drain saw bare Esc Release with no matching Press -> dispatching as Esc",
                    );
                    handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
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
/// actually observed. All three codepoints are already inert as query text
/// — `App::push_char` drops control characters outright — so recovering the
/// intended key here costs nothing and fixes real, silent breakage.
fn normalize_key_code(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') => KeyCode::Backspace,
        KeyCode::Char('\u{1b}') => KeyCode::Esc,
        other => other,
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
        handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
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
fn apply_sgr_action(app: &mut App, ctx: &Context, list_area: Rect, event: sgr::SgrMouseEvent) {
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
            handle_key(app, KeyCode::Esc, KeyModifiers::NONE, ctx);
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
    // Anchor psmux splits on banto's own pane so the resume pane lands
    // next to banto, not in whatever window the client has focused.
    let anchor = std::env::var("TMUX_PANE").ok();
    let outcome = opener::open_session(
        ctx.store,
        &SysinfoProbe,
        backend,
        &session,
        SystemCommandRunner,
        anchor.as_deref(),
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

/// Render the always-visible summary panel below the list: the selected
/// session's activity dot + title, cwd, and preview excerpt. A top border is
/// the only visual separation from the list, to keep this compact. The
/// preview line renders blank until `SessionRow::preview` extraction lands
/// (currently always `None`) — the layout doesn't change either way.
fn render_summary(frame: &mut Frame, app: &App, area: Rect) {
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
    let cwd_line = Line::from(Span::styled(
        row.cwd_display(),
        Style::default().fg(Color::DarkGray),
    ));
    let preview_line = Line::from(Span::styled(
        row.preview.as_deref().unwrap_or_default(),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(vec![title_line, cwd_line, preview_line]),
        inner,
    );
}

/// Render the bottom status bar: key hints (or a transient message) on the
/// left, match count right-aligned. Rendered as two separate widgets (rather
/// than one line) so the count stays visible even when the hints are too long
/// for a narrow terminal and get truncated.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    const NORMAL_HINTS: &str = "j/k\u{2191}\u{2193} move  PgUp/PgDn page  Enter open  / search  \
                                n new  p pin  a agents  q/Esc quit";
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
const MODAL_MIN_MARGIN: u16 = 1;
/// A modal never grows wider/taller than this, so a full-width pane still
/// gets a comfortable margin around it instead of an edge-to-edge dialog.
const MODAL_MAX_WIDTH: u16 = 70;
const MODAL_MAX_HEIGHT: u16 = 20;

/// Center a modal box within `area`: margin shrinks toward
/// [`MODAL_MIN_MARGIN`] in a narrow/tall pane (the modal fills almost the
/// whole width) and grows in a full-width pane, since the modal caps out at
/// [`MODAL_MAX_WIDTH`]/[`MODAL_MAX_HEIGHT`] instead of stretching edge to edge.
fn modal_area(area: Rect) -> Rect {
    let width = area
        .width
        .saturating_sub(MODAL_MIN_MARGIN * 2)
        .clamp(1, MODAL_MAX_WIDTH);
    let height = area
        .height
        .saturating_sub(MODAL_MIN_MARGIN * 2)
        .clamp(1, MODAL_MAX_HEIGHT);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

/// Render whichever modal is open as a centered overlay on top of the rest
/// of the UI: [`Clear`] blanks the area first so the list/summary panel
/// behind it doesn't bleed through.
fn render_modal(frame: &mut Frame, modal: &Modal, area: Rect) {
    let area = modal_area(area);
    frame.render_widget(Clear, area);
    match modal {
        Modal::NewSession(state) => render_new_session_modal(frame, state, area),
    }
}

/// Render the `n` new-session dialog: a one-line cwd input (with a blinking
/// cursor, same convention as the search box) above a fuzzy-filtered list of
/// previously seen cwds to pick from instead of typing a full path.
fn render_new_session_modal(frame: &mut Frame, state: &NewSessionState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" New Session \u{2014} cwd ")
        .title_bottom(" Enter launch  Esc cancel ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [input_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    frame.render_widget(Paragraph::new(state.input()), input_area);
    if input_area.width > 0 {
        let input_cols = state.input().chars().count() as u16;
        let cursor_x = (input_area.x + input_cols).min(input_area.x + input_area.width - 1);
        frame.set_cursor_position(Position::new(cursor_x, input_area.y));
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
        .map(|candidate| ListItem::new((*candidate).to_string()))
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
    fn test_context<'a>(store: &'a Store, thresholds: &'a AgeThresholds) -> Context<'a> {
        Context {
            claude_home: Path::new("."),
            thresholds,
            store,
            opener_mode: OpenerMode::Auto,
            input_log: std::cell::RefCell::new(None),
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
        // uses, so check it wider (see `search_mode_hint_differs_...`).
        let wide_text = draw_with_width(&app, 110);
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
        // the narrow 60-col terminal `draw` uses, so check it wider.
        let wide_text = draw_with_width(&app, 130);
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
    fn q_quits_in_normal_mode_but_is_query_input_in_search_mode() {
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Esc, KeyModifiers::NONE, &ctx);
        assert!(app.should_quit());
    }

    #[test]
    fn esc_in_search_mode_clears_the_query_and_returns_to_normal_without_quitting() {
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "A", "", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL, &ctx);

        assert!(app.should_quit());
    }

    #[test]
    fn n_opens_the_new_session_modal_from_normal_mode() {
        let store = Store::open_in_memory().unwrap();
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Char('n'), KeyModifiers::NONE, &ctx);

        assert!(app.modal().is_some());
    }

    #[test]
    fn n_is_query_text_in_search_mode_not_the_new_session_shortcut() {
        let store = Store::open_in_memory().unwrap();
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
    fn esc_closes_the_open_modal_without_quitting_or_reaching_the_recognizer() {
        let store = Store::open_in_memory().unwrap();
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
    fn typing_and_arrow_keys_drive_the_open_modal_instead_of_the_background_list() {
        let store = Store::open_in_memory().unwrap();
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
    fn modal_area_shrinks_margin_in_a_narrow_pane_and_caps_width_in_a_wide_one() {
        // Narrow: minimal margin, modal fills almost the whole width.
        let narrow = modal_area(Rect::new(0, 0, 30, 20));
        assert_eq!(narrow.width, 28); // 30 - 2*MODAL_MIN_MARGIN
        assert_eq!(narrow.x, 1);

        // Wide: capped at MODAL_MAX_WIDTH, leaving a large margin.
        let wide = modal_area(Rect::new(0, 0, 200, 50));
        assert_eq!(wide.width, 70); // MODAL_MAX_WIDTH
        assert_eq!(wide.x, 65); // centered: (200 - 70) / 2
    }

    #[test]
    fn render_summary_shows_the_selected_session_and_a_placeholder_when_empty() {
        let mut app = App::new(vec![row("a", "Fix login", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(10);

        let text = draw(&app);
        assert!(text.contains("Details"), "panel border missing:\n{text}");
        assert!(text.contains("Fix login"), "title missing:\n{text}");
        assert!(text.contains("/work/alpha"), "cwd missing:\n{text}");

        let empty_app = App::new(Vec::new());
        let empty_text = draw(&empty_app);
        assert!(
            empty_text.contains("No session selected."),
            "placeholder missing:\n{empty_text}"
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
    }
}
