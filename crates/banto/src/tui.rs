//! ratatui render loop for the chōba (帳場, formerly the "classic" list) —
//! terminal setup/teardown, event handling and drawing.
//!
//! This is a thin shell over [`crate::app::App`]; all list logic lives there.
//! The terminal is restored both on normal exit and on panic (via a panic
//! hook), and mouse capture is enabled for wheel/click support. All code here
//! is cross-platform — crossterm handles the Windows specifics.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{self, Stdout};
use std::path::PathBuf;
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
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use banto_core::app::{
    App, ClickOutcome, GroupJoinTarget, Modal, Mode, NewSessionPlacement, OpenAction,
};
use banto_core::config::{AgentBinaries, OpenerMode, ResolvedAgents};
use banto_core::model::{AgentKind, SessionId, SessionMeta, SessionToOpen};
use banto_core::status::AgeThresholds;
use banto_io::claude_home::ClaudeHome;
use banto_io::codex_home::CodexHome;
use banto_io::codex_liveness::SysinfoStartTime;
use banto_io::lineage::resolve_lineage;
use banto_io::opener::SystemCommandRunner;
use banto_io::process::{ProcessRunner, SystemProcessRunner};
use banto_io::status::{SysinfoProbe, read_live_sessions};
use banto_io::store::Store;
use banto_io::watch::{ChangeSource, Debouncer, NotifyChangeSource};
use banto_tui::render_modal::{render_modal, windowed_view};
use banto_tui::text::truncate_to_width;
use banto_tui::view;

use crate::opener::{self, OpenOutcome};
use crate::session;
use crate::sgr::{self, SgrParse};

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
/// still resolving in far less time than a human notices as lag. That
/// "negligible risk" analysis covers the full SGR-mouse grammar only, not
/// the shorter headless-arrow shape `[`+`A`/`B`/`C`/`D` (see
/// `arrow_key_for`), which two-key genuine typing (`[Active`, `[Draft]`, a
/// `[A-Z]` regex class, ...) reaches easily inside this window — the reason
/// the whole headless path this grace period gates is itself gated to
/// platforms where it applies (see [`Context::headless_leak_recovery`])
/// rather than active unconditionally.
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
    /// Owned rather than borrowed like the rest of this struct's fields:
    /// `ClaudeHome` is a cheap path wrapper, and owning it here means the
    /// dozens of `test_context`/`test_context_with_headless_recovery` call
    /// sites below don't each need a place of their own to hold one just to
    /// satisfy a borrow.
    claude_home: ClaudeHome,
    /// `None` when Codex home resolution failed entirely (no home
    /// directory) or `$CODEX_HOME`/`~/.codex` simply doesn't exist yet —
    /// either way, no Codex sessions, not an error.
    codex_home: Option<CodexHome>,
    /// `[agent_binaries]` from config.toml — see `opener::agent_binary`.
    agent_binaries: AgentBinaries,
    /// `Config.agents`, resolved — which providers [`session::discover_all`]
    /// is allowed to run at all. Also threaded into `App::with_enabled_agents`
    /// so the empty-list placeholder can say why, when it's this and not a
    /// genuinely empty machine.
    enabled_agents: BTreeSet<AgentKind>,
    thresholds: &'a AgeThresholds,
    /// `RefCell`-wrapped — see `main`'s construction site for why.
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
    /// Session ids whose lineage scan found no parent this run (see
    /// [`superseded_from_metas`]) — kept in memory only, across every reload for
    /// the lifetime of this process, so a permanently-unresolvable
    /// continuation isn't re-scanned (tens of MB) every reload; a fresh
    /// banto run starts with an empty set and retries it.
    superseded_failed: RefCell<HashSet<SessionId>>,
    /// Whether the headless (`ESC`-less) leaked-sequence recovery path (see
    /// [`is_headless_bracket`]/[`resolve_headless_bracket`]) is active for
    /// this run. Only ConPTY is known to drop the leading `ESC` byte before
    /// a leaked SGR/arrow report reaches us; on every other platform a bare,
    /// unmodified `[` is just the character it looks like, so this stays
    /// `false` there — retiring both the false-positive risk (two ordinary
    /// keystrokes, `[` then `A`/`B`/`C`/`D`, misread as an arrow key) and
    /// the [`HEADLESS_GRACE`] stall on every lone `[`. The `ESC`-prefixed
    /// path ([`resolve_escape`]) is unaffected by this flag and stays active
    /// everywhere, since a genuine multiplexer split can still deliver a
    /// leaked sequence with its `ESC` intact regardless of host OS. Read
    /// once at construction via `cfg!(windows)` (see [`run`]) rather than
    /// re-derived ad hoc at each decision point — the house pattern for
    /// injecting environment as a plain input (see
    /// `banto_io::opener::detect_backend`'s `env` parameter).
    headless_leak_recovery: bool,
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
pub(crate) struct LiveWatch {
    source: Option<NotifyChangeSource>,
    debouncer: Debouncer,
}

impl LiveWatch {
    pub(crate) fn new(claude_home: &ClaudeHome, codex_home: Option<&CodexHome>) -> Self {
        Self {
            source: NotifyChangeSource::new(claude_home, codex_home).ok(),
            debouncer: Debouncer::new(DEBOUNCE_QUIET),
        }
    }

    /// Drain any pending filesystem changes and report whether their quiet
    /// period has elapsed as of `now`, i.e. whether a reload is due.
    pub(crate) fn poll_ready(&mut self, now: SystemTime) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        for change in source.drain() {
            self.debouncer.record(change.root, change.at);
        }
        !self.debouncer.poll(now).is_empty()
    }
}

pub fn run(
    claude_home: &ClaudeHome,
    codex_home: Option<CodexHome>,
    agent_binaries: AgentBinaries,
    thresholds: &AgeThresholds,
    opener_mode: OpenerMode,
    store: &RefCell<Store>,
    resolved_agents: ResolvedAgents,
) -> Result<()> {
    // Computed before `resolved_agents.enabled` moves below.
    let agents_notice = session::agents_ignored_notice(&resolved_agents);
    let enabled_agents = resolved_agents.enabled;
    let metas = session::discover_all(claude_home, codex_home.as_ref(), &enabled_agents)?;
    let superseded_failed = RefCell::new(HashSet::new());
    let (rows, pinned, groups, session_groups, hidden, directors, superseded) = {
        let store = store.borrow();
        let superseded = superseded_from_metas(&metas, &store, &superseded_failed);
        let rows = session::rows_from_metas(metas, claude_home, thresholds);
        let rows = exclude_archived(rows, &store);
        let pinned = load_pinned(&store);
        let groups = load_groups(&store);
        let session_groups = load_session_groups(&store, &groups);
        let hidden = load_hidden_worker_ids(&store);
        let directors = load_directors(&store);
        (
            rows,
            pinned,
            groups,
            session_groups,
            hidden,
            directors,
            superseded,
        )
    };
    let mut app = App::new(rows)
        .with_pinned(pinned)
        .with_groups(groups, session_groups)
        .with_hidden_worker_ids(hidden)
        .with_directors(directors)
        .with_superseded(superseded)
        .with_enabled_agents(enabled_agents.clone());
    // A one-time startup notice, not part of `Context`/reload — see
    // `session::agents_ignored_notice`'s doc.
    if let Some(notice) = agents_notice {
        app.set_status(notice, Instant::now());
    }
    let ctx = Context {
        claude_home: claude_home.clone(),
        codex_home,
        agent_binaries,
        enabled_agents,
        thresholds,
        store,
        opener_mode,
        input_log: std::cell::RefCell::new(open_input_log()),
        last_genuine_esc: RefCell::new(None),
        pending_inplace: RefCell::new(None),
        superseded_failed,
        headless_leak_recovery: cfg!(windows),
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

/// Drop archived sessions from `rows` — the union of two independent facts:
/// banto's own archive (soft-hide via `d` — see
/// `App::open_confirm_archive_modal`/`confirm_modal`), and, for Codex,
/// `SessionRow::source_archived` (`threads.archived`, set by `codex
/// archive`). Two facts, not one: banto's archive is per-session-id and
/// product-neutral, entirely independent of whatever the session's own
/// product thinks; `source_archived` is the reverse — a fact banto can only
/// ever read, since `~/.codex` stays read-only to it (never a write, so
/// never an unarchive from banto's side either). Either one hides the row;
/// neither is cleared by the other disagreeing. Concretely: unarchiving in
/// Codex (`codex unarchive`) clears `source_archived` on the next reload,
/// and the row reappears *unless* the operator separately archived that
/// same session in banto too, in which case it stays hidden until banto's
/// own archive is lifted — a real store write, unaffected by anything
/// Codex reports. A store read failure is tolerated: nothing gets excluded
/// rather than blocking the TUI.
pub(crate) fn exclude_archived(
    rows: Vec<session::SessionRow>,
    store: &Store,
) -> Vec<session::SessionRow> {
    let archived: HashSet<String> = store
        .archived_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect();
    rows.into_iter()
        .filter(|row| !row.source_archived && !archived.contains(&row.id))
        .collect()
}

/// Load every known group, alphabetical by name. Tolerant: a read failure
/// just means no groups are known yet, rather than blocking the TUI.
pub(crate) fn load_groups(store: &Store) -> Vec<(i64, String)> {
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
pub(crate) fn load_session_groups(store: &Store, groups: &[(i64, String)]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for &(group_id, _) in groups {
        for session_id in store.group_members(group_id).unwrap_or_default() {
            map.insert(session_id.0, group_id);
        }
    }
    map
}

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
pub(crate) fn load_pinned(store: &Store) -> HashSet<String> {
    store
        .pinned_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Load every brigade Worker's Claude session id, across every brigade, that
/// has been assigned one so far — [`App`] hides these from the list (see
/// `App::hidden`). Tolerant: a read failure just means nothing is hidden yet,
/// rather than blocking the TUI from starting.
pub(crate) fn load_hidden_worker_ids(store: &Store) -> HashSet<String> {
    store
        .brigade_worker_session_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Load every brigade Director's Claude session id, across every brigade —
/// [`App`] marks these in the list/summary (see `App::directors`). Tolerant:
/// a read failure just means no marker shows yet, rather than blocking the
/// TUI from starting.
pub(crate) fn load_directors(store: &Store) -> HashSet<String> {
    store
        .brigade_director_session_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Spend this reload's lineage-resolution budget against already-discovered
/// `metas`, then return every session id with a known auto-compaction
/// continuation — [`App`] hides these (see `App::superseded`). Takes
/// `metas` rather than discovering its own: `SessionMeta`'s
/// `continuation_of_uuid` doesn't survive into [`session::load_rows`]'s
/// `SessionRow`s, but `banto_io::lineage::resolve_lineage` already wants
/// exactly `&[SessionMeta]`, so every call site — which already needs a
/// discover() pass for the rows anyway — can hand the same one here instead
/// of paying for a second. Tolerant: a lineage-resolution failure just means
/// no progress this pass, not a blocked TUI.
pub(crate) fn superseded_from_metas(
    metas: &[SessionMeta],
    store: &Store,
    failed: &RefCell<HashSet<SessionId>>,
) -> HashSet<String> {
    let mut failed = failed.borrow_mut();
    let _ = resolve_lineage(store, metas, &mut failed);
    store
        .lineage_parent_ids()
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.0)
        .collect()
}

/// Leave the alternate screen, disable mouse capture and raw mode. Used on
/// final quit ([`run`]) — the main screen/scrollback underneath is what the
/// user sees again, so this is the one place that's allowed to touch it.
fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

/// Disable raw mode and mouse capture but deliberately stay on the
/// alternate screen — the in-place hand-off's partial teardown (see
/// [`run_pending_inplace`]), as opposed to [`restore_terminal`]'s full one.
/// The child gets a normal (cooked, non-mouse-capturing) terminal either
/// way; the difference is *which* screen it inherits. Staying on the alt
/// screen means anything drawn here (the loading splash) and anything the
/// child itself draws both stay off the user's real main screen/scrollback,
/// which is never touched until banto's own final [`restore_terminal`] —
/// this is what makes the whole hand-off non-destructive.
fn pause_for_child() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), DisableMouseCapture)?;
    Ok(())
}

/// Run an in-place launch to completion, handing banto's pane to it without
/// ever touching the user's real main screen/scrollback: disable raw
/// mode/mouse capture but stay on the alternate screen ([`pause_for_child`]),
/// clear *that* screen and draw a centered loading splash on it
/// ([`draw_splash`]), block on the child with inherited stdio (`claude`
/// paints its own UI over the same alt screen, and may leave it on exit —
/// either way is fine), then unconditionally re-establish banto's TUI
/// ([`setup_terminal`], which enters the alt screen regardless of whether
/// the child already left it) and reload rows (the just-used session's
/// mtime/activity changed). This is the thin, untested-by-design shell
/// around [`opener::decide_inplace_resume`]'s pure decision (see
/// [`activate`]) — the standard "shell out to a full-screen program and
/// come back" pattern; crossterm handles the re-init.
///
/// An earlier version used a full [`restore_terminal`] here (leaving the alt
/// screen entirely) with a plain `println!` for the loading message — safe,
/// but visually unceremonious. A version before *that* cleared and centered
/// on the MAIN screen, which was destructive: it wiped the user's own
/// scrollback, and if `claude` exited before painting anything, the message
/// was left stranded even after banto itself had quit. Doing the same
/// clear+center on the alt screen instead keeps the effect (a clean,
/// legible loading splash) without either downside — the main screen is
/// simply never touched by any of this.
///
/// `*terminal` is replaced with a freshly re-initialized one rather than
/// reused, mirroring [`run`]'s own one-time `setup_terminal` — there is no
/// cheaper way to resume drawing after ceding raw mode/mouse capture to the
/// child. If re-initializing fails, that error propagates (matching
/// [`run`]'s "always restore, but a failure is still an error" discipline)
/// rather than leaving the app silently stuck outside raw mode.
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
    pause_for_child()?;
    draw_splash(&pending.startup_message);
    let result = SystemProcessRunner.run_in(&pending.argv, &pending.cwd);
    *terminal = setup_terminal()?;
    ctx.log(&format!("run_pending_inplace child result={result:?}"));

    let message = match result {
        Ok(_) => "returned from session".to_string(),
        Err(err) => format!("failed to run {:?}: {err}", pending.argv),
    };
    app.set_status(message, Instant::now());
    reload(app, ctx);
    Ok(())
}

/// Clear the (still-active) alternate screen and draw `message` centered on
/// it — the loading splash shown while `claude` is starting (see
/// [`run_pending_inplace`]). Safe to clear here — unlike the main
/// screen/scrollback, the alt screen is transient scratch space the user
/// never otherwise sees, so nothing of theirs is lost. `claude` paints over
/// it once it starts; if the child exits first, [`run_pending_inplace`]'s
/// following [`setup_terminal`] repaints over it too.
///
/// Best-effort throughout (`crossterm::terminal::size()` failing — e.g.
/// stdout isn't actually a terminal — falls back to a conservative 80x24;
/// individual draw calls' errors are swallowed): this is a cosmetic step
/// between the partial teardown and spawning the child, and must never be
/// what blocks an otherwise-working resume.
fn draw_splash(message: &str) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    );
    let lines = [message.to_string()];
    for (col, row, line) in centered_lines(&lines, cols, rows) {
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
/// [`draw_splash`], its only caller. Degrades gracefully rather than
/// panicking/underflowing when the terminal is smaller than the block:
/// `saturating_sub` clamps both axes to 0 (top-left) instead of going
/// negative.
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
    let mut watch = LiveWatch::new(&ctx.claude_home, ctx.codex_home.as_ref());

    loop {
        // Compute the layout up front so the viewport height and mouse
        // hit-testing agree with what we are about to render.
        let size = terminal.size()?;
        let [_, list_area, _, _] = layout_areas(Rect::new(0, 0, size.width, size.height));
        app.set_viewport_height(list_area.height as usize);

        terminal.draw(|frame| render(frame, app, SystemTime::now()))?;

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
                            // A bare Esc Release here is ambiguous: a genuine
                            // dropped press (mouse motion can drop it
                            // upstream) or the trailing Release of an Esc
                            // already dispatched by `resolve_escape`'s
                            // timeout — see `ESC_RELEASE_SUPPRESS_WINDOW`'s
                            // doc for why both look identical at this point.
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
                    } else if headless_bracket_recovery_active(ctx, code, key.modifiers) {
                        // See `Context::headless_leak_recovery`'s doc for why
                        // this platform-gated path exists at all.
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

fn toggle_grouped_view(app: &mut App) {
    let grouped = app.toggle_grouped_view();
    app.set_status(
        if grouped {
            "grouped view (Pinned / groups / Ungrouped)"
        } else {
            "flat view"
        }
        .to_string(),
        Instant::now(),
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
/// `ConfirmDisband`/`ConfirmKill` are the emporium's own modals — the chōba
/// never opens either, so confirming them here is a no-op (Esc still
/// closes it, via the shared `close_modal` in [`handle_modal_key`]).
fn confirm_modal(app: &mut App, ctx: &Context) {
    match app.modal() {
        Some(Modal::NewSession(_)) => confirm_new_session_modal(app, ctx),
        Some(Modal::ConfirmArchive { .. }) => confirm_archive_modal(app, ctx),
        Some(Modal::GroupJoin(_)) => confirm_group_join_modal(app, ctx),
        Some(Modal::ConfirmDisband { .. }) | Some(Modal::ConfirmKill { .. }) | None => {}
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
                // Always Claude, deliberately ignoring `state`'s own agent
                // choice: the chōba is feature-frozen (out of scope, not an
                // oversight — see `opener::new_session_wrap_argv`'s doc for
                // the split placement's identical reasoning), and its key
                // dispatch never binds anything to
                // `App::modal_toggle_new_session_agent`, so `state.agent()`
                // can never actually be anything but the default here.
                argv: opener::inplace_argv(AgentKind::ClaudeCode, None, &cwd, &ctx.agent_binaries),
                startup_message: opener::new_session_startup_message(&cwd),
                cwd,
            });
        }
        NewSessionPlacement::Split => {
            let backend = opener::resolve_backend(
                ctx.opener_mode,
                |key| std::env::var(key).ok(),
                cfg!(windows),
            );
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
                &ctx.agent_binaries,
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
            app.set_status(message, Instant::now());
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
    app.set_status(message, Instant::now());
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
                app.set_status(
                    format!("failed to create group \"{name}\": {err}"),
                    Instant::now(),
                );
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
    app.set_status(message, Instant::now());
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
        vec![('\u{1b}', KeyModifiers::NONE)],
        sgr::parse_prefix,
        ESCAPE_GRACE,
    )? {
        EscapeOutcome::Done => Ok(()),
        EscapeOutcome::Swallowed => drain_more(app, ctx, list_area),
    }
}

/// Resolve a `Char('[')` key event with no modifiers by buffering it as a
/// possible SGR mouse sequence with its leading `ESC` already missing. Only
/// called when [`headless_bracket_recovery_active`] has already said this
/// platform needs it — see [`Context::headless_leak_recovery`]'s doc for why
/// the leading `ESC` goes missing at all. Unlike [`resolve_escape`] there is
/// no ambiguity to wait out at entry: an ordinary typed `[` looks identical
/// to the start of a leaked sequence at this first byte either way, so
/// buffering always begins — [`HEADLESS_GRACE`] is what keeps genuine typing
/// from being mistaken for one (see [`swallow_one_sequence`]'s
/// `NotSgr`/timeout path).
fn resolve_headless_bracket(app: &mut App, ctx: &Context, list_area: Rect) -> Result<()> {
    match swallow_one_sequence(
        app,
        ctx,
        list_area,
        vec![('[', KeyModifiers::NONE)],
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
                    // Same ambiguous bare Esc Release as `event_loop`'s
                    // matching branch — see `ESC_RELEASE_SUPPRESS_WINDOW`'s
                    // doc for why.
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
                    vec![('\u{1b}', KeyModifiers::NONE)],
                    sgr::parse_prefix,
                    ESCAPE_GRACE,
                )? {
                    EscapeOutcome::Done => return Ok(()),
                    EscapeOutcome::Swallowed => {}
                }
            }
            Event::Key(key)
                if headless_bracket_recovery_active(
                    ctx,
                    normalize_key_code(key.code),
                    key.modifiers,
                ) =>
            {
                match swallow_one_sequence(
                    app,
                    ctx,
                    list_area,
                    vec![('[', KeyModifiers::NONE)],
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
/// buffering as a possible sequence start. Shape only — whether this
/// platform actually needs treating it that way is a separate question, see
/// [`headless_bracket_recovery_active`].
fn is_headless_bracket(code: KeyCode, mods: KeyModifiers) -> bool {
    code == KeyCode::Char('[') && mods.is_empty()
}

/// Whether a key event should actually be buffered as a possible headless
/// leaked sequence: it has the right shape ([`is_headless_bracket`]) AND
/// headless recovery is active for this run
/// ([`Context::headless_leak_recovery`]). Both [`event_loop`] and
/// [`drain_more`] gate their headless-bracket arm through this single
/// function so the two can't drift out of sync with each other.
fn headless_bracket_recovery_active(ctx: &Context, code: KeyCode, mods: KeyModifiers) -> bool {
    ctx.headless_leak_recovery && is_headless_bracket(code, mods)
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
///
/// `modifiers` is the *terminating letter's own* modifiers — the
/// discriminator a second dogfooding report (2026-07-26) forced: pasting
/// the literal text `"[A [B [C [D"` into the search box was misrecognized
/// as four arrow presses, eating everything but the spaces. Both halves of
/// the discriminator were measured, not assumed: (a) a genuine leaked ANSI
/// byte carries no modifiers at all (`swallow_one_sequence`'s own
/// `BANTO_INPUT_LOG` captures never show any), while (b) Windows Terminal
/// synthesizes SHIFT on each pasted uppercase letter (VkKeyScan-style — it
/// derives the modifier state from what typing that character would
/// require, so the physically-held key, if any, never merges in); in the
/// captured burst `' '`/`'['` carried no modifiers while only `A`/`B`/`C`/`D`
/// did, and the paste's own Shift+Insert Release landed 69ms *after* the
/// burst, ruling out an ordinary released Shift as the source. So: any
/// modifier on the terminating letter means this is real text, not a leaked
/// byte — replay it. Residual, knowingly accepted: a CapsLock user
/// hand-typing `[A` still produces a modifier-free `A` and still misfires;
/// strictly better than today's 100%-certain paste corruption, and the same
/// class of residual risk already accepted when this whole path was gated
/// to Windows (PR #4).
fn arrow_key_for(pending: &[char], modifiers: KeyModifiers) -> Option<KeyCode> {
    if !modifiers.is_empty() {
        return None;
    }
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
/// [`resolve_escape`]/[`resolve_headless_bracket`] have consumed so far,
/// paired with each byte's own modifiers — the seed byte is always
/// modifier-free, since both entry points already require that) and
/// re-check `parse` after each additional byte, waiting up to `grace` each
/// time for the next one (see [`resolve_escape`]/[`resolve_headless_bracket`]
/// — the same split-pacing risk applies at every byte boundary within the
/// sequence, not just its start):
/// - a complete match is applied via [`apply_sgr_action`] and swallowed;
/// - a definite mismatch replays the buffered characters as ordinary key
///   presses (see [`replay`]);
/// - if nothing more arrives in time, whatever was buffered is replayed the same way.
///
/// Modifiers are carried alongside each buffered character (rather than
/// discarded, as before) solely so [`arrow_key_for`] can see the
/// terminating letter's own — `parse`/[`replay`] still only ever see the
/// plain `char`s, and replay's own dispatch is unchanged: it always sends
/// `KeyModifiers::NONE`, regardless of what a replayed character actually
/// arrived with (this round changes leaked-arrow *recognition*, not
/// replay's dispatch semantics).
fn swallow_one_sequence(
    app: &mut App,
    ctx: &Context,
    list_area: Rect,
    mut pending: Vec<(char, KeyModifiers)>,
    parse: fn(&[char]) -> SgrParse,
    grace: Duration,
) -> Result<EscapeOutcome> {
    loop {
        let chars: Vec<char> = pending.iter().map(|&(c, _)| c).collect();
        match parse(&chars) {
            SgrParse::Complete(event) => {
                ctx.log(&format!("esc: swallowed complete sequence {event:?}"));
                apply_sgr_action(app, ctx, list_area, event);
                return Ok(EscapeOutcome::Swallowed);
            }
            SgrParse::NotSgr => {
                let last_modifiers = pending.last().map_or(KeyModifiers::NONE, |&(_, m)| m);
                if let Some(code) = arrow_key_for(&chars, last_modifiers) {
                    ctx.log(&format!(
                        "esc: recognized leaked arrow key {code:?} from buffer {chars:?}"
                    ));
                    handle_key(app, code, KeyModifiers::NONE, ctx);
                    return Ok(EscapeOutcome::Swallowed);
                }
                ctx.log(&format!("esc: NotSgr, replaying buffer {chars:?}"));
                replay(app, ctx, &chars);
                return Ok(EscapeOutcome::Done);
            }
            SgrParse::Incomplete => {
                if !event::poll(grace)? {
                    ctx.log(&format!(
                        "esc: per-byte grace expired, replaying buffer {chars:?}"
                    ));
                    replay(app, ctx, &chars);
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
                            // buffered as a candidate sequence byte. Its own
                            // modifiers (SHIFT included) still ride along in
                            // `pending`, for `arrow_key_for` to judge.
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                pending.push((c, key.modifiers))
                            }
                            KeyCode::Esc => pending.push(('\u{1b}', key.modifiers)),
                            other => {
                                end_interrupted_buffer(app, ctx, &chars);
                                handle_key(app, other, key.modifiers, ctx);
                                return Ok(EscapeOutcome::Done);
                            }
                        }
                    }
                    Event::Mouse(mouse) => {
                        end_interrupted_buffer(app, ctx, &chars);
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
        agent: row.agent,
        title: row.display_title().to_string(),
        cwd: row
            .cwd
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from(".")),
    })
}

/// Guards an open action against silently resuming a brigade Director from
/// the chōba: today that resumes it as plain `claude --resume` — no
/// `--mcp-config`, no role briefing — handing back a Director that
/// half-remembers its cell but has lost every tool it had to run it. The
/// chōba is feature-frozen (docs/REQUIREMENTS.md) and never wires that up
/// (the relay can't run there anyway), so the fix is a warning, not a
/// feature: refuse the first attempt and name the escape (the emporium,
/// where the cell is staged properly), then let a repeat through — the
/// operator may well want a plain resume, e.g. to peek at a transcript.
///
/// Returns `true` when the caller (`activate`/`activate_split`) should
/// proceed with its normal open path; `false` means it already posted the
/// warning status and the caller must return without opening anything. A
/// no-op (`true`) for any non-Director session, so `activate`/
/// `activate_split` can call this unconditionally rather than duplicating
/// `app.is_selected_director()` at each call site.
fn guard_director_open(app: &mut App, id: &str, action: OpenAction) -> bool {
    if !app.is_selected_director() {
        return true;
    }
    let now = Instant::now();
    if app.confirm_director_open(id, action, now) {
        return true;
    }
    app.set_status(
        "\u{1f91d} Director of a brigade — its cell lives in the oodana \
         (banto --emporium); press again to open solo here"
            .to_string(),
        now,
    );
    false
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

    if !guard_director_open(app, &id, OpenAction::Resume) {
        return;
    }

    // Only consulted here — in-place mode has no pane map, so this is the
    // *only* double-resume guard, not a fallback for an untracked case.
    let live = read_live_sessions(&ctx.claude_home.sessions_dir());
    let open_ctx = opener::OpenContext {
        probe: &SysinfoProbe,
        live: &live,
        binaries: &ctx.agent_binaries,
        codex_home: ctx.codex_home.as_ref(),
        start_time: &SysinfoStartTime,
    };
    match opener::decide_inplace_resume(&session, &open_ctx) {
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
            app.set_status(
                format!("session {id} is already running elsewhere"),
                Instant::now(),
            );
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

    if !guard_director_open(app, &id, OpenAction::Split) {
        return;
    }

    let backend = opener::resolve_backend(
        ctx.opener_mode,
        |key| std::env::var(key).ok(),
        cfg!(windows),
    );
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
    let live = read_live_sessions(&ctx.claude_home.sessions_dir());
    let outcome = opener::open_session(
        &ctx.store.borrow(),
        backend,
        &session,
        SystemCommandRunner,
        anchor.as_deref(),
        &opener::OpenContext {
            probe: &SysinfoProbe,
            live: &live,
            binaries: &ctx.agent_binaries,
            codex_home: ctx.codex_home.as_ref(),
            start_time: &SysinfoStartTime,
        },
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
    app.set_status(message, Instant::now());
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
    app.set_status(message, Instant::now());
}

fn toggle_agent_filter(app: &mut App) {
    let showing = app.toggle_agent_filter();
    app.set_status(
        if showing {
            "showing hidden sessions".to_string()
        } else {
            "hiding agent/superseded sessions".to_string()
        },
        Instant::now(),
    );
}

/// Re-read sessions from disk and re-classify their activity, preserving
/// selection (by session id), query and scroll clamping — see
/// [`App::replace_rows`]. Archived sessions are excluded, same as the
/// initial load in [`run`]. Also refreshes the hidden-worker/director id
/// sets (a brigade may have formed, spawned a Worker, or disbanded since the
/// last reload) and spends this reload's lineage-resolution budget (see
/// [`superseded_from_metas`]) against the same discover() pass the rows come
/// from. A read failure is tolerated: the previous rows (and superseded set)
/// are kept rather than the TUI erroring out over a transient filesystem
/// hiccup.
fn reload(app: &mut App, ctx: &Context) {
    if let Ok(metas) = session::discover_all(
        &ctx.claude_home,
        ctx.codex_home.as_ref(),
        &ctx.enabled_agents,
    ) {
        let store = ctx.store.borrow();
        let superseded = superseded_from_metas(&metas, &store, &ctx.superseded_failed);
        let rows = session::rows_from_metas(metas, &ctx.claude_home, ctx.thresholds);
        let rows = exclude_archived(rows, &store);
        app.replace_rows(rows);
        app.set_hidden_worker_ids(load_hidden_worker_ids(&store));
        app.set_directors(load_directors(&store));
        app.set_superseded(superseded);
    }
}

/// Render the whole UI for one frame: search box, list, the always-visible
/// summary panel, status bar, and finally a modal overlay on top of
/// everything else, if one is open. `now` is threaded down to
/// `view::render_list`/`view::render_summary` (the age columns) rather than
/// read internally — the clock is read once, here, at the draw call's
/// boundary.
fn render(frame: &mut Frame, app: &App, now: SystemTime) {
    let [search_area, list_area, summary_area, status_area] = layout_areas(frame.area());
    render_search(frame, app, search_area);
    view::render_list(frame, app, list_area, now);
    view::render_summary(frame, app, summary_area, now);
    render_status(frame, app, status_area);
    if let Some(modal) = app.modal() {
        // `false`: the chōba binds no key to `App::modal_toggle_new_session_agent`
        // (its new-session path is feature-frozen) — see `render_modal`'s doc.
        render_modal(frame, modal, frame.area(), false);
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

/// Render the bottom status bar: key hints (or a transient message) on the
/// left, match count right-aligned. Rendered as two separate widgets (rather
/// than one line) so the count stays visible even when the hints are too long
/// for a narrow terminal and get truncated.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    const NORMAL_HINTS: &str = "j/k\u{2191}\u{2193} move  PgUp/PgDn page  Enter open  s split  \
                                / search  n new  N new-split  d archive  g group  Tab view  \
                                p pin  a hidden  q/Esc quit";
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
            let hidden = app.hidden_count();
            if hidden > 0 {
                let plural = if hidden == 1 { "" } else { "s" };
                hints.push_str(&format!("  ({hidden} session{plural} hidden)"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRow;
    use banto_core::model::{Activity, AgeBucket, AgentKind};
    use banto_tui::render_modal::modal_area;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use std::path::PathBuf;

    fn row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            id: id.into(),
            agent: AgentKind::ClaudeCode,
            title: Some(title.into()),
            cwd: Some(PathBuf::from(cwd)),
            activity,
            is_agent: false,
            preview: None,
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            source_archived: false,
        }
    }

    fn agent_row(id: &str, title: &str, cwd: &str, activity: Activity) -> SessionRow {
        SessionRow {
            is_agent: true,
            ..row(id, title, cwd, activity)
        }
    }

    /// A synthetic [`SessionMeta`] fixture with no continuation.
    fn plain_meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: SessionId(id.to_string()),
            agent: AgentKind::ClaudeCode,
            title: None,
            cwd: None,
            source_path: PathBuf::from(format!("{id}.jsonl")),
            mtime: SystemTime::UNIX_EPOCH,
            size: 0,
            is_agent: false,
            preview: None,
            continuation_of_uuid: None,
            source_archived: false,
        }
    }

    /// Same as [`plain_meta`], but flagged as an auto-compaction
    /// continuation of `parent_uuid`, at a real `source_path` on disk (so
    /// `resolve_lineage`'s streaming scan, invoked through
    /// [`superseded_from_metas`], has a project directory to search).
    fn continuation_meta(id: &str, source_path: PathBuf, parent_uuid: &str) -> SessionMeta {
        SessionMeta {
            continuation_of_uuid: Some(parent_uuid.to_string()),
            source_path,
            ..plain_meta(id)
        }
    }

    #[test]
    fn superseded_from_metas_resolves_a_continuation_from_the_shared_discover_pass() {
        // No independent discover() here: `metas` is handed in directly, the
        // same way every call site now shares one discover() pass between
        // rows and lineage — see `superseded_from_metas`'s doc.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("parent.jsonl"),
            "{\"type\":\"user\",\"uuid\":\"P1\"}\n",
        )
        .unwrap();
        let child_path = dir.path().join("child.jsonl");
        std::fs::write(&child_path, "{\"type\":\"mode\"}\n").unwrap();
        let metas = vec![continuation_meta("child", child_path, "P1")];

        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert_eq!(superseded, HashSet::from(["parent".to_string()]));
        assert!(failed.borrow().is_empty());
    }

    #[test]
    fn superseded_from_metas_is_empty_for_sessions_without_a_continuation() {
        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());
        let metas = vec![plain_meta("a"), plain_meta("b")];

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert!(superseded.is_empty());
        assert!(failed.borrow().is_empty());
    }

    #[test]
    fn superseded_from_metas_records_an_unresolvable_continuation_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let child_path = dir.path().join("child.jsonl");
        std::fs::write(&child_path, "{\"type\":\"mode\"}\n").unwrap();
        let metas = vec![continuation_meta(
            "child",
            child_path,
            "nowhere-to-be-found",
        )];

        let store = Store::open_in_memory().unwrap();
        let failed = RefCell::new(HashSet::new());

        let superseded = superseded_from_metas(&metas, &store, &failed);

        assert!(superseded.is_empty());
        assert!(failed.borrow().contains(&SessionId("child".to_string())));
    }

    // --- exclude_archived: banto's archive and Codex's own flag, unioned --

    #[test]
    fn exclude_archived_hides_a_row_banto_itself_archived() {
        let store = Store::open_in_memory().unwrap();
        store.archive_session(&SessionId("a".to_string())).unwrap();
        let rows = vec![row("a", "A", "", Activity::Alive)];

        assert!(exclude_archived(rows, &store).is_empty());
    }

    #[test]
    fn exclude_archived_hides_a_row_only_codex_marked_archived() {
        let store = Store::open_in_memory().unwrap();
        // banto's own archive table has nothing for "a" at all — this row's
        // only reason to be hidden is `source_archived`, set by discovery
        // from Codex's own `threads.archived` (see `provider::codex`).
        let rows = vec![SessionRow {
            source_archived: true,
            ..row("a", "A", "", Activity::Alive)
        }];

        assert!(exclude_archived(rows, &store).is_empty());
    }

    #[test]
    fn exclude_archived_leaves_an_unarchived_row_alone() {
        let store = Store::open_in_memory().unwrap();
        let rows = vec![row("a", "A", "", Activity::Alive)];

        assert_eq!(exclude_archived(rows, &store).len(), 1);
    }

    #[test]
    fn exclude_archived_stays_hidden_via_bantos_own_archive_even_after_codex_unarchives() {
        // The disagreement case: the operator archived "a" in banto (`d`)
        // independently of Codex; Codex's own `archived` flag has since
        // gone back to false (`codex unarchive`, reflected on the next
        // discovery as `source_archived: false`). The row stays hidden —
        // banto's own archive is a separate fact banto never clears just
        // because the source disagrees; only banto's own unarchive would.
        let store = Store::open_in_memory().unwrap();
        store.archive_session(&SessionId("a".to_string())).unwrap();
        let rows = vec![SessionRow {
            source_archived: false,
            ..row("a", "A", "", Activity::Alive)
        }];

        assert!(exclude_archived(rows, &store).is_empty());
    }

    /// A `Context` for tests exercising `handle_key`/`handle_normal_key`/
    /// `handle_search_key` directly — these are ordinary functions with no
    /// terminal dependency (only `resolve_escape` touches `event::poll`/
    /// `read`), so they're testable without a real terminal, just an
    /// in-memory store (and caller-owned `thresholds`, so the returned
    /// `Context`'s lifetime doesn't outlive a temporary) to satisfy
    /// `Context`'s shape. Defaults `headless_leak_recovery` to `true` (the
    /// existing headless-recovery tests below all predate the platform
    /// gate and assert that behavior); tests that need the flag off use
    /// [`test_context_with_headless_recovery`] instead.
    fn test_context<'a>(store: &'a RefCell<Store>, thresholds: &'a AgeThresholds) -> Context<'a> {
        test_context_with_headless_recovery(store, thresholds, true)
    }

    /// Same as [`test_context`], with `headless_leak_recovery` set
    /// explicitly instead of defaulted — for tests asserting the
    /// gate itself, or the flag-off (non-Windows) behavior it now guards.
    fn test_context_with_headless_recovery<'a>(
        store: &'a RefCell<Store>,
        thresholds: &'a AgeThresholds,
        headless_leak_recovery: bool,
    ) -> Context<'a> {
        Context {
            claude_home: ClaudeHome::new(PathBuf::from(".")),
            codex_home: None,
            agent_binaries: AgentBinaries::default(),
            enabled_agents: AgentKind::ALL.into_iter().collect(),
            thresholds,
            store,
            opener_mode: OpenerMode::Auto,
            input_log: std::cell::RefCell::new(None),
            last_genuine_esc: RefCell::new(None),
            pending_inplace: RefCell::new(None),
            superseded_failed: RefCell::new(HashSet::new()),
            headless_leak_recovery,
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
        terminal
            .draw(|frame| render(frame, app, SystemTime::now()))
            .unwrap();
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
        // Pinned "Beta task" sorts first, under the Pinned section header
        // (grouped view is on by default) — the header carries the pin
        // marker; the row itself stays unmarked (repeating it on every row
        // under the header would be pure noise; see
        // `App::VisibleRow::in_pinned_section`).
        let lines: Vec<&str> = text.lines().collect();
        let beta_pos = lines
            .iter()
            .position(|line| line.contains("Beta task"))
            .unwrap();
        let alpha_pos = lines
            .iter()
            .position(|line| line.contains("Alpha task"))
            .unwrap();
        assert!(
            beta_pos < alpha_pos,
            "pinned row should be listed first:\n{text}"
        );
        assert!(
            !lines[beta_pos].contains('\u{1F4CC}'), // 📌
            "pin marker should be suppressed on a row under the Pinned header:\n{text}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Pinned") && line.contains('\u{1F4CC}')),
            "Pinned section header should carry the pin marker:\n{text}"
        );
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
            wide_text.contains("1 session hidden"),
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

    /// Regression for the race `ESC_RELEASE_SUPPRESS_WINDOW`'s doc
    /// describes: proves the trailing Release is swallowed rather than
    /// firing a second Esc.
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

    /// Previously untestable: the summary panel's relative-age text depended
    /// on the real wall clock (`render` read `SystemTime::now()` internally),
    /// so the exact age string could never be asserted deterministically.
    /// Now that `now` is an argument, an injected `now` and a fixed `mtime`
    /// pin the exact humanized age.
    #[test]
    fn render_summary_shows_a_deterministic_relative_age_with_an_injected_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mtime = now - Duration::from_secs(3 * 3600);
        let mut app = App::new(vec![SessionRow {
            mtime,
            ..row("a", "Alpha", "/work/alpha", Activity::Alive)
        }]);
        app.set_viewport_height(10);

        let mut terminal = Terminal::new(TestBackend::new(60, 15)).unwrap();
        terminal.draw(|frame| render(frame, &app, now)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("3h ago"), "got {text}");
    }

    #[test]
    fn summary_panel_is_dropped_in_a_too_short_terminal() {
        let mut app = App::new(vec![row("a", "Alpha", "/work/alpha", Activity::Alive)]);
        app.set_viewport_height(3);

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| render(frame, &app, SystemTime::now()))
            .unwrap();
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
            launch.startup_message,
            opener::new_session_startup_message(&PathBuf::from("."))
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
        assert_eq!(
            launch.startup_message,
            opener::resume_startup_message("Alpha")
        );
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
    fn enter_on_a_director_row_warns_first_then_opens_on_a_repeat() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("dir-1", "Director", "/work/dir", Activity::Alive)]);
        app.set_viewport_height(10);
        app = app.with_directors(["dir-1".to_string()].into_iter().collect());

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);
        assert!(
            ctx.pending_inplace.borrow().is_none(),
            "first Enter on a Director must warn, not open"
        );
        let warning = app.status().expect("expected a warning status");
        assert!(
            warning.contains("oodana"),
            "warning should name the emporium escape: {warning}"
        );

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);
        let launch = ctx
            .pending_inplace
            .borrow_mut()
            .take()
            .expect("second Enter should proceed to open");
        assert_eq!(
            launch.argv,
            ["claude", "--resume", "dir-1"].map(str::to_string)
        );
    }

    #[test]
    fn split_after_an_enter_warning_on_a_director_warns_again_instead_of_confirming() {
        // Action isolation: a warning armed by Enter (Resume) must not let
        // a subsequent `s` (Split) through — each action confirms
        // independently. Confirming `s` itself would shell out to a real
        // backend (see `enter_stages_an_in_place_resume_for_the_selected_
        // session`'s note on why `activate_split`'s proceed path stays
        // untested here); this only needs the warning, which returns before
        // any of that I/O runs.
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("dir-1", "Director", "/work/dir", Activity::Alive)]);
        app.set_viewport_height(10);
        app = app.with_directors(["dir-1".to_string()].into_iter().collect());

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);
        assert!(ctx.pending_inplace.borrow().is_none());

        handle_key(&mut app, KeyCode::Char('s'), KeyModifiers::NONE, &ctx);
        let warning = app
            .status()
            .expect("expected a fresh warning for the split action");
        assert!(warning.contains("oodana"));
    }

    #[test]
    fn enter_on_a_non_director_row_opens_immediately_with_no_warning() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row(
            "plain-1",
            "Plain",
            "/work/plain",
            Activity::Alive,
        )]);
        app.set_viewport_height(10);

        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE, &ctx);

        assert!(
            ctx.pending_inplace.borrow().is_some(),
            "a non-Director session must open on the first Enter"
        );
        assert!(app.status().is_none());
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
        terminal
            .draw(|frame| render(frame, &app, SystemTime::now()))
            .unwrap();
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
    fn archive_modal_content_has_one_column_of_padding_inside_the_border() {
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.open_confirm_archive_modal();

        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal
            .draw(|frame| render(frame, &app, SystemTime::now()))
            .unwrap();
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
        assert_eq!(
            arrow_key_for(&['[', 'A'], KeyModifiers::NONE),
            Some(KeyCode::Up)
        );
        assert_eq!(
            arrow_key_for(&['[', 'B'], KeyModifiers::NONE),
            Some(KeyCode::Down)
        );
        assert_eq!(
            arrow_key_for(&['[', 'C'], KeyModifiers::NONE),
            Some(KeyCode::Right)
        );
        assert_eq!(
            arrow_key_for(&['[', 'D'], KeyModifiers::NONE),
            Some(KeyCode::Left)
        );
        assert_eq!(
            arrow_key_for(&['\u{1b}', '[', 'A'], KeyModifiers::NONE),
            Some(KeyCode::Up)
        );
        assert_eq!(
            arrow_key_for(&['\u{1b}', '[', 'D'], KeyModifiers::NONE),
            Some(KeyCode::Left)
        );
    }

    #[test]
    fn arrow_key_for_rejects_shapes_that_are_not_a_bare_arrow_key() {
        assert_eq!(arrow_key_for(&['[', 'x'], KeyModifiers::NONE), None);
        assert_eq!(arrow_key_for(&['[', '<'], KeyModifiers::NONE), None); // SGR mouse lead-in
        assert_eq!(arrow_key_for(&['['], KeyModifiers::NONE), None);
        assert_eq!(arrow_key_for(&[], KeyModifiers::NONE), None);
    }

    /// A terminating letter that arrived with ANY modifier is real text (a
    /// pasted/typed uppercase letter under Windows Terminal's
    /// VkKeyScan-style SHIFT synthesis — see `arrow_key_for`'s doc), not a
    /// leaked byte, regardless of the shape otherwise matching.
    #[test]
    fn arrow_key_for_rejects_a_terminating_letter_carrying_any_modifier() {
        assert_eq!(arrow_key_for(&['[', 'A'], KeyModifiers::SHIFT), None);
        assert_eq!(arrow_key_for(&['[', 'A'], KeyModifiers::CONTROL), None);
        assert_eq!(
            arrow_key_for(&['\u{1b}', '[', 'B'], KeyModifiers::SHIFT),
            None
        );
    }

    /// Exercises the headless-recovery *mechanism* directly (bypassing the
    /// platform gate at its call sites) to prove it still correctly resolves
    /// a leaked headless arrow shape when engaged. See the
    /// `with_headless_recovery_disabled_*` tests below for the same "[A"
    /// input on the platforms where the gate now keeps this from running.
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
            vec![('[', KeyModifiers::NONE), ('A', KeyModifiers::NONE)],
            sgr::parse_headless_prefix,
            HEADLESS_GRACE,
        )
        .unwrap();

        assert!(matches!(outcome, EscapeOutcome::Swallowed));
        assert_eq!(app.selected_row().unwrap().id, "a");
        assert_eq!(app.query(), "", "must not have been typed as garbage");
    }

    /// Pasting the literal text `"[A [B [C [D"` moved the selection four
    /// times and left only the spaces in the query, because Windows
    /// Terminal synthesizes SHIFT on each pasted uppercase letter (see
    /// `arrow_key_for`'s doc) and the old recognition never looked at
    /// modifiers at all. Each `"[X"` pair is its own `swallow_one_sequence`
    /// call in production (mirroring `drain_more`'s per-sequence loop); the
    /// space between them is an ordinary keystroke the main loop would
    /// dispatch directly, simulated here with a plain `push_char`.
    #[test]
    fn a_pasted_shift_modified_arrow_lookalike_burst_replays_as_literal_text() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context(&store, &thresholds);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();
        let list_area = Rect::new(0, 4, 60, 3);

        for letter in ['A', 'B', 'C', 'D'] {
            let outcome = swallow_one_sequence(
                &mut app,
                &ctx,
                list_area,
                vec![('[', KeyModifiers::NONE), (letter, KeyModifiers::SHIFT)],
                sgr::parse_headless_prefix,
                HEADLESS_GRACE,
            )
            .unwrap();
            assert!(matches!(outcome, EscapeOutcome::Done));
            app.push_char(' '); // the literal space the paste sends between tokens
        }

        assert_eq!(
            app.query(),
            "[A [B [C [D ",
            "the whole burst must land verbatim, not move the selection"
        );
    }

    #[test]
    fn headless_bracket_recovery_active_is_gated_by_the_platform_flag() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx_on = test_context_with_headless_recovery(&store, &thresholds, true);
        let ctx_off = test_context_with_headless_recovery(&store, &thresholds, false);

        assert!(headless_bracket_recovery_active(
            &ctx_on,
            KeyCode::Char('['),
            KeyModifiers::NONE
        ));
        assert!(!headless_bracket_recovery_active(
            &ctx_off,
            KeyCode::Char('['),
            KeyModifiers::NONE
        ));
        // The flag can't turn a non-bracket shape into a match either way.
        assert!(!headless_bracket_recovery_active(
            &ctx_on,
            KeyCode::Char('x'),
            KeyModifiers::NONE
        ));
    }

    /// The Unix-side regression test for the bug this gate exists to fix:
    /// with headless recovery off, typing `[` then `A` (or `B`/`C`/`D`) in
    /// the search box — e.g. "[Active]", "[Draft]", a `[A-Z]` regex class —
    /// must land as literal query text, not get read as list navigation.
    #[test]
    fn with_headless_recovery_disabled_bracket_letter_types_instead_of_navigating() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context_with_headless_recovery(&store, &thresholds, false);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        assert!(!headless_bracket_recovery_active(
            &ctx,
            KeyCode::Char('['),
            KeyModifiers::NONE
        ));
        // With the flag off this is exactly what `event_loop` dispatches:
        // both keys go straight to `handle_key`, never through
        // `resolve_headless_bracket`/`swallow_one_sequence`/`arrow_key_for`.
        handle_key(&mut app, KeyCode::Char('['), KeyModifiers::NONE, &ctx);
        handle_key(&mut app, KeyCode::Char('A'), KeyModifiers::NONE, &ctx);

        // `push_char`'s own reset-selection-to-top-match on every keystroke
        // (ordinary search-as-you-type UX) is a separate concern from what
        // this test checks — the query landing as "[A" already proves
        // `arrow_key_for` never ran: had it fired, the query would still be
        // empty and selection would have moved via `select_prev`/
        // `select_next` instead of a re-filter.
        assert_eq!(
            app.query(),
            "[A",
            "must be typed as text, not read as Up/Down/Left/Right"
        );
    }

    /// Companion to the above: a lone `[` (nothing typed after it) must
    /// still appear immediately when the flag is off — it never enters
    /// `resolve_headless_bracket`, so it never pays the [`HEADLESS_GRACE`]
    /// wait that path would otherwise impose on every single `[` keystroke.
    #[test]
    fn with_headless_recovery_disabled_a_lone_bracket_types_with_no_grace_wait() {
        let store = RefCell::new(Store::open_in_memory().unwrap());
        let thresholds = AgeThresholds::default();
        let ctx = test_context_with_headless_recovery(&store, &thresholds, false);
        let mut app = App::new(vec![row("a", "Alpha", "", Activity::Alive)]);
        app.set_viewport_height(10);
        app.enter_search();

        assert!(!headless_bracket_recovery_active(
            &ctx,
            KeyCode::Char('['),
            KeyModifiers::NONE
        ));
        handle_key(&mut app, KeyCode::Char('['), KeyModifiers::NONE, &ctx);

        assert_eq!(app.query(), "[");
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
            vec![
                ('\u{1b}', KeyModifiers::NONE),
                ('[', KeyModifiers::NONE),
                ('B', KeyModifiers::NONE),
            ],
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
            vec![
                ('\u{1b}', KeyModifiers::NONE),
                ('[', KeyModifiers::NONE),
                ('B', KeyModifiers::NONE),
            ],
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
